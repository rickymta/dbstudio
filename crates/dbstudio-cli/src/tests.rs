use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use clap::CommandFactory;
use dbstudio_backup::runner::{job_to_row, row_to_job};
use dbstudio_backup::{
    BackupEngine, BackupJob, BackupSpec, Compression, Destination, MissedRunPolicy, NotifyPolicy,
    Retention,
};
use dbstudio_core::{
    Capabilities, ColumnMeta, ConnectionConfig, DbKind, Driver, DriverDescriptor, DriverStatus,
    EndOutcome, Environment, ExecSummary, ObjectKind, ObjectNode, ObjectPath, QueryRequest,
    ResultSink, ScriptDialect, ServerInfo, Session, StatementEnd, TableDataRequest, Value,
    ValueKind, folder,
};
use dbstudio_export::ExportContent;
use dbstudio_store::{ConnectionProfile, SecretChange, backup_secret_account};

use super::*;

const JOB: &str = "00000000-0000-0000-0000-000000000001";

fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
    let mut v = vec!["dbstudio-cli"];
    v.extend_from_slice(args);
    Cli::try_parse_from(v)
}

#[test]
fn command_definition_is_valid() {
    Cli::command().debug_assert();
}

#[test]
fn parses_backup_job_and_list() {
    match parse(&["backup", "--job", JOB]).unwrap().command {
        Command::Backup { job, list } => {
            assert_eq!(job, Some(Uuid::from_u128(1)));
            assert!(!list);
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        parse(&["backup", "--list"]).unwrap().command,
        Command::Backup {
            job: None,
            list: true
        }
    ));
    assert!(parse(&["backup"]).is_err());
    assert!(parse(&["backup", "--list", "--job", JOB]).is_err());
    assert!(parse(&["backup", "--job", "not-a-uuid"]).is_err());
}

#[test]
fn parses_restore_verify_tools_and_global_options() {
    let cli = parse(&[
        "--data-dir",
        "/d",
        "--secret-store",
        "memory",
        "restore",
        "--file",
        "a.dbsbak",
        "--connection",
        JOB,
        "--database",
        "app",
        "--yes",
        "--confirm-text",
        "app",
        "--create-database",
    ])
    .unwrap();
    assert_eq!(cli.data_dir, Some(PathBuf::from("/d")));
    assert_eq!(cli.secret_store, SecretStoreKind::Memory);
    match cli.command {
        Command::Restore {
            file,
            connection,
            database,
            yes,
            confirm_text,
            create_database,
            clean,
            passphrase_env,
            ..
        } => {
            assert_eq!(file, PathBuf::from("a.dbsbak"));
            assert_eq!(connection, Uuid::from_u128(1));
            assert_eq!(database.as_deref(), Some("app"));
            assert!(yes && create_database && !clean);
            assert_eq!(confirm_text.as_deref(), Some("app"));
            assert_eq!(passphrase_env, PASSPHRASE_ENV);
        }
        other => panic!("{other:?}"),
    }
    // `--data-dir` is global: accepted after the subcommand too.
    let cli = parse(&["tools", "--data-dir", "/x"]).unwrap();
    assert_eq!(cli.data_dir, Some(PathBuf::from("/x")));
    assert_eq!(cli.secret_store, SecretStoreKind::Keychain);
    assert!(matches!(
        parse(&["verify", "--file", "f"]).unwrap().command,
        Command::Verify { .. }
    ));
    assert!(parse(&["restore", "--file", "f"]).is_err());
    assert!(parse(&[]).is_err());
}

#[test]
fn exit_codes_and_help() {
    assert_eq!(run(["dbstudio-cli", "backup"]), EXIT_USAGE);
    assert_eq!(run(["dbstudio-cli", "--help"]), EXIT_OK);
    assert_eq!(
        run([
            "dbstudio-cli",
            "--data-dir",
            "/nonexistent-dbstudio-dir",
            "backup",
            "--list"
        ]),
        EXIT_ERROR
    );
    assert_eq!((EXIT_OK, EXIT_ERROR, EXIT_USAGE, EXIT_LOCKED), (0, 1, 2, 3));
    let help = Cli::command().render_long_help().to_string();
    for needle in ["cron", "launchd", "Task Scheduler", "--job <JOB-ID>"] {
        assert!(help.contains(needle), "help lacks {needle}");
    }
}

