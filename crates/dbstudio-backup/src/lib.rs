//! Backup / restore engine, `.dbsbak` container and scheduler logic.
//!
//! A backup is a `.dbsbak` container ([`container`]) produced either by the integrated engine
//! ([`logical`]: DDL and `INSERT` scripts written through the session) or by a native tool
//! ([`native`]). [`runner`] ties a saved job to the store (run records, locks, retention).

use std::collections::BTreeMap;
use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;

use dbstudio_core::{
    ConnectionConfig, DdlOptions, JobError, JobReporter, ObjectKind, ObjectPath, Secret, Session,
};
use dbstudio_export::{ExportContent, RowError};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub mod container;
pub mod crypto;
pub mod logical;
pub mod native;
pub mod runner;
pub mod scheduler;

pub use chrono;
pub use native::detect_tools;
pub use scheduler::{
    Clock, Scheduler, SystemClock, next_after, parse_cron, parse_tz, preview_from,
};

// ---------------------------------------------------------------------------------------------
// Specs
// ---------------------------------------------------------------------------------------------

fn content_both() -> ExportContent {
    ExportContent::Both
}

fn default_template() -> String {
    "{connection}-{database}-{date:%Y%m%d-%H%M%S}.dbsbak".to_string()
}

fn yes() -> bool {
    true
}

/// What to back up.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupSpec {
    /// Connection to back up.
    pub connection_id: Uuid,
    /// Databases or objects.
    pub targets: Vec<ObjectPath>,
    /// Logical (portable `.dbsbak`) or native tool.
    pub engine: BackupEngine,
    /// Payload compression.
    #[serde(default)]
    pub compression: Compression,
    /// Encrypt with a passphrase.
    #[serde(default)]
    pub encrypted: bool,
    /// Where the file goes.
    pub destination: Destination,
}

/// How the backup is produced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum BackupEngine {
    /// Integrated `.dbsbak` made with the export writers.
    Logical {
        /// Schema, data or both.
        #[serde(default = "content_both")]
        content: ExportContent,
        /// DDL options.
        #[serde(default)]
        ddl: DdlOptions,
    },
    /// External native tool.
    Native {
        /// Which tool.
        tool: NativeTool,
        /// Allow-listed options per tool.
        #[serde(default)]
        options: BTreeMap<String, String>,
    },
}

/// Native tools DBStudio can drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NativeTool {
    /// `pg_dump`.
    PgDump,
    /// `pg_restore`.
    PgRestore,
    /// `psql`.
    Psql,
    /// `mysqldump`.
    MySqlDump,
    /// `mysql`.
    MySqlClient,
    /// `mongodump`.
    MongoDump,
    /// `mongorestore`.
    MongoRestore,
    /// Oracle Data Pump export.
    Expdp,
    /// Oracle Data Pump import.
    Impdp,
    /// `redis-cli`.
    RedisCli,
    /// T-SQL `BACKUP DATABASE` (no binary).
    MsSqlServerBackup,
    /// ClickHouse `BACKUP ... TO Disk(...)` run by the server (no binary).
    ClickHouseServerBackup,
    /// CockroachDB `BACKUP ... INTO 'nodelocal://...'` run by the cluster (no binary).
    CockroachServerBackup,
    /// YugabyteDB `ysql_dump`.
    YsqlDump,
    /// YugabyteDB `ysqlsh`.
    Ysqlsh,
}

impl NativeTool {
    /// Every tool.
    pub const ALL: [NativeTool; 15] = [
        NativeTool::PgDump,
        NativeTool::PgRestore,
        NativeTool::Psql,
        NativeTool::MySqlDump,
        NativeTool::MySqlClient,
        NativeTool::MongoDump,
        NativeTool::MongoRestore,
        NativeTool::Expdp,
        NativeTool::Impdp,
        NativeTool::RedisCli,
        NativeTool::MsSqlServerBackup,
        NativeTool::ClickHouseServerBackup,
        NativeTool::CockroachServerBackup,
        NativeTool::YsqlDump,
        NativeTool::Ysqlsh,
    ];

    /// Executable name, `None` for the T-SQL engine.
    pub fn binary(self) -> Option<&'static str> {
        Some(match self {
            NativeTool::PgDump => "pg_dump",
            NativeTool::PgRestore => "pg_restore",
            NativeTool::Psql => "psql",
            NativeTool::MySqlDump => "mysqldump",
            NativeTool::MySqlClient => "mysql",
            NativeTool::MongoDump => "mongodump",
            NativeTool::MongoRestore => "mongorestore",
            NativeTool::Expdp => "expdp",
            NativeTool::Impdp => "impdp",
            NativeTool::RedisCli => "redis-cli",
            NativeTool::YsqlDump => "ysql_dump",
            NativeTool::Ysqlsh => "ysqlsh",
            NativeTool::MsSqlServerBackup
            | NativeTool::ClickHouseServerBackup
            | NativeTool::CockroachServerBackup => return None,
        })
    }
}

