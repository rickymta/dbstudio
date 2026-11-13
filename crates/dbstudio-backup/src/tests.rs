use std::collections::BTreeMap;

use std::path::PathBuf;

use chrono::{DateTime, TimeZone, Utc};
use dbstudio_core::testing::write_or_check;
use dbstudio_core::{CancelToken, JobProgress, ObjectKind};
use serde::Serialize;
use serde_json::{Value as Json, json};

use super::*;

fn j<T: Serialize>(v: &T) -> Json {
    serde_json::to_value(v).unwrap()
}

fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

fn spec() -> BackupSpec {
    BackupSpec {
        connection_id: id(1),
        targets: vec![ObjectPath::root().child(ObjectKind::Database, "app")],
        engine: BackupEngine::Logical {
            content: ExportContent::Both,
            ddl: DdlOptions::default(),
        },
        compression: Compression::default(),
        encrypted: true,
        destination: Destination::LocalDir {
            path: "/backups".into(),
            file_template: default_template(),
        },
    }
}

fn job() -> BackupJob {
    BackupJob {
        id: id(2),
        name: "nightly".into(),
        enabled: true,
        spec: spec(),
        schedule: Some(Schedule {
            cron: "0 2 * * *".into(),
            timezone: ScheduleTz::Local,
        }),
        retention: Retention {
            keep_last: Some(7),
            max_age_days: None,
        },
        missed_run: MissedRunPolicy::RunOnce,
        notify: NotifyPolicy::default(),
        last_run_at: Some("2025-01-01T02:00:00Z".into()),
        next_run_at: Some("2025-01-02T02:00:00Z".into()),
        created_at: "2024-12-01T00:00:00Z".into(),
        updated_at: "2024-12-01T00:00:00Z".into(),
    }
}

fn run() -> BackupRun {
    BackupRun {
        id: id(3),
        job_id: Some(id(2)),
        job_name: "nightly".into(),
        connection_id: Some(id(1)),
        kind: RunKind::Backup,
        trigger: RunTrigger::Schedule,
        engine: "logical".into(),
        status: RunStatus::Failed,
        started_at: "2025-01-01T02:00:00Z".into(),
        finished_at: Some("2025-01-01T02:00:05Z".into()),
        duration_ms: Some(5000),
        file_path: Some("/backups/app.dbsbak".into()),
        size_bytes: Some(1024),
        checksum: Some("ab".repeat(32)),
        objects: Some(3),
        rows: Some(100),
        error: Some(JobError {
            kind: "io".into(),
            message: "disk full".into(),
        }),
        log_tail: Some("line".into()),
    }
}

fn restore() -> RestoreSpec {
    RestoreSpec {
        file: "/backups/app.dbsbak".into(),
        target_connection_id: id(4),
        target_database: Some("app_copy".into()),
        create_database: true,
        clean: false,
        objects: vec![],
        continue_on_error: true,
    }
}

fn tool_info() -> NativeToolInfo {
    NativeToolInfo {
        tool: NativeTool::PgDump,
        path: Some("/usr/bin/pg_dump".into()),
        version: Some("pg_dump (PostgreSQL) 16.4".into()),
        source: ToolSource::Path,
        problem: None,
    }
}

fn inspection() -> BackupInspection {
    BackupInspection {
        header: json!({"formatVersion": 1, "compression": "zstd", "encryption": null}),
        preamble: Some(json!({"engine": "postgres"})),
        encrypted: false,
        native: None,
    }
}

fn backup_outcome() -> BackupOutcome {
    BackupOutcome {
        files: vec!["/backups/app.dbsbak".into()],
        size_bytes: 1024,
        checksum: "cd".repeat(32),
        objects: 3,
        rows: 100,
    }
}

fn restore_outcome() -> RestoreOutcome {
    RestoreOutcome {
        objects: 3,
        rows: 100,
        errors: vec![RowError {
            line: 12,
            message: "relation exists".into(),
        }],
    }
}

struct Reporter(CancelToken);
impl JobReporter for Reporter {
    fn progress(&self, _p: &JobProgress) {}
    fn cancel_token(&self) -> &CancelToken {
        &self.0
    }
}

struct FixedClock(DateTime<Utc>);
impl Clock for FixedClock {
    fn now(&self) -> DateTime<Utc> {
        self.0
    }
}