// ---------------------------------------------------------------------------------------------
// A fake PostgreSQL-like server
// ---------------------------------------------------------------------------------------------

type Executed = Arc<Mutex<Vec<(Option<String>, String)>>>;

struct FakeSession {
    executed: Executed,
}

#[async_trait]
impl Session for FakeSession {
    fn script_dialect(&self) -> ScriptDialect {
        ScriptDialect::Postgres
    }
    async fn ping(&self) -> dbstudio_core::Result<()> {
        Ok(())
    }
    async fn server_info(&self) -> dbstudio_core::Result<ServerInfo> {
        Ok(ServerInfo {
            product: "Fake".into(),
            version: "16.0".into(),
            extra: BTreeMap::new(),
        })
    }
    async fn execute(
        &self,
        req: QueryRequest,
        _sink: &mut dyn ResultSink,
    ) -> dbstudio_core::Result<ExecSummary> {
        self.executed.lock().unwrap().push((req.database, req.text));
        Ok(ExecSummary::default())
    }
    async fn list_children(&self, parent: &ObjectPath) -> dbstudio_core::Result<Vec<ObjectNode>> {
        let last = parent.segments.last().map(|s| s.kind);
        Ok(match last {
            Some(ObjectKind::Database) => {
                let path = parent.child(ObjectKind::Folder, folder::TABLES);
                vec![ObjectNode {
                    path,
                    kind: ObjectKind::Folder,
                    name: folder::TABLES.into(),
                    has_children: true,
                    detail: None,
                }]
            }
            Some(ObjectKind::Folder) => ["accounts", "items"]
                .iter()
                .map(|n| ObjectNode {
                    path: parent.child(ObjectKind::Table, *n),
                    kind: ObjectKind::Table,
                    name: (*n).into(),
                    has_children: false,
                    detail: None,
                })
                .collect(),
            _ => vec![],
        })
    }
    async fn object_ddl(&self, path: &ObjectPath) -> dbstudio_core::Result<String> {
        let name = &path.last().unwrap().name;
        Ok(format!("CREATE TABLE \"{name}\" (id int, name text);\n"))
    }
    async fn fetch_table(
        &self,
        req: TableDataRequest,
        sink: &mut dyn ResultSink,
    ) -> dbstudio_core::Result<ExecSummary> {
        let col = |n: &str, k| ColumnMeta {
            name: n.into(),
            type_name: String::new(),
            value_kind: k,
            nullable: None,
        };
        sink.on_columns(vec![
            col("id", ValueKind::Int),
            col("name", ValueKind::Text),
        ])?;
        let rows: Vec<Vec<Value>> = if req.offset == 0 {
            vec![
                vec![Value::Int(1), Value::Text("it's".into())],
                vec![Value::Int(2), Value::Null],
            ]
        } else {
            vec![]
        };
        let n = rows.len() as u64;
        sink.on_rows(rows)?;
        sink.on_statement_end(StatementEnd {
            outcome: EndOutcome::Rows {
                row_count: n,
                truncated: false,
            },
            elapsed_ms: 0,
        })?;
        Ok(ExecSummary::default())
    }
    async fn close(&self) -> dbstudio_core::Result<()> {
        Ok(())
    }
}

struct FakeDriver {
    executed: Executed,
    password: Arc<Mutex<Vec<Option<String>>>>,
}

#[async_trait]
impl Driver for FakeDriver {
    fn descriptor(&self) -> DriverDescriptor {
        DriverDescriptor::new(
            DbKind::Postgres,
            Capabilities::planned_for(DbKind::Postgres),
            DriverStatus::Available,
        )
    }
    async fn connect(
        &self,
        _config: &ConnectionConfig,
        secret: Option<&Secret>,
    ) -> dbstudio_core::Result<Box<dyn Session>> {
        self.password
            .lock()
            .unwrap()
            .push(secret.map(|s| s.expose().to_string()));
        Ok(Box::new(FakeSession {
            executed: self.executed.clone(),
        }))
    }
}

