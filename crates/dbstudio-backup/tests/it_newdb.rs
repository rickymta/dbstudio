//! Backup / restore on the newer databases (ScyllaDB, Cassandra, ClickHouse, CockroachDB,
//! YugabyteDB, TiDB): logical backup -> restore into an empty namespace gives identical rows,
//! partial restore picks tables, clean restore replaces. Only with `DBSTUDIO_IT=1`; a server
//! that is not running skips its test (`DBSTUDIO_IT_STRICT=1` fails instead).

#[path = "../../dbstudio-export/tests/it_newdb/fixtures.rs"]
mod fixtures;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use dbstudio_backup::{
    BackupContext, BackupEngine, BackupSpec, Compression, Destination, NativeTool, RestoreSpec,
    inspect, run_backup, run_restore, verify,
};
use dbstudio_core::{CancelToken, JobProgress, JobReporter, ObjectKind, ObjectPath};
use dbstudio_export::ExportContent;
use fixtures::{E, Fx, probe, run, try_run};
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

fn ctx(fx: &Fx, rep: &Arc<Rep>, tools: BTreeMap<NativeTool, String>) -> BackupContext {
    let (config, secret) = fx.config();
    BackupContext {
        session: fx.admin.clone(),
        config,
        secret,
        tunnel_addr: None,
        reporter: rep.clone(),
        job_name: Some("it".into()),
        tool_overrides: tools,
        work_dir: None,
    }
}

fn spec(ns: &str, dir: &Path, encrypted: bool, engine: BackupEngine) -> BackupSpec {
    BackupSpec {
        connection_id: Uuid::nil(),
        targets: vec![ObjectPath::root().child(ObjectKind::Database, ns)],
        engine,
        compression: Compression::Zstd { level: 3 },
        encrypted,
        destination: Destination::LocalDir {
            path: dir.to_string_lossy().into_owned(),
            file_template: "{database}-{date:%H%M%S}.dbsbak".into(),
        },
    }
}

fn logical() -> BackupEngine {
    BackupEngine::Logical {
        content: ExportContent::Both,
        ddl: Default::default(),
    }
}

fn restore_spec(file: &str, target: &str) -> RestoreSpec {
    RestoreSpec {
        file: file.to_string(),
        target_connection_id: Uuid::nil(),
        target_database: Some(target.to_string()),
        create_database: true,
        clean: false,
        objects: vec![],
        continue_on_error: false,
    }
}

fn new_ns(fx: &Fx, what: &str) -> String {
    let ns = format!(
        "exp_it_{}_{what}_{}",
        fx.label.replace('-', "_"),
        &Uuid::new_v4().simple().to_string()[..8]
    );
    fx.track(&ns);
    ns
}

async fn logical_round_trip(fx: &Fx) {
    let dir = tempfile::tempdir().unwrap();
    let rep = Arc::new(Rep::default());
    let pass = dbstudio_core::Secret::new("correct horse");
    for encrypted in [false, true] {
        let p = encrypted.then_some(&pass);
        let out = run_backup(
            ctx(fx, &rep, BTreeMap::new()),
            &spec(&fx.src.ns, dir.path(), encrypted, logical()),
            p,
        )
        .await
        .unwrap_or_else(|e| panic!("[{}] backup: {e}", fx.label));
        assert_eq!(out.rows, fx.total_rows(), "[{}] rows backed up", fx.label);
        let file = PathBuf::from(&out.files[0]);
        let ins = inspect(&file, p).await.unwrap();
        assert_eq!(ins.encrypted, encrypted);
        let pre = ins.preamble.clone().unwrap();
        assert_eq!(pre["database"], fx.src.ns.as_str());
        assert_eq!(pre["formatVersion"], 3);
        verify(&file, p, &*rep).await.unwrap();
        let tables = ins
            .objects
            .iter()
            .filter(|o| o.kind == ObjectKind::Table)
            .count();
        assert_eq!(
            tables,
            fx.tables().len(),
            "[{}] {:?}",
            fx.label,
            ins.objects
        );

        let dst_ns = new_ns(fx, "rt");
        let res = run_restore(
            ctx(fx, &rep, BTreeMap::new()),
            &restore_spec(&file.to_string_lossy(), &dst_ns),
            p,
        )
        .await
        .unwrap_or_else(|e| panic!("[{}] restore: {e}", fx.label));
        assert!(res.errors.is_empty(), "[{}] {:?}", fx.label, res.errors);
        let dst = fx.attach(&dst_ns).await;
        assert_same(fx, &dst).await;
        assert_sequence(fx, &dst).await;
    }
}

