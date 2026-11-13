//! Integrated ("logical") backup: DDL + `INSERT` scripts written through the session, and the
//! matching restore.
//!
//! Content of the tar (see `container`): `schema/pre.sql` (schemas, sequences, tables in
//! dependency order), `data/NNNN-<object>.sql` (one `INSERT` script per table),
//! `schema/post.sql` (views, sequence ownership and values). Every object's statements are
//! wrapped in `-- @object NAME` / `-- @end` comments so a restore can pick a subset.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;

use dbstudio_core::{
    ColumnMeta, Error, JobProgress, JobReporter, ObjectKind, ObjectPath, QueryRequest, Result,
    ResultSink, ScriptDialect, Session, SortKey, StatementEnd, TableDataRequest, Value,
};
use dbstudio_export::{ExportContent, RowError};
use dbstudio_sql::Dialect;
use serde_json::{Value as Json, json};
use uuid::Uuid;

use crate::container::{Scanned, SpoolEntry, SpoolWriter};
use crate::sqlscan::{Marker, split_statements, strip_markers};

/// Rows fetched per `fetch_table` call.
const PAGE_ROWS: u64 = 5_000;
/// Rows per multi-row `INSERT`.
const INSERT_ROWS: usize = 100;
/// Flush an `INSERT` when its text exceeds this many bytes.
const INSERT_BYTES: usize = 512 * 1024;

/// Things a logical backup does not contain (shown in the preamble and the restore preview).
pub const UNSUPPORTED_SCOPE: [&str; 6] = [
    "roles/grants",
    "extensions",
    "functions and procedures",
    "triggers",
    "materialized views",
    "table partitioning details",
];

#[derive(Debug, Clone)]
pub(crate) struct Obj {
    pub kind: ObjectKind,
    pub path: ObjectPath,
    pub schema: Option<String>,
    pub name: String,
}

impl Obj {
    fn from_path(path: &ObjectPath, kind: ObjectKind) -> Option<(String, Obj)> {
        let segs: Vec<_> = path
            .segments
            .iter()
            .filter(|s| s.kind != ObjectKind::Folder)
            .collect();
        let db = segs.iter().find(|s| s.kind == ObjectKind::Database)?;
        let schema = segs
            .iter()
            .find(|s| s.kind == ObjectKind::Schema)
            .map(|s| s.name.clone());
        let last = segs.last()?;
        Some((
            db.name.clone(),
            Obj {
                kind,
                path: path.clone(),
                schema,
                name: last.name.clone(),
            },
        ))
    }

    /// Name shown in the preamble and used in markers.
    pub(crate) fn display(&self) -> String {
        match &self.schema {
            Some(s) => format!("{s}.{}", self.name),
            None => self.name.clone(),
        }
    }

    fn qualified(&self, d: &Dialect) -> String {
        match &self.schema {
            Some(s) => format!("{}.{}", d.quote_ident(s), d.quote_ident(&self.name)),
            None => d.quote_ident(&self.name),
        }
    }

    fn kind_name(&self) -> &'static str {
        match self.kind {
            ObjectKind::View => "view",
            ObjectKind::Sequence => "sequence",
            _ => "table",
        }
    }
}

fn dialect_name(d: ScriptDialect) -> Result<&'static str> {
    match d {
        ScriptDialect::Postgres => Ok("postgres"),
        ScriptDialect::MySql { .. } => Ok("mysql"),
        ScriptDialect::MsSql => Ok("mssql"),
        ScriptDialect::Oracle => Ok("oracle"),
        _ => Err(Error::Unsupported(
            "integrated backup is only available for SQL databases; use a native tool".into(),
        )),
    }
}

fn is_system_schema(name: &str) -> bool {
    matches!(name, "pg_catalog" | "information_schema" | "pg_toast")
        || name.starts_with("pg_temp")
        || name.starts_with("pg_toast_temp")
}

fn wanted_folder(name: &str) -> bool {
    matches!(
        name,
        dbstudio_core::folder::SCHEMAS
            | dbstudio_core::folder::TABLES
            | dbstudio_core::folder::VIEWS
            | dbstudio_core::folder::SEQUENCES
    )
}

