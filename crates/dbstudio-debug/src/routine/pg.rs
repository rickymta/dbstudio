//! PostgreSQL routine debugger over `pldbgapi` (package P).
//!
//! Two connections cooperate: the **target** (the lease of the editor tab, which runs the
//! statement) and a hidden **proxy** lease (opened through [`ProxyOpener`]) that drives the
//! `pldbg_*` functions. The target never talks to the debugger itself: a *global breakpoint*
//! restricted to the backend pid of the target makes it stop and connect to the listener of the
//! proxy.
//!
//! # Behaviour verified on a real server (`pg16dbg`, PostgreSQL 16, pldebugger 1.1)
//!
//! These are the points the design marked "(cần kiểm)"; each one is covered by an
//! integration test in `tests/it_pgdbg.rs` or by the experiments noted here.
//!
//! * **First line of a routine.** A global breakpoint at line `-1` and one at `NULL` behave the
//!   same: the target stops at the first executable statement of the routine (the statement
//!   after `BEGIN`, not the `DECLARE`). The code uses `-1` ([`ENTRY_LINE`]).
//! * **Line numbers.** `pldbg_set_breakpoint`, `pldbg_set_global_breakpoint` and
//!   `pldbg_get_stack` count lines of `pg_proc.prosrc` from 1, the leading newline after `$$`
//!   being line 1 - the numbers the editor shows. The `linenumber` of the tuples returned by
//!   `pldbg_wait_for_breakpoint` / `pldbg_step_*` / `pldbg_continue` is **one more** than the
//!   stack line, so this module never uses it: every stop is located through
//!   `pldbg_get_stack`.
//! * **Non-executable lines.** `pldbg_set_*breakpoint` returns `true` for any line, also lines
//!   that hold no statement (blank, `DECLARE`, past the end); such a breakpoint simply never
//!   hits. A breakpoint on the `BEGIN` line stops at the first statement of the block.
//! * **Listener reuse.** A listener survives a cancelled `pldbg_wait_for_target`
//!   (`statement_timeout` / cancel request): the same session handle accepts new global
//!   breakpoints and a later target. It also accepts a second top-level call of the target
//!   backend when the first one has ended.
//! * **Global breakpoints outlive the statement.** They stay armed in shared memory, pointing at
//!   the listener, until they are dropped while attached (`pldbg_drop_breakpoint`, works only
//!   on an attached session) or the proxy connection closes (then the target runs freely).
//!   Hence [`PgRoutineDebugger::disarm`] closes the proxy after every statement, and the entry
//!   breakpoints are dropped as soon as the target attaches (Step Into only wants the first
//!   entry).
//! * **Permissions.** `pldbg_create_listener` works for anyone (`EXECUTE` granted to `PUBLIC`),
//!   but `pldbg_set_global_breakpoint` raises `42501 must be a superuser to create a
//!   breakpoint`. [`PgRoutineDebugger::support`] therefore requires a superuser role
//!   (`NoPermission` otherwise).
//! * **`pldbg_deposit_value`.** The value is evaluated as an expression when it parses
//!   (`1+1` stores `2`, `NULL` stores null, `'x'` stores `x`) and otherwise taken as the text
//!   form of the value (`abc` into a `text`, `2024-02-03` into a `date`, `{4,5}` into an array).
//!   Other variables are not visible to the expression (`a + 100` fails). It returns `false`
//!   (not an error) on an unknown variable or a value that does not convert.
//! * **End of the target.** When the routine returns (or is aborted) the pending `pldbg_*`
//!   call fails with `08006 debugger connection terminated`; the proxy session itself stays
//!   usable. [`RoutineStop::TargetDone`] is built from that error.
//! * **Abort.** `pldbg_abort_target` makes the target statement fail with `57014 canceling
//!   statement due to user request`; the target connection stays usable.
//! * **Proxy dies while the target is stopped.** The target does not hang: it runs on freely
//!   and the statement completes normally (tested by terminating the proxy backend).
//! * **Other backends.** With `targetPid` set, other sessions calling the same routine are not
//!   stopped. `targetPid` is an `i32`, never an `Option`: the helpers that build the SQL cannot
//!   produce a breakpoint for every backend.
//! * **First statement.** A breakpoint on the line of the *first statement* of a routine never
//!   hits; a breakpoint on the `BEGIN` line does, and the stop is reported at the first
//!   statement. [`first_statement_alias`] adds that second breakpoint when arming.
//! * **Duplicate breakpoints.** Setting a global breakpoint that already exists (same routine,
//!   line and pid) fails with `another debugger is already waiting for that breakpoint`;
//!   [`PgRoutineDebugger::arm`] treats it as armed.
//! * **The end of the outermost routine.** `pldbg_continue` / `pldbg_step_*` issued when the
//!   last routine returns do not fail at once while the target backend stays connected: they
//!   wait ~9 s for the next call and then fail with `select() failed waiting for target`. The
//!   engine must therefore race every blocking call against the end of the target statement
//!   (`wait` and `command` both take the `target_done` future); without one (`step_out`, used by
//!   tests) the end is found by polling `pg_stat_activity` from a second hidden connection every
//!   200 ms.
//! * **Roles.** A non-superuser cannot read `shared_preload_libraries` (permission error): the
//!   probe reads `pg_settings`, which hides the value, and the role is refused as `NoPermission`.
//! * **Variables.** `varclass` is `A` (argument) or `L` (local); a null value comes back as the
//!   text `NULL`, a boolean as `t` / `f`. Names are not unique (a loop variable can shadow a local), so a variable is
//!   identified by `(frame, line, name)`.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dbstudio_core::debug::{
    BreakpointStatus, DebugFrame, DebugVariable, FrameKind, RoutineBreakpoint, RoutineDebugSupport,
    RoutineRef, RoutineSource, SourceRef, VariableKind,
};
use dbstudio_core::{
    CollectingSink, Error, Lease, QueryRequest, Result, ScriptDialect, StatementOutcome, Value,
};
use dbstudio_sql::lexer::{Token, TokenKind, tokenize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{ProxyOpener, RoutineCommand, RoutineDebugger, RoutineStop, Step};

/// Line value of a global breakpoint that stops at the first statement of a routine.
pub const ENTRY_LINE: i32 = -1;

/// Most routines Step Into offers for one statement.
pub const MAX_CANDIDATES: usize = 32;

/// Ceiling of the simulated Step Out before it falls back to `continue`.
pub const STEP_OUT_LIMIT: u32 = 10_000;

/// Message of the error `pldbgapi` raises when the target is gone.
const TARGET_GONE: &str = "debugger connection terminated";

/// Message of the error `pldbgapi` raises after ~9 s when the routine returned but the target
/// backend neither exited nor called another routine: a step or `continue` issued at the last
/// line of the outermost routine keeps waiting for the next call. The host must not wait for
/// it: it races the call against the end of the target statement (the
/// `target_done` argument of `wait` / `command`).
const TARGET_TIMEOUT: &str = "select() failed waiting for target";

/// How often the end of the target statement is polled when the host gave no future for it.
const WATCH_INTERVAL: Duration = Duration::from_millis(200);

// ------------------------------------------------------------------------------------------
// Pure helpers (unit tested)
// ------------------------------------------------------------------------------------------

/// SQL string literal that is correct whatever `standard_conforming_strings` says.
pub fn quote_literal(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 3);
    out.push_str("E'");
    for c in text.chars() {
        match c {
            '\'' => out.push_str("''"),
            '\\' => out.push_str("\\\\"),
            '\0' => {}
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// `SELECT pldbg_set_global_breakpoint(...)` for exactly one backend. `target_pid` is not an
/// `Option` on purpose: a breakpoint without a pid would stop every backend of the server.
pub fn global_breakpoint_sql(session: i32, func_oid: u32, line: i32, target_pid: i32) -> String {
    format!("SELECT pldbg_set_global_breakpoint({session}, {func_oid}::oid, {line}, {target_pid})")
}

/// Everything `debug_support` looks at, gathered by a few catalog queries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupportFacts {
    /// `version()` text.
    pub version: String,
    /// `pg_extension.extversion` of `pldbgapi` in this database.
    pub extension_version: Option<String>,
    /// `pldbgapi` is listed in `pg_available_extensions`.
    pub extension_available: bool,
    /// `shared_preload_libraries`; `None` when the role may not read it (non-privileged roles).
    pub preload: Option<String>,
    /// Current role is a superuser.
    pub superuser: bool,
    /// `EXECUTE` on the listener and global breakpoint functions.
    pub can_execute: bool,
}

/// Server flavour, from `version()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PgFlavor {
    /// Plain PostgreSQL (and compatible forks).
    Postgres,
    /// YugabyteDB YSQL: same detection, "experimental" support.
    Yugabyte,
    /// CockroachDB: no debugger API.
    Cockroach,
}

/// Flavour of a server from its `version()` text.
pub fn flavor_from_version(version: &str) -> PgFlavor {
    let v = version.to_ascii_lowercase();
    if v.contains("cockroachdb") {
        PgFlavor::Cockroach
    } else if v.contains("yugabyte") || v.contains("-yb-") {
        PgFlavor::Yugabyte
    } else {
        PgFlavor::Postgres
    }
}

/// Decide the support level (design section 4.2.1). Pure so every branch is unit tested.
pub fn decide_support(facts: &SupportFacts) -> RoutineDebugSupport {
    if flavor_from_version(&facts.version) == PgFlavor::Cockroach {
        return RoutineDebugSupport::Unsupported {
            reason: "CockroachDB has no debugger API".into(),
        };
    }
    if facts.extension_version.is_none() {
        return if facts.extension_available {
            RoutineDebugSupport::NeedsExtension {
                statement: "CREATE EXTENSION pldbgapi;".into(),
            }
        } else {
            RoutineDebugSupport::Unsupported {
                reason: "the pldbgapi extension is not installed on this server; install the \
                         pldebugger package of your PostgreSQL distribution (for example \
                         `postgresql-<version>-pldebugger` from apt.postgresql.org), add \
                         `plugin_debugger` to shared_preload_libraries, restart the server and \
                         run CREATE EXTENSION pldbgapi;"
                    .into(),
            }
        };
    }
    if facts
        .preload
        .as_deref()
        .is_some_and(|p| !preload_has_debugger(p))
    {
        return RoutineDebugSupport::NeedsServerConfig {
            hint: "add plugin_debugger to shared_preload_libraries in postgresql.conf \
                   (shared_preload_libraries = 'plugin_debugger') and restart the server"
                .into(),
        };
    }
    if !facts.can_execute {
        return RoutineDebugSupport::NoPermission {
            hint: "the current role cannot execute the pldbg_* functions; \
                   GRANT EXECUTE on them or connect as a superuser"
                .into(),
        };
    }
    if !facts.superuser {
        return RoutineDebugSupport::NoPermission {
            hint: "pldbg_set_global_breakpoint requires a superuser role; connect as a \
                   superuser to debug routines"
                .into(),
        };
    }
    RoutineDebugSupport::Available
}

fn preload_has_debugger(preload: &str) -> bool {
    preload
        .split(',')
        .map(|s| s.trim().trim_matches('"').trim())
        .any(|lib| lib == "plugin_debugger" || lib.ends_with("/plugin_debugger"))
}

/// Text that tells the user how to fix a server that cannot debug routines.
pub fn guidance(support: &RoutineDebugSupport) -> String {
    match support {
        RoutineDebugSupport::Available => "routine debugging is available".into(),
        RoutineDebugSupport::NeedsExtension { statement } => {
            format!("the pldbgapi extension is not created in this database; run: {statement}")
        }
        RoutineDebugSupport::NeedsServerConfig { hint }
        | RoutineDebugSupport::NoPermission { hint } => hint.clone(),
        RoutineDebugSupport::Unsupported { reason } => reason.clone(),
    }
}

/// Names a statement calls: `name(` or `schema.name(`, and the name after `CALL`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallNames {
    /// Unqualified names (lower-cased unless quoted).
    pub bare: Vec<String>,
    /// `(schema, name)` pairs.
    pub qualified: Vec<(String, String)>,
}

