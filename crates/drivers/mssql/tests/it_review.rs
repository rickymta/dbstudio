//! Regression tests of the review of package I: sandboxed explain, pooled connections that must
//! come back clean, cancel races, doomed transactions, key checks, DDL round trips, LOB limits.

mod common;

use std::sync::Arc;

use common::*;
use dbstudio_core::{
    BulkInsertRequest, ChangeSet, CollectingSink, EditValue, Error, ExplainRequest, Lease,
    LeaseOptions, ObjectKind, ObjectPath, OnConflict, QueryRequest, RowChange, RowChangeSet,
    TableDataRequest, TableStreamRequest, TlsMode, TxOptions, TxStatus, Value, folder,
};

macro_rules! it {
    () => {
        if !it_enabled() {
            return;
        }
    };
}

fn explain_req(text: &str, analyze: bool) -> ExplainRequest {
    ExplainRequest {
        query_id: uuid::Uuid::new_v4(),
        text: text.into(),
        database: None,
        analyze,
        options: Default::default(),
        timeout_ms: None,
    }
}

fn tbl(db: &str, name: &str) -> ObjectPath {
    ObjectPath::root()
        .child(ObjectKind::Database, db)
        .child(ObjectKind::Folder, folder::SCHEMAS)
        .child(ObjectKind::Schema, "dbo")
        .child(ObjectKind::Folder, folder::TABLES)
        .child(ObjectKind::Table, name)
}

async fn count(s: &dyn dbstudio_core::Session, table: &str) -> Value {
    let r = run(s, &format!("SELECT COUNT(*) FROM {table}")).await;
    rows_of(&r[0]).rows[0][0].clone()
}

// ---- 1, 2: explain is a sandbox -------------------------------------------------------------

#[tokio::test]
async fn explain_refuses_statements_that_could_escape_the_sandbox() {
    it!();
    let s = work().await;
    run(&*s, "DROP TABLE IF EXISTS dbo.sandbox;\nGO\nCREATE TABLE dbo.sandbox (id INT PRIMARY KEY);\nGO\nINSERT INTO dbo.sandbox VALUES (1), (2);").await;
    for bad in [
        "DELETE FROM dbo.sandbox; COMMIT",
        "DELETE FROM dbo.sandbox COMMIT",
        "SELECT 1; TRUNCATE TABLE dbo.sandbox",
        "GO\nSET SHOWPLAN_XML OFF\nGO\nDELETE FROM dbo.sandbox",
        "SELECT 1\nGO\nDELETE FROM dbo.sandbox",
        "SET SHOWPLAN_XML OFF",
        "COMMIT",
        "EXEC('DELETE FROM dbo.sandbox')",
    ] {
        for analyze in [false, true] {
            let r = s.explain(explain_req(bad, analyze)).await;
            assert!(
                matches!(r, Err(Error::Unsupported(_))),
                "{bad:?} analyze={analyze}: {r:?}"
            );
        }
    }
    assert_eq!(
        count(&*s, "dbo.sandbox").await,
        Value::Int(2),
        "nothing was written"
    );
    // Writes are still analyzed, inside the rolled back transaction.
    let p = s
        .explain(explain_req("DELETE FROM dbo.sandbox", true))
        .await
        .unwrap();
    assert!(p.rolled_back);
    assert_eq!(count(&*s, "dbo.sandbox").await, Value::Int(2));
}

// ---- 4, 13: pooled connections come back clean ---------------------------------------------

