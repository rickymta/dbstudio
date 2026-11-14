//! SQL script writer: DDL, then batched `INSERT` statements in the dialect of the source.

use std::io::Write;
use std::path::PathBuf;

use dbstudio_core::{
    ColumnMeta, DbKind, DdlItem, DdlSection, Error, ObjectKind, ObjectPath, Result, ResultSink,
    ScriptDialect, StatementEnd, Value,
};
use dbstudio_sql::Dialect;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::{WriterConfig, qualifier};
use crate::fsio::Output;
use crate::{ExportPhase, ExportedObject, ObjectMeta, SqlOptions, TableWriter};

/// SQL Server accepts at most 1000 rows in one `VALUES` list.
const MSSQL_MAX_ROWS: usize = 1000;
/// Rows per `INSERT ... VALUES` for ClickHouse.
const CLICKHOUSE_BATCH_ROWS: usize = 1000;
const FLUSH_AT: usize = 256 * 1024;

#[derive(Debug)]
pub(crate) struct SqlWriter {
    out: Output,
    opts: SqlOptions,
    dialect: ScriptDialect,
    q: Dialect,
    buf: String,
    started: bool,
    tx_open: bool,
    table: Option<TableState>,
    wrote_any: bool,
    kind: DbKind,
    /// Objects seen so far, in file order, with their phases.
    objects: Vec<ExportedObject>,
    /// `(object index, phase)` of the last marker written, so consecutive statements of the
    /// same object share one marker.
    last_marker: Option<(usize, ExportPhase)>,
    /// A `-- dbstudio:common` marker is already the last marker.
    in_common: bool,
    /// Random per file: markers carry it so a value that merely looks like a marker is not one.
    nonce: String,
    /// Index into `objects` of the table whose rows are being written.
    data_object: Option<usize>,
}

#[derive(Debug)]
struct TableState {
    /// `INSERT INTO t (a, b) VALUES`
    prefix: String,
    /// Statement under construction (prefix and rows).
    stmt: String,
    rows_in_stmt: usize,
    per_row: bool,
    batch_rows: usize,
    /// Indexes of the columns that are written; generated columns are computed by the server
    /// and cannot be inserted.
    keep: Vec<usize>,
    /// Native type name of every result column (hint for CQL / ClickHouse collection literals).
    types: Vec<String>,
}

/// `extra` text of a column that the server computes (`GENERATED ALWAYS AS`).
pub(crate) fn is_generated(extra: Option<&str>) -> bool {
    let e = extra.unwrap_or("").to_ascii_lowercase();
    e.contains("stored generated")
        || e.contains("virtual generated")
        || e.contains("generated stored")
        || e.contains("generated virtual")
}

impl SqlWriter {
    pub fn create(cfg: &WriterConfig, opts: SqlOptions) -> Result<SqlWriter> {
        // The script must read the same on any server: MySQL literals always use backslash
        // escapes (and the prologue turns `NO_BACKSLASH_ESCAPES` off), whatever the source or
        // the destination session is set to.
        let dialect = match cfg.dialect {
            ScriptDialect::MySql { mariadb, .. } => ScriptDialect::mysql(mariadb, true),
            other => other,
        };
        Ok(SqlWriter {
            out: Output::create(&cfg.dest, cfg.overwrite, false)?,
            q: Dialect::new(dialect),
            dialect,
            opts,
            buf: String::new(),
            started: false,
            tx_open: false,
            table: None,
            wrote_any: false,
            kind: cfg.kind,
            objects: Vec::new(),
            last_marker: None,
            in_common: false,
            nonce: uuid::Uuid::new_v4().simple().to_string()[..16].to_string(),
            data_object: None,
        })
    }