/// Tables, views and sequences below `start`.
fn walk<'a>(
    session: &'a dyn Session,
    start: ObjectPath,
    out: &'a mut Vec<Obj>,
    cancel: &'a dyn JobReporter,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
    Box::pin(async move {
        if cancel.cancel_token().is_cancelled() {
            return Err(Error::Cancelled);
        }
        for node in session.list_children(&start).await? {
            match node.kind {
                ObjectKind::Folder if wanted_folder(&node.name) => {
                    walk(session, node.path, out, cancel).await?;
                }
                ObjectKind::Schema if !is_system_schema(&node.name) => {
                    walk(session, node.path, out, cancel).await?;
                }
                ObjectKind::Table | ObjectKind::View | ObjectKind::Sequence => {
                    if let Some((_, obj)) = Obj::from_path(&node.path, node.kind) {
                        out.push(obj);
                    }
                }
                _ => {}
            }
        }
        Ok(())
    })
}

/// Database name of a target path, when it has one.
pub(crate) fn database_of(path: &ObjectPath) -> Option<String> {
    path.segments
        .iter()
        .find(|s| s.kind == ObjectKind::Database)
        .map(|s| s.name.clone())
}

/// Objects selected by one target.
pub(crate) async fn discover(
    session: &dyn Session,
    target: &ObjectPath,
    reporter: &dyn JobReporter,
) -> Result<Vec<Obj>> {
    let mut out = Vec::new();
    let last = target
        .segments
        .iter()
        .rfind(|s| s.kind != ObjectKind::Folder);
    match last.map(|s| s.kind) {
        Some(k @ (ObjectKind::Table | ObjectKind::View | ObjectKind::Sequence)) => {
            if let Some((_, obj)) = Obj::from_path(target, k) {
                out.push(obj);
            }
        }
        _ => walk(session, target.clone(), &mut out, reporter).await?,
    }
    Ok(out)
}

/// Order tables so that referenced tables come first (cycles keep their listed order).
async fn order_tables(session: &dyn Session, tables: Vec<Obj>) -> Vec<Obj> {
    let names: BTreeSet<String> = tables.iter().map(Obj::display).collect();
    let mut deps: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for t in &tables {
        let mut set = BTreeSet::new();
        if let Ok(d) = session.table_details(&t.path).await {
            for fk in d.foreign_keys {
                if let Some((_, r)) = Obj::from_path(&fk.referenced_table, ObjectKind::Table) {
                    let n = r.display();
                    if n != t.display() && names.contains(&n) {
                        set.insert(n);
                    }
                }
            }
        }
        deps.insert(t.display(), set);
    }
    let mut remaining = tables;
    let mut done: BTreeSet<String> = BTreeSet::new();
    let mut ordered = Vec::new();
    while !remaining.is_empty() {
        let (ready, rest): (Vec<Obj>, Vec<Obj>) = remaining
            .into_iter()
            .partition(|t| deps[&t.display()].is_subset(&done));
        if ready.is_empty() {
            // Cycle: keep the rest as listed.
            ordered.extend(rest);
            break;
        }
        done.extend(ready.iter().map(Obj::display));
        ordered.extend(ready);
        remaining = rest;
    }
    ordered
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(80)
        .collect()
}

/// PostgreSQL array text literal body (`{a,"b"}`).
fn pg_array_text(items: &[Value]) -> String {
    let parts: Vec<String> = items
        .iter()
        .map(|v| match v {
            Value::Null => "NULL".to_string(),
            Value::Array(inner) => pg_array_text(inner),
            Value::Bool(b) => b.to_string(),
            Value::Int(i) => i.to_string(),
            Value::UInt(u) => u.to_string(),
            Value::Float(f) => f.to_string(),
            Value::Decimal(s) => s.clone(),
            Value::Bytes(b) => {
                let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
                format!("\"\\\\x{hex}\"")
            }
            Value::Text(s)
            | Value::Date(s)
            | Value::Time(s)
            | Value::DateTime(s)
            | Value::Timestamp(s)
            | Value::Uuid(s)
            | Value::Json(s) => quote_array_elem(s),
            Value::Ext(e) => quote_array_elem(&e.text),
            Value::Document(_) => "NULL".to_string(),
        })
        .collect();
    format!("{{{}}}", parts.join(","))
}