struct Fixture {
    deps: CliDeps,
    conn: Uuid,
    dir: tempfile::TempDir,
    executed: Executed,
    passwords: Arc<Mutex<Vec<Option<String>>>>,
}

fn fixture(environment: Option<Environment>) -> Fixture {
    let executed = Arc::new(Mutex::new(Vec::new()));
    let passwords = Arc::new(Mutex::new(Vec::new()));
    let mut registry = DriverRegistry::new();
    registry.register(Arc::new(FakeDriver {
        executed: executed.clone(),
        password: passwords.clone(),
    }));
    let secrets = Arc::new(MemorySecretStore::new());
    let service = Arc::new(ConnectionService::new(
        Store::open_in_memory().unwrap(),
        secrets.clone(),
    ));
    let mut config = ConnectionConfig::new(DbKind::Postgres, "fake-pg", "h");
    config.database = Some("app".into());
    config.environment = environment;
    let conn = config.id;
    service
        .save(
            ConnectionProfile {
                config,
                group_id: None,
                color: None,
                save_password: true,
                has_password: false,
                sort_order: 0,
                created_at: String::new(),
                updated_at: String::new(),
            },
            SecretChange::Set {
                value: Secret::new("db-password"),
            },
        )
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    Fixture {
        deps: CliDeps {
            env: RunEnv {
                service,
                secrets,
                registry: Arc::new(registry),
                locks_dir: dir.path().join("locks"),
            },
            passphrase: Box::new(|var| (var == "TEST_PASS").then(|| Secret::new("job passphrase"))),
            assume_yes: Some(true),
        },
        conn,
        dir,
        executed,
        passwords,
    }
}

fn save_job(f: &Fixture, id: u128, encrypted: bool, keep_last: Option<u32>) -> BackupJob {
    let dest = f.dir.path().join("backups");
    let job = BackupJob {
        id: Uuid::from_u128(id),
        name: "nightly".into(),
        enabled: true,
        spec: BackupSpec {
            connection_id: f.conn,
            targets: vec![ObjectPath::root().child(ObjectKind::Database, "app")],
            engine: BackupEngine::Logical {
                content: ExportContent::Both,
                ddl: Default::default(),
            },
            compression: Compression::Zstd { level: 3 },
            encrypted,
            destination: Destination::LocalDir {
                path: dest.to_string_lossy().into_owned(),
                file_template: "{job}-{date:%H%M%S}.dbsbak".into(),
            },
        },
        schedule: None,
        retention: Retention {
            keep_last,
            max_age_days: None,
        },
        missed_run: MissedRunPolicy::RunOnce,
        notify: NotifyPolicy::default(),
        last_run_at: None,
        next_run_at: None,
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
    };
    f.deps
        .env
        .service
        .store()
        .backup_job_save(&job_to_row(&job, None).unwrap())
        .unwrap();
    job
}

async fn cli(f: &Fixture, args: &[&str]) -> (u8, String) {
    let mut v = vec!["dbstudio-cli", "--secret-store", "memory"];
    v.extend_from_slice(args);
    let parsed = Cli::try_parse_from(v).unwrap();
    let mut out = Vec::new();
    let code = execute_async(&parsed, &f.deps, &mut out).await;
    (code, String::from_utf8(out).unwrap())
}