    /// Marker line that starts the statements of one object phase. A reader that restores only
    /// some objects runs the statements after a marker only when it selected that object.
    fn object_marker(
        &mut self,
        kind: ObjectKind,
        schema: Option<String>,
        name: &str,
        phase: ExportPhase,
    ) -> usize {
        let index = match self
            .objects
            .iter()
            .position(|o| o.kind == kind && o.schema == schema && o.name == name)
        {
            Some(i) => i,
            None => {
                self.objects.push(ExportedObject {
                    kind,
                    schema: schema.clone(),
                    name: name.to_string(),
                    phases: Vec::new(),
                    rows: None,
                });
                self.objects.len() - 1
            }
        };
        if self.last_marker == Some((index, phase)) && !self.in_common {
            return index;
        }
        let obj = &mut self.objects[index];
        if !obj.phases.contains(&phase) {
            obj.phases.push(phase);
        }
        self.last_marker = Some((index, phase));
        self.in_common = false;
        let kind_text = serde_json::to_value(kind)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_else(|| "table".into());
        self.buf.push_str(&format!(
            "-- dbstudio:object nonce={} kind={kind_text}",
            self.nonce
        ));
        if let Some(s) = &schema {
            self.buf.push_str(" schema=");
            crate::values::json_string(&mut self.buf, &single_line(s));
        }
        self.buf.push_str(" name=");
        crate::values::json_string(&mut self.buf, &single_line(name));
        self.buf.push_str(&format!(" phase={}\n", phase.marker()));
        index
    }

    /// Marker for statements that belong to no object (transaction framing, session settings):
    /// a partial restore always runs them.
    fn common_marker(&mut self) {
        if self.in_common {
            return;
        }
        self.in_common = true;
        self.last_marker = None;
        self.buf
            .push_str(&format!("-- dbstudio:common nonce={}\n", self.nonce));
    }

    fn flush_buf(&mut self) -> Result<()> {
        if !self.buf.is_empty() {
            self.out.write_all(self.buf.as_bytes())?;
            self.buf.clear();
        }
        Ok(())
    }

    fn begin_sql(&self) -> Option<&'static str> {
        match self.dialect {
            // No multi-statement transactions.
            ScriptDialect::Cql | ScriptDialect::ClickHouse => None,
            ScriptDialect::MySql { .. } => Some("START TRANSACTION;"),
            ScriptDialect::MsSql => Some("BEGIN TRANSACTION;"),
            ScriptDialect::Oracle => None,
            _ => Some("BEGIN;"),
        }
    }

    fn fk_off(&self) -> Option<(&'static str, &'static str)> {
        match self.dialect {
            ScriptDialect::MySql { .. } => {
                Some(("SET FOREIGN_KEY_CHECKS=0;", "SET FOREIGN_KEY_CHECKS=1;"))
            }
            // CockroachDB / YugabyteDB have no `session_replication_role` switch.
            ScriptDialect::Postgres
                if matches!(self.kind, DbKind::CockroachDb | DbKind::YugabyteDb) =>
            {
                None
            }
            ScriptDialect::Postgres => Some((
                "SET session_replication_role = 'replica';",
                "SET session_replication_role = 'origin';",
            )),
            _ => None,
        }
    }

    fn prologue(&mut self) {
        if self.started {
            return;
        }
        self.started = true;
        let now = OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_default();
        self.buf
            .push_str(&format!("-- DBStudio SQL export\n-- Generated: {now}\n\n"));
        match self.dialect {
            ScriptDialect::MySql { .. } => self.buf.push_str(
                "SET NAMES utf8mb4;\n\
                 SET @DBSTUDIO_OLD_SQL_MODE=@@SQL_MODE, SQL_MODE='NO_AUTO_VALUE_ON_ZERO';\n\
                 SET @DBSTUDIO_OLD_TIME_ZONE=@@TIME_ZONE, TIME_ZONE='+00:00';\n",
            ),
            ScriptDialect::Postgres => self
                .buf
                .push_str("SET standard_conforming_strings = on;\nSET client_encoding = 'UTF8';\n"),
            _ => {}
        }
        if self.opts.disable_fk_checks {
            if let Some((off, _)) = self.fk_off() {
                self.buf.push_str(off);
                self.buf.push('\n');
            }
        }
        self.buf.push('\n');
    }

    fn open_tx(&mut self) {
        if self.opts.wrap_in_transaction && !self.tx_open {
            if let Some(b) = self.begin_sql() {
                self.common_marker();
                self.buf.push_str(b);
                self.buf.push('\n');
                self.tx_open = true;
            }
        }
    }

    fn close_tx(&mut self) {
        if self.tx_open {
            self.common_marker();
            self.buf.push_str("COMMIT;\n\n");
            self.tx_open = false;
        }
    }

    fn flush_stmt(&mut self) {
        if let Some(t) = self.table.as_mut() {
            if t.rows_in_stmt > 0 {
                self.buf.push_str(&t.stmt);
                self.buf.push_str(";\n");
                t.stmt.clear();
                t.rows_in_stmt = 0;
            }
        }
    }

    fn row_tuple(&self, row: &[Value], keep: &[usize], types: &[String]) -> String {
        let mut s = String::from("(");
        for (n, &i) in keep.iter().enumerate() {
            if n > 0 {
                s.push_str(", ");
            }
            let v = row.get(i).unwrap_or(&Value::Null);
            match self.dialect {
                // Collections, maps, tuples and UDTs need the column type to be written as
                // real CQL / ClickHouse literals (never as JSON text).
                ScriptDialect::Cql | ScriptDialect::ClickHouse => {
                    s.push_str(&self.q.literal_for_type(
                        v,
                        types.get(i).map(String::as_str).filter(|t| !t.is_empty()),
                    ))
                }
                _ => s.push_str(&sql_literal(self.q, self.dialect, v)),
            }
        }
        s.push(')');
        s
    }
}

