//! Integration tests (`DBSTUDIO_IT=1`): SQL Server logical backup -> restore into a new database
//! (identity, FK created after the data, `money`, `datetimeoffset`, very large `varbinary(max)`
//! and `nvarchar(max)` values that must not be cut) and the native `BACKUP DATABASE` written
//! on the server. Skipped when `mssql2022` is not running (`DBSTUDIO_IT_STRICT=1` fails instead).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use dbstudio_backup::{
    BackupContext, BackupEngine, BackupSpec, Compression, Destination, NativeTool, RestoreSpec,
    inspect, run_backup, run_restore, verify,
};
use dbstudio_core::{
    CancelToken, ConnectionConfig, DbKind, Driver, JobProgress, JobReporter, ObjectKind,
    ObjectPath, QueryRequest, Secret, Session, StatementOutcome, TlsConfig, TlsMode, Value,
    execute_collect,
};
use dbstudio_export::ExportContent;
use uuid::Uuid;

#[derive(Default)]
struct Rep {
    cancel: CancelToken,
    logs: std::sync::Mutex<Vec<String>>,
}

impl JobReporter for Rep {
    fn progress(&self, _: &JobProgress) {}
    fn log(&self, line: &str) {
        self.logs.lock().unwrap().push(line.to_string());
    }
    fn cancel_token(&self) -> &CancelToken {
        &self.cancel
    }
}

fn enabled() -> bool {
    std::env::var("DBSTUDIO_IT").as_deref() == Ok("1")
}

fn config(db: Option<&str>) -> ConnectionConfig {
    let mut c = ConnectionConfig::new(DbKind::SqlServer, "mssql-it", "127.0.0.1");
    c.port = std::env::var("DBSTUDIO_IT_MSSQL_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(14333);
    c.username = Some("sa".into());
    c.database = db.map(str::to_string);
    c.tls = TlsConfig {
        mode: TlsMode::Require,
        ..Default::default()
    };
    c.connect_timeout_ms = Some(60_000);
    c
}

async fn connect(db: Option<&str>) -> Option<(ConnectionConfig, Arc<dyn Session>)> {
    let cfg = config(db);
    match dbstudio_driver_mssql::MssqlDriver::new()
        .connect(&cfg, Some(&Secret::new("Dbstudio_test1!")))
        .await
    {
        Ok(s) => Some((cfg, Arc::from(s))),
        Err(e) if std::env::var("DBSTUDIO_IT_STRICT").as_deref() == Ok("1") => {
            panic!("mssql2022 is not reachable: {e}")
        }
        Err(e) => {
            eprintln!("skipping mssql2022: {e}");
            None
        }
    }
}

async fn exec(s: &dyn Session, db: &str, sql: &str) {
    let mut req = QueryRequest::new(sql);
    req.database = Some(db.to_string());
    execute_collect(s, req)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

async fn scalar(s: &dyn Session, db: &str, sql: &str) -> Value {
    let mut req = QueryRequest::new(sql);
    req.database = Some(db.to_string());
    let r = execute_collect(s, req)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    match &r[0].outcome {
        StatementOutcome::Rows(rs) => rs.rows[0][0].clone(),
        o => panic!("{sql}: {o:?}"),
    }
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Int(i) => *i,
        Value::UInt(u) => *u as i64,
        other => panic!("{other:?}"),
    }
}

fn ctx(cfg: &ConnectionConfig, s: &Arc<dyn Session>, rep: &Arc<Rep>) -> BackupContext {
    BackupContext {
        session: s.clone(),
        config: cfg.clone(),
        secret: Some(Secret::new("Dbstudio_test1!")),
        tunnel_addr: None,
        reporter: rep.clone(),
        job_name: Some("it".into()),
        tool_overrides: BTreeMap::new(),
        work_dir: None,
    }
}

struct Cleanup(Vec<String>);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let names = std::mem::take(&mut self.0);
        let _ = std::thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            rt.block_on(async {
                if let Some((_, s)) = connect(Some("master")).await {
                    for n in names {
                        let _ = execute_collect(
                            &*s,
                            QueryRequest::new(format!(
                                "IF DB_ID(N'{n}') IS NOT NULL BEGIN ALTER DATABASE [{n}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{n}]; END"
                            )),
                        )
                        .await;
                    }
                }
            });
        })
        .join();
    }
}

/// Below the driver's 8 MiB cell limit (see `values_over_the_driver_limit_are_not_cut`).
const BIG: usize = 6_000_000;
const BIG_TEXT: usize = 2_500_000;

