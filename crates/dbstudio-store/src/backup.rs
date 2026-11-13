//! Backup job and run records (`backup_jobs`, `backup_runs`). Package H owns this file.
//!
//! The typed `BackupJob` / `BackupRun` live in `dbstudio-backup` (which depends on this crate);
//! the store only holds their rows, with the structured parts as JSON text.

use dbstudio_core::Result;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use rusqlite::types::Value;
use rusqlite::{OptionalExtension, Row, params};

use crate::db::{Store, db_err, opt_uuid, parse_uuid};

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

const JOB_COLS: &str = "id, name, connection_id, enabled, spec, cron, timezone, retention, \
    missed_policy, notify, encrypted, last_scheduled_for, last_run_at, next_run_at, created_at, \
    updated_at";

const RUN_COLS: &str = "id, job_id, job_name, connection_id, kind, trigger, engine, status, \
    started_at, finished_at, duration_ms, file_path, size_bytes, checksum, objects, rows, \
    error_kind, error_message, log_tail";

fn job_from_row(r: &Row<'_>) -> rusqlite::Result<(BackupJobRow, Vec<String>)> {
    // Ids are parsed after the query so a corrupt row is an error, not a panic.
    let ids = vec![r.get::<_, String>(0)?, r.get::<_, String>(2)?];
    Ok((
        BackupJobRow {
            id: Uuid::nil(),
            name: r.get(1)?,
            connection_id: Uuid::nil(),
            enabled: r.get(3)?,
            spec: r.get(4)?,
            cron: r.get(5)?,
            timezone: r.get(6)?,
            retention: r.get(7)?,
            missed_policy: r.get(8)?,
            notify: r.get(9)?,
            encrypted: r.get(10)?,
            last_scheduled_for: r.get(11)?,
            last_run_at: r.get(12)?,
            next_run_at: r.get(13)?,
            created_at: r.get(14)?,
            updated_at: r.get(15)?,
        },
        ids,
    ))
}

fn finish_job((mut job, ids): (BackupJobRow, Vec<String>)) -> Result<BackupJobRow> {
    job.id = parse_uuid(&ids[0])?;
    job.connection_id = parse_uuid(&ids[1])?;
    Ok(job)
}

fn run_from_row(r: &Row<'_>) -> rusqlite::Result<(BackupRunRow, Vec<Option<String>>)> {
    let ids = vec![
        r.get::<_, Option<String>>(0)?,
        r.get::<_, Option<String>>(1)?,
        r.get::<_, Option<String>>(3)?,
    ];
    Ok((
        BackupRunRow {
            id: Uuid::nil(),
            job_id: None,
            job_name: r.get(2)?,
            connection_id: None,
            kind: r.get(4)?,
            trigger: r.get(5)?,
            engine: r.get(6)?,
            status: r.get(7)?,
            started_at: r.get(8)?,
            finished_at: r.get(9)?,
            duration_ms: r.get::<_, Option<i64>>(10)?.map(|v| v.max(0) as u64),
            file_path: r.get(11)?,
            size_bytes: r.get::<_, Option<i64>>(12)?.map(|v| v.max(0) as u64),
            checksum: r.get(13)?,
            objects: r.get::<_, Option<i64>>(14)?.map(|v| v.max(0) as u64),
            rows: r.get::<_, Option<i64>>(15)?.map(|v| v.max(0) as u64),
            error_kind: r.get(16)?,
            error_message: r.get(17)?,
            log_tail: r.get(18)?,
        },
        ids,
    ))
}

fn finish_run((mut run, ids): (BackupRunRow, Vec<Option<String>>)) -> Result<BackupRunRow> {
    run.id = parse_uuid(ids[0].as_deref().unwrap_or_default())?;
    run.job_id = opt_uuid(ids[1].clone())?;
    run.connection_id = opt_uuid(ids[2].clone())?;
    Ok(run)
}

