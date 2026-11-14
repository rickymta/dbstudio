//! Shared state of one connection (`Core`) and the statement engine used by both the session and
//! its leases: script execution, paging, cqlsh-style commands, cancellation.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dbstudio_core::{
    ColumnMeta, EndOutcome, Error, ExecSummary, QueryRequest, Result, ResultSink, StatementEnd,
    StatementStart, TextRange, Value,
};
use scylla::client::session::Session;
use scylla::response::PagingState;
use scylla::statement::{Consistency, SerialConsistency, Statement};
use scylla::value::Row as ScyllaRow;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::conn::{Pool, consistency_name, parse_consistency};
use crate::convert::to_value;
use crate::error;
use crate::rows::query_rows;
use crate::schema::SchemaCache;
use crate::types::CqlType;

/// Default rows per streamed batch / server page.
pub(crate) const DEFAULT_BATCH: usize = 1000;

/// Facts about the server read at connect time.
#[derive(Debug, Clone, Default)]
pub(crate) struct ServerFacts {
    pub version: String,
    pub cluster_name: String,
    pub data_center: String,
    pub partitioner: String,
    pub scylla: bool,
}

impl ServerFacts {
    pub(crate) fn flavor(&self) -> &'static str {
        if self.scylla { "scylladb" } else { "cassandra" }
    }
}

/// State a console keeps between statements (a lease, or one script of `Session::execute`).
#[derive(Debug, Clone)]
pub(crate) struct ExecState {
    pub keyspace: Option<String>,
    pub consistency: Consistency,
    pub serial: Option<SerialConsistency>,
    pub tracing: bool,
}

/// A result set that hit `max_rows` and can continue.
pub(crate) struct Pending {
    pub session: Arc<Session>,
    pub text: String,
    pub state: PagingState,
    pub consistency: Consistency,
    pub serial: Option<SerialConsistency>,
}

/// Paging states by (query, offset) so the data grid can page forward (CQL has no OFFSET).
#[derive(Default)]
pub(crate) struct PagingCache {
    entries: Vec<((String, u64), PagingState)>,
}

const PAGING_CACHE_SIZE: usize = 64;

impl PagingCache {
    pub(crate) fn put(&mut self, query: String, offset: u64, state: PagingState) {
        self.entries
            .retain(|((q, o), _)| !(q == &query && *o == offset));
        self.entries.insert(0, ((query, offset), state));
        self.entries.truncate(PAGING_CACHE_SIZE);
    }

    /// The state at the largest cached offset `<= offset` of `query`.
    pub(crate) fn best(&mut self, query: &str, offset: u64) -> Option<(u64, PagingState)> {
        let idx = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, ((q, o), _))| q == query && *o <= offset)
            .max_by_key(|(_, ((_, o), _))| *o)
            .map(|(i, _)| i)?;
        let entry = self.entries.remove(idx);
        let out = (entry.0.1, entry.1.clone());
        self.entries.insert(0, entry);
        Some(out)
    }
}

/// Everything of a connection that sessions and leases share.
pub(crate) struct Core {
    pub pool: Pool,
    pub cache: SchemaCache,
    pub facts: ServerFacts,
    pub paging: Mutex<PagingCache>,
    cancels: Mutex<HashMap<Uuid, CancellationToken>>,
}

/// Removes a registered query when dropped.
pub(crate) struct CancelGuard<'a> {
    core: &'a Core,
    id: Uuid,
    pub token: CancellationToken,
}

impl Drop for CancelGuard<'_> {
    fn drop(&mut self) {
        self.core
            .cancels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.id);
    }
}

/// Character ranges of the statements of a script: the same splitter the guard analyses, so what
/// is confirmed is what runs.
pub(crate) fn statement_ranges(text: &str) -> Vec<std::ops::Range<usize>> {
    dbstudio_sql::split(dbstudio_core::ScriptDialect::Cql, text)
        .into_iter()
        .map(|r| r.start as usize..r.end as usize)
        .collect()
}

