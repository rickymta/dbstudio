use std::collections::BTreeMap;

use chrono::TimeZone;
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
fn stub_contract_detect_tools_reports_not_detected() {
    let tools = detect_tools(&BTreeMap::new());
    assert_eq!(tools.len(), 10);
    assert!(
        tools
            .iter()
            .all(|t| t.path.is_none() && t.problem.as_deref() == Some("not detected"))
    );
    assert!(
        tools
            .iter()
            .all(|t| t.tool != NativeTool::MsSqlServerBackup)
    );
}

#[test]
fn stub_contract_retention_and_scheduler_return_nothing() {
    let now = Utc.with_ymd_and_hms(2025, 1, 2, 3, 0, 0).unwrap();
    assert!(apply_retention(&job(), &[run()], now).is_empty());
    let s = Scheduler::new(FixedClock(now));
    assert_eq!(s.clock().now(), now);
    assert_eq!(s.next_due(&job(), None), None);
    assert!(s.due_jobs(&[job()], &BTreeMap::new()).is_empty());
    assert!(s.catch_up(&[job()], &BTreeMap::new()).is_empty());
    assert_eq!(SystemClock.now().timezone(), Utc);
}

#[tokio::test]
async fn stub_contract_operations_are_unsupported() {
    let dir = std::path::Path::new("/nonexistent.dbsbak");
    assert_eq!(inspect(dir, None).await.unwrap_err().kind(), "unsupported");
    let reporter = Reporter(CancelToken::default());
    assert_eq!(
        verify(dir, None, &reporter).await.unwrap_err().kind(),
        "unsupported"
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