#[test]
fn defaults() {
    assert_eq!(Compression::default(), Compression::Zstd { level: 3 });
    assert_eq!(
        NotifyPolicy::default(),
        NotifyPolicy {
            on_success: true,
            on_failure: true
        }
    );
    assert_eq!(MissedRunPolicy::default(), MissedRunPolicy::RunOnce);
    assert_eq!(
        Retention::default(),
        Retention {
            keep_last: None,
            max_age_days: None
        }
    );
}

#[test]
fn json_shapes() {
    assert_eq!(j(&Compression::None), json!({"kind": "none"}));
    assert_eq!(
        j(&Compression::Zstd { level: 3 }),
        json!({"kind": "zstd", "level": 3})
    );
    assert_eq!(
        j(&Compression::Gzip { level: 6 }),
        json!({"kind": "gzip", "level": 6})
    );
    assert_eq!(j(&NativeTool::MySqlDump), json!("mySqlDump"));
    assert_eq!(
        j(&NativeTool::MsSqlServerBackup),
        json!("msSqlServerBackup")
    );
    // Defaults are filled when optional fields are missing.
    let s: BackupSpec = serde_json::from_value(json!({
        "connectionId": id(1),
        "targets": [],
        "engine": {"kind": "logical"},
        "destination": {"kind": "localDir", "path": "/b"}
    }))
    .unwrap();
    assert_eq!(s.compression, Compression::Zstd { level: 3 });
    assert!(!s.encrypted);
    assert_eq!(
        s.destination,
        Destination::LocalDir {
            path: "/b".into(),
            file_template: default_template()
        }
    );
    assert!(matches!(
        s.engine,
        BackupEngine::Logical {
            content: ExportContent::Both,
            ..
        }
    ));
    for v in [j(&job()), j(&run())] {
        assert!(v.is_object());
    }
    assert_eq!(
        serde_json::from_value::<BackupJob>(j(&job())).unwrap(),
        job()
    );
    assert_eq!(
        serde_json::from_value::<BackupRun>(j(&run())).unwrap(),
        run()
    );
    assert_eq!(
        serde_json::from_value::<RestoreSpec>(j(&restore())).unwrap(),
        restore()
    );
    assert_eq!(
        serde_json::from_value::<NativeToolInfo>(j(&tool_info())).unwrap(),
        tool_info()
    );
    assert_eq!(
        serde_json::from_value::<BackupInspection>(j(&inspection())).unwrap(),
        inspection()
    );
    assert_eq!(
        serde_json::from_value::<BackupOutcome>(j(&backup_outcome())).unwrap(),
        backup_outcome()
    );
    assert_eq!(
        serde_json::from_value::<RestoreOutcome>(j(&restore_outcome())).unwrap(),
        restore_outcome()
    );
}

#[test]
fn detection_lists_every_binary_tool() {
    let tools = detect_tools(&BTreeMap::new());
    assert_eq!(tools.len(), 10);
    // Either found, or a human readable problem is attached.
    assert!(
        tools
            .iter()
            .all(|t| t.path.is_some() || t.problem.is_some())
    );
    assert!(
        tools
            .iter()
            .all(|t| t.tool != NativeTool::MsSqlServerBackup)
    );
}

fn run_of(n: u128, at: &str, file: &str) -> BackupRun {
    BackupRun {
        id: id(100 + n),
        started_at: at.into(),
        file_path: Some(file.into()),
        status: RunStatus::Succeeded,
        ..run()
    }
}