#[tokio::test]
async fn pooled_connections_never_leak_session_state() {
    it!();
    let s = fixture().await;
    let one =
        |r: Vec<dbstudio_core::StatementResult>| rows_of(r.last().unwrap()).rows[0][0].clone();
    for (change, probe, expected) in [
        (
            "SET LANGUAGE German",
            "SELECT @@LANGUAGE",
            Value::Text("us_english".into()),
        ),
        ("SET DATEFIRST 3", "SELECT @@DATEFIRST", Value::Int(7)),
        (
            "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE",
            "SELECT transaction_isolation_level FROM sys.dm_exec_sessions WHERE session_id = @@SPID",
            Value::Int(2),
        ),
        (
            "SET LOCK_TIMEOUT 7",
            "SELECT @@LOCK_TIMEOUT",
            Value::Int(-1),
        ),
        ("BEGIN TRANSACTION", "SELECT @@TRANCOUNT", Value::Int(0)),
        // The statement text hides the SET from any keyword scan.
        (
            "EXEC('SET ' + 'LANGUAGE French')",
            "SELECT @@LANGUAGE",
            Value::Text("us_english".into()),
        ),
        (
            "EXEC('US' + 'E master')",
            "SELECT DB_NAME()",
            Value::Text(IT_DB.into()),
        ),
    ] {
        run(&*s, change).await;
        assert_eq!(one(run(&*s, probe).await), expected, "after `{change}`");
    }
    // NOCOUNT hidden in dynamic SQL: the next call still reports counts.
    run(&*s, "EXEC('SET NOCOUNT ' + 'ON')").await;
    let r = run(&*s, "SELECT 1 AS a; SELECT 2 AS b").await;
    assert_eq!(r.len(), 2);
    // A temp table does not survive into the next call either.
    run(&*s, "CREATE TABLE #leak (a INT)").await;
    assert_eq!(
        one(run(&*s, "SELECT OBJECT_ID('tempdb..#leak')").await),
        Value::Null
    );
    // Reading a table works after all that (explicit READ COMMITTED).
    let mut sink = CollectingSink::new();
    s.fetch_table(
        TableDataRequest {
            query_id: uuid::Uuid::new_v4(),
            table: tbl(IT_DB, "paging"),
            filters: vec![],
            raw_where: None,
            sort: vec![],
            offset: 0,
            limit: 3,
            batch_size: None,
        },
        &mut sink,
    )
    .await
    .unwrap();
}

// ---- 3: cancel races ---------------------------------------------------------------------

#[tokio::test]
async fn late_cancels_never_corrupt_the_lease() {
    it!();
    let s = fixture().await;
    let l: Arc<dyn Lease> = Arc::from(s.open_lease(LeaseOptions::default()).await.unwrap());
    for _ in 0..25 {
        let req = QueryRequest::new("SELECT 1 AS a");
        let id = req.query_id;
        let canceller = {
            let l = l.clone();
            tokio::spawn(async move {
                for _ in 0..3 {
                    l.cancel(id).await.unwrap();
                    tokio::task::yield_now().await;
                }
            })
        };
        let mut sink = CollectingSink::new();
        match l.execute(req, &mut sink).await {
            Ok(_) | Err(Error::Cancelled) => {}
            Err(e) => panic!("unexpected {e}"),
        }
        canceller.await.unwrap();
        let st = l.state();
        assert!(
            !st.broken && st.transaction == TxStatus::Idle && st.running.is_none(),
            "{st:?}"
        );
    }
    // The next statement is not hit by a stale cancel.
    let mut sink = CollectingSink::new();
    l.execute(QueryRequest::new("SELECT 42"), &mut sink)
        .await
        .unwrap();
    assert_eq!(rows_of(&sink.into_results()[0]).rows[0][0], Value::Int(42));
    l.close().await.unwrap();
}

// ---- 5, 6: lease transaction state -----------------------------------------------------

/// With `XACT_ABORT ON` the next failing statement makes the server end the transaction.
async fn arm_abort(l: &dyn Lease) {
    let mut sink = CollectingSink::new();
    l.execute(QueryRequest::new("SET XACT_ABORT ON"), &mut sink)
        .await
        .unwrap();
}

