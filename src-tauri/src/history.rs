//! Query history recording around `query_execute` / `lease_execute`.
//!
//! An entry is written as `running` before the driver starts (so a crash leaves a trace) and
//! finished with the outcome afterwards; the history of the connection is then trimmed to
//! `AppSettings::history_max_per_connection`. The text is the one the user typed (before
//! parameter binding, so parameter values never reach the history) with secrets redacted.
//! Recording never fails a query: store errors are only logged.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use dbstudio_core::{
    ConnectionConfig, EndOutcome, Error, ExecSummary, QueryEvent, Result, ScriptAnalysis,
    ScriptDialect, StatementClass,
};
use dbstudio_store::{ConnectionService, NewHistoryEntry};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::state::AppState;

/// History entry of a running request; finish it with [`Tracker::finish`]. Dropped unfinished
/// (the request future was abandoned), the entry ends as `cancelled`.
pub struct Tracker {
    svc: Arc<ConnectionService>,
    entry_id: i64,
    connection_id: uuid::Uuid,
    keep: u32,
    started: Instant,
    finished: bool,
}

/// Counts the rows a request returned or affected, for the history row. Clones share the count
/// (one goes into the event closure, the other is read afterwards).
#[derive(Debug, Default, Clone)]
pub struct RowCounter {
    rows: Arc<AtomicU64>,
}

impl RowCounter {
    /// Account for an event streamed to the frontend.
    pub fn observe(&self, event: &QueryEvent) {
        if let QueryEvent::StatementEnd { end } = event {
            match end.outcome {
                EndOutcome::Rows { row_count, .. } => {
                    self.rows.fetch_add(row_count, Ordering::Relaxed);
                }
                EndOutcome::Affected { rows } => {
                    self.rows.fetch_add(rows, Ordering::Relaxed);
                }
                EndOutcome::Done => {}
            }
        }
    }

    /// Rows returned or affected so far.
    pub fn rows(&self) -> u64 {
        self.rows.load(Ordering::Relaxed)
    }
}

fn now_iso() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

fn kind_of(analysis: &ScriptAnalysis) -> &'static str {
    if analysis.statements.len() > 1 {
        "script"
    } else if analysis
        .statements
        .first()
        .is_some_and(|s| matches!(s.class, StatementClass::Explain { .. }))
    {
        "explain"
    } else {
        "query"
    }
}

fn outcome_of(result: &Result<ExecSummary>) -> (&'static str, Option<String>) {
    match result {
        Ok(_) => ("ok", None),
        Err(Error::Cancelled) => ("cancelled", None),
        Err(Error::Timeout) => ("error", Some("the query timed out".to_string())),
        Err(e) => ("error", Some(e.to_string())),
    }
}

impl Tracker {
    /// Start recording `text` (as typed) for the connection of `config`. `None` when history is
    /// off, the connection is not a saved one, or the store failed.
    pub async fn start(
        state: &AppState,
        config: &ConnectionConfig,
        database: Option<String>,
        dialect: ScriptDialect,
        text: &str,
        analysis: &ScriptAnalysis,
    ) -> Option<Tracker> {
        let svc = state.connections.clone();
        let settings = match svc.settings() {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "history: cannot read settings");
                return None;
            }
        };
        if !settings.history_enabled {
            return None;
        }
        let entry = NewHistoryEntry {
            connection_id: config.id,
            database: database.or_else(|| config.database.clone()),
            kind: kind_of(analysis).to_string(),
            text: dbstudio_sql::redact::redact(dialect, text),
            started_at: now_iso(),
            status: "running".to_string(),
            elapsed_ms: None,
            statements: u32::try_from(analysis.statements.len()).ok(),
            rows: None,
            error_message: None,
        };
        let connection_id = config.id;
        let keep = settings.history_max_per_connection;
        let store_svc = svc.clone();
        let appended =
            tokio::task::spawn_blocking(move || store_svc.store().history_append(&entry))
                .await
                .map_err(|e| Error::Other(e.to_string()))
                .and_then(|r| r);
        match appended {
            Ok(row) => Some(Tracker {
                svc,
                entry_id: row.id,
                connection_id,
                keep,
                started: Instant::now(),
                finished: false,
            }),
            // Not a saved connection (or a store failure): nothing to record.
            Err(e) => {
                tracing::debug!(error = %e, "history: entry not recorded");
                None
            }
        }
    }

    /// Id of the history row.
    pub fn id(&self) -> i64 {
        self.entry_id
    }

    /// Record the outcome and trim the connection's history.
    pub async fn finish(mut self, result: &Result<ExecSummary>, rows: u64) {
        self.finished = true;
        let (status, message) = outcome_of(result);
        let statements = result.as_ref().ok().map(|s| s.statements);
        let elapsed = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let svc = self.svc.clone();
        let (id, connection_id, keep) = (self.entry_id, self.connection_id, self.keep);
        let done = tokio::task::spawn_blocking(move || -> Result<()> {
            let store = svc.store();
            store.history_finish(
                id,
                status,
                Some(elapsed),
                statements,
                Some(rows),
                message.as_deref(),
            )?;
            store.history_trim(connection_id, keep)?;
            Ok(())
        })
        .await;
        match done {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!(error = %e, "history: cannot finish entry"),
            Err(e) => tracing::warn!(error = %e, "history: finish task failed"),
        }
    }
}

impl Drop for Tracker {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        // The request future was dropped before it finished: do not leave `running` behind.
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let svc = self.svc.clone();
        let (id, elapsed) = (
            self.entry_id,
            u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
        );
        handle.spawn_blocking(move || {
            let _ = svc
                .store()
                .history_finish(id, "cancelled", Some(elapsed), None, None, None);
        });
    }
}

