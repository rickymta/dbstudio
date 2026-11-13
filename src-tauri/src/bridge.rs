//! Development-only HTTP bridge: lets the UI run in a normal browser (`pnpm dev`) while calling
//! the REAL backend of the running desktop app (real databases, real keychain).
//!
//! Compiled only with the `dev-bridge` cargo feature AND `debug_assertions` AND without
//! Tauri's `custom-protocol` feature (which `tauri build` enables, also for `--debug` bundles):
//! any other combination fails to compile (see `lib.rs`), and `spawn_from_env` additionally
//! checks `tauri::is_dev()` at runtime. Started only when `DBSTUDIO_DEV_BRIDGE_TOKEN` is set and
//! at least 32 characters long; listens on `127.0.0.1` only; every request needs the token
//! header, a loopback `Host` (DNS-rebinding defence) and, when present, an allowed `Origin`.
//!
//! * `POST /ipc/<command>` with a JSON object of arguments (camelCase keys, like Tauri's
//!   `invoke`) answers `200` + result JSON, or `400` + the serialized `Error`.
//! * The commands with a Tauri `Channel` (`table_fetch`, `query_execute`, `lease_execute`,
//!   `lease_fetch_more`) stream NDJSON: one `QueryEvent` per line, then exactly one
//!   `{"type":"result","result":<return value>}` or `{"type":"error","error":..}` line.
//! * `GET /events` streams NDJSON `{"event":"<name>","payload":..}` for every application event
//!   (`job://progress`, `lease://state`, `backup://changed`).

use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use dbstudio_backup::{BackupJob, BackupSpec, NativeTool, RestoreSpec};
use dbstudio_core::{
    ChangeSet, ChildrenPageRequest, ConfirmationGrant, DbKind, DdlRequest, Error, ExecuteRequest,
    ExplainRequest, JobId, KeyReadRequest, LeaseContext, LeaseOptions, ObjectPath,
    ObjectSearchRequest, QueryEvent, RawPlan, Result, SchemaModelRequest, Secret,
    TableCountRequest, TableDataRequest, TxOptions,
};
use dbstudio_export::{ExportSpec, ImportPreviewRequest, ImportSpec};
use dbstudio_plan::PlanRules;
use dbstudio_store::{
    AppSettings, ConnectionGroup, ConnectionProfile, EditorTabState, ErdLayout, ExportPreset,
    HistoryQuery, SavedFolder, SavedQuery, SecretChange,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio_stream::StreamExt;
use uuid::Uuid;

use crate::commands::{
    app, background, backup, browse, changes, connections, editor, erd, export, files, history,
    jobs, keyvalue, plan, sessions, ssh, uri,
};
use crate::host::FileFilter;
use crate::state::AppState;

const TOKEN_HEADER: &str = "x-dbstudio-bridge-token";
const ALLOWED_ORIGINS: [&str; 2] = ["http://localhost:1420", "http://127.0.0.1:1420"];
const DEFAULT_PORT: u16 = 1421;
/// Request body limit. Large `save_cell_value` payloads (over ~12 MB of bytes) are therefore
/// refused through the bridge; the native command has its own, larger limit.
const BODY_LIMIT: usize = 16 * 1024 * 1024;
/// Shortest accepted token.
pub const MIN_TOKEN_LEN: usize = 32;
/// Lines buffered per `table_fetch` stream. A client that falls this far behind cancels the fetch.
const STREAM_BUFFER: usize = 1024;

#[derive(Clone)]
struct Ctx {
    state: AppState,
    token: String,
    port: u16,
}

fn host_allowed(host: Option<&HeaderValue>, port: u16) -> bool {
    let Some(host) = host.and_then(|h| h.to_str().ok()) else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    host == format!("127.0.0.1:{port}") || host == format!("localhost:{port}")
}

/// Compare without early exit so response time does not reveal how much of the token matched.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        diff |= usize::from(a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0));
    }
    diff == 0
}

fn error_response(status: StatusCode, error: &Error) -> Response {
    (
        status,
        axum::Json(serde_json::to_value(error).unwrap_or(Value::Null)),
    )
        .into_response()
}

fn allowed_origin(req: &Request) -> Option<HeaderValue> {
    let origin = req.headers().get(header::ORIGIN)?;
    ALLOWED_ORIGINS
        .iter()
        .any(|o| origin.as_bytes() == o.as_bytes())
        .then(|| origin.clone())
}

fn add_cors(response: &mut Response, origin: Option<HeaderValue>) {
    let Some(origin) = origin else { return };
    let h = response.headers_mut();
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    h.insert(header::VARY, HeaderValue::from_static("origin"));
    h.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("x-dbstudio-bridge-token, content-type"),
    );
    h.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
}

/// CORS and token check for every request.
async fn guard(State(ctx): State<Ctx>, req: Request, next: Next) -> Response {
    // Only loopback host names: a DNS-rebinding page reaches 127.0.0.1 under its own name.
    if !host_allowed(req.headers().get(header::HOST), ctx.port) {
        return error_response(
            StatusCode::FORBIDDEN,
            &Error::Other("host not allowed".into()),
        );
    }
    // A cross-origin browser request is refused outright, not just left without CORS headers.
    // Requests without an Origin header (curl, same-origin tools) are fine.
    let origin = allowed_origin(&req);
    if req.headers().contains_key(header::ORIGIN) && origin.is_none() {
        return error_response(
            StatusCode::FORBIDDEN,
            &Error::Other("origin not allowed".into()),
        );
    }
    if req.method() == Method::OPTIONS {
        // Preflight requests never carry custom headers, so they are answered before the token
        // check; they reveal nothing but the (static) CORS policy.
        let mut resp = StatusCode::NO_CONTENT.into_response();
        add_cors(&mut resp, origin);
        resp.headers_mut().insert(
            header::ACCESS_CONTROL_MAX_AGE,
            HeaderValue::from_static("600"),
        );
        return resp;
    }
    let authorized = req
        .headers()
        .get(TOKEN_HEADER)
        .is_some_and(|v| constant_time_eq(v.as_bytes(), ctx.token.as_bytes()));
    let mut resp = if authorized {
        next.run(req).await
    } else {
        error_response(
            StatusCode::UNAUTHORIZED,
            &Error::Other("missing or invalid bridge token".into()),
        )
    };
    add_cors(&mut resp, origin);
    resp
}