fn quote_array_elem(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn literal(d: &Dialect, v: &Value) -> String {
    match (d.0, v) {
        (ScriptDialect::Postgres, Value::Array(items)) => d.literal_text(&pg_array_text(items)),
        _ => d.literal(v),
    }
}

/// Writes `INSERT` statements for the rows it receives.
struct InsertSink<'a> {
    out: &'a mut SpoolWriter,
    dialect: Dialect,
    table: String,
    skip: &'a BTreeSet<String>,
    /// Indexes of the columns that are written.
    keep: Vec<usize>,
    head: String,
    pending: Vec<String>,
    pending_bytes: usize,
    rows: u64,
}

impl<'a> InsertSink<'a> {
    fn flush(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let body = self.pending.join(",\n");
        self.out.write_all(self.head.as_bytes())?;
        self.out.write_all(body.as_bytes())?;
        self.out.write_all(b";\n")?;
        self.pending.clear();
        self.pending_bytes = 0;
        Ok(())
    }
}

impl ResultSink for InsertSink<'_> {
    fn on_columns(&mut self, columns: Vec<ColumnMeta>) -> Result<()> {
        self.keep = columns
            .iter()
            .enumerate()
            .filter(|(_, c)| !self.skip.contains(&c.name))
            .map(|(i, _)| i)
            .collect();
        let names: Vec<String> = self
            .keep
            .iter()
            .map(|&i| self.dialect.quote_ident(&columns[i].name))
            .collect();
        let overriding = if matches!(self.dialect.0, ScriptDialect::Postgres) {
            " OVERRIDING SYSTEM VALUE"
        } else {
            ""
        };
        self.head = format!(
            "INSERT INTO {} ({}){} VALUES\n",
            self.table,
            names.join(", "),
            overriding
        );
        Ok(())
    }

    fn on_rows(&mut self, rows: Vec<Vec<Value>>) -> Result<()> {
        for row in rows {
            let cells: Vec<String> = self
                .keep
                .iter()
                .map(|&i| literal(&self.dialect, row.get(i).unwrap_or(&Value::Null)))
                .collect();
            let tuple = format!("({})", cells.join(", "));
            self.pending_bytes += tuple.len();
            self.pending.push(tuple);
            self.rows += 1;
            self.out.rows += 1;
            if self.pending.len() >= INSERT_ROWS || self.pending_bytes >= INSERT_BYTES {
                self.flush()?;
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

/// Discards everything (used to run DDL / `INSERT`).
struct NullSink;

impl ResultSink for NullSink {
    fn on_columns(&mut self, _: Vec<ColumnMeta>) -> Result<()> {
        Ok(())
    }
    fn on_rows(&mut self, _: Vec<Vec<Value>>) -> Result<()> {
        Ok(())
    }
    fn on_message(&mut self, _: String) -> Result<()> {
        Ok(())
    }
    fn on_statement_end(&mut self, _: StatementEnd) -> Result<()> {
        Ok(())
    }
}

async fn dump_table(
    session: &dyn Session,
    dialect: &Dialect,
    obj: &Obj,
    out: &mut SpoolWriter,
    reporter: &dyn JobReporter,
    progress: &mut JobProgress,
) -> Result<()> {
    let details = session.table_details(&obj.path).await.ok();
    let mut skip = BTreeSet::new();
    let mut sort = Vec::new();
    if let Some(d) = &details {
        for c in &d.columns {
            let extra = c.extra.as_deref().unwrap_or("").to_ascii_lowercase();
            if extra.contains("generated") && !extra.contains("identity") {
                skip.insert(c.name.clone());
            }
        }
        let mut pk: Vec<_> = d.columns.iter().filter(|c| c.primary_key).collect();
        pk.sort_by_key(|c| c.ordinal);
        sort = pk
            .into_iter()
            .map(|c| SortKey {
                column: c.name.clone(),
                descending: false,
            })
            .collect();
        if sort.is_empty() {
            if let Some(ix) = d.indexes.iter().find(|i| i.primary || i.unique) {
                sort = ix
                    .columns
                    .iter()
                    .map(|c| SortKey {
                        column: c.clone(),
                        descending: false,
                    })
                    .collect();
            }
        }
    }
    let table = obj.qualified(dialect);
    let mut offset = 0u64;
    loop {
        if reporter.cancel_token().is_cancelled() {
            return Err(Error::Cancelled);
        }
        let mut sink = InsertSink {
            out,
            dialect: *dialect,
            table: table.clone(),
            skip: &skip,
            keep: Vec::new(),
            head: String::new(),
            pending: Vec::new(),
            pending_bytes: 0,
            rows: 0,
        };
        session
            .fetch_table(
                TableDataRequest {
                    query_id: Uuid::new_v4(),
                    table: obj.path.clone(),
                    filters: Vec::new(),
                    raw_where: None,
                    sort: sort.clone(),
                    offset,
                    limit: PAGE_ROWS,
                    batch_size: Some(1000),
                },
                &mut sink,
            )
            .await?;
        sink.flush()?;
        let n = sink.rows;
        offset += n;
        progress.rows_done += n;
        progress.bytes_done += 0;
        reporter.progress(progress);
        if n < PAGE_ROWS {
            return Ok(());
        }
    }
}

fn marker_block(name: &str, sql: &str) -> String {
    let body = sql.trim_end();
    // `SHOW CREATE` output has no terminator.
    let semi = if body.is_empty() || body.ends_with(';') {
        ""
    } else {
        ";"
    };
    format!("-- @object {name}\n{body}{semi}\n-- @end\n\n")
}

/// Split a PostgreSQL sequence DDL into the `CREATE` part and the `OWNED BY` part.
fn split_sequence_ddl(ddl: &str) -> (String, Option<String>) {
    match ddl.find("ALTER SEQUENCE") {
        Some(i) => (
            ddl[..i].trim_end().to_string(),
            Some(ddl[i..].trim_end().to_string()),
        ),
        None => (ddl.trim_end().to_string(), None),
    }
}

/// `schema.table` of an `ALTER SEQUENCE .. OWNED BY "schema"."table"."column"` statement.
fn owner_table(alter: &str) -> Option<String> {
    let rest = alter.split("OWNED BY").nth(1)?.trim().trim_end_matches(';');
    // `format('%I.%I.%I')` output: identifiers are quoted only when needed.
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = rest.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, quoted) {
            ('"', false) => quoted = true,
            ('"', true) if chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            ('"', true) => quoted = false,
            ('.', false) => parts.push(std::mem::take(&mut cur)),
            (c, _) => cur.push(c),
        }
    }
    parts.push(cur);
    (parts.len() == 3).then(|| format!("{}.{}", parts[0], parts[1]))
}

/// What the dump produced for one database.
pub(crate) struct Dump {
    pub entries: Vec<SpoolEntry>,
    pub preamble: Json,
    pub objects: u32,
    pub rows: u64,
}

/// Dump the objects of `database` selected by `targets` (empty = the whole database).
pub(crate) async fn dump(
    session: &dyn Session,
    spool: &Path,
    database: &str,
    targets: &[ObjectPath],
    content: ExportContent,
    source: Json,
    reporter: &dyn JobReporter,
) -> Result<Dump> {
    let sd = session.script_dialect();
    let dname = dialect_name(sd)?;
    let dialect = Dialect::new(sd);
    let mut found = Vec::new();
    for t in targets {
        found.extend(discover(session, t, reporter).await?);
    }
    let mut seen = BTreeSet::new();
    found.retain(|o| seen.insert((o.kind_name(), o.display())));
    let sequences: Vec<Obj> = found
        .iter()
        .filter(|o| o.kind == ObjectKind::Sequence)
        .cloned()
        .collect();
    let views: Vec<Obj> = found
        .iter()
        .filter(|o| o.kind == ObjectKind::View)
        .cloned()
        .collect();
    let tables = order_tables(
        session,
        found
            .into_iter()
            .filter(|o| o.kind == ObjectKind::Table)
            .collect(),
    )
    .await;

    let mut progress = JobProgress {
        phase: "schema".into(),
        objects_total: Some((tables.len() + views.len() + sequences.len()) as u32),
        ..Default::default()
    };
    reporter.progress(&progress);

    let want_schema = content != ExportContent::Data;
    let want_data = content != ExportContent::Schema;
    let mut entries = Vec::new();
    let mut idx = 0usize;
    let mut pre = String::from("-- DBStudio logical backup: structure\n\n");
    let mut post = String::from("-- DBStudio logical backup: views, sequence state\n\n");
    let mut objects_json = Vec::new();

    if want_schema {
        // Schemas other than the default one (PostgreSQL).
        if sd == ScriptDialect::Postgres {
            let schemas: BTreeSet<&String> = tables
                .iter()
                .chain(&views)
                .chain(&sequences)
                .filter_map(|o| o.schema.as_ref())
                .filter(|s| s.as_str() != "public")
                .collect();
            for s in schemas {
                pre.push_str(&format!(
                    "CREATE SCHEMA IF NOT EXISTS {};\n",
                    dialect.quote_ident(s)
                ));
            }
            pre.push('\n');
        }
        for s in &sequences {
            let ddl = session.object_ddl(&s.path).await?;
            let (create, owned) = split_sequence_ddl(&ddl);
            // A sequence owned by a column travels with its table in a subset restore.
            let marker = owned
                .as_deref()
                .and_then(owner_table)
                .unwrap_or_else(|| s.display());
            pre.push_str(&marker_block(&marker, &create));
            let mut tail = String::new();
            if let Some(o) = owned {
                tail.push_str(&o);
                tail.push('\n');
            }
            if sd == ScriptDialect::Postgres {
                let sql = format!(
                    "SELECT last_value, is_called FROM {}",
                    s.qualified(&dialect)
                );
                let mut req = QueryRequest::new(sql);
                req.database = Some(database.to_string());
                let r = dbstudio_core::execute_collect(session, req).await?;
                if let Some(row) = r.iter().find_map(|r| match &r.outcome {
                    dbstudio_core::StatementOutcome::Rows(rs) => rs.rows.first().cloned(),
                    _ => None,
                }) {
                    if let (Some(Value::Int(last)), Some(Value::Bool(called))) =
                        (row.first(), row.get(1))
                    {
                        tail.push_str(&format!(
                            "SELECT pg_catalog.setval({}, {last}, {called});\n",
                            dialect.literal_text(&s.qualified(&dialect))
                        ));
                    }
                }
            }
            post.push_str(&marker_block(&marker, &tail));
            objects_json.push(json!({"name": s.display(), "kind": "sequence"}));
            progress.objects_done += 1;
        }
        for t in &tables {
            let ddl = session.object_ddl(&t.path).await?;
            pre.push_str(&marker_block(&t.display(), &ddl));
            progress.object = Some(t.display());
            progress.objects_done += 1;
            reporter.progress(&progress);
        }
        for v in &views {
            let ddl = session.object_ddl(&v.path).await?;
            post.push_str(&marker_block(&v.display(), &ddl));
            objects_json.push(json!({"name": v.display(), "kind": "view"}));
            progress.objects_done += 1;
        }
    }

    let mut rows_total = 0u64;
    let mut table_json = Vec::new();
    if want_data {
        progress.phase = "data".into();
        progress.objects_done = 0;
        for t in &tables {
            if reporter.cancel_token().is_cancelled() {
                return Err(Error::Cancelled);
            }
            progress.object = Some(t.display());
            reporter.progress(&progress);
            idx += 1;
            let name = format!("data/{idx:04}-{}.sql", sanitize(&t.display()));
            let mut w = SpoolWriter::create(spool, idx, &name)?;
            dump_table(session, &dialect, t, &mut w, reporter, &mut progress).await?;
            rows_total += w.rows;
            entries.push(w.finish()?);
            table_json.push(json!({"name": t.display(), "kind": "table", "dataFile": name}));
            progress.objects_done += 1;
        }
    } else {
        for t in &tables {
            table_json.push(json!({"name": t.display(), "kind": "table"}));
        }
    }
    let mut all_objects = table_json;
    all_objects.extend(objects_json);

    let mut head = Vec::new();
    if want_schema {
        for (n, text) in [("schema/pre.sql", &pre), ("schema/post.sql", &post)] {
            idx += 1;
            let mut w = SpoolWriter::create(spool, 10_000 + idx, n)?;
            w.write_all(text.as_bytes())?;
            head.push(w.finish()?);
        }
    }
    head.extend(entries);

    let preamble = json!({
        "formatVersion": 1,
        "engine": "logical",
        "createdAt": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "database": database,
        "source": source,
        "dialect": dname,
        "backslashEscapes": matches!(sd, ScriptDialect::MySql { backslash_escapes: true, .. }),
        "content": match content {
            ExportContent::Schema => "schema",
            ExportContent::Data => "data",
            ExportContent::Both => "both",
        },
        "objects": all_objects,
        "unsupported": UNSUPPORTED_SCOPE,
    });
    Ok(Dump {
        objects: all_objects_count(&preamble),
        rows: rows_total,
        entries: head,
        preamble,
    })
}

fn all_objects_count(preamble: &Json) -> u32 {
    preamble["objects"].as_array().map_or(0, |a| a.len() as u32)
}

// ---------------------------------------------------------------------------------------------
// Restore
// ---------------------------------------------------------------------------------------------

/// Statement runner shared by the restore phases.
struct Runner<'a> {
    session: &'a dyn Session,
    database: String,
    errors: Vec<RowError>,
    continue_on_error: bool,
    line: u64,
}

