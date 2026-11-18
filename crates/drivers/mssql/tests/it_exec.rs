//! Integration tests: connection, query execution, multiple result sets, PRINT, `GO`, cancel.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use dbstudio_core::{
    CollectingSink, ColumnMeta, Driver, EndOutcome, Error, QueryRequest, ResultSink, StatementEnd,
    StatementOutcome, TlsMode, Value, execute_collect,
};

macro_rules! it {
    () => {
        if !it_enabled() {
            return;
        }
    };
}

#[tokio::test]
async fn smoke_select_and_metadata() {
    it!();
    let s = fixture().await;
    let r = run(&*s, "SELECT id, name FROM dbo.simple_t ORDER BY id").await;
    assert_eq!(r.len(), 1);
    let rs = rows_of(&r[0]);
    assert_eq!(rs.rows.len(), 3);
    assert_eq!(rs.rows[0], vec![Value::Int(1), Value::Text("one".into())]);
    assert_eq!(rs.rows[2][1], Value::Null);
    assert_eq!(
        (
            rs.columns[0].type_name.as_str(),
            rs.columns[1].type_name.as_str()
        ),
        ("int", "nvarchar")
    );
    let info = s.server_info().await.unwrap();
    assert_eq!(info.product, "Microsoft SQL Server");
    assert!(info.version.starts_with("16."), "{}", info.version);
    assert!(info.extra.contains_key("edition"));
    s.ping().await.unwrap();
    assert_eq!(s.script_dialect(), dbstudio_core::ScriptDialect::MsSql);
}

#[tokio::test]
async fn script_of_several_statements_gives_one_result_each() {
    it!();
    let s = work().await;
    run(
        &*s,
        "DROP TABLE IF EXISTS dbo.exec_a;\nGO\nCREATE TABLE dbo.exec_a (a INT);",
    )
    .await;
    let r = run(
        &*s,
        "INSERT INTO dbo.exec_a VALUES (1),(2),(3); UPDATE dbo.exec_a SET a = a + 1 WHERE a > 1; \
         SELECT a FROM dbo.exec_a ORDER BY a; PRINT N'done é'; DELETE FROM dbo.exec_a",
    )
    .await;
    assert_eq!(
        r.len(),
        5,
        "{:#?}",
        r.iter().map(|x| &x.outcome).collect::<Vec<_>>()
    );
    assert_eq!(r[0].outcome, StatementOutcome::Affected { rows: 3 });
    assert_eq!(r[1].outcome, StatementOutcome::Affected { rows: 2 });
    assert_eq!(
        rows_of(&r[2]).rows,
        vec![
            vec![Value::Int(1)],
            vec![Value::Int(3)],
            vec![Value::Int(4)]
        ]
    );
    // PRINT is a statement of its own; its message is attached to its end.
    assert_eq!(r[3].messages, vec!["done é".to_string()]);
    assert_eq!(r[4].outcome, StatementOutcome::Affected { rows: 3 });
}

#[tokio::test]
async fn multiple_result_sets_and_messages_from_one_batch() {
    it!();
    let s = fixture().await;
    let r = run(
        &*s,
        "SELECT 1 AS a; RAISERROR(N'low severity', 5, 1) WITH NOWAIT; SELECT 2 AS b, N'x' AS c; EXEC sales.sp_customer_count",
    )
    .await;
    let sets: Vec<_> = r
        .iter()
        .filter(|x| matches!(x.outcome, StatementOutcome::Rows(_)))
        .collect();
    assert_eq!(sets.len(), 3);
    assert_eq!(rows_of(sets[1]).columns.len(), 2);
    assert_eq!(rows_of(sets[2]).rows[0][0], Value::Int(4));
    assert!(
        r.iter()
            .any(|x| x.messages.iter().any(|m| m == "low severity"))
    );
}

