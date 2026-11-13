//! Paged object tree and key-value commands (package L).

use std::sync::Arc;

use dbstudio_core::{
    ChildrenPage, ChildrenPageRequest, ColumnMeta, ConfirmationGrant, ConfirmationRequest, DbKind,
    DestructiveReason, DestructiveStatement, EndOutcome, Error, JobId, JobKind, JobProgress,
    JobReporter, KeyReadRequest, KeyValuePage, ObjectKind, ObjectPath, QueryRequest, ResultSink,
    StatementEnd, TextRange, Value,
};
use tauri::State;
use uuid::Uuid;

use crate::guard;
use crate::jobs::JobContext;
use crate::state::AppState;

type Result<T> = std::result::Result<T, Error>;

pub(crate) async fn children_page_impl(
    state: &AppState,
    session_id: Uuid,
    request: ChildrenPageRequest,
) -> Result<ChildrenPage> {
    state
        .sessions
        .get(session_id)?
        .list_children_page(request)
        .await
}

pub(crate) async fn key_read_impl(
    state: &AppState,
    session_id: Uuid,
    request: KeyReadRequest,
) -> Result<KeyValuePage> {
    state.sessions.get(session_id)?.read_key(request).await
}

/// Pseudo command of the Redis driver that scans with `SCAN MATCH` and `UNLINK`s in batches,
/// reporting `deleted=<n> scanned=<rounds>` messages (see `dbstudio_driver_redis::DELETE_PATTERN`).
const DELETE_PATTERN: &str = "DBSTUDIO.DELPATTERN";

/// Quote `arg` for a redis-cli line (`"..."` with `\"`, `\\` and `\xHH` escapes).
fn quote_cli_arg(arg: &str) -> String {
    let mut out = String::from("\"");
    for b in arg.bytes() {
        match b {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            0x20..=0x7e => out.push(b as char),
            // Non-ASCII bytes are escaped one by one; the server sees the original UTF-8.
            b => out.push_str(&format!("\\x{b:02x}")),
        }
    }
    out.push('"');
    out
}

/// Database number named by the `Database` segment of `database`.
fn database_name(database: &ObjectPath) -> Result<String> {
    database
        .segments
        .iter()
        .find(|s| s.kind == ObjectKind::Database)
        .map(|s| s.name.clone())
        .ok_or_else(|| Error::Other("path does not name a database".into()))
}

/// Reports the progress of the delete scan to the job and stops it on cancellation.
struct DeleteSink {
    job: Arc<JobContext>,
    deleted: u64,
}

impl ResultSink for DeleteSink {
    fn on_columns(&mut self, _: Vec<ColumnMeta>) -> Result<()> {
        Ok(())
    }

    fn on_rows(&mut self, _: Vec<Vec<Value>>) -> Result<()> {
        Ok(())
    }

    fn on_message(&mut self, message: String) -> Result<()> {
        if let Some(n) = message
            .split_whitespace()
            .find_map(|part| part.strip_prefix("deleted="))
            .and_then(|n| n.parse::<u64>().ok())
        {
            self.deleted = n;
        }
        self.job.progress(&JobProgress {
            phase: "delete".into(),
            rows_done: self.deleted,
            message: Some(message),
            ..JobProgress::default()
        });
        if self.job.cancel_token().is_cancelled() {
            return Err(Error::Cancelled);
        }
        Ok(())
    }

    fn on_statement_end(&mut self, end: StatementEnd) -> Result<()> {
        if let EndOutcome::Affected { rows } = end.outcome {
            self.deleted = rows;
        }
        Ok(())
    }
}

