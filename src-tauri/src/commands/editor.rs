//! SQL editor / console commands: script analysis, guarded execution, leases and SQL files.
//!
//! `query_execute` / `lease_execute` run the guard flow (design 1.5), record the query history
//! and stream the driver's events; the other lease commands forward to `dbstudio_core::Lease`.

use std::path::Path;

use dbstudio_core::{
    Error, ExecuteRequest, ExecuteResult, LeaseCloseReport, LeaseContext, LeaseOptions, LeaseState,
    ObjectPath, QueryEvent, Result, ScriptAnalysis, ScriptDialect, TxOptions,
};
use dbstudio_sql::{OffsetMap, ScriptTemplate, Vocabulary};
use serde::Serialize;
use tauri::State;
use tauri::ipc::Channel;
use uuid::Uuid;

use super::app::write_atomic;
use super::blocking;
use crate::grants::Purpose;
use crate::guard::{self, GuardInput};
use crate::history::{RowCounter, Tracker};
use crate::host::FileFilter;
use crate::lease::{LeaseInfo, LeaseReopened, MAX_LEASES_PER_SESSION};
use crate::sink::EventSink;
use crate::state::AppState;

/// Largest SQL file `sql_file_open` reads (bytes).
pub const MAX_SQL_FILE_BYTES: u64 = 50 * 1024 * 1024;

/// Result of `sql_file_open`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SqlFile {
    /// Path the user picked.
    pub path: String,
    /// File content (UTF-8, BOM removed).
    pub text: String,
}

fn sql_filters() -> Vec<FileFilter> {
    vec![FileFilter {
        name: "SQL".into(),
        extensions: vec!["sql".into(), "txt".into()],
    }]
}

pub(crate) fn analyze_impl(
    state: &AppState,
    session_id: Uuid,
    text: &str,
) -> Result<ScriptAnalysis> {
    let session = state.sessions.get(session_id)?;
    Ok(dbstudio_sql::analyze(session.script_dialect(), text))
}

pub(crate) fn vocabulary_impl(state: &AppState, session_id: Uuid) -> Result<Vocabulary> {
    let session = state.sessions.get(session_id)?;
    Ok(dbstudio_sql::vocabulary(session.script_dialect()))
}

/// Wrap `emit` so `StatementStart` ranges (positions in the bound text) are reported in the text
/// the user typed.
fn mapped_emit<F>(map: OffsetMap, mut emit: F) -> impl FnMut(QueryEvent) -> bool + Send
where
    F: FnMut(QueryEvent) -> bool + Send,
{
    move |event| match event {
        QueryEvent::StatementStart { mut start } => {
            start.range.start = map.to_original(start.range.start);
            start.range.end = map.to_original(start.range.end);
            emit(QueryEvent::StatementStart { start })
        }
        other => emit(other),
    }
}

/// Error positions come back in the bound text; the editor wants the typed text.
fn map_error(map: &OffsetMap, error: Error) -> Error {
    match error {
        Error::Query {
            message,
            position: Some(p),
        } => Error::Query {
            message,
            position: Some(map.to_original(p)),
        },
        other => other,
    }
}

/// Fill the limits the request left open from the settings: `editor_max_rows` caps the rows
/// kept per result set and `query_timeout_ms` (when not 0) bounds the statement. Values the
/// caller set win. A settings read failure leaves the request as it was.
pub(crate) async fn apply_editor_limits(state: &AppState, query: &mut dbstudio_core::QueryRequest) {
    if query.max_rows.is_some() && query.timeout_ms.is_some() {
        return;
    }
    let svc = state.connections.clone();
    match blocking(move || svc.settings()).await {
        Ok(settings) => {
            if query.max_rows.is_none() {
                query.max_rows = Some(u64::from(settings.editor_max_rows));
            }
            if query.timeout_ms.is_none() && settings.query_timeout_ms > 0 {
                query.timeout_ms = Some(settings.query_timeout_ms);
            }
        }
        Err(e) => tracing::warn!(error = %e, "cannot read the editor limits from the settings"),
    }
}

/// Run a script on a pooled connection of the session: guard (steps 1-4), then the driver with
/// events streamed through `emit`.
pub(crate) async fn execute_impl<F>(
    state: &AppState,
    session_id: Uuid,
    request: ExecuteRequest,
    emit: F,
) -> Result<ExecuteResult>
where
    F: FnMut(QueryEvent) -> bool + Send,
{
    let (session, config) = state.sessions.get_with_config(session_id)?;
    let ExecuteRequest {
        mut query,
        params,
        confirm,
        record_history,
    } = request;
    let target = format!("session:{session_id}");
    let confirm_text = guard::confirm_text_for(&config, None);
    let (read_only, production) = guard::policy_flags(&config);
    let dialect = session.script_dialect();
    let context = query
        .database
        .clone()
        .or_else(|| config.database.clone())
        .unwrap_or_default();
    let guarded = guard::run_guard(
        &state.confirm,
        &GuardInput {
            dialect,
            text: &query.text,
            params: &params,
            read_only,
            production,
            confirm_text: &confirm_text,
            confirm: confirm.as_ref(),
            target_id: &target,
            context: &context,
        },
    )?;
    let tracker = if record_history {
        Tracker::start(
            state,
            &config,
            query.database.clone(),
            dialect,
            &query.text,
            &guarded.analysis,
        )
        .await
    } else {
        None
    };
    let map = guarded.bound.map;
    query.text = guarded.bound.text;
    apply_editor_limits(state, &mut query).await;
    let counter = RowCounter::default();
    let seen = counter.clone();
    let mut emit = emit;
    let mut sink = EventSink::new(mapped_emit(map.clone(), move |event| {
        seen.observe(&event);
        emit(event)
    }));
    let result = session
        .execute(query, &mut sink)
        .await
        .map_err(|e| map_error(&map, e));
    drop(sink);
    let history_id = tracker.as_ref().map(Tracker::id);
    if let Some(tracker) = tracker {
        tracker.finish(&result, counter.rows()).await;
    }
    Ok(ExecuteResult {
        summary: result?,
        state: None,
        history_id,
    })
}

pub(crate) async fn lease_open_impl(
    state: &AppState,
    session_id: Uuid,
    options: LeaseOptions,
) -> Result<LeaseInfo> {
    let session = state.sessions.get(session_id)?;
    if state.leases.count_for_session(session_id) >= MAX_LEASES_PER_SESSION {
        return Err(Error::Busy(format!(
            "a session can hold at most {MAX_LEASES_PER_SESSION} leases"
        )));
    }
    let lease = session.open_lease(options).await?;
    state.leases.register(session_id, lease)
}

/// Reopen a lease whose connection broke (lost session, failed keepalive, idle limit); the lease
/// keeps its id, database, schema and auto-commit mode. `transaction_lost` tells the tab that a
/// transaction was open when the connection broke. A healthy lease is returned unchanged.
/// Concurrent calls for one lease are serialised: the later ones get the first one's result
/// (a second reopen would throw away the connection the first just made).
pub(crate) async fn lease_reopen_impl(state: &AppState, lease_id: Uuid) -> Result<LeaseReopened> {
    let (session_id, options) = state.leases.reopen_options(lease_id)?;
    let gate = state.leases.reopen_gate(lease_id);
    let mut last = gate.lock().await;
    if !state.leases.needs_reopen(lease_id)? {
        if let Some(done) = last.as_ref() {
            return Ok(done.clone());
        }
        return Ok(LeaseReopened {
            lease: LeaseInfo {
                lease_id,
                session_id,
                state: state.leases.state_of(lease_id)?,
            },
            transaction_lost: false,
        });
    }
    let session = state.sessions.get(session_id)?;
    let fresh = session.open_lease(options).await?;
    let transaction_lost = state.leases.replace(lease_id, fresh).await?;
    let done = LeaseReopened {
        lease: LeaseInfo {
            lease_id,
            session_id,
            state: state.leases.state_of(lease_id)?,
        },
        transaction_lost,
    };
    *last = Some(done.clone());
    Ok(done)
}

