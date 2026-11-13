//! `JobManager`: in-memory registry of background jobs (export, import, backup, ...).
//!
//! Jobs are not persisted (backup history lives in `backup_runs`); the last
//! [`KEEP_FINISHED`] finished jobs are kept. Every state change is published as a
//! [`JobEvent`] on `job://progress`. Package G owns this file.

#![allow(dead_code)] // scaffold: consumed by the export/backup packages

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use dbstudio_core::{
    CancelToken, Error, JobError, JobEvent, JobId, JobInfo, JobKind, JobProgress, JobReporter,
    JobStatus, Result,
};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::events::{EventEmitter, JOB_PROGRESS, emit_json};

/// Finished jobs kept in memory.
pub const KEEP_FINISHED: usize = 100;
/// Jobs that run at the same time; the others wait as `queued`.
pub const MAX_PARALLEL: usize = 3;
/// Minimum time between two progress events of one job.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default()
}

struct Entry {
    info: JobInfo,
    cancel: CancelToken,
    last_progress_event: Option<Instant>,
}

fn is_finished(status: JobStatus) -> bool {
    matches!(
        status,
        JobStatus::Succeeded | JobStatus::Failed | JobStatus::Cancelled
    )
}

/// Registry of jobs.
pub struct JobManager {
    events: Arc<dyn EventEmitter>,
    jobs: Mutex<HashMap<JobId, Entry>>,
    slots: Arc<tokio::sync::Semaphore>,
}

impl std::fmt::Debug for JobManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobManager")
            .field("jobs", &self.lock().len())
            .finish()
    }
}

/// Handed to the work of a spawned job; implements [`JobReporter`] for the engines.
pub struct JobContext {
    id: JobId,
    manager: Arc<JobManager>,
    cancel: CancelToken,
}

impl std::fmt::Debug for JobContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobContext").field("id", &self.id).finish()
    }
}

impl JobContext {
    /// Id of the job.
    pub fn id(&self) -> JobId {
        self.id
    }
}

impl JobReporter for JobContext {
    fn progress(&self, p: &JobProgress) {
        self.manager.progress(self.id, p.clone());
    }

    fn cancel_token(&self) -> &CancelToken {
        &self.cancel
    }
}

impl JobManager {
    /// Manager publishing through `events`.
    pub fn new(events: Arc<dyn EventEmitter>) -> Self {
        Self::with_limit(events, MAX_PARALLEL)
    }