#[tokio::test]
async fn go_separates_batches_and_repeats() {
    it!();
    let s = work().await;
    run(&*s, "DROP TABLE IF EXISTS dbo.exec_go;\nGO\nCREATE TABLE dbo.exec_go (id INT IDENTITY, v NVARCHAR(10));").await;
    let r = run(&*s, "INSERT INTO dbo.exec_go (v) VALUES (N'x')\nGO 3\nSELECT COUNT(*) FROM dbo.exec_go\ngo\nSELECT 'GO inside string:\nGO\nstill text' AS t").await;
    let affected: Vec<_> = r
        .iter()
        .filter(|x| matches!(x.outcome, StatementOutcome::Affected { .. }))
        .collect();
    assert_eq!(affected.len(), 3, "GO 3 runs the batch three times");
    let sets: Vec<_> = r
        .iter()
        .filter_map(|x| match &x.outcome {
            StatementOutcome::Rows(rs) => Some(rs),
            _ => None,
        })
        .collect();
    assert_eq!(sets.len(), 2);
    assert_eq!(sets[0].rows[0][0], Value::Int(3));
    assert_eq!(
        text(&sets[1].rows[0][0]),
        "GO inside string:\nGO\nstill text"
    );
    // Without GO the same statements are one batch and CREATE VIEW fails; with GO it works.
    let r = try_run(&*s, "CREATE VIEW dbo.exec_v AS SELECT 1 AS x\nGO\nSELECT * FROM dbo.exec_v\nGO\nDROP VIEW dbo.exec_v").await.unwrap();
    assert_eq!(rows_of(&r[1]).rows[0][0], Value::Int(1));
    // A failing batch stops the script; earlier batches stay done.
    let e = try_run(&*s, "INSERT INTO dbo.exec_go (v) VALUES (N'ok')\nGO\nSELECT * FROM dbo.nope\nGO\nINSERT INTO dbo.exec_go (v) VALUES (N'never')").await.unwrap_err();
    assert!(is_query_err(&e));
    let r = run(&*s, "SELECT COUNT(*) FROM dbo.exec_go WHERE v = N'never'").await;
    assert_eq!(rows_of(&r[0]).rows[0][0], Value::Int(0));
}

#[tokio::test]
async fn error_positions_are_character_offsets_into_the_whole_text() {
    it!();
    let s = fixture().await;
    let text = "SELECT '😀é' AS x\nGO\nSELECT 1\nSELECT * FROM dbo.missing_table_zz\n";
    let e = try_run(&*s, text).await.unwrap_err();
    let expected = text.chars().take_while(|_| true).collect::<String>();
    let byte = expected.find("SELECT * FROM dbo.missing").unwrap();
    let chars = expected[..byte].chars().count() as u32;
    match e {
        Error::Query { message, position } => {
            assert!(message.contains("missing_table_zz"), "{message}");
            assert_eq!(position, Some(chars));
        }
        other => panic!("{other:?}"),
    }
    // A syntax error in a single line batch points at the start of that line.
    let e = try_run(&*s, "SELECT 1 +").await.unwrap_err();
    assert!(
        matches!(
            e,
            Error::Query {
                position: Some(0),
                ..
            }
        ),
        "{e:?}"
    );
    // Errors inside a procedure carry no position (their line numbers are the procedure's).
    run(
        &*s,
        "CREATE OR ALTER PROCEDURE dbo.p_err AS BEGIN SELECT 1/0; END",
    )
    .await;
    let e = try_run(&*s, "EXEC dbo.p_err").await.unwrap_err();
    assert!(matches!(e, Error::Query { position: None, .. }), "{e:?}");
    run(&*s, "DROP PROCEDURE dbo.p_err").await;
}

#[tokio::test]
async fn cancel_stops_a_long_wait_quickly() {
    it!();
    let s = fixture().await;
    let req = QueryRequest::new("WAITFOR DELAY '00:01:07'; SELECT 1");
    let id = req.query_id;
    let sess = s.clone();
    let started = Instant::now();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        sess.cancel(id).await
    });
    let r = execute_collect(&*s, req).await;
    canceller.await.unwrap().unwrap();
    assert!(matches!(r, Err(Error::Cancelled)), "{r:?}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "took {:?}",
        started.elapsed()
    );
    // The server really stopped waiting, and the driver keeps working.
    let r = run(&*s, "SELECT COUNT(*) FROM sys.dm_exec_requests r CROSS APPLY sys.dm_exec_sql_text(r.sql_handle) t WHERE r.wait_type = 'WAITFOR' AND t.text LIKE '%00:01:07%' AND r.session_id <> @@SPID").await;
    assert_eq!(rows_of(&r[0]).rows[0][0], Value::Int(0));
    run(&*s, "SELECT 1").await;
    // Cancelling an unknown or finished id is fine.
    s.cancel(uuid::Uuid::new_v4()).await.unwrap();
    s.cancel(id).await.unwrap();
}