/// A sequence that was used keeps counting after the restore (PostgreSQL protocol servers).
async fn assert_sequence(fx: &Fx, dst: &fixtures::Dst) {
    if !fx.e.is_pg() {
        return;
    }
    let next = fixtures::scalar(&*dst.session, None, "SELECT nextval('exp_seq')").await;
    let next: i64 = next.parse().unwrap();
    if fx.e == E::Yugabyte {
        // YugabyteDB hands out sequence values in cached blocks of 100: the restored copy
        // continues after the block, never inside what was already used.
        assert!(next >= 103, "[{}] next value {next}", fx.label);
    } else {
        assert_eq!(next, 103, "[{}] the sequence value is restored", fx.label);
    }
}

async fn assert_same(fx: &Fx, dst: &fixtures::Dst) {
    let want = fx.contents(&fx.src).await;
    let got = fx.contents(dst).await;
    for ((t, w), (_, g)) in want.iter().zip(&got) {
        assert_eq!(w.len(), g.len(), "[{}] row count of {t}", fx.label);
        for (i, (a, b)) in w.iter().zip(g).enumerate() {
            assert_eq!(a, b, "[{}] {t} differs at sorted row {i}", fx.label);
        }
    }
}

/// Only the chosen table (and what it needs) is restored.
async fn partial_restore(fx: &Fx) {
    let dir = tempfile::tempdir().unwrap();
    let rep = Arc::new(Rep::default());
    let out = run_backup(
        ctx(fx, &rep, BTreeMap::new()),
        &spec(&fx.src.ns, dir.path(), false, logical()),
        None,
    )
    .await
    .unwrap();
    let file = out.files[0].clone();
    let tables = fx.tables();
    // The table that has no foreign key to the other one.
    let (keep, keep_rows) = tables[0];
    let (other, _) = tables[1];
    let dst_ns = new_ns(fx, "part");
    let mut rs = restore_spec(&file, &dst_ns);
    rs.objects = vec![keep.to_string()];
    let res = run_restore(ctx(fx, &rep, BTreeMap::new()), &rs, None)
        .await
        .unwrap_or_else(|e| panic!("[{}] partial restore: {e}", fx.label));
    assert!(res.errors.is_empty(), "[{}] {:?}", fx.label, res.errors);
    assert_eq!(res.objects, 1, "[{}]", fx.label);
    let dst = fx.attach(&dst_ns).await;
    assert_eq!(
        fx.content(&dst, keep).await,
        fx.content(&fx.src, keep).await,
        "[{}] {keep}",
        fx.label
    );
    assert_eq!(fx.content(&dst, keep).await.len() as u64, keep_rows);
    let mut req = dbstudio_core::QueryRequest::new(format!("SELECT * FROM {}{other}", dst.q));
    req.database = dst.script_db.clone();
    let missing = dbstudio_core::execute_collect(&*dst.session, req).await;
    assert!(
        missing.is_err(),
        "[{}] {other} must not exist after a partial restore",
        fx.label
    );
    // `clean` with selected objects would lose everything else.
    let mut dirty = rs.clone();
    dirty.clean = true;
    dirty.create_database = false;
    assert!(
        run_restore(ctx(fx, &rep, BTreeMap::new()), &dirty, None)
            .await
            .is_err()
    );
    // A name that is not in the backup is refused.
    let mut bad = rs.clone();
    bad.objects = vec!["nope".into()];
    assert!(
        run_restore(ctx(fx, &rep, BTreeMap::new()), &bad, None)
            .await
            .is_err()
    );
}