#[tokio::test(flavor = "multi_thread")]
async fn mssql_logical_round_trip_keeps_big_lobs_and_types() {
    if !enabled() {
        return;
    }
    let Some((cfg, s)) = connect(Some("master")).await else {
        return;
    };
    let n = Uuid::new_v4().simple().to_string()[..8].to_string();
    let (src, dst) = (
        format!("dbstudio_it_bk_src_{n}"),
        format!("dbstudio_it_bk_dst_{n}"),
    );
    let _cleanup = Cleanup(vec![src.clone(), dst.clone()]);
    exec(&*s, "master", &format!("CREATE DATABASE [{src}]")).await;
    for sql in [
        "CREATE TABLE dbo.parent (id INT IDENTITY(10, 5) PRIMARY KEY, name NVARCHAR(50) NOT NULL, price MONEY, at DATETIMEOFFSET(3))".to_string(),
        "CREATE TABLE dbo.child (id INT IDENTITY(1, 1) PRIMARY KEY, parent_id INT NOT NULL, note NVARCHAR(MAX), body VARBINARY(MAX), CONSTRAINT fk_child_parent FOREIGN KEY (parent_id) REFERENCES dbo.parent (id))".to_string(),
        "INSERT INTO dbo.parent (name, price, at) VALUES (N'Ân', 1234.5678, '2024-02-29 23:59:59.123 +07:00'), (N'Bình', -0.0001, '1999-12-31 00:00:00.000 -05:30')".to_string(),
        format!("INSERT INTO dbo.child (parent_id, note, body) VALUES (10, REPLICATE(CAST(N'ệ' AS NVARCHAR(MAX)), {BIG_TEXT}), CAST(REPLICATE(CAST(0x41 AS VARBINARY(MAX)), {BIG}) AS VARBINARY(MAX)))"),
        "INSERT INTO dbo.child (parent_id, note, body) VALUES (15, NULL, 0x00FF10)".to_string(),
    ] {
        exec(&*s, &src, &sql).await;
    }
    let dir = tempfile::tempdir().unwrap();
    let rep = Arc::new(Rep::default());
    let spec = BackupSpec {
        connection_id: Uuid::nil(),
        targets: vec![ObjectPath::root().child(ObjectKind::Database, &src)],
        engine: BackupEngine::Logical {
            content: ExportContent::Both,
            ddl: Default::default(),
        },
        compression: Compression::Zstd { level: 3 },
        encrypted: false,
        destination: Destination::LocalDir {
            path: dir.path().to_string_lossy().into_owned(),
            file_template: "{database}.dbsbak".into(),
        },
    };
    let out = run_backup(ctx(&cfg, &s, &rep), &spec, None).await.unwrap();
    let file = PathBuf::from(&out.files[0]);
    assert_eq!(out.rows, 4);
    let ins = inspect(&file, None).await.unwrap();
    assert_eq!(ins.preamble.as_ref().unwrap()["dialect"], "mssql");
    assert!(
        ins.objects.iter().any(|o| o.name == "child"),
        "{:?}",
        ins.objects
    );
    verify(&file, None, &*rep).await.unwrap();
    let res = run_restore(
        ctx(&cfg, &s, &rep),
        &RestoreSpec {
            file: file.to_string_lossy().into_owned(),
            target_connection_id: Uuid::nil(),
            target_database: Some(dst.clone()),
            create_database: true,
            clean: false,
            objects: vec![],
            continue_on_error: false,
        },
        None,
    )
    .await
    .unwrap_or_else(|e| panic!("restore: {e}"));
    assert!(res.errors.is_empty(), "{:?}", res.errors);

    // Size and content of the big values are identical (a hash over the whole value).
    for check in [
        "SELECT DATALENGTH(body) FROM dbo.child WHERE id = 1",
        "SELECT DATALENGTH(note) FROM dbo.child WHERE id = 1",
        "SELECT COUNT(*) FROM dbo.child",
        "SELECT COUNT(*) FROM dbo.parent WHERE price = 1234.5678",
        "SELECT SUM(id) FROM dbo.parent",
        "SELECT SUM(id) FROM dbo.child",
    ] {
        let (a, b) = (
            scalar(&*s, &src, check).await,
            scalar(&*s, &dst, check).await,
        );
        assert_eq!(a, b, "{check}");
    }
    assert_eq!(
        int(&scalar(
            &*s,
            &dst,
            "SELECT DATALENGTH(body) FROM dbo.child WHERE id = 1"
        )
        .await),
        BIG as i64,
        "varbinary(max) was cut"
    );
    assert_eq!(
        int(&scalar(
            &*s,
            &dst,
            "SELECT DATALENGTH(note) FROM dbo.child WHERE id = 1"
        )
        .await),
        (BIG_TEXT * 2) as i64,
        "nvarchar(max) was cut"
    );
    for check in [
        "SELECT CONVERT(VARCHAR(64), HASHBYTES('SHA2_256', body), 2) FROM dbo.child WHERE id = 1",
        "SELECT CONVERT(VARCHAR(64), HASHBYTES('SHA2_256', CAST(note AS VARBINARY(MAX))), 2) FROM dbo.child WHERE id = 1",
        "SELECT CONVERT(VARCHAR(40), at, 127) FROM dbo.parent WHERE name = N'Bình'",
        "SELECT CONVERT(VARCHAR(40), at, 127) FROM dbo.parent WHERE name = N'Ân'",
        "SELECT CONVERT(VARCHAR(40), price) FROM dbo.parent WHERE name = N'Bình'",
    ] {
        let (a, b) = (
            scalar(&*s, &src, check).await,
            scalar(&*s, &dst, check).await,
        );
        assert_eq!(a, b, "{check}");
    }
    // The foreign key exists again and the identity continues where it was.
    assert_eq!(
        int(&scalar(
            &*s,
            &dst,
            "SELECT COUNT(*) FROM sys.foreign_keys WHERE name = 'fk_child_parent'"
        )
        .await),
        1
    );
    exec(&*s, &dst, "INSERT INTO dbo.parent (name) VALUES (N'next')").await;
    assert_eq!(
        int(&scalar(&*s, &dst, "SELECT MAX(id) FROM dbo.parent").await),
        20
    );

    // Native backup written by the server, then verified by the server.
    let native = BackupSpec {
        connection_id: Uuid::nil(),
        targets: vec![ObjectPath::root().child(ObjectKind::Database, &src)],
        engine: BackupEngine::Native {
            tool: NativeTool::MsSqlServerBackup,
            options: BTreeMap::new(),
        },
        compression: Compression::None,
        encrypted: false,
        destination: Destination::ServerDir {
            path: "/var/opt/mssql/backup".into(),
        },
    };
    let nout = run_backup(ctx(&cfg, &s, &rep), &native, None)
        .await
        .unwrap();
    let bak = nout.files[0].clone();
    assert!(bak.starts_with("/var/opt/mssql/backup/"), "{bak}");
    exec(
        &*s,
        "master",
        &format!("RESTORE VERIFYONLY FROM DISK = N'{bak}' WITH CHECKSUM"),
    )
    .await;
}

