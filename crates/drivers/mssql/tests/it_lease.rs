//! Integration tests: leases (dedicated connection, manual transactions, pending results,
//! cancel, close), table streaming and bulk insert.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use dbstudio_core::{
    BulkInsertRequest, CollectingSink, EditValue, Error, ExplainRequest, Lease, LeaseContext,
    LeaseOptions, ObjectKind, ObjectPath, OnConflict, QueryRequest, StatementOutcome,
    TableStreamRequest, TxOptions, TxStatus, Value, folder,
};

macro_rules! it {
    () => {
        if !it_enabled() {
            return;
        }
    };
}

async fn open(s: &dyn dbstudio_core::Session, auto_commit: bool) -> Box<dyn Lease> {
    s.open_lease(LeaseOptions {
        auto_commit,
        ..Default::default()
    })
    .await
    .unwrap()
}

async fn lrun(l: &dyn Lease, sql: &str) -> Vec<dbstudio_core::StatementResult> {
    let mut sink = CollectingSink::new();
    l.execute(QueryRequest::new(sql), &mut sink)
        .await
        .unwrap_or_else(|e| panic!("`{sql}` failed: {e}"));
    sink.into_results()
}

fn scalar(r: &[dbstudio_core::StatementResult]) -> Value {
    rows_of(r.last().unwrap()).rows[0][0].clone()
}

async fn make(s: &dyn dbstudio_core::Session, name: &str) {
    run(s, &format!("DROP TABLE IF EXISTS dbo.{name};\nGO\nCREATE TABLE dbo.{name} (id INT PRIMARY KEY, v NVARCHAR(20));\nGO\nINSERT INTO dbo.{name} VALUES (1, N'a');")).await;
}

fn path(db: &str, name: &str) -> ObjectPath {
    ObjectPath::root()
        .child(ObjectKind::Database, db)
        .child(ObjectKind::Folder, folder::SCHEMAS)
        .child(ObjectKind::Schema, "dbo")
        .child(ObjectKind::Folder, folder::TABLES)
        .child(ObjectKind::Table, name)
}

#[tokio::test]
async fn script_in_a_lease_and_state() {
    it!();
    let s = fixture().await;
    let l = open(&*s, true).await;
    let st = l.state();
    assert!(st.auto_commit && st.transaction == TxStatus::Idle && !st.broken);
    assert_eq!(st.database.as_deref(), Some(IT_DB));
    let r = lrun(&*l, "SELECT 1 AS a; PRINT N'hi'; SELECT 2 AS b").await;
    assert_eq!(
        r.iter()
            .filter(|x| matches!(x.outcome, StatementOutcome::Rows(_)))
            .count(),
        2
    );
    assert!(r.iter().any(|x| x.messages == vec!["hi".to_string()]));
    l.ping().await.unwrap();
    // `USE` sticks to the lease (and only to it).
    l.set_context(LeaseContext {
        database: Some("master".into()),
        schema: None,
    })
    .await
    .unwrap();
    assert_eq!(
        scalar(&lrun(&*l, "SELECT DB_NAME()").await),
        Value::Text("master".into())
    );
    assert_eq!(l.state().database.as_deref(), Some("master"));
    run(&*s, "SELECT 1").await;
    let r = run(&*s, "SELECT DB_NAME()").await;
    assert_eq!(text(&rows_of(&r[0]).rows[0][0]), IT_DB);
    // Session variables persist on the lease connection.
    lrun(&*l, "CREATE TABLE #mine (a INT); INSERT #mine VALUES (7)").await;
    assert_eq!(
        scalar(&lrun(&*l, "SELECT a FROM #mine").await),
        Value::Int(7)
    );
    l.close().await.unwrap();
}