/// Build the bridge router around the shared application state.
pub fn router(state: AppState, token: String, port: u16) -> Router {
    let ctx = Ctx { state, token, port };
    Router::new()
        .route("/ipc/{command}", post(ipc))
        .route("/events", get(events))
        .fallback(|| async {
            error_response(StatusCode::NOT_FOUND, &Error::NotFound("route".into()))
        })
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .layer(middleware::from_fn_with_state(ctx.clone(), guard))
        .with_state(ctx)
}

fn arg<T: DeserializeOwned>(args: &Value, key: &str) -> Result<T> {
    serde_json::from_value(args.get(key).cloned().unwrap_or(Value::Null))
        .map_err(|e| Error::Other(format!("invalid argument `{key}`: {e}")))
}

fn out<T: Serialize>(result: Result<T>) -> Result<Value> {
    result.and_then(|v| {
        serde_json::to_value(v).map_err(|e| Error::Other(format!("cannot serialize result: {e}")))
    })
}

/// Run a non-streaming command. `None` means the command does not exist.
async fn dispatch(state: &AppState, command: &str, a: &Value) -> Option<Result<Value>> {
    macro_rules! call {
        ($e:expr) => {
            Some(async { out($e.await) }.await)
        };
    }
    // Argument errors short-circuit through this helper.
    macro_rules! args {
        ($($name:ident : $ty:ty = $key:literal),*) => {
            $(let $name: $ty = match arg(a, $key) { Ok(v) => v, Err(e) => return Some(Err(e)) };)*
        };
    }
    match command {
        "app_info" => Some(out(Ok(app::app_info_impl(state)))),
        "list_drivers" => Some(out(Ok(app::list_drivers_impl(state)))),
        "log_dir" => Some(out(app::log_dir_impl(state))),
        "open_log_dir" => call!(app::open_log_dir_impl(state)),
        "settings_get" => call!(app::settings_get_impl(state)),
        "settings_set" => {
            args!(settings: AppSettings = "settings");
            call!(app::settings_set_impl(state, settings))
        }
        "save_cell_value" => {
            args!(name: String = "suggestedName", data: String = "dataBase64");
            call!(app::save_cell_value_impl(state, name, data))
        }
        "connections_list" => call!(connections::list_impl(state)),
        "connection_save" => {
            args!(
                profile: ConnectionProfile = "profile",
                secret: SecretChange = "secret",
                ssh_secret: Option<SecretChange> = "sshSecret"
            );
            call!(connections::save_with_ssh_impl(
                state, profile, secret, ssh_secret
            ))
        }
        "connection_delete" => {
            args!(id: Uuid = "id");
            call!(connections::delete_impl(state, id))
        }
        "connection_duplicate" => {
            args!(id: Uuid = "id");
            call!(connections::duplicate_impl(state, id))
        }
        "group_save" => {
            args!(group: ConnectionGroup = "group");
            call!(connections::group_save_impl(state, group))
        }
        "group_delete" => {
            args!(id: Uuid = "id");
            call!(connections::group_delete_impl(state, id))
        }
        "connection_test" => {
            args!(profile: ConnectionProfile = "profile", secret: SecretChange = "secret");
            call!(connections::test_impl(state, profile, secret))
        }
        "connections_export" => {
            args!(path: String = "path", ids: Option<Vec<Uuid>> = "ids");
            call!(connections::export_impl(state, path, ids))
        }
        "connections_import" => {
            args!(path: String = "path");
            call!(connections::import_impl(state, path))
        }
        "session_open" => {
            args!(id: Uuid = "connectionId", password: Option<Secret> = "password");
            call!(sessions::open_impl(state, id, password))
        }
        "session_close" => {
            args!(id: Uuid = "sessionId");
            call!(sessions::close_impl(state, id))
        }
        "object_children" => {
            args!(id: Uuid = "sessionId", path: dbstudio_core::ObjectPath = "path");
            call!(sessions::children_impl(state, id, path))
        }
        "object_ddl" => {
            args!(id: Uuid = "sessionId", path: dbstudio_core::ObjectPath = "path");
            call!(sessions::ddl_impl(state, id, path))
        }
        "object_search" => {
            args!(id: Uuid = "sessionId", request: ObjectSearchRequest = "request");
            call!(sessions::search_impl(state, id, request))
        }
        "query_cancel" => {
            args!(id: Uuid = "sessionId", query: Uuid = "queryId");
            call!(sessions::cancel_impl(state, id, query))
        }
        "table_details" => {
            args!(id: Uuid = "sessionId", path: dbstudio_core::ObjectPath = "path");
            call!(browse::details_impl(state, id, path))
        }
        "table_count" => {
            args!(id: Uuid = "sessionId", request: TableCountRequest = "request");
            call!(browse::count_impl(state, id, request))
        }
        "driver_diagnostics" => {
            args!(kind: DbKind = "kind");
            Some(out(app::driver_diagnostics_impl(state, kind)))
        }
        "ssh_trust_host" => {
            args!(
                host: String = "host",
                port: u16 = "port",
                key_type: String = "keyType",
                fingerprint: String = "fingerprint"
            );
            call!(ssh::trust_host_impl(
                state,
                host,
                port,
                key_type,
                fingerprint
            ))
        }
        "connection_parse_uri" => {
            args!(uri_text: String = "uri");
            Some(out(uri::parse_uri_impl(state, &uri_text)))
        }
        "script_analyze" => {
            args!(id: Uuid = "sessionId", text: String = "text");
            Some(out(editor::analyze_impl(state, id, &text)))
        }
        "script_vocabulary" => {
            args!(id: Uuid = "sessionId");
            Some(out(editor::vocabulary_impl(state, id)))
        }
        "lease_open" => {
            args!(id: Uuid = "sessionId", options: LeaseOptions = "options");
            call!(editor::lease_open_impl(state, id, options))
        }
        "lease_close" => {
            args!(id: Uuid = "leaseId");
            call!(editor::lease_close_impl(state, id))
        }
        "lease_discard_result" => {
            args!(id: Uuid = "leaseId");
            call!(editor::lease_discard_result_impl(state, id))
        }
        "lease_cancel" => {
            args!(id: Uuid = "leaseId", query: Uuid = "queryId");
            call!(editor::lease_cancel_impl(state, id, query))
        }
        "lease_begin" => {
            args!(id: Uuid = "leaseId", options: TxOptions = "options");
            call!(editor::lease_begin_impl(state, id, options))
        }
        "lease_commit" => {
            args!(id: Uuid = "leaseId");
            call!(editor::lease_commit_impl(state, id))
        }
        "lease_rollback" => {
            args!(id: Uuid = "leaseId");
            call!(editor::lease_rollback_impl(state, id))
        }
        "lease_set_auto_commit" => {
            args!(id: Uuid = "leaseId", on: bool = "autoCommit");
            call!(editor::lease_set_auto_commit_impl(state, id, on))
        }
        "lease_set_context" => {
            args!(id: Uuid = "leaseId", context: LeaseContext = "context");
            call!(editor::lease_set_context_impl(state, id, context))
        }
        "lease_state" => {
            args!(id: Uuid = "leaseId");
            Some(out(editor::lease_state_impl(state, id)))
        }
        "sql_file_open" => call!(editor::sql_file_open_impl(state)),
        "sql_file_save" => {
            args!(path: Option<String> = "path", text: String = "text");
            call!(editor::sql_file_save_impl(state, path, text))
        }
        "history_list" => {
            args!(query: HistoryQuery = "query");
            call!(history::history_list_impl(state, query))
        }
        "history_delete" => {
            args!(ids: Vec<i64> = "ids");
            call!(history::history_delete_impl(state, ids))
        }
        "history_clear" => {
            args!(connection_id: Option<Uuid> = "connectionId");
            call!(history::history_clear_impl(state, connection_id))
        }
        "saved_queries_list" => call!(history::saved_queries_list_impl(state)),
        "saved_query_save" => {
            args!(query: SavedQuery = "query");
            call!(history::saved_query_save_impl(state, query))
        }
        "saved_query_delete" => {
            args!(id: Uuid = "id");
            call!(history::saved_query_delete_impl(state, id))
        }
        "saved_folder_save" => {
            args!(folder: SavedFolder = "folder");
            call!(history::saved_folder_save_impl(state, folder))
        }
        "saved_folder_delete" => {
            args!(id: Uuid = "id");
            call!(history::saved_folder_delete_impl(state, id))
        }
        "editor_tabs_get" => call!(history::editor_tabs_get_impl(state)),
        "editor_tabs_set" => {
            args!(tabs: Vec<EditorTabState> = "tabs");
            call!(history::editor_tabs_set_impl(state, tabs))
        }
        "table_editability" => {
            args!(id: Uuid = "sessionId", path: ObjectPath = "path");
            call!(changes::editability_impl(state, id, path))
        }
        "changes_preview" => {
            args!(id: Uuid = "sessionId", changes: ChangeSet = "changes");
            call!(changes::preview_impl(state, id, changes))
        }
        "changes_apply" => {
            args!(id: Uuid = "sessionId", changes: ChangeSet = "changes");
            call!(changes::apply_impl(state, id, changes))
        }
        "plan_explain" => {
            args!(
                id: Uuid = "sessionId",
                lease: Option<Uuid> = "leaseId",
                request: ExplainRequest = "request",
                rules: Option<PlanRules> = "rules"
            );
            call!(plan::explain_impl(state, id, lease, request, rules))
        }
        "plan_parse" => {
            args!(raw: RawPlan = "raw", rules: Option<PlanRules> = "rules");
            Some(out(plan::parse_impl(state, raw, rules)))
        }
        "plan_file_save" => {
            args!(raw: RawPlan = "raw");
            call!(plan::file_save_impl(state, raw))
        }
        "plan_file_open" => call!(plan::file_open_impl(state)),
        "schema_model" => {
            args!(id: Uuid = "sessionId", request: SchemaModelRequest = "request");
            call!(erd::schema_model_impl(state, id, request))
        }
        "erd_layouts_list" => {
            args!(id: Uuid = "connectionId");
            call!(erd::layouts_list_impl(state, id))
        }
        "erd_layout_get" => {
            args!(id: Uuid = "id");
            call!(erd::layout_get_impl(state, id))
        }
        "erd_layout_save" => {
            args!(layout: ErdLayout = "layout");
            call!(erd::layout_save_impl(state, layout))
        }
        "erd_layout_delete" => {
            args!(id: Uuid = "id");
            call!(erd::layout_delete_impl(state, id))
        }
        "ddl_generate" => {
            args!(id: Uuid = "sessionId", request: DdlRequest = "request");
            call!(erd::ddl_generate_impl(state, id, request))
        }
        "file_pick_save" => {
            args!(name: String = "suggestedName", filters: Vec<FileFilter> = "filters");
            call!(files::pick_save_impl(state, name, filters))
        }
        "file_pick_open" => {
            args!(filters: Vec<FileFilter> = "filters", directory: bool = "directory");
            call!(files::pick_open_impl(state, filters, directory))
        }
        "file_save_bytes" => {
            args!(
                name: String = "suggestedName",
                data: String = "dataBase64",
                filters: Vec<FileFilter> = "filters"
            );
            call!(files::save_bytes_impl(state, name, data, filters))
        }
        "export_start" => {
            args!(spec: ExportSpec = "spec");
            call!(export::start_impl(state, spec))
        }
        "export_presets_list" => {
            args!(connection_id: Option<Uuid> = "connectionId");
            call!(export::presets_list_impl(state, connection_id))
        }
        "export_preset_save" => {
            args!(preset: ExportPreset = "preset");
            call!(export::preset_save_impl(state, preset))
        }
        "export_preset_delete" => {
            args!(id: Uuid = "id");
            call!(export::preset_delete_impl(state, id))
        }
        "import_preview" => {
            args!(request: ImportPreviewRequest = "request");
            call!(export::import_preview_impl(state, request))
        }
        "import_start" => {
            args!(spec: ImportSpec = "spec", confirm: Option<ConfirmationGrant> = "confirm");
            call!(export::import_start_impl(state, spec, confirm))
        }
        "jobs_list" => Some(out(Ok(jobs::list_impl(state)))),
        "job_get" => {
            args!(id: JobId = "jobId");
            Some(out(jobs::get_impl(state, id)))
        }
        "job_cancel" => {
            args!(id: JobId = "jobId");
            Some(out(jobs::cancel_impl(state, id)))
        }
        "jobs_clear_finished" => Some(out(Ok(jobs::clear_finished_impl(state)))),
        "backup_jobs_list" => call!(backup::jobs_list_impl(state)),
        "backup_job_save" => {
            args!(job: BackupJob = "job", passphrase: SecretChange = "passphrase");
            call!(backup::job_save_impl(state, job, passphrase))
        }
        "backup_job_delete" => {
            args!(id: Uuid = "id");
            call!(backup::job_delete_impl(state, id))
        }
        "backup_job_run_now" => {
            args!(id: Uuid = "id");
            call!(backup::job_run_now_impl(state, id))
        }
        "backup_run_manual" => {
            args!(
                spec: BackupSpec = "spec",
                passphrase: Option<Secret> = "passphrase",
                save: Option<bool> = "savePassphrase"
            );
            call!(backup::run_manual_impl(
                state,
                spec,
                passphrase,
                save.unwrap_or(false)
            ))
        }
        "backup_runs_list" => {
            args!(
                job_id: Option<Uuid> = "jobId",
                limit: u32 = "limit",
                before: Option<String> = "before"
            );
            call!(backup::runs_list_impl(state, job_id, limit, before))
        }
        "backup_inspect" => {
            args!(path: String = "path", passphrase: Option<Secret> = "passphrase");
            call!(backup::inspect_impl(state, path, passphrase))
        }
        "backup_verify" => {
            args!(path: String = "path", passphrase: Option<Secret> = "passphrase");
            call!(backup::verify_impl(state, path, passphrase))
        }
        "restore_start" => {
            args!(
                spec: RestoreSpec = "spec",
                passphrase: Option<Secret> = "passphrase",
                confirm: Option<ConfirmationGrant> = "confirm"
            );
            call!(backup::restore_start_impl(state, spec, passphrase, confirm))
        }
        "backup_tools_detect" => call!(backup::tools_detect_impl(state)),
        "backup_tool_set_path" => {
            args!(tool: NativeTool = "tool", path: Option<String> = "path");
            call!(backup::tool_set_path_impl(state, tool, path))
        }
        "schedule_preview" => {
            args!(cron: String = "cron", timezone: String = "timezone", count: u32 = "count");
            Some(out(backup::schedule_preview_impl(cron, timezone, count)))
        }
        "background_get" => call!(background::get_impl(state)),
        "background_set" => {
            args!(settings: background::BackgroundSettings = "settings");
            call!(background::set_impl(state, settings))
        }
        "object_children_page" => {
            args!(id: Uuid = "sessionId", request: ChildrenPageRequest = "request");
            call!(keyvalue::children_page_impl(state, id, request))
        }
        "key_read" => {
            args!(id: Uuid = "sessionId", request: KeyReadRequest = "request");
            call!(keyvalue::key_read_impl(state, id, request))
        }
        "keys_delete_pattern" => {
            args!(
                id: Uuid = "sessionId",
                database: ObjectPath = "database",
                pattern: String = "pattern",
                confirm: Option<ConfirmationGrant> = "confirm"
            );
            call!(keyvalue::keys_delete_pattern_impl(
                state, id, database, pattern, confirm
            ))
        }
        _ => None,
    }
}