/// `clean` replaces what is in the target (not available for CQL: refused with a clear message).
async fn clean_restore(fx: &Fx) {
    let dir = tempfile::tempdir().unwrap();
    let rep = Arc::new(Rep::default());
    let out = run_backup(
        ctx(fx, &rep, BTreeMap::new()),
        &spec(&fx.src.ns, dir.path(), false, logical()),
        None,
    )
    .await
    .unwrap();
    let dst = fx.fresh("clean").await;
    // Something that is not in the backup: it must be gone afterwards.
    let junk = match fx.e {
        E::Scylla | E::Cassandra => "CREATE TABLE {q}junk (id int PRIMARY KEY)",
        E::ClickHouse => "CREATE TABLE {q}junk (id UInt8) ENGINE = Memory",
        _ => "CREATE TABLE {q}junk (id INT PRIMARY KEY)",
    };
    run(
        &*dst.session,
        dst.script_db.as_deref(),
        &junk.replace("{q}", &dst.q),
    )
    .await;
    let mut rs = restore_spec(&out.files[0], &dst.ns);
    rs.create_database = false;
    rs.clean = true;
    let res = run_restore(ctx(fx, &rep, BTreeMap::new()), &rs, None).await;
    if fx.e.is_cql() {
        let e = res.expect_err("clean is not available for CQL");
        assert!(e.message().contains("clean"), "{}", e.message());
        // The target is untouched.
        run(
            &*dst.session,
            dst.script_db.as_deref(),
            &format!("SELECT * FROM {}junk", dst.q),
        )
        .await;
        return;
    }
    let res = res.unwrap_or_else(|e| panic!("[{}] clean restore: {e}", fx.label));
    assert!(res.errors.is_empty(), "[{}] {:?}", fx.label, res.errors);
    let dst = fx.attach(&dst.ns).await;
    assert_same(fx, &dst).await;
    let mut req = dbstudio_core::QueryRequest::new(format!("SELECT * FROM {}junk", dst.q));
    req.database = dst.script_db.clone();
    assert!(
        dbstudio_core::execute_collect(&*dst.session, req)
            .await
            .is_err(),
        "[{}] clean must drop what the backup does not contain",
        fx.label
    );
}

/// Native backups. ClickHouse and CockroachDB write on the server (and are verified with the
/// server's own `RESTORE`); TiDB goes through `mysqldump` and YugabyteDB through `ysql_dump`
/// (shims that forward to a container, as the host has no client tools).
async fn native(fx: &Fx) {
    let rep = Arc::new(Rep::default());
    match fx.e {
        E::ClickHouse => {
            let mut sp = spec(
                &fx.src.ns,
                Path::new("."),
                false,
                BackupEngine::Native {
                    tool: NativeTool::ClickHouseServerBackup,
                    options: BTreeMap::new(),
                },
            );
            sp.destination = Destination::ServerDir {
                path: "backups".into(),
            };
            let out = run_backup(ctx(fx, &rep, BTreeMap::new()), &sp, None)
                .await
                .unwrap_or_else(|e| panic!("[{}] native backup: {e}", fx.label));
            let (disk, file) = out.files[0].split_once('/').unwrap();
            let dst_ns = new_ns(fx, "nat");
            run(
                &*fx.admin,
                None,
                &format!(
                    "RESTORE DATABASE `{}` AS `{dst_ns}` FROM Disk('{disk}', '{file}')",
                    fx.src.ns
                ),
            )
            .await;
            let dst = fx.attach(&dst_ns).await;
            assert_same(fx, &dst).await;
            // A path that is not a plain disk name is refused before anything runs.
            let mut bad = sp.clone();
            bad.destination = Destination::ServerDir {
                path: "x', 'y".into(),
            };
            assert!(
                run_backup(ctx(fx, &rep, BTreeMap::new()), &bad, None)
                    .await
                    .is_err()
            );
        }
        E::Cockroach => {
            let mut sp = spec(
                &fx.src.ns,
                Path::new("."),
                false,
                BackupEngine::Native {
                    tool: NativeTool::CockroachServerBackup,
                    options: BTreeMap::new(),
                },
            );
            sp.destination = Destination::ServerDir {
                path: "nodelocal://1/dbstudio-it".into(),
            };
            let out = run_backup(ctx(fx, &rep, BTreeMap::new()), &sp, None)
                .await
                .unwrap_or_else(|e| panic!("[{}] native backup: {e}", fx.label));
            let uri = out.files[0].clone();
            let dst_ns = new_ns(fx, "nat");
            run(
                &*fx.admin,
                None,
                &format!(
                    "RESTORE DATABASE \"{}\" FROM LATEST IN '{uri}' WITH new_db_name = '{dst_ns}'",
                    fx.src.ns
                ),
            )
            .await;
            let dst = fx.attach(&dst_ns).await;
            assert_same(fx, &dst).await;
            let mut bad = sp.clone();
            bad.destination = Destination::ServerDir {
                path: "s3://bucket/x?AWS_SECRET_ACCESS_KEY=1".into(),
            };
            assert!(
                run_backup(ctx(fx, &rep, BTreeMap::new()), &bad, None)
                    .await
                    .is_err()
            );
        }
        E::TiDb | E::Yugabyte => native_shim(fx, &rep).await,
        _ => {
            // CQL: no native tool (nodetool snapshots are file level on the node). A tool of
            // another database that is installed is refused with the reason.
            let tools = BTreeMap::from([(NativeTool::MySqlDump, "/bin/echo".to_string())]);
            let e = run_backup(
                ctx(fx, &rep, tools),
                &spec(
                    &fx.src.ns,
                    Path::new("."),
                    false,
                    BackupEngine::Native {
                        tool: NativeTool::MySqlDump,
                        options: BTreeMap::new(),
                    },
                ),
                None,
            )
            .await
            .unwrap_err();
            assert!(
                e.message().contains("cannot back up"),
                "[{}] {}",
                fx.label,
                e.message()
            );
        }
    }
}

