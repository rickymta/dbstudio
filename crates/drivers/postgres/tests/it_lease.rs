//! Integration tests of the PostgreSQL lease (dedicated editor connection) against `pg16`/`pg12`.
//! They only run when `DBSTUDIO_IT=1`.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use dbstudio_core::{
    CollectingSink, EndOutcome, Error, Lease, LeaseContext, LeaseOptions, QueryRequest, Session,
    StatementOutcome, TxOptions, TxStatus, Value,
};
use uuid::Uuid;

macro_rules! on_both {
    ($f:ident, $n16:ident, $n12:ident) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $n16() {
            if enabled() {
                $f(PG16).await
            }
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $n12() {
            if enabled() {
                $f(PG12).await
            }
        }
    };
}

fn unique(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::new_v4().simple())
}

async fn open(session: &dyn Session, auto_commit: bool) -> Box<dyn Lease> {
    session
        .open_lease(LeaseOptions {
            database: None,
            schema: None,
            auto_commit,
        })
        .await
        .expect("open lease")
}

async fn run(lease: &dyn Lease, sql: &str) -> Vec<dbstudio_core::StatementResult> {
    let mut sink = CollectingSink::new();
    lease
        .execute(QueryRequest::new(sql), &mut sink)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    sink.into_results()
}

async fn count(session: &dyn Session, sql: &str) -> i64 {
    match scalar(session, sql).await {
        Value::Int(n) => n,
        other => panic!("not an int: {other:?}"),
    }
}

// ----------------------------------------------------------------------------------------------

async fn script_of_three(port: u16) {
    let s = connect(port).await;
    let lease = open(&*s, true).await;
    let table = unique("lease_script");
    let results = run(
        &*lease,
        &format!(
            "CREATE TABLE {table} (id int); INSERT INTO {table} VALUES (1),(2),(3); \
             DO $$ BEGIN RAISE NOTICE 'hello from lease'; END $$; SELECT count(*) FROM {table}"
        ),
    )
    .await;
    assert_eq!(results.len(), 4);
    assert!(matches!(results[0].outcome, StatementOutcome::Done));
    assert!(matches!(
        results[1].outcome,
        StatementOutcome::Affected { rows: 3 }
    ));
    assert!(
        results[2]
            .messages
            .iter()
            .any(|m| m.contains("NOTICE") && m.contains("hello from lease")),
        "{:?}",
        results[2].messages
    );
    match &results[3].outcome {
        StatementOutcome::Rows(rs) => assert_eq!(rs.rows[0][0], Value::Int(3)),
        other => panic!("{other:?}"),
    }
    exec(&*s, &format!("DROP TABLE {table}")).await;
    let report = lease.close().await.unwrap();
    assert!(!report.rolled_back && !report.forced);
    // Auto-commit: statement start events carry ranges.
}
on_both!(script_of_three, script_of_three_16, script_of_three_12);

async fn statement_start_events(port: u16) {
    #[derive(Default)]
    struct Starts(Vec<(u32, u32, u32)>, CollectingSink);
    impl dbstudio_core::ResultSink for Starts {
        fn on_columns(&mut self, c: Vec<dbstudio_core::ColumnMeta>) -> dbstudio_core::Result<()> {
            self.1.on_columns(c)
        }
        fn on_rows(&mut self, r: Vec<Vec<Value>>) -> dbstudio_core::Result<()> {
            self.1.on_rows(r)
        }
        fn on_message(&mut self, m: String) -> dbstudio_core::Result<()> {
            self.1.on_message(m)
        }
        fn on_statement_end(
            &mut self,
            e: dbstudio_core::StatementEnd,
        ) -> dbstudio_core::Result<()> {
            self.1.on_statement_end(e)
        }
        fn on_statement_start(
            &mut self,
            s: dbstudio_core::StatementStart,
        ) -> dbstudio_core::Result<()> {
            self.0.push((s.index, s.range.start, s.range.end));
            Ok(())
        }
    }
    let s = connect(port).await;
    let lease = open(&*s, true).await;
    let mut sink = Starts::default();
    lease
        .execute(
            QueryRequest::new("select 1;\nselect 'é😀'; select 3"),
            &mut sink,
        )
        .await
        .unwrap();
    assert_eq!(sink.0, [(0, 0, 8), (1, 10, 21), (2, 23, 31)]);
    lease.close().await.unwrap();
}
on_both!(statement_start_events, starts_16, starts_12);

