//! Query plan commands (package E): EXPLAIN, parsing and `.dbplan.json` files.

use std::path::Path;

use dbstudio_core::{
    ConfirmationGrant, ConfirmationRequest, ConnectionConfig, DestructiveStatement, Error,
    ExplainRequest, RawPlan, ScriptDialect, StatementClass, TextRange,
};
use dbstudio_plan::{PlanError, PlanRules, PlanTree};
use dbstudio_store::NewPlanHistory;
use serde::Serialize;
use tauri::State;
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
/// comments and quoted identifiers).
fn has_impure_calls(text: &str) -> bool {
    any_word(text, |word, depth| {
        impure_call(word) || (depth == 0 && word == "into")
    })
}

/// Whether a destructive keyword appears in `text` outside strings, comments and quoted
/// identifiers (used for statements the classifier did not understand).
fn has_destructive_keyword(text: &str) -> bool {
    any_word(text, |word, _| {
        matches!(word, "drop" | "truncate" | "alter" | "delete" | "update")
    })
}

/// Calls `f(lowercase word, parenthesis depth)` for every bare word of `text` (skipping
/// strings, comments and quoted identifiers) until it returns true.
fn any_word(text: &str, mut f: impl FnMut(&str, u32) -> bool) -> bool {
    let b = text.as_bytes();
    let (mut i, mut depth) = (0usize, 0u32);
    while i < b.len() {
        match b[i] {
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            b'#' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            }
            q @ (b'\'' | b'"' | b'`') => {
                i += 1;
                while i < b.len() {
                    if b[i] == b'\\' && q == b'\'' {
                        i += 2;
                        continue;
                    }
                    if b[i] == q {
                        if b.get(i + 1) == Some(&q) {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                i += 1;
            }
            c if c.is_ascii_alphabetic() || c == b'_' => {
                let start = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
                let word = text[start..i].to_ascii_lowercase();
                if f(&word, depth) {
                    return true;
                }
            }
            _ => i += 1,
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
        && !has_impure_calls(&text);
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
                ) && has_destructive_keyword(&text)
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
    check_explain(
        &state.confirm,
        session.script_dialect(),
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
    let entry = NewPlanHistory {
        connection_id: Some(connection_id),
        database,
        // Secrets inside literals never reach the history.
        statement: dbstudio_sql::redact::redact(dialect, &raw.statement),
        raw_plan: raw.clone(),
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

pub(crate) fn parse_impl(
    state: &AppState,
    raw: RawPlan,
    rules: Option<PlanRules>,
) -> Result<PlanTree> {
    dbstudio_plan::parse_and_analyze(&raw, &effective_rules(state, rules)).map_err(plan_error)
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
pub fn plan_parse(
    state: State<'_, AppState>,
    raw: RawPlan,
    rules: Option<PlanRules>,
) -> Result<PlanTree> {
    tracing::debug!(command = "plan_parse", "ipc call");
    parse_impl(&state, raw, rules)
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

    #[test]
    fn parse_returns_an_analysed_tree() {
        let (state, _, _) = test_state_with_host(DriverRegistry::new());
        let tree = parse_impl(&state, raw(PlanFormat::PostgresJson, PG_SPILL), None).unwrap();
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
        let strict =
            parse_impl(&state, raw(PlanFormat::PostgresJson, PG_SPILL), Some(rules)).unwrap();
        assert!(
            strict
                .warnings
                .iter()
                .any(|w| w.rule == PlanRule::FullScanLargeTable)
        );
    }

    #[test]
    fn parse_errors_are_plain_errors() {
        let (state, _, _) = test_state_with_host(DriverRegistry::new());
        let e = parse_impl(&state, raw(PlanFormat::PostgresJson, "{}"), None).unwrap_err();
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
    fn the_keyword_scan_ignores_strings_comments_and_quoted_names() {
        for text in [
            "frobnicate; DELETE FROM t",
            "x drop y",
            "call p(); truncate t",
            "alter session set x = 1",
            "update t set a = 1",
        ] {
            assert!(has_destructive_keyword(text), "{text}");
        }
        for text in [
            "frobnicate t",
            "frobnicate 'delete' -- drop",
            "frobnicate \"update\" /* truncate */",
            "frobnicate `alter` # drop",
            "deleted updates dropbox",
        ] {
            assert!(!has_destructive_keyword(text), "{text}");
        }
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