#[tokio::test]
async fn auto_commit_follows_the_server_and_doomed_transactions_recover() {
    it!();
    let s = work().await;
    run(
        &*s,
        "DROP TABLE IF EXISTS dbo.doom_t;\nGO\nCREATE TABLE dbo.doom_t (id INT PRIMARY KEY);",
    )
    .await;
    let l = s.open_lease(LeaseOptions::default()).await.unwrap();
    // The user switches the mode with plain SQL.
    let mut sink = CollectingSink::new();
    l.execute(QueryRequest::new("SET IMPLICIT_TRANSACTIONS ON"), &mut sink)
        .await
        .unwrap();
    assert!(!l.state().auto_commit);
    l.execute(
        QueryRequest::new("SET IMPLICIT_TRANSACTIONS OFF"),
        &mut sink,
    )
    .await
    .unwrap();
    assert!(l.state().auto_commit);

    // Doomed transaction + analyze: everything is rolled back, the lease lives on.
    l.begin(TxOptions::default()).await.unwrap();
    arm_abort(&*l).await;
    assert_eq!(l.state().transaction, TxStatus::Active);
    let e = l
        .explain(explain_req("SELECT 1/0 FROM sys.objects", true))
        .await
        .unwrap_err();
    assert!(e.to_string().contains("rolled back"), "{e}");
    let st = l.state();
    assert!(!st.broken && st.transaction == TxStatus::Idle, "{st:?}");
    let mut sink = CollectingSink::new();
    l.execute(
        QueryRequest::new("INSERT INTO dbo.doom_t VALUES (1)"),
        &mut sink,
    )
    .await
    .unwrap();

    // Aborted transaction + bulk insert (duplicate key under XACT_ABORT).
    l.begin(TxOptions::default()).await.unwrap();
    let e = l
        .bulk_insert(BulkInsertRequest {
            table: tbl(WORK_DB, "doom_t"),
            columns: vec!["id".into()],
            rows: vec![vec![EditValue::Value(Value::Int(1))]],
            on_conflict: OnConflict::Error,
            conflict_columns: vec![],
        })
        .await
        .unwrap_err();
    assert!(e.to_string().contains("rolled back"), "{e}");
    let st = l.state();
    assert!(!st.broken && st.transaction == TxStatus::Idle, "{st:?}");
    assert_eq!(
        count(&*s, "dbo.doom_t").await,
        Value::Int(1),
        "only the committed row exists"
    );
    // Still usable.
    let mut sink = CollectingSink::new();
    l.execute(QueryRequest::new("SELECT 1"), &mut sink)
        .await
        .unwrap();
    l.close().await.unwrap();
}

// ---- 7: the key of a change set is checked ----------------------------------------------

#[tokio::test]
async fn change_sets_must_use_a_real_key() {
    it!();
    let s = work().await;
    run(&*s, "DROP TABLE IF EXISTS dbo.keyed;\nGO\nCREATE TABLE dbo.keyed (id INT PRIMARY KEY, grp INT NOT NULL, v NVARCHAR(10));\nGO\nINSERT INTO dbo.keyed VALUES (1, 7, N'a'), (2, 7, N'b');").await;
    // `grp` matches two rows but is not a key.
    let cs = ChangeSet::Rows(RowChangeSet {
        table: tbl(WORK_DB, "keyed"),
        key_columns: vec!["grp".into()],
        changes: vec![RowChange::Delete {
            key: vec![Value::Int(7)],
        }],
    });
    let e = s.apply_changes(cs).await.unwrap_err();
    assert!(e.to_string().contains("key_columns"), "{e}");
    assert_eq!(count(&*s, "dbo.keyed").await, Value::Int(2));
    // The real key works.
    let cs = ChangeSet::Rows(RowChangeSet {
        table: tbl(WORK_DB, "keyed"),
        key_columns: vec!["id".into()],
        changes: vec![RowChange::Delete {
            key: vec![Value::Int(1)],
        }],
    });
    assert_eq!(s.apply_changes(cs).await.unwrap().affected, vec![1]);
}

// ---- 8: DDL round trip ----------------------------------------------------------------------

const DDL_SRC: &str = "dbstudio_it_ddl";
const DDL_DST: &str = "dbstudio_it_ddl_copy";

async fn recreate(m: &dyn dbstudio_core::Session, name: &str, collation: &str) {
    run(m, &format!("IF DB_ID(N'{name}') IS NOT NULL BEGIN ALTER DATABASE [{name}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{name}]; END\nGO\nCREATE DATABASE [{name}] COLLATE {collation}")).await;
}