#[tokio::test]
async fn manual_transactions_commit_and_rollback() {
    it!();
    let s = work().await;
    make(&*s, "lease_tx").await;
    let l = open(&*s, false).await;
    assert!(!l.state().auto_commit);
    lrun(&*l, "INSERT INTO dbo.lease_tx VALUES (2, N'uncommitted')").await;
    assert_eq!(
        l.state().transaction,
        TxStatus::Active,
        "implicit transaction opened by the write"
    );
    // Another connection does not see it (READ COMMITTED would block, so peek with NOLOCK-free count of rows via a snapshot of ids it can see).
    let other = open(&*s, true).await;
    let seen = lrun(&*other, "SELECT COUNT(*) FROM dbo.lease_tx WITH (READPAST)").await;
    assert_eq!(
        scalar(&seen),
        Value::Int(1),
        "the uncommitted row is invisible to other connections"
    );
    l.rollback().await.unwrap();
    assert_eq!(l.state().transaction, TxStatus::Idle);
    assert_eq!(
        scalar(&lrun(&*l, "SELECT COUNT(*) FROM dbo.lease_tx").await),
        Value::Int(1)
    );
    // The SELECT itself opened a new implicit transaction; commit makes the insert durable.
    lrun(&*l, "INSERT INTO dbo.lease_tx VALUES (3, N'kept')").await;
    l.commit().await.unwrap();
    assert_eq!(l.state().transaction, TxStatus::Idle);
    assert_eq!(
        scalar(&lrun(&*other, "SELECT COUNT(*) FROM dbo.lease_tx").await),
        Value::Int(2)
    );
    // Explicit begin in auto-commit mode.
    let a = open(&*s, true).await;
    a.begin(TxOptions::default()).await.unwrap();
    assert_eq!(a.state().transaction, TxStatus::Active);
    lrun(&*a, "INSERT INTO dbo.lease_tx VALUES (4, N'x')").await;
    // Leaving auto-commit with an open transaction is refused; closing rolls it back.
    let err = a.set_auto_commit(false).await;
    assert!(err.is_ok(), "switching to manual is allowed");
    a.set_auto_commit(true)
        .await
        .expect_err("auto-commit with an open transaction is busy");
    let report = a.close().await.unwrap();
    assert!(report.rolled_back && !report.forced);
    assert_eq!(
        scalar(&lrun(&*other, "SELECT COUNT(*) FROM dbo.lease_tx").await),
        Value::Int(2)
    );
    // Failure inside a transaction keeps it open (no XACT_ABORT) and rollback recovers.
    let b = open(&*s, true).await;
    b.begin(TxOptions::default()).await.unwrap();
    let mut sink = CollectingSink::new();
    let e = b
        .execute(
            QueryRequest::new("INSERT INTO dbo.lease_tx VALUES (1, N'dup')"),
            &mut sink,
        )
        .await
        .unwrap_err();
    assert!(is_query_err(&e));
    assert_eq!(b.state().transaction, TxStatus::Active);
    b.rollback().await.unwrap();
    assert_eq!(b.state().transaction, TxStatus::Idle);
    // Switching modes.
    b.set_auto_commit(false).await.unwrap();
    assert!(!b.state().auto_commit);
    b.set_auto_commit(true).await.unwrap();
    assert!(b.state().auto_commit);
    for l in [b, l, other] {
        l.close().await.unwrap();
    }
}

#[tokio::test]
async fn busy_while_running_and_cancel_by_id() {
    it!();
    let s = fixture().await;
    let l: Arc<dyn Lease> = Arc::from(open(&*s, true).await);
    let req = QueryRequest::new("WAITFOR DELAY '00:01:00'; SELECT 1");
    let id = req.query_id;
    let runner = {
        let l = l.clone();
        tokio::spawn(async move {
            let mut sink = CollectingSink::new();
            l.execute(req, &mut sink).await
        })
    };
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(l.state().running, Some(id));
    let mut sink = CollectingSink::new();
    let e = l
        .execute(QueryRequest::new("SELECT 1"), &mut sink)
        .await
        .unwrap_err();
    assert!(matches!(e, Error::Busy(_)), "{e}");
    // A cancel for another id does nothing; the right one stops the wait within 2 s.
    l.cancel(uuid::Uuid::new_v4()).await.unwrap();
    assert_eq!(l.state().running, Some(id));
    let t = Instant::now();
    l.cancel(id).await.unwrap();
    let r = runner.await.unwrap();
    assert!(matches!(r, Err(Error::Cancelled)), "{r:?}");
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    assert_eq!(l.state().running, None);
    // The lease is usable again and a late cancel never hits the next statement.
    l.cancel(id).await.unwrap();
    assert_eq!(scalar(&lrun(&*l, "SELECT 5").await), Value::Int(5));
    l.close().await.unwrap();
}

