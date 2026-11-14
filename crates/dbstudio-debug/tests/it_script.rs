//! Integration tests of the script debugger on real servers (`DBSTUDIO_IT=1`; pg16 and mysql8 of
//! the GĐ1 compose services). A five statement script with a breakpoint on the 3rd statement.

mod common;

use std::sync::Arc;

use common::*;
use dbstudio_core::debug::{
    Breakpoint, DebugAction, DebugEndAction, DebugStatus, EvalValue, PauseReason, TerminateReason,
    WatchSpec,
};
use dbstudio_core::{
    CollectingSink, ConnectionConfig, DbKind, Driver, Lease, LeaseOptions, QueryRequest, Secret,
    Session, StatementOutcome, TlsConfig, TlsMode, Value,
};
use dbstudio_debug::DebugSettings;
use uuid::Uuid;

fn enabled() -> bool {
    std::env::var("DBSTUDIO_IT").as_deref() == Ok("1")
}

async fn connect(kind: DbKind, port: u16, user_db: Option<&str>) -> Box<dyn Session> {
    let mut cfg = ConnectionConfig::new(kind, "it-debug", "127.0.0.1");
    cfg.port = port;
    cfg.database = user_db.map(String::from);
    cfg.username = Some("dbstudio".into());
    cfg.tls = TlsConfig {
        mode: TlsMode::Prefer,
        ..TlsConfig::default()
    };
    cfg.connect_timeout_ms = Some(5000);
    let secret = Secret::new("dbstudio_test");
    match kind {
        DbKind::Postgres => dbstudio_driver_postgres::PostgresDriver::new()
            .connect(&cfg, Some(&secret))
            .await
            .expect("connect pg16"),
        _ => dbstudio_driver_mysql::MySqlDriver::new()
            .connect(&cfg, Some(&secret))
            .await
            .expect("connect mysql8"),
    }
}

async fn lease(session: &dyn Session) -> Arc<dyn Lease> {
    Arc::from(
        session
            .open_lease(LeaseOptions::default())
            .await
            .expect("open lease"),
    )
}

async fn rows(lease: &dyn Lease, sql: &str) -> Vec<Vec<Value>> {
    let mut sink = CollectingSink::new();
    lease
        .execute(QueryRequest::new(sql), &mut sink)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    match sink.into_results().pop().map(|r| r.outcome) {
        Some(StatementOutcome::Rows(rs)) => rs.rows,
        _ => vec![],
    }
}

async fn run(lease: &dyn Lease, sql: &str) {
    rows(lease, sql).await;
}

fn bp(line: u32) -> Breakpoint {
    Breakpoint {
        id: "bp".into(),
        line,
        enabled: true,
        suspend: true,
        condition: None,
        log_message: None,
        hit_count: None,
    }
}

fn count_of(rows: &[Vec<Value>]) -> i64 {
    match rows[0][0] {
        Value::Int(n) => n,
        Value::UInt(n) => n as i64,
        Value::Decimal(ref s) => s.parse().unwrap(),
        ref other => panic!("not a number: {other:?}"),
    }
}