async fn signature(s: &dyn dbstudio_core::Session) -> Vec<String> {
    let r = run(
        s,
        "SELECT x FROM (
           SELECT CONCAT('col|', OBJECT_NAME(c.object_id), '|', c.name, '|', c.collation_name, '|', c.is_nullable, '|', c.is_computed, '|', c.is_identity) AS x
             FROM sys.columns c JOIN sys.objects o ON o.object_id = c.object_id WHERE o.is_ms_shipped = 0 AND o.type = 'U'
           UNION ALL SELECT CONCAT('persisted|', OBJECT_NAME(object_id), '|', name, '|', is_persisted) FROM sys.computed_columns
           UNION ALL SELECT CONCAT('ident|', OBJECT_NAME(object_id), '|', name, '|', is_not_for_replication) FROM sys.identity_columns
           UNION ALL SELECT CONCAT('chk|', OBJECT_NAME(parent_object_id), '|', name, '|', is_disabled, '|', is_not_trusted, '|', is_not_for_replication) FROM sys.check_constraints
           UNION ALL SELECT CONCAT('fk|', OBJECT_NAME(parent_object_id), '|', name, '|', is_disabled, '|', is_not_trusted, '|', is_not_for_replication) FROM sys.foreign_keys
           UNION ALL SELECT CONCAT('idx|', OBJECT_NAME(object_id), '|', name, '|', type, '|', is_unique, '|', ignore_dup_key, '|', fill_factor, '|', ISNULL(filter_definition, '')) FROM sys.indexes WHERE name IS NOT NULL AND OBJECTPROPERTY(object_id, 'IsMSShipped') = 0
           UNION ALL SELECT CONCAT('type|', name, '|', is_table_type) FROM sys.types WHERE is_user_defined = 1
         ) q ORDER BY x",
    )
    .await;
    rows_of(&r[0]).rows.iter().map(|r| text(&r[0])).collect()
}

#[tokio::test]
async fn ddl_round_trip_keeps_constraints_options_collations_and_sequences() {
    it!();
    ensure_fixture().await;
    let m = master().await;
    recreate(&*m, DDL_SRC, "Latin1_General_100_CI_AS").await;
    recreate(&*m, DDL_DST, "SQL_Latin1_General_CP1_CS_AS").await;
    let src = session_on(DDL_SRC).await;
    run(
        &*src,
        "CREATE TYPE dbo.Pair AS TABLE (c dbo.Code, n INT NOT NULL DEFAULT 1);"
            .replace(
                "CREATE TYPE dbo.Pair",
                "CREATE TYPE dbo.Code FROM NVARCHAR(10) NOT NULL;\nGO\nCREATE TYPE dbo.Pair",
            )
            .as_str(),
    )
    .await;
    run(&*src, "CREATE SEQUENCE dbo.sq AS INT START WITH 10 INCREMENT BY 3 MINVALUE 1 MAXVALUE 1000;\nGO\nSELECT NEXT VALUE FOR dbo.sq; SELECT NEXT VALUE FOR dbo.sq; SELECT NEXT VALUE FOR dbo.sq;").await;
    run(&*src, "CREATE TABLE dbo.parent (
        id INT IDENTITY(1,1) NOT FOR REPLICATION NOT NULL CONSTRAINT PK_parent PRIMARY KEY WITH (FILLFACTOR = 80),
        code NVARCHAR(20) COLLATE SQL_Latin1_General_CP1_CS_AS NOT NULL,
        name NVARCHAR(20) NULL,
        qty INT NOT NULL,
        dbl AS (qty * 2) PERSISTED NOT NULL,
        CONSTRAINT UQ_parent_code UNIQUE (code) WITH (IGNORE_DUP_KEY = ON),
        CONSTRAINT CK_qty CHECK (qty >= 0)
      );
      GO
      CREATE TABLE dbo.child (id INT NOT NULL PRIMARY KEY, pid INT NOT NULL,
        CONSTRAINT FK_child_parent FOREIGN KEY (pid) REFERENCES dbo.parent (id) NOT FOR REPLICATION,
        CONSTRAINT CK_child CHECK (id > 0));
      GO
      INSERT INTO dbo.parent (code, qty) VALUES (N'a', 1);
      ALTER TABLE dbo.parent NOCHECK CONSTRAINT CK_qty;
      INSERT INTO dbo.parent (code, qty) VALUES (N'neg', -5);
      ALTER TABLE dbo.child NOCHECK CONSTRAINT FK_child_parent;
      INSERT INTO dbo.child VALUES (1, 999);
      ALTER TABLE dbo.child NOCHECK CONSTRAINT CK_child;
      INSERT INTO dbo.child VALUES (-1, 1);
      ALTER TABLE dbo.child CHECK CONSTRAINT CK_child;
      GO
      CREATE UNIQUE INDEX IX_filtered ON dbo.parent (name) WHERE name IS NOT NULL;
      GO
      CREATE VIEW dbo.v WITH SCHEMABINDING AS SELECT id, qty FROM dbo.parent;
      GO
      CREATE UNIQUE CLUSTERED INDEX CIX_v ON dbo.v (id);").await;
    // Sanity of the source: the constraints really are in the odd states.
    let sig = signature(&*src).await;
    assert!(
        sig.iter().any(|x| x == "chk|parent|CK_qty|1|1|0"),
        "{sig:#?}"
    );
    assert!(
        sig.iter().any(|x| x == "chk|child|CK_child|0|1|0"),
        "{sig:#?}"
    );
    assert!(
        sig.iter().any(|x| x == "fk|child|FK_child_parent|1|1|1"),
        "{sig:#?}"
    );

    let items = src
        .generate_ddl(dbstudio_core::DdlRequest {
            scope: ObjectPath::root().child(ObjectKind::Database, DDL_SRC),
            objects: vec![],
            options: Default::default(),
        })
        .await
        .unwrap();
    // Alias types are created before table types.
    let pos = |name: &str| {
        items
            .iter()
            .position(|i| i.kind == ObjectKind::Type && i.path.last().unwrap().name == name)
            .unwrap()
    };
    assert!(pos("Code") < pos("Pair"));
    let dst = session_on(DDL_DST).await;
    for it in &items {
        run(&*dst, &it.sql).await;
    }
    assert_eq!(signature(&*src).await, signature(&*dst).await);
    // The sequence continues where the source stopped (3 values were used: 10, 13, 16).
    if !legacy_forced() {
        let r = run(&*dst, "SELECT NEXT VALUE FOR dbo.sq").await;
        assert_eq!(rows_of(&r[0]).rows[0][0], Value::Int(19));
    }
    // Re-running with guards is harmless, including the indexed view and the untrusted checks.
    let again = src
        .generate_ddl(dbstudio_core::DdlRequest {
            scope: ObjectPath::root().child(ObjectKind::Database, DDL_SRC),
            objects: vec![],
            options: dbstudio_core::DdlOptions {
                if_not_exists: true,
                ..Default::default()
            },
        })
        .await
        .unwrap();
    for it in &again {
        run(&*dst, &it.sql).await;
    }
    assert_eq!(signature(&*src).await, signature(&*dst).await);
    // object_ddl of the table shows the same states.
    let t = src.object_ddl(&tbl(DDL_SRC, "child")).await.unwrap();
    assert!(
        t.contains("WITH NOCHECK ADD CONSTRAINT [CK_child] CHECK"),
        "{t}"
    );
    assert!(t.contains("NOT FOR REPLICATION"), "{t}");
    for s in [src, dst] {
        s.close().await.ok();
    }
    run(&*m, &format!("ALTER DATABASE [{DDL_SRC}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{DDL_SRC}]; ALTER DATABASE [{DDL_DST}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{DDL_DST}]")).await;
}