impl Store {
    /// All jobs ordered by name.
    pub fn backup_jobs_list(&self) -> Result<Vec<BackupJobRow>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {JOB_COLS} FROM backup_jobs ORDER BY name COLLATE NOCASE, id"
            ))
            .map_err(db_err)?;
        let rows = stmt
            .query_map([], job_from_row)
            .map_err(db_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_err)?;
        rows.into_iter().map(finish_job).collect()
    }

    /// One job by id.
    pub fn backup_job_get(&self, id: Uuid) -> Result<Option<BackupJobRow>> {
        let conn = self.lock();
        let row = conn
            .query_row(
                &format!("SELECT {JOB_COLS} FROM backup_jobs WHERE id = ?1"),
                params![id.to_string()],
                job_from_row,
            )
            .optional()
            .map_err(db_err)?;
        row.map(finish_job).transpose()
    }

    /// Insert or update (by id) a job. `created_at` of an existing job is kept.
    pub fn backup_job_save(&self, job: &BackupJobRow) -> Result<BackupJobRow> {
        {
            let conn = self.lock();
            conn.execute(
                &format!(
                    "INSERT INTO backup_jobs ({JOB_COLS}) VALUES \
                     (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16) \
                     ON CONFLICT(id) DO UPDATE SET name = excluded.name, \
                       connection_id = excluded.connection_id, enabled = excluded.enabled, \
                       spec = excluded.spec, cron = excluded.cron, timezone = excluded.timezone, \
                       retention = excluded.retention, missed_policy = excluded.missed_policy, \
                       notify = excluded.notify, encrypted = excluded.encrypted, \
                       last_scheduled_for = excluded.last_scheduled_for, \
                       last_run_at = excluded.last_run_at, next_run_at = excluded.next_run_at, \
                       updated_at = excluded.updated_at"
                ),
                params![
                    job.id.to_string(),
                    job.name,
                    job.connection_id.to_string(),
                    job.enabled,
                    job.spec,
                    job.cron,
                    job.timezone,
                    job.retention,
                    job.missed_policy,
                    job.notify,
                    job.encrypted,
                    job.last_scheduled_for,
                    job.last_run_at,
                    job.next_run_at,
                    job.created_at,
                    job.updated_at,
                ],
            )
            .map_err(db_err)?;
        }
        self.backup_job_get(job.id)?
            .ok_or_else(|| dbstudio_core::Error::Storage("backup job vanished".into()))
    }

    /// Record scheduling state only (does not touch the job definition).
    pub fn backup_job_set_schedule_state(
        &self,
        id: Uuid,
        last_scheduled_for: Option<&str>,
        last_run_at: Option<&str>,
        next_run_at: Option<&str>,
    ) -> Result<()> {
        self.lock()
            .execute(
                "UPDATE backup_jobs SET last_scheduled_for = COALESCE(?2, last_scheduled_for), \
                 last_run_at = COALESCE(?3, last_run_at), next_run_at = ?4 WHERE id = ?1",
                params![id.to_string(), last_scheduled_for, last_run_at, next_run_at],
            )
            .map_err(db_err)?;
        Ok(())
    }

    /// Delete a job (its runs keep their records with `job_id` cleared).
    pub fn backup_job_delete(&self, id: Uuid) -> Result<()> {
        self.lock()
            .execute(
                "DELETE FROM backup_jobs WHERE id = ?1",
                params![id.to_string()],
            )
            .map_err(db_err)?;
        Ok(())
    }

    /// Insert a run record.
    pub fn backup_run_insert(&self, run: &BackupRunRow) -> Result<()> {
        self.lock()
            .execute(
                &format!(
                    "INSERT INTO backup_runs ({RUN_COLS}) VALUES \
                     (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, \
                      ?17, ?18, ?19)"
                ),
                run_params(run),
            )
            .map_err(db_err)?;
        Ok(())
    }

    /// Update a run record (progress, finish, log tail).
    pub fn backup_run_update(&self, run: &BackupRunRow) -> Result<()> {
        let n = self
            .lock()
            .execute(
                // `job_id` and `connection_id` are left alone: the job or connection may have
                // been deleted while the run was going (the foreign key then cleared them).
                "UPDATE backup_runs SET job_name = ?3, \
                 kind = ?5, trigger = ?6, engine = ?7, status = ?8, started_at = ?9, \
                 finished_at = ?10, duration_ms = ?11, file_path = ?12, size_bytes = ?13, \
                 checksum = ?14, objects = ?15, rows = ?16, error_kind = ?17, \
                 error_message = ?18, log_tail = ?19 WHERE id = ?1",
                run_params(run),
            )
            .map_err(db_err)?;
        if n == 0 {
            return Err(dbstudio_core::Error::NotFound(format!(
                "backup run {}",
                run.id
            )));
        }
        Ok(())
    }

    /// Runs newest first, optionally of one job, at most `limit`.
    pub fn backup_runs_list(&self, job_id: Option<Uuid>, limit: u32) -> Result<Vec<BackupRunRow>> {
        self.backup_runs_page(job_id, limit, None)
    }

    /// Like [`Store::backup_runs_list`], only runs that started before `before` (ISO-8601).
    pub fn backup_runs_page(
        &self,
        job_id: Option<Uuid>,
        limit: u32,
        before: Option<&str>,
    ) -> Result<Vec<BackupRunRow>> {
        let conn = self.lock();
        let job = job_id.map(|j| j.to_string());
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {RUN_COLS} FROM backup_runs \
                 WHERE (?1 IS NULL OR job_id = ?1) AND (?2 IS NULL OR started_at < ?2) \
                 ORDER BY started_at DESC, rowid DESC LIMIT ?3"
            ))
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![job, before, i64::from(limit)], run_from_row)
            .map_err(db_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_err)?;
        rows.into_iter().map(finish_run).collect()
    }

    /// Mark every `running` run as `interrupted` (after a crash); returns how many.
    pub fn backup_runs_mark_interrupted(&self) -> Result<u64> {
        let n = self
            .lock()
            .execute(
                "UPDATE backup_runs SET status = 'interrupted', finished_at = COALESCE(finished_at, ?1) \
                 WHERE status = 'running'",
                params![crate::db::now_iso()],
            )
            .map_err(db_err)?;
        Ok(n as u64)
    }
}