#[cfg(unix)]
async fn native_shim(fx: &Fx, rep: &Arc<Rep>) {
    use std::os::unix::fs::PermissionsExt;
    let shims = tempfile::tempdir().unwrap();
    let (dump, restore, dump_tool, restore_tool, body) = match fx.e {
        E::TiDb => (
            "mysqldump",
            "mysql",
            NativeTool::MySqlDump,
            NativeTool::MySqlClient,
            // Runs the client of the MySQL 8 container against the TiDB service on the compose
            // network (`host.docker.internal` does not resolve on Linux runners).
            r#"#!/bin/bash
args=(); pw=""
while [ $# -gt 0 ]; do case "$1" in
  --defaults-extra-file=*) f="${1#--defaults-extra-file=}"; pw=$(sed -n 's/^password="\(.*\)"$//p' "$f"); shift;;
  -h|-P) shift 2;; --ssl-mode=*) shift;; *) args+=("$1"); shift;; esac; done
exec docker exec -i -e MYSQL_PWD="$pw" dbstudio-test-mysql8-1 TOOL -h tidb -P 4000 "${args[@]}"
"#
            .to_string(),
        ),
        _ => (
            "ysql_dump",
            "ysqlsh",
            NativeTool::YsqlDump,
            NativeTool::Ysqlsh,
            // Inside the container the server listens on its own address, not on localhost.
            r#"#!/bin/bash
[ "$1" = "--version" ] && exec docker exec -i dbstudio-test-yugabyte-1 /home/yugabyte/postgres/bin/TOOL --version
args=(); while [ $# -gt 0 ]; do case "$1" in -h|-p) shift 2;; *) args+=("$1"); shift;; esac; done
exec docker exec -i -e PGPASSWORD dbstudio-test-yugabyte-1 bash -c 'exec /home/yugabyte/postgres/bin/TOOL -h $(hostname) -p 5433 "$@"' _ "${args[@]}"
"#
            .to_string(),
        ),
    };
    let mut tools = BTreeMap::new();
    for (tool, name) in [(dump_tool, dump), (restore_tool, restore)] {
        let path = shims.path().join(name);
        std::fs::write(&path, body.replace("TOOL", name)).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        tools.insert(tool, path.to_string_lossy().into_owned());
    }
    let dir = tempfile::tempdir().unwrap();
    let out = run_backup(
        ctx(fx, rep, tools.clone()),
        &spec(
            &fx.src.ns,
            dir.path(),
            false,
            BackupEngine::Native {
                tool: dump_tool,
                options: BTreeMap::new(),
            },
        ),
        None,
    )
    .await
    .unwrap_or_else(|e| panic!("[{}] native backup: {e}", fx.label));
    let file = out.files[0].clone();
    assert_eq!(
        inspect(Path::new(&file), None).await.unwrap().native,
        Some(dump_tool)
    );
    let dst_ns = new_ns(fx, "nat");
    let res = run_restore(ctx(fx, rep, tools), &restore_spec(&file, &dst_ns), None)
        .await
        .unwrap_or_else(|e| panic!("[{}] native restore: {e}", fx.label));
    assert!(res.errors.is_empty(), "{:?}", res.errors);
    let dst = fx.attach(&dst_ns).await;
    assert_same(fx, &dst).await;
}

#[cfg(not(unix))]
async fn native_shim(_fx: &Fx, _rep: &Arc<Rep>) {}

async fn all(e: E, label: &str) {
    if !probe(e) {
        return;
    }
    let fx = Fx::new(e, label).await;
    logical_round_trip(&fx).await;
    partial_restore(&fx).await;
    clean_restore(&fx).await;
    native(&fx).await;
    let _ = try_run;
}

#[tokio::test(flavor = "multi_thread")]
async fn clickhouse() {
    all(E::ClickHouse, "ch").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn tidb() {
    all(E::TiDb, "tidb").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn scylla() {
    all(E::Scylla, "scylla").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cassandra5() {
    all(E::Cassandra, "cas5").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cockroach() {
    all(E::Cockroach, "crdb").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn yugabyte() {
    all(E::Yugabyte, "yb").await;
}