fn line(value: &impl Serialize) -> Bytes {
    let mut text = serde_json::to_string(value).unwrap_or_else(|_| "null".into());
    text.push('\n');
    Bytes::from(text)
}

/// Receiver of a streaming command's events; `false` when the client is gone.
type Emit = Box<dyn FnMut(QueryEvent) -> bool + Send>;

/// What to cancel on the server when the client disconnects before the command finished.
enum CancelTarget {
    Session(Uuid),
    Lease(Uuid),
}

/// A parsed streaming command, ready to run.
struct StreamPlan {
    query_id: Uuid,
    cancel: CancelTarget,
    run: Pin<Box<dyn Future<Output = Result<Value>> + Send>>,
}

/// Commands that stream `QueryEvent`s through a Tauri `Channel` in the desktop app.
fn is_stream_command(command: &str) -> bool {
    matches!(
        command,
        "table_fetch" | "query_execute" | "lease_execute" | "lease_fetch_more"
    )
}

/// Parse the arguments of a streaming command. `None` means `command` does not stream.
fn stream_plan(
    state: &AppState,
    command: &str,
    a: &Value,
    emit: Emit,
) -> Option<Result<StreamPlan>> {
    let state = state.clone();
    Some(match command {
        "table_fetch" => (|| {
            let session_id: Uuid = arg(a, "sessionId")?;
            let request: TableDataRequest = arg(a, "request")?;
            Ok(StreamPlan {
                query_id: request.query_id,
                cancel: CancelTarget::Session(session_id),
                run: Box::pin(async move {
                    out(browse::fetch_impl(&state, session_id, request, emit).await)
                }),
            })
        })(),
        "query_execute" => (|| {
            let session_id: Uuid = arg(a, "sessionId")?;
            let request: ExecuteRequest = arg(a, "request")?;
            Ok(StreamPlan {
                query_id: request.query.query_id,
                cancel: CancelTarget::Session(session_id),
                run: Box::pin(async move {
                    out(editor::execute_impl(&state, session_id, request, emit).await)
                }),
            })
        })(),
        "lease_execute" => (|| {
            let lease_id: Uuid = arg(a, "leaseId")?;
            let request: ExecuteRequest = arg(a, "request")?;
            Ok(StreamPlan {
                query_id: request.query.query_id,
                cancel: CancelTarget::Lease(lease_id),
                run: Box::pin(async move {
                    out(editor::lease_execute_impl(&state, lease_id, request, emit).await)
                }),
            })
        })(),
        "lease_fetch_more" => (|| {
            let lease_id: Uuid = arg(a, "leaseId")?;
            let query_id: Uuid = arg(a, "queryId")?;
            let max_rows: u64 = arg(a, "maxRows")?;
            let batch_size: Option<u32> = arg(a, "batchSize")?;
            Ok(StreamPlan {
                query_id,
                cancel: CancelTarget::Lease(lease_id),
                run: Box::pin(async move {
                    out(editor::lease_fetch_more_impl(
                        &state, lease_id, query_id, max_rows, batch_size, emit,
                    )
                    .await)
                }),
            })
        })(),
        _ => return None,
    })
}