/// Result of `script_generate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScriptGenerated {
    /// The generated script.
    pub text: String,
    /// Dialect the text is written in (tells the editor how to lex it).
    pub dialect: ScriptDialect,
}

/// Description of the table at `path` for the template generator. Document stores have no
/// `table_details`: their columns come from the inferred schema model.
async fn template_details(
    session: &dyn dbstudio_core::Session,
    dialect: ScriptDialect,
    path: &ObjectPath,
) -> Result<dbstudio_core::TableDetails> {
    use dbstudio_core::{IndexDetail, ObjectKind, SchemaModelRequest};
    match dialect {
        ScriptDialect::RedisCli => Err(Error::Unsupported("script_generate".into())),
        ScriptDialect::MongoShell => {
            let name = path
                .last()
                .map(|s| s.name.clone())
                .ok_or_else(|| Error::Invalid("script_generate needs a collection path".into()))?;
            // The scope is the database: everything up to its segment.
            let end = path
                .segments
                .iter()
                .position(|s| s.kind == ObjectKind::Database)
                .map_or(0, |i| i + 1);
            let scope = ObjectPath {
                segments: path.segments[..end].to_vec(),
            };
            let model = session
                .schema_model(SchemaModelRequest {
                    scope,
                    tables: vec![path.clone()],
                    include_views: true,
                    include_comments: false,
                    detail: Default::default(),
                    sample_size: None,
                })
                .await?;
            let table = model
                .tables
                .into_iter()
                .find(|t| t.name == name)
                .ok_or_else(|| Error::NotFound(format!("collection {name}")))?;
            let mut indexes = Vec::new();
            if !table.primary_key.is_empty() {
                indexes.push(IndexDetail {
                    name: "_id_".into(),
                    columns: table.primary_key.clone(),
                    unique: true,
                    primary: true,
                    method: None,
                    definition: None,
                    partial: false,
                });
            }
            for (i, key) in table.unique_keys.iter().enumerate() {
                indexes.push(IndexDetail {
                    name: format!("unique_{i}"),
                    columns: key.clone(),
                    unique: true,
                    primary: false,
                    method: None,
                    definition: None,
                    partial: false,
                });
            }
            Ok(dbstudio_core::TableDetails {
                path: table.path,
                kind: table.kind,
                comment: table.comment,
                columns: table.columns,
                indexes,
                foreign_keys: Vec::new(),
                estimated_rows: table.estimated_rows,
                size_bytes: None,
            })
        }
        _ => session.table_details(path).await,
    }
}

/// Generate a `SELECT` / `INSERT` / `UPDATE` / `DELETE` template for a table (on top of
/// `dbstudio_sql::template`).
pub(crate) async fn script_generate_impl(
    state: &AppState,
    session_id: Uuid,
    path: ObjectPath,
    template: ScriptTemplate,
) -> Result<ScriptGenerated> {
    let session = state.sessions.get(session_id)?;
    let dialect = session.script_dialect();
    let details = template_details(&*session, dialect, &path).await?;
    Ok(ScriptGenerated {
        text: dbstudio_sql::template::generate(dialect, &details, &path, template),
        dialect,
    })
}

pub(crate) async fn lease_close_impl(state: &AppState, lease_id: Uuid) -> Result<LeaseCloseReport> {
    state.leases.close(lease_id).await
}

/// Like [`execute_impl`] on a lease; the confirmation text follows the lease's current database.
pub(crate) async fn lease_execute_impl<F>(
    state: &AppState,
    lease_id: Uuid,
    request: ExecuteRequest,
    emit: F,
) -> Result<ExecuteResult>
where
    F: FnMut(QueryEvent) -> bool + Send,
{
    let (session_id, lease) = state.leases.get_with_session(lease_id)?;
    let (session, config) = state.sessions.get_with_config(session_id)?;
    let ExecuteRequest {
        mut query,
        params,
        confirm,
        record_history,
    } = request;
    let target = format!("lease:{lease_id}");
    let lease_db = lease.state().database;
    let confirm_text = guard::confirm_text_for(&config, lease_db.as_deref());
    let (read_only, production) = guard::policy_flags(&config);
    // The lease splits text with its own dialect (per-connection `sql_mode`): the guard must
    // see the same statements.
    let dialect = lease
        .script_dialect()
        .unwrap_or_else(|| session.script_dialect());
    let lease_state = lease.state();
    let context = format!(
        "{}/{}",
        lease_state.database.as_deref().unwrap_or_default(),
        lease_state.schema.as_deref().unwrap_or_default()
    );
    let guarded = guard::run_guard(
        &state.confirm,
        &GuardInput {
            dialect,
            text: &query.text,
            params: &params,
            read_only,
            production,
            confirm_text: &confirm_text,
            confirm: confirm.as_ref(),
            target_id: &target,
            context: &context,
        },
    )?;
    let tracker = if record_history {
        Tracker::start(
            state,
            &config,
            lease_db,
            dialect,
            &query.text,
            &guarded.analysis,
        )
        .await
    } else {
        None
    };
    let map = guarded.bound.map;
    query.text = guarded.bound.text;
    apply_editor_limits(state, &mut query).await;
    let counter = RowCounter::default();
    let seen = counter.clone();
    let mut emit = emit;
    let mut sink = EventSink::new(mapped_emit(map.clone(), move |event| {
        seen.observe(&event);
        emit(event)
    }));
    let result = lease
        .execute(query, &mut sink)
        .await
        .map_err(|e| map_error(&map, e));
    drop(sink);
    let history_id = tracker.as_ref().map(Tracker::id);
    if let Some(tracker) = tracker {
        tracker.finish(&result, counter.rows()).await;
    }
    // Whatever happened (a failed transaction, a lost connection), the tab learns the new state.
    state.leases.notify_state(lease_id);
    Ok(ExecuteResult {
        summary: result?,
        state: state.leases.state_of(lease_id).ok(),
        history_id,
    })
}

pub(crate) async fn lease_fetch_more_impl<F>(
    state: &AppState,
    lease_id: Uuid,
    query_id: Uuid,
    max_rows: u64,
    batch_size: Option<u32>,
    emit: F,
) -> Result<ExecuteResult>
where
    F: FnMut(QueryEvent) -> bool + Send,
{
    let lease = state.leases.get(lease_id)?;
    let mut sink = EventSink::new(emit);
    let result = lease
        .fetch_more(query_id, max_rows, batch_size, &mut sink)
        .await;
    // The pending result changed (or the connection broke): tell the tab.
    state.leases.notify_state(lease_id);
    let summary = result?;
    Ok(ExecuteResult {
        summary,
        state: state.leases.state_of(lease_id).ok(),
        history_id: None,
    })
}

/// A failed lease command may have changed the lease (it broke, the transaction failed).
fn notify_on_error<T>(state: &AppState, lease_id: Uuid, result: &Result<T>) {
    if result.is_err() {
        state.leases.notify_state(lease_id);
    }
}

pub(crate) async fn lease_discard_result_impl(
    state: &AppState,
    lease_id: Uuid,
) -> Result<LeaseState> {
    let lease = state.leases.get(lease_id)?;
    let result = lease.discard_result().await;
    notify_on_error(state, lease_id, &result);
    result?;
    state.leases.state_of(lease_id)
}

pub(crate) async fn lease_cancel_impl(
    state: &AppState,
    lease_id: Uuid,
    query_id: Uuid,
) -> Result<()> {
    state.leases.get(lease_id)?.cancel(query_id).await
}

pub(crate) async fn lease_begin_impl(
    state: &AppState,
    lease_id: Uuid,
    options: TxOptions,
) -> Result<LeaseState> {
    let (session_id, lease) = state.leases.get_with_session(lease_id)?;
    // A read-only connection never gets a read-write transaction.
    let mut options = options;
    // (Servers without read-only transactions, e.g. TiDB, rely on the statement guard: the
    // profile's read-only flag then does not turn into a read-only transaction.)
    let (session, config) = state.sessions.get_with_config(session_id)?;
    let supports_ro_tx = session.capabilities().is_none_or(|c| c.read_only_session);
    if config.read_only && supports_ro_tx {
        options.read_only = true;
    }
    let result = lease.begin(options).await;
    notify_on_error(state, lease_id, &result);
    result?;
    state.leases.state_of(lease_id)
}