impl CallNames {
    /// True when the statement calls nothing.
    pub fn is_empty(&self) -> bool {
        self.bare.is_empty() && self.qualified.is_empty()
    }
}

/// Collect the routine names a PostgreSQL statement calls (design section 4.2.5).
pub fn call_names(text: &str) -> CallNames {
    let chars: Vec<char> = text.chars().collect();
    let tokens: Vec<Token> = tokenize(ScriptDialect::Postgres, text)
        .into_iter()
        .filter(|t| !t.kind.is_trivia())
        .collect();
    let slice = |t: &Token| -> String {
        chars[t.range.start as usize..t.range.end as usize]
            .iter()
            .collect()
    };
    let ident = |t: &Token| -> Option<String> {
        match t.kind {
            TokenKind::Word => Some(slice(t).to_lowercase()),
            TokenKind::QuotedIdent => {
                let s = slice(t);
                let inner = s.trim_matches('"').replace("\"\"", "\"");
                Some(inner)
            }
            _ => None,
        }
    };
    let is_punct = |t: &Token, p: &str| t.kind == TokenKind::Punct && slice(t) == p;

    let mut out = CallNames::default();
    let mut i = 0;
    while i < tokens.len() {
        let Some(first) = ident(&tokens[i]) else {
            i += 1;
            continue;
        };
        if i > 0 && is_punct(&tokens[i - 1], ".") {
            i += 1;
            continue;
        }
        let mut chain = vec![first];
        let mut j = i + 1;
        while j + 1 < tokens.len() && is_punct(&tokens[j], ".") {
            match ident(&tokens[j + 1]) {
                Some(next) => {
                    chain.push(next);
                    j += 2;
                }
                None => break,
            }
        }
        let after_call = i > 0
            && tokens[i - 1].kind == TokenKind::Word
            && slice(&tokens[i - 1]).eq_ignore_ascii_case("call");
        let called = j < tokens.len() && is_punct(&tokens[j], "(");
        if (called || after_call) && !(chain.len() == 1 && chain[0] == "call") {
            match chain.len() {
                1 => push_unique(&mut out.bare, chain.remove(0)),
                n => {
                    let name = chain.remove(n - 1);
                    let schema = chain.remove(n - 2);
                    let pair = (schema, name);
                    if !out.qualified.contains(&pair) {
                        out.qualified.push(pair);
                    }
                }
            }
        }
        i = j.max(i + 1);
    }
    out
}