async fn cancel_target(state: &AppState, target: &CancelTarget, query_id: Uuid) {
    match target {
        CancelTarget::Session(id) => {
            if let Ok(session) = state.sessions.get(*id) {
                let _ = session.cancel(query_id).await;
            }
        }
        CancelTarget::Lease(id) => {
            if let Ok(lease) = state.leases.get(*id) {
                let _ = lease.cancel(query_id).await;
            }
        }
    }
}

fn ndjson_response(rx: tokio::sync::mpsc::Receiver<Bytes>) -> Response {
    let body =
        Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx).map(Ok::<_, Infallible>));
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/x-ndjson")
        .header(header::CACHE_CONTROL, "no-store")
        .body(body)
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// A streaming command as an NDJSON stream with a bounded buffer. Every event is one line, the
/// last line is `{"type":"result","result":..}` or `{"type":"error","error":..}`. When the client
/// disconnects the receiver is dropped; the task notices through `tx.closed()` (even before the
/// driver has produced anything), cancels the query on the server and drops the command.
fn command_stream(state: AppState, command: &str, args: &Value) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(STREAM_BUFFER);
    // The sink is synchronous, so a full buffer ends the command (`Cancelled`) rather than
    // waiting for the client.
    let sender = tx.clone();
    let emit: Emit = Box::new(move |event: QueryEvent| sender.try_send(line(&event)).is_ok());
    let plan = match stream_plan(&state, command, args, emit) {
        Some(Ok(plan)) => plan,
        Some(Err(e)) => return error_response(StatusCode::BAD_REQUEST, &e),
        None => {
            return error_response(
                StatusCode::NOT_FOUND,
                &Error::NotFound(format!("command `{command}`")),
            );
        }
    };
    let StreamPlan {
        query_id,
        cancel,
        run,
    } = plan;
    tokio::spawn(async move {
        tokio::select! {
            result = run => {
                let last = match result {
                    Ok(value) => line(&json!({"type": "result", "result": value})),
                    Err(error) => line(&json!({"type": "error", "error": error})),
                };
                let _ = tx.send(last).await;
            }
            () = tx.closed() => {
                cancel_target(&state, &cancel, query_id).await;
                tracing::debug!(query = %query_id, "bridge client left; command cancelled");
            }
        }
    });
    ndjson_response(rx)
}