pub(crate) async fn lease_commit_impl(state: &AppState, lease_id: Uuid) -> Result<LeaseState> {
    let lease = state.leases.get(lease_id)?;
    let result = lease.commit().await;
    notify_on_error(state, lease_id, &result);
    result?;
    state.leases.state_of(lease_id)
}

pub(crate) async fn lease_rollback_impl(state: &AppState, lease_id: Uuid) -> Result<LeaseState> {
    let lease = state.leases.get(lease_id)?;
    let result = lease.rollback().await;
    notify_on_error(state, lease_id, &result);
    result?;
    state.leases.state_of(lease_id)
}

pub(crate) async fn lease_set_auto_commit_impl(
    state: &AppState,
    lease_id: Uuid,
    auto_commit: bool,
) -> Result<LeaseState> {
    let lease = state.leases.get(lease_id)?;
    let result = lease.set_auto_commit(auto_commit).await;
    notify_on_error(state, lease_id, &result);
    result?;
    state.leases.state_of(lease_id)
}

pub(crate) async fn lease_set_context_impl(
    state: &AppState,
    lease_id: Uuid,
    context: LeaseContext,
) -> Result<LeaseState> {
    let lease = state.leases.get(lease_id)?;
    let result = lease.set_context(context).await;
    notify_on_error(state, lease_id, &result);
    result?;
    state.leases.state_of(lease_id)
}

pub(crate) fn lease_state_impl(state: &AppState, lease_id: Uuid) -> Result<LeaseState> {
    state.leases.state_of(lease_id)
}

/// Decode file bytes as UTF-8, dropping a BOM.
fn decode_sql(bytes: Vec<u8>) -> Result<String> {
    let text = String::from_utf8(bytes)
        .map_err(|_| Error::Invalid("the file is not valid UTF-8 text".into()))?;
    Ok(text
        .strip_prefix('\u{feff}')
        .map(str::to_string)
        .unwrap_or(text))
}

/// Native Open dialog, then read the file (<= 50 MB). The path is granted for reading and for
/// saving back.
pub(crate) async fn sql_file_open_impl(state: &AppState) -> Result<Option<SqlFile>> {
    let host = state.host.clone();
    let grants = state.grants.clone();
    blocking(move || {
        let Some(picked) = host.pick_open_path(&sql_filters(), false)? else {
            return Ok(None);
        };
        let path = grants.grant(&picked, Purpose::Read)?;
        grants.grant(&path, Purpose::Write)?;
        let len = std::fs::metadata(&path)?.len();
        if len > MAX_SQL_FILE_BYTES {
            return Err(Error::Invalid(format!(
                "the file is larger than {} MB",
                MAX_SQL_FILE_BYTES >> 20
            )));
        }
        let text = decode_sql(std::fs::read(&path)?)?;
        Ok(Some(SqlFile {
            path: path.to_string_lossy().into_owned(),
            text,
        }))
    })
    .await
}

/// Save to `path` (must be granted) or ask for a path; `None` when the dialog was cancelled.
pub(crate) async fn sql_file_save_impl(
    state: &AppState,
    path: Option<String>,
    text: String,
) -> Result<Option<String>> {
    let host = state.host.clone();
    let grants = state.grants.clone();
    blocking(move || {
        let target = match path {
            Some(p) => grants.check(Path::new(&p), Purpose::Write)?,
            None => {
                let Some(picked) = host.pick_save_path_filtered("query.sql", &sql_filters())?
                else {
                    return Ok(None);
                };
                let canon = grants.grant(&picked, Purpose::Write)?;
                grants.grant(&canon, Purpose::Read)?;
                canon
            }
        };
        write_atomic(&target, text.as_bytes())?;
        Ok(Some(target.to_string_lossy().into_owned()))
    })
    .await
}

#[tauri::command]
pub fn script_analyze(
    state: State<'_, AppState>,
    session_id: Uuid,
    text: String,
) -> Result<ScriptAnalysis> {
    tracing::debug!(command = "script_analyze", session = %session_id, "ipc call");
    analyze_impl(&state, session_id, &text)
}

#[tauri::command]
pub fn script_vocabulary(state: State<'_, AppState>, session_id: Uuid) -> Result<Vocabulary> {
    tracing::debug!(command = "script_vocabulary", session = %session_id, "ipc call");
    vocabulary_impl(&state, session_id)
}

/// Run a script on a pooled connection; streams events through `on_event`.
#[tauri::command]
pub async fn query_execute(
    state: State<'_, AppState>,
    session_id: Uuid,
    request: ExecuteRequest,
    on_event: Channel<QueryEvent>,
) -> Result<ExecuteResult> {
    tracing::debug!(command = "query_execute", session = %session_id, query = %request.query.query_id, "ipc call");
    execute_impl(&state, session_id, request, move |event| {
        on_event.send(event).is_ok()
    })
    .await
}

#[tauri::command]
pub async fn lease_open(
    state: State<'_, AppState>,
    session_id: Uuid,
    options: LeaseOptions,
) -> Result<LeaseInfo> {
    tracing::debug!(command = "lease_open", session = %session_id, "ipc call");
    lease_open_impl(&state, session_id, options).await
}

/// Reopen a broken lease on the reconnected session.
#[tauri::command]
pub async fn lease_reopen(state: State<'_, AppState>, lease_id: Uuid) -> Result<LeaseReopened> {
    tracing::debug!(command = "lease_reopen", lease = %lease_id, "ipc call");
    lease_reopen_impl(&state, lease_id).await
}

/// Generate a script template for a table.
#[tauri::command]
pub async fn script_generate(
    state: State<'_, AppState>,
    session_id: Uuid,
    path: ObjectPath,
    template: ScriptTemplate,
) -> Result<ScriptGenerated> {
    tracing::debug!(command = "script_generate", session = %session_id, "ipc call");
    script_generate_impl(&state, session_id, path, template).await
}

#[tauri::command]
pub async fn lease_close(state: State<'_, AppState>, lease_id: Uuid) -> Result<LeaseCloseReport> {
    tracing::debug!(command = "lease_close", lease = %lease_id, "ipc call");
    lease_close_impl(&state, lease_id).await
}

#[tauri::command]
pub async fn lease_execute(
    state: State<'_, AppState>,
    lease_id: Uuid,
    request: ExecuteRequest,
    on_event: Channel<QueryEvent>,
) -> Result<ExecuteResult> {
    tracing::debug!(command = "lease_execute", lease = %lease_id, query = %request.query.query_id, "ipc call");
    lease_execute_impl(&state, lease_id, request, move |event| {
        on_event.send(event).is_ok()
    })
    .await
}

#[tauri::command]
pub async fn lease_fetch_more(
    state: State<'_, AppState>,
    lease_id: Uuid,
    query_id: Uuid,
    max_rows: u64,
    batch_size: Option<u32>,
    on_event: Channel<QueryEvent>,
) -> Result<ExecuteResult> {
    tracing::debug!(command = "lease_fetch_more", lease = %lease_id, query = %query_id, "ipc call");
    lease_fetch_more_impl(
        &state,
        lease_id,
        query_id,
        max_rows,
        batch_size,
        move |event| on_event.send(event).is_ok(),
    )
    .await
}

#[tauri::command]
pub async fn lease_discard_result(
    state: State<'_, AppState>,
    lease_id: Uuid,
) -> Result<LeaseState> {
    tracing::debug!(command = "lease_discard_result", lease = %lease_id, "ipc call");
    lease_discard_result_impl(&state, lease_id).await
}

#[tauri::command]
pub async fn lease_cancel(
    state: State<'_, AppState>,
    lease_id: Uuid,
    query_id: Uuid,
) -> Result<()> {
    tracing::debug!(command = "lease_cancel", lease = %lease_id, query = %query_id, "ipc call");
    lease_cancel_impl(&state, lease_id, query_id).await
}

