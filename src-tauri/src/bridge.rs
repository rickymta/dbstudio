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
//! * `POST /ipc/table_fetch` streams NDJSON: one `QueryEvent` per line, then exactly one
//!   `{"type":"result","summary":..}` or `{"type":"error","error":..}` line.

use std::convert::Infallible;
use std::net::SocketAddr;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use dbstudio_core::{
    Error, ObjectSearchRequest, QueryEvent, Result, Secret, TableCountRequest, TableDataRequest,
};
use dbstudio_store::{AppSettings, ConnectionGroup, ConnectionProfile, SecretChange};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio_stream::StreamExt;
use uuid::Uuid;

use crate::commands::{app, browse, connections, sessions};
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
        HeaderValue::from_static("POST, OPTIONS"),
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
            args!(profile: ConnectionProfile = "profile", secret: SecretChange = "secret");
            call!(connections::save_impl(state, profile, secret))
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
        _ => None,
    }
}

fn line(value: &impl Serialize) -> Bytes {
    let mut text = serde_json::to_string(value).unwrap_or_else(|_| "null".into());
    text.push('\n');
    Bytes::from(text)
}

/// `table_fetch` as an NDJSON stream with a bounded buffer. When the client disconnects the
/// receiver is dropped; the task notices through `tx.closed()` (even before the driver has
/// produced anything), cancels the query on the server and drops the fetch.
fn fetch_stream(state: AppState, session_id: Uuid, request: TableDataRequest) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(STREAM_BUFFER);
    let query_id = request.query_id;
    tokio::spawn(async move {
        let session = state.sessions.get(session_id).ok();
        let sender = tx.clone();
        // The sink is synchronous, so a full buffer ends the fetch (`Cancelled`) rather than
        // waiting for the client.
        let fetch = browse::fetch_impl(&state, session_id, request, move |event: QueryEvent| {
            sender.try_send(line(&event)).is_ok()
        });
        tokio::select! {
            result = fetch => {
                let last = match result {
                    Ok(summary) => line(&json!({"type": "result", "summary": summary})),
                    Err(error) => line(&json!({"type": "error", "error": error})),
                };
                let _ = tx.send(last).await;
            }
            () = tx.closed() => {
                if let Some(session) = session {
                    let _ = session.cancel(query_id).await;
                }
                tracing::debug!(query = %query_id, "bridge client left; fetch cancelled");
            }
        }
    });
    let body =
        Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx).map(Ok::<_, Infallible>));
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/x-ndjson")
        .header(header::CACHE_CONTROL, "no-store")
        .body(body)
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
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
    if command == "table_fetch" {
        let parsed = arg::<Uuid>(&args, "sessionId")
            .and_then(|id| arg::<TableDataRequest>(&args, "request").map(|r| (id, r)));
        return match parsed {
            Ok((id, request)) => fetch_stream(ctx.state, id, request),
            Err(e) => error_response(StatusCode::BAD_REQUEST, &e),
        };
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
        assert_eq!(lines[6]["summary"]["statements"], 1);

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
}
