//! `Lease`: a dedicated physical connection for an editor tab or console.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    BulkInsertRequest, BulkInsertResult, Error, ExecSummary, ExplainRequest, QueryRequest, RawPlan,
    Result, ResultSink, TableStreamRequest,
};

/// Options of [`crate::Session::open_lease`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaseOptions {
    /// Initial database.
    #[serde(default)]
    pub database: Option<String>,
    /// Initial schema.
    #[serde(default)]
    pub schema: Option<String>,
    /// Start in auto-commit mode (default true).
    #[serde(default = "crate::script::yes")]
    pub auto_commit: bool,
}

impl Default for LeaseOptions {
    fn default() -> Self {
        LeaseOptions {
            database: None,
            schema: None,
            auto_commit: true,
        }
    }
}

/// Transaction isolation level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum IsolationLevel {
    /// `READ UNCOMMITTED`.
    ReadUncommitted,
    /// `READ COMMITTED`.
    ReadCommitted,
    /// `REPEATABLE READ`.
    RepeatableRead,
    /// `SERIALIZABLE`.
    Serializable,
    /// SQL Server `SNAPSHOT`.
    Snapshot,
}

/// Options of [`Lease::begin`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TxOptions {
    /// `READ ONLY` transaction.
    #[serde(default)]
    pub read_only: bool,
    /// Isolation level; server default when `None`.
    #[serde(default)]
    pub isolation: Option<IsolationLevel>,
}

/// Transaction state of a lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TxStatus {
    /// No open transaction.
    Idle,
    /// Transaction open.
    Active,
    /// PostgreSQL "aborted": only `ROLLBACK` works.
    Failed,
    /// Cannot be determined.
    Unknown,
}

/// Cheap snapshot of a lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaseState {
    /// Auto-commit mode.
    pub auto_commit: bool,
    /// Transaction state.
    pub transaction: TxStatus,
    /// Query id currently executing.
    pub running: Option<Uuid>,
    /// Query id whose last result set can `fetch_more`.
    pub pending_result: Option<Uuid>,
    /// Current database.
    pub database: Option<String>,
    /// Current schema.
    pub schema: Option<String>,
    /// Connection lost; only `close()` is valid.
    pub broken: bool,
}

impl LeaseState {
    /// State of a freshly opened lease.
    pub fn initial(opts: &LeaseOptions) -> Self {
        LeaseState {
            auto_commit: opts.auto_commit,
            transaction: TxStatus::Idle,
            running: None,
            pending_result: None,
            database: opts.database.clone(),
            schema: opts.schema.clone(),
            broken: false,
        }
    }
}

/// Argument of [`Lease::set_context`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaseContext {
    /// Database to switch to.
    #[serde(default)]
    pub database: Option<String>,
    /// Schema to switch to.
    #[serde(default)]
    pub schema: Option<String>,
}

/// Outcome of [`Lease::close`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaseCloseReport {
    /// An open transaction was rolled back.
    pub rolled_back: bool,
    /// Steps timed out and the socket was closed forcibly.
    pub forced: bool,
}

/// A dedicated physical connection (outside the pool) with the TLS/SSH/read-only settings of its
/// session.
///
/// See the design document (section 1.6.3) for the mandatory semantics: busy handling, pending
/// results, cancellation, close ordering and loss of connection.
#[async_trait]
pub trait Lease: Send + Sync {
    /// Run `req` on this connection. `req.database` is ignored (use [`Lease::set_context`]).
    /// One statement batch at a time: a concurrent call fails with [`Error::Busy`]. When the LAST
    /// result set of the request hits `max_rows` and more rows exist, the result stays open
    /// (`state.pending_result = Some(query_id)`) until `fetch_more`, `discard_result`, the next
    /// execute/begin/commit/rollback/set_*, or close.
    async fn execute(&self, req: QueryRequest, sink: &mut dyn ResultSink) -> Result<ExecSummary>;

    /// Continue the pending result: `on_rows*` then `on_statement_end` (`row_count` = rows of
    /// this call, `truncated` = more remain). No `on_columns`. Wrong id => [`Error::NotFound`].
    async fn fetch_more(
        &self,
        query_id: Uuid,
        max_rows: u64,
        batch_size: Option<u32>,
        sink: &mut dyn ResultSink,
    ) -> Result<ExecSummary>;

    /// Drop the pending result set, if any.
    async fn discard_result(&self) -> Result<()>;

    /// Cancel only if `query_id` is the running statement (a late cancel never hits the next
    /// one). Unknown or finished id => `Ok(())`.
    async fn cancel(&self, query_id: Uuid) -> Result<()>;

    /// Start a transaction.
    async fn begin(&self, opts: TxOptions) -> Result<()>;