pub(crate) async fn keys_delete_pattern_impl(
    state: &AppState,
    session_id: Uuid,
    database: ObjectPath,
    pattern: String,
    confirm: Option<ConfirmationGrant>,
) -> Result<JobId> {
    let (session, config) = state.sessions.get_with_config(session_id)?;
    if config.kind != DbKind::Redis {
        return Err(Error::Unsupported("key_value".into()));
    }
    guard::ensure_writable(&config, "deleting keys")?;
    if pattern.is_empty() {
        return Err(Error::Other("the pattern must not be empty".into()));
    }
    let db = database_name(&database)?;
    let (_, production) = guard::policy_flags(&config);
    if production {
        // Deleting by pattern is always destructive: the grant is bound to this exact request.
        let target = format!("session:{session_id}");
        let payload = serde_json::to_vec(&serde_json::json!({
            "database": database,
            "pattern": pattern,
        }))
        .map_err(|e| Error::Other(e.to_string()))?;
        let granted = confirm
            .as_ref()
            .is_some_and(|g| state.confirm.consume(g, &target, &payload).is_ok());
        if !granted {
            let confirm_text = guard::confirm_text_for(&config, Some(&db));
            let (token, expires_in_ms) = state.confirm.issue(&target, &payload, &confirm_text);
            let text = format!("DELETE KEYS MATCH {pattern}");
            let end = u32::try_from(text.chars().count()).unwrap_or(u32::MAX);
            return Err(Error::ConfirmationRequired(Box::new(ConfirmationRequest {
                token,
                confirm_text,
                statements: vec![DestructiveStatement {
                    index: 0,
                    range: TextRange { start: 0, end },
                    text,
                    reason: DestructiveReason::DeleteByPattern,
                }],
                expires_in_ms,
            })));
        }
    }
    let title = format!("Delete keys {pattern} (db{db})");
    let job = state.jobs.spawn(
        JobKind::KeyDelete,
        title,
        Some(config.id),
        move |ctx| async move {
            let mut req =
                QueryRequest::new(format!("{DELETE_PATTERN} {}", quote_cli_arg(&pattern)));
            req.database = Some(db);
            let mut sink = DeleteSink {
                job: ctx.clone(),
                deleted: 0,
            };
            let outcome = session.execute(req, &mut sink).await;
            let deleted = sink.deleted;
            ctx.progress(&JobProgress {
                phase: "delete".into(),
                rows_done: deleted,
                ..JobProgress::default()
            });
            outcome?;
            Ok(Some(serde_json::json!({ "deleted": deleted })))
        },
    );
    Ok(job)
}

#[tauri::command]
pub async fn object_children_page(
    state: State<'_, AppState>,
    session_id: Uuid,
    request: ChildrenPageRequest,
) -> Result<ChildrenPage> {
    tracing::debug!(command = "object_children_page", session = %session_id, "ipc call");
    children_page_impl(&state, session_id, request).await
}

#[tauri::command]
pub async fn key_read(
    state: State<'_, AppState>,
    session_id: Uuid,
    request: KeyReadRequest,
) -> Result<KeyValuePage> {
    tracing::debug!(command = "key_read", session = %session_id, "ipc call");
    key_read_impl(&state, session_id, request).await
}