fn push_unique(list: &mut Vec<String>, s: String) {
    if !list.contains(&s) {
        list.push(s);
    }
}

/// SQL that expands to the stable signature `schema.name(argtypes)` of a `pg_proc` row `p`
/// joined with its namespace `n`.
const SIGNATURE_SQL: &str = "quote_ident(n.nspname) || '.' || quote_ident(p.proname) || '(' || \
                             oidvectortypes(p.proargtypes) || ')'";

fn sql_text_array(items: &[String]) -> String {
    if items.is_empty() {
        return "ARRAY[]::text[]".into();
    }
    let list: Vec<String> = items.iter().map(|s| quote_literal(s)).collect();
    format!("ARRAY[{}]::text[]", list.join(","))
}

/// Query that finds the PL/pgSQL routines a statement may call.
pub fn candidates_sql(names: &CallNames, force: bool) -> String {
    let bare = sql_text_array(&names.bare);
    let qualified: Vec<String> = names
        .qualified
        .iter()
        .map(|(s, n)| format!("{s}.{n}"))
        .collect();
    // `n.nspname || '.' || p.proname` is compared unquoted on both sides.
    let qualified_arr = sql_text_array(&qualified);
    let visible = if force {
        String::new()
    } else {
        " AND pg_function_is_visible(p.oid)".to_string()
    };
    format!(
        "SELECT p.oid::int8, {SIGNATURE_SQL} FROM pg_proc p \
         JOIN pg_namespace n ON n.oid = p.pronamespace \
         JOIN pg_language l ON l.oid = p.prolang \
         WHERE l.lanname = 'plpgsql' AND p.prokind IN ('f', 'p') AND \
         ((p.proname = ANY({bare}){visible}) OR (n.nspname || '.' || p.proname) = ANY({qualified_arr})) \
         ORDER BY 2 LIMIT {MAX_CANDIDATES}"
    )
}

/// Query that finds the PL/pgSQL trigger functions of the tables a statement touches.
pub fn trigger_candidates_sql(tables: &[String]) -> String {
    let rels: Vec<String> = tables
        .iter()
        .map(|t| format!("to_regclass({})::oid", quote_literal(t)))
        .collect();
    format!(
        "SELECT DISTINCT p.oid::int8, {SIGNATURE_SQL} FROM pg_trigger t \
         JOIN pg_proc p ON p.oid = t.tgfoid \
         JOIN pg_namespace n ON n.oid = p.pronamespace \
         JOIN pg_language l ON l.oid = p.prolang \
         WHERE NOT t.tgisinternal AND l.lanname = 'plpgsql' AND t.tgrelid = ANY(ARRAY[{}]::oid[]) \
         ORDER BY 2 LIMIT {MAX_CANDIDATES}",
        rels.join(",")
    )
}

/// sha256 of a routine source as lowercase hex.
pub fn source_hash(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

async fn wait_done(done: &mut Option<&mut (dyn Future<Output = ()> + Send + Unpin)>) {
    match done {
        Some(f) => {
            let f: &mut (dyn Future<Output = ()> + Send + Unpin) = &mut **f;
            f.await
        }
        None => std::future::pending().await,
    }
}

/// True when the target backend is no longer running a statement.
async fn target_statement_ended(watcher: &dyn Lease, pid: i32) -> bool {
    let sql = format!("SELECT state FROM pg_stat_activity WHERE pid = {pid}");
    let mut sink = CollectingSink::new();
    if watcher
        .execute(QueryRequest::new(sql), &mut sink)
        .await
        .is_err()
    {
        return false;
    }
    for r in sink.into_results() {
        if let StatementOutcome::Rows(rs) = r.outcome {
            return match rs.rows.first().and_then(|r| r.first()) {
                Some(Value::Text(state)) => {
                    !(state == "active" || state == "fastpath function call")
                }
                Some(_) => false,
                None => true,
            };
        }
    }
    false
}

/// Line a breakpoint must also be set on so that it stops at the first statement of a routine.
///
/// `pldebugger` never stops at the first statement through a breakpoint on that statement's own
/// line: only a breakpoint on the `BEGIN` line (the outer block) does, and the stop is then
/// reported at the first statement. Returns the `BEGIN` line when `line` is the line of the
/// first statement after it.
pub fn first_statement_alias(prosrc: &str, line: u32) -> Option<u32> {
    let chars: Vec<char> = prosrc.chars().collect();
    let tokens: Vec<Token> = tokenize(ScriptDialect::Postgres, prosrc)
        .into_iter()
        .filter(|t| !t.kind.is_trivia())
        .collect();
    let line_of = |t: &Token| {
        1 + chars[..t.range.start as usize]
            .iter()
            .filter(|c| **c == '\n')
            .count() as u32
    };
    let begin = tokens.iter().position(|t| {
        t.kind == TokenKind::Word
            && chars[t.range.start as usize..t.range.end as usize]
                .iter()
                .collect::<String>()
                .eq_ignore_ascii_case("begin")
    })?;
    let first = tokens.get(begin + 1)?;
    let begin_line = line_of(&tokens[begin]);
    (line_of(first) == line && begin_line != line).then_some(begin_line)
}

fn collect_candidates(rows: Vec<Vec<Value>>, found: &mut Vec<RoutineRef>) {
    for row in rows {
        let (Some(oid), Some(sig)) = (
            row.first().and_then(value_i64),
            row.get(1).and_then(value_text),
        ) else {
            continue;
        };
        if found.len() < MAX_CANDIDATES && !found.iter().any(|r| r.signature == sig) {
            found.push(RoutineRef {
                signature: sig,
                oid: Some(oid.to_string()),
                path: None,
            });
        }
    }
}

fn is_target_gone(e: &Error) -> bool {
    matches!(e, Error::Query { message, .. }
        if message.contains(TARGET_GONE) || message.contains(TARGET_TIMEOUT))
}

fn value_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Int(n) => Some(*n),
        Value::UInt(n) => i64::try_from(*n).ok(),
        Value::Decimal(s) | Value::Text(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn value_text(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::Text(s) | Value::Decimal(s) => Some(s.clone()),
        Value::Int(n) => Some(n.to_string()),
        Value::UInt(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        other => Some(format!("{other:?}")),
    }
}

fn value_bool(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Text(s) => s == "t" || s == "true",
        Value::Int(n) => *n != 0,
        _ => false,
    }
}

/// Text shown in the Variables tree for a value of type `type_name`.
pub fn display_value(type_name: &str, value: &str) -> String {
    const RAW: [&str; 8] = [
        "smallint",
        "integer",
        "bigint",
        "numeric",
        "real",
        "double precision",
        "boolean",
        "oid",
    ];
    let shown = if value == "NULL" || RAW.contains(&type_name) || type_name.starts_with("numeric(")
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "''"))
    };
    if shown.chars().count() > 200 {
        let cut: String = shown.chars().take(197).collect();
        format!("{cut}...")
    } else {
        shown
    }
}

// ------------------------------------------------------------------------------------------
// The debugger
// ------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct StackRow {
    level: u32,
    target_name: String,
    func: u32,
    line: u32,
    args: String,
}