    /// Commit the open transaction.
    async fn commit(&self) -> Result<()>;

    /// Roll back the open transaction.
    async fn rollback(&self) -> Result<()>;

    /// Switching to auto-commit with an open transaction => [`Error::Busy`] (the UI asks first).
    async fn set_auto_commit(&self, on: bool) -> Result<()>;

    /// `USE db` / `SET search_path` / `ALTER SESSION SET CURRENT_SCHEMA` / `SELECT n` (Redis).
    async fn set_context(&self, ctx: LeaseContext) -> Result<()>;

    /// Cheap cached snapshot, never blocks.
    fn state(&self) -> LeaseState;

    /// Lexer dialect of THIS connection when it can differ from the session's (MySQL
    /// `sql_mode`: `ANSI_QUOTES`, `NO_BACKSLASH_ESCAPES` are per connection). The guard and
    /// the explain checks of a lease must split text exactly like the lease will, so they use
    /// this and fall back to [`crate::Session::script_dialect`] on `None` (the default).
    fn script_dialect(&self) -> Option<crate::ScriptDialect> {
        None
    }

    /// Check that the connection is alive.
    async fn ping(&self) -> Result<()>;

    /// Explain a statement on this connection.
    async fn explain(&self, req: ExplainRequest) -> Result<RawPlan> {
        let _ = req;
        Err(Error::Unsupported("explain".into()))
    }

    /// Stream a whole table (export/backup) on this connection, within its transaction.
    async fn stream_table(
        &self,
        req: TableStreamRequest,
        sink: &mut dyn ResultSink,
    ) -> Result<ExecSummary> {
        let _ = (req, sink);
        Err(Error::Unsupported("table_stream".into()))
    }

    /// Insert rows inside the current transaction of the lease (no auto-commit).
    async fn bulk_insert(&self, req: BulkInsertRequest) -> Result<BulkInsertResult> {
        let _ = req;
        Err(Error::Unsupported("bulk_insert".into()))
    }

    /// Cancel running work, roll back an open transaction, close the connection. Returns whether
    /// a transaction was rolled back. Idempotent.
    async fn close(&self) -> Result<LeaseCloseReport>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn lease_state_shape() {
        let s = LeaseState {
            auto_commit: false,
            transaction: TxStatus::Failed,
            running: Some(Uuid::nil()),
            pending_result: None,
            database: Some("app".into()),
            schema: None,
            broken: false,
        };
        let json = serde_json::to_value(&s).unwrap();
        assert_eq!(
            json,
            json!({"autoCommit": false, "transaction": "failed",
                   "running": "00000000-0000-0000-0000-000000000000", "pendingResult": null,
                   "database": "app", "schema": null, "broken": false})
        );
        assert_eq!(serde_json::from_value::<LeaseState>(json).unwrap(), s);
    }

    #[test]
    fn options_defaults() {
        let o: LeaseOptions = serde_json::from_value(json!({})).unwrap();
        assert!(o.auto_commit && o.database.is_none());
        assert_eq!(o, LeaseOptions::default());
        let state = LeaseState::initial(&LeaseOptions {
            database: Some("d".into()),
            schema: Some("s".into()),
            auto_commit: false,
        });
        assert!(!state.auto_commit && state.transaction == TxStatus::Idle);
        let t: TxOptions = serde_json::from_value(json!({})).unwrap();
        assert_eq!(t, TxOptions::default());
        let t = TxOptions {
            read_only: true,
            isolation: Some(IsolationLevel::RepeatableRead),
        };
        let json = serde_json::to_value(t).unwrap();
        assert_eq!(
            json,
            json!({"readOnly": true, "isolation": "repeatableRead"})
        );
        assert_eq!(serde_json::from_value::<TxOptions>(json).unwrap(), t);
    }

    #[test]
    fn close_report_and_context() {
        assert_eq!(
            serde_json::to_value(LeaseCloseReport {
                rolled_back: true,
                forced: false
            })
            .unwrap(),
            json!({"rolledBack": true, "forced": false})
        );
        let c: LeaseContext = serde_json::from_value(json!({"schema": "s"})).unwrap();
        assert_eq!(c.schema.as_deref(), Some("s"));
        assert!(c.database.is_none());
        for (level, text) in [
            (IsolationLevel::ReadUncommitted, "readUncommitted"),
            (IsolationLevel::ReadCommitted, "readCommitted"),
            (IsolationLevel::Serializable, "serializable"),
            (IsolationLevel::Snapshot, "snapshot"),
        ] {
            assert_eq!(serde_json::to_value(level).unwrap(), text);
        }
        assert_eq!(serde_json::to_value(TxStatus::Unknown).unwrap(), "unknown");
    }
}
