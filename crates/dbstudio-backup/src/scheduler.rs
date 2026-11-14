//! Pure scheduling logic. The tick loop, locking and notifications live in `src-tauri`.

use std::collections::BTreeMap;

use chrono::{DateTime, Local, Utc};
use croner::Cron;
use dbstudio_core::{Error, Result};
use uuid::Uuid;

use crate::{BackupJob, MissedRunPolicy, Schedule, ScheduleTz};

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

/// Parse a five field cron expression.
pub fn parse_cron(expr: &str) -> Result<Cron> {
    let fields = expr.split_whitespace().count();
    if fields != 5 {
        return Err(Error::Invalid(format!(
            "cron expression needs 5 fields (minute hour day month weekday), got {fields}"
        )));
    }
    expr.parse::<Cron>()
        .map_err(|e| Error::Invalid(format!("invalid cron expression: {e}")))
}

/// Time zone name used by `schedule_preview` (`local` or `utc`, case-insensitive).
pub fn parse_tz(name: &str) -> Result<ScheduleTz> {
    match name.trim().to_ascii_lowercase().as_str() {
        "local" => Ok(ScheduleTz::Local),
        "utc" => Ok(ScheduleTz::Utc),
        other => Err(Error::Invalid(format!("unknown time zone {other:?}"))),
    }
}

/// First instant of `schedule` strictly after `after`.
pub fn next_after(schedule: &Schedule, after: DateTime<Utc>) -> Result<Option<DateTime<Utc>>> {
    let cron = parse_cron(&schedule.cron)?;
    Ok(match schedule.timezone {
        ScheduleTz::Utc => cron.find_next_occurrence(&after, false).ok(),
        ScheduleTz::Local => cron
            .find_next_occurrence(&after.with_timezone(&Local), false)
            .ok()
            .map(|d| d.with_timezone(&Utc)),
    })
}

/// Latest instant of `schedule` at or before `at` (no walk over the missed slots).
pub fn previous_at_or_before(schedule: &Schedule, at: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let cron = parse_cron(&schedule.cron).ok()?;
    match schedule.timezone {
        ScheduleTz::Utc => cron.find_previous_occurrence(&at, true).ok(),
        ScheduleTz::Local => cron
            .find_previous_occurrence(&at.with_timezone(&Local), true)
            .ok()
            .map(|d| d.with_timezone(&Utc)),
    }
}

