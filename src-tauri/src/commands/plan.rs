//! Query plan commands (package E): EXPLAIN, parsing and `.dbplan.json` files.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dbstudio_core::{
    ConfirmationGrant, ConfirmationRequest, ConnectionConfig, DestructiveStatement, Error,
    ExplainRequest, ObjectSearchRequest, RawPlan, ScriptDialect, StatementClass, TextRange,
    TxStatus,
};
use dbstudio_plan::advice::{
    Flavor, Resolution, ServerFacts, resolve_table, search_kind, search_scope,
};
use dbstudio_plan::{
    AdviceMetadata, AdviceReport, AdvisorContext, Caveat, MetadataState, PlanError, PlanRules,
    PlanTree, TableFailure, TableKey, TableMeta, TableRef,
};
use dbstudio_store::NewPlanHistory;
use serde::Serialize;
use tauri::State;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use uuid::Uuid;

use super::app::write_atomic;
use super::blocking;
use crate::grants::Purpose;
use crate::guard::{self, ConfirmTokens};
use crate::host::FileFilter;
use crate::state::AppState;

type Result<T> = std::result::Result<T, Error>;

/// Result of `plan_explain`: the raw plan always, the tree when it could be parsed.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanResult {
    pub raw: RawPlan,
    pub tree: Option<PlanTree>,
    pub parse_error: Option<String>,
}

fn plan_filters() -> Vec<FileFilter> {
    vec![FileFilter {
        name: "DBStudio plan".into(),
        extensions: vec!["json".into()],
    }]
}

fn plan_error(e: PlanError) -> Error {
    match e {
        PlanError::Unsupported(format) => Error::Unsupported(format!("plan_format:{format:?}")),
        PlanError::Invalid(message) => Error::Invalid(message),
    }
}

/// Rules the user asked for, else the defaults with the misestimate factor from the settings.
fn effective_rules(state: &AppState, rules: Option<PlanRules>) -> PlanRules {
    rules.unwrap_or_else(|| {
        let factor = state
            .connections
            .settings()
            .map(|s| f64::from(s.plan_misestimate_factor))
            .unwrap_or_else(|_| PlanRules::default().misestimate_factor);
        PlanRules {
            misestimate_factor: factor,
            ..PlanRules::default()
        }
    })
}

/// Functions that act outside the transaction of an ANALYZE (locks, sequences, other servers,
/// files) or on the server itself: a statement calling them is not a pure read.
fn impure_call(word: &str) -> bool {
    const EXACT: &[&str] = &[
        "nextval",
        "setval",
        "lo_import",
        "lo_export",
        "pg_terminate_backend",
        "pg_cancel_backend",
        "pg_reload_conf",
        "get_lock",
        "release_lock",
        "release_all_locks",
        "load_file",
    ];
    EXACT.contains(&word)
        || word.starts_with("pg_advisory")
        || word.starts_with("pg_try_advisory")
        || word.starts_with("dblink")
}

/// Whether `text` calls an impure function or has a top-level `INTO` (outside strings,
/// comments and quoted identifiers, as the dialect's own lexer sees them).
fn has_impure_calls(dialect: ScriptDialect, text: &str) -> bool {
    any_word(dialect, text, |word, depth| {
        impure_call(word) || (depth == 0 && word == "into")
    })
}

/// Whether a statement the classifier did not understand contains a keyword that changes or
/// destroys data, outside strings, comments and quoted identifiers.
fn has_destructive_keyword(dialect: ScriptDialect, text: &str) -> bool {
    any_word(dialect, text, |word, _| {
        matches!(
            word,
            "drop"
                | "truncate"
                | "alter"
                | "delete"
                | "update"
                | "insert"
                | "merge"
                | "grant"
                | "create"
                | "rename"
        )
    })
}