#[tokio::test]
async fn cancel_keeps_the_transaction_open() {
    it!();
    let s = work().await;
    make(&*s, "lease_cx").await;
    let l: Arc<dyn Lease> = Arc::from(open(&*s, true).await);
    l.begin(TxOptions::default()).await.unwrap();
    lrun(&*l, "INSERT INTO dbo.lease_cx VALUES (2, N'in tx')").await;
    let req = QueryRequest::new("WAITFOR DELAY '00:01:00'");
    let id = req.query_id;
    let runner = {
        let l = l.clone();
        tokio::spawn(async move { l.execute(req, &mut CollectingSink::new()).await })
    };
    tokio::time::sleep(Duration::from_millis(500)).await;
    l.cancel(id).await.unwrap();
    assert!(matches!(runner.await.unwrap(), Err(Error::Cancelled)));
    assert_eq!(
        l.state().transaction,
        TxStatus::Active,
        "cancel does not end the transaction"
    );
    assert_eq!(
        scalar(&lrun(&*l, "SELECT COUNT(*) FROM dbo.lease_cx").await),
        Value::Int(2)
    );
    l.rollback().await.unwrap();
    assert_eq!(
        scalar(&lrun(&*l, "SELECT COUNT(*) FROM dbo.lease_cx").await),
        Value::Int(1)
    );
    l.close().await.unwrap();
}

#[tokio::test]
async fn fetch_more_three_times() {
    it!();
    let s = fixture().await;
    let l = open(&*s, true).await;
    let mut req = QueryRequest::new("SELECT id FROM dbo.paging ORDER BY id");
    req.max_rows = Some(100);
    req.batch_size = Some(30);
    let id = req.query_id;
    let mut sink = CollectingSink::new();
    let summary = l.execute(req, &mut sink).await.unwrap();
    assert_eq!(summary.statements, 1);
    let res = sink.into_results();
    let rs = rows_of(&res[0]);
    assert_eq!(rs.rows.len(), 100);
    assert!(rs.truncated);
    assert_eq!(l.state().pending_result, Some(id));
    assert_eq!(l.state().running, None);
    let mut next = 101i64;
    for round in 0..3 {
        let mut sink = CollectingSink::new();
        l.fetch_more(id, 100, Some(40), &mut sink).await.unwrap();
        let res = sink.into_results();
        let rs = rows_of(&res[0]);
        assert_eq!(rs.rows.len(), 100, "round {round}");
        assert!(rs.truncated);
        for r in &rs.rows {
            assert_eq!(r[0], Value::Int(next));
            next += 1;
        }
    }
    // Wrong id.
    let e = l
        .fetch_more(uuid::Uuid::new_v4(), 10, None, &mut CollectingSink::new())
        .await
        .unwrap_err();
    assert!(matches!(e, Error::NotFound(_)));
    // Drain the rest: the last call reports no truncation and the lease is clean.
    let mut sink = CollectingSink::new();
    l.fetch_more(id, 100_000, None, &mut sink).await.unwrap();
    let res = sink.into_results();
    let rs = rows_of(&res[0]);
    assert_eq!(rs.rows.len() as i64, 2500 - 400);
    assert!(!rs.truncated);
    assert_eq!(l.state().pending_result, None);
    // A new statement discards a pending result.
    let mut req = QueryRequest::new("SELECT id FROM dbo.paging");
    req.max_rows = Some(5);
    let id2 = req.query_id;
    l.execute(req, &mut CollectingSink::new()).await.unwrap();
    assert_eq!(l.state().pending_result, Some(id2));
    assert_eq!(scalar(&lrun(&*l, "SELECT 1").await), Value::Int(1));
    assert_eq!(l.state().pending_result, None);
    // Explicit discard.
    let mut req = QueryRequest::new("SELECT id FROM dbo.paging");
    req.max_rows = Some(5);
    l.execute(req, &mut CollectingSink::new()).await.unwrap();
    l.discard_result().await.unwrap();
    assert_eq!(l.state().pending_result, None);
    assert_eq!(scalar(&lrun(&*l, "SELECT 2").await), Value::Int(2));
    l.close().await.unwrap();
}

#[tokio::test]
async fn close_during_running_and_idempotent() {
    it!();
    let s = fixture().await;
    let l: Arc<dyn Lease> = Arc::from(open(&*s, false).await);
    lrun(&*l, "CREATE TABLE #t (a INT); INSERT #t VALUES (1)").await;
    assert_eq!(l.state().transaction, TxStatus::Active);
    let runner = {
        let l = l.clone();
        tokio::spawn(async move {
            l.execute(
                QueryRequest::new("WAITFOR DELAY '00:01:00'"),
                &mut CollectingSink::new(),
            )
            .await
        })
    };
    tokio::time::sleep(Duration::from_millis(500)).await;
    let t = Instant::now();
    let report = l.close().await.unwrap();
    assert!(t.elapsed() < Duration::from_secs(8), "{:?}", t.elapsed());
    assert!(report.rolled_back);
    assert!(runner.await.unwrap().is_err());
    let again = l.close().await.unwrap();
    assert!(!again.rolled_back && !again.forced);
    let e = l
        .execute(QueryRequest::new("SELECT 1"), &mut CollectingSink::new())
        .await
        .unwrap_err();
    assert_eq!(e.kind(), "connection");
}

