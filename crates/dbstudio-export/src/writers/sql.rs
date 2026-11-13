//! SQL script writer: DDL, then batched `INSERT` statements in the dialect of the source.

use std::io::Write;
use std::path::PathBuf;

use dbstudio_core::{
    ColumnMeta, DdlItem, DdlSection, Error, Result, ResultSink, ScriptDialect, StatementEnd, Value,
};
use dbstudio_sql::Dialect;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::{WriterConfig, qualifier};
use crate::fsio::Output;
use crate::{ObjectMeta, SqlOptions, TableWriter};

/// SQL Server accepts at most 1000 rows in one `VALUES` list.
const MSSQL_MAX_ROWS: usize = 1000;
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
}

impl SqlWriter {
    pub fn create(cfg: &WriterConfig, opts: SqlOptions) -> Result<SqlWriter> {
        Ok(SqlWriter {
            out: Output::create(&cfg.dest, cfg.overwrite, false)?,
            q: Dialect::new(cfg.dialect),
            dialect: cfg.dialect,
            opts,
            buf: String::new(),
            started: false,
            tx_open: false,
            table: None,
            wrote_any: false,
        })
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
        if matches!(self.dialect, ScriptDialect::MySql { .. }) {
            self.buf.push_str("SET NAMES utf8mb4;\n");
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
                self.buf.push_str(b);
                self.buf.push('\n');
                self.tx_open = true;
            }
        }
    }

    fn close_tx(&mut self) {
        if self.tx_open {
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

    fn row_tuple(&self, row: &[Value]) -> String {
        let mut s = String::from("(");
        for (i, v) in row.iter().enumerate() {
            if i > 0 {
                s.push_str(", ");
            }
            s.push_str(&sql_literal(self.q, self.dialect, v));
        }
        s.push(')');
        s
    }
}

/// SQL literal of a value in the source dialect. PostgreSQL arrays become array literals,
/// documents become JSON text.
pub(crate) fn sql_literal(q: Dialect, dialect: ScriptDialect, v: &Value) -> String {
    match v {
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
        for row in &rows {
            let tuple = self.row_tuple(row);
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
        for item in items {
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
        let columns = if self.opts.include_column_names {
            let list: Vec<String> = meta
                .columns
                .iter()
                .map(|c| q.quote_ident(&c.name))
                .collect();
            format!(" ({})", list.join(", "))
        } else {
            String::new()
        };
        let per_row = matches!(self.dialect, ScriptDialect::Oracle);
        let mut batch_rows = self.opts.insert_batch_rows.max(1) as usize;
        if matches!(self.dialect, ScriptDialect::MsSql) {
            batch_rows = batch_rows.min(MSSQL_MAX_ROWS);
        }
        self.buf
            .push_str(&format!("-- Data of {}\n", meta.name.replace('\n', " ")));
        self.table = Some(TableState {
            prefix: format!("INSERT INTO {name}{columns} VALUES"),
            stmt: String::new(),
            rows_in_stmt: 0,
            per_row,
            batch_rows,
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

    fn finish(mut self: Box<Self>) -> Result<Vec<PathBuf>> {
        self.prologue();
        self.flush_stmt();
        self.close_tx();
        if self.opts.disable_fk_checks {
            if let Some((_, on)) = self.fk_off() {
                self.buf.push_str(on);
                self.buf.push('\n');
            }
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
            "'{1,\"a \\\"b\\\"\",NULL,{t}}'"
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
}
