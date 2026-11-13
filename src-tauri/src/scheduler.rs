//! Backup scheduler glue: owns the tick loop, reads due jobs from the store and starts them
//! through the `JobManager`. The "which job is due" logic is `dbstudio_backup::Scheduler`.
//!
//! At start-up runs left `running` by a crash become `interrupted`, jobs that missed a slot
//! while the app was off run once (`RunOnce`, trigger `catchUp`) or are skipped (`Skip`). Then
//! every [`TICK`] the due jobs are started; the per-job file lock stops a run overlapping with
//! the same job in the CLI.

use std::collections::BTreeMap;
use std::time::Duration;

use dbstudio_backup::chrono::{self, DateTime, Utc};
use dbstudio_backup::runner::{next_run_text, row_to_job};
use dbstudio_backup::{BackupJob, Clock, RunTrigger, Scheduler, SystemClock};
use dbstudio_core::Result;
use dbstudio_store::Store;
use uuid::Uuid;

use crate::commands::backup::spawn_job_run;
use crate::state::AppState;

/// Time between two looks at the schedule.
pub const TICK: Duration = Duration::from_secs(30);

/// A job the scheduler decided to start.
#[derive(Debug, Clone)]
pub struct DueRun {
    /// The job.
    pub job: BackupJob,
    /// `schedule` or `catchUp`.
    pub trigger: RunTrigger,
}

