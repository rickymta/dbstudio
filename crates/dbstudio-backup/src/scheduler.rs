//! Pure scheduling logic. The tick loop, locking and notifications live in `src-tauri`.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::BackupJob;

/// Source of the current time (fake in tests).
pub trait Clock {
    /// Now, in UTC.
    fn now(&self) -> DateTime<Utc>;
}

/// The system clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// Decides which jobs are due. Scaffold: nothing is ever due.
#[derive(Debug, Clone)]
pub struct Scheduler<C: Clock> {
    clock: C,
}

impl<C: Clock> Scheduler<C> {
    /// Scheduler reading time from `clock`.
    pub fn new(clock: C) -> Self {
        Scheduler { clock }
    }

    /// The clock in use.
    pub fn clock(&self) -> &C {
        &self.clock
    }

    /// First scheduled instant after `last_scheduled_for` (or after now when `None`).
    pub fn next_due(
        &self,
        job: &BackupJob,
        last_scheduled_for: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        let _ = (job, last_scheduled_for);
        None
    }

    /// Ids of enabled jobs with a scheduled instant in `(last_scheduled_for, now]`.
    pub fn due_jobs(
        &self,
        jobs: &[BackupJob],
        last_scheduled_for: &BTreeMap<Uuid, DateTime<Utc>>,
    ) -> Vec<Uuid> {
        let _ = (jobs, last_scheduled_for);
        Vec::new()
    }

    /// Ids of jobs that missed a slot and whose policy is `RunOnce` (called at start-up).
    pub fn catch_up(
        &self,
        jobs: &[BackupJob],
        last_scheduled_for: &BTreeMap<Uuid, DateTime<Utc>>,
    ) -> Vec<Uuid> {
        let _ = (jobs, last_scheduled_for);
        Vec::new()
    }
}