/// Text on one line (markers must not span lines).
fn single_line(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Object a DDL item belongs to for the markers: indexes, constraints and triggers belong to
/// their table; everything else is its own object. Returns `(kind, schema, name)`.
fn owner_of(
    kind: ObjectKind,
    path: &ObjectPath,
    dialect: ScriptDialect,
) -> (ObjectKind, Option<String>, String) {
    let owner = path
        .segments
        .iter()
        .rposition(|s| {
            !matches!(
                s.kind,
                ObjectKind::Folder | ObjectKind::Index | ObjectKind::Trigger | ObjectKind::Column
            )
        })
        .or_else(|| path.segments.len().checked_sub(1));
    let Some(i) = owner else {
        return (kind, None, String::new());
    };
    let seg = &path.segments[i];
    // The path segment says what the owner is (an index item may point at its table).
    let own_kind = seg.kind;
    let parent = ObjectPath {
        segments: path.segments[..i].to_vec(),
    };
    let schema = match own_kind {
        ObjectKind::Database | ObjectKind::Schema => None,
        _ => qualifier(Some(&parent), None, dialect),
    };
    (own_kind, schema, seg.name.clone())
}

/// SQL literal of a value in the source dialect. PostgreSQL arrays become array literals,
/// documents become JSON text.
pub(crate) fn sql_literal(q: Dialect, dialect: ScriptDialect, v: &Value) -> String {
    match v {
        Value::Array(_) | Value::Document(_)
            if matches!(dialect, ScriptDialect::Cql | ScriptDialect::ClickHouse) =>
        {
            q.literal_for_type(v, None)
        }
        Value::Array(items) if matches!(dialect, ScriptDialect::Postgres) => {
            q.literal_text(&pg_array_text(items))
        }
        Value::Array(_) | Value::Document(_) => {
            let mut s = String::new();
            crate::values::write_json(
                &mut s,
                v,
                crate::BinaryFormat::Hex,
                crate::ExtendedJson::Off,
                None,
                0,
            );
            q.literal_text(&s)
        }
        other => q.literal(other),
    }
}

/// PostgreSQL array text form (`{1,"a b",NULL}`) of `items`.
fn pg_array_text(items: &[Value]) -> String {
    let mut s = String::from("{");
    for (i, v) in items.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        match v {
            Value::Null => s.push_str("NULL"),
            Value::Array(inner) => s.push_str(&pg_array_text(inner)),
            Value::Bool(b) => s.push(if *b { 't' } else { 'f' }),
            Value::Int(n) => s.push_str(&n.to_string()),
            Value::UInt(n) => s.push_str(&n.to_string()),
            Value::Float(f) => s.push_str(&format!("{f:?}")),
            Value::Decimal(t) => s.push_str(t),
            Value::Bytes(b) => {
                s.push_str("\"\\\\x");
                for byte in b {
                    s.push_str(&format!("{byte:02x}"));
                }
                s.push('"');
            }
            other => {
                let text = crate::values::cell_text(other, crate::BinaryFormat::Hex)
                    .map(|c| c.into_owned())
                    .unwrap_or_default();
                s.push('"');
                for c in text.chars() {
                    if c == '"' || c == '\\' {
                        s.push('\\');
                    }
                    s.push(c);
                }
                s.push('"');
            }
        }
    }
    s.push('}');
    s
}