/// Calls `f(lowercase word, parenthesis depth)` for every bare word of `text` (the dialect's
/// lexer skips strings, comments and quoted identifiers: `#` starts a comment only where the
/// dialect says so, `\'` escapes only where it is allowed) until `f` returns true.
fn any_word(dialect: ScriptDialect, text: &str, mut f: impl FnMut(&str, u32) -> bool) -> bool {
    use dbstudio_sql::lexer::{TokenKind, tokenize};
    let chars: Vec<char> = text.chars().collect();
    let mut depth = 0u32;
    for token in tokenize(dialect, text) {
        let (start, end) = (token.range.start as usize, token.range.end as usize);
        let Some(slice) = chars.get(start..end) else {
            continue;
        };
        match token.kind {
            TokenKind::Punct if slice == ['('] => depth += 1,
            TokenKind::Punct if slice == [')'] => depth = depth.saturating_sub(1),
            TokenKind::Word => {
                let word: String = slice.iter().collect::<String>().to_ascii_lowercase();
                if f(&word, depth) {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// The guard of `plan_explain`. A plain EXPLAIN never runs the statement and always passes
/// (the drivers make sure it is exactly one statement). EXPLAIN ANALYZE runs it, so anything
/// that is not a pure read is refused on read-only connections and needs the typed
/// confirmation on production ones, like `query_execute`.
pub(crate) fn check_explain(
    tokens: &ConfirmTokens,
    dialect: ScriptDialect,
    config: &ConnectionConfig,
    request: &ExplainRequest,
    target: &str,
    confirm: Option<&ConfirmationGrant>,
) -> Result<()> {
    if !request.analyze {
        return Ok(());
    }
    let analysis = dbstudio_sql::analyze(dialect, &request.text);
    let statements: Vec<_> = analysis
        .statements
        .iter()
        .filter(|s| s.class != StatementClass::Empty)
        .collect();
    let [stmt] = statements.as_slice() else {
        return Err(Error::Query {
            message: "EXPLAIN takes exactly one statement".into(),
            position: statements.get(1).map(|s| s.range.start),
        });
    };
    let text: String = request
        .text
        .chars()
        .skip(stmt.range.start as usize)
        .take((stmt.range.end - stmt.range.start) as usize)
        .collect();
    let pure_read = analysis.problems.is_empty()
        && stmt.class == StatementClass::Read
        && !has_impure_calls(dialect, &text);
    if pure_read {
        return Ok(());
    }
    let (read_only, production) = guard::policy_flags(config);
    if read_only {
        return Err(Error::ReadOnlyViolation {
            message: "EXPLAIN ANALYZE executes the statement, which is not a plain read".into(),
            position: Some(stmt.range.start),
        });
    }
    if !production {
        return Ok(());
    }
    // The grant is bound to this statement run as an ANALYZE on this target.
    let payload = [request.text.as_bytes(), b"\0explain-analyze"].concat();
    let confirm_text = guard::confirm_text_for(config, request.database.as_deref());
    if let Some(grant) = confirm {
        match tokens.consume(grant, target, &payload) {
            Ok(()) => return Ok(()),
            Err(why) => tracing::debug!(?why, "explain confirmation refused"),
        }
    }
    let (token, expires_in_ms) = tokens.issue(target, &payload, &confirm_text);
    Err(Error::ConfirmationRequired(Box::new(ConfirmationRequest {
        token,
        confirm_text,
        statements: vec![DestructiveStatement {
            index: stmt.index,
            range: TextRange {
                start: stmt.range.start,
                end: stmt.range.end,
            },
            text: text.chars().take(2000).collect(),
            // Plain writes have no "destructive" reason; the statement is listed so the
            // dialog shows what will run. A statement the classifier did not understand is
            // scanned for destructive keywords instead.
            reason: stmt.destructive.unwrap_or_else(|| {
                if matches!(
                    stmt.class,
                    StatementClass::Unknown | StatementClass::Procedural
                ) && has_destructive_keyword(dialect, &text)
                {
                    dbstudio_core::DestructiveReason::KeywordScan
                } else {
                    dbstudio_core::DestructiveReason::Write
                }
            }),
        }],
        expires_in_ms,
    })))
}

pub(crate) async fn explain_impl(
    state: &AppState,
    session_id: Uuid,
    lease_id: Option<Uuid>,
    request: ExplainRequest,
    rules: Option<PlanRules>,
    confirm: Option<ConfirmationGrant>,
) -> Result<PlanResult> {
    let (session, config) = state.sessions.get_with_config(session_id)?;
    let target = match lease_id {
        Some(id) => format!("lease:{id}"),
        None => format!("session:{session_id}"),
    };
    let check_dialect = match lease_id {
        Some(id) => state
            .leases
            .get(id)?
            .script_dialect()
            .unwrap_or_else(|| session.script_dialect()),
        None => session.script_dialect(),
    };
    check_explain(
        &state.confirm,
        check_dialect,
        &config,
        &request,
        &target,
        confirm.as_ref(),
    )?;
    let database = request.database.clone();
    let raw = match lease_id {
        Some(lease_id) => state.leases.get(lease_id)?.explain(request).await?,
        None => session.explain(request).await?,
    };
    let rules = effective_rules(state, rules);
    let (tree, parse_error) = match dbstudio_plan::parse_and_analyze(&raw, &rules) {
        Ok(tree) => (Some(tree), None),
        Err(e) => (None, Some(e.to_string())),
    };
    record_history(
        state,
        config.id,
        session.script_dialect(),
        database,
        &raw,
        tree.as_ref(),
    )
    .await;
    Ok(PlanResult {
        raw,
        tree,
        parse_error,
    })
}

/// Keep the plan in the plan history when the setting is on. Never fails the explain: a
/// storage problem or an oversized plan is only logged.
async fn record_history(
    state: &AppState,
    connection_id: Uuid,
    dialect: ScriptDialect,
    database: Option<String>,
    raw: &RawPlan,
    tree: Option<&PlanTree>,
) {
    let svc = state.connections.clone();
    let statement = dbstudio_sql::redact::redact(dialect, &raw.statement);
    if statement != raw.statement {
        // The plan text repeats the statement's literals (filters, index conditions), so a
        // statement that carried a secret cannot be kept with its plan: skip it entirely
        // rather than store the secret.
        tracing::debug!("plan not kept in the history: the statement contains a secret");
        return;
    }
    let mut raw_plan = raw.clone();
    raw_plan.statement = statement.clone();
    let entry = NewPlanHistory {
        connection_id: Some(connection_id),
        database,
        statement,
        raw_plan,
        total_cost: tree.and_then(|t| t.totals.total_cost),
        total_ms: tree.and_then(|t| t.totals.execution_time_ms),
        warnings: tree.map_or(0, |t| u32::try_from(t.warnings.len()).unwrap_or(u32::MAX)),
    };
    let result = blocking(move || {
        let settings = svc.settings()?;
        if !settings.plan_history_enabled {
            return Ok(false);
        }
        svc.store()
            .plan_history_add(&entry, settings.plan_history_max_per_connection)
            .map(|_| true)
    })
    .await;
    if let Err(e) = result {
        tracing::debug!(error = %e, "plan not kept in the history");
    }
}

/// Parse and analyse a plan (with its advice) on a blocking thread: a big plan or statement
/// must not stall the async runtime.
pub(crate) async fn parse_impl(
    state: &AppState,
    raw: RawPlan,
    rules: Option<PlanRules>,
) -> Result<PlanTree> {
    let rules = effective_rules(state, rules);
    blocking(move || dbstudio_plan::parse_and_analyze(&raw, &rules).map_err(plan_error)).await
}

/// Limits of the catalog lookup behind `plan_advise` (design section 5.5).
#[derive(Debug, Clone, Copy)]
pub(crate) struct AdviseLimits {
    /// Most tables read; the rest count as skipped.
    pub max_tables: usize,
    /// Time allowed for one table (search and details).
    pub per_table: Duration,
    /// Time allowed for the whole lookup.
    pub total: Duration,
    /// Tables read at the same time.
    pub parallel: usize,
}

impl Default for AdviseLimits {
    fn default() -> Self {
        AdviseLimits {
            max_tables: 6,
            per_table: Duration::from_secs(3),
            total: Duration::from_secs(8),
            parallel: 3,
        }
    }
}

/// Why one table could not be read.
enum LookupFailure {
    Timeout,
    NotFound,
    Ambiguous,
    Unsupported(String),
    Failed(String),
}

impl LookupFailure {
    fn from_error(e: Error) -> Self {
        match e {
            Error::NotFound(_) => LookupFailure::NotFound,
            Error::Timeout => LookupFailure::Timeout,
            Error::Unsupported(m) => LookupFailure::Unsupported(m),
            other => LookupFailure::Failed(other.to_string()),
        }
    }

    fn reason(&self) -> &'static str {
        match self {
            LookupFailure::Timeout => "timeout",
            LookupFailure::NotFound => "notFound",
            LookupFailure::Ambiguous => "ambiguous",
            LookupFailure::Unsupported(_) => "unsupported",
            LookupFailure::Failed(_) => "error",
        }
    }

    fn message(&self) -> String {
        match self {
            LookupFailure::Unsupported(m) | LookupFailure::Failed(m) => {
                m.chars().take(200).collect()
            }
            _ => String::new(),
        }
    }
}

/// Find `table` in the catalog and read its columns and indexes.
async fn lookup_table(
    session: Arc<dyn dbstudio_core::Session>,
    flavor: Flavor,
    table: TableRef,
    request_database: Option<String>,
    search_path: Vec<String>,
) -> std::result::Result<TableMeta, LookupFailure> {
    let found = session
        .search_objects(ObjectSearchRequest {
            query: table.name.clone(),
            scope: search_scope(flavor, &table, request_database.as_deref()),
            kinds: vec![search_kind(flavor)],
            limit: 50,
        })
        .await
        .map_err(LookupFailure::from_error)?;
    let path = match resolve_table(flavor, &table, &search_path, &found) {
        Resolution::Found(path) => path,
        Resolution::Ambiguous => return Err(LookupFailure::Ambiguous),
        Resolution::NotFound => return Err(LookupFailure::NotFound),
    };
    let details = session
        .table_details(&path)
        .await
        .map_err(LookupFailure::from_error)?;
    Ok(TableMeta {
        path,
        estimated_rows: details.estimated_rows,
        columns: details.columns,
        indexes: details.indexes,
    })
}

fn add_caveat(advice: &mut dbstudio_plan::PlanAdvice, key: &str, verify: bool) {
    if !advice.caveats.iter().any(|c| c.key == key) {
        advice.caveats.push(Caveat {
            key: key.into(),
            verify,
            params: Default::default(),
        });
    }
}

/// Query Advisor, tier 2 entry point (design sections 5.5 and 7): advice from the plan and the
/// statement, then again with what the catalog says about the tables the first pass named.
/// A table that cannot be read lowers the confidence of its advice; it never fails the call.
pub(crate) async fn advise_impl(
    state: &AppState,
    session_id: Uuid,
    lease_id: Option<Uuid>,
    raw: RawPlan,
    database: Option<String>,
    rules: Option<PlanRules>,
    use_catalog: bool,
) -> Result<AdviceReport> {
    let enabled = state
        .connections
        .settings()
        .map_or(true, |s| s.plan_advisor_metadata);
    advise_with(
        state,
        session_id,
        lease_id,
        raw,
        database,
        rules,
        enabled,
        use_catalog,
        AdviseLimits::default(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn advise_with(
    state: &AppState,
    session_id: Uuid,
    lease_id: Option<Uuid>,
    raw: RawPlan,
    database: Option<String>,
    rules: Option<PlanRules>,
    metadata_enabled: bool,
    use_catalog: bool,
    limits: AdviseLimits,
) -> Result<AdviceReport> {
    let started = Instant::now();
    let (session, _config) = state.sessions.get_with_config(session_id)?;
    let plan_dialect = dbstudio_plan::advice::dialect_of_format(raw.format, &raw.server_version);
    let (conn_dialect, in_transaction) = match lease_id {
        Some(id) => {
            let lease = state.leases.get(id)?;
            (
                lease
                    .script_dialect()
                    .unwrap_or_else(|| session.script_dialect()),
                lease.state().transaction == TxStatus::Active,
            )
        }
        None => (session.script_dialect(), false),
    };
    let rules = effective_rules(state, rules);
    // The connection's dialect (quoting flags of the live session) only applies to a plan of its
    // own product; a plan of another product keeps its own dialect and never meets this catalog.
    let same_product =
        std::mem::discriminant(&conn_dialect) == std::mem::discriminant(&plan_dialect);
    let dialect = if same_product {
        conn_dialect
    } else {
        plan_dialect
    };
    let mut facts = ServerFacts {
        in_transaction: same_product && in_transaction,
        ..ServerFacts::default()
    };
    let tree = {
        let (raw, rules, facts) = (raw.clone(), rules, facts.clone());
        blocking(move || {
            let mut ctx = AdvisorContext::basic(&raw, &rules);
            ctx.dialect = dialect;
            ctx.server = facts;
            dbstudio_plan::parse_analyze_advise(&raw, &rules, &ctx).map_err(plan_error)
        })
        .await?
    };
    let mut metadata = AdviceMetadata {
        state: MetadataState::NotRequested,
        ..AdviceMetadata::default()
    };
    if !metadata_enabled {
        metadata.state = MetadataState::Disabled;
        return Ok(AdviceReport {
            advice: tree.advice,
            metadata,
        });
    }
    // A plan of another product, or one that has no known connection (opened from a file),
    // says nothing reliable about this catalog: matching it would index the wrong tables.
    if !same_product || !use_catalog {
        metadata.state = MetadataState::Unavailable;
        metadata.reason = Some(
            if same_product {
                "planWithoutConnection"
            } else {
                "dialectMismatch"
            }
            .into(),
        );
        return Ok(AdviceReport {
            advice: tree.advice,
            metadata,
        });
    }
    // Tables the first pass wants to know about, most important advice first.
    let mut wanted: Vec<(TableKey, TableRef)> = Vec::new();
    for a in &tree.advice {
        if let Some(t) = &a.table {
            let key = TableKey::new(t.schema.as_deref(), &t.name);
            if !wanted.iter().any(|(k, _)| *k == key) {
                wanted.push((key, t.clone()));
            }
        }
    }
    if wanted.is_empty() {
        return Ok(AdviceReport {
            advice: tree.advice,
            metadata,
        });
    }
    metadata.skipped =
        u32::try_from(wanted.len().saturating_sub(limits.max_tables)).unwrap_or(u32::MAX);
    wanted.truncate(limits.max_tables);
    let deadline = started + limits.total;
    let flavor = {
        let mut probe = AdvisorContext::basic(&raw, &rules);
        probe.dialect = dialect;
        probe.flavor()
    };
    // Schemas a bare table name resolves against, in order (PostgreSQL: the session's
    // `search_path`, led by the lease's current schema).
    let mut search_path: Vec<String> = match flavor {
        Flavor::MsSql => vec!["dbo".to_string()],
        _ => Vec::new(),
    };
    let pg_like = matches!(
        flavor,
        Flavor::Postgres | Flavor::Yugabyte | Flavor::Cockroach
    );
    if pg_like {
        if let Some(schema) = lease_id
            .and_then(|id| state.leases.get(id).ok())
            .and_then(|l| l.state().schema)
            .filter(|s| !s.is_empty())
        {
            search_path.push(schema);
        }
    }
    // Edition and search path (one `server_info` call).
    if pg_like || matches!(flavor, Flavor::MsSql | Flavor::Oracle) {
        let wait = limits
            .per_table
            .min(deadline.saturating_duration_since(Instant::now()));
        if let Ok(Ok(info)) = tokio::time::timeout(wait, session.server_info()).await {
            facts.edition = info
                .extra
                .get("edition")
                .or_else(|| info.extra.get("banner"))
                .cloned();
            facts.engine_edition = info.extra.get("engineEdition").and_then(|e| e.parse().ok());
            if flavor == Flavor::Oracle {
                search_path.extend(info.extra.get("user").cloned());
            }
            if pg_like {
                search_path.extend(
                    info.extra
                        .get("searchPath")
                        .into_iter()
                        .flat_map(|p| p.split('\u{1f}'))
                        .filter(|s| !s.is_empty() && *s != "pg_catalog")
                        .map(str::to_string),
                );
            }
        }
    }
    if pg_like && search_path.is_empty() {
        search_path.push("public".into());
    }
    let mut tables: BTreeMap<TableKey, TableMeta> = BTreeMap::new();
    // Read the tables, a few at a time.
    let gate = Arc::new(Semaphore::new(limits.parallel.max(1)));
    let mut tasks: JoinSet<(usize, std::result::Result<TableMeta, LookupFailure>)> = JoinSet::new();
    for (i, (_, table)) in wanted.iter().enumerate() {
        let session = Arc::clone(&session);
        let gate = Arc::clone(&gate);
        let table = table.clone();
        let database = database.clone();
        let search_path = search_path.clone();
        let per_table = limits.per_table;
        tasks.spawn(async move {
            let _permit = gate.acquire_owned().await;
            let left = deadline.saturating_duration_since(Instant::now());
            let wait = per_table.min(left);
            let outcome = if wait.is_zero() {
                Err(LookupFailure::Timeout)
            } else {
                match tokio::time::timeout(
                    wait,
                    lookup_table(session, flavor, table, database, search_path),
                )
                .await
                {
                    Ok(r) => r,
                    Err(_) => Err(LookupFailure::Timeout),
                }
            };
            (i, outcome)
        });
    }
    let mut outcomes: Vec<Option<std::result::Result<TableMeta, LookupFailure>>> =
        (0..wanted.len()).map(|_| None).collect();
    while let Some(done) = tasks.join_next().await {
        match done {
            Ok((i, outcome)) => outcomes[i] = Some(outcome),
            Err(e) => tracing::debug!(error = %e, "plan metadata task failed"),
        }
    }
    let mut failures: Vec<(TableKey, LookupFailure)> = Vec::new();
    for ((key, table), outcome) in wanted.iter().zip(outcomes) {
        match outcome.unwrap_or(Err(LookupFailure::Failed("lookup aborted".into()))) {
            Ok(meta) => {
                let mut checked = table.clone();
                checked.path = Some(meta.path.clone());
                metadata.checked.push(checked);
                tables.insert(key.clone(), meta);
            }
            Err(f) => {
                metadata.failed.push(TableFailure {
                    table: table.name.clone(),
                    reason: f.reason().into(),
                    message: f.message(),
                });
                failures.push((key.clone(), f));
            }
        }
    }
    metadata.state = if metadata.checked.is_empty() {
        MetadataState::Unavailable
    } else if metadata.failed.is_empty() && metadata.skipped == 0 {
        MetadataState::Checked
    } else {
        MetadataState::Partial
    };
    let state_now = metadata.state;
    let (tree, tables) = {
        let (raw, rules, facts) = (raw.clone(), rules, facts.clone());
        blocking(move || {
            let mut ctx = AdvisorContext::basic(&raw, &rules);
            ctx.dialect = dialect;
            ctx.server = facts;
            ctx.tables = tables;
            ctx.metadata = state_now;
            let mut tree = tree;
            tree.advice = dbstudio_plan::advise(&tree, &raw.statement, &ctx);
            Ok((tree, ctx.tables))
        })
        .await?
    };
    let mut tree = tree;
    let mut ctx = AdvisorContext::basic(&raw, &rules);
    ctx.tables = tables;
    for a in &mut tree.advice {
        let Some(t) = a.table.as_mut() else { continue };
        let key = TableKey::new(t.schema.as_deref(), &t.name);
        if let Some(meta) = ctx.table_meta(t.schema.as_deref(), &t.name) {
            t.path = Some(meta.path.clone());
        }
        let failure = failures.iter().find(|(k, _)| *k == key).map(|(_, f)| f);
        match failure {
            Some(LookupFailure::Ambiguous) => add_caveat(a, "ambiguousTable", true),
            Some(LookupFailure::NotFound) if lease_id.is_some() => {
                add_caveat(a, "leaseObjectsInvisible", true);
            }
            _ => {}
        }
    }
    metadata.elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    Ok(AdviceReport {
        advice: tree.advice,
        metadata,
    })
}

/// Save a plan through a Save dialog; `None` when cancelled.
pub(crate) async fn file_save_impl(state: &AppState, raw: RawPlan) -> Result<Option<String>> {
    let host = state.host.clone();
    let grants = state.grants.clone();
    blocking(move || {
        let Some(picked) = host.pick_save_path_filtered("plan.dbplan.json", &plan_filters())?
        else {
            return Ok(None);
        };
        let path = grants.grant(&picked, Purpose::Write)?;
        let json = serde_json::to_vec_pretty(&raw)
            .map_err(|e| Error::Other(format!("cannot serialize plan: {e}")))?;
        write_atomic(&path, &json)?;
        Ok(Some(path.to_string_lossy().into_owned()))
    })
    .await
}

/// Open a plan through an Open dialog; `None` when cancelled.
pub(crate) async fn file_open_impl(state: &AppState) -> Result<Option<RawPlan>> {
    let host = state.host.clone();
    let grants = state.grants.clone();
    blocking(move || {
        let Some(picked) = host.pick_open_path(&plan_filters(), false)? else {
            return Ok(None);
        };
        let path = grants.grant(&picked, Purpose::Read)?;
        let bytes = std::fs::read(Path::new(&path))?;
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| Error::Invalid(format!("not a DBStudio plan file: {e}")))
    })
    .await
}

/// Plan history (PLAN-07, package E): newest first, without the raw plans.
pub(crate) async fn history_list_impl(
    state: &AppState,
    connection_id: Option<Uuid>,
    limit: u32,
    before: Option<String>,
) -> Result<Vec<dbstudio_store::PlanHistoryEntry>> {
    let svc = state.connections.clone();
    blocking(move || {
        svc.store()
            .plan_history_list(connection_id, limit, before.as_deref())
    })
    .await
}

/// One plan history entry with its raw plan.
pub(crate) async fn history_get_impl(
    state: &AppState,
    id: Uuid,
) -> Result<dbstudio_store::PlanHistoryRecord> {
    let svc = state.connections.clone();
    blocking(move || svc.store().plan_history_get(id)).await
}

pub(crate) async fn history_delete_impl(state: &AppState, id: Uuid) -> Result<()> {
    let svc = state.connections.clone();
    blocking(move || svc.store().plan_history_delete(id)).await
}

/// Delete the plan history of one connection (all when `None`); returns how many rows went.
pub(crate) async fn history_clear_impl(
    state: &AppState,
    connection_id: Option<Uuid>,
) -> Result<u32> {
    let svc = state.connections.clone();
    blocking(move || svc.store().plan_history_clear(connection_id)).await
}

#[tauri::command]
pub async fn plan_history_list(
    state: State<'_, AppState>,
    connection_id: Option<Uuid>,
    limit: u32,
    before: Option<String>,
) -> Result<Vec<dbstudio_store::PlanHistoryEntry>> {
    tracing::debug!(command = "plan_history_list", "ipc call");
    history_list_impl(&state, connection_id, limit, before).await
}

#[tauri::command]
pub async fn plan_history_get(
    state: State<'_, AppState>,
    id: Uuid,
) -> Result<dbstudio_store::PlanHistoryRecord> {
    tracing::debug!(command = "plan_history_get", "ipc call");
    history_get_impl(&state, id).await
}

#[tauri::command]
pub async fn plan_history_delete(state: State<'_, AppState>, id: Uuid) -> Result<()> {
    tracing::debug!(command = "plan_history_delete", "ipc call");
    history_delete_impl(&state, id).await
}

#[tauri::command]
pub async fn plan_history_clear(
    state: State<'_, AppState>,
    connection_id: Option<Uuid>,
) -> Result<u32> {
    tracing::debug!(command = "plan_history_clear", "ipc call");
    history_clear_impl(&state, connection_id).await
}

#[tauri::command]
pub async fn plan_explain(
    state: State<'_, AppState>,
    session_id: Uuid,
    lease_id: Option<Uuid>,
    request: ExplainRequest,
    rules: Option<PlanRules>,
    confirm: Option<ConfirmationGrant>,
) -> Result<PlanResult> {
    tracing::debug!(command = "plan_explain", session = %session_id, "ipc call");
    explain_impl(&state, session_id, lease_id, request, rules, confirm).await
}

#[tauri::command]
pub async fn plan_parse(
    state: State<'_, AppState>,
    raw: RawPlan,
    rules: Option<PlanRules>,
) -> Result<PlanTree> {
    tracing::debug!(command = "plan_parse", "ipc call");
    parse_impl(&state, raw, rules).await
}

#[tauri::command]
pub async fn plan_advise(
    state: State<'_, AppState>,
    session_id: Uuid,
    lease_id: Option<Uuid>,
    raw: RawPlan,
    database: Option<String>,
    rules: Option<PlanRules>,
    use_catalog: Option<bool>,
) -> Result<AdviceReport> {
    tracing::debug!(command = "plan_advise", session = %session_id, "ipc call");
    advise_impl(
        &state,
        session_id,
        lease_id,
        raw,
        database,
        rules,
        use_catalog.unwrap_or(true),
    )
    .await
}

#[tauri::command]
pub async fn plan_file_save(state: State<'_, AppState>, raw: RawPlan) -> Result<Option<String>> {
    tracing::debug!(command = "plan_file_save", "ipc call");
    file_save_impl(&state, raw).await
}

#[tauri::command]
pub async fn plan_file_open(state: State<'_, AppState>) -> Result<Option<RawPlan>> {
    tracing::debug!(command = "plan_file_open", "ipc call");
    file_open_impl(&state).await
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use dbstudio_core::{DriverRegistry, PlanFormat};
    use dbstudio_plan::PlanRule;

    use super::*;
    use crate::state::testing::test_state_with_host;

    const PG_SPILL: &str =
        include_str!("../../../crates/dbstudio-plan/tests/fixtures/pg_04_sort_spill_analyze.json");

    fn raw(format: PlanFormat, text: &str) -> RawPlan {
        RawPlan {
            format,
            text: text.into(),
            statement: "select * from items order by sku".into(),
            analyzed: true,
            rolled_back: true,
            server_version: "16.2".into(),
            captured_at: "2025-01-01T00:00:00Z".into(),
            extra: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn parse_returns_an_analysed_tree() {
        let (state, _, _) = test_state_with_host(DriverRegistry::new());
        let tree = parse_impl(&state, raw(PlanFormat::PostgresJson, PG_SPILL), None)
            .await
            .unwrap();
        assert!(tree.analyzed);
        assert!(
            tree.warnings
                .iter()
                .any(|w| w.rule == PlanRule::SpillToDisk)
        );
        assert_eq!(tree.hot_path.first(), Some(&0));
        // Explicit rules win over the defaults.
        let rules = PlanRules {
            large_table_rows: 10.0,
            ..PlanRules::default()
        };
        let strict = parse_impl(&state, raw(PlanFormat::PostgresJson, PG_SPILL), Some(rules))
            .await
            .unwrap();
        assert!(
            strict
                .warnings
                .iter()
                .any(|w| w.rule == PlanRule::FullScanLargeTable)
        );
    }

    #[tokio::test]
    async fn parse_errors_are_plain_errors() {
        let (state, _, _) = test_state_with_host(DriverRegistry::new());
        let e = parse_impl(&state, raw(PlanFormat::PostgresJson, "{}"), None)
            .await
            .unwrap_err();
        assert!(matches!(e, Error::Invalid(_)), "{e:?}");
    }

    #[test]
    fn default_rules_use_the_settings_factor() {
        let (state, _, _) = test_state_with_host(DriverRegistry::new());
        let rules = effective_rules(&state, None);
        assert!(rules.misestimate_factor >= 1.0);
        assert_eq!(
            rules.large_table_rows,
            PlanRules::default().large_table_rows
        );
    }

    #[tokio::test]
    async fn plan_files_round_trip_through_dialogs() {
        let (state, _, host) = test_state_with_host(DriverRegistry::new());
        let original = raw(PlanFormat::PostgresJson, PG_SPILL);
        // Cancelled dialogs do nothing.
        assert_eq!(
            file_save_impl(&state, original.clone()).await.unwrap(),
            None
        );
        assert_eq!(file_open_impl(&state).await.unwrap(), None);

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("q.dbplan.json");
        *host.save_to.lock().unwrap() = Some(target.clone());
        let saved = file_save_impl(&state, original.clone())
            .await
            .unwrap()
            .unwrap();
        assert!(saved.ends_with("q.dbplan.json"));
        assert_eq!(
            host.suggested.lock().unwrap().as_slice(),
            ["plan.dbplan.json", "plan.dbplan.json"]
        );

        *host.open_from.lock().unwrap() = Some(target.clone());
        let opened = file_open_impl(&state).await.unwrap().unwrap();
        assert_eq!(opened, original);

        // A file that is not a plan file is rejected, not misread.
        let junk = dir.path().join("junk.json");
        std::fs::write(&junk, "[1, 2]").unwrap();
        *host.open_from.lock().unwrap() = Some(junk);
        assert!(file_open_impl(&state).await.is_err());
    }

    // ---- guard of plan_explain ----

    use dbstudio_core::{DbKind, Environment};

    fn config(read_only: bool, production: bool) -> ConnectionConfig {
        let mut c = ConnectionConfig::new(DbKind::Postgres, "prod", "localhost");
        c.database = Some("shop".into());
        c.read_only = read_only;
        c.environment = production.then_some(Environment::Production);
        c
    }

    fn req(text: &str, analyze: bool) -> ExplainRequest {
        ExplainRequest {
            query_id: Uuid::new_v4(),
            text: text.into(),
            database: None,
            analyze,
            options: Default::default(),
            timeout_ms: None,
        }
    }

    fn check(c: &ConnectionConfig, text: &str, analyze: bool) -> Result<()> {
        check_explain(
            &ConfirmTokens::new(),
            ScriptDialect::Postgres,
            c,
            &req(text, analyze),
            "session:1",
            None,
        )
    }

    #[test]
    fn plain_explain_always_passes_the_guard() {
        for (ro, prod) in [(false, false), (true, false), (true, true), (false, true)] {
            for sql in ["UPDATE t SET a = 1", "DELETE FROM t", "select 1"] {
                check(&config(ro, prod), sql, false).unwrap();
            }
        }
    }

    #[test]
    fn analyze_of_a_pure_read_passes_everywhere() {
        for (ro, prod) in [(false, false), (true, false), (true, true), (false, true)] {
            check(&config(ro, prod), "SELECT * FROM t WHERE a = 1", true).unwrap();
            check(
                &config(ro, prod),
                "WITH x AS (SELECT 1) SELECT * FROM x",
                true,
            )
            .unwrap();
        }
    }

    #[test]
    fn read_only_connections_refuse_analyze_of_anything_but_reads() {
        let ro = config(true, false);
        for sql in [
            "UPDATE t SET a = 1",
            "DELETE FROM t",
            "INSERT INTO t VALUES (1)",
            "WITH x AS (DELETE FROM t RETURNING *) SELECT * FROM x",
            "CREATE TABLE t2 AS SELECT 1",
            "SELECT pg_advisory_lock(1)",
            "SELECT nextval('s')",
            "SELECT dblink_exec('x', 'drop table t')",
            "SELECT * INTO t3 FROM t",
            "SELECT 1; DELETE FROM t",
            "SELECT 'unterminated",
        ] {
            let e = check(&ro, sql, true).unwrap_err();
            assert!(
                matches!(e, Error::ReadOnlyViolation { .. } | Error::Query { .. }),
                "{sql}: {e:?}"
            );
        }
        // Words inside strings and quoted identifiers do not count.
        check(&ro, "SELECT 'nextval(', \"dblink\" FROM t", true).unwrap();
    }

    #[test]
    fn production_asks_for_the_typed_confirmation_once() {
        let tokens = ConfirmTokens::new();
        let prod = config(false, true);
        let text = "UPDATE orders SET status = 'x' WHERE id = 1";
        let run = |text: &str, grant: Option<&ConfirmationGrant>| {
            check_explain(
                &tokens,
                ScriptDialect::Postgres,
                &prod,
                &req(text, true),
                "session:1",
                grant,
            )
        };
        let Err(Error::ConfirmationRequired(c)) = run(text, None) else {
            panic!("expected a confirmation");
        };
        assert_eq!(c.confirm_text, "shop");
        assert_eq!(c.statements.len(), 1);
        assert!(c.statements[0].text.starts_with("UPDATE orders"));
        // Wrong text: refused and a fresh token is issued (the old one is spent).
        let wrong = ConfirmationGrant {
            token: c.token.clone(),
            typed: "nope".into(),
        };
        assert!(matches!(
            run(text, Some(&wrong)),
            Err(Error::ConfirmationRequired(_))
        ));
        let Err(Error::ConfirmationRequired(c)) = run(text, None) else {
            panic!()
        };
        let grant = ConfirmationGrant {
            token: c.token.clone(),
            typed: "shop".into(),
        };
        // A grant for another statement does not carry over.
        assert!(matches!(
            run("DELETE FROM orders WHERE id = 2", Some(&grant)),
            Err(Error::ConfirmationRequired(_))
        ));
        let Err(Error::ConfirmationRequired(c)) = run(text, None) else {
            panic!()
        };
        let grant = ConfirmationGrant {
            token: c.token,
            typed: "shop".into(),
        };
        run(text, Some(&grant)).unwrap();
        // Single use.
        assert!(matches!(
            run(text, Some(&grant)),
            Err(Error::ConfirmationRequired(_))
        ));
        // Destructive statements name their reason.
        let Err(Error::ConfirmationRequired(c)) = run("DELETE FROM orders", None) else {
            panic!()
        };
        assert_eq!(
            c.statements[0].reason,
            dbstudio_core::DestructiveReason::DeleteWithoutWhere
        );
    }

    #[test]
    fn development_connections_may_analyze_writes() {
        check(&config(false, false), "UPDATE t SET a = 1", true).unwrap();
    }

    #[tokio::test]
    async fn explain_impl_applies_the_guard_before_the_driver() {
        let (driver, probe) = crate::session::testing::MockDriver::new(DbKind::Postgres);
        let mut registry = DriverRegistry::new();
        registry.register(driver);
        let (state, _, _) = test_state_with_host(registry);
        let mut cfg = config(true, false);
        cfg.name = "ro".into();
        let profile = dbstudio_store::ConnectionProfile {
            config: cfg,
            group_id: None,
            color: None,
            save_password: false,
            has_password: false,
            sort_order: 0,
            created_at: String::new(),
            updated_at: String::new(),
        };
        let saved = crate::commands::connections::save_impl(
            &state,
            profile,
            dbstudio_store::SecretChange::Keep,
        )
        .await
        .unwrap();
        let session_id = crate::commands::sessions::open_impl(&state, saved.config.id, None)
            .await
            .unwrap()
            .session_id;
        // The mock session has an opaque dialect: everything is "unknown", so ANALYZE is
        // refused before the driver is reached.
        let e = explain_impl(&state, session_id, None, req("select 1", true), None, None)
            .await
            .unwrap_err();
        assert!(matches!(e, Error::ReadOnlyViolation { .. }), "{e:?}");
        // A plain EXPLAIN reaches the driver.
        let done = explain_impl(&state, session_id, None, req("select 1", false), None, None)
            .await
            .unwrap();
        assert!(done.tree.is_some());
        let _ = probe;
    }

    async fn state_with_session(read_only: bool, production: bool) -> (AppState, Uuid, Uuid) {
        let (driver, _) = crate::session::testing::MockDriver::new(DbKind::Postgres);
        let mut registry = DriverRegistry::new();
        registry.register(driver);
        let (state, _, _) = test_state_with_host(registry);
        let profile = dbstudio_store::ConnectionProfile {
            config: config(read_only, production),
            group_id: None,
            color: None,
            save_password: false,
            has_password: false,
            sort_order: 0,
            created_at: String::new(),
            updated_at: String::new(),
        };
        let saved = crate::commands::connections::save_impl(
            &state,
            profile,
            dbstudio_store::SecretChange::Keep,
        )
        .await
        .unwrap();
        let session_id = crate::commands::sessions::open_impl(&state, saved.config.id, None)
            .await
            .unwrap()
            .session_id;
        (state, session_id, saved.config.id)
    }

    #[tokio::test]
    async fn explained_plans_land_in_the_history_with_redacted_statements() {
        let (state, sid, conn) = state_with_session(false, false).await;
        assert!(
            history_list_impl(&state, None, 10, None)
                .await
                .unwrap()
                .is_empty()
        );
        explain_impl(&state, sid, None, req("select 1", false), None, None)
            .await
            .unwrap();
        let mut analyzed = req("select 2", true);
        analyzed.database = Some("shop".into());
        explain_impl(&state, sid, None, analyzed, None, None)
            .await
            .unwrap();
        let list = history_list_impl(&state, Some(conn), 10, None)
            .await
            .unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].statement, "select 2");
        assert!(list[0].analyze);
        assert_eq!(list[0].database.as_deref(), Some("shop"));
        assert_eq!(list[0].connection_id, Some(conn));
        assert!(list[0].total_cost.is_some());
        assert!(list[0].warnings >= 1, "the spill fixture has warnings");
        let json = serde_json::to_value(&list[0]).unwrap();
        assert!(json.get("rawPlan").is_none(), "the list has no raw plans");
        let record = history_get_impl(&state, list[0].id).await.unwrap();
        assert_eq!(record.raw_plan.statement, "select 2");
        assert_eq!(record.entry, list[0]);
        history_delete_impl(&state, list[0].id).await.unwrap();
        assert_eq!(history_clear_impl(&state, Some(conn)).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn statements_with_secrets_never_reach_the_history() {
        let (driver, probe) = crate::session::testing::MockDriver::new(DbKind::Postgres);
        *probe.dialect.lock().unwrap() = Some(ScriptDialect::Postgres);
        let mut registry = DriverRegistry::new();
        registry.register(driver);
        let (state, _, _) = test_state_with_host(registry);
        let saved = crate::commands::connections::save_impl(
            &state,
            dbstudio_store::ConnectionProfile {
                config: config(false, false),
                group_id: None,
                color: None,
                save_password: false,
                has_password: false,
                sort_order: 0,
                created_at: String::new(),
                updated_at: String::new(),
            },
            dbstudio_store::SecretChange::Keep,
        )
        .await
        .unwrap();
        let sid = crate::commands::sessions::open_impl(&state, saved.config.id, None)
            .await
            .unwrap()
            .session_id;
        explain_impl(&state, sid, None, req("select 1", false), None, None)
            .await
            .unwrap();
        explain_impl(
            &state,
            sid,
            None,
            req("select PASSWORD('hunter2')", false),
            None,
            None,
        )
        .await
        .unwrap();
        let list = history_list_impl(&state, None, 10, None).await.unwrap();
        assert_eq!(list.len(), 1, "the statement with a secret is not kept");
        let record = history_get_impl(&state, list[0].id).await.unwrap();
        assert!(!serde_json::to_string(&record).unwrap().contains("hunter2"));
    }

    #[tokio::test]
    async fn the_history_respects_its_settings() {
        let (state, sid, conn) = state_with_session(false, false).await;
        crate::commands::app::settings_set_impl(
            &state,
            dbstudio_store::AppSettings {
                plan_history_max_per_connection: 10,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        for i in 0..13 {
            explain_impl(
                &state,
                sid,
                None,
                req(&format!("select {i}"), false),
                None,
                None,
            )
            .await
            .unwrap();
        }
        let list = history_list_impl(&state, Some(conn), 100, None)
            .await
            .unwrap();
        assert_eq!(list.len(), 10, "trimmed to the setting");
        assert_eq!(list[0].statement, "select 12");
        // Off: nothing more is recorded, and the explain still works.
        crate::commands::app::settings_set_impl(
            &state,
            dbstudio_store::AppSettings {
                plan_history_enabled: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        explain_impl(&state, sid, None, req("select off", false), None, None)
            .await
            .unwrap();
        let list = history_list_impl(&state, Some(conn), 100, None)
            .await
            .unwrap();
        assert!(list.iter().all(|e| e.statement != "select off"));
    }

    #[test]
    fn the_keyword_scan_follows_the_dialect_lexer() {
        let pg = ScriptDialect::Postgres;
        let my = ScriptDialect::mysql(false, true);
        for text in [
            "frobnicate; DELETE FROM t",
            "x drop y",
            "call p(); truncate t",
            "alter session set x = 1",
            "update t set a = 1",
            "frobnicate insert",
            "x merge y",
            "grant all",
            "create thing",
            "rename a to b",
        ] {
            assert!(has_destructive_keyword(pg, text), "{text}");
        }
        for text in [
            "frobnicate t",
            "frobnicate 'delete' -- drop",
            "frobnicate \"update\" /* truncate */",
            "deleted updates dropbox",
        ] {
            assert!(!has_destructive_keyword(pg, text), "{text}");
        }
        // `#` starts a comment in MySQL only; backticks quote there only.
        assert!(has_destructive_keyword(pg, "frobnicate # drop"));
        assert!(!has_destructive_keyword(my, "frobnicate # drop"));
        assert!(!has_destructive_keyword(my, "frobnicate `alter`"));
    }

    #[test]
    fn a_backslash_does_not_hide_an_impure_call_where_it_is_no_escape() {
        let pg = ScriptDialect::Postgres;
        let my = ScriptDialect::mysql(false, true);
        // PostgreSQL: the string is `a\`, `nextval` is a real call.
        assert!(has_impure_calls(pg, "SELECT 'a\\', nextval('s')"));
        // MySQL with backslash escapes: `\'` stays inside the string.
        assert!(!has_impure_calls(my, "SELECT 'a\\', nextval('s')"));
        assert!(has_impure_calls(pg, "SELECT * INTO t2 FROM t"));
        assert!(!has_impure_calls(pg, "SELECT (SELECT 1 INTO x) -- nextval"));
    }

    #[test]
    fn a_plain_write_is_listed_as_write_on_production() {
        let prod = config(false, true);
        let Err(Error::ConfirmationRequired(c)) = check_explain(
            &ConfirmTokens::new(),
            ScriptDialect::Postgres,
            &prod,
            &req("UPDATE t SET a=1 WHERE id=1", true),
            "session:1",
            None,
        ) else {
            panic!("expected a confirmation");
        };
        assert_eq!(
            c.statements[0].reason,
            dbstudio_core::DestructiveReason::Write
        );
    }
}

#[cfg(test)]
mod advise_tests {
    use std::collections::{BTreeMap, HashMap};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use dbstudio_core::{
        Capabilities, ColumnDetail, DbKind, Driver, DriverDescriptor, DriverRegistry, DriverStatus,
        ExecSummary, IndexDetail, ObjectKind, ObjectNode, ObjectPath, PlanFormat, QueryRequest,
        ResultSink, Secret, ServerInfo, Session, TableDetails, ValueKind,
    };

    use super::*;
    use crate::state::testing::test_state_with_host;

    /// Two sequential scans under a join, each reading 200 000 (100 000) rows to keep 10.
    const JOIN_PLAN: &str = r#"[{"Plan":{"Node Type":"Hash Join","Join Type":"Inner",
        "Startup Cost":0.0,"Total Cost":9000.0,"Plan Rows":10,"Actual Startup Time":1.0,
        "Actual Total Time":200.0,"Actual Rows":10,"Actual Loops":1,
        "Hash Cond":"(o.customer_id = c.id)","Plans":[
        {"Node Type":"Seq Scan","Parent Relationship":"Outer","Relation Name":"orders",
         "Alias":"o","Startup Cost":0.0,"Total Cost":4000.0,"Plan Rows":10,
         "Actual Startup Time":0.1,"Actual Total Time":100.0,"Actual Rows":10,"Actual Loops":1,
         "Filter":"(status = 'x'::text)","Rows Removed by Filter":199990},
        {"Node Type":"Seq Scan","Parent Relationship":"Inner","Relation Name":"customers",
         "Alias":"c","Startup Cost":0.0,"Total Cost":3000.0,"Plan Rows":10,
         "Actual Startup Time":0.1,"Actual Total Time":80.0,"Actual Rows":10,"Actual Loops":1,
         "Filter":"(region = 'eu'::text)","Rows Removed by Filter":99990}]},
        "Execution Time":200.5}]"#;

    /// The same join as an estimate-only plan: the advice depends on the table sizes.
    const ESTIMATE_PLAN: &str = r#"[{"Plan":{"Node Type":"Hash Join","Join Type":"Inner",
        "Startup Cost":0.0,"Total Cost":9000.0,"Plan Rows":10,
        "Hash Cond":"(o.customer_id = c.id)","Plans":[
        {"Node Type":"Seq Scan","Parent Relationship":"Outer","Relation Name":"orders",
         "Alias":"o","Startup Cost":0.0,"Total Cost":4000.0,"Plan Rows":50000,
         "Filter":"(status = 'x'::text)"},
        {"Node Type":"Seq Scan","Parent Relationship":"Inner","Relation Name":"customers",
         "Alias":"c","Startup Cost":0.0,"Total Cost":3000.0,"Plan Rows":50000,
         "Filter":"(region = 'eu'::text)"}]}}]"#;

    /// Counts what the advisor asks the catalog.
    #[derive(Default)]
    struct Calls {
        searches: AtomicUsize,
        details: AtomicUsize,
    }

    /// A catalog of `(schema, table)` pairs; tables named in `slow` take five seconds.
    struct Catalog {
        tables: Vec<(String, String)>,
        slow: Vec<String>,
        indexes: HashMap<String, Vec<IndexDetail>>,
        calls: Arc<Calls>,
        lost_details: Mutex<bool>,
        /// What `server_info` reports as the session's `search_path` (unit separated).
        search_path: Option<String>,
    }

    struct FakeDriver(Arc<Catalog>);

    struct FakeSession(Arc<Catalog>);

    fn path_of(schema: &str, table: &str) -> ObjectPath {
        ObjectPath::root()
            .child(ObjectKind::Database, "shop")
            .child(ObjectKind::Folder, "schemas")
            .child(ObjectKind::Schema, schema)
            .child(ObjectKind::Folder, "tables")
            .child(ObjectKind::Table, table)
    }

    #[async_trait]
    impl Driver for FakeDriver {
        fn descriptor(&self) -> DriverDescriptor {
            DriverDescriptor::new(
                DbKind::Postgres,
                Capabilities::planned_for(DbKind::Postgres),
                DriverStatus::Available,
            )
        }

        async fn connect(
            &self,
            _config: &ConnectionConfig,
            _secret: Option<&Secret>,
        ) -> Result<Box<dyn Session>> {
            Ok(Box::new(FakeSession(self.0.clone())))
        }
    }

    #[async_trait]
    impl Session for FakeSession {
        async fn ping(&self) -> Result<()> {
            Ok(())
        }
        async fn server_info(&self) -> Result<ServerInfo> {
            let mut info = ServerInfo::default();
            if let Some(p) = &self.0.search_path {
                info.extra.insert("searchPath".into(), p.clone());
            }
            Ok(info)
        }
        async fn execute(&self, _: QueryRequest, _: &mut dyn ResultSink) -> Result<ExecSummary> {
            Err(Error::Unsupported("execute".into()))
        }
        async fn list_children(&self, _: &ObjectPath) -> Result<Vec<ObjectNode>> {
            Ok(vec![])
        }
        fn script_dialect(&self) -> ScriptDialect {
            ScriptDialect::Postgres
        }
        async fn search_objects(&self, req: ObjectSearchRequest) -> Result<Vec<ObjectNode>> {
            self.0.calls.searches.fetch_add(1, Ordering::SeqCst);
            Ok(self
                .0
                .tables
                .iter()
                .filter(|(_, t)| t.contains(&req.query.to_lowercase()))
                .map(|(s, t)| ObjectNode {
                    path: path_of(s, t),
                    kind: ObjectKind::Table,
                    name: t.clone(),
                    has_children: true,
                    detail: Some(s.clone()),
                })
                .collect())
        }
        async fn table_details(&self, path: &ObjectPath) -> Result<TableDetails> {
            self.0.calls.details.fetch_add(1, Ordering::SeqCst);
            let name = path.last().map(|s| s.name.clone()).unwrap_or_default();
            if self.0.slow.contains(&name) {
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            if *self.0.lost_details.lock().unwrap() {
                return Err(Error::Connection("gone".into()));
            }
            let column = |n: &str| ColumnDetail {
                name: n.into(),
                ordinal: 1,
                type_name: "text".into(),
                value_kind: ValueKind::Text,
                nullable: true,
                default: None,
                comment: None,
                primary_key: false,
                extra: None,
            };
            Ok(TableDetails {
                path: path.clone(),
                kind: ObjectKind::Table,
                comment: None,
                columns: vec![column("status"), column("region")],
                indexes: self.0.indexes.get(&name).cloned().unwrap_or_default(),
                foreign_keys: vec![],
                estimated_rows: Some(1_000_000),
                size_bytes: None,
            })
        }
        async fn close(&self) -> Result<()> {
            Ok(())
        }
    }

    fn catalog(tables: &[(&str, &str)]) -> Catalog {
        Catalog {
            tables: tables
                .iter()
                .map(|(s, t)| (s.to_string(), t.to_string()))
                .collect(),
            slow: vec![],
            indexes: HashMap::new(),
            calls: Arc::new(Calls::default()),
            lost_details: Mutex::new(false),
            search_path: None,
        }
    }

    async fn open(cat: Catalog) -> (AppState, Uuid, Arc<Calls>) {
        let calls = cat.calls.clone();
        let mut registry = DriverRegistry::new();
        registry.register(Arc::new(FakeDriver(Arc::new(cat))));
        let (state, _, _) = test_state_with_host(registry);
        let mut cfg = ConnectionConfig::new(DbKind::Postgres, "shop", "localhost");
        cfg.database = Some("shop".into());
        let profile = dbstudio_store::ConnectionProfile {
            config: cfg,
            group_id: None,
            color: None,
            save_password: false,
            has_password: false,
            sort_order: 0,
            created_at: String::new(),
            updated_at: String::new(),
        };
        let saved = crate::commands::connections::save_impl(
            &state,
            profile,
            dbstudio_store::SecretChange::Keep,
        )
        .await
        .unwrap();
        let sid = crate::commands::sessions::open_impl(&state, saved.config.id, None)
            .await
            .unwrap()
            .session_id;
        (state, sid, calls)
    }

    fn raw(text: &str) -> RawPlan {
        RawPlan {
            format: PlanFormat::PostgresJson,
            text: text.into(),
            statement: "select * from orders o join customers c on o.customer_id = c.id".into(),
            analyzed: true,
            rolled_back: true,
            server_version: "16.2".into(),
            captured_at: "2025-01-01T00:00:00Z".into(),
            extra: BTreeMap::new(),
        }
    }

    fn quick() -> AdviseLimits {
        AdviseLimits {
            per_table: Duration::from_millis(300),
            total: Duration::from_secs(2),
            ..AdviseLimits::default()
        }
    }

    async fn run(
        state: &AppState,
        sid: Uuid,
        enabled: bool,
        limits: AdviseLimits,
    ) -> Result<AdviceReport> {
        advise_with(
            state,
            sid,
            None,
            raw(JOIN_PLAN),
            None,
            None,
            enabled,
            true,
            limits,
        )
        .await
    }

    async fn run_estimate(state: &AppState, sid: Uuid, limits: AdviseLimits) -> AdviceReport {
        let mut r = raw(ESTIMATE_PLAN);
        r.analyzed = false;
        advise_with(state, sid, None, r, None, None, true, true, limits)
            .await
            .unwrap()
    }

    fn advice_for<'a>(r: &'a AdviceReport, table: &str) -> Vec<&'a dbstudio_plan::PlanAdvice> {
        r.advice
            .iter()
            .filter(|a| a.table.as_ref().is_some_and(|t| t.name == table))
            .collect()
    }

    fn has_caveat(a: &dbstudio_plan::PlanAdvice, key: &str) -> bool {
        a.caveats.iter().any(|c| c.key == key)
    }

    #[tokio::test]
    async fn full_metadata_is_checked_and_resolves_paths() {
        let (state, sid, calls) =
            open(catalog(&[("public", "orders"), ("public", "customers")])).await;
        let report = run(&state, sid, true, quick()).await.unwrap();
        assert_eq!(report.metadata.state, MetadataState::Checked);
        assert_eq!(report.metadata.checked.len(), 2);
        assert!(report.metadata.failed.is_empty());
        assert_eq!(calls.details.load(Ordering::SeqCst), 2);
        let orders = advice_for(&report, "orders");
        assert!(!orders.is_empty());
        for a in &orders {
            let path = a.table.as_ref().unwrap().path.clone().expect("path");
            assert_eq!(path, path_of("public", "orders"));
            assert!(!has_caveat(a, "metadataUnavailable"), "{:?}", a.caveats);
        }
        assert!(report.metadata.elapsed_ms < 2_000);
    }

    #[tokio::test]
    async fn a_slow_table_makes_the_report_partial_and_lowers_confidence() {
        let mut cat = catalog(&[("public", "orders"), ("public", "customers")]);
        cat.slow = vec!["customers".into()];
        let (state, sid, _) = open(cat).await;
        let report = run_estimate(&state, sid, quick()).await;
        assert_eq!(report.metadata.state, MetadataState::Partial);
        assert_eq!(report.metadata.checked.len(), 1);
        assert_eq!(report.metadata.failed.len(), 1);
        assert_eq!(report.metadata.failed[0].table, "customers");
        assert_eq!(report.metadata.failed[0].reason, "timeout");
        let customers = advice_for(&report, "customers");
        assert!(!customers.is_empty());
        for a in customers {
            assert!(has_caveat(a, "metadataUnavailable"), "{:?}", a.caveats);
            assert_eq!(a.confidence, dbstudio_plan::Confidence::Low);
            assert!(a.table.as_ref().unwrap().path.is_none());
        }
        let orders = advice_for(&report, "orders");
        assert!(!orders.is_empty());
        for a in orders {
            assert!(!has_caveat(a, "metadataUnavailable"), "{:?}", a.caveats);
            assert!(a.table.as_ref().unwrap().path.is_some());
        }
    }

    #[tokio::test]
    async fn the_total_budget_stops_slow_lookups() {
        let mut cat = catalog(&[("public", "orders"), ("public", "customers")]);
        cat.slow = vec!["orders".into(), "customers".into()];
        let (state, sid, _) = open(cat).await;
        let limits = AdviseLimits {
            per_table: Duration::from_secs(3),
            total: Duration::from_millis(200),
            parallel: 1,
            ..AdviseLimits::default()
        };
        let started = Instant::now();
        let report = run(&state, sid, true, limits).await.unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(report.metadata.state, MetadataState::Unavailable);
        assert!(report.metadata.failed.iter().all(|f| f.reason == "timeout"));
        assert_eq!(report.metadata.failed.len(), 2);
        assert!(!report.advice.is_empty(), "tier 1 advice stays");
    }

    #[tokio::test]
    async fn too_many_tables_are_skipped() {
        let (state, sid, calls) =
            open(catalog(&[("public", "orders"), ("public", "customers")])).await;
        let limits = AdviseLimits {
            max_tables: 1,
            ..quick()
        };
        let report = run(&state, sid, true, limits).await.unwrap();
        assert_eq!(report.metadata.skipped, 1);
        assert_eq!(report.metadata.state, MetadataState::Partial);
        assert_eq!(calls.details.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn the_setting_off_never_touches_the_catalog() {
        let (state, sid, calls) =
            open(catalog(&[("public", "orders"), ("public", "customers")])).await;
        let report = run(&state, sid, false, quick()).await.unwrap();
        assert_eq!(report.metadata.state, MetadataState::Disabled);
        assert!(report.metadata.checked.is_empty());
        assert!(!report.advice.is_empty());
        assert_eq!(calls.searches.load(Ordering::SeqCst), 0);
        assert_eq!(calls.details.load(Ordering::SeqCst), 0);
        // The real entry point reads the setting (on by default).
        let on = advise_impl(&state, sid, None, raw(JOIN_PLAN), None, None, true)
            .await
            .unwrap();
        assert_eq!(on.metadata.state, MetadataState::Checked);
    }

    #[tokio::test]
    async fn a_lost_session_is_not_found_and_a_bad_plan_is_invalid() {
        let (state, sid, _) = open(catalog(&[])).await;
        let lost = advise_impl(
            &state,
            Uuid::new_v4(),
            None,
            raw(JOIN_PLAN),
            None,
            None,
            true,
        )
        .await
        .unwrap_err();
        assert_eq!(lost.kind(), "notFound");
        let bad = advise_impl(&state, sid, None, raw("nope"), None, None, true)
            .await
            .unwrap_err();
        assert_eq!(bad.kind(), "invalid");
        // A lease that is gone is not found either.
        let gone = advise_impl(
            &state,
            sid,
            Some(Uuid::new_v4()),
            raw(JOIN_PLAN),
            None,
            None,
            true,
        )
        .await
        .unwrap_err();
        assert_eq!(gone.kind(), "notFound");
    }

    #[tokio::test]
    async fn same_name_in_two_schemas_is_ambiguous_unless_the_default_schema_has_it() {
        let (state, sid, _) = open(catalog(&[
            ("sales", "orders"),
            ("archive", "orders"),
            ("public", "customers"),
        ]))
        .await;
        let report = run(&state, sid, true, quick()).await.unwrap();
        let failure = report
            .metadata
            .failed
            .iter()
            .find(|f| f.table == "orders")
            .expect("orders fails");
        assert_eq!(failure.reason, "ambiguous");
        assert_eq!(report.metadata.state, MetadataState::Partial);
        for a in advice_for(&report, "orders") {
            assert!(has_caveat(a, "ambiguousTable"));
            assert!(a.table.as_ref().unwrap().path.is_none());
        }
        // `public` wins when it holds exactly one of them.
        let (state, sid, _) = open(catalog(&[
            ("public", "orders"),
            ("archive", "orders"),
            ("public", "customers"),
        ]))
        .await;
        let report = run(&state, sid, true, quick()).await.unwrap();
        assert_eq!(report.metadata.state, MetadataState::Checked);
    }

    #[tokio::test]
    async fn the_session_search_path_decides_between_schemas() {
        let tables = [
            ("sales", "orders"),
            ("archive", "orders"),
            ("sales", "customers"),
        ];
        // `archive` is first on the path: its `orders` is the one a bare name means.
        let mut cat = catalog(&tables);
        cat.search_path = Some("archive\u{1f}sales\u{1f}pg_catalog".into());
        let (state, sid, _) = open(cat).await;
        let report = run(&state, sid, true, quick()).await.unwrap();
        assert_eq!(
            report.metadata.state,
            MetadataState::Checked,
            "{:?}",
            report.metadata
        );
        let orders = report
            .metadata
            .checked
            .iter()
            .find(|t| t.name == "orders")
            .unwrap();
        let path = orders.path.as_ref().unwrap();
        assert!(
            path.segments.iter().any(|s| s.name == "archive"),
            "{path:?}"
        );
        // Same tables, `public` hard-coded would have said ambiguous: with neither schema on
        // the path it is still ambiguous (never a guess).
        let mut cat = catalog(&tables);
        cat.search_path = Some("elsewhere".into());
        let (state, sid, _) = open(cat).await;
        let report = run(&state, sid, true, quick()).await.unwrap();
        assert!(
            report
                .metadata
                .failed
                .iter()
                .any(|f| f.table == "orders" && f.reason == "ambiguous")
        );
    }

    #[tokio::test]
    async fn a_plan_of_another_product_or_without_a_connection_never_reads_the_catalog() {
        let (state, sid, calls) =
            open(catalog(&[("public", "orders"), ("public", "customers")])).await;
        // A MySQL plan on a PostgreSQL session.
        let mysql = RawPlan {
            format: PlanFormat::MySqlJson,
            text: r#"{"query_block":{"select_id":1,"table":{"table_name":"orders","access_type":"ALL",
                "rows_examined_per_scan":1000000,"filtered":"0.1","attached_condition":"(`s`.`orders`.`a` = 1)"}}}"#
                .into(),
            statement: "select * from orders where a = 1".into(),
            analyzed: false,
            rolled_back: false,
            server_version: "8.0.36".into(),
            captured_at: "2025-01-01T00:00:00Z".into(),
            extra: BTreeMap::new(),
        };
        let report = advise_with(&state, sid, None, mysql, None, None, true, true, quick())
            .await
            .unwrap();
        assert_eq!(report.metadata.state, MetadataState::Unavailable);
        assert_eq!(report.metadata.reason.as_deref(), Some("dialectMismatch"));
        // Tier 1 still speaks, in the plan's own dialect (backticks, not PostgreSQL quoting).
        for a in &report.advice {
            for act in &a.actions {
                if let Some(sql) = &act.sql {
                    assert!(!sql.contains("CONCURRENTLY"), "{sql}");
                }
            }
        }
        // A plan from a file: same product, but no connection to ask.
        let report = advise_with(
            &state,
            sid,
            None,
            raw(JOIN_PLAN),
            None,
            None,
            true,
            false,
            quick(),
        )
        .await
        .unwrap();
        assert_eq!(report.metadata.state, MetadataState::Unavailable);
        assert_eq!(
            report.metadata.reason.as_deref(),
            Some("planWithoutConnection")
        );
        assert!(!report.advice.is_empty());
        assert_eq!(calls.searches.load(Ordering::SeqCst), 0);
        assert_eq!(calls.details.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn missing_tables_and_failing_details_are_reported_not_fatal() {
        let (state, sid, _) = open(catalog(&[("public", "customers")])).await;
        let report = run(&state, sid, true, quick()).await.unwrap();
        assert_eq!(report.metadata.state, MetadataState::Partial);
        assert!(
            report
                .metadata
                .failed
                .iter()
                .any(|f| f.table == "orders" && f.reason == "notFound")
        );
        let cat = catalog(&[("public", "orders"), ("public", "customers")]);
        *cat.lost_details.lock().unwrap() = true;
        let (state, sid, _) = open(cat).await;
        let report = run(&state, sid, true, quick()).await.unwrap();
        assert_eq!(report.metadata.state, MetadataState::Unavailable);
        assert!(report.metadata.failed.iter().all(|f| f.reason == "error"));
    }

    #[tokio::test]
    async fn an_existing_index_turns_the_suggestion_into_a_statistics_hint() {
        let mut cat = catalog(&[("public", "orders"), ("public", "customers")]);
        cat.indexes.insert(
            "orders".into(),
            vec![IndexDetail {
                name: "ix_orders_status".into(),
                columns: vec!["status".into()],
                unique: false,
                primary: false,
                method: Some("btree".into()),
                definition: None,
                partial: false,
            }],
        );
        let (state, sid, _) = open(cat).await;
        let report = run(&state, sid, true, quick()).await.unwrap();
        let orders = advice_for(&report, "orders");
        assert!(
            orders
                .iter()
                .any(|a| a.rule == dbstudio_plan::AdviceRule::ExistingIndexUnused),
            "{:?}",
            orders.iter().map(|a| a.rule).collect::<Vec<_>>()
        );
        assert!(orders.iter().all(|a| {
            a.actions
                .iter()
                .all(|x| x.kind != dbstudio_plan::ActionKind::CreateIndex)
        }));
    }

    #[tokio::test]
    async fn a_plan_without_tables_asks_nothing() {
        let (state, sid, calls) = open(catalog(&[])).await;
        let plan = r#"[{"Plan":{"Node Type":"Result","Startup Cost":0.0,"Total Cost":0.01,
            "Plan Rows":1,"Actual Startup Time":0.0,"Actual Total Time":0.0,"Actual Rows":1,
            "Actual Loops":1},"Execution Time":0.1}]"#;
        let report = advise_with(
            &state,
            sid,
            None,
            raw(plan),
            None,
            None,
            true,
            true,
            quick(),
        )
        .await
        .unwrap();
        assert_eq!(report.metadata.state, MetadataState::NotRequested);
        assert_eq!(calls.searches.load(Ordering::SeqCst), 0);
    }
}