    /// Manager running at most `limit` jobs at once.
    pub fn with_limit(events: Arc<dyn EventEmitter>, limit: usize) -> Self {
        JobManager {
            events,
            jobs: Mutex::new(HashMap::new()),
            slots: Arc::new(tokio::sync::Semaphore::new(limit.max(1))),
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<JobId, Entry>> {
        self.jobs.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn publish(&self, event: &JobEvent) {
        emit_json(&*self.events, JOB_PROGRESS, event);
    }

    /// Register a queued job.
    pub fn create(
        &self,
        kind: JobKind,
        title: impl Into<String>,
        connection_id: Option<Uuid>,
        cancellable: bool,
    ) -> (JobId, CancelToken) {
        let id = Uuid::new_v4();
        let cancel = CancelToken::new();
        let info = JobInfo {
            id,
            kind,
            title: title.into(),
            connection_id,
            status: JobStatus::Queued,
            progress: JobProgress::default(),
            created_at: now_rfc3339(),
            started_at: None,
            finished_at: None,
            error: None,
            result: None,
            cancellable,
        };
        self.lock().insert(
            id,
            Entry {
                info,
                cancel: cancel.clone(),
                last_progress_event: None,
            },
        );
        (id, cancel)
    }

    /// Queued -> running; publishes `started`.
    pub fn start(&self, id: JobId) {
        let info = {
            let mut jobs = self.lock();
            let Some(e) = jobs.get_mut(&id) else { return };
            if e.info.status != JobStatus::Queued {
                return;
            }
            e.info.status = JobStatus::Running;
            e.info.started_at = Some(now_rfc3339());
            e.info.clone()
        };
        self.publish(&JobEvent::Started { job: info });
    }

    /// Record progress; the event is throttled to one per [`PROGRESS_INTERVAL`] and job (the
    /// stored progress is always current).
    pub fn progress(&self, id: JobId, progress: JobProgress) {
        let emit = {
            let mut jobs = self.lock();
            let Some(e) = jobs.get_mut(&id) else { return };
            if e.info.status != JobStatus::Running {
                return;
            }
            e.info.progress = progress.clone();
            let due = e
                .last_progress_event
                .is_none_or(|t| t.elapsed() >= PROGRESS_INTERVAL);
            if due {
                e.last_progress_event = Some(Instant::now());
            }
            due
        };
        if emit {
            self.publish(&JobEvent::Progress {
                job_id: id,
                progress,
            });
        }
    }

    /// Finish a job. `Err(Cancelled)` (or a cancelled token with any error) ends as `Cancelled`.
    pub fn finish(&self, id: JobId, outcome: Result<Option<serde_json::Value>>) {
        let info = {
            let mut jobs = self.lock();
            let Some(e) = jobs.get_mut(&id) else { return };
            if is_finished(e.info.status) {
                return;
            }
            e.info.finished_at = Some(now_rfc3339());
            match outcome {
                Ok(result) => {
                    e.info.status = JobStatus::Succeeded;
                    e.info.result = result;
                }
                Err(err) => {
                    e.info.status = if matches!(err, Error::Cancelled) || e.cancel.is_cancelled() {
                        JobStatus::Cancelled
                    } else {
                        JobStatus::Failed
                    };
                    e.info.error = Some(JobError {
                        kind: err.kind().to_string(),
                        message: err.message(),
                    });
                }
            }
            let info = e.info.clone();
            Self::prune(&mut jobs);
            info
        };
        self.publish(&JobEvent::Finished { job: info });
    }

    /// Drop the oldest finished jobs beyond [`KEEP_FINISHED`].
    fn prune(jobs: &mut HashMap<JobId, Entry>) {
        let mut finished: Vec<(String, JobId)> = jobs
            .values()
            .filter(|e| is_finished(e.info.status))
            .map(|e| (e.info.finished_at.clone().unwrap_or_default(), e.info.id))
            .collect();
        if finished.len() <= KEEP_FINISHED {
            return;
        }
        finished.sort();
        for (_, id) in &finished[..finished.len() - KEEP_FINISHED] {
            jobs.remove(id);
        }
    }

    /// Create a job and run `work` on the tokio runtime once a slot is free (at most
    /// [`MAX_PARALLEL`] jobs run at once, the others stay `queued`; a queued job can be
    /// cancelled). The job finishes with whatever `work` returns.
    pub fn spawn<F, Fut>(
        self: &Arc<Self>,
        kind: JobKind,
        title: impl Into<String>,
        connection_id: Option<Uuid>,
        work: F,
    ) -> JobId
    where
        F: FnOnce(Arc<JobContext>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<Option<serde_json::Value>>> + Send + 'static,
    {
        let (id, cancel) = self.create(kind, title, connection_id, true);
        let ctx = Arc::new(JobContext {
            id,
            manager: Arc::clone(self),
            cancel,
        });
        let manager = Arc::clone(self);
        let slots = Arc::clone(&self.slots);
        tokio::spawn(async move {
            let permit = tokio::select! {
                p = slots.acquire_owned() => p.ok(),
                () = ctx.cancel.cancelled() => None,
            };
            let Some(permit) = permit else {
                manager.finish(id, Err(Error::Cancelled));
                return;
            };
            manager.start(id);
            let outcome = work(ctx).await;
            drop(permit);
            manager.finish(id, outcome);
        });
        id
    }

    /// All jobs, newest first.
    pub fn list(&self) -> Vec<JobInfo> {
        let mut all: Vec<JobInfo> = self.lock().values().map(|e| e.info.clone()).collect();
        all.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
        all
    }

    /// One job, or `NotFound`.
    pub fn get(&self, id: JobId) -> Result<JobInfo> {
        self.lock()
            .get(&id)
            .map(|e| e.info.clone())
            .ok_or_else(|| Error::NotFound(format!("job {id}")))
    }

    /// Ask a running job to stop (cooperative); finished jobs are left alone.
    pub fn cancel(&self, id: JobId) -> Result<()> {
        let jobs = self.lock();
        let e = jobs
            .get(&id)
            .ok_or_else(|| Error::NotFound(format!("job {id}")))?;
        if !is_finished(e.info.status) {
            e.cancel.cancel();
        }
        Ok(())
    }

    /// Forget finished jobs; returns how many were removed.
    pub fn clear_finished(&self) -> usize {
        let mut jobs = self.lock();
        let before = jobs.len();
        jobs.retain(|_, e| !is_finished(e.info.status));
        before - jobs.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventHub;

    fn manager() -> (Arc<JobManager>, Arc<EventHub>) {
        let hub = EventHub::new();
        (Arc::new(JobManager::new(hub.clone())), hub)
    }

    #[tokio::test]
    async fn lifecycle_publishes_events_and_lists_newest_first() {
        let (jobs, hub) = manager();
        let mut rx = hub.subscribe();
        let (a, _) = jobs.create(JobKind::Export, "a", None, true);
        let (b, _) = jobs.create(JobKind::Backup, "b", None, false);
        assert_eq!(jobs.get(a).unwrap().status, JobStatus::Queued);
        jobs.start(a);
        jobs.progress(
            a,
            JobProgress {
                phase: "data".into(),
                rows_done: 5,
                ..Default::default()
            },
        );
        assert_eq!(jobs.get(a).unwrap().progress.rows_done, 5);
        jobs.finish(a, Ok(Some(serde_json::json!({"rows": 5}))));
        let done = jobs.get(a).unwrap();
        assert_eq!(done.status, JobStatus::Succeeded);
        assert_eq!(done.result.unwrap()["rows"], 5);
        assert!(done.started_at.is_some() && done.finished_at.is_some());

        let types: Vec<String> = (0..3)
            .map(|_| {
                rx.try_recv().unwrap().payload["type"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(types, ["started", "progress", "finished"]);
        assert_eq!(jobs.list().len(), 2);
        assert!(jobs.list().iter().any(|j| j.id == b));
    }

    #[tokio::test]
    async fn progress_events_are_throttled() {
        let (jobs, hub) = manager();
        let mut rx = hub.subscribe();
        let (id, _) = jobs.create(JobKind::Export, "x", None, true);
        jobs.start(id);
        for rows in 0..50 {
            jobs.progress(
                id,
                JobProgress {
                    rows_done: rows,
                    ..Default::default()
                },
            );
        }
        let mut seen = 0;
        while rx.try_recv().is_ok() {
            seen += 1;
        }
        // `started` plus a single throttled progress event.
        assert_eq!(seen, 2);
        assert_eq!(jobs.get(id).unwrap().progress.rows_done, 49);
    }

    #[tokio::test]
    async fn cancel_and_failure_statuses() {
        let (jobs, _) = manager();
        let (a, token) = jobs.create(JobKind::Export, "a", None, true);
        jobs.start(a);
        jobs.cancel(a).unwrap();
        assert!(token.is_cancelled());
        jobs.finish(
            a,
            Err(Error::Query {
                message: "boom".into(),
                position: None,
            }),
        );
        // Cancellation requested: ends as cancelled even with another error.
        assert_eq!(jobs.get(a).unwrap().status, JobStatus::Cancelled);

        let (b, _) = jobs.create(JobKind::Import, "b", None, true);
        jobs.start(b);
        jobs.finish(b, Err(Error::Unsupported("import".into())));
        let failed = jobs.get(b).unwrap();
        assert_eq!(failed.status, JobStatus::Failed);
        assert_eq!(failed.error.unwrap().kind, "unsupported");
        // Finishing twice keeps the first outcome; unknown ids are NotFound.
        jobs.finish(b, Ok(None));
        assert_eq!(jobs.get(b).unwrap().status, JobStatus::Failed);
        assert!(matches!(
            jobs.cancel(Uuid::new_v4()),
            Err(Error::NotFound(_))
        ));
        assert!(matches!(jobs.get(Uuid::new_v4()), Err(Error::NotFound(_))));
    }

    #[tokio::test]
    async fn clear_finished_and_retention() {
        let (jobs, _) = manager();
        let (running, _) = jobs.create(JobKind::Export, "r", None, true);
        jobs.start(running);
        for i in 0..(KEEP_FINISHED + 5) {
            let (id, _) = jobs.create(JobKind::Export, format!("j{i}"), None, true);
            jobs.start(id);
            jobs.finish(id, Ok(None));
        }
        let finished = jobs.list().iter().filter(|j| is_finished(j.status)).count();
        assert_eq!(finished, KEEP_FINISHED);
        assert_eq!(jobs.clear_finished(), KEEP_FINISHED);
        let left = jobs.list();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id, running);
    }

    #[tokio::test]
    async fn parallel_limit_queues_and_queued_jobs_cancel() {
        let hub = EventHub::new();
        let jobs = Arc::new(JobManager::with_limit(hub, 1));
        let gate = Arc::new(tokio::sync::Notify::new());
        let g = gate.clone();
        let first = jobs.spawn(JobKind::Export, "first", None, move |_| async move {
            g.notified().await;
            Ok(None)
        });
        let second = jobs.spawn(JobKind::Export, "second", None, |_| async { Ok(None) });
        let third = jobs.spawn(JobKind::Export, "third", None, |_| async { Ok(None) });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(jobs.get(first).unwrap().status, JobStatus::Running);
        assert_eq!(jobs.get(second).unwrap().status, JobStatus::Queued);
        jobs.cancel(third).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(jobs.get(third).unwrap().status, JobStatus::Cancelled);
        gate.notify_one();
        for _ in 0..100 {
            if jobs.get(second).unwrap().status == JobStatus::Succeeded {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(jobs.get(first).unwrap().status, JobStatus::Succeeded);
        assert_eq!(jobs.get(second).unwrap().status, JobStatus::Succeeded);
    }

    #[tokio::test]
    async fn spawn_runs_work_and_reports() {
        let (jobs, _) = manager();
        let id = jobs.spawn(JobKind::Verify, "v", None, |ctx| async move {
            ctx.progress(&JobProgress {
                phase: "verify".into(),
                ..Default::default()
            });
            Ok(Some(serde_json::json!({"ok": true})))
        });
        for _ in 0..100 {
            if jobs.get(id).unwrap().status == JobStatus::Succeeded {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(jobs.get(id).unwrap().status, JobStatus::Succeeded);
    }
}