#[test]
fn retention_keeps_the_newest_n_and_only_files_of_the_job() {
    let now = Utc.with_ymd_and_hms(2025, 1, 10, 0, 0, 0).unwrap();
    let mut j = job();
    j.retention = Retention {
        keep_last: Some(3),
        max_age_days: None,
    };
    let mut runs: Vec<BackupRun> = (1..=5)
        .map(|n| {
            run_of(
                n,
                &format!("2025-01-0{n}T02:00:00Z"),
                &format!("/b/{n}.dbsbak"),
            )
        })
        .collect();
    // Not deleted: other job, failed run, restore run, run without a file.
    let mut other = run_of(6, "2024-12-01T00:00:00Z", "/b/other.dbsbak");
    other.job_id = Some(id(77));
    let mut failed = run_of(7, "2024-12-02T00:00:00Z", "/b/failed.dbsbak");
    failed.status = RunStatus::Failed;
    let mut restore = run_of(8, "2024-12-03T00:00:00Z", "/b/restore.dbsbak");
    restore.kind = RunKind::Restore;
    let mut nofile = run_of(9, "2024-12-04T00:00:00Z", "");
    nofile.file_path = None;
    runs.extend([other, failed, restore, nofile]);
    let doomed = apply_retention(&j, &runs, now);
    assert_eq!(
        doomed,
        vec![PathBuf::from("/b/1.dbsbak"), PathBuf::from("/b/2.dbsbak")]
    );
    // No rule, nothing deleted.
    j.retention = Retention::default();
    assert!(apply_retention(&j, &runs, now).is_empty());
}

#[test]
fn retention_by_age_always_keeps_the_newest_file() {
    let now = Utc.with_ymd_and_hms(2025, 3, 1, 0, 0, 0).unwrap();
    let mut j = job();
    j.retention = Retention {
        keep_last: None,
        max_age_days: Some(30),
    };
    let runs = vec![
        run_of(1, "2025-02-27T00:00:00Z", "/b/new.dbsbak"),
        run_of(2, "2025-02-01T00:00:00Z", "/b/mid.dbsbak"),
        run_of(3, "2025-01-01T00:00:00Z", "/b/old.dbsbak"),
    ];
    assert_eq!(
        apply_retention(&j, &runs, now),
        vec![PathBuf::from("/b/old.dbsbak")]
    );
    // Everything is older than the limit: the newest one survives.
    let later = Utc.with_ymd_and_hms(2025, 9, 1, 0, 0, 0).unwrap();
    let doomed = apply_retention(&j, &runs, later);
    assert_eq!(doomed.len(), 2);
    assert!(!doomed.contains(&PathBuf::from("/b/new.dbsbak")));
}

#[test]
fn file_name_template_expands_and_sanitizes() {
    let now = chrono::Local.with_ymd_and_hms(2026, 5, 6, 7, 8, 9).unwrap();
    assert_eq!(
        render_file_name(&default_template(), "My/Conn", "app", "nightly", now),
        "My_Conn-app-20260506-070809.dbsbak"
    );
    assert_eq!(
        render_file_name("{job}_{date}_{unknown}", "c", "d", "../j", now),
        "_j_20260506-070809_{unknown}.dbsbak"
    );
    assert_eq!(
        render_file_name("a/b\\c-{date:%Y}.dbsbak", "c", "d", "j", now),
        "a_b_c-2026.dbsbak"
    );
    assert_eq!(
        render_file_name("", "c", "d", "j", now),
        "backup-20260506-070809.dbsbak"
    );
}

#[tokio::test]
async fn inspect_and_verify_reject_foreign_files() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("x.dbsbak");
    std::fs::write(&f, b"not a backup at all").unwrap();
    assert_eq!(inspect(&f, None).await.unwrap_err().kind(), "integrity");
    let reporter = Reporter(CancelToken::default());
    assert_eq!(
        verify(&f, None, &reporter).await.unwrap_err().kind(),
        "integrity"
    );
    assert_eq!(
        verify(&dir.path().join("missing"), None, &reporter)
            .await
            .unwrap_err()
            .kind(),
        "io"
    );
    assert_eq!(SystemClock.now().timezone(), Utc);
    assert!(
        Scheduler::new(FixedClock(Utc::now()))
            .next_due(&job(), None)
            .is_some()
    );
}

#[test]
fn fixtures() {
    write_or_check(
        "backup",
        &json!({
            "BackupSpec": j(&spec()),
            "BackupEngine": j(&spec().engine),
            "Compression": j(&Compression::default()),
            "Destination": j(&spec().destination),
            "BackupJob": j(&job()),
            "Schedule": j(&job().schedule),
            "BackupRun": j(&run()),
            "RestoreSpec": j(&restore()),
            "BackupInspection": j(&inspection()),
            "NativeToolInfo": j(&tool_info()),
            "BackupOutcome": j(&backup_outcome()),
            "RestoreOutcome": j(&restore_outcome()),
        }),
    );
}