impl ResultSink for SqlWriter {
    fn on_columns(&mut self, _columns: Vec<ColumnMeta>) -> Result<()> {
        Ok(())
    }

    fn on_rows(&mut self, rows: Vec<Vec<Value>>) -> Result<()> {
        if self.table.is_none() {
            return Ok(());
        }
        if let Some(i) = self.data_object {
            let o = &mut self.objects[i];
            o.rows = Some(o.rows.unwrap_or(0) + rows.len() as u64);
        }
        for row in &rows {
            let (keep, types) = self
                .table
                .as_ref()
                .map(|t| (t.keep.clone(), t.types.clone()))
                .unwrap_or_default();
            let tuple = self.row_tuple(row, &keep, &types);
            let max_bytes = self.opts.max_statement_bytes as usize;
            let t = self.table.as_mut().expect("checked above");
            if t.rows_in_stmt > 0
                && (t.per_row
                    || t.rows_in_stmt >= t.batch_rows
                    || (max_bytes > 0 && t.stmt.len() + tuple.len() > max_bytes))
            {
                self.buf.push_str(&t.stmt);
                self.buf.push_str(";\n");
                t.stmt.clear();
                t.rows_in_stmt = 0;
            }
            if t.rows_in_stmt == 0 {
                t.stmt.push_str(&t.prefix);
                t.stmt.push('\n');
            } else {
                t.stmt.push_str(",\n");
            }
            t.stmt.push_str(&tuple);
            t.rows_in_stmt += 1;
            if self.buf.len() >= FLUSH_AT {
                self.flush_buf()?;
            }
        }
        Ok(())
    }

    fn on_message(&mut self, _message: String) -> Result<()> {
        Ok(())
    }

    fn on_statement_end(&mut self, _end: StatementEnd) -> Result<()> {
        Ok(())
    }
}

impl TableWriter for SqlWriter {
    fn write_ddl(&mut self, section: DdlSection, items: &[DdlItem]) -> Result<()> {
        self.prologue();
        if section == DdlSection::PostData {
            self.close_tx();
        }
        let phase = match section {
            DdlSection::PreData => ExportPhase::Pre,
            DdlSection::PostData => ExportPhase::Post,
        };
        for item in items {
            let (kind, schema, name) = owner_of(item.kind, &item.path, self.dialect);
            let _ = self.object_marker(kind, schema, &name, phase);
            self.buf.push_str(item.sql.trim_end());
            self.buf.push('\n');
            if matches!(self.dialect, ScriptDialect::MsSql) {
                self.buf.push_str("GO\n");
            }
            self.buf.push('\n');
            self.wrote_any = true;
            if self.buf.len() >= FLUSH_AT {
                self.flush_buf()?;
            }
        }
        Ok(())
    }