#[tauri::command]
pub async fn keys_delete_pattern(
    state: State<'_, AppState>,
    session_id: Uuid,
    database: ObjectPath,
    pattern: String,
    confirm: Option<ConfirmationGrant>,
) -> Result<JobId> {
    tracing::debug!(command = "keys_delete_pattern", session = %session_id, "ipc call");
    keys_delete_pattern_impl(&state, session_id, database, pattern, confirm).await
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use dbstudio_core::{
        ConnectionConfig, Environment, JobStatus, KeyReadRequest, QueryRequest, Secret, TlsMode,
        execute_collect,
    };

    use super::*;
    use crate::state::build_registry;
    use crate::state::testing::test_state;

    #[test]
    fn cli_arguments_are_quoted() {
        assert_eq!(quote_cli_arg("a*"), "\"a*\"");
        assert_eq!(quote_cli_arg("q\"b\\"), "\"q\\\"b\\\\\"");
        assert_eq!(quote_cli_arg("é"), "\"\\xc3\\xa9\"");
    }

    #[test]
    fn database_is_taken_from_the_path() {
        let p = ObjectPath::root().child(ObjectKind::Database, "3");
        assert_eq!(database_name(&p).unwrap(), "3");
        assert!(database_name(&ObjectPath::root()).is_err());
    }

    fn enabled() -> bool {
        std::env::var("DBSTUDIO_IT").as_deref() == Ok("1")
    }

    /// The tests share database 13 of the server: run them one at a time.
    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn redis_config(environment: Option<Environment>, read_only: bool) -> ConnectionConfig {
        let mut cfg = ConnectionConfig::new(DbKind::Redis, "it", "127.0.0.1");
        cfg.port = std::env::var("DBSTUDIO_IT_REDIS6_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(63796);
        cfg.username = Some("dbstudio".into());
        cfg.tls.mode = TlsMode::Disable;
        cfg.environment = environment;
        cfg.read_only = read_only;
        cfg
    }

    async fn open(
        state: &AppState,
        environment: Option<Environment>,
        read_only: bool,
    ) -> (Uuid, ObjectPath) {
        let cfg = redis_config(environment, read_only);
        let info = state
            .sessions
            .open(&cfg, Some(&Secret::new("dbstudio_test")))
            .await
            .unwrap();
        (
            info.session_id,
            ObjectPath::root().child(ObjectKind::Database, "13"),
        )
    }

    async fn seed(state: &AppState, session: Uuid, keys: usize) {
        let s = state.sessions.get(session).unwrap();
        let mut req = QueryRequest::new("FLUSHDB");
        req.database = Some("13".into());
        execute_collect(s.as_ref(), req).await.unwrap();
        let script: String = (0..keys)
            .map(|i| format!("SET tmp:{i} v\n"))
            .chain(["SET keep v".to_string()])
            .collect();
        let mut req = QueryRequest::new(script);
        req.database = Some("13".into());
        execute_collect(s.as_ref(), req).await.unwrap();
    }

    async fn wait_finished(state: &AppState, job: JobId) -> dbstudio_core::JobInfo {
        for _ in 0..200 {
            let info = state.jobs.get(job).unwrap();
            if !matches!(info.status, JobStatus::Queued | JobStatus::Running) {
                return info;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("job did not finish");
    }

    async fn dbsize(state: &AppState, session: Uuid) -> i64 {
        let s = state.sessions.get(session).unwrap();
        let mut req = QueryRequest::new("DBSIZE");
        req.database = Some("13".into());
        let r = execute_collect(s.as_ref(), req).await.unwrap();
        match &r[0].outcome {
            dbstudio_core::StatementOutcome::Rows(rs) => match rs.rows[0][0] {
                Value::Int(n) => n,
                _ => panic!(),
            },
            _ => panic!(),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn delete_pattern_runs_as_a_job() {
        if !enabled() {
            return;
        }
        let _serial = SERIAL.lock().await;
        let (state, _) = test_state(build_registry());
        let (session, db) = open(&state, Some(Environment::Development), false).await;
        seed(&state, session, 1500).await;
        let job = keys_delete_pattern_impl(&state, session, db, "tmp:*".into(), None)
            .await
            .unwrap();
        let info = wait_finished(&state, job).await;
        assert_eq!(info.status, JobStatus::Succeeded, "{:?}", info.error);
        assert_eq!(info.kind, JobKind::KeyDelete);
        assert_eq!(info.result.unwrap()["deleted"], 1500);
        assert_eq!(info.progress.rows_done, 1500);
        assert_eq!(dbsize(&state, session).await, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn delete_pattern_on_production_needs_a_matching_confirmation() {
        if !enabled() {
            return;
        }
        let _serial = SERIAL.lock().await;
        let (state, _) = test_state(build_registry());
        let (session, db) = open(&state, Some(Environment::Production), false).await;
        seed(&state, session, 20).await;
        let ask = |confirm| {
            keys_delete_pattern_impl(&state, session, db.clone(), "tmp:*".into(), confirm)
        };
        let req = match ask(None).await {
            Err(Error::ConfirmationRequired(r)) => r,
            other => panic!("{other:?}"),
        };
        assert_eq!(req.confirm_text, "db13");
        assert_eq!(req.statements[0].reason, DestructiveReason::DeleteByPattern);
        assert_eq!(dbsize(&state, session).await, 21);
        // Wrong text: refused (and the token is spent).
        let wrong = ConfirmationGrant {
            token: req.token.clone(),
            typed: "13".into(),
        };
        let req2 = match ask(Some(wrong)).await {
            Err(Error::ConfirmationRequired(r)) => r,
            other => panic!("{other:?}"),
        };
        assert_ne!(req2.token, req.token);
        // A token for another pattern does not match.
        let other_pattern = keys_delete_pattern_impl(
            &state,
            session,
            db.clone(),
            "keep*".into(),
            Some(ConfirmationGrant {
                token: req2.token.clone(),
                typed: "db13".into(),
            }),
        )
        .await;
        assert!(matches!(other_pattern, Err(Error::ConfirmationRequired(_))));
        assert_eq!(dbsize(&state, session).await, 21);
        // Right token and text: runs. The token cannot be reused afterwards.
        let req3 = match ask(None).await {
            Err(Error::ConfirmationRequired(r)) => r,
            other => panic!("{other:?}"),
        };
        let grant = ConfirmationGrant {
            token: req3.token,
            typed: "db13".into(),
        };
        let job = ask(Some(grant.clone())).await.unwrap();
        assert_eq!(
            wait_finished(&state, job).await.status,
            JobStatus::Succeeded
        );
        assert_eq!(dbsize(&state, session).await, 1);
        assert!(matches!(
            ask(Some(grant)).await,
            Err(Error::ConfirmationRequired(_))
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn delete_pattern_is_refused_on_read_only_connections() {
        if !enabled() {
            return;
        }
        let _serial = SERIAL.lock().await;
        let (state, _) = test_state(build_registry());
        let (session, db) = open(&state, None, true).await;
        let err = keys_delete_pattern_impl(&state, session, db, "*".into(), None)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), "readOnly");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn key_read_goes_through_the_session() {
        if !enabled() {
            return;
        }
        let _serial = SERIAL.lock().await;
        let (state, _) = test_state(build_registry());
        let (session, db) = open(&state, None, false).await;
        seed(&state, session, 1).await;
        let page = key_read_impl(
            &state,
            session,
            KeyReadRequest {
                key: db.child(ObjectKind::Key, "keep"),
                cursor: None,
                count: 10,
                pattern: None,
                max_string_bytes: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(page.key_type, "string");
        let tree = children_page_impl(
            &state,
            session,
            ChildrenPageRequest {
                parent: db,
                cursor: None,
                limit: 10,
                pattern: None,
            },
        )
        .await
        .unwrap();
        assert!(tree.nodes.iter().any(|n| n.name == "tmp:"));
    }

    /// Needs the real `redis_cli` classifier of `dbstudio-sql` (package A, milestone A1).
    #[tokio::test]
    #[ignore = "needs dbstudio-sql A1 (RedisCli classification); enable after merging feature/quandh/p2_sql"]
    async fn flushdb_on_production_needs_confirmation_and_read_only_blocks_writes() {
        use crate::guard::{ConfirmTokens, GuardInput, run_guard};
        let tokens = ConfirmTokens::new();
        let mut input = GuardInput {
            dialect: dbstudio_core::ScriptDialect::RedisCli,
            text: "FLUSHDB",
            params: &[],
            read_only: false,
            production: true,
            confirm_text: "db0",
            confirm: None,
            target_id: "session:x",
        };
        assert!(matches!(
            run_guard(&tokens, &input),
            Err(Error::ConfirmationRequired(_))
        ));
        input.production = false;
        input.read_only = true;
        input.text = "SET a 1";
        assert!(matches!(
            run_guard(&tokens, &input),
            Err(Error::ReadOnlyViolation { .. })
        ));
        input.text = "GET a";
        assert!(run_guard(&tokens, &input).is_ok());
    }
}