fn iso(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

type LastSlots = BTreeMap<Uuid, DateTime<Utc>>;

fn load(store: &Store) -> Result<(Vec<BackupJob>, LastSlots)> {
    let rows = store.backup_jobs_list()?;
    let mut jobs = Vec::new();
    let mut last = BTreeMap::new();
    for row in rows {
        let Ok(job) = row_to_job(&row) else {
            tracing::warn!(job = %row.id, "skipping a backup job that cannot be read");
            continue;
        };
        if let Some(t) = row
            .last_scheduled_for
            .as_deref()
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        {
            last.insert(job.id, t.with_timezone(&Utc));
        }
        jobs.push(job);
    }
    Ok((jobs, last))
}

/// New scheduled jobs have no history: their first slot is the first one after now.
fn seed_new_jobs<C: Clock>(
    store: &Store,
    sched: &Scheduler<C>,
    jobs: &[BackupJob],
    last: &mut BTreeMap<Uuid, DateTime<Utc>>,
) -> Result<()> {
    let now = sched.clock().now();
    for job in jobs.iter().filter(|j| j.enabled && j.schedule.is_some()) {
        if let std::collections::btree_map::Entry::Vacant(slot) = last.entry(job.id) {
            store.backup_job_set_schedule_state(
                job.id,
                Some(&iso(now)),
                None,
                next_run_text(job, now).as_deref(),
            )?;
            slot.insert(now);
        }
    }
    Ok(())
}

/// Record the latest slot of `job` as handled and its next run time.
fn record_slot<C: Clock>(
    store: &Store,
    sched: &Scheduler<C>,
    job: &BackupJob,
    last: &mut BTreeMap<Uuid, DateTime<Utc>>,
) -> Result<()> {
    let Some(prev) = last.get(&job.id).copied() else {
        return Ok(());
    };
    let slot = sched
        .latest_slot(job, prev)
        .unwrap_or_else(|| sched.clock().now());
    store.backup_job_set_schedule_state(
        job.id,
        Some(&iso(slot)),
        None,
        next_run_text(job, sched.clock().now()).as_deref(),
    )?;
    last.insert(job.id, slot);
    Ok(())
}

/// Start-up plan: recover from a crash, then decide the catch-up runs.
pub fn startup_plan<C: Clock>(store: &Store, sched: &Scheduler<C>) -> Result<Vec<DueRun>> {
    let interrupted = store.backup_runs_mark_interrupted()?;
    if interrupted > 0 {
        tracing::warn!(
            runs = interrupted,
            "marked unfinished backup runs as interrupted"
        );
    }
    let (jobs, mut last) = load(store)?;
    seed_new_jobs(store, sched, &jobs, &mut last)?;
    let catch = sched.catch_up(&jobs, &last);
    let skip = sched.skipped(&jobs, &last);
    let mut out = Vec::new();
    for job in &jobs {
        if catch.contains(&job.id) {
            record_slot(store, sched, job, &mut last)?;
            out.push(DueRun {
                job: job.clone(),
                trigger: RunTrigger::CatchUp,
            });
        } else if skip.contains(&job.id) {
            record_slot(store, sched, job, &mut last)?;
        }
    }
    Ok(out)
}

/// One tick: the jobs whose slot has come. Their slot is recorded before they start, so a job
/// is never started twice for the same slot.
pub fn tick_plan<C: Clock>(store: &Store, sched: &Scheduler<C>) -> Result<Vec<DueRun>> {
    let (jobs, mut last) = load(store)?;
    seed_new_jobs(store, sched, &jobs, &mut last)?;
    let mut out = Vec::new();
    for id in sched.due_jobs(&jobs, &last) {
        if let Some(job) = jobs.iter().find(|j| j.id == id) {
            record_slot(store, sched, job, &mut last)?;
            out.push(DueRun {
                job: job.clone(),
                trigger: RunTrigger::Schedule,
            });
        }
    }
    Ok(out)
}

fn launch(state: &AppState, due: Vec<DueRun>) {
    for d in due {
        tracing::info!(job = %d.job.id, trigger = ?d.trigger, "starting scheduled backup");
        spawn_job_run(state, d.job, d.trigger);
    }
}

/// Start the scheduler loop on the Tauri async runtime.
pub fn start(state: AppState) {
    tauri::async_runtime::spawn(async move {
        let sched = Scheduler::new(SystemClock);
        match startup_plan(state.connections.store(), &sched) {
            Ok(due) => launch(&state, due),
            Err(e) => tracing::error!(error = %e, "scheduler start-up failed"),
        }
        let mut interval = tokio::time::interval(TICK);
        interval.tick().await;
        loop {
            interval.tick().await;
            match tick_plan(state.connections.store(), &sched) {
                Ok(due) => launch(&state, due),
                Err(e) => tracing::error!(error = %e, "scheduler tick failed"),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use dbstudio_backup::chrono::TimeZone;
    use dbstudio_backup::runner::job_to_row;
    use dbstudio_backup::{
        BackupEngine, BackupSpec, Compression, Destination, MissedRunPolicy, NotifyPolicy,
        Retention, Schedule, ScheduleTz,
    };
    use dbstudio_core::{ConnectionConfig, DbKind};
    use dbstudio_export::ExportContent;
    use dbstudio_store::ConnectionProfile;

    use super::*;

    struct Fake(Mutex<DateTime<Utc>>);
    impl Clock for &Fake {
        fn now(&self) -> DateTime<Utc> {
            *self.0.lock().unwrap()
        }
    }
    impl Fake {
        fn set(&self, h: u32, m: u32) {
            *self.0.lock().unwrap() = Utc.with_ymd_and_hms(2026, 3, 1, h, m, 0).unwrap();
        }
    }

    fn store_with_job(policy: MissedRunPolicy) -> (Store, Uuid) {
        let store = Store::open_in_memory().unwrap();
        let mut config = ConnectionConfig::new(DbKind::Postgres, "c", "h");
        config.id = Uuid::from_u128(9);
        store
            .save_profile(&ConnectionProfile {
                config,
                group_id: None,
                color: None,
                save_password: false,
                has_password: false,
                sort_order: 0,
                created_at: String::new(),
                updated_at: String::new(),
            })
            .unwrap();
        let job = BackupJob {
            id: Uuid::from_u128(1),
            name: "every5".into(),
            enabled: true,
            spec: BackupSpec {
                connection_id: Uuid::from_u128(9),
                targets: vec![],
                engine: BackupEngine::Logical {
                    content: ExportContent::Both,
                    ddl: Default::default(),
                },
                compression: Compression::None,
                encrypted: false,
                destination: Destination::LocalDir {
                    path: "/b".into(),
                    file_template: "x".into(),
                },
            },
            schedule: Some(Schedule {
                cron: "*/5 * * * *".into(),
                timezone: ScheduleTz::Utc,
            }),
            retention: Retention::default(),
            missed_run: policy,
            notify: NotifyPolicy::default(),
            last_run_at: None,
            next_run_at: None,
            created_at: "2026-03-01T00:00:00Z".into(),
            updated_at: "2026-03-01T00:00:00Z".into(),
        };
        store
            .backup_job_save(&job_to_row(&job, None).unwrap())
            .unwrap();
        (store, job.id)
    }

    fn last_slot(store: &Store, id: Uuid) -> String {
        store
            .backup_job_get(id)
            .unwrap()
            .unwrap()
            .last_scheduled_for
            .unwrap()
    }

    #[test]
    fn ticks_fire_on_the_five_minute_marks_and_never_twice() {
        let (store, id) = store_with_job(MissedRunPolicy::RunOnce);
        let clock = Fake(Mutex::new(
            Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap(),
        ));
        let sched = Scheduler::new(&clock);
        assert!(startup_plan(&store, &sched).unwrap().is_empty());
        assert_eq!(last_slot(&store, id), "2026-03-01T00:00:00Z");
        for (h, m, fires) in [
            (0, 2, false),
            (0, 5, true),
            (0, 5, false),
            (0, 9, false),
            (0, 10, true),
        ] {
            clock.set(h, m);
            let due = tick_plan(&store, &sched).unwrap();
            assert_eq!(due.len(), usize::from(fires), "{h}:{m}");
            if fires {
                assert_eq!(due[0].trigger, RunTrigger::Schedule);
            }
        }
        assert_eq!(last_slot(&store, id), "2026-03-01T00:10:00Z");
        let next = store
            .backup_job_get(id)
            .unwrap()
            .unwrap()
            .next_run_at
            .unwrap();
        assert_eq!(next, "2026-03-01T00:15:00Z");
    }

    #[test]
    fn catch_up_runs_once_after_the_app_was_off() {
        let (store, id) = store_with_job(MissedRunPolicy::RunOnce);
        let clock = Fake(Mutex::new(
            Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap(),
        ));
        let sched = Scheduler::new(&clock);
        startup_plan(&store, &sched).unwrap();
        // A run was in progress when the app died.
        store
            .backup_run_insert(&dbstudio_store::BackupRunRow {
                id: Uuid::from_u128(5),
                job_id: Some(id),
                job_name: "n".into(),
                connection_id: None,
                kind: "backup".into(),
                trigger: "schedule".into(),
                engine: "logical".into(),
                status: "running".into(),
                started_at: "2026-03-01T00:04:00Z".into(),
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
            })
            .unwrap();
        // Restart 23 minutes later: four slots were missed, one catch-up run is started.
        clock.set(0, 23);
        let due = startup_plan(&store, &sched).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].trigger, RunTrigger::CatchUp);
        assert_eq!(last_slot(&store, id), "2026-03-01T00:20:00Z");
        assert_eq!(
            store.backup_runs_list(None, 5).unwrap()[0].status,
            "interrupted"
        );
        // The next tick (same minute) does not start it again; the 00:25 slot does.
        assert!(tick_plan(&store, &sched).unwrap().is_empty());
        clock.set(0, 25);
        assert_eq!(tick_plan(&store, &sched).unwrap().len(), 1);
    }

    #[test]
    fn skip_policy_moves_on_without_running() {
        let (store, id) = store_with_job(MissedRunPolicy::Skip);
        let clock = Fake(Mutex::new(
            Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap(),
        ));
        let sched = Scheduler::new(&clock);
        startup_plan(&store, &sched).unwrap();
        clock.set(1, 3);
        assert!(startup_plan(&store, &sched).unwrap().is_empty());
        assert_eq!(last_slot(&store, id), "2026-03-01T01:00:00Z");
    }
}