// ---- 9: large values are cut on the server ------------------------------------------------

#[tokio::test]
async fn large_values_are_limited_when_reading_tables() {
    it!();
    ensure_fixture().await;
    let setup = work().await;
    run(&*setup, "DROP TABLE IF EXISTS dbo.lob_t;\nGO\nCREATE TABLE dbo.lob_t (id INT PRIMARY KEY, t NVARCHAR(MAX), b VARBINARY(MAX), x XML, s NVARCHAR(50));\nGO\nINSERT INTO dbo.lob_t VALUES (1, REPLICATE(CAST(N'é' AS NVARCHAR(MAX)), 20000), CAST(REPLICATE(CAST('ab' AS VARCHAR(MAX)), 20000) AS VARBINARY(MAX)), CAST(N'<r>' + REPLICATE(CAST(N'x' AS NVARCHAR(MAX)), 20000) + N'</r>' AS XML), N'short'), (2, N'tiny', 0x0102, N'<a/>', N'ok');").await;
    let mut cfg = config(Some(WORK_DB), TlsMode::Require);
    cfg.options.insert("max_lob_bytes".into(), "1000".into());
    let s: Arc<dyn dbstudio_core::Session> = Arc::from(connect_cfg(&cfg).await.unwrap());
    let mut sink = CollectingSink::new();
    s.fetch_table(
        TableDataRequest {
            query_id: uuid::Uuid::new_v4(),
            table: tbl(WORK_DB, "lob_t"),
            filters: vec![],
            raw_where: None,
            sort: vec![],
            offset: 0,
            limit: 10,
            batch_size: None,
        },
        &mut sink,
    )
    .await
    .unwrap();
    let res = sink.into_results();
    let rs = rows_of(&res[0]);
    let col = |n: &str| rs.columns.iter().position(|c| c.name == n).unwrap();
    for (name, ty) in [
        ("t", "truncatedText"),
        ("x", "truncatedText"),
        ("b", "truncatedBytes"),
    ] {
        match &rs.rows[0][col(name)] {
            Value::Ext(e) => {
                assert_eq!(e.ty, ty, "{name}");
                assert!(
                    e.text.len() <= 1400,
                    "{name}: {} bytes travelled",
                    e.text.len()
                );
            }
            other => panic!("{name}: {other:?}"),
        }
    }
    assert_eq!(rs.rows[0][col("s")], Value::Text("short".into()));
    assert_eq!(rs.rows[1][col("t")], Value::Text("tiny".into()));
    assert_eq!(rs.rows[1][col("b")], Value::Bytes(vec![1, 2]));
    // Export streaming reads large values whole (the limit is for interactive reads only).
    let l = s.open_lease(LeaseOptions::default()).await.unwrap();
    let mut sink = CollectingSink::new();
    l.stream_table(
        TableStreamRequest {
            query_id: uuid::Uuid::new_v4(),
            table: tbl(WORK_DB, "lob_t"),
            columns: vec!["id".into(), "t".into()],
            filters: vec![],
            raw_where: None,
            order_by_key: true,
            batch_size: None,
        },
        &mut sink,
    )
    .await
    .unwrap();
    let res = sink.into_results();
    let rs = rows_of(&res[0]);
    assert!(matches!(&rs.rows[0][1], Value::Text(t) if *t == "é".repeat(20000)));
    assert_eq!(rs.rows[1][1], Value::Text("tiny".into()));
    l.close().await.unwrap();
}