    fn begin_object(&mut self, meta: &ObjectMeta) -> Result<()> {
        self.prologue();
        self.open_tx();
        let q = self.q;
        let name = if self.opts.ddl.qualify_schema {
            let schema = meta.details.as_ref().and_then(|d| d.schema.as_deref());
            match qualifier(meta.path.as_ref(), schema, self.dialect) {
                Some(s) => format!("{}.{}", q.quote_ident(&s), q.quote_ident(meta.table_name())),
                None => q.quote_ident(meta.table_name()),
            }
        } else {
            q.quote_ident(meta.table_name())
        };
        let generated: Vec<bool> = meta
            .columns
            .iter()
            .map(|c| {
                meta.details
                    .as_ref()
                    .and_then(|d| d.columns.iter().find(|x| x.name == c.name))
                    .is_some_and(|d| is_generated(d.extra.as_deref()))
            })
            .collect();
        let keep: Vec<usize> = (0..meta.columns.len()).filter(|i| !generated[*i]).collect();
        let columns = if self.opts.include_column_names || keep.len() != meta.columns.len() {
            let list: Vec<String> = keep
                .iter()
                .map(|&i| q.quote_ident(&meta.columns[i].name))
                .collect();
            format!(" ({})", list.join(", "))
        } else {
            String::new()
        };
        // `GENERATED ALWAYS AS IDENTITY` refuses explicit values unless the statement says so.
        let overriding = if matches!(self.dialect, ScriptDialect::Postgres)
            && keep.iter().any(|&i| {
                meta.details
                    .as_ref()
                    .and_then(|d| d.columns.iter().find(|x| x.name == meta.columns[i].name))
                    .and_then(|d| d.extra.as_deref())
                    .is_some_and(|e| e.to_ascii_lowercase().contains("identity always"))
            }) {
            " OVERRIDING SYSTEM VALUE"
        } else {
            ""
        };
        // Oracle and CQL take one row per `INSERT`.
        let per_row = matches!(self.dialect, ScriptDialect::Oracle | ScriptDialect::Cql);
        let mut batch_rows = self.opts.insert_batch_rows.max(1) as usize;
        if matches!(self.dialect, ScriptDialect::MsSql) {
            batch_rows = batch_rows.min(MSSQL_MAX_ROWS);
        }
        if matches!(self.dialect, ScriptDialect::ClickHouse) {
            // ClickHouse prefers few big inserts.
            batch_rows = CLICKHOUSE_BATCH_ROWS;
        }
        if meta.path.is_some() {
            let kind = meta.details.as_ref().map_or(ObjectKind::Table, |d| d.kind);
            let schema = qualifier(
                meta.path.as_ref(),
                meta.details.as_ref().and_then(|d| d.schema.as_deref()),
                self.dialect,
            );
            self.object_marker(kind, schema, meta.table_name(), ExportPhase::Data);
            self.data_object = self
                .objects
                .iter()
                .position(|o| o.name == meta.table_name() && o.phases.contains(&ExportPhase::Data));
        } else {
            self.data_object = None;
        }
        self.buf.push_str(&format!(
            "-- Data of {}\n",
            meta.name
                .chars()
                .map(|c| if c.is_control() { ' ' } else { c })
                .collect::<String>()
        ));
        self.table = Some(TableState {
            prefix: format!("INSERT INTO {name}{columns}{overriding} VALUES"),
            stmt: String::new(),
            rows_in_stmt: 0,
            per_row,
            batch_rows,
            keep,
            types: meta.columns.iter().map(|c| c.type_name.clone()).collect(),
        });
        Ok(())
    }

    fn end_object(&mut self) -> Result<()> {
        self.flush_stmt();
        self.table = None;
        self.buf.push('\n');
        self.flush_buf()
    }

    fn bytes(&self) -> u64 {
        self.out.bytes()
    }

    fn objects(&mut self) -> Vec<ExportedObject> {
        self.objects.clone()
    }

    fn nonce(&self) -> Option<String> {
        Some(self.nonce.clone())
    }