/// Routine debugger of PostgreSQL servers with `pldbgapi` (one instance per debug session).
pub struct PgRoutineDebugger {
    opener: Arc<dyn ProxyOpener>,
    database: Option<String>,
    support: Option<RoutineDebugSupport>,
    target_pid: Option<i32>,
    proxy: Option<Arc<dyn Lease>>,
    /// Second hidden connection that polls `pg_stat_activity` for the end of the target
    /// statement while the proxy is blocked inside a `pldbg_*` call.
    watcher: Option<Arc<dyn Lease>>,
    session: Option<i32>,
    /// Source text of routines seen while arming (first statement detection).
    sources: HashMap<u32, String>,
    /// Routines with an armed entry breakpoint (dropped when the target attaches).
    entry_oids: Vec<u32>,
    armed: bool,
    attached: bool,
    finished: bool,
    selected_frame: u32,
    stop_seq: u64,
    signatures: HashMap<u32, RoutineRef>,
    var_baseline: HashMap<String, String>,
    var_current: HashMap<String, String>,
    var_seq: u64,
    depth: u32,
}

impl std::fmt::Debug for PgRoutineDebugger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgRoutineDebugger")
            .field("target_pid", &self.target_pid)
            .field("session", &self.session)
            .field("armed", &self.armed)
            .field("attached", &self.attached)
            .finish_non_exhaustive()
    }
}

impl PgRoutineDebugger {
    /// New debugger; the proxy connection is opened through `opener` (on `database` when given)
    /// the first time a statement is armed, and closed by [`RoutineDebugger::disarm`].
    pub fn new(opener: Arc<dyn ProxyOpener>, database: Option<String>) -> Self {
        PgRoutineDebugger {
            opener,
            database,
            support: None,
            target_pid: None,
            proxy: None,
            watcher: None,
            session: None,
            sources: HashMap::new(),
            entry_oids: Vec::new(),
            armed: false,
            attached: false,
            finished: false,
            selected_frame: 0,
            stop_seq: 0,
            signatures: HashMap::new(),
            var_baseline: HashMap::new(),
            var_current: HashMap::new(),
            var_seq: 0,
            depth: 0,
        }
    }

    /// Backend pid of the target connection (learned by `support` / `step_into_targets`).
    pub fn target_pid(&self) -> Option<i32> {
        self.target_pid
    }

    /// Set the target backend pid explicitly (when the host already knows it).
    pub fn set_target_pid(&mut self, pid: i32) {
        self.target_pid = Some(pid);
    }

    /// True while the target is stopped inside a routine.
    pub fn is_stopped(&self) -> bool {
        self.attached && !self.finished
    }

    /// Stack depth of the last stop (1 = outermost routine).
    pub fn depth(&self) -> u32 {
        self.depth
    }

    // ---- low level -----------------------------------------------------------------------

    async fn rows(lease: &dyn Lease, sql: &str) -> Result<Vec<Vec<Value>>> {
        let mut sink = CollectingSink::new();
        lease.execute(QueryRequest::new(sql), &mut sink).await?;
        let mut out = Vec::new();
        for r in sink.into_results() {
            if let StatementOutcome::Rows(rs) = r.outcome {
                out.extend(rs.rows);
            }
        }
        Ok(out)
    }

    async fn proxy(&mut self) -> Result<Arc<dyn Lease>> {
        if let Some(p) = &self.proxy {
            return Ok(p.clone());
        }
        let p = self.opener.open_proxy(self.database.clone()).await?;
        self.proxy = Some(p.clone());
        Ok(p)
    }

    fn session(&self) -> Result<i32> {
        self.session.ok_or_else(|| {
            Error::Invalid("no routine debug listener: arm a statement first".into())
        })
    }

    fn stopped(&self) -> Result<i32> {
        if !self.attached || self.finished {
            return Err(Error::Invalid(
                "the target is not stopped inside a routine".into(),
            ));
        }
        self.session()
    }

    async fn probe_target(&mut self, target: &dyn Lease) -> Result<i32> {
        let rows = Self::rows(target, "SELECT pg_backend_pid()").await?;
        let pid = rows
            .first()
            .and_then(|r| r.first())
            .and_then(value_i64)
            .and_then(|n| i32::try_from(n).ok())
            .ok_or_else(|| Error::Other("cannot read pg_backend_pid() of the target".into()))?;
        self.target_pid = Some(pid);
        Ok(pid)
    }

    async fn gather_facts(lease: &dyn Lease) -> Result<SupportFacts> {
        let rows = Self::rows(
            lease,
            "SELECT version(), \
             (SELECT extversion FROM pg_extension WHERE extname = 'pldbgapi'), \
             EXISTS (SELECT 1 FROM pg_available_extensions WHERE name = 'pldbgapi'), \
             (SELECT setting FROM pg_settings WHERE name = 'shared_preload_libraries'), \
             COALESCE((SELECT rolsuper FROM pg_roles WHERE rolname = current_user), false)",
        )
        .await?;
        let row = rows
            .first()
            .ok_or_else(|| Error::Other("empty answer of the support probe".into()))?;
        let extension_version = row.get(1).and_then(value_text);
        // The function privilege probe needs the extension; without it the answer is moot.
        let can_execute = if extension_version.is_some() {
            let r = Self::rows(
                lease,
                "SELECT has_function_privilege('pldbg_create_listener()', 'EXECUTE') AND \
                 has_function_privilege('pldbg_set_global_breakpoint(integer, oid, integer, integer)', 'EXECUTE')",
            )
            .await?;
            r.first().and_then(|r| r.first()).is_some_and(value_bool)
        } else {
            false
        };
        Ok(SupportFacts {
            version: row.first().and_then(value_text).unwrap_or_default(),
            extension_version,
            extension_available: row.get(2).is_some_and(value_bool),
            preload: row.get(3).and_then(value_text),
            superuser: row.get(4).is_some_and(value_bool),
            can_execute,
        })
    }

    /// Support level of the target, cached for the session; also learns the target pid.
    async fn ensure_support(&mut self, target: &dyn Lease) -> Result<RoutineDebugSupport> {
        if self.target_pid.is_none() {
            self.probe_target(target).await?;
        }
        if let Some(s) = &self.support {
            return Ok(s.clone());
        }
        let facts = Self::gather_facts(target).await?;
        let s = decide_support(&facts);
        self.support = Some(s.clone());
        Ok(s)
    }

    /// Resolve `oid` -> routine identity for every oid not cached yet.
    async fn resolve_oids(&mut self, proxy: &dyn Lease, oids: &[u32]) -> Result<()> {
        let missing: Vec<String> = oids
            .iter()
            .filter(|o| !self.signatures.contains_key(o))
            .map(u32::to_string)
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        let sql = format!(
            "SELECT p.oid::int8, {SIGNATURE_SQL} FROM pg_proc p \
             JOIN pg_namespace n ON n.oid = p.pronamespace WHERE p.oid IN ({})",
            missing.join(",")
        );
        for row in Self::rows(proxy, &sql).await? {
            if let (Some(oid), Some(sig)) = (
                row.first().and_then(value_i64),
                row.get(1).and_then(value_text),
            ) {
                let oid = oid as u32;
                self.signatures.insert(
                    oid,
                    RoutineRef {
                        signature: sig,
                        oid: Some(oid.to_string()),
                        path: None,
                    },
                );
            }
        }
        Ok(())
    }