// ---- 10: conflict policies with typed values -------------------------------------------------

#[tokio::test]
async fn conflict_policies_cast_values_and_reject_default() {
    it!();
    let s = work().await;
    run(&*s, "DROP TABLE IF EXISTS dbo.bulk_c;\nGO\nCREATE TABLE dbo.bulk_c (id INT PRIMARY KEY, at DATETIME2(3) NULL, g UNIQUEIDENTIFIER NULL, d DECIMAL(10,2) NULL, n NVARCHAR(5) NULL);").await;
    let l = s.open_lease(LeaseOptions::default()).await.unwrap();
    let row = |id: i64, at: Option<&str>| {
        vec![
            EditValue::Value(Value::Int(id)),
            at.map_or(EditValue::Value(Value::Null), |a| EditValue::Text(a.into())),
            EditValue::Value(Value::Null),
            EditValue::Value(Value::Decimal("12.50".into())),
            EditValue::Text("x".into()),
        ]
    };
    let cols: Vec<String> = ["id", "at", "g", "d", "n"]
        .iter()
        .map(|c| c.to_string())
        .collect();
    let req = |rows, on_conflict| BulkInsertRequest {
        table: tbl(WORK_DB, "bulk_c"),
        columns: cols.clone(),
        rows,
        on_conflict,
        conflict_columns: vec![],
    };
    // All-NULL columns (guid) and typed text (datetime) work in the derived table.
    let r = l
        .bulk_insert(req(
            vec![row(1, Some("2024-05-31 13:45:00.123")), row(2, None)],
            OnConflict::Skip,
        ))
        .await
        .unwrap();
    assert_eq!((r.inserted, r.skipped), (2, 0));
    let r = l
        .bulk_insert(req(
            vec![row(2, Some("2025-01-01")), row(3, None)],
            OnConflict::Update,
        ))
        .await
        .unwrap();
    assert_eq!((r.inserted, r.updated), (1, 1));
    // DEFAULT is refused instead of producing invalid SQL.
    let mut rows = vec![row(9, None)];
    rows[0][4] = EditValue::Default;
    let e = l
        .bulk_insert(req(rows.clone(), OnConflict::Skip))
        .await
        .unwrap_err();
    assert_eq!(e.kind(), "unsupported");
    // ... but fine in a plain insert.
    l.bulk_insert(req(rows, OnConflict::Error)).await.unwrap();
    assert_eq!(count(&*s, "dbo.bulk_c").await, Value::Int(4));
    l.close().await.unwrap();
}