async fn backend_activity(s: &dyn Session, pid: i64) -> (String, Option<String>) {
    let r = rows(
        s,
        &format!("SELECT state, wait_event FROM pg_stat_activity WHERE pid = {pid}"),
    )
    .await;
    let get = |v: &Value| match v {
        Value::Text(t) => Some(t.clone()),
        _ => None,
    };
    (get(&r[0][0]).unwrap_or_default(), get(&r[0][1]))
}

async fn cancel_sleep(port: u16) {
    let s = connect(port).await;
    let lease: Arc<dyn Lease> = Arc::from(open(&*s, true).await);
    let pid = match &run(&*lease, "SELECT pg_backend_pid()").await[0].outcome {
        StatementOutcome::Rows(rs) => match rs.rows[0][0] {
            Value::Int(n) => n,
            ref other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    };
    let req = QueryRequest::new("SELECT pg_sleep(60)");
    let id = req.query_id;
    let runner = {
        let lease = lease.clone();
        tokio::spawn(async move { lease.execute(req, &mut CollectingSink::new()).await })
    };
    // Wait until the server is really sleeping on behalf of the lease.
    let started = Instant::now();
    loop {
        let (state, wait) = backend_activity(&*s, pid).await;
        if state == "active" && wait.as_deref() == Some("PgSleep") {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(10), "never started");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let cancelled_at = Instant::now();
    lease.cancel(id).await.unwrap();
    let result = runner.await.unwrap();
    assert!(matches!(result, Err(Error::Cancelled)), "{result:?}");
    assert!(
        cancelled_at.elapsed() < Duration::from_secs(2),
        "{:?}",
        cancelled_at.elapsed()
    );
    // The server confirms: that backend no longer sleeps.
    let (state, wait) = backend_activity(&*s, pid).await;
    assert_ne!(wait.as_deref(), Some("PgSleep"), "{state}");
    assert_ne!(state, "active");
    // The lease is usable again.
    let r = run(&*lease, "SELECT 42").await;
    assert_eq!(r.len(), 1);
    lease.close().await.unwrap();
}
on_both!(cancel_sleep, cancel_16, cancel_12);

async fn timeout_cancels(port: u16) {
    let s = connect(port).await;
    let lease = open(&*s, true).await;
    let mut req = QueryRequest::new("SELECT pg_sleep(60)");
    req.timeout_ms = Some(300);
    let started = Instant::now();
    let err = lease
        .execute(req, &mut CollectingSink::new())
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Timeout), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(3));
    run(&*lease, "SELECT 1").await;
    lease.close().await.unwrap();
}
on_both!(timeout_cancels, timeout_16, timeout_12);

async fn manual_commit_and_rollback(port: u16) {
    let s = connect(port).await;
    let table = unique("lease_tx");
    exec(&*s, &format!("CREATE TABLE {table} (id int, v text)")).await;
    exec(&*s, &format!("INSERT INTO {table} VALUES (1, 'old')")).await;

    let lease = open(&*s, false).await;
    assert!(!lease.state().auto_commit);
    assert_eq!(lease.state().transaction, TxStatus::Idle);
    run(
        &*lease,
        &format!("UPDATE {table} SET v = 'new' WHERE id = 1"),
    )
    .await;
    // Lazy BEGIN happened.
    assert_eq!(lease.state().transaction, TxStatus::Active);
    // Another connection does not see the change.
    assert_eq!(
        scalar(&*s, &format!("SELECT v FROM {table} WHERE id = 1")).await,
        Value::Text("old".into())
    );
    // The lease sees its own change.
    let r = run(&*lease, &format!("SELECT v FROM {table}")).await;
    match &r[0].outcome {
        StatementOutcome::Rows(rs) => assert_eq!(rs.rows[0][0], Value::Text("new".into())),
        other => panic!("{other:?}"),
    }
    lease.commit().await.unwrap();
    assert_eq!(lease.state().transaction, TxStatus::Idle);
    assert_eq!(
        scalar(&*s, &format!("SELECT v FROM {table} WHERE id = 1")).await,
        Value::Text("new".into())
    );

    // Rollback undoes.
    run(&*lease, &format!("UPDATE {table} SET v = 'gone'")).await;
    assert_eq!(lease.state().transaction, TxStatus::Active);
    lease.rollback().await.unwrap();
    assert_eq!(lease.state().transaction, TxStatus::Idle);
    assert_eq!(
        scalar(&*s, &format!("SELECT v FROM {table} WHERE id = 1")).await,
        Value::Text("new".into())
    );

    // Closing with an open transaction rolls it back and says so.
    run(&*lease, &format!("DELETE FROM {table}")).await;
    let report = lease.close().await.unwrap();
    assert!(report.rolled_back && !report.forced, "{report:?}");
    assert_eq!(
        count(&*s, &format!("SELECT count(*) FROM {table}")).await,
        1
    );
    exec(&*s, &format!("DROP TABLE {table}")).await;
}
on_both!(manual_commit_and_rollback, manual_16, manual_12);