    async fn routine_oid(&mut self, proxy: &dyn Lease, signature: &str) -> Result<Option<u32>> {
        let sql = format!(
            "SELECT to_regprocedure({})::oid::int8",
            quote_literal(signature)
        );
        let rows = Self::rows(proxy, &sql).await?;
        let oid = rows
            .first()
            .and_then(|r| r.first())
            .and_then(value_i64)
            .map(|n| n as u32);
        if let Some(oid) = oid {
            self.signatures.entry(oid).or_insert_with(|| RoutineRef {
                signature: signature.to_string(),
                oid: Some(oid.to_string()),
                path: None,
            });
        }
        Ok(oid)
    }

    async fn stack_rows(&mut self) -> Result<Vec<StackRow>> {
        let s = self.session()?;
        let proxy = self.proxy().await?;
        let rows = Self::rows(
            &*proxy,
            &format!(
                "SELECT level, targetname, func::int8, linenumber, args FROM pldbg_get_stack({s}) ORDER BY level"
            ),
        )
        .await?;
        let mut out = Vec::new();
        for r in rows {
            let (Some(level), Some(func), Some(line)) = (
                r.first().and_then(value_i64),
                r.get(2).and_then(value_i64),
                r.get(3).and_then(value_i64),
            ) else {
                continue;
            };
            out.push(StackRow {
                level: level as u32,
                target_name: r.get(1).and_then(value_text).unwrap_or_default(),
                func: func as u32,
                line: line.max(0) as u32,
                args: r.get(4).and_then(value_text).unwrap_or_default(),
            });
        }
        let oids: Vec<u32> = out.iter().map(|r| r.func).collect();
        self.resolve_oids(&*proxy, &oids).await?;
        Ok(out)
    }

    fn routine_of(&self, oid: u32, fallback: &str) -> RoutineRef {
        self.signatures.get(&oid).cloned().unwrap_or(RoutineRef {
            signature: fallback.to_string(),
            oid: Some(oid.to_string()),
            path: None,
        })
    }

    /// Locate the stop through the stack (the tuple lines of `pldbg_step_*` are off by one).
    async fn position(&mut self) -> Result<RoutineStop> {
        let stack = self.stack_rows().await?;
        let Some(top) = stack.first() else {
            self.finished = true;
            return Ok(RoutineStop::TargetDone);
        };
        self.stop_seq += 1;
        self.selected_frame = 0;
        self.depth = stack.len() as u32;
        Ok(RoutineStop::At {
            routine: self.routine_of(top.func, &top.target_name),
            line: top.line,
            depth: self.depth,
        })
    }

    async fn handle_command_error(&mut self, e: Error) -> Result<RoutineStop> {
        if is_target_gone(&e) {
            self.finished = true;
            Ok(RoutineStop::TargetDone)
        } else {
            Err(e)
        }
    }

    async fn command_inner(
        &mut self,
        cmd: RoutineCommand,
        done: Option<&mut (dyn Future<Output = ()> + Send + Unpin)>,
    ) -> Result<RoutineStop> {
        let s = self.stopped()?;
        let func = match cmd {
            RoutineCommand::Abort => {
                let proxy = self.proxy().await?;
                let _ = Self::rows(&*proxy, &format!("SELECT pldbg_abort_target({s})")).await;
                self.finished = true;
                return Ok(RoutineStop::Aborted);
            }
            RoutineCommand::StepInto => "pldbg_step_into",
            RoutineCommand::StepOver => "pldbg_step_over",
            RoutineCommand::Continue => "pldbg_continue",
        };
        match self
            .proxy_call(format!("SELECT linenumber FROM {func}({s})"), done)
            .await
        {
            Ok(true) => self.position().await,
            Ok(false) => {
                self.finished = true;
                Ok(RoutineStop::TargetDone)
            }
            Err(e) => self.handle_command_error(e).await,
        }
    }

    // ---- extras beyond the trait ----------------------------------------------------------

    /// Step Out, simulated (design section 2.4): `step_over` until the stack is shallower than
    /// when the call started; after [`STEP_OUT_LIMIT`] steps fall back to `continue`.
    pub async fn step_out(&mut self) -> Result<RoutineStop> {
        let start = self.depth;
        for _ in 0..STEP_OUT_LIMIT {
            match self.command_inner(RoutineCommand::StepOver, None).await? {
                RoutineStop::At { depth, .. } if depth >= start => continue,
                other => return Ok(other),
            }
        }
        self.command_inner(RoutineCommand::Continue, None).await
    }

    /// Set a breakpoint on the attached target (Run to Cursor, edits while paused).
    pub async fn add_breakpoint(&mut self, routine: &RoutineRef, line: u32) -> Result<bool> {
        let s = self.stopped()?;
        let proxy = self.proxy().await?;
        let Some(oid) = self.routine_oid(&*proxy, &routine.signature).await? else {
            return Ok(false);
        };
        let rows = Self::rows(
            &*proxy,
            &format!("SELECT pldbg_set_breakpoint({s}, {oid}::oid, {line})"),
        )
        .await?;
        Ok(rows.first().and_then(|r| r.first()).is_some_and(value_bool))
    }

    /// Remove a breakpoint of the attached target.
    pub async fn remove_breakpoint(&mut self, routine: &RoutineRef, line: u32) -> Result<bool> {
        let s = self.stopped()?;
        let proxy = self.proxy().await?;
        let Some(oid) = self.routine_oid(&*proxy, &routine.signature).await? else {
            return Ok(false);
        };
        let rows = Self::rows(
            &*proxy,
            &format!("SELECT pldbg_drop_breakpoint({s}, {oid}::oid, {line})"),
        )
        .await?;
        Ok(rows.first().and_then(|r| r.first()).is_some_and(value_bool))
    }

    /// Routines called by `text` (and, with `force`, trigger functions of `tables`).
    pub async fn candidates_for_text(
        &mut self,
        target: &dyn Lease,
        text: &str,
        tables: &[String],
        force: bool,
    ) -> Result<Vec<RoutineRef>> {
        self.ensure_support(target).await?;
        let names = call_names(text);
        let mut found: Vec<RoutineRef> = Vec::new();
        if !names.is_empty() {
            let rows = Self::rows(target, &candidates_sql(&names, force)).await?;
            collect_candidates(rows, &mut found);
        }
        if force && !tables.is_empty() {
            let rows = Self::rows(target, &trigger_candidates_sql(tables)).await?;
            collect_candidates(rows, &mut found);
        }
        Ok(found)
    }

    /// Translate "extension missing" failures of the first `pldbg_*` call into guidance.
    fn explain_setup_error(&self, e: Error) -> Error {
        let missing = matches!(&e, Error::Query { message, .. }
            if message.contains("pldbg_") && message.contains("does not exist"));
        if missing {
            let hint = match &self.support {
                Some(s) if *s != RoutineDebugSupport::Available => guidance(s),
                _ => "the pldbgapi extension is not created in this database; run: CREATE \
                      EXTENSION pldbgapi; (and add plugin_debugger to shared_preload_libraries)"
                    .into(),
            };
            return Error::Unsupported(hint);
        }
        e
    }

    async fn set_global(
        &mut self,
        proxy: &dyn Lease,
        session: i32,
        pid: i32,
        oid: u32,
        line: i32,
    ) -> Result<bool> {
        match Self::rows(proxy, &global_breakpoint_sql(session, oid, line, pid)).await {
            Ok(rows) => Ok(rows.first().and_then(|r| r.first()).is_some_and(value_bool)),
            // Two breakpoints on one line (or an alias that coincides with one): already armed.
            Err(Error::Query { message, .. }) if message.contains("already waiting") => Ok(true),
            Err(e) => Err(e),
        }
    }