/// First words of a statement (comments skipped), lowercase, at most `n`.
pub(crate) fn first_words(text: &str, n: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    let mut words = Vec::new();
    while i < chars.len() && words.len() < n {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '-' && chars.get(i + 1) == Some(&'-')
            || c == '/' && chars.get(i + 1) == Some(&'/')
        {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                i += 1;
            }
            i += 2;
        } else if c.is_alphanumeric() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            words.push(chars[start..i].iter().collect::<String>().to_lowercase());
        } else {
            words.push(c.to_string());
            i += 1;
        }
    }
    words
}

/// What a statement is, for routing and the read-only check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Kind {
    Select,
    Use(String),
    Consistency(Option<String>),
    SerialConsistency(Option<String>),
    Tracing(Option<bool>),
    Describe,
    Unsupported(&'static str),
    /// A shell command with a malformed argument.
    Invalid(&'static str),
    /// Read-only server statement that is not a SELECT (`LIST ...`).
    Other,
    Write,
}

fn unquote_ident(s: &str) -> String {
    let t = s.trim().trim_end_matches(';').trim();
    if let Some(inner) = t.strip_prefix('"').and_then(|x| x.strip_suffix('"')) {
        inner.replace("\"\"", "\"")
    } else {
        t.to_ascii_lowercase()
    }
}

/// `name` or `"quoted ""name"""`, nothing else.
fn single_identifier(s: &str) -> bool {
    let t = s.trim();
    if let Some(inner) = t.strip_prefix('"').and_then(|x| x.strip_suffix('"')) {
        return !inner.is_empty() && !inner.replace("\"\"", "").contains('"');
    }
    !t.is_empty() && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Everything after the first `n` words of `text` (raw).
fn after_words(text: &str, n: usize) -> String {
    let mut rest = text.trim_start();
    for _ in 0..n {
        rest = rest.trim_start();
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        rest = &rest[end..];
    }
    rest.trim().trim_end_matches(';').trim().to_string()
}

pub(crate) fn classify(text: &str) -> Kind {
    let w = first_words(text, 3);
    let first = w.first().map(String::as_str).unwrap_or("");
    match first {
        "select" => Kind::Select,
        "use" => {
            let arg = after_words(text, 1);
            if single_identifier(&arg) {
                Kind::Use(unquote_ident(&arg))
            } else {
                Kind::Invalid("USE takes exactly one keyspace name")
            }
        }
        "consistency" => {
            let arg = after_words(text, 1);
            Kind::Consistency((!arg.is_empty()).then_some(arg))
        }
        "serial" if w.get(1).map(String::as_str) == Some("consistency") => {
            let arg = after_words(text, 2);
            Kind::SerialConsistency((!arg.is_empty()).then_some(arg))
        }
        "tracing" => {
            let arg = after_words(text, 1).to_ascii_lowercase();
            Kind::Tracing(match arg.as_str() {
                "on" => Some(true),
                "off" => Some(false),
                _ => None,
            })
        }
        "describe" | "desc" => Kind::Describe,
        "copy" => {
            Kind::Unsupported("COPY is a cqlsh feature; use DBStudio export / import instead")
        }
        "source" => Kind::Unsupported("SOURCE is a cqlsh feature; open the file in the editor"),
        "paging" | "expand" | "capture" | "clear" | "cls" | "login" | "show" | "unicode" => {
            Kind::Unsupported("this cqlsh command is not available in DBStudio")
        }
        "list" => Kind::Other,
        _ => Kind::Write,
    }
}

/// Whether a read-only connection may run this statement.
fn allowed_read_only(kind: &Kind) -> bool {
    !matches!(kind, Kind::Write)
}

fn add_position(e: Error, base: usize) -> Error {
    match e {
        Error::Query {
            message,
            position: Some(p),
        } => Error::Query {
            message,
            position: Some(p + base as u32),
        },
        other => other,
    }
}

impl Core {
    pub(crate) fn new(pool: Pool, facts: ServerFacts) -> Core {
        Core {
            pool,
            cache: SchemaCache::default(),
            facts,
            paging: Mutex::new(PagingCache::default()),
            cancels: Mutex::new(HashMap::new()),
        }
    }

    /// Register a cancellable request.
    pub(crate) fn register(&self, id: Uuid) -> CancelGuard<'_> {
        let token = CancellationToken::new();
        self.cancels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, token.clone());
        CancelGuard {
            core: self,
            id,
            token,
        }
    }

    /// Cancel a registered request (unknown id: nothing happens).
    pub(crate) fn cancel(&self, id: Uuid) {
        if let Some(t) = self
            .cancels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
        {
            t.cancel();
        }
    }

    pub(crate) fn initial_state(&self, keyspace: Option<String>) -> ExecState {
        ExecState {
            keyspace: keyspace.or_else(|| self.pool.settings.default_keyspace.clone()),
            consistency: self.pool.settings.consistency,
            serial: None,
            tracing: false,
        }
    }

    /// Drop caches that a schema change invalidates.
    fn after_write(&self, text: &str) {
        let w = first_words(text, 2);
        self.cache.invalidate();
        if w.first().map(String::as_str) == Some("drop")
            && matches!(w.get(1).map(String::as_str), Some("keyspace" | "schema"))
        {
            self.pool.clear();
        }
    }

    /// Run a script. `pending`, when given, receives the open result set of the last statement
    /// that hit `max_rows` (lease semantics); without it the rest of such a set is dropped.
    pub(crate) async fn run_script(
        &self,
        state: &mut ExecState,
        req: &QueryRequest,
        sink: &mut dyn ResultSink,
        mut pending: Option<&mut Option<Pending>>,
    ) -> Result<ExecSummary> {
        let started = Instant::now();
        let guard = self.register(req.query_id);
        let chars: Vec<char> = req.text.chars().collect();
        let ranges = statement_ranges(&req.text);
        let mut executed = 0u32;
        for (index, range) in ranges.iter().enumerate() {
            let text: String = chars[range.clone()].iter().collect();
            sink.on_statement_start(StatementStart {
                index: index as u32,
                range: TextRange {
                    start: range.start as u32,
                    end: range.end as u32,
                },
            })?;
            if let Some(p) = pending.as_deref_mut() {
                *p = None;
            }
            let is_last = index + 1 == ranges.len();
            let keep = pending.is_some() && is_last;
            let fut = self.run_statement(state, &text, req, sink, keep);
            let outcome = tokio::select! {
                r = fut => r,
                _ = guard.token.cancelled() => Err(Error::Cancelled),
            };
            match outcome {
                Ok(p) => {
                    if let (Some(slot), Some(p)) = (pending.as_deref_mut(), p) {
                        *slot = Some(p);
                    }
                }
                Err(e) => return Err(add_position(e, range.start)),
            }
            executed += 1;
        }
        Ok(ExecSummary {
            statements: executed,
            elapsed_ms: started.elapsed().as_millis() as u64,
        })
    }

    async fn run_statement(
        &self,
        state: &mut ExecState,
        text: &str,
        req: &QueryRequest,
        sink: &mut dyn ResultSink,
        keep_open: bool,
    ) -> Result<Option<Pending>> {
        let started = Instant::now();
        let kind = classify(text);
        if self.pool.settings.read_only && !allowed_read_only(&kind) {
            return Err(Error::ReadOnlyViolation {
                message: "the connection is read-only: this statement may modify data or schema"
                    .into(),
                position: Some(0),
            });
        }
        let done = |sink: &mut dyn ResultSink, started: Instant| {
            sink.on_statement_end(StatementEnd {
                outcome: EndOutcome::Done,
                elapsed_ms: started.elapsed().as_millis() as u64,
            })
        };
        match kind {
            Kind::Unsupported(msg) => Err(Error::Unsupported(msg.into())),
            Kind::Invalid(msg) => Err(Error::Invalid(msg.into())),
            Kind::Use(ks) => {
                // Fails when the keyspace does not exist.
                self.pool.session(Some(&ks)).await?;
                state.keyspace = Some(ks.clone());
                sink.on_message(format!("Now using keyspace {ks}."))?;
                done(sink, started)?;
                Ok(None)
            }
            Kind::Consistency(arg) => {
                match arg {
                    None => sink.on_message(format!(
                        "Current consistency level is {}.",
                        consistency_name(state.consistency)
                    ))?,
                    Some(a) => {
                        let level = parse_consistency(&a).ok_or_else(|| {
                            Error::Invalid(format!("unknown consistency level `{a}`"))
                        })?;
                        state.consistency = level;
                        sink.on_message(format!(
                            "Consistency level set to {}.",
                            consistency_name(level)
                        ))?;
                    }
                }
                done(sink, started)?;
                Ok(None)
            }
            Kind::SerialConsistency(arg) => {
                match arg {
                    None => sink.on_message(format!(
                        "Current serial consistency level is {}.",
                        state.serial.map_or("SERIAL", |s| match s {
                            SerialConsistency::Serial => "SERIAL",
                            SerialConsistency::LocalSerial => "LOCAL_SERIAL",
                        })
                    ))?,
                    Some(a) => {
                        let level = match a.to_ascii_uppercase().as_str() {
                            "SERIAL" => SerialConsistency::Serial,
                            "LOCAL_SERIAL" => SerialConsistency::LocalSerial,
                            _ => {
                                return Err(Error::Invalid(format!(
                                    "serial consistency must be SERIAL or LOCAL_SERIAL, not `{a}`"
                                )));
                            }
                        };
                        state.serial = Some(level);
                        sink.on_message(format!(
                            "Serial consistency level set to {}.",
                            a.to_ascii_uppercase()
                        ))?;
                    }
                }
                done(sink, started)?;
                Ok(None)
            }
            Kind::Tracing(on) => {
                match on {
                    Some(v) => {
                        state.tracing = v;
                        sink.on_message(
                            if v {
                                "Tracing is enabled."
                            } else {
                                "Tracing is disabled."
                            }
                            .into(),
                        )?;
                    }
                    None => sink.on_message(
                        if state.tracing {
                            "Tracing is enabled."
                        } else {
                            "Tracing is disabled."
                        }
                        .into(),
                    )?,
                }
                done(sink, started)?;
                Ok(None)
            }
            Kind::Describe => {
                self.describe(state, text, req, sink).await?;
                Ok(None)
            }
            Kind::Select | Kind::Other | Kind::Write => {
                // `state.keyspace` starts from `req.database` and follows every `USE`, so a
                // `USE ks2; DROP TABLE t` runs in ks2, as the guard assumes.
                let session = self.pool.session(state.keyspace.as_deref()).await?;
                let batch = req
                    .batch_size
                    .map(|b| b.max(1) as usize)
                    .unwrap_or(DEFAULT_BATCH);
                let mut stmt = Statement::new(text.to_string());
                stmt.set_consistency(state.consistency);
                stmt.set_serial_consistency(state.serial);
                stmt.set_tracing(state.tracing);
                if let Some(ms) = req.timeout_ms.filter(|m| *m > 0) {
                    stmt.set_request_timeout(Some(Duration::from_millis(ms)));
                }
                let pending = self
                    .page_loop(
                        &session,
                        stmt,
                        PagingState::start(),
                        req.max_rows,
                        batch,
                        true,
                        sink,
                        started,
                        keep_open,
                        text,
                    )
                    .await?;
                if !matches!(kind, Kind::Select | Kind::Other) {
                    self.after_write(text);
                }
                Ok(pending)
            }
        }
    }

    /// Fetch pages of `stmt`, streaming rows to `sink`, until the end or `max_rows`.
    /// `send_columns` is false when continuing a pending result. Ends the statement.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn page_loop(
        &self,
        session: &Arc<Session>,
        mut stmt: Statement,
        mut paging: PagingState,
        max_rows: Option<u64>,
        batch: usize,
        mut send_columns: bool,
        sink: &mut dyn ResultSink,
        started: Instant,
        keep_open: bool,
        text: &str,
    ) -> Result<Option<Pending>> {
        let mut total: u64 = 0;
        let consistency = stmt.get_consistency().unwrap_or(Consistency::LocalOne);
        let serial = stmt.get_serial_consistency();
        loop {
            let remaining = max_rows.map(|m| m.saturating_sub(total));
            let page = match remaining {
                Some(r) => (r as usize).min(batch).max(1),
                None => batch,
            };
            stmt.set_page_size(page.min(i32::MAX as usize) as i32);
            let (result, response) = session
                .query_single_page(stmt.clone(), &[], paging.clone())
                .await
                .map_err(|e| error::from_execution(&e, text))?;
            for w in result.warnings() {
                sink.on_message(format!("warning: {w}"))?;
            }
            let tracing_id = result.tracing_id();
            if !result.is_rows() {
                if let Some(id) = tracing_id {
                    self.emit_trace(session, id, sink).await;
                }
                sink.on_statement_end(StatementEnd {
                    outcome: EndOutcome::Done,
                    elapsed_ms: started.elapsed().as_millis() as u64,
                })?;
                return Ok(None);
            }
            let rows = result
                .into_rows_result()
                .map_err(|e| Error::Other(e.to_string()))?;
            let types: Vec<CqlType> = rows
                .column_specs()
                .iter()
                .map(|c| CqlType::from_column_type(c.typ()))
                .collect();
            if send_columns {
                let metas = rows
                    .column_specs()
                    .iter()
                    .zip(&types)
                    .map(|(c, t)| ColumnMeta {
                        name: c.name().to_string(),
                        type_name: t.to_string(),
                        value_kind: t.value_kind(),
                        nullable: None,
                    })
                    .collect();
                sink.on_columns(metas)?;
                send_columns = false;
            }
            let iter = rows
                .rows::<ScyllaRow>()
                .map_err(|e| Error::Other(e.to_string()))?;
            let mut out: Vec<Vec<Value>> = Vec::new();
            for row in iter {
                let row = row.map_err(|e| Error::Other(e.to_string()))?;
                out.push(
                    row.columns
                        .iter()
                        .zip(&types)
                        .map(|(v, t)| v.as_ref().map_or(Value::Null, |v| to_value(v, t)))
                        .collect(),
                );
            }
            total += out.len() as u64;
            if !out.is_empty() {
                sink.on_rows(out)?;
            }
            if let Some(id) = tracing_id {
                self.emit_trace(session, id, sink).await;
            }
            match response.into_paging_control_flow() {
                std::ops::ControlFlow::Break(()) => {
                    sink.on_statement_end(StatementEnd {
                        outcome: EndOutcome::Rows {
                            row_count: total,
                            truncated: false,
                        },
                        elapsed_ms: started.elapsed().as_millis() as u64,
                    })?;
                    return Ok(None);
                }
                std::ops::ControlFlow::Continue(next) => {
                    if max_rows.is_some_and(|m| total >= m) {
                        sink.on_statement_end(StatementEnd {
                            outcome: EndOutcome::Rows {
                                row_count: total,
                                truncated: true,
                            },
                            elapsed_ms: started.elapsed().as_millis() as u64,
                        })?;
                        return Ok(keep_open.then(|| Pending {
                            session: session.clone(),
                            text: text.to_string(),
                            state: next,
                            consistency,
                            serial,
                        }));
                    }
                    paging = next;
                }
            }
        }
    }

    /// Print a tracing session as messages (cqlsh `TRACING ON`).
    async fn emit_trace(&self, session: &Session, id: Uuid, sink: &mut dyn ResultSink) {
        match session.get_tracing_info(&id).await {
            Ok(info) => {
                let _ = sink.on_message(format!(
                    "Tracing session {id}: {}us on {}",
                    info.duration.unwrap_or(0),
                    info.coordinator
                        .map_or_else(|| "?".to_string(), |c| c.to_string())
                ));
                for e in info.events {
                    let _ = sink.on_message(format!(
                        "  {:>8}us  {}  {}",
                        e.source_elapsed.unwrap_or(0),
                        e.source.map_or_else(String::new, |s| s.to_string()),
                        e.activity.unwrap_or_default()
                    ));
                }
            }
            Err(e) => {
                let _ = sink.on_message(format!("Tracing session {id}: not available ({e})"));
            }
        }
    }

    /// Continue a pending result set.
    pub(crate) async fn continue_pending(
        &self,
        pending: Pending,
        max_rows: u64,
        batch: usize,
        sink: &mut dyn ResultSink,
    ) -> Result<(ExecSummary, Option<Pending>)> {
        let started = Instant::now();
        let mut stmt = Statement::new(pending.text.clone());
        stmt.set_consistency(pending.consistency);
        stmt.set_serial_consistency(pending.serial);
        let next = self
            .page_loop(
                &pending.session,
                stmt,
                pending.state.clone(),
                Some(max_rows),
                batch,
                false,
                sink,
                started,
                true,
                &pending.text,
            )
            .await?;
        Ok((
            ExecSummary {
                statements: 1,
                elapsed_ms: started.elapsed().as_millis() as u64,
            },
            next,
        ))
    }

    /// Run one internal query on the base session and return its rows by name.
    pub(crate) async fn rows(&self, cql: &str) -> Result<Vec<crate::rows::Row>> {
        query_rows(&self.pool.base(), cql).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_statements() {
        assert_eq!(classify("SELECT * FROM t"), Kind::Select);
        assert_eq!(classify("  -- c\n select 1"), Kind::Select);
        assert_eq!(classify("USE \"MyKs\";"), Kind::Use("MyKs".into()));
        assert_eq!(classify("use Shop"), Kind::Use("shop".into()));
        assert!(matches!(classify("USE ks DROP TABLE t"), Kind::Invalid(_)));
        assert!(matches!(classify("USE ks /* x */ DROP"), Kind::Invalid(_)));
        assert_eq!(classify("USE \"a b\""), Kind::Use("a b".into()));
        assert_eq!(
            classify("CONSISTENCY LOCAL_QUORUM"),
            Kind::Consistency(Some("LOCAL_QUORUM".into()))
        );
        assert_eq!(classify("CONSISTENCY"), Kind::Consistency(None));
        assert_eq!(
            classify("SERIAL CONSISTENCY LOCAL_SERIAL;"),
            Kind::SerialConsistency(Some("LOCAL_SERIAL".into()))
        );
        assert_eq!(classify("tracing on"), Kind::Tracing(Some(true)));
        assert_eq!(classify("TRACING"), Kind::Tracing(None));
        assert_eq!(classify("DESCRIBE KEYSPACES"), Kind::Describe);
        assert!(matches!(classify("COPY t TO 'x'"), Kind::Unsupported(_)));
        assert!(matches!(classify("SOURCE 'x.cql'"), Kind::Unsupported(_)));
        assert_eq!(classify("INSERT INTO t (a) VALUES (1)"), Kind::Write);
        assert_eq!(classify("DROP TABLE t"), Kind::Write);
        assert_eq!(classify("LIST ROLES"), Kind::Other);
        assert!(!allowed_read_only(&Kind::Write));
        assert!(allowed_read_only(&Kind::Select));
    }

    /// The statements the driver runs are exactly the ones the guard classified.
    fn same_as_guard(text: &str) {
        let driver = statement_ranges(text);
        let chars: Vec<char> = text.chars().collect();
        // Statements the guard sees but the driver does not run may only be comments.
        let guard: Vec<_> = dbstudio_sql::analyze(dbstudio_core::ScriptDialect::Cql, text)
            .statements
            .iter()
            .map(|s| s.range.start as usize..s.range.end as usize)
            .filter(|r| {
                let slice: String = chars[r.clone()].iter().collect();
                dbstudio_sql::lexer::tokenize(dbstudio_core::ScriptDialect::Cql, &slice)
                    .iter()
                    .any(|t| {
                        !t.kind.is_trivia()
                            && !(slice.chars().nth(t.range.start as usize) == Some(';'))
                    })
            })
            .collect();
        assert_eq!(driver, guard, "{text:?}");
    }

    #[test]
    fn splitting_matches_the_guard_on_known_inputs() {
        for text in [
            "DESCRIBE TABLES\nDROP TABLE t;",
            "CONSISTENCY ALL\nTRUNCATE t;",
            "TRACING ON\nDROP TABLE t",
            "USE ks\nDROP TABLE t;",
            "PAGING OFF\nDROP TABLE t;",
            "SERIAL CONSISTENCY SERIAL\nDROP TABLE t;",
            "// x\r/* \nDROP TABLE ks.t; */ SELECT 1",
            "-- x\rDROP TABLE t; SELECT 1",
            "USE ks\r\nDROP TABLE t;",
            "BEGIN BATCH\nINSERT INTO t (a) VALUES (1);\nAPPLY BATCH;\nSELECT 1",
        ] {
            same_as_guard(text);
        }
        let n = statement_ranges("DESCRIBE TABLES\nDROP TABLE t;").len();
        assert_eq!(n, 2);
        assert_eq!(
            statement_ranges("// x\r/* \nDROP TABLE ks.t; */ SELECT 1").len(),
            1
        );
    }

    proptest::proptest! {
        #[test]
        fn splitting_matches_the_guard(
            parts in proptest::collection::vec(
                (proptest::sample::select(vec![
                    "SELECT 1", "DESCRIBE TABLES", "CONSISTENCY ALL", "USE ks", "TRACING ON",
                    "PAGING OFF", "SERIAL CONSISTENCY SERIAL", "DROP TABLE t", "TRUNCATE t",
                    "BEGIN UNLOGGED BATCH", "APPLY BATCH", "INSERT INTO t (a) VALUES (1)",
                    "-- c", "// c", "/* c */", "/* a\nb */", "'x;y'", "$$a;b$$", "\"q;\"", ";",
                    "desc keyspaces", "COPY t TO 'f'",
                ]),
                proptest::sample::select(vec![" ", "\n", "\r", "\r\n", "; ", "\t", ";\n", ""])),
                0..14,
            )
        ) {
            let text: String = parts.iter().map(|(a, b)| format!("{a}{b}")).collect();
            same_as_guard(&text);
        }
    }

    #[test]
    fn words_skip_comments() {
        assert_eq!(
            first_words("/* x */ DROP -- y\n KEYSPACE ks", 2),
            vec!["drop", "keyspace"]
        );
    }

    #[test]
    fn position_offset() {
        let e = add_position(
            Error::Query {
                message: "m".into(),
                position: Some(3),
            },
            10,
        );
        assert!(matches!(
            e,
            Error::Query {
                position: Some(13),
                ..
            }
        ));
    }

    #[test]
    fn paging_cache_picks_largest_offset_not_beyond() {
        let mut c = PagingCache::default();
        c.put("q".into(), 100, PagingState::start());
        c.put("q".into(), 300, PagingState::start());
        c.put("other".into(), 200, PagingState::start());
        assert_eq!(c.best("q", 250).map(|(o, _)| o), Some(100));
        assert_eq!(c.best("q", 300).map(|(o, _)| o), Some(300));
        assert_eq!(c.best("q", 50).map(|(o, _)| o), None);
        for i in 0..100 {
            c.put("big".into(), i, PagingState::start());
        }
        assert!(c.entries.len() <= PAGING_CACHE_SIZE);
    }
}