async fn explicit_begin_in_auto_commit_and_failed_state(port: u16) {
    let s = connect(port).await;
    let lease = open(&*s, true).await;
    lease.begin(TxOptions::default()).await.unwrap();
    assert_eq!(lease.state().transaction, TxStatus::Active);
    assert_eq!(
        lease.begin(TxOptions::default()).await.unwrap_err().kind(),
        "busy"
    );
    assert_eq!(
        lease.set_auto_commit(true).await.unwrap_err().kind(),
        "busy"
    );
    // An error aborts the transaction block.
    let err = lease
        .execute(QueryRequest::new("SELECT 1/0"), &mut CollectingSink::new())
        .await
        .unwrap_err();
    assert_eq!(err.kind(), "query");
    assert_eq!(lease.state().transaction, TxStatus::Failed);
    // Only ROLLBACK works.
    assert!(
        lease
            .execute(QueryRequest::new("SELECT 1"), &mut CollectingSink::new())
            .await
            .is_err()
    );
    lease.rollback().await.unwrap();
    assert_eq!(lease.state().transaction, TxStatus::Idle);

    // A typed BEGIN / COMMIT is tracked as well.
    run(&*lease, "BEGIN").await;
    assert_eq!(lease.state().transaction, TxStatus::Active);
    run(&*lease, "COMMIT").await;
    assert_eq!(lease.state().transaction, TxStatus::Idle);

    // Isolation / read only options.
    lease
        .begin(TxOptions {
            read_only: true,
            isolation: Some(dbstudio_core::IsolationLevel::Serializable),
        })
        .await
        .unwrap();
    let err = lease
        .execute(
            QueryRequest::new("CREATE TEMP TABLE nope (a int)"),
            &mut CollectingSink::new(),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("read-only"), "{err}");
    lease.rollback().await.unwrap();
    assert!(
        lease
            .begin(TxOptions {
                read_only: false,
                isolation: Some(dbstudio_core::IsolationLevel::Snapshot),
            })
            .await
            .is_err()
    );
    lease.close().await.unwrap();
}
on_both!(
    explicit_begin_in_auto_commit_and_failed_state,
    failed_16,
    failed_12
);

async fn fetch_more_three_times(port: u16) {
    let s = connect(port).await;
    let lease = open(&*s, true).await;
    let mut req = QueryRequest::new("SELECT g AS n FROM generate_series(1, 3500) g");
    req.max_rows = Some(1000);
    req.batch_size = Some(300);
    let id = req.query_id;
    let mut sink = Rec::default();
    lease.execute(req, &mut sink).await.unwrap();
    assert_eq!(sink.rows.len(), 1000);
    assert!(matches!(
        sink.ends[0].outcome,
        EndOutcome::Rows {
            row_count: 1000,
            truncated: true
        }
    ));
    assert_eq!(lease.state().pending_result, Some(id));
    // Auto-commit: the cursor's own transaction is not reported as the user's.
    assert_eq!(lease.state().transaction, TxStatus::Idle);

    let mut seen = 1000i64;
    let mut sizes = Vec::new();
    for _ in 0..3 {
        let mut sink = Rec::default();
        lease.fetch_more(id, 1000, None, &mut sink).await.unwrap();
        // Continues exactly where the last call stopped (no row lost or repeated).
        assert_eq!(sink.rows[0][0], Value::Int(seen + 1), "no overlap");
        seen += sink.rows.len() as i64;
        sizes.push(sink.rows.len());
        assert!(sink.cols.is_empty(), "no new columns event");
        assert_eq!(sink.ends.len(), 1);
    }
    assert_eq!(sizes, [1000, 1000, 500]);
    assert_eq!(seen, 3500);
    assert_eq!(lease.state().pending_result, None);
    lease.close().await.unwrap();
}
on_both!(fetch_more_three_times, fetch_16, fetch_12);