#[tokio::test]
async fn explain_inside_a_transaction_keeps_the_outer_work() {
    it!();
    let s = work().await;
    make(&*s, "lease_ex").await;
    let l = open(&*s, true).await;
    l.begin(TxOptions::default()).await.unwrap();
    lrun(&*l, "INSERT INTO dbo.lease_ex VALUES (2, N'outer')").await;
    let plan = l
        .explain(ExplainRequest {
            query_id: uuid::Uuid::new_v4(),
            text: "UPDATE dbo.lease_ex SET v = N'changed'".into(),
            database: None,
            analyze: true,
            options: Default::default(),
            timeout_ms: None,
        })
        .await
        .unwrap();
    assert!(plan.rolled_back && plan.text.contains("<ShowPlanXML"));
    assert_eq!(
        l.state().transaction,
        TxStatus::Active,
        "the outer transaction survives"
    );
    let r = lrun(&*l, "SELECT v FROM dbo.lease_ex ORDER BY id").await;
    let vals: Vec<_> = rows_of(&r[0]).rows.iter().map(|r| text(&r[0])).collect();
    assert_eq!(
        vals,
        vec!["a", "outer"],
        "the analyzed update was undone, the insert kept"
    );
    l.rollback().await.unwrap();
    l.close().await.unwrap();
}