impl Runner<'_> {
    async fn run(&mut self, sql: &str) -> Result<bool> {
        self.line += 1;
        let mut req = QueryRequest::new(sql.to_string());
        req.database = Some(self.database.clone());
        match self.session.execute(req, &mut NullSink).await {
            Ok(_) => Ok(true),
            Err(Error::Cancelled) => Err(Error::Cancelled),
            Err(e) if self.continue_on_error => {
                self.errors.push(RowError {
                    line: self.line,
                    message: e.message(),
                });
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }
}

/// Statements of a script file that belong to the selection.
fn select_statements(
    text: &str,
    backslash: bool,
    subset: &BTreeSet<String>,
) -> Vec<(Option<String>, String)> {
    let mut current: Option<String> = None;
    let mut out = Vec::new();
    for raw in split_statements(text, backslash) {
        let (markers, sql) = strip_markers(&raw);
        for m in markers {
            match m {
                Marker::Object(n) => current = Some(n),
                Marker::End => current = None,
            }
        }
        if sql.is_empty() {
            continue;
        }
        let wanted = match &current {
            Some(n) => subset.is_empty() || subset.contains(n),
            None => true,
        };
        if wanted {
            out.push((current.clone(), sql.to_string()));
        }
    }
    out
}

fn same_family(a: &str, b: ScriptDialect) -> bool {
    matches!(
        (a, b),
        ("postgres", ScriptDialect::Postgres)
            | ("mysql", ScriptDialect::MySql { .. })
            | ("mssql", ScriptDialect::MsSql)
            | ("oracle", ScriptDialect::Oracle)
    )
}

/// Restore the extracted logical backup into `session`.
pub(crate) async fn restore(
    session: &dyn Session,
    scanned: &Scanned,
    spec: &crate::RestoreSpec,
    reporter: &dyn JobReporter,
) -> Result<crate::RestoreOutcome> {
    let pre = &scanned.preamble;
    let src_dialect = pre["dialect"].as_str().unwrap_or("");
    let sd = session.script_dialect();
    if dialect_name(sd).is_err() || !same_family(src_dialect, sd) {
        return Err(Error::Unsupported(format!(
            "this backup was made from {src_dialect}; restore it into a server of the same kind"
        )));
    }
    let dialect = Dialect::new(sd);
    let backslash = pre["backslashEscapes"].as_bool().unwrap_or(false);
    let target_db = spec
        .target_database
        .clone()
        .or_else(|| pre["database"].as_str().map(str::to_string))
        .ok_or_else(|| Error::Other("target database is not specified".into()))?;
    let subset: BTreeSet<String> = spec.objects.iter().cloned().collect();
    let mut runner = Runner {
        session,
        database: target_db.clone(),
        errors: Vec::new(),
        continue_on_error: spec.continue_on_error,
        line: 0,
    };
    let mut progress = JobProgress {
        phase: "restore".into(),
        ..Default::default()
    };
    reporter.progress(&progress);

    if spec.create_database {
        let (sql, over) = match sd {
            ScriptDialect::Postgres => (
                format!("CREATE DATABASE {}", dialect.quote_ident(&target_db)),
                Some("postgres"),
            ),
            _ => (
                format!(
                    "CREATE DATABASE IF NOT EXISTS {}",
                    dialect.quote_ident(&target_db)
                ),
                None,
            ),
        };
        let mut req = QueryRequest::new(sql);
        req.database = over.map(str::to_string);
        if let Err(e) = session.execute(req, &mut NullSink).await {
            // An existing database is fine: the data goes into it.
            if !e.message().contains("already exists") {
                return Err(e);
            }
        }
    }

    let objects = pre["objects"].as_array().cloned().unwrap_or_default();
    let selected =
        |o: &Json| subset.is_empty() || subset.contains(o["name"].as_str().unwrap_or(""));

    if spec.clean {
        // Reverse of creation order: views, tables (reverse dependency order), sequences.
        let names = |kind: &str| -> Vec<String> {
            objects
                .iter()
                .filter(|o| o["kind"] == kind && selected(o))
                .filter_map(|o| o["name"].as_str().map(str::to_string))
                .collect()
        };
        let quote = |display: &str| -> String {
            match display.split_once('.') {
                Some((s, n)) if sd == ScriptDialect::Postgres => {
                    format!("{}.{}", dialect.quote_ident(s), dialect.quote_ident(n))
                }
                _ => dialect.quote_ident(display),
            }
        };
        let cascade = if sd == ScriptDialect::Postgres {
            " CASCADE"
        } else {
            ""
        };
        for v in names("view") {
            runner
                .run(&format!("DROP VIEW IF EXISTS {}{cascade}", quote(&v)))
                .await?;
        }
        for t in names("table").into_iter().rev() {
            runner
                .run(&format!("DROP TABLE IF EXISTS {}{cascade}", quote(&t)))
                .await?;
        }
        for s in names("sequence") {
            runner
                .run(&format!("DROP SEQUENCE IF EXISTS {}{cascade}", quote(&s)))
                .await?;
        }
    }

    let read = |name: &str| -> Result<Option<String>> {
        match scanned.files.get(name) {
            Some(p) => Ok(Some(std::fs::read_to_string(p)?)),
            None => Ok(None),
        }
    };

    // Structure.
    if let Some(text) = read("schema/pre.sql")? {
        for (_, sql) in select_statements(&text, backslash, &subset) {
            if reporter.cancel_token().is_cancelled() {
                return Err(Error::Cancelled);
            }
            runner.run(&sql).await?;
        }
    }

    // Data.
    progress.phase = "data".into();
    progress.objects_total = Some(
        objects
            .iter()
            .filter(|o| o["dataFile"].is_string() && selected(o))
            .count() as u32,
    );
    let mut rows_done = 0u64;
    let mut restored = 0u32;
    for o in objects.iter().filter(|o| selected(o)) {
        let Some(file) = o["dataFile"].as_str() else {
            continue;
        };
        let Some(text) = read(file)? else { continue };
        progress.object = o["name"].as_str().map(str::to_string);
        let errors_before = runner.errors.len();
        for (_, sql) in select_statements(&text, backslash, &BTreeSet::new()) {
            if reporter.cancel_token().is_cancelled() {
                return Err(Error::Cancelled);
            }
            runner.run(&sql).await?;
        }
        if runner.errors.len() == errors_before {
            let rows = scanned
                .manifest
                .entries
                .iter()
                .find(|e| e.path == file)
                .map_or(0, |e| e.rows);
            rows_done += rows;
        }
        restored += 1;
        progress.objects_done += 1;
        progress.rows_done = rows_done;
        reporter.progress(&progress);
    }

    // Views and sequence state; a view may depend on a later one, so retry failures.
    if let Some(text) = read("schema/post.sql")? {
        let mut pending: Vec<String> = select_statements(&text, backslash, &subset)
            .into_iter()
            .map(|(_, s)| s)
            .collect();
        let saved = runner.continue_on_error;
        for pass in 0..3 {
            let mut failed = Vec::new();
            let last = pass == 2;
            runner.continue_on_error = saved || !last;
            let before = runner.errors.len();
            for sql in &pending {
                if reporter.cancel_token().is_cancelled() {
                    return Err(Error::Cancelled);
                }
                if !runner.run(sql).await? {
                    failed.push(sql.clone());
                }
            }
            if failed.is_empty() {
                runner.errors.truncate(before);
                break;
            }
            if !last {
                runner.errors.truncate(before);
            }
            pending = failed;
        }
        runner.continue_on_error = saved;
    }

    Ok(crate::RestoreOutcome {
        objects: restored,
        rows: rows_done,
        errors: runner.errors,
    })
}

/// Pure helper for tests: statements selected from a script for `subset`.
#[cfg(test)]
pub(crate) fn selected_for_test(text: &str, subset: &[&str]) -> Vec<String> {
    let set = subset.iter().map(|s| s.to_string()).collect();
    select_statements(text, false, &set)
        .into_iter()
        .map(|(_, s)| s)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subset_selection_follows_markers() {
        let script = "-- @object a\nCREATE TABLE a (x int);\nCREATE INDEX i ON a (x);\n-- @end\n\n\
                      -- @object b\nCREATE TABLE b (y int);\n-- @end\n\nSELECT 1;\n";
        assert_eq!(
            selected_for_test(script, &["a"]),
            [
                "CREATE TABLE a (x int)",
                "CREATE INDEX i ON a (x)",
                "SELECT 1"
            ]
        );
        assert_eq!(selected_for_test(script, &[]).len(), 4);
        assert_eq!(
            selected_for_test(script, &["b"]),
            ["CREATE TABLE b (y int)", "SELECT 1"]
        );
    }

    #[test]
    fn sequence_ddl_is_split_and_arrays_are_quoted() {
        let (a, b) = split_sequence_ddl("CREATE SEQUENCE s;\n\nALTER SEQUENCE s OWNED BY t.c;\n");
        assert_eq!(a, "CREATE SEQUENCE s;");
        assert_eq!(b.as_deref(), Some("ALTER SEQUENCE s OWNED BY t.c;"));
        assert_eq!(
            pg_array_text(&[
                Value::Int(1),
                Value::Null,
                Value::Text("a\"b".into()),
                Value::Array(vec![Value::Bool(true)]),
            ]),
            "{1,NULL,\"a\\\"b\",{true}}"
        );
        let d = Dialect::new(ScriptDialect::Postgres);
        assert_eq!(
            literal(&d, &Value::Array(vec![Value::Text("x'y".into())])),
            "'{\"x''y\"}'"
        );
    }

    #[test]
    fn owned_sequences_follow_their_table() {
        assert_eq!(
            owner_table("ALTER SEQUENCE app.s OWNED BY app.customers.id;").as_deref(),
            Some("app.customers")
        );
        assert_eq!(
            owner_table("ALTER SEQUENCE s OWNED BY \"My App\".\"Tab\"\"x\".id;").as_deref(),
            Some("My App.Tab\"x")
        );
        assert_eq!(owner_table("CREATE SEQUENCE s;"), None);
    }

    #[test]
    fn names_are_sanitized() {
        assert_eq!(sanitize("public.my table/../x"), "public.my_table_.._x");
    }
}