async fn pending_result_is_dropped_by_next_command(port: u16) {
    let s = connect(port).await;
    let lease = open(&*s, true).await;
    let mut req = QueryRequest::new("SELECT g FROM generate_series(1, 100) g");
    req.max_rows = Some(10);
    lease
        .execute(req, &mut CollectingSink::new())
        .await
        .unwrap();
    assert!(lease.state().pending_result.is_some());
    // The pending cursor holds a transaction on the server.
    assert!(
        count(
            &*s,
            "SELECT count(*) FROM pg_stat_activity WHERE state = 'idle in transaction' \
             AND application_name = 'DBStudio'",
        )
        .await
            >= 1
    );
    lease.discard_result().await.unwrap();
    assert_eq!(lease.state().pending_result, None);

    let mut req = QueryRequest::new("SELECT g FROM generate_series(1, 100) g");
    req.max_rows = Some(10);
    let id = req.query_id;
    lease
        .execute(req, &mut CollectingSink::new())
        .await
        .unwrap();
    run(&*lease, "SELECT 1").await;
    assert_eq!(lease.state().pending_result, None);
    let err = lease
        .fetch_more(id, 10, None, &mut CollectingSink::new())
        .await
        .unwrap_err();
    assert_eq!(err.kind(), "notFound");

    // Result of a statement a cursor cannot host stays pending from memory.
    let mut req = QueryRequest::new("EXPLAIN (VERBOSE) SELECT g FROM generate_series(1, 100) g");
    req.max_rows = Some(1);
    let id = req.query_id;
    lease
        .execute(req, &mut CollectingSink::new())
        .await
        .unwrap();
    assert_eq!(lease.state().pending_result, Some(id));
    let mut sink = Rec::default();
    lease.fetch_more(id, 100, None, &mut sink).await.unwrap();
    assert!(!sink.rows.is_empty());
    assert_eq!(lease.state().pending_result, None);
    lease.close().await.unwrap();
}
on_both!(
    pending_result_is_dropped_by_next_command,
    pending_16,
    pending_12
);

async fn busy_while_running_and_context(port: u16) {
    let s = connect(port).await;
    let lease: Arc<dyn Lease> = Arc::from(open(&*s, true).await);
    let req = QueryRequest::new("SELECT pg_sleep(30)");
    let id = req.query_id;
    let runner = {
        let lease = lease.clone();
        tokio::spawn(async move { lease.execute(req, &mut CollectingSink::new()).await })
    };
    while lease.state().running.is_none() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let err = lease
        .execute(QueryRequest::new("SELECT 1"), &mut CollectingSink::new())
        .await
        .unwrap_err();
    assert_eq!(err.kind(), "busy");
    lease.cancel(id).await.unwrap();
    let _ = runner.await.unwrap();

    // Schema context.
    let schema = unique("lease_schema");
    exec(&*s, &format!("CREATE SCHEMA {schema}")).await;
    lease
        .set_context(LeaseContext {
            database: None,
            schema: Some(schema.clone()),
        })
        .await
        .unwrap();
    assert_eq!(lease.state().schema.as_deref(), Some(schema.as_str()));
    run(&*lease, "CREATE TABLE t_in_schema (a int)").await;
    assert_eq!(
        count(
            &*s,
            &format!(
                "SELECT count(*) FROM information_schema.tables WHERE table_schema = '{schema}'"
            )
        )
        .await,
        1
    );
    // A typed SET search_path updates the state too.
    run(&*lease, "SET search_path TO public").await;
    assert_eq!(lease.state().schema.as_deref(), Some("public"));
    exec(&*s, &format!("DROP SCHEMA {schema} CASCADE")).await;
    lease.ping().await.unwrap();
    lease.close().await.unwrap();
}
on_both!(busy_while_running_and_context, busy_16, busy_12);