/// `GET /events`: every application event as one NDJSON line until the client leaves.
async fn events(State(ctx): State<Ctx>) -> Response {
    let mut rx = ctx.state.events.subscribe();
    let (tx, out_rx) = tokio::sync::mpsc::channel::<Bytes>(STREAM_BUFFER);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                event = rx.recv() => match event {
                    Ok(event) => {
                        let msg = line(&json!({"event": event.name, "payload": event.payload}));
                        if tx.send(msg).await.is_err() {
                            break;
                        }
                    }
                    // A slow client misses events; keep going.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
                () = tx.closed() => break,
            }
        }
    });
    ndjson_response(out_rx)
}

async fn ipc(State(ctx): State<Ctx>, Path(command): Path<String>, body: Bytes) -> Response {
    let args: Value = if body.iter().all(u8::is_ascii_whitespace) {
        json!({})
    } else {
        match serde_json::from_slice(&body) {
            Ok(v @ Value::Object(_)) => v,
            Ok(_) => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    &Error::Other("request body must be a JSON object".into()),
                );
            }
            Err(e) => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    &Error::Other(format!("invalid JSON body: {e}")),
                );
            }
        }
    };
    tracing::debug!(command = %command, "bridge call");
    if is_stream_command(&command) {
        return command_stream(ctx.state, &command, &args);
    }
    match dispatch(&ctx.state, &command, &args).await {
        Some(Ok(value)) => (StatusCode::OK, axum::Json(value)).into_response(),
        Some(Err(error)) => error_response(StatusCode::BAD_REQUEST, &error),
        None => error_response(
            StatusCode::NOT_FOUND,
            &Error::NotFound(format!("command `{command}`")),
        ),
    }
}