/// Payload compression.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Compression {
    /// Stored.
    None,
    /// gzip.
    Gzip {
        /// 0-9.
        level: u32,
    },
    /// zstd.
    Zstd {
        /// zstd level.
        level: i32,
    },
}

impl Default for Compression {
    fn default() -> Self {
        Compression::Zstd { level: 3 }
    }
}

/// Where a backup file is written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Destination {
    /// Directory on this machine. `file_template` placeholders: `{connection}`, `{database}`,
    /// `{date:%Y%m%d-%H%M%S}`, `{job}`.
    #[serde(rename_all = "camelCase")]
    LocalDir {
        /// Directory.
        path: String,
        /// File name template.
        #[serde(default = "default_template")]
        file_template: String,
    },
    /// Directory on the SERVER (MSSQL `BACKUP DATABASE`, Oracle `DIRECTORY`).
    ServerDir {
        /// Directory.
        path: String,
    },
}

// ---------------------------------------------------------------------------------------------
// Jobs and runs
// ---------------------------------------------------------------------------------------------

/// A saved, optionally scheduled backup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupJob {
    /// Job id.
    pub id: Uuid,
    /// Display name.
    pub name: String,
    /// Scheduled runs enabled.
    pub enabled: bool,
    /// What to back up.
    pub spec: BackupSpec,
    /// When to run.
    pub schedule: Option<Schedule>,
    /// Old file cleanup.
    #[serde(default)]
    pub retention: Retention,
    /// What to do about missed runs.
    #[serde(default)]
    pub missed_run: MissedRunPolicy,
    /// Notifications.
    #[serde(default)]
    pub notify: NotifyPolicy,
    /// RFC 3339 time of the last run.
    pub last_run_at: Option<String>,
    /// RFC 3339 time of the next run.
    pub next_run_at: Option<String>,
    /// RFC 3339.
    pub created_at: String,
    /// RFC 3339.
    pub updated_at: String,
}

/// Cron schedule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Schedule {
    /// Five field cron expression (`croner`).
    pub cron: String,
    /// Time zone the expression is in.
    pub timezone: ScheduleTz,
}

/// Time zone of a schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ScheduleTz {
    /// Local time.
    Local,
    /// UTC.
    Utc,
}

/// Which old backup files to delete.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Retention {
    /// Keep the newest N.
    pub keep_last: Option<u32>,
    /// Delete files older than N days.
    pub max_age_days: Option<u32>,
}

/// What to do when the app was not running at a scheduled time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MissedRunPolicy {
    /// Run once on start.
    #[default]
    RunOnce,
    /// Wait for the next slot.
    Skip,
}

/// OS notification policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NotifyPolicy {
    /// Notify when a run succeeds.
    pub on_success: bool,
    /// Notify when a run fails.
    pub on_failure: bool,
}

impl Default for NotifyPolicy {
    fn default() -> Self {
        NotifyPolicy {
            on_success: yes(),
            on_failure: yes(),
        }
    }
}

/// Kind of a [`BackupRun`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RunKind {
    /// Backup.
    Backup,
    /// Restore.
    Restore,
    /// Verify.
    Verify,
}

/// What started a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RunTrigger {
    /// The user.
    Manual,
    /// The scheduler.
    Schedule,
    /// Catch-up after a missed slot.
    CatchUp,
    /// The `dbstudio-cli` binary.
    Cli,
}

/// State of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RunStatus {
    /// In progress.
    Running,
    /// Finished fine.
    Succeeded,
    /// Failed.
    Failed,
    /// Cancelled by the user.
    Cancelled,
    /// The app died while it was running.
    Interrupted,
}

/// A row of the run history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupRun {
    /// Run id.
    pub id: Uuid,
    /// Job, when started from one.
    pub job_id: Option<Uuid>,
    /// Job name at that time.
    pub job_name: String,
    /// Connection.
    pub connection_id: Option<Uuid>,
    /// Backup, restore or verify.
    pub kind: RunKind,
    /// What started it.
    pub trigger: RunTrigger,
    /// `logical` or the native tool name.
    pub engine: String,
    /// State.
    pub status: RunStatus,
    /// RFC 3339.
    pub started_at: String,
    /// RFC 3339.
    pub finished_at: Option<String>,
    /// Duration.
    pub duration_ms: Option<u64>,
    /// Output file.
    pub file_path: Option<String>,
    /// Output size.
    pub size_bytes: Option<u64>,
    /// SHA-256 of the file.
    pub checksum: Option<String>,
    /// Objects processed.
    pub objects: Option<u32>,
    /// Rows processed.
    pub rows: Option<u64>,
    /// Failure.
    pub error: Option<JobError>,
    /// Last log lines.
    pub log_tail: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Restore, inspection, tools