#[tauri::command]
pub async fn lease_begin(
    state: State<'_, AppState>,
    lease_id: Uuid,
    options: TxOptions,
) -> Result<LeaseState> {
    tracing::debug!(command = "lease_begin", lease = %lease_id, "ipc call");
    lease_begin_impl(&state, lease_id, options).await
}

#[tauri::command]
pub async fn lease_commit(state: State<'_, AppState>, lease_id: Uuid) -> Result<LeaseState> {
    tracing::debug!(command = "lease_commit", lease = %lease_id, "ipc call");
    lease_commit_impl(&state, lease_id).await
}

#[tauri::command]
pub async fn lease_rollback(state: State<'_, AppState>, lease_id: Uuid) -> Result<LeaseState> {
    tracing::debug!(command = "lease_rollback", lease = %lease_id, "ipc call");
    lease_rollback_impl(&state, lease_id).await
}

#[tauri::command]
pub async fn lease_set_auto_commit(
    state: State<'_, AppState>,
    lease_id: Uuid,
    auto_commit: bool,
) -> Result<LeaseState> {
    tracing::debug!(command = "lease_set_auto_commit", lease = %lease_id, "ipc call");
    lease_set_auto_commit_impl(&state, lease_id, auto_commit).await
}

#[tauri::command]
pub async fn lease_set_context(
    state: State<'_, AppState>,
    lease_id: Uuid,
    context: LeaseContext,
) -> Result<LeaseState> {
    tracing::debug!(command = "lease_set_context", lease = %lease_id, "ipc call");
    lease_set_context_impl(&state, lease_id, context).await
}

#[tauri::command]
pub fn lease_state(state: State<'_, AppState>, lease_id: Uuid) -> Result<LeaseState> {
    tracing::debug!(command = "lease_state", lease = %lease_id, "ipc call");
    lease_state_impl(&state, lease_id)
}

/// Native Open dialog for a SQL file; `None` when cancelled.
#[tauri::command]
pub async fn sql_file_open(state: State<'_, AppState>) -> Result<Option<SqlFile>> {
    tracing::debug!(command = "sql_file_open", "ipc call");
    sql_file_open_impl(&state).await
}

/// Save a SQL file to a granted `path`, or ask for one when omitted.
#[tauri::command]
pub async fn sql_file_save(
    state: State<'_, AppState>,
    path: Option<String>,
    text: String,
) -> Result<Option<String>> {
    tracing::debug!(command = "sql_file_save", "ipc call");
    sql_file_save_impl(&state, path, text).await
}

#[cfg(test)]
mod tests {
    //! Command-level tests of the editor backend: the guard (read-only, production
    //! confirmation), history recording and, against real servers (`DBSTUDIO_IT=1`), leases.
    //!
    //! The unit tests use a keyword classifier (`guard::test_support`) because the real
    //! `dbstudio_sql::analyze` belongs to package A; tests marked "real classifier" run only
    //! once it exists.

    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use dbstudio_core::{
        ConfirmationGrant, ConnectionConfig, DbKind, DriverRegistry, Environment, Error,
        ExecuteRequest, QueryEvent, QueryRequest, Secret,
    };
    use dbstudio_store::{ConnectionProfile, HistoryQuery, SecretChange};
    use serde_json::json;
    use uuid::Uuid;

    use super::*;
    use crate::commands::{connections, sessions};
    use crate::guard::ConfirmTokens;
    use crate::guard::test_support::{
        TestClock, real_classifier_available, use_keyword_classifier,
    };
    use crate::session::testing::{MockDriver, Probe};
    use crate::state::testing::test_state;
    use crate::state::{AppState, build_registry};

    fn request(text: &str) -> ExecuteRequest {
        serde_json::from_value(json!({"text": text, "queryId": Uuid::new_v4()})).unwrap()
    }

    fn with_confirm(text: &str, token: &str, typed: &str) -> ExecuteRequest {
        let mut r = request(text);
        r.confirm = Some(ConfirmationGrant {
            token: token.into(),
            typed: typed.into(),
        });
        r
    }