/// §2.17: the lease moves to another database on a new connection; an open transaction makes it
/// `busy`.
async fn switch_database(port: u16) {
    let s = fixture(port).await;
    let lease: Arc<dyn Lease> = Arc::from(open(&*s, true).await);
    assert_eq!(lease.state().database.as_deref(), Some("dbstudio"));
    // A session setting of the old connection does not follow, a search path does.
    run(&*lease, "SET search_path TO public").await;
    lease
        .set_context(LeaseContext {
            database: Some("dbs_it_other".into()),
            schema: None,
        })
        .await
        .unwrap();
    assert_eq!(lease.state().database.as_deref(), Some("dbs_it_other"));
    let cur = |r: Vec<dbstudio_core::StatementResult>| match &r[0].outcome {
        StatementOutcome::Rows(rs) => rs.rows[0][0].clone(),
        o => panic!("{o:?}"),
    };
    assert_eq!(
        cur(run(&*lease, "SELECT current_database()").await),
        Value::Text("dbs_it_other".into())
    );
    assert_eq!(
        cur(run(&*lease, "SELECT count(*) FROM other_t").await),
        Value::Int(1)
    );
    // Same database again is a no-op; a missing one fails and leaves the lease usable.
    lease
        .set_context(LeaseContext {
            database: Some("dbs_it_other".into()),
            schema: None,
        })
        .await
        .unwrap();
    let err = lease
        .set_context(LeaseContext {
            database: Some("no_such_db_xyz".into()),
            schema: None,
        })
        .await
        .unwrap_err();
    assert_eq!(err.kind(), "query", "{err:?}");
    assert_eq!(
        cur(run(&*lease, "SELECT current_database()").await),
        Value::Text("dbs_it_other".into())
    );
    // An open transaction blocks the switch.
    lease.set_auto_commit(false).await.unwrap();
    run(&*lease, "SELECT 1").await;
    let err = lease
        .set_context(LeaseContext {
            database: Some("dbstudio".into()),
            schema: None,
        })
        .await
        .unwrap_err();
    assert_eq!(err.kind(), "busy");
    lease.rollback().await.unwrap();
    // A result that is still open is dropped by the switch (the lease drops it before any
    // command), so the switch goes through.
    lease.set_auto_commit(true).await.unwrap();
    let mut req = QueryRequest::new("SELECT generate_series(1, 100)");
    req.max_rows = Some(10);
    lease
        .execute(req, &mut CollectingSink::new())
        .await
        .unwrap();
    lease
        .set_context(LeaseContext {
            database: Some("dbstudio".into()),
            schema: None,
        })
        .await
        .unwrap();
    assert_eq!(
        cur(run(&*lease, "SELECT current_database()").await),
        Value::Text("dbstudio".into())
    );
    // The new connection can be cancelled.
    let req = QueryRequest::new("SELECT pg_sleep(30)");
    let id = req.query_id;
    let runner = {
        let lease = lease.clone();
        tokio::spawn(async move { lease.execute(req, &mut CollectingSink::new()).await })
    };
    while lease.state().running.is_none() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // `running` is set before the server has the query, so a single early cancel can land before
    // the query starts and be ignored: cancel again until the statement ends (bounded).
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while !runner.is_finished() && std::time::Instant::now() < deadline {
        lease.cancel(id).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(runner.await.unwrap().is_err());
    lease.close().await.unwrap();
}
on_both!(switch_database, switch_db_16, switch_db_12);

async fn lease_options_database_and_connection_loss(port: u16) {
    let s = connect(port).await;
    // A lease on another database.
    let _ = fixture(port).await;
    let lease = s
        .open_lease(LeaseOptions {
            database: Some("dbs_it_other".into()),
            schema: None,
            auto_commit: true,
        })
        .await
        .unwrap();
    assert_eq!(lease.state().database.as_deref(), Some("dbs_it_other"));
    let r = run(&*lease, "SELECT current_database()").await;
    match &r[0].outcome {
        StatementOutcome::Rows(rs) => assert_eq!(rs.rows[0][0], Value::Text("dbs_it_other".into())),
        other => panic!("{other:?}"),
    }
    // Kill the backend from outside: the lease becomes broken.
    let pid = match &run(&*lease, "SELECT pg_backend_pid()").await[0].outcome {
        StatementOutcome::Rows(rs) => match rs.rows[0][0] {
            Value::Int(n) => n,
            ref other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    };
    exec(&*s, &format!("SELECT pg_terminate_backend({pid})")).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let err = lease
        .execute(QueryRequest::new("SELECT 1"), &mut CollectingSink::new())
        .await
        .unwrap_err();
    assert_eq!(err.kind(), "connection", "{err:?}");
    assert!(lease.state().broken);
    let report = lease.close().await.unwrap();
    assert!(!report.forced);
}
on_both!(lease_options_database_and_connection_loss, loss_16, loss_12);

// ---------------------------------------------------------------------------------------------
// Review fixes

/// A sink that stops the request after the first batch.
struct StopAfterFirstBatch(usize);

impl dbstudio_core::ResultSink for StopAfterFirstBatch {
    fn on_columns(&mut self, _: Vec<dbstudio_core::ColumnMeta>) -> dbstudio_core::Result<()> {
        Ok(())
    }
    fn on_rows(&mut self, _: Vec<Vec<Value>>) -> dbstudio_core::Result<()> {
        self.0 += 1;
        Err(Error::Cancelled)
    }
    fn on_message(&mut self, _: String) -> dbstudio_core::Result<()> {
        Ok(())
    }
    fn on_statement_end(&mut self, _: dbstudio_core::StatementEnd) -> dbstudio_core::Result<()> {
        Ok(())
    }
}

async fn transaction_flags(port: u16) {
    let s = connect(port).await;
    let lease = open(&*s, true).await;

    // A request stopped by the client leaves a healthy transaction healthy (not "failed").
    lease.begin(TxOptions::default()).await.unwrap();
    let mut req = QueryRequest::new("SELECT g FROM generate_series(1, 100000) g");
    req.batch_size = Some(100);
    let mut sink = StopAfterFirstBatch(0);
    let err = lease.execute(req, &mut sink).await.unwrap_err();
    assert!(matches!(err, Error::Cancelled), "{err:?}");
    assert_eq!(lease.state().transaction, TxStatus::Active);
    run(&*lease, "SELECT 1").await;
    lease.rollback().await.unwrap();

    // ... while a server-side error does abort it, and ROLLBACK TO recovers it.
    lease.begin(TxOptions::default()).await.unwrap();
    run(&*lease, "SAVEPOINT a").await;
    lease
        .execute(QueryRequest::new("SELECT 1/0"), &mut CollectingSink::new())
        .await
        .unwrap_err();
    assert_eq!(lease.state().transaction, TxStatus::Failed);
    run(&*lease, "ROLLBACK TO SAVEPOINT a").await;
    assert_eq!(lease.state().transaction, TxStatus::Active);
    run(&*lease, "SELECT 2").await;
    lease.rollback().await.unwrap();

    // COMMIT AND CHAIN keeps a transaction open.
    run(&*lease, "BEGIN").await;
    run(&*lease, "COMMIT AND CHAIN").await;
    assert_eq!(lease.state().transaction, TxStatus::Active);
    run(&*lease, "ROLLBACK AND CHAIN").await;
    assert_eq!(lease.state().transaction, TxStatus::Active);
    run(&*lease, "COMMIT").await;
    assert_eq!(lease.state().transaction, TxStatus::Idle);
    lease.close().await.unwrap();
}
on_both!(transaction_flags, flags_16, flags_12);

async fn manual_mode_runs_vacuum_outside_a_transaction(port: u16) {
    let s = connect(port).await;
    let lease = open(&*s, false).await;
    let table = unique("lease_vac");
    run(&*lease, &format!("CREATE TABLE {table} (a int)")).await;
    run(&*lease, "COMMIT").await;
    // No implicit BEGIN before VACUUM (it would fail inside a transaction block).
    run(&*lease, &format!("VACUUM {table}")).await;
    assert_eq!(lease.state().transaction, TxStatus::Idle);
    run(
        &*lease,
        &format!("CREATE INDEX CONCURRENTLY {table}_i ON {table} (a)"),
    )
    .await;
    assert_eq!(lease.state().transaction, TxStatus::Idle);
    // An ordinary statement still opens the transaction lazily, after which VACUUM is refused
    // by the server with its own clear message.
    run(&*lease, &format!("INSERT INTO {table} VALUES (1)")).await;
    assert_eq!(lease.state().transaction, TxStatus::Active);
    let err = lease
        .execute(
            QueryRequest::new(format!("VACUUM {table}")),
            &mut CollectingSink::new(),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("transaction block"), "{err}");
    lease.rollback().await.unwrap();
    run(&*lease, &format!("DROP TABLE {table}")).await;
    run(&*lease, "COMMIT").await;
    lease.close().await.unwrap();
}
on_both!(
    manual_mode_runs_vacuum_outside_a_transaction,
    vacuum_16,
    vacuum_12
);

async fn read_only_profile_stays_read_only(port: u16) {
    let mut cfg = config(port, dbstudio_core::TlsMode::Prefer);
    cfg.read_only = true;
    let s = try_connect(&cfg, PASSWORD).await.expect("connect");
    let table = unique("lease_ro");
    let admin = connect(port).await;
    exec(&*admin, &format!("CREATE TABLE {table} (a int)")).await;

    let lease = open(&*s, true).await;
    let insert = format!("INSERT INTO {table} VALUES (1)");
    let refused = |e: Error| e.to_string().contains("read-only");
    assert!(refused(
        lease
            .execute(QueryRequest::new(&insert), &mut CollectingSink::new())
            .await
            .unwrap_err()
    ));
    // A script that switches read-only off does not survive into the next request ...
    run(&*lease, "SET default_transaction_read_only = off").await;
    assert!(refused(
        lease
            .execute(QueryRequest::new(&insert), &mut CollectingSink::new())
            .await
            .unwrap_err()
    ));
    // ... and a transaction started through the lease is read-only whatever was asked.
    lease.begin(TxOptions::default()).await.unwrap();
    assert!(refused(
        lease
            .execute(QueryRequest::new(&insert), &mut CollectingSink::new())
            .await
            .unwrap_err()
    ));
    lease.rollback().await.unwrap();
    assert_eq!(
        count(&*admin, &format!("SELECT count(*) FROM {table}")).await,
        0
    );
    lease.close().await.unwrap();
    exec(&*admin, &format!("DROP TABLE {table}")).await;
}
on_both!(read_only_profile_stays_read_only, ro_16, ro_12);

fn explain_req(text: &str, analyze: bool) -> dbstudio_core::ExplainRequest {
    dbstudio_core::ExplainRequest {
        query_id: Uuid::new_v4(),
        text: text.into(),
        database: None,
        analyze,
        options: dbstudio_core::ExplainOptions::default(),
        timeout_ms: None,
    }
}

async fn lease_explain(port: u16) {
    let admin = connect(port).await;
    let table = unique("it_lease_explain");
    exec(
        &*admin,
        &format!("CREATE TABLE {table} (id int PRIMARY KEY, v int NOT NULL)"),
    )
    .await;
    let lease = open(&*admin, true).await;
    // Outside a transaction: plain and ANALYZE of a write (rolled back).
    let plain = lease
        .explain(explain_req(&format!("SELECT * FROM {table}"), false))
        .await
        .expect("plain");
    assert!(!plain.analyzed);
    let raw = lease
        .explain(explain_req(
            &format!("INSERT INTO {table} VALUES (1, 1)"),
            true,
        ))
        .await
        .expect("analyze outside tx");
    assert!(raw.analyzed && raw.rolled_back);
    assert_eq!(
        count(&*admin, &format!("SELECT count(*) FROM {table}")).await,
        0
    );
    assert_eq!(lease.state().transaction, TxStatus::Idle);
    // A failing ANALYZE does not leave a transaction behind.
    lease
        .explain(explain_req("SELECT * FROM no_such_table_x", true))
        .await
        .unwrap_err();
    assert_eq!(lease.state().transaction, TxStatus::Idle);
    // Several statements are refused.
    lease
        .explain(explain_req("SELECT 1; SELECT 2", false))
        .await
        .unwrap_err();
    // Inside a transaction: earlier work survives, the ANALYZE write is undone, and a failing
    // explain does not abort the transaction.
    lease.begin(TxOptions::default()).await.unwrap();
    run(&*lease, &format!("INSERT INTO {table} VALUES (10, 10)")).await;
    let raw = lease
        .explain(explain_req(
            &format!("INSERT INTO {table} VALUES (11, 11)"),
            true,
        ))
        .await
        .expect("analyze in tx");
    assert!(raw.analyzed && raw.rolled_back);
    lease
        .explain(explain_req("SELECT * FROM no_such_table_x", false))
        .await
        .unwrap_err();
    assert_eq!(lease.state().transaction, TxStatus::Active);
    let rows = run(&*lease, &format!("SELECT id FROM {table} ORDER BY id")).await;
    let ids = match &rows[0].outcome {
        StatementOutcome::Rows(rs) => rs.rows.clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(ids, vec![vec![Value::Int(10)]]);
    lease.commit().await.unwrap();
    assert_eq!(
        count(&*admin, &format!("SELECT count(*) FROM {table}")).await,
        1
    );
    lease.close().await.unwrap();
    exec(&*admin, &format!("DROP TABLE {table}")).await;
}
on_both!(lease_explain, lease_explain_16, lease_explain_12);