    fn finish(mut self: Box<Self>) -> Result<Vec<PathBuf>> {
        self.prologue();
        self.flush_stmt();
        self.close_tx();
        if self.opts.disable_fk_checks {
            if let Some((_, on)) = self.fk_off() {
                self.common_marker();
                self.buf.push_str(on);
                self.buf.push('\n');
            }
        }
        if matches!(self.dialect, ScriptDialect::MySql { .. }) {
            self.common_marker();
            self.buf.push_str(
                "SET SQL_MODE=@DBSTUDIO_OLD_SQL_MODE;\nSET TIME_ZONE=@DBSTUDIO_OLD_TIME_ZONE;\n",
            );
        }
        self.flush_buf()?;
        if !self.wrote_any && self.out.bytes() == 0 {
            return Err(Error::Other("nothing to export".into()));
        }
        let SqlWriter { out, .. } = *self;
        let (path, _) = out.commit()?;
        Ok(vec![path])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_core::ValueKind;

    fn pg() -> Dialect {
        Dialect::new(ScriptDialect::Postgres)
    }

    #[test]
    fn pg_arrays_and_documents() {
        let arr = Value::Array(vec![
            Value::Int(1),
            Value::Text("a \"b\"".into()),
            Value::Null,
            Value::Array(vec![Value::Bool(true)]),
        ]);
        assert_eq!(
            sql_literal(pg(), ScriptDialect::Postgres, &arr),
            "E'{1,\"a \\\\\"b\\\\\"\",NULL,{t}}'"
        );
        let doc = Value::Document(vec![("a".into(), Value::Text("it's".into()))]);
        assert_eq!(
            sql_literal(pg(), ScriptDialect::Postgres, &doc),
            "'{\"a\":\"it''s\"}'"
        );
        assert_eq!(
            sql_literal(pg(), ScriptDialect::Postgres, &Value::Null),
            "NULL"
        );
        let _ = ValueKind::Int;
    }

    fn script(
        dialect: ScriptDialect,
        kind: dbstudio_core::DbKind,
        types: &[&str],
        row: Vec<Value>,
    ) -> String {
        let dir = tempfile::tempdir().unwrap();
        let cfg = WriterConfig {
            format: crate::ExportFormat::Sql(SqlOptions::default()),
            dialect,
            binary: crate::BinaryFormat::Hex,
            content: crate::ExportContent::Data,
            dest: dir.path().join("t.sql"),
            overwrite: false,
            zip: false,
            tuning: crate::ExportTuning::default(),
            flatten_fields: vec![],
            ejson_cells: false,
            kind,
        };
        let mut w = SqlWriter::create(&cfg, SqlOptions::default()).unwrap();
        let columns: Vec<ColumnMeta> = types
            .iter()
            .enumerate()
            .map(|(i, t)| ColumnMeta {
                name: format!("c{i}"),
                type_name: (*t).into(),
                value_kind: ValueKind::Null,
                nullable: Some(true),
            })
            .collect();
        let meta = ObjectMeta {
            path: None,
            name: "t".into(),
            columns,
            details: None,
            estimated_rows: None,
        };
        w.begin_object(&meta).unwrap();
        w.on_rows(vec![row]).unwrap();
        w.end_object().unwrap();
        let files = Box::new(w).finish().unwrap();
        std::fs::read_to_string(&files[0]).unwrap()
    }

    #[test]
    fn cql_collections_are_written_as_cql_literals_not_json() {
        let t = |s: &str| Value::Text(s.into());
        let sql = script(
            ScriptDialect::Cql,
            dbstudio_core::DbKind::Cassandra,
            &["list<text>", "set<int>", "map<text,int>", "tuple<int,text>"],
            vec![
                Value::Array(vec![t("a"), t("b")]),
                Value::Array(vec![Value::Int(1), Value::Int(2)]),
                Value::Document(vec![("k".into(), Value::Int(1))]),
                Value::Array(vec![Value::Int(1), t("x")]),
            ],
        );
        assert!(sql.contains("['a', 'b']"), "{sql}");
        assert!(sql.contains("{1, 2}"), "{sql}");
        assert!(sql.contains("{'k': 1}"), "{sql}");
        assert!(sql.contains("(1, 'x')"), "{sql}");
        assert!(!sql.contains("[\\\"") && !sql.contains("{\\\""), "{sql}");
    }

    #[test]
    fn clickhouse_arrays_maps_and_tuples_are_written_as_literals() {
        let t = |s: &str| Value::Text(s.into());
        let sql = script(
            ScriptDialect::ClickHouse,
            dbstudio_core::DbKind::ClickHouse,
            &[
                "Array(String)",
                "Map(String, Int32)",
                "Tuple(Int32, String)",
            ],
            vec![
                Value::Array(vec![t("a"), t("b")]),
                Value::Document(vec![("k".into(), Value::Int(1))]),
                Value::Array(vec![Value::Int(1), t("x")]),
            ],
        );
        assert!(sql.contains("['a', 'b']"), "{sql}");
        assert!(sql.contains("map('k', 1)"), "{sql}");
        assert!(sql.contains("(1, 'x')"), "{sql}");
    }
}