    async fn close_proxy(&mut self) {
        if let Some(w) = self.watcher.take() {
            let _ = w.close().await;
        }
        if let Some(p) = self.proxy.take() {
            let _ = p.close().await;
        }
        self.session = None;
    }

    async fn watcher(&mut self) -> Result<Arc<dyn Lease>> {
        if let Some(w) = &self.watcher {
            return Ok(w.clone());
        }
        let w = self.opener.open_proxy(self.database.clone()).await?;
        self.watcher = Some(w.clone());
        Ok(w)
    }

    /// Run one blocking `pldbg_*` call on the proxy and race it against the end of the target
    /// statement: `done` when the host has the future, else a poll of `pg_stat_activity`.
    /// `Ok(false)` = the target statement finished first (the call was cancelled).
    async fn proxy_call(
        &mut self,
        sql: String,
        mut done: Option<&mut (dyn Future<Output = ()> + Send + Unpin)>,
    ) -> Result<bool> {
        let proxy = self.proxy().await?;
        let watcher = if done.is_none() {
            Some(self.watcher().await?)
        } else {
            None
        };
        let pid = self.target_pid;
        let request = QueryRequest::new(sql);
        let query_id: Uuid = request.query_id;
        let mut sink = CollectingSink::new();
        let exec = proxy.execute(request, &mut sink);
        tokio::pin!(exec);
        let mut tick = tokio::time::interval(WATCH_INTERVAL);
        tick.tick().await;
        let finished_first = loop {
            tokio::select! {
                r = &mut exec => return r.map(|_| true),
                () = wait_done(&mut done), if done.is_some() => break true,
                _ = tick.tick(), if watcher.is_some() => {
                    let (Some(w), Some(pid)) = (&watcher, pid) else { continue };
                    if target_statement_ended(&**w, pid).await {
                        break true;
                    }
                }
            }
        };
        debug_assert!(finished_first);
        let _ = proxy.cancel(query_id).await;
        // The call may have completed in the meantime: then the target did stop.
        match exec.await {
            Ok(_) => Ok(true),
            Err(_) => Ok(false),
        }
    }

    async fn attach_after_wait(&mut self) -> Result<RoutineStop> {
        let s = self.session()?;
        let proxy = self.proxy().await?;
        self.attached = true;
        self.finished = false;
        Self::rows(
            &*proxy,
            &format!("SELECT func::int8, linenumber FROM pldbg_wait_for_breakpoint({s})"),
        )
        .await?;
        // The entry breakpoints only serve the first stop of Step Into.
        let entries = std::mem::take(&mut self.entry_oids);
        for oid in entries {
            let _ = Self::rows(
                &*proxy,
                &format!("SELECT pldbg_drop_breakpoint({s}, {oid}::oid, {ENTRY_LINE})"),
            )
            .await;
        }
        self.position().await
    }
}

#[async_trait]
impl RoutineDebugger for PgRoutineDebugger {
    async fn support(&mut self, target: &dyn Lease) -> Result<RoutineDebugSupport> {
        self.support = None;
        self.probe_target(target).await?;
        self.ensure_support(target).await
    }

    async fn step_into_targets(
        &mut self,
        target: &dyn Lease,
        step: &Step,
        force: bool,
    ) -> Result<Vec<RoutineRef>> {
        // The pid may change after a reconnect of the tab.
        self.probe_target(target).await?;
        if !matches!(
            self.ensure_support(target).await?,
            RoutineDebugSupport::Available
        ) {
            return Ok(Vec::new());
        }
        self.candidates_for_text(target, &step.text, &step.info.targets, force)
            .await
    }

    async fn arm(
        &mut self,
        targets: &[RoutineRef],
        bps: &[RoutineBreakpoint],
    ) -> Result<Vec<BreakpointStatus>> {
        let pid = self.target_pid.ok_or_else(|| {
            Error::Invalid("the target backend pid is unknown: call support() first".into())
        })?;
        if let Some(s) = &self.support
            && *s != RoutineDebugSupport::Available
        {
            return Err(Error::Unsupported(guidance(s)));
        }
        // A previous statement may have left global breakpoints behind: start clean.
        self.disarm().await?;
        let mut statuses: Vec<BreakpointStatus> = Vec::new();
        if targets.is_empty() && bps.is_empty() {
            return Ok(statuses);
        }
        let proxy = self.proxy().await?;
        let session = match Self::rows(&*proxy, "SELECT pldbg_create_listener()").await {
            Ok(rows) => rows
                .first()
                .and_then(|r| r.first())
                .and_then(value_i64)
                .and_then(|n| i32::try_from(n).ok())
                .ok_or_else(|| Error::Other("pldbg_create_listener returned no handle".into()))?,
            Err(e) => {
                let e = self.explain_setup_error(e);
                self.close_proxy().await;
                return Err(e);
            }
        };
        self.session = Some(session);
        let result = self
            .arm_all(&*proxy, session, pid, targets, bps, &mut statuses)
            .await;
        if let Err(e) = result {
            self.close_proxy().await;
            return Err(e);
        }
        self.armed = true;
        Ok(statuses)
    }

    async fn wait(
        &mut self,
        target_done: &mut (dyn Future<Output = ()> + Send + Unpin),
    ) -> Result<RoutineStop> {
        if !self.armed || self.finished {
            target_done.await;
            return Ok(RoutineStop::TargetDone);
        }
        let s = self.session()?;
        let sql = format!("SELECT pldbg_wait_for_target({s})");
        if self.proxy_call(sql, Some(target_done)).await? {
            self.attach_after_wait().await
        } else {
            self.finished = true;
            Ok(RoutineStop::TargetDone)
        }
    }

    async fn command(
        &mut self,
        cmd: RoutineCommand,
        target_done: &mut (dyn Future<Output = ()> + Send + Unpin),
    ) -> Result<RoutineStop> {
        self.command_inner(cmd, Some(target_done)).await
    }

    async fn frames(&mut self) -> Result<Vec<DebugFrame>> {
        self.stopped()?;
        let stack = self.stack_rows().await?;
        Ok(stack
            .iter()
            .map(|r| {
                let routine = self.routine_of(r.func, &r.target_name);
                DebugFrame {
                    id: r.level,
                    kind: FrameKind::Routine,
                    name: format!("{}:{}", r.target_name, r.line),
                    source: SourceRef::Routine { routine },
                    line: r.line,
                    language: Some("plpgsql".into()),
                    args: if r.args.is_empty() {
                        None
                    } else {
                        Some(r.args.clone())
                    },
                }
            })
            .collect())
    }

    async fn select_frame(&mut self, frame: u32) -> Result<()> {
        let s = self.stopped()?;
        let proxy = self.proxy().await?;
        Self::rows(
            &*proxy,
            &format!("SELECT linenumber FROM pldbg_select_frame({s}, {frame})"),
        )
        .await?;
        self.selected_frame = frame;
        Ok(())
    }