#[tokio::test]
async fn timeout_uses_the_cancel_path() {
    it!();
    let s = fixture().await;
    let mut req = QueryRequest::new("WAITFOR DELAY '00:01:00'");
    req.timeout_ms = Some(600);
    let started = Instant::now();
    let r = execute_collect(&*s, req).await;
    assert!(matches!(r, Err(Error::Timeout)), "{r:?}");
    assert!(started.elapsed() < Duration::from_secs(3));
    run(&*s, "SELECT 1").await;
}

#[tokio::test]
async fn max_rows_truncates_and_the_connection_survives() {
    it!();
    let s = fixture().await;
    let mut req = QueryRequest::new("SELECT id FROM dbo.paging ORDER BY id");
    req.max_rows = Some(10);
    req.batch_size = Some(4);
    let r = execute_collect(&*s, req).await.unwrap();
    let rs = rows_of(&r[0]);
    assert_eq!(rs.rows.len(), 10);
    assert!(rs.truncated);
    let mut req = QueryRequest::new("SELECT id FROM dbo.paging WHERE id <= 10");
    req.max_rows = Some(10);
    let r = execute_collect(&*s, req).await.unwrap();
    assert!(
        !rows_of(&r[0]).truncated,
        "exactly max_rows rows is not truncated"
    );
    run(&*s, "SELECT 1").await;
}

struct StopAfterFirst(u32);