#[tokio::test]
async fn stream_table_and_bulk_insert() {
    it!();
    let s = work().await;
    run(&*s, "DROP TABLE IF EXISTS dbo.bulk_t;\nGO\nCREATE TABLE dbo.bulk_t (id INT IDENTITY(1,1) PRIMARY KEY, code NVARCHAR(10) NOT NULL UNIQUE, qty INT NULL, note NVARCHAR(50) NULL, at DATETIME2(3) NULL);").await;
    let l = open(&*s, true).await;
    let t = path(WORK_DB, "bulk_t");
    let row = |id: i64, code: &str, qty: Option<i64>| {
        vec![
            EditValue::Value(Value::Int(id)),
            EditValue::Text(code.to_string()),
            EditValue::Value(qty.map_or(Value::Null, Value::Int)),
            EditValue::Text("o'k 😀".into()),
            EditValue::Text("2024-05-31 13:45:00.123".into()),
        ]
    };
    let cols: Vec<String> = ["id", "code", "qty", "note", "at"]
        .iter()
        .map(|c| c.to_string())
        .collect();
    let rows: Vec<_> = (1..=2500)
        .map(|i| row(i, &format!("c{i}"), (i % 3 != 0).then_some(i)))
        .collect();
    // Plain insert of 2500 rows (five statements) with explicit identity values, atomically.
    let res = l
        .bulk_insert(BulkInsertRequest {
            table: t.clone(),
            columns: cols.clone(),
            rows,
            on_conflict: OnConflict::Error,
            conflict_columns: vec![],
        })
        .await
        .unwrap();
    assert_eq!((res.inserted, res.updated, res.skipped), (2500, 0, 0));
    assert_eq!(
        scalar(&lrun(&*l, "SELECT COUNT(*), MAX(id) FROM dbo.bulk_t").await),
        Value::Int(2500)
    );
    // A conflict rolls the whole call back.
    let dup = l
        .bulk_insert(BulkInsertRequest {
            table: t.clone(),
            columns: cols.clone(),
            rows: vec![row(5000, "new", Some(1)), row(1, "c1", Some(1))],
            on_conflict: OnConflict::Error,
            conflict_columns: vec![],
        })
        .await;
    assert!(dup.is_err());
    assert_eq!(
        scalar(&lrun(&*l, "SELECT COUNT(*) FROM dbo.bulk_t").await),
        Value::Int(2500)
    );
    // Skip and update by the primary key.
    let res = l
        .bulk_insert(BulkInsertRequest {
            table: t.clone(),
            columns: cols.clone(),
            rows: vec![row(1, "c1", Some(111)), row(2501, "c2501", None)],
            on_conflict: OnConflict::Skip,
            conflict_columns: vec![],
        })
        .await
        .unwrap();
    assert_eq!((res.inserted, res.updated, res.skipped), (1, 0, 1));
    let res = l
        .bulk_insert(BulkInsertRequest {
            table: t.clone(),
            columns: vec!["code".into(), "qty".into()],
            rows: vec![
                vec![
                    EditValue::Text("c2".into()),
                    EditValue::Value(Value::Int(222)),
                ],
                vec![
                    EditValue::Text("brand-new".into()),
                    EditValue::Value(Value::Null),
                ],
            ],
            on_conflict: OnConflict::Update,
            conflict_columns: vec!["code".into()],
        })
        .await
        .unwrap();
    assert_eq!((res.inserted, res.updated, res.skipped), (1, 1, 0));
    let r = lrun(&*l, "SELECT qty, note FROM dbo.bulk_t WHERE code = N'c2'").await;
    assert_eq!(
        rows_of(&r[0]).rows[0],
        vec![Value::Int(222), Value::Text("o'k 😀".into())]
    );
    // Inside a manual transaction the rows join it and roll back with it.
    l.begin(TxOptions::default()).await.unwrap();
    l.bulk_insert(BulkInsertRequest {
        table: t.clone(),
        columns: vec!["code".into()],
        rows: vec![vec![EditValue::Text("tx-row".into())]],
        on_conflict: OnConflict::Error,
        conflict_columns: vec![],
    })
    .await
    .unwrap();
    l.rollback().await.unwrap();
    assert_eq!(
        scalar(
            &lrun(
                &*l,
                "SELECT COUNT(*) FROM dbo.bulk_t WHERE code = N'tx-row'"
            )
            .await
        ),
        Value::Int(0)
    );

    // Streaming: columns subset, filter, order by key; batches of 250.
    let mut sink = CollectingSink::new();
    let summary = l
        .stream_table(
            TableStreamRequest {
                query_id: uuid::Uuid::new_v4(),
                table: t.clone(),
                columns: vec!["id".into(), "code".into()],
                filters: vec![dbstudio_core::ColumnFilter {
                    column: "qty".into(),
                    op: dbstudio_core::FilterOp::IsNull,
                    values: vec![],
                }],
                raw_where: None,
                order_by_key: true,
                batch_size: Some(250),
            },
            &mut sink,
        )
        .await
        .unwrap();
    assert_eq!(summary.statements, 1);
    let res = sink.into_results();
    let rs = rows_of(&res[0]);
    assert_eq!(rs.columns.len(), 2);
    assert_eq!(
        rs.rows.len(),
        833 + 1 + 1,
        "every third row, the NULL-qty one and brand-new"
    );
    let ids: Vec<i64> = rs
        .rows
        .iter()
        .map(|r| if let Value::Int(i) = r[0] { i } else { 0 })
        .collect();
    assert!(ids.windows(2).all(|w| w[0] < w[1]), "ordered by the key");
    // Whole table streamed without a filter.
    let mut sink = CollectingSink::new();
    l.stream_table(
        TableStreamRequest {
            query_id: uuid::Uuid::new_v4(),
            table: t,
            columns: vec![],
            filters: vec![],
            raw_where: None,
            order_by_key: false,
            batch_size: None,
        },
        &mut sink,
    )
    .await
    .unwrap();
    assert_eq!(rows_of(&sink.into_results()[0]).rows.len(), 2502);
    l.close().await.unwrap();
}

#[tokio::test]
async fn a_lost_connection_marks_the_lease_broken() {
    it!();
    let s = work().await;
    let l = open(&*s, true).await;
    let spid = scalar(&lrun(&*l, "SELECT @@SPID").await);
    let Value::Int(spid) = spid else { panic!() };
    // Kill the lease's session from outside.
    run(&*master().await, &format!("KILL {spid}")).await;
    let mut sink = CollectingSink::new();
    let e = l
        .execute(QueryRequest::new("SELECT 1"), &mut sink)
        .await
        .unwrap_err();
    assert_eq!(e.kind(), "connection", "{e}");
    assert!(l.state().broken);
    let e = l
        .execute(QueryRequest::new("SELECT 1"), &mut sink)
        .await
        .unwrap_err();
    assert_eq!(e.kind(), "connection");
    l.close().await.unwrap();
}