    fn profile(
        kind: DbKind,
        read_only: bool,
        environment: Option<Environment>,
        database: &str,
    ) -> ConnectionProfile {
        let mut config = ConnectionConfig::new(kind, "mock", "localhost");
        config.read_only = read_only;
        config.environment = environment;
        config.database = Some(database.into());
        ConnectionProfile {
            config,
            group_id: None,
            color: None,
            save_password: false,
            has_password: false,
            sort_order: 0,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    struct Mock {
        state: AppState,
        probe: Arc<Probe>,
    }

    fn mock() -> Mock {
        let (driver, probe) = MockDriver::new(DbKind::Postgres);
        let mut registry = DriverRegistry::new();
        registry.register(driver);
        let (state, _) = test_state(registry);
        Mock { state, probe }
    }

    async fn open(state: &AppState, p: ConnectionProfile) -> Uuid {
        let saved = connections::save_impl(state, p, SecretChange::Keep)
            .await
            .unwrap();
        sessions::open_impl(state, saved.config.id, None)
            .await
            .unwrap()
            .session_id
    }

    async fn run(state: &AppState, sid: Uuid, req: ExecuteRequest) -> Result<ExecuteResult> {
        execute_impl(state, sid, req, |_| true).await
    }

    fn confirmation(err: Error) -> dbstudio_core::ConfirmationRequest {
        match err {
            Error::ConfirmationRequired(c) => *c,
            other => panic!("expected confirmationRequired, got {other:?}"),
        }
    }

    fn history(state: &AppState) -> Vec<dbstudio_store::HistoryEntry> {
        state
            .connections
            .store()
            .history_list(&HistoryQuery {
                connection_id: None,
                search: None,
                status: None,
                before_id: None,
                limit: 500,
            })
            .unwrap()
    }

    #[tokio::test]
    async fn read_only_blocks_insert_update_delete_and_ddl_before_the_driver() {
        let _classifier = use_keyword_classifier();
        let m = mock();
        let sid = open(&m.state, profile(DbKind::Postgres, true, None, "app")).await;
        for (text, expected) in [
            ("INSERT INTO t VALUES (1)", 0),
            ("UPDATE t SET a = 1 WHERE id = 1", 0),
            ("DELETE FROM t WHERE id = 1", 0),
            ("CREATE TABLE x (a int)", 0),
            ("DROP TABLE t", 0),
            ("ALTER TABLE t ADD b int", 0),
            ("TRUNCATE t", 0),
            // The second statement is the offender: the position points at it.
            ("SELECT 1; DELETE FROM t WHERE id = 1", 10),
        ] {
            let err = run(&m.state, sid, request(text)).await.unwrap_err();
            match err {
                Error::ReadOnlyViolation { position, .. } => {
                    assert_eq!(position, Some(expected), "{text}");
                }
                other => panic!("{text}: {other:?}"),
            }
        }
        assert!(m.probe.executed.lock().unwrap().is_empty());
        // Reads, transaction control and session statements go through.
        for ok in ["SELECT 1", "SHOW x", "BEGIN", "SET a = 1"] {
            run(&m.state, sid, request(ok)).await.unwrap();
        }
        assert_eq!(m.probe.executed.lock().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn read_only_position_is_where_the_offending_statement_starts() {
        let _classifier = use_keyword_classifier();
        let m = mock();
        let sid = open(&m.state, profile(DbKind::Postgres, true, None, "app")).await;
        let err = run(
            &m.state,
            sid,
            request("SELECT 1;\n  DELETE FROM t WHERE id = 1"),
        )
        .await
        .unwrap_err();
        assert!(matches!(
            err,
            Error::ReadOnlyViolation {
                position: Some(12),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn production_destructive_statements_need_a_single_use_token() {
        let _classifier = use_keyword_classifier();
        let m = mock();
        let sid = open(
            &m.state,
            profile(
                DbKind::Postgres,
                false,
                Some(Environment::Production),
                "appdb",
            ),
        )
        .await;
        for text in [
            "DELETE FROM t",
            "DROP TABLE t",
            "TRUNCATE t",
            "UPDATE t SET a = 1",
        ] {
            let c = confirmation(run(&m.state, sid, request(text)).await.unwrap_err());
            assert_eq!(c.token.len(), 32, "{text}");
            assert_eq!(c.confirm_text, "appdb");
            assert_eq!(c.statements.len(), 1);
            assert_eq!(c.statements[0].text, text);
            assert!(c.expires_in_ms > 0);
        }
        assert!(m.probe.executed.lock().unwrap().is_empty());
        // Harmless statements and targeted deletes run without any confirmation.
        run(&m.state, sid, request("DELETE FROM t WHERE id = 1"))
            .await
            .unwrap();

        // Wrong typed name: refused, and the token is burnt (a new one is issued).
        let c = confirmation(
            run(&m.state, sid, request("DROP TABLE t"))
                .await
                .unwrap_err(),
        );
        let again = confirmation(
            run(
                &m.state,
                sid,
                with_confirm("DROP TABLE t", &c.token, "wrongdb"),
            )
            .await
            .unwrap_err(),
        );
        assert_ne!(again.token, c.token);
        // Case matters.
        let again = confirmation(
            run(
                &m.state,
                sid,
                with_confirm("DROP TABLE t", &again.token, "APPDB"),
            )
            .await
            .unwrap_err(),
        );
        // A token that never existed.
        confirmation(
            run(
                &m.state,
                sid,
                with_confirm("DROP TABLE t", "0123456789abcdef0123456789abcdef", "appdb"),
            )
            .await
            .unwrap_err(),
        );
        // A token issued for other text does not unlock this one.
        let for_other = confirmation(run(&m.state, sid, request("TRUNCATE t")).await.unwrap_err());
        confirmation(
            run(
                &m.state,
                sid,
                with_confirm("DROP TABLE t", &for_other.token, "appdb"),
            )
            .await
            .unwrap_err(),
        );
        // A token issued for another session does not unlock this one either.
        let other_sid = open(
            &m.state,
            profile(
                DbKind::Postgres,
                false,
                Some(Environment::Production),
                "appdb",
            ),
        )
        .await;
        let foreign = confirmation(
            run(&m.state, other_sid, request("DROP TABLE t"))
                .await
                .unwrap_err(),
        );
        confirmation(
            run(
                &m.state,
                sid,
                with_confirm("DROP TABLE t", &foreign.token, "appdb"),
            )
            .await
            .unwrap_err(),
        );
        assert_eq!(m.probe.executed.lock().unwrap().len(), 1);

        // The right token with the exact name runs, once.
        let good = confirmation(
            run(&m.state, sid, request("DROP TABLE t"))
                .await
                .unwrap_err(),
        );
        run(
            &m.state,
            sid,
            with_confirm("DROP TABLE t", &good.token, "appdb"),
        )
        .await
        .unwrap();
        assert_eq!(
            m.probe.executed.lock().unwrap().last().map(String::as_str),
            Some("DROP TABLE t")
        );
        let reuse = run(
            &m.state,
            sid,
            with_confirm("DROP TABLE t", &good.token, "appdb"),
        )
        .await
        .unwrap_err();
        confirmation(reuse);
        let _ = again;
    }

    #[tokio::test]
    async fn an_expired_confirmation_is_refused() {
        let _classifier = use_keyword_classifier();
        let mut m = mock();
        let clock = TestClock::new();
        m.state.confirm = Arc::new(ConfirmTokens::with_clock(
            crate::guard::TOKEN_TTL,
            clock.clone(),
        ));
        let sid = open(
            &m.state,
            profile(
                DbKind::Postgres,
                false,
                Some(Environment::Production),
                "appdb",
            ),
        )
        .await;
        let c = confirmation(
            run(&m.state, sid, request("DROP TABLE t"))
                .await
                .unwrap_err(),
        );
        clock.advance(Duration::from_secs(119));
        // Still valid a second before the deadline: prove it with a second token.
        let c2 = confirmation(
            run(&m.state, sid, request("DROP TABLE t"))
                .await
                .unwrap_err(),
        );
        clock.advance(Duration::from_secs(1));
        // `c` is now expired; `c2` (issued later) is not.
        confirmation(
            run(
                &m.state,
                sid,
                with_confirm("DROP TABLE t", &c.token, "appdb"),
            )
            .await
            .unwrap_err(),
        );
        run(
            &m.state,
            sid,
            with_confirm("DROP TABLE t", &c2.token, "appdb"),
        )
        .await
        .unwrap();
        // Expiry of an unused token.
        let c3 = confirmation(
            run(&m.state, sid, request("DROP TABLE t"))
                .await
                .unwrap_err(),
        );
        clock.advance(Duration::from_secs(121));
        confirmation(
            run(
                &m.state,
                sid,
                with_confirm("DROP TABLE t", &c3.token, "appdb"),
            )
            .await
            .unwrap_err(),
        );
    }

    #[tokio::test]
    async fn the_confirmation_is_in_terms_of_the_typed_text_not_the_bound_text() {
        let _classifier = use_keyword_classifier();
        let m = mock();
        let sid = open(
            &m.state,
            profile(
                DbKind::Postgres,
                false,
                Some(Environment::Production),
                "appdb",
            ),
        )
        .await;
        // Statements are shown with ranges in the typed text.
        let c = confirmation(
            run(&m.state, sid, request("SELECT 1;  DELETE FROM t"))
                .await
                .unwrap_err(),
        );
        assert_eq!(c.statements[0].range.start, 11);
        assert_eq!(c.statements[0].index, 1);
    }

    #[tokio::test]
    async fn read_only_wins_over_production_confirmation() {
        let _classifier = use_keyword_classifier();
        let m = mock();
        let sid = open(
            &m.state,
            profile(
                DbKind::Postgres,
                true,
                Some(Environment::Production),
                "appdb",
            ),
        )
        .await;
        let err = run(&m.state, sid, request("DROP TABLE t"))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), "readOnly");
    }

    #[tokio::test]
    async fn read_only_cannot_be_switched_off_by_a_statement_the_classifier_allows() {
        let _classifier = use_keyword_classifier();
        let m = mock();
        let sid = open(&m.state, profile(DbKind::Postgres, true, None, "app")).await;
        for text in [
            "SET default_transaction_read_only = off",
            "SET SESSION default_transaction_read_only TO 'off'",
            "SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE",
            "BEGIN READ WRITE",
            "BEGIN; SET TRANSACTION READ WRITE",
            "SELECT set_config('default_transaction_read_only', 'off', false)",
            "SET SESSION TRANSACTION READ WRITE",
            "START TRANSACTION READ WRITE",
            "SET transaction_read_only = 0",
            "SET SESSION tx_read_only = 0",
            "SET sql_mode = ''",
            "select 1; set  DEFAULT_TRANSACTION_READ_ONLY=off",
        ] {
            let err = run(&m.state, sid, request(text)).await.unwrap_err();
            assert_eq!(err.kind(), "readOnly", "{text}: {err:?}");
        }
        assert!(m.probe.executed.lock().unwrap().is_empty());
        // Reading the setting, a read-only transaction and quoted/commented words are fine.
        for ok in [
            "SHOW transaction_read_only",
            "SELECT 'READ WRITE'",
            "BEGIN READ ONLY",
            "SELECT 1 -- READ WRITE",
            "SELECT /* set_config */ 1",
            "SET search_path = public",
        ] {
            run(&m.state, sid, request(ok))
                .await
                .unwrap_or_else(|e| panic!("{ok}: {e:?}"));
        }
        // The position points at the offending statement in the typed text.
        let err = run(&m.state, sid, request("SELECT 1;\n  BEGIN READ WRITE"))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            Error::ReadOnlyViolation {
                position: Some(12),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn production_fallback_asks_when_the_classifier_cannot_tell() {
        // No classifier override: `dbstudio_sql::analyze` may not classify anything yet.
        let m = mock();
        let sid = open(
            &m.state,
            profile(
                DbKind::Postgres,
                false,
                Some(Environment::Production),
                "appdb",
            ),
        )
        .await;
        for text in [
            "DROP TABLE t",
            "TRUNCATE t",
            "DELETE FROM t",
            "ALTER TABLE t ADD b int",
            "UPDATE t SET a = 1",
            "SELECT 1; DROP TABLE t",
            "DO $$ BEGIN DELETE FROM t; END $$",
        ] {
            let c = confirmation(run(&m.state, sid, request(text)).await.unwrap_err());
            assert_eq!(c.confirm_text, "appdb", "{text}");
            assert!(!c.statements.is_empty(), "{text}");
        }
        assert!(m.probe.executed.lock().unwrap().is_empty());
        // Ordinary statements do not ask (strings and comments: see the guard unit tests).
        for ok in ["SELECT 1", "UPDATE t SET a = 1 WHERE id = 1"] {
            run(&m.state, sid, request(ok))
                .await
                .unwrap_or_else(|e| panic!("{ok}: {e:?}"));
        }
        // The confirmation then lets it through.
        let c = confirmation(run(&m.state, sid, request("TRUNCATE t")).await.unwrap_err());
        run(&m.state, sid, with_confirm("TRUNCATE t", &c.token, "appdb"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_confirmation_is_bound_to_the_context_it_was_issued_for() {
        let _classifier = use_keyword_classifier();
        let m = mock();
        let sid = open(
            &m.state,
            profile(
                DbKind::Postgres,
                false,
                Some(Environment::Production),
                "appdb",
            ),
        )
        .await;
        let in_db = |db: &str, mut r: ExecuteRequest| {
            r.query.database = Some(db.into());
            r
        };
        let c = confirmation(
            run(&m.state, sid, in_db("one", request("DROP TABLE t")))
                .await
                .unwrap_err(),
        );
        // The same token and text, but another database: refused.
        confirmation(
            run(
                &m.state,
                sid,
                in_db("two", with_confirm("DROP TABLE t", &c.token, "appdb")),
            )
            .await
            .unwrap_err(),
        );
        // In the right context it works.
        let c = confirmation(
            run(&m.state, sid, in_db("one", request("DROP TABLE t")))
                .await
                .unwrap_err(),
        );
        run(
            &m.state,
            sid,
            in_db("one", with_confirm("DROP TABLE t", &c.token, "appdb")),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn real_classifier_enforces_the_same_rules() {
        if !real_classifier_available() {
            return; // package A's classifier is not merged yet
        }
        let m = mock();
        let ro = open(&m.state, profile(DbKind::Postgres, true, None, "app")).await;
        let err = run(&m.state, ro, request("INSERT INTO t VALUES (1)"))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), "readOnly");
        let prod = open(
            &m.state,
            profile(
                DbKind::Postgres,
                false,
                Some(Environment::Production),
                "appdb",
            ),
        )
        .await;
        confirmation(
            run(&m.state, prod, request("DELETE FROM t"))
                .await
                .unwrap_err(),
        );
        confirmation(
            run(&m.state, prod, request("DROP TABLE t"))
                .await
                .unwrap_err(),
        );
    }

    #[tokio::test]
    async fn query_execute_records_history() {
        let m = mock();
        let sid = open(&m.state, profile(DbKind::Postgres, false, None, "app")).await;
        let result = run(&m.state, sid, request("select 1")).await.unwrap();
        let id = result.history_id.expect("history id");
        let entries = history(&m.state);
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.id, id);
        assert_eq!((e.status.as_str(), e.kind.as_str()), ("ok", "query"));
        assert_eq!(e.text, "select 1");
        assert_eq!(e.database.as_deref(), Some("app"));
        assert_eq!((e.statements, e.rows), (Some(1), Some(1)));
        assert!(e.elapsed_ms.is_some() && e.error_message.is_none());

        // A failing query is recorded as an error with its message.
        let err = run(&m.state, sid, request("fail")).await.unwrap_err();
        assert_eq!(err.kind(), "query");
        let e = &history(&m.state)[0];
        assert_eq!(e.status, "error");
        assert_eq!(e.error_message.as_deref(), Some("boom"));

        // Opting out records nothing.
        let mut quiet = request("select 2");
        quiet.record_history = false;
        let result = run(&m.state, sid, quiet).await.unwrap();
        assert_eq!(result.history_id, None);
        assert_eq!(history(&m.state).len(), 2);

        // A receiver that went away is a cancelled query.
        let _ = execute_impl(&m.state, sid, request("select 3"), |_| false)
            .await
            .unwrap_err();
        assert_eq!(history(&m.state)[0].status, "cancelled");
    }

    #[tokio::test]
    async fn refused_statements_leave_no_history() {
        let _classifier = use_keyword_classifier();
        let m = mock();
        let sid = open(&m.state, profile(DbKind::Postgres, true, None, "app")).await;
        run(&m.state, sid, request("DELETE FROM t WHERE a = 1"))
            .await
            .unwrap_err();
        assert!(history(&m.state).is_empty());
    }

    #[tokio::test]
    async fn lease_reopen_keeps_the_id_and_reports_a_lost_transaction() {
        let m = mock();
        m.probe
            .rich
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let sid = open(&m.state, profile(DbKind::Postgres, false, None, "app")).await;
        let idle = lease_open_impl(
            &m.state,
            sid,
            LeaseOptions {
                database: Some("app".into()),
                schema: Some("s1".into()),
                auto_commit: false,
            },
        )
        .await
        .unwrap();
        let in_tx = lease_open_impl(&m.state, sid, LeaseOptions::default())
            .await
            .unwrap();
        m.state
            .leases
            .get(in_tx.lease_id)
            .unwrap()
            .begin(TxOptions::default())
            .await
            .unwrap();
        // A healthy lease is returned as it is.
        let same = lease_reopen_impl(&m.state, idle.lease_id).await.unwrap();
        assert_eq!(same.lease.lease_id, idle.lease_id);
        assert!(!same.transaction_lost);
        let opens = m
            .probe
            .lease_opens
            .load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(opens, 2, "nothing was reopened");

        // The connections die; the keepalive notices and marks both leases lost.
        m.probe.kill_all();
        m.state.leases.keepalive_tick(Duration::ZERO).await;
        assert!(m.state.leases.state_of(idle.lease_id).unwrap().broken);

        let back = lease_reopen_impl(&m.state, idle.lease_id).await.unwrap();
        assert_eq!(back.lease.lease_id, idle.lease_id);
        assert_eq!(back.lease.session_id, sid);
        assert!(!back.transaction_lost);
        assert!(!back.lease.state.broken);
        assert_eq!(back.lease.state.schema.as_deref(), Some("s1"));
        assert!(!back.lease.state.auto_commit);

        let back = lease_reopen_impl(&m.state, in_tx.lease_id).await.unwrap();
        assert!(back.transaction_lost, "the open transaction was lost");
        assert_eq!(back.lease.state.transaction, dbstudio_core::TxStatus::Idle);
        // Once reopened the lease is healthy: asking again changes nothing and reports the
        // first outcome.
        let again = lease_reopen_impl(&m.state, in_tx.lease_id).await.unwrap();
        assert!(again.transaction_lost);
        assert_eq!(m.state.leases.count_for_session(sid), 2);
    }

    #[tokio::test]
    async fn concurrent_reopens_of_one_lease_share_one_new_connection() {
        let m = mock();
        m.probe
            .rich
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let sid = open(&m.state, profile(DbKind::Postgres, false, None, "app")).await;
        let lease = lease_open_impl(&m.state, sid, LeaseOptions::default())
            .await
            .unwrap();
        m.state
            .leases
            .get(lease.lease_id)
            .unwrap()
            .begin(TxOptions::default())
            .await
            .unwrap();
        m.probe.kill_all();
        m.state.leases.keepalive_tick(Duration::ZERO).await;
        let before = m
            .probe
            .lease_opens
            .load(std::sync::atomic::Ordering::SeqCst);
        let (a, b, c) = tokio::join!(
            lease_reopen_impl(&m.state, lease.lease_id),
            lease_reopen_impl(&m.state, lease.lease_id),
            lease_reopen_impl(&m.state, lease.lease_id)
        );
        let after = m
            .probe
            .lease_opens
            .load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(after, before + 1, "one new connection, not three");
        for done in [a.unwrap(), b.unwrap(), c.unwrap()] {
            assert!(
                done.transaction_lost,
                "every caller learns the transaction is gone"
            );
            assert!(!done.lease.state.broken);
        }
    }

    #[tokio::test]
    async fn script_generate_reads_the_table_and_uses_the_session_dialect() {
        let m = mock();
        m.probe
            .rich
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let sid = open(&m.state, profile(DbKind::Postgres, false, None, "app")).await;
        let path = ObjectPath::root()
            .child(dbstudio_core::ObjectKind::Database, "app")
            .child(dbstudio_core::ObjectKind::Table, "items");
        for template in [
            ScriptTemplate::Select,
            ScriptTemplate::Insert,
            ScriptTemplate::Update,
            ScriptTemplate::Delete,
        ] {
            let got = script_generate_impl(&m.state, sid, path.clone(), template)
                .await
                .unwrap();
            let session = m.state.sessions.get(sid).unwrap();
            let details = session.table_details(&path).await.unwrap();
            assert_eq!(got.dialect, session.script_dialect());
            assert_eq!(
                got.text,
                dbstudio_sql::template::generate(got.dialect, &details, &path, template)
            );
        }
        // Redis has no tables to template.
        *m.probe.dialect.lock().unwrap() = Some(ScriptDialect::RedisCli);
        let sid = open(&m.state, profile(DbKind::Postgres, false, None, "app")).await;
        let e = script_generate_impl(&m.state, sid, path, ScriptTemplate::Select)
            .await
            .unwrap_err();
        assert_eq!(e.kind(), "unsupported");
    }

    #[tokio::test]
    async fn only_a_password_the_user_typed_stays_in_memory() {
        let m = mock();
        // Stored in the keychain: read again on reconnect, not kept in the session.
        let mut stored = profile(DbKind::Postgres, false, None, "app");
        stored.save_password = true;
        let saved = connections::save_impl(
            &m.state,
            stored,
            SecretChange::Set {
                value: Secret::new("keychain-pw"),
            },
        )
        .await
        .unwrap();
        let info = sessions::open_impl(&m.state, saved.config.id, None)
            .await
            .unwrap();
        assert!(
            m.state
                .sessions
                .remembered_secret(info.session_id)
                .is_none()
        );
        // Typed at the prompt: kept for the life of the session.
        let saved = connections::save_impl(
            &m.state,
            profile(DbKind::Postgres, false, None, "app"),
            SecretChange::Keep,
        )
        .await
        .unwrap();
        let info = sessions::open_impl(&m.state, saved.config.id, Some(Secret::new("typed")))
            .await
            .unwrap();
        assert_eq!(
            m.state
                .sessions
                .remembered_secret(info.session_id)
                .unwrap()
                .expose(),
            "typed"
        );
        // A manual reconnect re-reads the profile and the keychain.
        let again = sessions::reconnect_impl(&m.state, info.session_id, None)
            .await
            .unwrap();
        assert_eq!(again.session_id, info.session_id);
        assert_eq!(
            m.probe.last_secret.lock().unwrap().clone(),
            Some(Some("typed".to_string()))
        );
    }

    #[tokio::test]
    async fn editor_limits_come_from_the_settings_unless_the_request_sets_them() {
        let m = mock();
        let sid = open(&m.state, profile(DbKind::Postgres, false, None, "app")).await;
        // Defaults: 1000 rows, no timeout.
        run(&m.state, sid, request("select 1")).await.unwrap();
        assert_eq!(*m.probe.last_limits.lock().unwrap(), (Some(1_000), None));
        crate::commands::app::settings_set_impl(
            &m.state,
            dbstudio_store::AppSettings {
                editor_max_rows: 500,
                query_timeout_ms: 7_000,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        run(&m.state, sid, request("select 2")).await.unwrap();
        assert_eq!(
            *m.probe.last_limits.lock().unwrap(),
            (Some(500), Some(7_000))
        );
        // What the caller set wins.
        let mut explicit = request("select 3");
        explicit.query.max_rows = Some(5);
        explicit.query.timeout_ms = Some(100);
        run(&m.state, sid, explicit).await.unwrap();
        assert_eq!(*m.probe.last_limits.lock().unwrap(), (Some(5), Some(100)));
    }

    #[tokio::test]
    async fn history_commands_roundtrip_and_survive_reopening_the_store() {
        use crate::commands::history as cmd;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dbstudio.db");
        let connection;
        {
            let secrets = Arc::new(dbstudio_store::MemorySecretStore::new());
            let svc = dbstudio_store::ConnectionService::new(
                dbstudio_store::Store::open(&path).unwrap(),
                secrets.clone(),
            );
            let (driver, _) = MockDriver::new(DbKind::Postgres);
            let mut registry = DriverRegistry::new();
            registry.register(driver);
            let state = AppState::new(
                registry,
                svc,
                secrets,
                Arc::new(crate::state::testing::FakeHost::default()),
            );
            let sid = open(&state, profile(DbKind::Postgres, false, None, "app")).await;
            connection = state.sessions.get_with_config(sid).unwrap().1.id;
            run(&state, sid, request("select 1")).await.unwrap();
            run(&state, sid, request("select 22")).await.unwrap();
        }
        // New process: same file.
        let secrets = Arc::new(dbstudio_store::MemorySecretStore::new());
        let svc = dbstudio_store::ConnectionService::new(
            dbstudio_store::Store::open(&path).unwrap(),
            secrets.clone(),
        );
        let state = AppState::new(
            DriverRegistry::new(),
            svc,
            secrets,
            Arc::new(crate::state::testing::FakeHost::default()),
        );
        let query = |search: Option<&str>| HistoryQuery {
            connection_id: Some(connection),
            search: search.map(str::to_string),
            status: None,
            before_id: None,
            limit: 50,
        };
        let all = cmd::history_list_impl(&state, query(None)).await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].text, "select 22");
        let found = cmd::history_list_impl(&state, query(Some("22")))
            .await
            .unwrap();
        assert_eq!(found.len(), 1);
        cmd::history_delete_impl(&state, vec![all[0].id])
            .await
            .unwrap();
        assert_eq!(
            cmd::history_clear_impl(&state, Some(connection))
                .await
                .unwrap(),
            1
        );
        assert!(
            cmd::history_list_impl(&state, query(None))
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn saved_queries_and_tabs_commands_roundtrip() {
        use crate::commands::history as cmd;
        use dbstudio_store::{EditorTabState, SavedFolder, SavedQuery};
        let (state, _) = test_state(DriverRegistry::new());
        let folder = cmd::saved_folder_save_impl(
            &state,
            SavedFolder {
                id: Uuid::new_v4(),
                name: "Reports".into(),
                parent_id: None,
                sort_order: 0,
            },
        )
        .await
        .unwrap();
        let q = cmd::saved_query_save_impl(
            &state,
            SavedQuery {
                id: Uuid::new_v4(),
                folder_id: Some(folder.id),
                name: "Top users".into(),
                text: "select 1".into(),
                is_snippet: false,
                db_kind: Some(DbKind::Postgres),
                connection_id: None,
                description: None,
                sort_order: 0,
                created_at: String::new(),
                updated_at: String::new(),
            },
        )
        .await
        .unwrap();
        let snap = cmd::saved_queries_list_impl(&state).await.unwrap();
        assert_eq!((snap.folders.len(), snap.queries.len()), (1, 1));
        cmd::saved_folder_delete_impl(&state, folder.id)
            .await
            .unwrap();
        let snap = cmd::saved_queries_list_impl(&state).await.unwrap();
        assert_eq!(snap.queries[0].folder_id, None);
        cmd::saved_query_delete_impl(&state, q.id).await.unwrap();
        assert!(
            cmd::saved_queries_list_impl(&state)
                .await
                .unwrap()
                .queries
                .is_empty()
        );

        let tab = EditorTabState {
            id: Uuid::new_v4(),
            position: 0,
            title: "Query 1".into(),
            connection_id: None,
            database: None,
            schema: None,
            text: "select 1".into(),
            file_path: None,
            cursor: 3,
            auto_commit: true,
            dirty: false,
        };
        cmd::editor_tabs_set_impl(&state, vec![tab.clone()])
            .await
            .unwrap();
        assert_eq!(cmd::editor_tabs_get_impl(&state).await.unwrap(), vec![tab]);
    }

    // ------------------------------------------------------------------------------------
    // Real servers

    fn it_enabled() -> bool {
        std::env::var("DBSTUDIO_IT").as_deref() == Ok("1")
    }

    struct Server {
        kind: DbKind,
        port: u16,
        database: &'static str,
        setup: &'static [&'static str],
    }

    async fn real_session(
        state: &AppState,
        s: &Server,
        read_only: bool,
        environment: Option<Environment>,
    ) -> Uuid {
        let mut p = profile(s.kind, read_only, environment, s.database);
        p.config.port = s.port;
        p.config.host = "127.0.0.1".into();
        p.config.username = Some("dbstudio".into());
        p.config.connect_timeout_ms = Some(5000);
        p.config.tls.mode = dbstudio_core::TlsMode::Disable;
        p.save_password = true;
        let saved = connections::save_impl(
            state,
            p,
            SecretChange::Set {
                value: Secret::new("dbstudio_test"),
            },
        )
        .await
        .unwrap();
        sessions::open_impl(state, saved.config.id, None)
            .await
            .unwrap()
            .session_id
    }

    async fn lease_scenario(s: &Server) {
        let _classifier = use_keyword_classifier();
        let (state, _) = test_state(build_registry());
        let admin = real_session(&state, s, false, None).await;
        let session = state.sessions.get(admin).unwrap();
        for sql in s.setup {
            dbstudio_core::execute_collect(&*session, QueryRequest::new(*sql))
                .await
                .unwrap_or_else(|e| panic!("{sql}: {e}"));
        }
        let opts = |auto_commit| LeaseOptions {
            database: None,
            schema: None,
            auto_commit,
        };

        // Script of three statements through lease_execute, streamed and recorded.
        let lease = lease_open_impl(&state, admin, opts(true)).await.unwrap();
        assert!(lease.state.auto_commit);
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let result = lease_execute_impl(
            &state,
            lease.lease_id,
            request("INSERT INTO lease_t (id) VALUES (1), (2); UPDATE lease_t SET v = 'x'; SELECT count(*) FROM lease_t"),
            move |e| {
                sink.lock().unwrap().push(e);
                true
            },
        )
        .await
        .unwrap();
        assert_eq!(result.summary.statements, 3);
        assert!(result.history_id.is_some());
        let starts = events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| matches!(e, QueryEvent::StatementStart { .. }))
            .count();
        assert_eq!(starts, 3);
        let h = &history(&state)[0];
        assert_eq!((h.kind.as_str(), h.status.as_str()), ("script", "ok"));
        assert_eq!(h.statements, Some(3));

        // Read-only and production guards apply to leases of those connections.
        let ro = real_session(&state, s, true, None).await;
        let ro_lease = lease_open_impl(&state, ro, opts(true)).await.unwrap();
        for text in [
            "INSERT INTO lease_t VALUES (9)",
            "UPDATE lease_t SET v = 'y' WHERE id = 1",
            "DELETE FROM lease_t WHERE id = 1",
            "CREATE TABLE nope (a int)",
        ] {
            let err = lease_execute_impl(&state, ro_lease.lease_id, request(text), |_| true)
                .await
                .unwrap_err();
            assert_eq!(err.kind(), "readOnly", "{text}");
        }
        lease_execute_impl(
            &state,
            ro_lease.lease_id,
            request("SELECT count(*) FROM lease_t"),
            |_| true,
        )
        .await
        .unwrap();
        lease_close_impl(&state, ro_lease.lease_id).await.unwrap();

        let prod = real_session(&state, s, false, Some(Environment::Production)).await;
        let prod_lease = lease_open_impl(&state, prod, opts(true)).await.unwrap();
        let c = confirmation(
            lease_execute_impl(
                &state,
                prod_lease.lease_id,
                request("DELETE FROM lease_t"),
                |_| true,
            )
            .await
            .unwrap_err(),
        );
        assert_eq!(c.confirm_text, "dbstudio");
        let wrong = confirmation(
            lease_execute_impl(
                &state,
                prod_lease.lease_id,
                with_confirm("DELETE FROM lease_t", &c.token, "nope"),
                |_| true,
            )
            .await
            .unwrap_err(),
        );
        let count_rows = |state: AppState, sid: Uuid| async move {
            let session = state.sessions.get(sid).unwrap();
            let r = dbstudio_core::execute_collect(
                &*session,
                QueryRequest::new("SELECT count(*) FROM lease_t"),
            )
            .await
            .unwrap();
            match &r[0].outcome {
                dbstudio_core::StatementOutcome::Rows(rs) => match rs.rows[0][0] {
                    dbstudio_core::Value::Int(n) => n,
                    dbstudio_core::Value::UInt(n) => n as i64,
                    ref other => panic!("{other:?}"),
                },
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(
            count_rows(state.clone(), admin).await,
            2,
            "nothing deleted yet"
        );
        lease_execute_impl(
            &state,
            prod_lease.lease_id,
            with_confirm("DELETE FROM lease_t", &wrong.token, "dbstudio"),
            |_| true,
        )
        .await
        .unwrap();
        assert_eq!(
            count_rows(state.clone(), admin).await,
            0,
            "deleted after confirmation"
        );
        lease_close_impl(&state, prod_lease.lease_id).await.unwrap();

        // Manual transaction through the commands; closing rolls back and says so.
        let manual = lease_open_impl(&state, admin, opts(false)).await.unwrap();
        lease_execute_impl(
            &state,
            manual.lease_id,
            request("INSERT INTO lease_t VALUES (7, 'm')"),
            |_| true,
        )
        .await
        .unwrap();
        let st = lease_state_impl(&state, manual.lease_id).unwrap();
        assert_eq!(st.transaction, dbstudio_core::TxStatus::Active);
        assert_eq!(
            count_rows(state.clone(), admin).await,
            0,
            "uncommitted rows are invisible"
        );
        let st = lease_commit_impl(&state, manual.lease_id).await.unwrap();
        assert_eq!(st.transaction, dbstudio_core::TxStatus::Idle);
        assert_eq!(count_rows(state.clone(), admin).await, 1);
        lease_execute_impl(
            &state,
            manual.lease_id,
            request("DELETE FROM lease_t WHERE id = 7"),
            |_| true,
        )
        .await
        .unwrap();
        let report = lease_close_impl(&state, manual.lease_id).await.unwrap();
        assert!(report.rolled_back);
        assert_eq!(count_rows(state.clone(), admin).await, 1);

        // Closing the session closes its leases.
        let extra = lease_open_impl(&state, admin, opts(true)).await.unwrap();
        sessions::close_impl(&state, admin).await.unwrap();
        assert!(matches!(
            lease_state_impl(&state, extra.lease_id),
            Err(Error::NotFound(_))
        ));
        lease_close_impl(&state, lease.lease_id).await.unwrap();
    }

    #[tokio::test]
    async fn postgres_leases_end_to_end() {
        if !it_enabled() {
            return;
        }
        lease_scenario(&Server {
            kind: DbKind::Postgres,
            port: 54316,
            database: "dbstudio",
            setup: &[
                "DROP TABLE IF EXISTS lease_t",
                "CREATE TABLE lease_t (id int PRIMARY KEY, v text)",
            ],
        })
        .await;
    }

    #[tokio::test]
    async fn mysql_leases_end_to_end() {
        if !it_enabled() {
            return;
        }
        lease_scenario(&Server {
            kind: DbKind::MySql,
            port: 33068,
            database: "dbstudio",
            setup: &[
                "DROP TABLE IF EXISTS lease_t",
                "CREATE TABLE lease_t (id int PRIMARY KEY, v varchar(10)) ENGINE=InnoDB",
            ],
        })
        .await;
    }
}