    async fn variables(&mut self) -> Result<Vec<DebugVariable>> {
        let s = self.stopped()?;
        let proxy = self.proxy().await?;
        let rows = Self::rows(
            &*proxy,
            &format!(
                "SELECT name, varclass, linenumber, isconst, format_type(dtype, NULL), value \
                 FROM pldbg_get_variables({s})"
            ),
        )
        .await?;
        if self.var_seq != self.stop_seq {
            self.var_baseline = std::mem::take(&mut self.var_current);
            self.var_seq = self.stop_seq;
        }
        let mut out = Vec::new();
        for r in rows {
            let name = r.first().and_then(value_text).unwrap_or_default();
            let class = r.get(1).and_then(value_text).unwrap_or_default();
            let line = r.get(2).and_then(value_i64).unwrap_or(0);
            let is_const = r.get(3).is_some_and(value_bool);
            let type_name = r.get(4).and_then(value_text).unwrap_or_default();
            let mut raw = r
                .get(5)
                .and_then(value_text)
                .unwrap_or_else(|| "NULL".into());
            if type_name == "boolean" {
                // The server answers `t` / `f`.
                raw = match raw.as_str() {
                    "t" => "true".into(),
                    "f" => "false".into(),
                    _ => raw,
                };
            }
            let reference = format!("{}:{}:{}", self.selected_frame, line, name);
            let changed = self
                .var_baseline
                .get(&reference)
                .is_some_and(|old| *old != raw);
            self.var_current.insert(reference.clone(), raw.clone());
            let value = match raw.as_str() {
                "NULL" => Value::Null,
                "true" if type_name == "boolean" => Value::Bool(true),
                "false" if type_name == "boolean" => Value::Bool(false),
                _ => Value::Text(raw.clone()),
            };
            out.push(DebugVariable {
                reference,
                name,
                kind: if class == "A" {
                    VariableKind::RoutineArgument
                } else {
                    VariableKind::RoutineLocal
                },
                value: Some(value),
                display: display_value(&type_name, &raw),
                type_name: Some(type_name),
                editable: !is_const,
                has_children: false,
                changed,
            });
        }
        Ok(out)
    }

    async fn set_variable(&mut self, reference: &str, value_text: &str) -> Result<DebugVariable> {
        let s = self.stopped()?;
        let mut parts = reference.splitn(3, ':');
        let (Some(frame), Some(line), Some(name)) = (parts.next(), parts.next(), parts.next())
        else {
            return Err(Error::Invalid(format!(
                "bad variable reference `{reference}`"
            )));
        };
        let frame: u32 = frame
            .parse()
            .map_err(|_| Error::Invalid(format!("bad variable reference `{reference}`")))?;
        let line: i64 = line
            .parse()
            .map_err(|_| Error::Invalid(format!("bad variable reference `{reference}`")))?;
        if frame != self.selected_frame {
            self.select_frame(frame).await?;
        }
        // A value set by hand counts as changed at this stop (shown highlighted).
        if let Some(old) = self.var_current.get(reference).cloned() {
            self.var_baseline.insert(reference.to_string(), old);
        }
        let proxy = self.proxy().await?;
        let rows = Self::rows(
            &*proxy,
            &format!(
                "SELECT pldbg_deposit_value({s}, {}, {line}, {})",
                quote_literal(name),
                quote_literal(value_text)
            ),
        )
        .await?;
        if !rows.first().and_then(|r| r.first()).is_some_and(value_bool) {
            return Err(Error::Invalid(format!(
                "the server rejected the value for `{name}` (unknown variable or value does not \
                 convert to its type)"
            )));
        }
        self.variables()
            .await?
            .into_iter()
            .find(|v| v.reference == reference)
            .ok_or_else(|| Error::NotFound(format!("variable `{name}`")))
    }

    async fn source(&mut self, routine: &RoutineRef) -> Result<RoutineSource> {
        let proxy = self.proxy().await?;
        let oid = match routine.oid.as_deref().and_then(|o| o.parse::<u32>().ok()) {
            Some(oid) => oid,
            None => self
                .routine_oid(&*proxy, &routine.signature)
                .await?
                .ok_or_else(|| Error::NotFound(format!("routine {}", routine.signature)))?,
        };
        let rows = Self::rows(
            &*proxy,
            &format!(
                "SELECT p.prosrc, l.lanname FROM pg_proc p \
                 JOIN pg_language l ON l.oid = p.prolang WHERE p.oid = {oid}::oid"
            ),
        )
        .await?;
        let row = rows
            .first()
            .ok_or_else(|| Error::NotFound(format!("routine {}", routine.signature)))?;
        let text = row.first().and_then(value_text).unwrap_or_default();
        Ok(RoutineSource {
            source_hash: source_hash(&text),
            language: row.get(1).and_then(value_text).unwrap_or_default(),
            first_line: 1,
            text,
        })
    }

    async fn disarm(&mut self) -> Result<()> {
        // Closing the proxy is the only way to clear global breakpoints of a target that never
        // attached; on a stopped target it makes the target run on freely.
        self.close_proxy().await;
        self.sources.clear();
        self.entry_oids.clear();
        self.armed = false;
        self.attached = false;
        self.finished = false;
        self.selected_frame = 0;
        self.depth = 0;
        Ok(())
    }
}

