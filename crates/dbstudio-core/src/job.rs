//! Background jobs (export, import, backup...): kinds, progress and cooperative cancellation.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use uuid::Uuid;

/// Identifier of a job.
pub type JobId = Uuid;

/// What a job does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum JobKind {
    /// Export of data or structure.
    Export,
    /// Import of a file.
    Import,
    /// Execution of a SQL script file.
    SqlScript,
    /// Backup.
    Backup,
    /// Restore.
    Restore,
    /// Backup verification.
    Verify,
    /// Redis delete by pattern.
    KeyDelete,
}

/// Lifecycle state of a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum JobStatus {
    /// Waiting for a slot.
    Queued,
    /// Running.
    Running,
    /// Finished successfully.
    Succeeded,
    /// Finished with an error.
    Failed,
    /// Cancelled by the user.
    Cancelled,
}

/// Progress of a job.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobProgress {
    /// Stable i18n key: `schema`, `data`, `compress`, `encrypt`, `verify`, `restore`.
    pub phase: String,
    /// Object being processed (`public.orders`).
    pub object: Option<String>,
    /// Objects finished.
    pub objects_done: u32,
    /// Objects in total.
    pub objects_total: Option<u32>,
    /// Rows processed.
    pub rows_done: u64,
    /// Estimated rows in total.
    pub rows_total: Option<u64>,
    /// Bytes processed.
    pub bytes_done: u64,
    /// Bytes in total, when known.
    pub bytes_total: Option<u64>,
    /// Free text.
    pub message: Option<String>,
}

/// Error of a failed job (from [`crate::Error::kind`] / [`crate::Error::message`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobError {
    /// Error kind.
    pub kind: String,
    /// Error message.
    pub message: String,
}

/// Snapshot of a job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobInfo {
    /// Job id.
    pub id: JobId,
    /// Kind.
    pub kind: JobKind,
    /// Human readable title, e.g. `Export app -> orders.xlsx`.
    pub title: String,
    /// Connection the job works on.
    pub connection_id: Option<Uuid>,
    /// Status.
    pub status: JobStatus,
    /// Progress.
    pub progress: JobProgress,
    /// RFC 3339 creation time.
    pub created_at: String,
    /// RFC 3339 start time.
    pub started_at: Option<String>,
    /// RFC 3339 end time.
    pub finished_at: Option<String>,
    /// Error when failed.
    pub error: Option<JobError>,
    /// Kind-specific outcome, e.g. `{"path": "...", "sizeBytes": 123, "rows": 10}`.
    pub result: Option<serde_json::Value>,
    /// The job can be cancelled.
    pub cancellable: bool,
}

/// Payload of the `job://progress` event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum JobEvent {
    /// The job started.
    Started {
        /// Snapshot.
        job: JobInfo,
    },
    /// Progress update (throttled).
    #[serde(rename_all = "camelCase")]
    Progress {
        /// Job id.
        job_id: JobId,
        /// New progress.
        progress: JobProgress,
    },
    /// The job finished (any final status).
    Finished {
        /// Snapshot.
        job: JobInfo,
    },
}

/// Cooperative cancellation flag shared by a job and the engine.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>, Arc<Notify>);

impl CancelToken {
    /// New, not cancelled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Request cancellation; wakes every [`CancelToken::cancelled`] waiter.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
        self.1.notify_waiters();
    }

    /// Whether cancellation was requested.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// Resolves once cancellation is requested (immediately if it already was).
    pub async fn cancelled(&self) {
        loop {
            // Created before the flag check so a concurrent `cancel` cannot be missed.
            let notified = self.1.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

/// What engines (export/backup) use to report.
pub trait JobReporter: Send + Sync {
    /// Report progress (throttled by the manager to at most 4/s per job).
    fn progress(&self, p: &JobProgress);
    /// A log line (tail kept for `backup_runs.log`).
    fn log(&self, line: &str) {
        let _ = line;
    }
    /// The job's cancellation token.
    fn cancel_token(&self) -> &CancelToken;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn info() -> JobInfo {
        JobInfo {
            id: Uuid::nil(),
            kind: JobKind::SqlScript,
            title: "Export app".into(),
            connection_id: None,
            status: JobStatus::Succeeded,
            progress: JobProgress::default(),
            created_at: "2026-11-13T00:00:00Z".into(),
            started_at: Some("2026-11-13T00:00:01Z".into()),
            finished_at: None,
            error: Some(JobError {
                kind: "query".into(),
                message: "boom".into(),
            }),
            result: Some(json!({"path": "/tmp/x", "rows": 1})),
            cancellable: false,
        }
    }

    #[test]
    fn progress_event_matches_documented_example() {
        let e = JobEvent::Progress {
            job_id: Uuid::nil(),
            progress: JobProgress {
                phase: "data".into(),
                object: Some("public.orders".into()),
                objects_done: 3,
                objects_total: Some(12),
                rows_done: 150_000,
                rows_total: Some(1_000_000),
                bytes_done: 20_971_520,
                bytes_total: None,
                message: None,
            },
        };
        let json = serde_json::to_value(&e).unwrap();
        assert_eq!(
            json,
            json!({"type": "progress", "jobId": "00000000-0000-0000-0000-000000000000",
                   "progress": {"phase": "data", "object": "public.orders", "objectsDone": 3,
                                "objectsTotal": 12, "rowsDone": 150000, "rowsTotal": 1000000,
                                "bytesDone": 20971520, "bytesTotal": null, "message": null}})
        );
        assert_eq!(serde_json::from_value::<JobEvent>(json).unwrap(), e);
    }

    #[test]
    fn info_and_started_finished() {
        let i = info();
        let json = serde_json::to_value(&i).unwrap();
        assert_eq!(json["kind"], "sqlScript");
        assert_eq!(json["status"], "succeeded");
        assert_eq!(json["createdAt"], "2026-11-13T00:00:00Z");
        assert_eq!(json["error"], json!({"kind": "query", "message": "boom"}));
        assert_eq!(serde_json::from_value::<JobInfo>(json).unwrap(), i);
        for e in [
            JobEvent::Started { job: i.clone() },
            JobEvent::Finished { job: i },
        ] {
            let v = serde_json::to_value(&e).unwrap();
            assert!(v["type"] == "started" || v["type"] == "finished");
            assert_eq!(serde_json::from_value::<JobEvent>(v).unwrap(), e);
        }
        for (k, s) in [
            (JobKind::KeyDelete, "keyDelete"),
            (JobKind::Verify, "verify"),
            (JobKind::Export, "export"),
        ] {
            assert_eq!(serde_json::to_value(k).unwrap(), s);
        }
        assert_eq!(
            serde_json::to_value(JobStatus::Cancelled).unwrap(),
            "cancelled"
        );
    }

    #[tokio::test]
    async fn cancel_token_wakes_waiters() {
        let t = CancelToken::new();
        assert!(!t.is_cancelled());
        let waiter = {
            let t = t.clone();
            tokio::spawn(async move { t.cancelled().await })
        };
        tokio::task::yield_now().await;
        t.cancel();
        waiter.await.unwrap();
        assert!(t.is_cancelled());
        // Already cancelled: resolves immediately.
        t.cancelled().await;
    }

    struct Reporter(CancelToken);
    impl JobReporter for Reporter {
        fn progress(&self, _: &JobProgress) {}
        fn cancel_token(&self) -> &CancelToken {
            &self.0
        }
    }

    #[test]
    fn reporter_default_log_is_noop() {
        let r = Reporter(CancelToken::new());
        r.log("line");
        r.progress(&JobProgress::default());
        assert!(!r.cancel_token().is_cancelled());
    }
}