impl ResultSink for StopAfterFirst {
    fn on_columns(&mut self, _: Vec<ColumnMeta>) -> dbstudio_core::Result<()> {
        Ok(())
    }
    fn on_rows(&mut self, _: Vec<Vec<Value>>) -> dbstudio_core::Result<()> {
        self.0 += 1;
        Err(Error::Cancelled)
    }
    fn on_message(&mut self, _: String) -> dbstudio_core::Result<()> {
        Ok(())
    }
    fn on_statement_end(&mut self, _: StatementEnd) -> dbstudio_core::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn a_stopping_sink_stops_the_server_work() {
    it!();
    let s = fixture().await;
    let mut sink = StopAfterFirst(0);
    let mut req = QueryRequest::new(
        "SELECT a.object_id FROM sys.all_objects a CROSS JOIN sys.all_objects b CROSS JOIN sys.all_objects c",
    );
    req.batch_size = Some(100);
    let started = Instant::now();
    let e = s.execute(req, &mut sink).await.unwrap_err();
    assert!(matches!(e, Error::Cancelled));
    assert_eq!(sink.0, 1);
    assert!(started.elapsed() < Duration::from_secs(10));
    run(&*s, "SELECT 1").await;
}

#[tokio::test]
async fn session_state_does_not_leak_between_calls() {
    it!();
    let s = fixture().await;
    run(&*s, "USE master").await;
    let r = run(&*s, "SELECT DB_NAME()").await;
    assert_eq!(text(&rows_of(&r[0]).rows[0][0]), IT_DB);
    run(&*s, "SET NOCOUNT ON; CREATE TABLE #leak (a INT)").await;
    let r = run(&*s, "SELECT OBJECT_ID('tempdb..#leak')").await;
    assert_eq!(rows_of(&r[0]).rows[0][0], Value::Null);
    // `req.database` overrides the default database.
    let mut req = QueryRequest::new("SELECT DB_NAME()");
    req.database = Some("master".into());
    let r = execute_collect(&*s, req).await.unwrap();
    assert_eq!(text(&rows_of(&r[0]).rows[0][0]), "master");
}

#[tokio::test]
async fn concurrent_queries_share_the_pool() {
    it!();
    let s = fixture().await;
    let mut tasks = Vec::new();
    for i in 0..8 {
        let s = s.clone();
        tasks.push(tokio::spawn(async move {
            let r = execute_collect(
                &*s,
                QueryRequest::new(format!(
                    "WAITFOR DELAY '00:00:01'; SELECT {i}, CAST(@@SPID AS INT)"
                )),
            )
            .await
            .unwrap();
            rows_of(r.last().unwrap()).rows[0].clone()
        }));
    }
    let mut got = Vec::new();
    let mut spids = std::collections::BTreeSet::new();
    for t in tasks {
        let row = t.await.unwrap();
        got.push(row[0].clone());
        spids.insert(format!("{:?}", row[1]));
    }
    got.sort_by_key(|v| if let Value::Int(i) = v { *i } else { -1 });
    assert_eq!(got, (0..8).map(Value::Int).collect::<Vec<_>>());
    // Pool of 4 (`DEFAULT_POOL_MAX`): the 8 overlapping queries run on at most 4 server sessions;
    // an unlimited pool would open 8. Counting sessions instead of timing two waves of WAITFOR
    // keeps this stable on busy CI runners, where WAITFOR may return early.
    assert!(spids.len() <= 4, "{} sessions: {spids:?}", spids.len());
    let _ = Arc::strong_count(&s);
}

#[tokio::test]
async fn login_and_tls_modes() {
    it!();
    // Wrong password.
    let mut cfg = config(Some("master"), TlsMode::Require);
    let e = dbstudio_driver_mssql::MssqlDriver::new()
        .connect(&cfg, Some(&dbstudio_core::Secret::new("wrong")))
        .await
        .err()
        .unwrap();
    assert_eq!(e.kind(), "authentication", "{e}");
    assert!(!e.to_string().contains("wrong"));
    // Nothing listening.
    cfg.port = 1;
    cfg.connect_timeout_ms = Some(3000);
    let e = connect_cfg(&cfg).await.err().unwrap();
    assert!(matches!(e.kind(), "connection" | "timeout"), "{e}");
    // Unknown database.
    let e = connect_cfg(&config(Some("no_such_db_zz"), TlsMode::Require))
        .await
        .err()
        .unwrap();
    assert!(matches!(e.kind(), "connection" | "authentication"), "{e}");

    let encrypted = |mode: TlsMode| async move {
        let s = connect_cfg(&config(Some("master"), mode)).await.unwrap();
        let r = run(&*s, "SELECT CAST(encrypt_option AS NVARCHAR(10)) FROM sys.dm_exec_connections WHERE session_id = @@SPID").await;
        text(&rows_of(&r[0]).rows[0][0])
    };
    assert_eq!(encrypted(TlsMode::Require).await, "TRUE");
    assert_eq!(encrypted(TlsMode::Prefer).await, "TRUE");
    assert_eq!(encrypted(TlsMode::Disable).await, "FALSE");
    // Verifying modes reject the server's self-signed certificate.
    for mode in [TlsMode::VerifyCa, TlsMode::VerifyFull] {
        let e = connect_cfg(&config(Some("master"), mode))
            .await
            .err()
            .unwrap();
        assert_eq!(e.kind(), "tls", "{mode:?}: {e}");
        // A CA file that is not the server's CA does not help either.
        let mut cfg = config(Some("master"), mode);
        cfg.tls.ca_cert_path = Some(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../../docker/certs/ca.crt")
                .to_string_lossy()
                .into_owned(),
        );
        let e = connect_cfg(&cfg).await.err().unwrap();
        assert_eq!(e.kind(), "tls", "{mode:?} with CA: {e}");
        let mut cfg = config(Some("master"), mode);
        cfg.tls.ca_cert_path = Some("/definitely/missing.pem".into());
        assert_eq!(connect_cfg(&cfg).await.err().unwrap().kind(), "tls");
    }
}

#[tokio::test]
async fn collecting_sink_and_streaming_agree() {
    it!();
    let s = fixture().await;
    let mut sink = CollectingSink::new();
    let mut req = QueryRequest::new("SELECT id FROM dbo.paging");
    req.batch_size = Some(300);
    let summary = s.execute(req, &mut sink).await.unwrap();
    assert_eq!(summary.statements, 1);
    let res = sink.into_results();
    assert_eq!(rows_of(&res[0]).rows.len(), 2500);
    assert!(matches!(res[0].outcome, StatementOutcome::Rows(_)));
    let _ = EndOutcome::Done;
}