/// The next `count` (at most 50) run times of `cron` after `from`, RFC 3339 in UTC.
pub fn preview_from(
    cron: &str,
    tz: ScheduleTz,
    count: u32,
    from: DateTime<Utc>,
) -> Result<Vec<String>> {
    let schedule = Schedule {
        cron: cron.trim().to_string(),
        timezone: tz,
    };
    parse_cron(&schedule.cron)?;
    let mut out = Vec::new();
    let mut at = from;
    for _ in 0..count.min(50) {
        match next_after(&schedule, at)? {
            Some(next) => {
                out.push(next.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
                at = next;
            }
            None => break,
        }
    }
    Ok(out)
}

/// Decides which jobs are due.
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
        let schedule = job.schedule.as_ref()?;
        let base = last_scheduled_for.unwrap_or_else(|| self.clock.now());
        next_after(schedule, base).ok().flatten()
    }

    /// The most recent scheduled instant in `(last_scheduled_for, now]`, if any. Recording it
    /// as the new `last_scheduled_for` makes several missed slots count as one run.
    pub fn latest_slot(
        &self,
        job: &BackupJob,
        last_scheduled_for: DateTime<Utc>,
    ) -> Option<DateTime<Utc>> {
        let schedule = job.schedule.as_ref()?;
        let slot = previous_at_or_before(schedule, self.clock.now())?;
        (slot > last_scheduled_for).then_some(slot)
    }

    /// Ids of enabled jobs with a scheduled instant in `(last_scheduled_for, now]`. A job
    /// without an entry has nothing due (the caller records `now` for new jobs).
    pub fn due_jobs(
        &self,
        jobs: &[BackupJob],
        last_scheduled_for: &BTreeMap<Uuid, DateTime<Utc>>,
    ) -> Vec<Uuid> {
        jobs.iter()
            .filter(|j| j.enabled && j.schedule.is_some())
            .filter(|j| {
                last_scheduled_for
                    .get(&j.id)
                    .and_then(|last| self.next_due(j, Some(*last)))
                    .is_some_and(|next| next <= self.clock.now())
            })
            .map(|j| j.id)
            .collect()
    }

    /// Ids of jobs that missed a slot and whose policy is `RunOnce` (called at start-up).
    pub fn catch_up(
        &self,
        jobs: &[BackupJob],
        last_scheduled_for: &BTreeMap<Uuid, DateTime<Utc>>,
    ) -> Vec<Uuid> {
        self.missed(jobs, last_scheduled_for, MissedRunPolicy::RunOnce)
    }

    /// Ids of jobs that missed a slot and whose policy is `Skip`: the caller only advances
    /// their `last_scheduled_for`.
    pub fn skipped(
        &self,
        jobs: &[BackupJob],
        last_scheduled_for: &BTreeMap<Uuid, DateTime<Utc>>,
    ) -> Vec<Uuid> {
        self.missed(jobs, last_scheduled_for, MissedRunPolicy::Skip)
    }

    fn missed(
        &self,
        jobs: &[BackupJob],
        last_scheduled_for: &BTreeMap<Uuid, DateTime<Utc>>,
        policy: MissedRunPolicy,
    ) -> Vec<Uuid> {
        let due = self.due_jobs(jobs, last_scheduled_for);
        jobs.iter()
            .filter(|j| j.missed_run == policy && due.contains(&j.id))
            .map(|j| j.id)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use chrono::TimeZone;

    use super::*;
    use crate::{BackupEngine, BackupSpec, Compression, Destination, NotifyPolicy, Retention};

    struct Fake(Mutex<DateTime<Utc>>);
    impl Fake {
        fn at(h: u32, m: u32) -> Self {
            Fake(Mutex::new(
                Utc.with_ymd_and_hms(2026, 3, 1, h, m, 0).unwrap(),
            ))
        }
        fn set(&self, h: u32, m: u32) {
            *self.0.lock().unwrap() = Utc.with_ymd_and_hms(2026, 3, 1, h, m, 0).unwrap();
        }
    }
    impl Clock for &Fake {
        fn now(&self) -> DateTime<Utc> {
            *self.0.lock().unwrap()
        }
    }

    fn t(h: u32, m: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 3, 1, h, m, 0).unwrap()
    }

    fn job(id: u128, cron: &str, policy: MissedRunPolicy) -> BackupJob {
        BackupJob {
            id: Uuid::from_u128(id),
            name: "j".into(),
            enabled: true,
            spec: BackupSpec {
                connection_id: Uuid::nil(),
                targets: vec![],
                engine: BackupEngine::Logical {
                    content: dbstudio_export::ExportContent::Both,
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
                cron: cron.into(),
                timezone: ScheduleTz::Utc,
            }),
            retention: Retention::default(),
            missed_run: policy,
            notify: NotifyPolicy::default(),
            last_run_at: None,
            next_run_at: None,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn every_five_minutes_fires_on_time() {
        let clock = Fake::at(0, 0);
        let s = Scheduler::new(&clock);
        let j = job(1, "*/5 * * * *", MissedRunPolicy::RunOnce);
        let mut last = BTreeMap::from([(j.id, t(0, 0))]);
        assert_eq!(s.next_due(&j, Some(t(0, 0))), Some(t(0, 5)));
        for (h, m, due) in [(0, 4, false), (0, 5, true), (0, 6, true)] {
            clock.set(h, m);
            assert_eq!(
                !s.due_jobs(std::slice::from_ref(&j), &last).is_empty(),
                due,
                "{h}:{m}"
            );
        }
        // After recording the slot nothing is due until the next one.
        last.insert(j.id, s.latest_slot(&j, t(0, 0)).unwrap());
        assert_eq!(last[&j.id], t(0, 5));
        assert!(s.due_jobs(std::slice::from_ref(&j), &last).is_empty());
        clock.set(0, 10);
        assert_eq!(s.due_jobs(std::slice::from_ref(&j), &last), vec![j.id]);
    }

    #[test]
    fn catch_up_runs_exactly_once_after_downtime() {
        let clock = Fake::at(0, 0);
        let s = Scheduler::new(&clock);
        let jobs = [
            job(1, "*/5 * * * *", MissedRunPolicy::RunOnce),
            job(2, "*/5 * * * *", MissedRunPolicy::Skip),
        ];
        let mut last = BTreeMap::from([(jobs[0].id, t(0, 0)), (jobs[1].id, t(0, 0))]);
        // The app was off for 23 minutes: four slots were missed.
        clock.set(0, 23);
        assert_eq!(s.catch_up(&jobs, &last), vec![jobs[0].id]);
        assert_eq!(s.skipped(&jobs, &last), vec![jobs[1].id]);
        // Running once records the latest missed slot, so the catch-up does not repeat.
        last.insert(jobs[0].id, s.latest_slot(&jobs[0], t(0, 0)).unwrap());
        last.insert(jobs[1].id, s.latest_slot(&jobs[1], t(0, 0)).unwrap());
        assert_eq!(last[&jobs[0].id], t(0, 20));
        assert!(s.catch_up(&jobs, &last).is_empty());
        assert!(s.skipped(&jobs, &last).is_empty());
        clock.set(0, 25);
        assert_eq!(s.due_jobs(&jobs, &last).len(), 2);
    }

    #[test]
    fn a_long_downtime_is_one_catch_up_and_costs_no_loop() {
        // A job that runs every minute and an app that was off for a month.
        let clock = Fake::at(0, 0);
        let s = Scheduler::new(&clock);
        let j = job(1, "* * * * *", MissedRunPolicy::RunOnce);
        let month_ago = t(0, 0) - chrono::Duration::days(30);
        let last = BTreeMap::from([(j.id, month_ago)]);
        assert_eq!(s.catch_up(std::slice::from_ref(&j), &last), vec![j.id]);
        // Exactly the latest slot, not a slot 10,080 minutes after the old mark.
        assert_eq!(s.latest_slot(&j, month_ago), Some(t(0, 0)));
        assert_eq!(s.latest_slot(&j, t(0, 0)), None);
    }

    #[test]
    fn disabled_manual_and_unknown_jobs_are_never_due() {
        let clock = Fake::at(5, 0);
        let s = Scheduler::new(&clock);
        let mut off = job(1, "* * * * *", MissedRunPolicy::RunOnce);
        off.enabled = false;
        let mut manual = job(2, "* * * * *", MissedRunPolicy::RunOnce);
        manual.schedule = None;
        let fresh = job(3, "* * * * *", MissedRunPolicy::RunOnce);
        let last = BTreeMap::from([(off.id, t(0, 0)), (manual.id, t(0, 0))]);
        assert!(s.due_jobs(&[off, manual, fresh.clone()], &last).is_empty());
        assert_eq!(s.next_due(&fresh, None), Some(t(5, 1)));
    }

    #[test]
    fn cron_validation_and_preview() {
        assert!(parse_cron("*/5 * * * *").is_ok());
        assert!(parse_cron("* * * *").is_err());
        assert!(parse_cron("0 0 0 * * *").is_err());
        assert!(parse_cron("61 * * * *").is_err());
        let p = preview_from("0 2 * * *", ScheduleTz::Utc, 3, t(3, 0)).unwrap();
        assert_eq!(
            p,
            [
                "2026-03-02T02:00:00Z",
                "2026-03-03T02:00:00Z",
                "2026-03-04T02:00:00Z"
            ]
        );
        assert_eq!(
            preview_from("* * * * *", ScheduleTz::Local, 500, t(0, 0))
                .unwrap()
                .len(),
            50
        );
        assert!(preview_from("nope", ScheduleTz::Utc, 3, t(0, 0)).is_err());
        assert_eq!(parse_tz("UTC").unwrap(), ScheduleTz::Utc);
        assert_eq!(parse_tz(" Local ").unwrap(), ScheduleTz::Local);
        assert!(parse_tz("Asia/Tokyo").is_err());
    }
}