fn backup_files(f: &Fixture) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(f.dir.path().join("backups"))
        .map(|rd| rd.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

#[tokio::test]
async fn backup_job_runs_with_the_keychain_passphrase_and_lists() {
    let f = fixture(None);
    let job = save_job(&f, 1, true, None);
    // No passphrase in the (fake) keychain: the run fails and is recorded as failed.
    let (code, _) = cli(&f, &["backup", "--job", &job.id.to_string()]).await;
    assert_eq!(code, EXIT_ERROR);
    let runs = f
        .deps
        .env
        .service
        .store()
        .backup_runs_list(Some(job.id), 10)
        .unwrap();
    assert_eq!(
        (runs[0].status.as_str(), runs[0].error_kind.as_deref()),
        ("failed", Some("other"))
    );
    assert!(backup_files(&f).is_empty());

    f.deps
        .env
        .secrets
        .set_account(
            &backup_secret_account(job.id),
            &Secret::new("job passphrase"),
        )
        .unwrap();
    let (code, out) = cli(&f, &["backup", "--job", &job.id.to_string()]).await;
    assert_eq!(code, EXIT_OK, "{out}");
    let files = backup_files(&f);
    assert_eq!(files.len(), 1);
    // The connection password came from the keychain-backed profile.
    assert_eq!(
        f.passwords.lock().unwrap().last().unwrap().as_deref(),
        Some("db-password")
    );
    // The file is encrypted and verifies with the job passphrase only.
    let pass = Secret::new("job passphrase");
    let info = inspect(&files[0], Some(&pass)).await.unwrap();
    assert!(info.encrypted);
    assert_eq!(
        info.preamble.unwrap()["objects"].as_array().unwrap().len(),
        2
    );
    let runs = f
        .deps
        .env
        .service
        .store()
        .backup_runs_list(Some(job.id), 10)
        .unwrap();
    assert_eq!(runs[0].status, "succeeded");
    assert_eq!(runs[0].trigger, "cli");
    assert_eq!(runs[0].rows, Some(4));
    assert_eq!(
        runs[0].file_path.as_deref(),
        Some(files[0].to_str().unwrap())
    );
    let saved = row_to_job(
        &f.deps
            .env
            .service
            .store()
            .backup_job_get(job.id)
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert!(saved.last_run_at.is_some());

    let (code, out) = cli(&f, &["backup", "--list"]).await;
    assert_eq!(code, EXIT_OK);
    assert!(
        out.contains(&job.id.to_string()) && out.contains("nightly") && out.contains("manual"),
        "{out}"
    );

    // Unknown job.
    let (code, _) = cli(
        &f,
        &["backup", "--job", "00000000-0000-0000-0000-0000000000ff"],
    )
    .await;
    assert_eq!(code, EXIT_ERROR);
}

#[tokio::test]
async fn a_running_job_is_locked_and_exits_with_3() {
    let f = fixture(None);
    let job = save_job(&f, 2, false, None);
    let mut lock = dbstudio_backup::runner::open_lock(&f.deps.env.locks_dir, job.id).unwrap();
    let _held = lock.try_write().unwrap();
    let (code, _) = cli(&f, &["backup", "--job", &job.id.to_string()]).await;
    assert_eq!(code, EXIT_LOCKED);
    assert!(backup_files(&f).is_empty());
    assert!(
        f.deps
            .env
            .service
            .store()
            .backup_runs_list(None, 10)
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn retention_keeps_only_the_newest_three_files() {
    let f = fixture(None);
    let job = save_job(&f, 3, false, Some(3));
    for _ in 0..5 {
        let (code, out) = cli(&f, &["backup", "--job", &job.id.to_string()]).await;
        assert_eq!(code, EXIT_OK, "{out}");
        // File names carry seconds: make the runs distinguishable for ordering.
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    }
    let files = backup_files(&f);
    assert_eq!(files.len(), 3, "{files:?}");
    let runs = f
        .deps
        .env
        .service
        .store()
        .backup_runs_list(Some(job.id), 10)
        .unwrap();
    assert_eq!(runs.len(), 5);
    assert_eq!(runs.iter().filter(|r| r.file_path.is_some()).count(), 3);
}

#[tokio::test]
async fn verify_and_restore_with_confirmation_rules() {
    let f = fixture(Some(Environment::Production));
    let job = save_job(&f, 4, true, None);
    f.deps
        .env
        .secrets
        .set_account(
            &backup_secret_account(job.id),
            &Secret::new("job passphrase"),
        )
        .unwrap();
    assert_eq!(
        cli(&f, &["backup", "--job", &job.id.to_string()]).await.0,
        EXIT_OK
    );
    let file = backup_files(&f).remove(0);
    let file = file.to_str().unwrap().to_string();
    let conn = f.conn.to_string();

    // Encrypted: the passphrase variable is required.
    assert_eq!(cli(&f, &["verify", "--file", &file]).await.0, EXIT_USAGE);
    let (code, out) = cli(
        &f,
        &["verify", "--file", &file, "--passphrase-env", "TEST_PASS"],
    )
    .await;
    assert_eq!(code, EXIT_OK, "{out}");

    // Production target: needs --yes and the database name.
    let restore = |extra: &[&'static str]| {
        let mut v = vec![
            "restore",
            "--file",
            &file,
            "--connection",
            &conn,
            "--passphrase-env",
            "TEST_PASS",
        ];
        v.extend_from_slice(extra);
        v.into_iter().map(str::to_string).collect::<Vec<_>>()
    };
    let run = |args: Vec<String>| {
        let f = &f;
        async move {
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            cli(f, &refs).await
        }
    };
    f.executed.lock().unwrap().clear();
    assert_eq!(run(restore(&["--yes"])).await.0, EXIT_USAGE);
    assert_eq!(
        run(restore(&["--yes", "--confirm-text", "wrong"])).await.0,
        EXIT_USAGE
    );
    assert!(
        f.executed.lock().unwrap().is_empty(),
        "nothing may run before confirmation"
    );
    let (code, out) = run(restore(&["--yes", "--confirm-text", "app"])).await;
    assert_eq!(code, EXIT_OK, "{out}");
    let sql: Vec<String> = f
        .executed
        .lock()
        .unwrap()
        .iter()
        .map(|(_, s)| s.clone())
        .collect();
    assert!(
        sql.iter()
            .any(|s| s.starts_with("CREATE TABLE \"accounts\""))
    );
    assert!(
        sql.iter()
            .any(|s| s.contains("INSERT INTO \"accounts\"") && s.contains("'it''s'")),
        "{sql:?}"
    );
    let history = f
        .deps
        .env
        .service
        .store()
        .backup_runs_list(None, 10)
        .unwrap();
    assert!(
        history
            .iter()
            .any(|r| r.kind == "restore" && r.status == "succeeded")
    );
    assert!(
        history
            .iter()
            .any(|r| r.kind == "verify" && r.status == "succeeded")
    );

    // A development target restores without the typed text, `clean` brings it back.
    let dev = fixture(Some(Environment::Development));
    let job = save_job(&dev, 5, false, None);
    assert_eq!(
        cli(&dev, &["backup", "--job", &job.id.to_string()]).await.0,
        EXIT_OK
    );
    let file = backup_files(&dev).remove(0);
    let conn = dev.conn.to_string();
    let file = file.to_str().unwrap();
    assert_eq!(
        cli(
            &dev,
            &["restore", "--file", file, "--connection", &conn, "--yes"]
        )
        .await
        .0,
        EXIT_OK
    );
    assert_eq!(
        cli(
            &dev,
            &[
                "restore",
                "--file",
                file,
                "--connection",
                &conn,
                "--yes",
                "--clean"
            ]
        )
        .await
        .0,
        EXIT_USAGE
    );
    assert_eq!(
        cli(
            &dev,
            &[
                "restore",
                "--file",
                file,
                "--connection",
                &conn,
                "--yes",
                "--clean",
                "--confirm-text",
                "app"
            ]
        )
        .await
        .0,
        EXIT_OK
    );
    let sql: Vec<String> = dev
        .executed
        .lock()
        .unwrap()
        .iter()
        .map(|(_, s)| s.clone())
        .collect();
    assert!(
        sql.iter()
            .any(|s| s.starts_with("DROP TABLE IF EXISTS \"items\""))
    );
}

#[tokio::test]
async fn tools_listing_prints_every_binary_tool() {
    let f = fixture(None);
    let (code, out) = cli(&f, &["tools"]).await;
    assert_eq!(code, EXIT_OK);
    assert_eq!(out.lines().count(), 10);
    assert!(out.contains("pg_dump") && out.contains("mysqldump"));
}