impl PgRoutineDebugger {
    async fn arm_all(
        &mut self,
        proxy: &dyn Lease,
        session: i32,
        pid: i32,
        targets: &[RoutineRef],
        bps: &[RoutineBreakpoint],
        statuses: &mut Vec<BreakpointStatus>,
    ) -> Result<()> {
        for t in targets {
            let Some(oid) = self.routine_oid(proxy, &t.signature).await? else {
                continue;
            };
            if self
                .set_global(proxy, session, pid, oid, ENTRY_LINE)
                .await?
            {
                self.entry_oids.push(oid);
            }
        }
        for rb in bps {
            let b = &rb.breakpoint;
            let mut status = BreakpointStatus {
                id: b.id.clone(),
                verified: false,
                line: b.line,
                message: None,
            };
            if !b.enabled {
                status.message = Some("breakpoint is disabled".into());
                statuses.push(status);
                continue;
            }
            let Some(oid) = self.routine_oid(proxy, &rb.routine.signature).await? else {
                status.message = Some(format!("routine {} not found", rb.routine.signature));
                statuses.push(status);
                continue;
            };
            let prosrc = match self.sources.get(&oid).cloned() {
                Some(text) => text,
                None => {
                    let rows = Self::rows(
                        proxy,
                        &format!("SELECT prosrc FROM pg_proc WHERE oid = {oid}::oid"),
                    )
                    .await?;
                    let text = rows
                        .first()
                        .and_then(|r| r.first())
                        .and_then(value_text)
                        .unwrap_or_default();
                    self.sources.insert(oid, text.clone());
                    text
                }
            };
            if let Some(expected) = &rb.source_hash
                && source_hash(&prosrc) != *expected
            {
                status.message = Some("the routine source has changed".into());
                statuses.push(status);
                continue;
            }
            let line = i32::try_from(b.line).unwrap_or(i32::MAX);
            status.verified = self.set_global(proxy, session, pid, oid, line).await?;
            if status.verified
                && let Some(alias) = first_statement_alias(&prosrc, b.line)
            {
                self.set_global(proxy, session, pid, oid, alias as i32)
                    .await?;
            }
            if !status.verified {
                status.message = Some("the server did not accept the breakpoint".into());
            }
            statuses.push(status);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> SupportFacts {
        SupportFacts {
            version: "PostgreSQL 16.4 on x86_64".into(),
            extension_version: Some("1.1".into()),
            extension_available: true,
            preload: Some("plugin_debugger".into()),
            superuser: true,
            can_execute: true,
        }
    }

    #[test]
    fn support_decision_tree() {
        assert_eq!(decide_support(&facts()), RoutineDebugSupport::Available);

        let mut f = facts();
        f.extension_version = None;
        assert_eq!(
            decide_support(&f),
            RoutineDebugSupport::NeedsExtension {
                statement: "CREATE EXTENSION pldbgapi;".into()
            }
        );
        f.extension_available = false;
        assert!(matches!(
            decide_support(&f),
            RoutineDebugSupport::Unsupported { reason } if reason.contains("pldebugger")
        ));

        let mut f = facts();
        f.preload = Some("pg_stat_statements, auto_explain".into());
        assert!(matches!(
            decide_support(&f),
            RoutineDebugSupport::NeedsServerConfig { hint } if hint.contains("shared_preload_libraries")
        ));
        f.preload = Some("pg_stat_statements, \"$libdir/plugin_debugger\"".into());
        assert_eq!(decide_support(&f), RoutineDebugSupport::Available);

        let mut f = facts();
        f.preload = None; // a non-privileged role cannot read the setting
        f.superuser = false;
        assert!(matches!(
            decide_support(&f),
            RoutineDebugSupport::NoPermission { hint } if hint.contains("superuser")
        ));
        let mut f = facts();
        f.can_execute = false;
        assert!(matches!(
            decide_support(&f),
            RoutineDebugSupport::NoPermission { .. }
        ));
    }

    #[test]
    fn flavors_from_version() {
        assert_eq!(
            flavor_from_version("PostgreSQL 16.4 (Debian)"),
            PgFlavor::Postgres
        );
        assert_eq!(
            flavor_from_version("PostgreSQL 11.2-YB-2.20.1.0-b0 on x86_64"),
            PgFlavor::Yugabyte
        );
        assert_eq!(
            flavor_from_version("CockroachDB CCL v23.2.0 (x86_64-pc-linux-gnu)"),
            PgFlavor::Cockroach
        );
        let mut f = facts();
        f.version = "CockroachDB CCL v23.2.0".into();
        assert!(matches!(
            decide_support(&f),
            RoutineDebugSupport::Unsupported { reason } if reason.contains("no debugger API")
        ));
        // Yugabyte follows the same steps as PostgreSQL.
        let mut f = facts();
        f.version = "PostgreSQL 11.2-YB-2.20.1.0".into();
        assert_eq!(decide_support(&f), RoutineDebugSupport::Available);
        f.extension_version = None;
        f.extension_available = false;
        assert!(matches!(
            decide_support(&f),
            RoutineDebugSupport::Unsupported { .. }
        ));
    }

    #[test]
    fn global_breakpoint_always_names_a_pid() {
        assert_eq!(
            global_breakpoint_sql(3, 16423, -1, 279),
            "SELECT pldbg_set_global_breakpoint(3, 16423::oid, -1, 279)"
        );
        assert!(
            !global_breakpoint_sql(1, 1, 5, 7)
                .to_lowercase()
                .contains("null")
        );
    }

    #[test]
    fn literals_are_escaped() {
        assert_eq!(quote_literal("a'b"), "E'a''b'");
        assert_eq!(quote_literal("a\\b"), "E'a\\\\b'");
        assert_eq!(quote_literal("x\0y"), "E'xy'");
    }

    #[test]
    fn call_names_from_statements() {
        let n = call_names("SELECT f_outer(1), Public.F_Inner(2) FROM t WHERE x IN (1, 2)");
        assert_eq!(n.bare, ["f_outer", "in"]);
        assert_eq!(n.qualified, [("public".to_string(), "f_inner".to_string())]);

        let n = call_names("CALL my_proc(1)");
        assert_eq!(n.bare, ["my_proc"]);
        let n = call_names("call s.p()");
        assert_eq!(n.qualified, [("s".to_string(), "p".to_string())]);
        // CALL without parentheses is still a call target.
        let n = call_names("CALL p");
        assert_eq!(n.bare, ["p"]);

        let n = call_names("SELECT \"My Func\"(1), 'not_a_call(1)', $$also_not(2)$$");
        assert_eq!(n.bare, ["My Func"]);
        // Casts, columns and comments are not calls.
        let n = call_names("SELECT a::int, b /* c(1) */ -- d(2)\n FROM t");
        assert!(n.is_empty());
        // Only the first identifier of a chain starts a candidate.
        let n = call_names("SELECT a.b.c(1)");
        assert_eq!(n.qualified, [("b".to_string(), "c".to_string())]);
    }

    #[test]
    fn candidate_queries_shape() {
        let n = call_names("SELECT f(1), s.g(2)");
        let sql = candidates_sql(&n, false);
        assert!(sql.contains("pg_function_is_visible"));
        assert!(sql.contains("LIMIT 32"));
        assert!(sql.contains("E'f'") && sql.contains("E's.g'"));
        assert!(!candidates_sql(&n, true).contains("pg_function_is_visible"));
        let t = trigger_candidates_sql(&["public.t".into(), "o'x".into()]);
        assert!(t.contains("to_regclass(E'public.t')") && t.contains("E'o''x'"));
        assert!(t.contains("NOT t.tgisinternal"));
    }

    #[test]
    fn first_statement_needs_the_begin_line() {
        let src = "\nDECLARE\n  r integer;\nBEGIN\n  r := 1;\n  RETURN r;\nEND\n";
        // Line 5 is the first statement; the BEGIN is on line 4.
        assert_eq!(first_statement_alias(src, 5), Some(4));
        assert_eq!(first_statement_alias(src, 6), None);
        assert_eq!(first_statement_alias(src, 4), None);
        // Comments and strings are skipped when looking for BEGIN.
        let src = "\nDECLARE\n  s text := 'BEGIN';\n  -- BEGIN\nBEGIN\n  /* c */\n  s := s;\nEND";
        assert_eq!(first_statement_alias(src, 7), Some(5));
        // No DECLARE: BEGIN is the first word.
        assert_eq!(first_statement_alias("BEGIN\n  RETURN 1;\nEND", 2), Some(1));
        // Statement on the BEGIN line itself: nothing to add.
        assert_eq!(first_statement_alias("BEGIN RETURN 1; END", 1), None);
    }

    #[test]
    fn hashing_and_display() {
        assert_eq!(
            source_hash(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(display_value("integer", "42"), "42");
        assert_eq!(display_value("text", "it's"), "'it''s'");
        assert_eq!(display_value("text", "NULL"), "NULL");
        let long = "x".repeat(300);
        assert_eq!(display_value("text", &long).chars().count(), 200);
    }

    #[test]
    fn target_gone_detection() {
        let gone = Error::Query {
            message: "debugger connection terminated".into(),
            position: None,
        };
        assert!(is_target_gone(&gone));
        assert!(!is_target_gone(&Error::Cancelled));
        assert!(
            guidance(&RoutineDebugSupport::NeedsExtension {
                statement: "CREATE EXTENSION pldbgapi;".into()
            })
            .contains("CREATE EXTENSION")
        );
    }
}
