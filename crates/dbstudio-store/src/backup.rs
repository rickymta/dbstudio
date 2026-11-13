//! Backup job and run records (`backup_jobs`, `backup_runs`). Package H owns this file.
//!
//! The typed `BackupJob` / `BackupRun` live in `dbstudio-backup` (which depends on this crate);
//! the store only holds their rows, with the structured parts as JSON text.

use dbstudio_core::Result;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::db::{Store, not_implemented};

/// A row of `backup_jobs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupJobRow {
    /// Job id.
    pub id: Uuid,
    /// Display name.
    pub name: String,
    /// Connection to back up.
    pub connection_id: Uuid,
    /// Whether the schedule is active.
    pub enabled: bool,
    /// `BackupSpec` JSON.
    pub spec: String,
    /// Cron expression; `None` = manual only.
    pub cron: Option<String>,
    /// IANA zone name or `local`.
    pub timezone: String,
    /// `Retention` JSON.
    pub retention: String,
    /// What to do about missed runs (`runOnce`, `skip`, ...).
    pub missed_policy: String,
    /// `NotifyPolicy` JSON.
    pub notify: String,
    /// True when the output is encrypted (passphrase in keychain account `backup:<id>`).
    pub encrypted: bool,
    /// ISO-8601 time the last scheduled run was meant for.
    pub last_scheduled_for: Option<String>,
    /// ISO-8601 time of the last run.
    pub last_run_at: Option<String>,
    /// ISO-8601 time of the next due run.
    pub next_run_at: Option<String>,
    /// ISO-8601 creation time.
    pub created_at: String,
    /// ISO-8601 last update time.
    pub updated_at: String,
}

/// A row of `backup_runs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupRunRow {
    /// Run id.
    pub id: Uuid,
    /// Job the run belongs to (cleared when the job is deleted).
    pub job_id: Option<Uuid>,
    /// Job name at the time of the run.
    pub job_name: String,
    /// Connection (cleared when the connection is deleted).
    pub connection_id: Option<Uuid>,
    /// `backup` | `restore` | `verify`.
    pub kind: String,
    /// `manual` | `schedule` | `catchUp` | `cli`.
    pub trigger: String,
    /// Engine used (`integrated`, `native`, ...).
    pub engine: String,
    /// `running` | `succeeded` | `failed` | `cancelled` | `interrupted`.
    pub status: String,
    /// ISO-8601 start time.
    pub started_at: String,
    /// ISO-8601 finish time.
    pub finished_at: Option<String>,
    /// Duration in milliseconds.
    pub duration_ms: Option<u64>,
    /// Output file.
    pub file_path: Option<String>,
    /// Output size in bytes.
    pub size_bytes: Option<u64>,
    /// Output checksum.
    pub checksum: Option<String>,
    /// Number of objects processed.
    pub objects: Option<u64>,
    /// Number of rows processed.
    pub rows: Option<u64>,
    /// Error classification.
    pub error_kind: Option<String>,
    /// Error message.
    pub error_message: Option<String>,
    /// Last 16 KB of the tool log.
    pub log_tail: Option<String>,
}

impl Store {
    /// All jobs ordered by name.
    pub fn backup_jobs_list(&self) -> Result<Vec<BackupJobRow>> {
        not_implemented("backup_jobs_list")
    }

    /// Insert or update (by id) a job.
    pub fn backup_job_save(&self, job: &BackupJobRow) -> Result<BackupJobRow> {
        let _ = job;
        not_implemented("backup_job_save")
    }

    /// Delete a job (its runs keep their records with `job_id` cleared).
    pub fn backup_job_delete(&self, id: Uuid) -> Result<()> {
        let _ = id;
        not_implemented("backup_job_delete")
    }

    /// Insert a run record.
    pub fn backup_run_insert(&self, run: &BackupRunRow) -> Result<()> {
        let _ = run;
        not_implemented("backup_run_insert")
    }

    /// Update a run record (progress, finish, log tail).
    pub fn backup_run_update(&self, run: &BackupRunRow) -> Result<()> {
        let _ = run;
        not_implemented("backup_run_update")
    }

    /// Runs newest first, optionally of one job, at most `limit`.
    pub fn backup_runs_list(&self, job_id: Option<Uuid>, limit: u32) -> Result<Vec<BackupRunRow>> {
        let _ = (job_id, limit);
        not_implemented("backup_runs_list")
    }

    /// Mark every `running` run as `interrupted` (after a crash); returns how many.
    pub fn backup_runs_mark_interrupted(&self) -> Result<u64> {
        not_implemented("backup_runs_mark_interrupted")
    }
}