#[cfg(test)]
mod tests {
    use dbstudio_core::{
        ConnectionConfig, DbKind, ExecSummary, QueryEvent, ScriptAnalysis, ScriptDialect,
        StatementEnd,
    };
    use dbstudio_store::{ConnectionProfile, HistoryQuery, SecretChange};

    use super::*;
    use crate::commands::connections;
    use crate::state::testing::test_state;
    use crate::state::{AppState, build_registry};

    fn analysis() -> ScriptAnalysis {
        dbstudio_sql::analyze(ScriptDialect::Postgres, "select 1")
    }

    async fn saved_connection(state: &AppState) -> ConnectionConfig {
        let p = ConnectionProfile {
            config: ConnectionConfig::new(DbKind::Postgres, "h", "localhost"),
            group_id: None,
            color: None,
            save_password: false,
            has_password: false,
            sort_order: 0,
            created_at: String::new(),
            updated_at: String::new(),
        };
        connections::save_impl(state, p, SecretChange::Keep)
            .await
            .unwrap()
            .config
    }

    fn list(state: &AppState) -> Vec<dbstudio_store::HistoryEntry> {
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
    async fn entry_runs_then_finishes_with_counts() {
        let (state, _) = test_state(build_registry());
        let cfg = saved_connection(&state).await;
        let t = Tracker::start(
            &state,
            &cfg,
            Some("app".into()),
            ScriptDialect::Postgres,
            "select 1",
            &analysis(),
        )
        .await
        .unwrap();
        assert_eq!(list(&state)[0].status, "running");
        let rows = RowCounter::default();
        rows.observe(&QueryEvent::StatementEnd {
            end: StatementEnd {
                outcome: EndOutcome::Rows {
                    row_count: 5,
                    truncated: false,
                },
                elapsed_ms: 1,
            },
        });
        rows.observe(&QueryEvent::StatementEnd {
            end: StatementEnd {
                outcome: EndOutcome::Affected { rows: 2 },
                elapsed_ms: 1,
            },
        });
        let id = t.id();
        t.finish(
            &Ok(ExecSummary {
                statements: 2,
                elapsed_ms: 3,
            }),
            rows.rows(),
        )
        .await;
        let e = &list(&state)[0];
        assert_eq!((e.id, e.status.as_str()), (id, "ok"));
        assert_eq!((e.rows, e.statements), (Some(7), Some(2)));
        assert_eq!(e.database.as_deref(), Some("app"));
        assert!(e.elapsed_ms.is_some() && e.error_message.is_none());
    }

    #[tokio::test]
    async fn errors_and_cancellation_are_recorded() {
        let (state, _) = test_state(build_registry());
        let cfg = saved_connection(&state).await;
        for (result, status, message) in [
            (
                Err(Error::query("syntax error")),
                "error",
                Some("syntax error"),
            ),
            (Err(Error::Cancelled), "cancelled", None),
            (Err(Error::Timeout), "error", Some("the query timed out")),
        ] {
            let t = Tracker::start(
                &state,
                &cfg,
                None,
                ScriptDialect::Postgres,
                "x",
                &analysis(),
            )
            .await
            .unwrap();
            t.finish(&result, 0).await;
            let e = &list(&state)[0];
            assert_eq!(e.status, status);
            assert_eq!(e.error_message.as_deref(), message);
        }
    }

    #[tokio::test]
    async fn dropped_tracker_ends_as_cancelled() {
        let (state, _) = test_state(build_registry());
        let cfg = saved_connection(&state).await;
        let t = Tracker::start(
            &state,
            &cfg,
            None,
            ScriptDialect::Postgres,
            "x",
            &analysis(),
        )
        .await
        .unwrap();
        drop(t);
        for _ in 0..50 {
            if list(&state)[0].status == "cancelled" {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("entry still running");
    }

    #[tokio::test]
    async fn disabled_history_and_unsaved_connections_record_nothing() {
        let (state, _) = test_state(build_registry());
        let cfg = saved_connection(&state).await;
        let mut settings = state.connections.settings().unwrap();
        settings.history_enabled = false;
        state.connections.set_settings(settings).unwrap();
        assert!(
            Tracker::start(
                &state,
                &cfg,
                None,
                ScriptDialect::Postgres,
                "x",
                &analysis()
            )
            .await
            .is_none()
        );
        settings.history_enabled = true;
        state.connections.set_settings(settings).unwrap();
        let unsaved = ConnectionConfig::new(DbKind::Postgres, "tmp", "localhost");
        assert!(
            Tracker::start(
                &state,
                &unsaved,
                None,
                ScriptDialect::Postgres,
                "x",
                &analysis()
            )
            .await
            .is_none()
        );
        assert!(list(&state).is_empty());
    }

    #[tokio::test]
    async fn history_is_trimmed_to_the_configured_size() {
        let (state, _) = test_state(build_registry());
        let cfg = saved_connection(&state).await;
        let mut settings = state.connections.settings().unwrap();
        settings.history_max_per_connection = 100;
        state.connections.set_settings(settings).unwrap();
        for i in 0..104 {
            let t = Tracker::start(
                &state,
                &cfg,
                None,
                ScriptDialect::Postgres,
                &format!("select {i}"),
                &analysis(),
            )
            .await
            .unwrap();
            t.finish(
                &Ok(ExecSummary {
                    statements: 1,
                    elapsed_ms: 0,
                }),
                0,
            )
            .await;
        }
        let kept = list(&state);
        assert_eq!(kept.len(), 100);
        assert_eq!(kept[0].text, "select 103");
        assert_eq!(kept[99].text, "select 4");
    }
}