fn run_params(r: &BackupRunRow) -> rusqlite::ParamsFromIter<Vec<Value>> {
    let opt_i = |v: Option<u64>| v.map_or(Value::Null, |x| Value::Integer(x as i64));
    let opt_t = |v: &Option<String>| v.clone().map_or(Value::Null, Value::Text);
    rusqlite::params_from_iter(vec![
        Value::Text(r.id.to_string()),
        r.job_id.map_or(Value::Null, |u| Value::Text(u.to_string())),
        Value::Text(r.job_name.clone()),
        r.connection_id
            .map_or(Value::Null, |u| Value::Text(u.to_string())),
        Value::Text(r.kind.clone()),
        Value::Text(r.trigger.clone()),
        Value::Text(r.engine.clone()),
        Value::Text(r.status.clone()),
        Value::Text(r.started_at.clone()),
        opt_t(&r.finished_at),
        opt_i(r.duration_ms),
        opt_t(&r.file_path),
        opt_i(r.size_bytes),
        opt_t(&r.checksum),
        opt_i(r.objects),
        opt_i(r.rows),
        opt_t(&r.error_kind),
        opt_t(&r.error_message),
        opt_t(&r.log_tail),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ConnectionProfile;
    use dbstudio_core::{ConnectionConfig, DbKind};

    fn row(id: u128, conn: Uuid) -> BackupJobRow {
        BackupJobRow {
            id: Uuid::from_u128(id),
            name: format!("job{id}"),
            connection_id: conn,
            enabled: true,
            spec: "{}".into(),
            cron: Some("*/5 * * * *".into()),
            timezone: "utc".into(),
            retention: "{}".into(),
            missed_policy: "runOnce".into(),
            notify: "{}".into(),
            encrypted: false,
            last_scheduled_for: None,
            last_run_at: None,
            next_run_at: None,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    fn run(id: u128, job: Option<Uuid>, started: &str) -> BackupRunRow {
        BackupRunRow {
            id: Uuid::from_u128(id),
            job_id: job,
            job_name: "n".into(),
            connection_id: None,
            kind: "backup".into(),
            trigger: "manual".into(),
            engine: "logical".into(),
            status: "running".into(),
            started_at: started.into(),
            finished_at: None,
            duration_ms: None,
            file_path: None,
            size_bytes: None,
            checksum: None,
            objects: None,
            rows: None,
            error_kind: None,
            error_message: None,
            log_tail: None,
        }
    }

    fn store_with_connection() -> (Store, Uuid) {
        let store = Store::open_in_memory().unwrap();
        let mut config = ConnectionConfig::new(DbKind::Postgres, "c", "localhost");
        config.id = Uuid::from_u128(99);
        let p = ConnectionProfile {
            config,
            group_id: None,
            color: None,
            save_password: false,
            has_password: false,
            sort_order: 0,
            created_at: String::new(),
            updated_at: String::new(),
        };
        store.save_profile(&p).unwrap();
        (store, p.config.id)
    }

    #[test]
    fn a_run_survives_the_deletion_of_its_job_and_still_gets_its_result() {
        let (store, conn) = store_with_connection();
        let j = row(1, conn);
        store.backup_job_save(&j).unwrap();
        let mut r = run(1, Some(j.id), "2026-01-01T00:00:01Z");
        store.backup_run_insert(&r).unwrap();
        // The job is deleted (the foreign key clears the link) while the run is going.
        store.backup_job_delete(j.id).unwrap();
        r.status = "succeeded".into();
        r.file_path = Some("/b/x.dbsbak".into());
        store.backup_run_update(&r).unwrap();
        let got = &store.backup_runs_list(None, 10).unwrap()[0];
        assert_eq!(got.status, "succeeded");
        assert_eq!(got.job_id, None);
        assert_eq!(got.file_path.as_deref(), Some("/b/x.dbsbak"));
    }

    #[test]
    fn jobs_and_runs_roundtrip() {
        let (store, conn) = store_with_connection();
        let mut j = row(1, conn);
        store.backup_job_save(&j).unwrap();
        j.name = "renamed".into();
        j.next_run_at = Some("2026-01-02T00:00:00Z".into());
        let saved = store.backup_job_save(&j).unwrap();
        assert_eq!(saved, j);
        assert_eq!(store.backup_jobs_list().unwrap().len(), 1);
        store
            .backup_job_set_schedule_state(j.id, Some("s"), Some("r"), None)
            .unwrap();
        let g = store.backup_job_get(j.id).unwrap().unwrap();
        assert_eq!(
            (
                g.last_scheduled_for.as_deref(),
                g.last_run_at.as_deref(),
                g.next_run_at
            ),
            (Some("s"), Some("r"), None)
        );

        store
            .backup_run_insert(&run(1, Some(j.id), "2026-01-01T00:00:01Z"))
            .unwrap();
        let mut r2 = run(2, Some(j.id), "2026-01-01T00:00:02Z");
        r2.size_bytes = Some(10);
        store.backup_run_insert(&r2).unwrap();
        store
            .backup_run_insert(&run(3, None, "2026-01-01T00:00:03Z"))
            .unwrap();
        let all = store.backup_runs_list(None, 10).unwrap();
        assert_eq!(
            all.iter().map(|r| r.id.as_u128()).collect::<Vec<_>>(),
            [3, 2, 1]
        );
        assert_eq!(store.backup_runs_list(Some(j.id), 10).unwrap().len(), 2);
        let page = store
            .backup_runs_page(None, 10, Some("2026-01-01T00:00:03Z"))
            .unwrap();
        assert_eq!(page.len(), 2);
        r2.status = "succeeded".into();
        store.backup_run_update(&r2).unwrap();
        assert_eq!(store.backup_runs_mark_interrupted().unwrap(), 2);
        let all = store.backup_runs_list(None, 10).unwrap();
        assert_eq!(all[1].status, "succeeded");
        assert_eq!(all[1].size_bytes, Some(10));
        assert_eq!(all[0].status, "interrupted");
        // Deleting the job keeps the runs.
        store.backup_job_delete(j.id).unwrap();
        assert_eq!(store.backup_runs_list(None, 10).unwrap().len(), 3);
        assert!(store.backup_run_update(&run(77, None, "x")).is_err());
    }
}
