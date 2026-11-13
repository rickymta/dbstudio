//! Integration tests of the MySQL / MariaDB lease (dedicated editor connection).
//! Run with `DBSTUDIO_IT=1`; the servers tested are `mysql8` and `mariadb`.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use dbstudio_core::{
    CollectingSink, EndOutcome, Error, IsolationLevel, Lease, LeaseContext, LeaseOptions,
    QueryRequest, ResultSink, Session, StatementEnd, StatementOutcome, StatementStart, TxOptions,
    TxStatus, Value,
};

fn lease_targets() -> Vec<Target> {
    [MYSQL8, MARIADB]
        .into_iter()
        .filter(|t| selected(*t))
        .collect()
}

async fn open(s: &dyn Session, db: &str, auto_commit: bool) -> Box<dyn Lease> {
    s.open_lease(LeaseOptions {
        database: Some(db.to_string()),
        schema: None,
        auto_commit,
    })
    .await
    .expect("open lease")
}

async fn exec(lease: &dyn Lease, sql: &str) -> Vec<dbstudio_core::StatementResult> {
    let mut sink = CollectingSink::new();
    lease
        .execute(QueryRequest::new(sql), &mut sink)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    sink.into_results()
}

fn first_int(r: &[dbstudio_core::StatementResult]) -> i64 {
    match &r[0].outcome {
        StatementOutcome::Rows(rs) => match rs.rows[0][0] {
            Value::Int(n) => n,
            Value::UInt(n) => n as i64,
            ref other => panic!("not an int: {other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

async fn count(s: &dyn Session, sql: &str) -> i64 {
    first_int(&run(s, sql).await)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn script_of_three_with_affected_and_warning() {
    if !it_enabled() {
        return;
    }
    for t in lease_targets() {
        let db = scratch_db(t, "lease_script").await;
        let s = user_session(t).await;
        let lease = open(&*s, &db, true).await;
        assert_eq!(lease.state().database.as_deref(), Some(db.as_str()));
        let r = exec(
            &*lease,
            "CREATE TABLE t (id int); INSERT INTO t VALUES (1),(2),(3); \
             DROP TABLE IF EXISTS nope_not_here; SELECT count(*) FROM t",
        )
        .await;
        assert_eq!(r.len(), 4, "[{}]", t.name);
        assert!(matches!(r[0].outcome, StatementOutcome::Done));
        assert!(matches!(
            r[1].outcome,
            StatementOutcome::Affected { rows: 3 }
        ));
        assert!(
            r[2].messages.iter().any(|m| m.contains("warning")),
            "[{}] {:?}",
            t.name,
            r[2].messages
        );
        assert_eq!(first_int(&r[3..]), 3);
        let report = lease.close().await.unwrap();
        assert!(!report.rolled_back && !report.forced);
    }
}

#[derive(Default)]
struct Starts(Vec<(u32, u32, u32)>, CollectingSink);

impl ResultSink for Starts {
    fn on_columns(&mut self, c: Vec<dbstudio_core::ColumnMeta>) -> dbstudio_core::Result<()> {
        self.1.on_columns(c)
    }
    fn on_rows(&mut self, r: Vec<Vec<Value>>) -> dbstudio_core::Result<()> {
        self.1.on_rows(r)
    }
    fn on_message(&mut self, m: String) -> dbstudio_core::Result<()> {
        self.1.on_message(m)
    }
    fn on_statement_end(&mut self, e: StatementEnd) -> dbstudio_core::Result<()> {
        self.1.on_statement_end(e)
    }
    fn on_statement_start(&mut self, s: StatementStart) -> dbstudio_core::Result<()> {
        self.0.push((s.index, s.range.start, s.range.end));
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn statement_start_events_carry_character_ranges() {
    if !it_enabled() {
        return;
    }
    for t in lease_targets() {
        let s = user_session(t).await;
        let lease = open(&*s, "information_schema", true).await;
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
}

async fn processlist(root: &dyn Session, id: i64) -> (String, String) {
    let rs = query(
        root,
        &format!(
            "SELECT COMMAND, IFNULL(INFO, '') FROM information_schema.PROCESSLIST WHERE ID = {id}"
        ),
    )
    .await;
    match (&rs.rows[0][0], &rs.rows[0][1]) {
        (Value::Text(a), Value::Text(b)) => (a.clone(), b.clone()),
        other => panic!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_sleep_within_two_seconds_and_server_confirms() {
    if !it_enabled() {
        return;
    }
    for t in lease_targets() {
        let s = user_session(t).await;
        let root = root_session(t).await;
        let lease: Arc<dyn Lease> = Arc::from(open(&*s, "information_schema", true).await);
        let conn_id = first_int(&exec(&*lease, "SELECT CONNECTION_ID()").await);
        let req = QueryRequest::new("SELECT SLEEP(60)");
        let id = req.query_id;
        let runner = {
            let lease = lease.clone();
            tokio::spawn(async move { lease.execute(req, &mut CollectingSink::new()).await })
        };
        let started = Instant::now();
        loop {
            let (command, info) = processlist(&*root, conn_id).await;
            if command == "Query" && info.contains("SLEEP(60)") {
                break;
            }
            assert!(started.elapsed() < Duration::from_secs(10), "never started");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let cancelled_at = Instant::now();
        lease.cancel(id).await.unwrap();
        let result = runner.await.unwrap();
        assert!(
            matches!(result, Err(Error::Cancelled)),
            "[{}] {result:?}",
            t.name
        );
        assert!(
            cancelled_at.elapsed() < Duration::from_secs(2),
            "[{}] {:?}",
            t.name,
            cancelled_at.elapsed()
        );
        // The server confirms: the connection is idle again.
        let (command, _) = processlist(&*root, conn_id).await;
        assert_eq!(command, "Sleep", "[{}]", t.name);
        // The lease works afterwards.
        assert_eq!(first_int(&exec(&*lease, "SELECT 42").await), 42);
        lease.close().await.unwrap();
        root.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn timeout_cancels_the_statement() {
    if !it_enabled() {
        return;
    }
    for t in lease_targets() {
        let s = user_session(t).await;
        let lease = open(&*s, "information_schema", true).await;
        let mut req = QueryRequest::new("SELECT SLEEP(60)");
        req.timeout_ms = Some(300);
        let started = Instant::now();
        let err = lease
            .execute(req, &mut CollectingSink::new())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Timeout), "[{}] {err:?}", t.name);
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_eq!(first_int(&exec(&*lease, "SELECT 1").await), 1);
        lease.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manual_commit_and_rollback() {
    if !it_enabled() {
        return;
    }
    for t in lease_targets() {
        let db = scratch_db(t, "lease_tx").await;
        let s = user_session(t).await;
        let other = user_session(t).await;
        let lease0 = open(&*s, &db, true).await;
        exec(
            &*lease0,
            "CREATE TABLE t (id int PRIMARY KEY, v varchar(10)) ENGINE=InnoDB; \
             INSERT INTO t VALUES (1, 'old')",
        )
        .await;
        lease0.close().await.unwrap();

        let lease = open(&*s, &db, false).await;
        assert!(!lease.state().auto_commit);
        assert_eq!(lease.state().transaction, TxStatus::Idle);
        exec(&*lease, "UPDATE t SET v = 'new' WHERE id = 1").await;
        assert_eq!(lease.state().transaction, TxStatus::Active);
        // Another connection does not see it.
        let seen = query(&*other, &format!("SELECT v FROM `{db}`.t WHERE id = 1")).await;
        assert_eq!(seen.rows[0][0], Value::Text("old".into()), "[{}]", t.name);
        lease.commit().await.unwrap();
        assert_eq!(lease.state().transaction, TxStatus::Idle);
        let seen = query(&*other, &format!("SELECT v FROM `{db}`.t WHERE id = 1")).await;
        assert_eq!(seen.rows[0][0], Value::Text("new".into()));

        exec(&*lease, "UPDATE t SET v = 'gone'").await;
        assert_eq!(lease.state().transaction, TxStatus::Active);
        lease.rollback().await.unwrap();
        assert_eq!(lease.state().transaction, TxStatus::Idle);
        let seen = query(&*other, &format!("SELECT v FROM `{db}`.t WHERE id = 1")).await;
        assert_eq!(seen.rows[0][0], Value::Text("new".into()));

        // Closing with an open transaction rolls it back and reports it.
        exec(&*lease, "DELETE FROM t").await;
        let report = lease.close().await.unwrap();
        assert!(
            report.rolled_back && !report.forced,
            "[{}] {report:?}",
            t.name
        );
        assert_eq!(
            count(&*other, &format!("SELECT count(*) FROM `{db}`.t")).await,
            1
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn explicit_transactions_in_auto_commit_mode() {
    if !it_enabled() {
        return;
    }
    for t in lease_targets() {
        let db = scratch_db(t, "lease_tx2").await;
        let s = user_session(t).await;
        let other = user_session(t).await;
        let lease = open(&*s, &db, true).await;
        exec(&*lease, "CREATE TABLE t (id int PRIMARY KEY) ENGINE=InnoDB").await;
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
        exec(&*lease, "INSERT INTO t VALUES (1)").await;
        assert_eq!(
            count(&*other, &format!("SELECT count(*) FROM `{db}`.t")).await,
            0
        );
        lease.commit().await.unwrap();
        assert_eq!(
            count(&*other, &format!("SELECT count(*) FROM `{db}`.t")).await,
            1
        );

        // Typed START TRANSACTION / ROLLBACK and DDL's implicit commit are tracked.
        exec(&*lease, "START TRANSACTION").await;
        assert_eq!(lease.state().transaction, TxStatus::Active);
        exec(&*lease, "INSERT INTO t VALUES (2)").await;
        exec(&*lease, "ROLLBACK").await;
        assert_eq!(lease.state().transaction, TxStatus::Idle);
        assert_eq!(
            count(&*other, &format!("SELECT count(*) FROM `{db}`.t")).await,
            1
        );
        exec(&*lease, "BEGIN").await;
        exec(&*lease, "CREATE TABLE t2 (a int)").await;
        assert_eq!(lease.state().transaction, TxStatus::Idle);

        // Options: read only, isolation; snapshot is not supported.
        lease
            .begin(TxOptions {
                read_only: true,
                isolation: Some(IsolationLevel::Serializable),
            })
            .await
            .unwrap();
        let err = lease
            .execute(
                QueryRequest::new("INSERT INTO t VALUES (9)"),
                &mut CollectingSink::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(err.kind(), "query", "[{}] {err:?}", t.name);
        lease.rollback().await.unwrap();
        assert_eq!(
            lease
                .begin(TxOptions {
                    read_only: false,
                    isolation: Some(IsolationLevel::Snapshot),
                })
                .await
                .unwrap_err()
                .kind(),
            "unsupported"
        );

        // Manual mode toggle.
        lease.set_auto_commit(false).await.unwrap();
        assert!(!lease.state().auto_commit);
        exec(&*lease, "INSERT INTO t VALUES (3)").await;
        assert_eq!(lease.state().transaction, TxStatus::Active);
        assert_eq!(
            count(&*other, &format!("SELECT count(*) FROM `{db}`.t")).await,
            1
        );
        lease.rollback().await.unwrap();
        lease.set_auto_commit(true).await.unwrap();
        exec(&*lease, "INSERT INTO t VALUES (4)").await;
        assert_eq!(
            count(&*other, &format!("SELECT count(*) FROM `{db}`.t")).await,
            2
        );
        lease.close().await.unwrap();
    }
}

async fn fixture_digits(lease: &dyn Lease) {
    exec(
        lease,
        "CREATE TABLE digits (d INT PRIMARY KEY); \
         INSERT INTO digits VALUES (0),(1),(2),(3),(4),(5),(6),(7),(8),(9)",
    )
    .await;
}

const NUMBERS: &str = "SELECT a.d + b.d*10 + c.d*100 + e.d*1000 + 1 AS n \
    FROM digits a, digits b, digits c, digits e \
    HAVING n <= 3500 ORDER BY n";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fetch_more_three_times_continues_exactly() {
    if !it_enabled() {
        return;
    }
    for t in lease_targets() {
        let db = scratch_db(t, "lease_fetch").await;
        let s = user_session(t).await;
        let lease = open(&*s, &db, true).await;
        fixture_digits(&*lease).await;

        let mut req = QueryRequest::new(NUMBERS);
        req.max_rows = Some(1000);
        req.batch_size = Some(300);
        let id = req.query_id;
        let mut sink = CountingRows::default();
        lease.execute(req, &mut sink).await.unwrap();
        assert_eq!(sink.rows.len(), 1000, "[{}]", t.name);
        assert!(matches!(
            sink.ends[0],
            EndOutcome::Rows {
                row_count: 1000,
                truncated: true
            }
        ));
        assert_eq!(lease.state().pending_result, Some(id));
        // The statement is parked: not "running", and no transaction is reported.
        assert_eq!(lease.state().running, None);
        assert_eq!(lease.state().transaction, TxStatus::Idle);

        let mut seen = 1000i64;
        let mut sizes = Vec::new();
        for _ in 0..3 {
            let mut sink = CountingRows::default();
            lease.fetch_more(id, 1000, None, &mut sink).await.unwrap();
            assert_eq!(sink.rows[0], Value::Int(seen + 1), "no gap, no overlap");
            seen += sink.rows.len() as i64;
            sizes.push(sink.rows.len());
            assert_eq!(sink.columns, 0, "fetch_more sends no columns");
            assert_eq!(sink.ends.len(), 1);
        }
        assert_eq!(sizes, [1000, 1000, 500], "[{}]", t.name);
        assert_eq!(seen, 3500);
        assert_eq!(lease.state().pending_result, None);
        // The connection is back and usable.
        assert_eq!(first_int(&exec(&*lease, "SELECT 7").await), 7);
        lease.close().await.unwrap();
    }
}

#[derive(Default)]
struct CountingRows {
    rows: Vec<Value>,
    columns: usize,
    ends: Vec<EndOutcome>,
}

impl ResultSink for CountingRows {
    fn on_columns(&mut self, c: Vec<dbstudio_core::ColumnMeta>) -> dbstudio_core::Result<()> {
        self.columns += c.len();
        Ok(())
    }
    fn on_rows(&mut self, rows: Vec<Vec<Value>>) -> dbstudio_core::Result<()> {
        self.rows.extend(rows.into_iter().map(|mut r| r.remove(0)));
        Ok(())
    }
    fn on_message(&mut self, _: String) -> dbstudio_core::Result<()> {
        Ok(())
    }
    fn on_statement_end(&mut self, e: StatementEnd) -> dbstudio_core::Result<()> {
        self.ends.push(e.outcome);
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pending_result_is_dropped_by_discard_and_the_next_command() {
    if !it_enabled() {
        return;
    }
    for t in lease_targets() {
        let db = scratch_db(t, "lease_pending").await;
        let s = user_session(t).await;
        let lease = open(&*s, &db, true).await;
        fixture_digits(&*lease).await;

        let mut req = QueryRequest::new(NUMBERS);
        req.max_rows = Some(10);
        lease
            .execute(req, &mut CollectingSink::new())
            .await
            .unwrap();
        assert!(lease.state().pending_result.is_some());
        lease.discard_result().await.unwrap();
        assert_eq!(lease.state().pending_result, None);
        assert_eq!(first_int(&exec(&*lease, "SELECT 1").await), 1);

        // A script whose earlier statements are small and whose last one is cut.
        let mut req = QueryRequest::new(format!("SELECT 1; {NUMBERS}"));
        req.max_rows = Some(10);
        let id = req.query_id;
        let mut sink = CollectingSink::new();
        lease.execute(req, &mut sink).await.unwrap();
        assert_eq!(sink.into_results().len(), 2);
        assert_eq!(lease.state().pending_result, Some(id));
        // The next execute drops it.
        assert_eq!(first_int(&exec(&*lease, "SELECT 2").await), 2);
        assert_eq!(lease.state().pending_result, None);
        let err = lease
            .fetch_more(id, 10, None, &mut CollectingSink::new())
            .await
            .unwrap_err();
        assert_eq!(err.kind(), "notFound");

        // Fetching with a wrong id does not disturb the pending result.
        let mut req = QueryRequest::new(NUMBERS);
        req.max_rows = Some(10);
        let id = req.query_id;
        lease
            .execute(req, &mut CollectingSink::new())
            .await
            .unwrap();
        let err = lease
            .fetch_more(uuid::Uuid::new_v4(), 10, None, &mut CollectingSink::new())
            .await
            .unwrap_err();
        assert_eq!(err.kind(), "notFound");
        let mut sink = CountingRows::default();
        lease.fetch_more(id, 5, None, &mut sink).await.unwrap();
        assert_eq!(sink.rows.len(), 5);
        lease.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn context_busy_and_connection_loss() {
    if !it_enabled() {
        return;
    }
    for t in lease_targets() {
        let db = scratch_db(t, "lease_ctx").await;
        let db2 = scratch_db(t, "lease_ctx2").await;
        let s = user_session(t).await;
        let root = root_session(t).await;
        let lease: Arc<dyn Lease> = Arc::from(open(&*s, &db, true).await);

        // Switching database, by command and by a typed USE.
        lease
            .set_context(LeaseContext {
                database: Some(db2.clone()),
                schema: None,
            })
            .await
            .unwrap();
        assert_eq!(lease.state().database.as_deref(), Some(db2.as_str()));
        exec(&*lease, &format!("USE `{db}`")).await;
        assert_eq!(lease.state().database.as_deref(), Some(db.as_str()));

        // Busy while a statement runs.
        let req = QueryRequest::new("SELECT SLEEP(30)");
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
        lease.ping().await.unwrap();

        // Kill the connection from outside: the lease becomes broken.
        let conn_id = first_int(&exec(&*lease, "SELECT CONNECTION_ID()").await);
        run(&*root, &format!("KILL {conn_id}")).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let err = lease
            .execute(QueryRequest::new("SELECT 1"), &mut CollectingSink::new())
            .await
            .unwrap_err();
        assert_eq!(err.kind(), "connection", "[{}] {err:?}", t.name);
        assert!(lease.state().broken);
        let report = lease.close().await.unwrap();
        assert!(!report.forced);
        root.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn read_only_connection_applies_to_leases() {
    if !it_enabled() {
        return;
    }
    for t in lease_targets() {
        let db = scratch_db(t, "lease_ro").await;
        let root = root_session(t).await;
        run(&*root, &format!("CREATE TABLE `{db}`.t (a int)")).await;
        let mut cfg = config(t, "dbstudio", dbstudio_core::TlsMode::Disable);
        cfg.read_only = true;
        let s = connect_cfg(&cfg).await.unwrap();
        let lease = open(&*s, &db, true).await;
        let err = lease
            .execute(
                QueryRequest::new("INSERT INTO t VALUES (1)"),
                &mut CollectingSink::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(err.kind(), "query", "[{}] {err:?}", t.name);
        lease.close().await.unwrap();
        root.close().await.unwrap();
    }
}