/// Start the bridge when `DBSTUDIO_DEV_BRIDGE_TOKEN` is set (port from
/// `DBSTUDIO_DEV_BRIDGE_PORT`, default 1421).
pub fn spawn_from_env(state: AppState) {
    if !tauri::is_dev() {
        tracing::error!("dev bridge refused: this is not a dev build (custom-protocol is active)");
        return;
    }
    let token = std::env::var("DBSTUDIO_DEV_BRIDGE_TOKEN").ok();
    let port = std::env::var("DBSTUDIO_DEV_BRIDGE_PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(DEFAULT_PORT);
    spawn_with(state, token, port);
}

/// Start the bridge with explicit settings. Returns whether it was started: a missing/empty
/// token disables it quietly, a token shorter than [`MIN_TOKEN_LEN`] disables it with an error.
pub fn spawn_with(state: AppState, token: Option<String>, port: u16) -> bool {
    let Some(token) = token.filter(|t| !t.is_empty()) else {
        tracing::info!(
            "dev-bridge feature compiled in but DBSTUDIO_DEV_BRIDGE_TOKEN is not set; bridge disabled"
        );
        return false;
    };
    if token.chars().count() < MIN_TOKEN_LEN {
        tracing::error!(
            "dev bridge NOT started: DBSTUDIO_DEV_BRIDGE_TOKEN must be at least {MIN_TOKEN_LEN} characters"
        );
        return false;
    }
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let app = router(state, token, port);
    tauri::async_runtime::spawn(async move {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => {
                tracing::warn!(%addr, "DEV BRIDGE ENABLED: the real backend is reachable over HTTP with the bridge token; development only");
                if let Err(e) = axum::serve(listener, app).await {
                    tracing::error!(error = %e, "dev bridge stopped");
                }
            }
            Err(e) => tracing::error!(%addr, error = %e, "dev bridge cannot listen"),
        }
    });
    true
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::to_bytes;
    use dbstudio_core::{ConnectionConfig, DbKind, DriverRegistry};
    use tower::ServiceExt;

    use super::*;
    use crate::session::testing::MockDriver;
    use crate::state::testing::test_state;

    const TOKEN: &str = "s3cret-token-0123456789-abcdefghijkl";
    const PORT: u16 = 1421;

    fn app_with_probe() -> (Router, Arc<crate::session::testing::Probe>, AppState) {
        let (driver, probe) = MockDriver::new(DbKind::Postgres);
        let mut registry = DriverRegistry::new();
        registry.register(driver);
        let (state, _) = test_state(registry);
        (router(state.clone(), TOKEN.into(), PORT), probe, state)
    }

    fn app() -> Router {
        app_with_probe().0
    }

    async fn send(
        app: &Router,
        method: Method,
        uri: &str,
        token: Option<&str>,
        origin: Option<&str>,
        body: Value,
    ) -> (StatusCode, axum::http::HeaderMap, Bytes) {
        send_with_host(app, method, uri, token, origin, body, "127.0.0.1:1421").await
    }

    async fn send_with_host(
        app: &Router,
        method: Method,
        uri: &str,
        token: Option<&str>,
        origin: Option<&str>,
        body: Value,
        host: &str,
    ) -> (StatusCode, axum::http::HeaderMap, Bytes) {
        let mut req = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header(header::HOST, host);
        if let Some(t) = token {
            req = req.header(TOKEN_HEADER, t);
        }
        if let Some(o) = origin {
            req = req.header(header::ORIGIN, o);
        }
        let req = req
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let (parts, body) = resp.into_parts();
        (
            parts.status,
            parts.headers,
            to_bytes(body, usize::MAX).await.unwrap(),
        )
    }

    async fn call(app: &Router, command: &str, body: Value) -> (StatusCode, Value) {
        let (status, _, bytes) = send(
            app,
            Method::POST,
            &format!("/ipc/{command}"),
            Some(TOKEN),
            None,
            body,
        )
        .await;
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    fn profile_json() -> Value {
        let cfg = ConnectionConfig::new(DbKind::Postgres, "mock", "localhost");
        json!({
            "id": cfg.id, "name": "mock", "kind": "postgres", "host": "localhost",
            "port": 5432, "database": null, "username": null,
            "tls": {"mode": "prefer"}, "readOnly": false, "environment": null,
            "savePassword": false
        })
    }

    #[tokio::test]
    async fn token_is_required() {
        let app = app();
        for token in [None, Some("wrong"), Some(""), Some("s3cret-token-longer")] {
            let (status, _, body) = send(
                &app,
                Method::POST,
                "/ipc/list_drivers",
                token,
                None,
                json!({}),
            )
            .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{token:?}");
            let json: Value = serde_json::from_slice(&body).unwrap();
            assert!(json["kind"].is_string() && json["message"].is_string());
        }
        // Unknown routes are also behind the token.
        let (status, ..) = send(&app, Method::POST, "/nope", None, None, json!({})).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd") && !constant_time_eq(b"abc", b"ab"));
    }

    #[tokio::test]
    async fn foreign_host_and_origin_are_forbidden() {
        let app = app();
        // DNS rebinding: right token, but the request arrives under another host name.
        for host in [
            "evil.example",
            "evil.example:1421",
            "127.0.0.1",
            "127.0.0.1:9999",
            "localhost.evil.com:1421",
        ] {
            let (status, headers, _) = send_with_host(
                &app,
                Method::POST,
                "/ipc/list_drivers",
                Some(TOKEN),
                None,
                json!({}),
                host,
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{host}");
            assert!(headers.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).is_none());
        }
        for host in ["127.0.0.1:1421", "localhost:1421", "LOCALHOST:1421"] {
            let (status, ..) = send_with_host(
                &app,
                Method::POST,
                "/ipc/list_drivers",
                Some(TOKEN),
                None,
                json!({}),
                host,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{host}");
        }
        // A foreign (or `null`) Origin is rejected even with the correct token; so is preflight.
        for origin in [
            "http://evil.example",
            "null",
            "http://localhost:1421",
            "https://localhost:1420",
        ] {
            let (status, headers, _) = send(
                &app,
                Method::POST,
                "/ipc/list_drivers",
                Some(TOKEN),
                Some(origin),
                json!({}),
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{origin}");
            assert!(headers.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).is_none());
            let (status, ..) = send(
                &app,
                Method::OPTIONS,
                "/ipc/list_drivers",
                None,
                Some(origin),
                json!({}),
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "preflight {origin}");
        }
    }

    #[tokio::test]
    async fn short_or_missing_token_does_not_start_the_bridge() {
        let (_, _, state) = app_with_probe();
        assert!(!spawn_with(state.clone(), None, PORT));
        assert!(!spawn_with(state.clone(), Some(String::new()), PORT));
        assert!(!spawn_with(
            state,
            Some("a".repeat(MIN_TOKEN_LEN - 1)),
            PORT
        ));
    }

    #[tokio::test]
    async fn disconnecting_client_cancels_a_slow_fetch() {
        let (app, probe, _) = app_with_probe();
        let profile = profile_json();
        let id = profile["id"].clone();
        call(
            &app,
            "connection_save",
            json!({"profile": profile, "secret": {"action": "keep"}}),
        )
        .await;
        let (_, opened) = call(&app, "session_open", json!({"connectionId": id})).await;
        let query_id = Uuid::new_v4();
        let body = json!({"sessionId": opened["sessionId"], "request": {
            "queryId": query_id,
            "table": {"segments": [{"kind": "table", "name": "t"}]},
            "rawWhere": "slow",
            "limit": 10
        }});
        let req = axum::http::Request::builder()
            .method(Method::POST)
            .uri("/ipc/table_fetch")
            .header(header::HOST, "127.0.0.1:1421")
            .header(TOKEN_HEADER, TOKEN)
            .body(Body::from(body.to_string()))
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        // The query is slow and has produced no line yet; the client goes away.
        drop(response);
        let mut cancelled = false;
        for _ in 0..100 {
            if probe.cancels.lock().unwrap().contains(&query_id) {
                cancelled = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            cancelled,
            "query_cancel was not issued after the client disconnected"
        );
    }

    #[tokio::test]
    async fn cors_policy() {
        let app = app();
        for origin in ALLOWED_ORIGINS {
            let (status, headers, _) = send(
                &app,
                Method::OPTIONS,
                "/ipc/list_drivers",
                None,
                Some(origin),
                json!({}),
            )
            .await;
            assert_eq!(status, StatusCode::NO_CONTENT);
            assert_eq!(headers[header::ACCESS_CONTROL_ALLOW_ORIGIN], origin);
            let allow = headers[header::ACCESS_CONTROL_ALLOW_HEADERS]
                .to_str()
                .unwrap();
            assert!(allow.contains("x-dbstudio-bridge-token") && allow.contains("content-type"));
        }
        let (status, headers, _) = send(
            &app,
            Method::OPTIONS,
            "/ipc/list_drivers",
            None,
            Some("http://evil.example"),
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(headers.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).is_none());
        // Real responses (also 401s, so the browser can read them) carry the header.
        let (_, headers, _) = send(
            &app,
            Method::POST,
            "/ipc/list_drivers",
            Some(TOKEN),
            Some("http://localhost:1420"),
            json!({}),
        )
        .await;
        assert_eq!(
            headers[header::ACCESS_CONTROL_ALLOW_ORIGIN],
            "http://localhost:1420"
        );
        let (status, headers, _) = send(
            &app,
            Method::POST,
            "/ipc/list_drivers",
            None,
            Some("http://127.0.0.1:1420"),
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(
            headers[header::ACCESS_CONTROL_ALLOW_ORIGIN],
            "http://127.0.0.1:1420"
        );
    }

    #[tokio::test]
    async fn ordinary_commands_and_error_shape() {
        let app = app();
        let (status, drivers) = call(&app, "list_drivers", json!({})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(drivers.as_array().unwrap().len(), 9);

        let (status, saved) = call(
            &app,
            "connection_save",
            json!({"profile": profile_json(), "secret": {"action": "keep"}}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{saved}");
        assert_eq!(saved["name"], "mock");
        let (_, list) = call(&app, "connections_list", json!({})).await;
        assert_eq!(list["profiles"].as_array().unwrap().len(), 1);

        let (_, settings) = call(&app, "settings_get", json!({})).await;
        assert_eq!(settings["logLevel"], "info");
        let (_, set) = call(
            &app,
            "settings_set",
            json!({"settings": {"gridMaxRows": 5}}),
        )
        .await;
        assert_eq!(set["gridMaxRows"], 1000);

        // Errors: 400 with the Error JSON.
        let (status, err) = call(
            &app,
            "object_children",
            json!({"sessionId": Uuid::new_v4(), "path": {"segments": []}}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(err["kind"], "notFound");
        assert!(err["message"].as_str().unwrap().contains("session"));
        // Bad arguments and bad bodies are 400 too.
        let (status, err) = call(&app, "session_close", json!({"sessionId": "nope"})).await;
        assert_eq!(
            (status, err["kind"].as_str()),
            (StatusCode::BAD_REQUEST, Some("other"))
        );
        let (status, ..) = send(
            &app,
            Method::POST,
            "/ipc/list_drivers",
            Some(TOKEN),
            None,
            json!([1]),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn unknown_command_is_404() {
        let app = app();
        let (status, err) = call(&app, "no_such_command", json!({})).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(err["kind"], "notFound");
        let (status, ..) = send(
            &app,
            Method::POST,
            "/elsewhere",
            Some(TOKEN),
            None,
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, ..) = send(
            &app,
            Method::GET,
            "/ipc/list_drivers",
            Some(TOKEN),
            None,
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn table_fetch_streams_ndjson() {
        let app = app();
        let profile = profile_json();
        let id = profile["id"].clone();
        call(
            &app,
            "connection_save",
            json!({"profile": profile, "secret": {"action": "keep"}}),
        )
        .await;
        let (status, opened) = call(&app, "session_open", json!({"connectionId": id})).await;
        assert_eq!(status, StatusCode::OK, "{opened}");
        let session_id = opened["sessionId"].clone();
        let request = json!({
            "queryId": Uuid::new_v4(),
            "table": {"segments": [{"kind": "table", "name": "t"}]},
            "limit": 100
        });

        let (status, headers, body) = send(
            &app,
            Method::POST,
            "/ipc/table_fetch",
            Some(TOKEN),
            None,
            json!({"sessionId": session_id, "request": request}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_TYPE], "application/x-ndjson");
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.ends_with('\n'));
        let lines: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let types: Vec<&str> = lines.iter().map(|l| l["type"].as_str().unwrap()).collect();
        assert_eq!(
            types,
            [
                "columns",
                "rows",
                "rows",
                "rows",
                "message",
                "statementEnd",
                "result"
            ]
        );
        assert_eq!(lines[1]["rows"][0][0], json!({"t": "int", "v": 1}));
        assert_eq!(lines[6]["result"]["statements"], 1);
        assert!(lines[6].get("summary").is_none());

        // Unknown session: the stream ends with exactly one error line.
        let (_, _, body) = send(
            &app,
            Method::POST,
            "/ipc/table_fetch",
            Some(TOKEN),
            None,
            json!({"sessionId": Uuid::new_v4(), "request": request}),
        )
        .await;
        let lines: Vec<Value> = String::from_utf8(body.to_vec())
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["type"], "error");
        assert_eq!(lines[0]["error"]["kind"], "notFound");

        // Malformed arguments are rejected before streaming.
        let (status, ..) = send(
            &app,
            Method::POST,
            "/ipc/table_fetch",
            Some(TOKEN),
            None,
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let _ = Arc::new(0);
    }

    async fn open_session(app: &Router) -> Value {
        let profile = profile_json();
        let id = profile["id"].clone();
        call(
            app,
            "connection_save",
            json!({"profile": profile, "secret": {"action": "keep"}}),
        )
        .await;
        let (status, opened) = call(app, "session_open", json!({"connectionId": id})).await;
        assert_eq!(status, StatusCode::OK, "{opened}");
        opened["sessionId"].clone()
    }

    async fn stream_lines(app: &Router, command: &str, body: Value) -> Vec<Value> {
        let (status, headers, bytes) = send(
            app,
            Method::POST,
            &format!("/ipc/{command}"),
            Some(TOKEN),
            None,
            body,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_TYPE], "application/x-ndjson");
        String::from_utf8(bytes.to_vec())
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn new_commands_are_dispatched() {
        let (app, _, state) = app_with_probe();
        // jobs
        let (status, jobs) = call(&app, "jobs_list", json!({})).await;
        assert_eq!((status, jobs), (StatusCode::OK, json!([])));
        let (id, _) = state
            .jobs
            .create(dbstudio_core::JobKind::Export, "Export", None, true);
        let (_, jobs) = call(&app, "jobs_list", json!({})).await;
        assert_eq!(jobs[0]["title"], "Export");
        assert_eq!(jobs[0]["status"], "queued");
        let (status, job) = call(&app, "job_get", json!({"jobId": id})).await;
        assert_eq!(
            (status, job["id"].as_str()),
            (StatusCode::OK, Some(id.to_string().as_str()))
        );
        let (status, _) = call(&app, "job_cancel", json!({"jobId": id})).await;
        assert_eq!(status, StatusCode::OK);
        state.jobs.finish(id, Err(Error::Cancelled));
        let (_, removed) = call(&app, "jobs_clear_finished", json!({})).await;
        assert_eq!(removed, json!(1));
        let (status, err) = call(&app, "job_get", json!({"jobId": Uuid::new_v4()})).await;
        assert_eq!(
            (status, err["kind"].as_str()),
            (StatusCode::BAD_REQUEST, Some("notFound"))
        );

        // script_analyze on a live (mock) session
        let session = open_session(&app).await;
        let (status, analysis) = call(
            &app,
            "script_analyze",
            json!({"sessionId": session, "text": "select 1"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{analysis}");
        assert_eq!(analysis["dialect"], json!({"name": "opaque"}));
        assert_eq!(analysis["statements"].as_array().unwrap().len(), 1);
        let (status, vocab) = call(&app, "script_vocabulary", json!({"sessionId": session})).await;
        assert_eq!(status, StatusCode::OK);
        assert!(vocab["keywords"].is_array());

        // driver_diagnostics, optional arguments default to null
        let (status, diag) = call(&app, "driver_diagnostics", json!({"kind": "postgres"})).await;
        assert_eq!(
            (status, diag["status"].clone()),
            (StatusCode::OK, json!({"state": "available"}))
        );
        let (status, page) = call(
            &app,
            "object_children_page",
            json!({"sessionId": session, "request": {"parent": {"segments": []}, "limit": 5}}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{page}");
        assert_eq!(page["nodes"], json!([]));
    }

    #[tokio::test]
    async fn unsupported_commands_return_the_standard_error_json() {
        let app = app();
        let session = open_session(&app).await;
        let (status, err) = call(
            &app,
            "lease_open",
            json!({"sessionId": session, "options": {"autoCommit": true}}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            err,
            json!({"kind": "unsupported", "message": "leases", "capability": "leases"})
        );
        let (status, err) = call(
            &app,
            "connection_parse_uri",
            json!({"uri": "postgres://h/d"}),
        )
        .await;
        assert_eq!(
            (status, err["kind"].as_str()),
            (StatusCode::BAD_REQUEST, Some("unsupported"))
        );
        let (status, err) = call(
            &app,
            "schedule_preview",
            json!({"cron": "* * * * *", "timezone": "utc", "count": 3}),
        )
        .await;
        assert_eq!(
            (status, err["kind"].as_str()),
            (StatusCode::BAD_REQUEST, Some("unsupported"))
        );
        // A command with a missing required argument is an argument error, not a 404.
        let (status, err) = call(&app, "lease_open", json!({})).await;
        assert_eq!(
            (status, err["kind"].as_str()),
            (StatusCode::BAD_REQUEST, Some("other"))
        );
    }

    #[tokio::test]
    async fn query_execute_streams_like_table_fetch() {
        let (app, probe, _) = app_with_probe();
        let session = open_session(&app).await;
        let query_id = Uuid::new_v4();
        let lines = stream_lines(
            &app,
            "query_execute",
            json!({"sessionId": session, "request": {"text": "select 1", "queryId": query_id}}),
        )
        .await;
        let types: Vec<&str> = lines.iter().map(|l| l["type"].as_str().unwrap()).collect();
        assert_eq!(
            types,
            [
                "statementStart",
                "columns",
                "rows",
                "statementEnd",
                "result"
            ]
        );
        let result = &lines[4]["result"];
        assert_eq!(result["summary"]["statements"], 1);
        assert_eq!(result["state"], json!(null));
        assert_eq!(probe.executed.lock().unwrap().as_slice(), ["select 1"]);

        // Errors end the stream with one error line.
        let lines = stream_lines(
            &app,
            "query_execute",
            json!({"sessionId": session, "request": {"text": "fail", "queryId": Uuid::new_v4()}}),
        )
        .await;
        let last = lines.last().unwrap();
        assert_eq!(last["type"], "error");
        assert_eq!(last["error"]["kind"], "query");
        assert_eq!(last["error"]["position"], 2);

        // Unknown lease: a single error line; bad arguments are rejected before streaming.
        let lines = stream_lines(
            &app,
            "lease_execute",
            json!({"leaseId": Uuid::new_v4(), "request": {"text": "x", "queryId": Uuid::new_v4()}}),
        )
        .await;
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["error"]["kind"], "notFound");
        let lines = stream_lines(
            &app,
            "lease_fetch_more",
            json!({"leaseId": Uuid::new_v4(), "queryId": Uuid::new_v4(), "maxRows": 10, "batchSize": null}),
        )
        .await;
        assert_eq!(lines[0]["error"]["kind"], "notFound");
        let (status, ..) = send(
            &app,
            Method::POST,
            "/ipc/query_execute",
            Some(TOKEN),
            None,
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn events_endpoint_streams_app_events_as_ndjson() {
        use crate::events::EventEmitter as _;
        let (app, _, state) = app_with_probe();
        // The token and host checks apply to GET /events too.
        let (status, ..) = send(&app, Method::GET, "/events", None, None, json!({})).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let req = axum::http::Request::builder()
            .method(Method::GET)
            .uri("/events")
            .header(header::HOST, "127.0.0.1:1421")
            .header(TOKEN_HEADER, TOKEN)
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "application/x-ndjson"
        );
        let mut body = response.into_body().into_data_stream();

        state.events.emit(
            crate::events::JOB_PROGRESS,
            json!({"type": "progress", "jobId": "j"}),
        );
        state
            .events
            .emit(crate::events::BACKUP_CHANGED, Value::Null);
        let mut text = String::new();
        while text.lines().count() < 2 {
            let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), body.next())
                .await
                .expect("event line in time")
                .expect("stream open")
                .unwrap();
            text.push_str(std::str::from_utf8(&chunk).unwrap());
        }
        let lines: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(
            lines[0],
            json!({"event": "job://progress", "payload": {"type": "progress", "jobId": "j"}})
        );
        assert_eq!(
            lines[1],
            json!({"event": "backup://changed", "payload": null})
        );
    }

    #[tokio::test]
    async fn cors_allows_get_for_the_events_stream() {
        let app = app();
        let (_, headers, _) = send(
            &app,
            Method::OPTIONS,
            "/events",
            None,
            Some("http://localhost:1420"),
            json!({}),
        )
        .await;
        assert!(
            headers[header::ACCESS_CONTROL_ALLOW_METHODS]
                .to_str()
                .unwrap()
                .contains("GET")
        );
    }
}