// ---------------------------------------------------------------------------------------------

/// What to restore and where.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreSpec {
    /// Backup file.
    pub file: String,
    /// Connection to restore into.
    pub target_connection_id: Uuid,
    /// Database to restore into; `None` = same name as the source.
    pub target_database: Option<String>,
    /// Create the database first.
    #[serde(default)]
    pub create_database: bool,
    /// Drop existing objects first (destructive: needs confirmation).
    #[serde(default)]
    pub clean: bool,
    /// Subset of objects (names from the preamble); empty = all.
    #[serde(default)]
    pub objects: Vec<String>,
    /// Keep going after errors.
    #[serde(default)]
    pub continue_on_error: bool,
}

/// What can be read from a backup without restoring it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupInspection {
    /// Clear header JSON.
    pub header: serde_json::Value,
    /// `preamble.json` (needs the passphrase when encrypted).
    pub preamble: Option<serde_json::Value>,
    /// The payload is encrypted.
    pub encrypted: bool,
    /// Set for files made by a native tool.
    pub native: Option<NativeTool>,
    /// Objects of a logical backup (`formatVersion >= 3`); empty for older files and native
    /// backups.
    #[serde(default)]
    pub objects: Vec<BackupObject>,
}

/// Which part of an object a logical backup block holds (`-- dbstudio:object ... phase=...`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BackupPhase {
    /// Structure created before the data.
    Pre,
    /// Rows.
    Data,
    /// Indexes, constraints and other structure created after the data.
    Post,
}

/// One object listed in the preamble of a logical backup (partial restore).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupObject {
    /// Kind of the object.
    pub kind: ObjectKind,
    /// Schema (or keyspace / database) the object belongs to.
    pub schema: Option<String>,
    /// Object name.
    pub name: String,
    /// Phases present in the file.
    pub phases: Vec<BackupPhase>,
    /// Rows written, for data phases.
    pub rows: Option<u64>,
}

/// Where a tool was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ToolSource {
    /// Path configured in settings.
    Configured,
    /// Found in `PATH`.
    Path,
    /// Found in a well-known directory.
    KnownDir,
}

/// Result of probing one native tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeToolInfo {
    /// Tool.
    pub tool: NativeTool,
    /// Executable path when found.
    pub path: Option<String>,
    /// Output of `--version`.
    pub version: Option<String>,
    /// How it was found.
    pub source: ToolSource,
    /// Why it cannot be used (not found, too old...).
    pub problem: Option<String>,
}

/// Result of a backup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupOutcome {
    /// Files written.
    pub files: Vec<String>,
    /// Total size.
    pub size_bytes: u64,
    /// SHA-256 of the (first) file.
    pub checksum: String,
    /// Objects backed up.
    pub objects: u32,
    /// Rows backed up.
    pub rows: u64,
}

/// Result of a restore.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreOutcome {
    /// Objects restored.
    pub objects: u32,
    /// Rows restored.
    pub rows: u64,
    /// Failed statements / rows.
    pub errors: Vec<RowError>,
}

/// Everything backup and restore need from the application.
pub struct BackupContext {
    /// Connection to read from / write to.
    pub session: Arc<dyn Session>,
    /// Connection settings (no secrets).
    pub config: ConnectionConfig,
    /// Password for native tools.
    pub secret: Option<Secret>,
    /// Local SSH tunnel endpoint native tools connect to.
    pub tunnel_addr: Option<SocketAddr>,
    /// Progress, log and cancellation.
    pub reporter: Arc<dyn JobReporter>,
    /// Job name for the `{job}` file name placeholder.
    pub job_name: Option<String>,
    /// Configured native tool paths.
    pub tool_overrides: BTreeMap<NativeTool, String>,
    /// Application data directory: private scratch space (`<work_dir>/tmp`, mode 0700) for the
    /// plaintext script, native tool output and secret files of a run. `None` = the system
    /// temporary directory.
    pub work_dir: Option<std::path::PathBuf>,
}

impl fmt::Debug for BackupContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BackupContext")
            .field("config", &self.config)
            .field("tunnel_addr", &self.tunnel_addr)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------------------------

mod ops;

pub use ops::{
    apply_retention, apply_retention_protecting, inspect, render_file_name, run_backup,
    run_restore, verify,
};

#[cfg(test)]
mod tests;