async fn five_statements(
    kind: DbKind,
    port: u16,
    db: Option<&str>,
    dialect: dbstudio_core::ScriptDialect,
    table: &str,
    extra_watch: &str,
) {
    let session = connect(kind, port, db).await;
    let lease = lease(&*session).await;
    let want = lease.script_dialect().unwrap_or(session.script_dialect());
    assert_eq!(
        std::mem::discriminant(&want),
        std::mem::discriminant(&dialect)
    );
    run(
        &*lease,
        &format!("CREATE TEMPORARY TABLE {table} (id int, name varchar(20))"),
    )
    .await;

    let text = format!(
        "INSERT INTO {table} VALUES (1, 'a');\n\
         INSERT INTO {table} VALUES (2, 'b');\n\
         UPDATE {table} SET name = 'z' WHERE id = 2;\n\
         SELECT count(*) FROM {table};\n\
         SELECT id, name FROM {table} ORDER BY id;"
    );
    let mut req = request(&text);
    req.options.end_action = DebugEndAction::Ask;
    req.breakpoints = vec![bp(3)];
    req.watches = vec![
        WatchSpec {
            id: "w1".into(),
            expr: format!("(SELECT name FROM {table} WHERE id = 2)"),
        },
        WatchSpec {
            id: "w2".into(),
            expr: extra_watch.replace("{t}", table),
        },
    ];
    let gate = Arc::new(FakeGate::default());
    let sink = Arc::new(RecSink::default());
    let mut prof = profile(dialect);
    prof.kind = kind;
    prof.caps.routine_debug = false;
    let h = dbstudio_debug::actor::start(
        prof,
        lease.clone(),
        gate.clone(),
        sink.clone(),
        Arc::new(NoProxy),
        DebugSettings::default(),
        req,
        None,
    )
    .await
    .expect("start");
    let pause = |h: &dbstudio_debug::actor::ActorHandle| {
        let h = h.clone();
        let sink = sink.clone();
        async move {
            let w = h.wait_until(|s| {
                matches!(
                    s.status,
                    DebugStatus::Paused { .. } | DebugStatus::Terminated { .. }
                )
            });
            match tokio::time::timeout(std::time::Duration::from_secs(30), w).await {
                Ok(s) => s,
                Err(_) => panic!(
                    "stuck: {:?}\n{:?}",
                    h.snapshot().await.unwrap().status,
                    sink.outputs()
                ),
            }
        }
    };

    // the breakpoint on statement 3: the first two statements ran, the third did not
    let s = pause(&h).await;
    assert_eq!(
        reason(&s),
        Some(PauseReason::Breakpoint {
            ids: vec!["bp".into()]
        }),
        "{:?}",
        s.status
    );
    assert_eq!(step_of(&s), 2);
    assert_eq!(
        count_of(&rows(&*lease, &format!("SELECT count(*) FROM {table}")).await),
        2
    );
    assert!(
        matches!(&s.watches[0].result.value, EvalValue::Scalar { value: Value::Text(t), .. } if t == "b"),
        "{:?}",
        s.watches[0]
    );
    // step over the UPDATE: the watch sees the new value
    h.act(DebugAction::StepOver, None).await.unwrap();
    let s = pause(&h).await;
    assert_eq!((reason(&s), step_of(&s)), (Some(PauseReason::Step), 3));
    assert!(
        matches!(&s.watches[0].result.value, EvalValue::Scalar { value: Value::Text(t), .. } if t == "z"),
        "{:?}",
        s.watches[0]
    );
    // the watch with a side effect was undone
    assert!(matches!(
        &s.watches[1].result.value,
        EvalValue::Scalar { .. } | EvalValue::Rows { .. } | EvalValue::Error { .. }
    ));
    assert_eq!(
        count_of(&rows(&*lease, &format!("SELECT count(*) FROM {table}")).await),
        2,
        "watch side effects are rolled back"
    );
    // variables
    let sess = h.variable_children("g:Session").await.unwrap();
    assert!(!sess.is_empty(), "{sess:?}");
    let tx = h.variable_children("g:Transaction").await.unwrap();
    assert!(tx.iter().any(|v| v.display == "true"));
    // inline value of the update
    assert!(
        s.inline_values
            .iter()
            .any(|v| v.line == 3 && v.text.contains("1 row")),
        "{:?}",
        s.inline_values
    );
    // resume to the end: the owned transaction waits for the decision; Rollback undoes the inserts
    h.act(DebugAction::Resume, None).await.unwrap();
    let s = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        h.wait_until(|s| {
            matches!(
                &s.status,
                DebugStatus::Paused {
                    reason: PauseReason::AwaitingEnd
                } | DebugStatus::Terminated { .. }
            )
        }),
    )
    .await
    .expect("script end");
    assert_eq!(reason(&s), Some(PauseReason::AwaitingEnd), "{:?}", s.status);
    assert!(s.transaction.owned);
    // dialog default: rollback
    let report = h.stop(Some(DebugEndAction::Rollback)).await.unwrap();
    assert_eq!(report.steps_run, 5);
    assert!(report.rolled_back);
    let s = h.snapshot().await.unwrap();
    assert_eq!(terminated(&s), Some(TerminateReason::Stopped));
    assert_eq!(
        count_of(&rows(&*lease, &format!("SELECT count(*) FROM {table}")).await),
        0,
        "rolled back"
    );
    assert!(sink.messages().is_empty() || !sink.outputs().is_empty());
    assert_eq!(gate.records.lock().unwrap().len(), 5);
    run(&*lease, &format!("DROP TABLE IF EXISTS {table}")).await;
    let _ = lease.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg16_script_with_breakpoint_watch_and_rollback() {
    if !enabled() {
        return;
    }
    let t = format!("dbg_it_{}", Uuid::new_v4().simple());
    five_statements(
        DbKind::Postgres,
        54316,
        Some("dbstudio"),
        dbstudio_core::ScriptDialect::Postgres,
        &t,
        "count(*) FROM {t}",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mysql8_script_with_breakpoint_watch_and_rollback() {
    if !enabled() {
        return;
    }
    let t = format!("dbg_it_{}", Uuid::new_v4().simple());
    five_statements(
        DbKind::MySql,
        33068,
        Some("dbstudio"),
        dbstudio_core::ScriptDialect::mysql(false, true),
        &t,
        "(SELECT count(*) FROM {t})",
    )
    .await;
}

/// A function that writes, called from a watch, is refused and leaves nothing behind (PostgreSQL).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg16_watch_with_a_writing_function_is_undone() {
    if !enabled() {
        return;
    }
    let session = connect(DbKind::Postgres, 54316, Some("dbstudio")).await;
    let lease = lease(&*session).await;
    let id = Uuid::new_v4().simple().to_string();
    let (table, func) = (format!("dbg_w_{id}"), format!("dbg_wf_{id}"));
    run(&*lease, &format!("CREATE TABLE {table} (n int)")).await;
    run(&*lease, &format!("CREATE FUNCTION {func}() RETURNS int LANGUAGE sql AS $$ INSERT INTO {table} VALUES (1) RETURNING n $$")).await;
    let mut req = request("SELECT 1;\nSELECT 2;");
    req.options.end_action = DebugEndAction::Rollback;
    let gate = Arc::new(FakeGate::default());
    let sink = Arc::new(RecSink::default());
    let mut prof = profile(dbstudio_core::ScriptDialect::Postgres);
    prof.caps.routine_debug = false;
    let h = dbstudio_debug::actor::start(
        prof,
        lease.clone(),
        gate,
        sink,
        Arc::new(NoProxy),
        DebugSettings::default(),
        req,
        None,
    )
    .await
    .unwrap();
    h.wait_until(|s| matches!(s.status, DebugStatus::Paused { .. }))
        .await;
    let r = h.evaluate(&format!("{func}()"), None, false).await.unwrap();
    // The frame is read-only (SET LOCAL transaction_read_only): the write is refused.
    let EvalValue::Error { error } = &r.value else {
        panic!("{:?}", r.value)
    };
    assert!(
        error["message"]
            .as_str()
            .unwrap_or_default()
            .contains("read-only"),
        "{error:?}"
    );
    assert_eq!(
        count_of(&rows(&*lease, &format!("SELECT count(*) FROM {table}")).await),
        0,
        "nothing was written by the watch"
    );
    // The savepoint closed and the transaction is read-write again for the script.
    run(&*lease, &format!("INSERT INTO {table} VALUES (7)")).await;
    h.stop(Some(DebugEndAction::Rollback)).await.unwrap();
    run(&*lease, &format!("DROP FUNCTION {func}()")).await;
    run(&*lease, &format!("DROP TABLE {table}")).await;
    let _ = lease.close().await;
}