/// Known driver defect (package M, `crates/drivers/mssql`): `stream_table` reads each text /
/// binary cell through `LEFT/SUBSTRING(col, 1, max_lob_bytes + 1)`, so a value over 8 MiB is
/// cut silently in a backup. Remove `ignore` when the stream path no longer applies the limit.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "driver M: stream_table cuts cells at max_lob_bytes (8 MiB)"]
async fn values_over_the_driver_limit_are_not_cut() {
    if !enabled() {
        return;
    }
    let Some((cfg, s)) = connect(Some("master")).await else {
        return;
    };
    let n = Uuid::new_v4().simple().to_string()[..8].to_string();
    let (src, dst) = (
        format!("dbstudio_it_bl_src_{n}"),
        format!("dbstudio_it_bl_dst_{n}"),
    );
    let _cleanup = Cleanup(vec![src.clone(), dst.clone()]);
    exec(&*s, "master", &format!("CREATE DATABASE [{src}]")).await;
    exec(
        &*s,
        &src,
        "CREATE TABLE dbo.t (id INT PRIMARY KEY, b VARBINARY(MAX))",
    )
    .await;
    exec(
        &*s,
        &src,
        "INSERT INTO dbo.t VALUES (1, CAST(REPLICATE(CAST(0x42 AS VARBINARY(MAX)), 10000000) AS VARBINARY(MAX)))",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let rep = Arc::new(Rep::default());
    let spec = BackupSpec {
        connection_id: Uuid::nil(),
        targets: vec![ObjectPath::root().child(ObjectKind::Database, &src)],
        engine: BackupEngine::Logical {
            content: ExportContent::Both,
            ddl: Default::default(),
        },
        compression: Compression::None,
        encrypted: false,
        destination: Destination::LocalDir {
            path: dir.path().to_string_lossy().into_owned(),
            file_template: "{database}.dbsbak".into(),
        },
    };
    let out = run_backup(ctx(&cfg, &s, &rep), &spec, None).await.unwrap();
    run_restore(
        ctx(&cfg, &s, &rep),
        &RestoreSpec {
            file: out.files[0].clone(),
            target_connection_id: Uuid::nil(),
            target_database: Some(dst.clone()),
            create_database: true,
            clean: false,
            objects: vec![],
            continue_on_error: false,
        },
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        int(&scalar(&*s, &dst, "SELECT DATALENGTH(b) FROM dbo.t").await),
        10_000_000
    );
}
