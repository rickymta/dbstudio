//! Execution: streaming, `max_rows`, errors, cancel, timeout, read-only, leases, JSON fallback.

mod common;

use std::time::{Duration, Instant};

use common::*;
use dbstudio_core::{
    CollectingSink, EndOutcome, Error, LeaseContext, LeaseOptions, QueryRequest, ResultSink,
    StatementEnd, StatementOutcome, TlsMode, TxOptions, Value, execute_collect,
};
use uuid::Uuid;

fn text(s: &str) -> Value {
    Value::Text(s.into())
}

#[tokio::test]
async fn server_info_ping_and_dialect() {
    let Some(s) = session().await else { return };
    s.ping().await.unwrap();
    let info = s.server_info().await.unwrap();
    assert_eq!(info.product, "ClickHouse");
    assert!(info.version.starts_with("26."), "{}", info.version);
    assert_eq!(info.extra["protocol"], "http");
    assert_eq!(s.script_dialect(), dbstudio_core::ScriptDialect::ClickHouse);
}

#[tokio::test]
async fn bad_credentials_and_unknown_database() {
    if !it_enabled() || session().await.is_none() {
        return;
    }
    let err = ClickHouseTestDriver::connect_with_password("wrong")
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Authentication(_)), "{err:?}");
    let mut cfg = config(http_port());
    cfg.database = Some("no_such_db_for_it".into());
    let err = connect_err(&cfg).await;
    assert!(matches!(err, Error::Query { .. }), "{err:?}");
    // Nobody listens: a connection error, quickly.
    let mut cfg = config(1);
    cfg.connect_timeout_ms = Some(2000);
    assert!(matches!(
        connect_err(&cfg).await,
        Error::Connection(_) | Error::Timeout
    ));
}

struct ClickHouseTestDriver;

impl ClickHouseTestDriver {
    async fn connect_with_password(pw: &str) -> Result<(), Error> {
        use dbstudio_core::{Driver, Secret};
        dbstudio_driver_clickhouse::ClickHouseDriver::new()
            .connect(&config(http_port()), Some(&Secret::new(pw)))
            .await
            .map(|_| ())
    }
}

#[tokio::test]
async fn statements_results_and_errors() {
    let Some(s) = session().await else { return };
    // A statement without result set, DML-ish SELECT, and a syntax error with a position.
    let r = run(s.as_ref(), "SELECT 1 AS a, 'x' AS b").await;
    assert_eq!(rows_of(&r[0]).rows, vec![vec![Value::Int(1), text("x")]]);
    let r = run(s.as_ref(), "SELECT number FROM numbers(0)").await;
    assert!(rows_of(&r[0]).rows.is_empty());
    assert_eq!(rows_of(&r[0]).columns.len(), 1);
    let err = execute_collect(s.as_ref(), QueryRequest::new("SELEC 1"))
        .await
        .unwrap_err();
    match err {
        Error::Query { message, .. } => assert!(message.contains("Code: 62"), "{message}"),
        other => panic!("{other:?}"),
    }
    // A user-written FORMAT clause does not break decoding.
    let r = run(s.as_ref(), "SELECT 7 FORMAT JSON").await;
    assert_eq!(rows_of(&r[0]).rows[0][0], Value::Int(7));
    // A trailing semicolon is fine.
    run(s.as_ref(), "SELECT 1;").await;
    // Unknown table.
    let err = execute_collect(
        s.as_ref(),
        QueryRequest::new("SELECT * FROM no_such_table_it"),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, Error::Query { .. }), "{err:?}");
}

#[tokio::test]
async fn failure_in_the_middle_of_the_stream_is_a_query_error() {
    let Some(s) = session().await else { return };
    let err = execute_collect(
        s.as_ref(),
        QueryRequest::new(
            "SELECT throwIf(number > 1500000) FROM numbers(3000000) SETTINGS max_block_size = 65536",
        ),
    )
    .await
    .unwrap_err();
    match err {
        Error::Query { message, .. } => assert!(message.contains("Code: 395"), "{message}"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn max_rows_truncates_and_batches() {
    let Some(s) = session().await else { return };
    let mut req = QueryRequest::new("SELECT number FROM numbers(10000)");
    req.max_rows = Some(2500);
    req.batch_size = Some(1000);
    #[derive(Default)]
    struct Batches(Vec<usize>, Option<EndOutcome>);
    impl ResultSink for Batches {
        fn on_columns(&mut self, _: Vec<dbstudio_core::ColumnMeta>) -> dbstudio_core::Result<()> {
            Ok(())
        }
        fn on_rows(&mut self, rows: Vec<Vec<Value>>) -> dbstudio_core::Result<()> {
            self.0.push(rows.len());
            Ok(())
        }
        fn on_message(&mut self, _: String) -> dbstudio_core::Result<()> {
            Ok(())
        }
        fn on_statement_end(&mut self, end: StatementEnd) -> dbstudio_core::Result<()> {
            self.1 = Some(end.outcome);
            Ok(())
        }
    }
    let mut b = Batches::default();
    s.execute(req, &mut b).await.unwrap();
    assert_eq!(b.0, vec![1000, 1000, 500]);
    assert_eq!(
        b.1,
        Some(EndOutcome::Rows {
            row_count: 2500,
            truncated: true
        })
    );
    // Exactly max_rows rows: not truncated.
    let mut req = QueryRequest::new("SELECT number FROM numbers(10)");
    req.max_rows = Some(10);
    let results = execute_collect(s.as_ref(), req).await.unwrap();
    match &results[0].outcome {
        StatementOutcome::Rows(rs) => assert!(!rs.truncated && rs.rows.len() == 10),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn huge_query_is_cancelled_within_two_seconds() {
    let Some(s) = session().await else { return };
    for sql in [
        // Streams forever.
        "SELECT number, sipHash64(number) FROM numbers(1000000000000)",
        // Computes forever without streaming a row.
        "SELECT count() FROM numbers(1000000000000) WHERE sipHash64(number) % 7 = 3",
    ] {
        let req = QueryRequest::new(sql);
        let id = req.query_id;
        let started = Instant::now();
        let session = s.clone();
        let canceller = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            session.cancel(id).await
        });
        let err = execute_collect(s.as_ref(), req).await.unwrap_err();
        assert!(matches!(err, Error::Cancelled), "{err:?}");
        assert!(
            started.elapsed() < Duration::from_millis(2500),
            "cancel took {:?}",
            started.elapsed()
        );
        canceller.await.unwrap().unwrap();
        // The server really stopped the query.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let n = scalar(
            s.as_ref(),
            &format!("SELECT count() FROM system.processes WHERE query_id = '{id}'"),
        )
        .await;
        assert_eq!(n, Value::UInt(0), "query still running on the server");
    }
    // Cancelling something that does not run.
    assert!(matches!(
        s.cancel(Uuid::new_v4()).await.unwrap_err(),
        Error::NotFound(_)
    ));
}

#[tokio::test]
async fn timeout_stops_a_slow_query() {
    let Some(s) = session().await else { return };
    let mut req = QueryRequest::new(
        "SELECT count() FROM numbers(1000000000000) WHERE sipHash64(number) % 7 = 3",
    );
    req.timeout_ms = Some(700);
    let started = Instant::now();
    let err = execute_collect(s.as_ref(), req).await.unwrap_err();
    assert!(matches!(err, Error::Timeout), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn read_only_connection_is_enforced_by_the_server() {
    let Some(s) = session().await else { return };
    let db = TempDb::create(s.as_ref(), http_port(), "ro").await;
    run(
        s.as_ref(),
        &format!(
            "CREATE TABLE {} (id UInt8) ENGINE = MergeTree ORDER BY id",
            db.t("t")
        ),
    )
    .await;
    let mut cfg = config(http_port());
    cfg.read_only = true;
    let ro = connect_cfg(&cfg).await.unwrap();
    let r = run(ro.as_ref(), &format!("SELECT count() FROM {}", db.t("t"))).await;
    assert_eq!(rows_of(&r[0]).rows[0][0], Value::UInt(0));
    for sql in [
        format!("INSERT INTO {} VALUES (1)", db.t("t")),
        format!("DROP TABLE {}", db.t("t")),
        format!("CREATE TABLE {} (x UInt8) ENGINE = Memory", db.t("u")),
    ] {
        let err = execute_collect(ro.as_ref(), QueryRequest::new(sql.clone()))
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::ReadOnlyViolation { .. }),
            "{sql}: {err:?}"
        );
    }
    // Nothing was written, and the capabilities say the same.
    assert_eq!(
        scalar(s.as_ref(), &format!("SELECT count() FROM {}", db.t("t"))).await,
        Value::UInt(0)
    );
    let ed = ro
        .editability(
            &dbstudio_core::ObjectPath::root()
                .child(dbstudio_core::ObjectKind::Database, db.name.clone())
                .child(dbstudio_core::ObjectKind::Table, "t"),
        )
        .await
        .unwrap();
    assert!(!ed.editable);
    assert_eq!(
        ed.reason,
        Some(dbstudio_core::NotEditableReason::ReadOnlyConnection)
    );
}

#[tokio::test]
async fn read_only_user_profile_works_with_read_only_flag() {
    let Some(s) = session().await else { return };
    run(
        s.as_ref(),
        "CREATE USER IF NOT EXISTS dbstudio_it_ro IDENTIFIED BY 'ro_pw' SETTINGS readonly = 1",
    )
    .await;
    run(s.as_ref(), "GRANT SELECT ON *.* TO dbstudio_it_ro").await;
    let mut cfg = config(http_port());
    cfg.username = Some("dbstudio_it_ro".into());
    cfg.read_only = true;
    let ro = dbstudio_driver_clickhouse::ClickHouseDriver::new();
    use dbstudio_core::{Driver, Secret};
    let ro = ro.connect(&cfg, Some(&Secret::new("ro_pw"))).await.unwrap();
    let r = run(ro.as_ref(), "SELECT 1").await;
    assert_eq!(rows_of(&r[0]).rows[0][0], Value::Int(1));
    let err = execute_collect(
        ro.as_ref(),
        QueryRequest::new("CREATE DATABASE dbstudio_it_ro_x"),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, Error::ReadOnlyViolation { .. } | Error::Permission(_)),
        "{err:?}"
    );
    run(s.as_ref(), "DROP USER IF EXISTS dbstudio_it_ro").await;
}

#[tokio::test]
async fn aggregate_state_columns_fall_back_to_json() {
    let Some(s) = session().await else { return };
    let r = run(
        s.as_ref(),
        "SELECT number AS n, sumState(number) AS st, toDecimal64(number, 2) AS d, [number, number + 1] AS a, \
         toUInt64(18446744073709551615) AS big FROM numbers(3) GROUP BY number ORDER BY number",
    )
    .await;
    let rs = rows_of(&r[0]);
    assert_eq!(rs.rows.len(), 3);
    assert_eq!(rs.columns[1].type_name, "AggregateFunction(sum, UInt64)");
    assert_eq!(rs.rows[2][0], Value::UInt(2));
    assert!(matches!(rs.rows[2][1], Value::Text(_)));
    assert_eq!(rs.rows[2][2], Value::Decimal("2.00".into()));
    assert_eq!(
        rs.rows[2][3],
        Value::Array(vec![Value::UInt(2), Value::UInt(3)])
    );
    assert_eq!(rs.rows[2][4], Value::UInt(u64::MAX));
}

#[tokio::test]
async fn multiple_statements_depend_on_the_splitter() {
    let Some(s) = session().await else { return };
    let r = execute_collect(s.as_ref(), QueryRequest::new("SELECT 1; SELECT 2")).await;
    match r {
        // dbstudio-sql knows ClickHouse: two statements, two results.
        Ok(results) => assert_eq!(results.len(), 2),
        // Until then the whole text is one statement and the server refuses it.
        Err(Error::Query { message, .. }) => {
            eprintln!("note: splitter is not ClickHouse aware yet: {message}");
        }
        Err(other) => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn lease_keeps_session_state_and_database() {
    let Some(s) = session().await else { return };
    let db = TempDb::create(s.as_ref(), http_port(), "lease").await;
    run(
        s.as_ref(),
        &format!("CREATE TABLE {} (id UInt8) ENGINE = Memory", db.t("t")),
    )
    .await;
    let lease = s.open_lease(LeaseOptions::default()).await.unwrap();
    let go = |sql: String| {
        let lease = &lease;
        async move {
            let mut sink = CollectingSink::new();
            lease
                .execute(QueryRequest::new(sql), &mut sink)
                .await
                .map(|_| sink.into_results())
        }
    };
    // SET and temporary tables live in the HTTP session.
    go("SET max_threads = 3".into()).await.unwrap();
    let r = go("SELECT value FROM system.settings WHERE name = 'max_threads'".into())
        .await
        .unwrap();
    assert_eq!(rows_of_result(&r[0])[0][0], text("3"));
    go("CREATE TEMPORARY TABLE tmp_it (x UInt8)".into())
        .await
        .unwrap();
    go("INSERT INTO tmp_it VALUES (5), (6)".into())
        .await
        .unwrap();
    let r = go("SELECT sum(x) FROM tmp_it".into()).await.unwrap();
    assert_eq!(rows_of_result(&r[0])[0][0], Value::UInt(11));
    // The plain session does not see the temporary table.
    assert!(
        execute_collect(s.as_ref(), QueryRequest::new("SELECT * FROM tmp_it"))
            .await
            .is_err()
    );
    // USE switches the database for the following statements.
    go(format!("USE `{}`", db.name)).await.unwrap();
    assert_eq!(lease.state().database.as_deref(), Some(db.name.as_str()));
    go("INSERT INTO t VALUES (1)".into()).await.unwrap();
    let r = go("SELECT count() FROM t".into()).await.unwrap();
    assert_eq!(rows_of_result(&r[0])[0][0], Value::UInt(1));
    // set_context validates the database.
    lease
        .set_context(LeaseContext {
            database: Some("system".into()),
            schema: None,
        })
        .await
        .unwrap();
    assert_eq!(lease.state().database.as_deref(), Some("system"));
    assert!(
        lease
            .set_context(LeaseContext {
                database: Some("no_such_db_it".into()),
                schema: None,
            })
            .await
            .is_err()
    );
    // No transactions.
    assert!(matches!(
        lease.begin(TxOptions::default()).await,
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        lease.set_auto_commit(false).await,
        Err(Error::Unsupported(_))
    ));
    assert!(
        s.open_lease(LeaseOptions {
            auto_commit: false,
            ..Default::default()
        })
        .await
        .is_err()
    );
    lease.ping().await.unwrap();
    lease.close().await.unwrap();
}

fn rows_of_result(r: &dbstudio_core::StatementResult) -> &Vec<Vec<Value>> {
    &rows_of(r).rows
}

#[tokio::test]
async fn lease_pending_result_is_fetched_in_pages() {
    let Some(s) = session().await else { return };
    let lease = s.open_lease(LeaseOptions::default()).await.unwrap();
    let mut req = QueryRequest::new("SELECT number FROM numbers(2500)");
    req.max_rows = Some(1000);
    let mut sink = CollectingSink::new();
    lease.execute(req.clone(), &mut sink).await.unwrap();
    let first = sink.into_results();
    assert!(rows_of(&first[0]).truncated);
    assert_eq!(lease.state().pending_result, Some(req.query_id));
    let mut total = 1000;
    loop {
        let mut sink = CollectingSink::new();
        lease
            .fetch_more(req.query_id, 1000, None, &mut sink)
            .await
            .unwrap();
        let r = sink.into_results();
        let rs = rows_of(&r[0]);
        total += rs.rows.len();
        if !rs.truncated {
            break;
        }
    }
    assert_eq!(total, 2500);
    assert_eq!(lease.state().pending_result, None);
    // Abandoning a pending result frees the lease.
    let mut req = QueryRequest::new("SELECT number FROM numbers(1000000000000)");
    req.max_rows = Some(10);
    let mut sink = CollectingSink::new();
    lease.execute(req, &mut sink).await.unwrap();
    lease.discard_result().await.unwrap();
    lease.ping().await.unwrap();
    lease.close().await.unwrap();
}

#[tokio::test]
async fn lease_cancel_stops_the_running_statement() {
    let Some(s) = session().await else { return };
    let lease = std::sync::Arc::new(s.open_lease(LeaseOptions::default()).await.unwrap());
    let req = QueryRequest::new(
        "SELECT count() FROM numbers(1000000000000) WHERE sipHash64(number) % 11 = 1",
    );
    let id = req.query_id;
    let l2 = lease.clone();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        l2.cancel(id).await
    });
    let started = Instant::now();
    let mut sink = CollectingSink::new();
    let err = lease.execute(req, &mut sink).await.unwrap_err();
    assert!(matches!(err, Error::Cancelled), "{err:?}");
    assert!(started.elapsed() < Duration::from_millis(2500));
    canceller.await.unwrap().unwrap();
    // The lease is usable afterwards.
    let mut sink = CollectingSink::new();
    lease
        .execute(QueryRequest::new("SELECT 1"), &mut sink)
        .await
        .unwrap();
    lease.close().await.unwrap();
}

#[tokio::test]
async fn tls_connections_use_https_and_verify() {
    if session().await.is_none() {
        return;
    }
    let ca = certs_dir().join("ca.crt").to_string_lossy().to_string();
    let mut cfg = config(tls_port());
    cfg.tls.mode = TlsMode::Require;
    let s = connect_cfg(&cfg).await.expect("require over https");
    assert_eq!(s.server_info().await.unwrap().extra["protocol"], "https");
    // VerifyFull with the right CA and a name the certificate lists.
    let mut cfg = config(tls_port());
    cfg.host = "localhost".into();
    cfg.tls.mode = TlsMode::VerifyFull;
    cfg.tls.ca_cert_path = Some(ca.clone());
    connect_cfg(&cfg).await.expect("verify full");
    // R6: connect to 127.0.0.1 (the tunnel end) but verify the original host name.
    let mut cfg = config(tls_port());
    cfg.tls.mode = TlsMode::VerifyFull;
    cfg.tls.ca_cert_path = Some(ca.clone());
    cfg.tls.server_name = Some("clickhouse26".into());
    connect_cfg(&cfg)
        .await
        .expect("verify full with server_name override");
    // A name the certificate does not list: VerifyFull fails with a TLS error, VerifyCa passes.
    cfg.tls.server_name = Some("other.example.com".into());
    let err = connect_err(&cfg).await;
    assert!(matches!(err, Error::Tls(_)), "{err:?}");
    cfg.tls.mode = TlsMode::VerifyCa;
    connect_cfg(&cfg).await.expect("verify ca ignores the name");
    // Wrong CA.
    let mut cfg = config(tls_port());
    cfg.host = "localhost".into();
    cfg.tls.mode = TlsMode::VerifyFull;
    cfg.tls.ca_cert_path = Some(
        certs_dir()
            .join("other-ca.crt")
            .to_string_lossy()
            .to_string(),
    );
    assert!(matches!(connect_err(&cfg).await, Error::Tls(_)));
    // Plain HTTP on the TLS port is not an HTTP server.
    let mut cfg = config(tls_port());
    cfg.tls.mode = TlsMode::Disable;
    assert!(connect_cfg(&cfg).await.is_err());
}

#[tokio::test]
async fn truncated_statement_does_not_disturb_the_next_one_in_a_script() {
    let Some(s) = session().await else { return };
    let mut req = QueryRequest::new(
        "SELECT number FROM numbers(1000000000000); SELECT 2 AS two; SELECT number FROM numbers(1000000000000)",
    );
    req.max_rows = Some(5);
    let results = execute_collect(s.as_ref(), req).await.unwrap();
    assert_eq!(results.len(), 3);
    match &results[0].outcome {
        StatementOutcome::Rows(rs) => assert!(rs.truncated && rs.rows.len() == 5),
        other => panic!("{other:?}"),
    }
    assert_eq!(rows_of(&results[1]).rows[0][0], Value::Int(2));
    // The same inside a lease (the HTTP session must be free again for each statement).
    let lease = s.open_lease(LeaseOptions::default()).await.unwrap();
    let mut req = QueryRequest::new("SELECT number FROM numbers(1000000000000); SELECT 3 AS three");
    req.max_rows = Some(5);
    let mut sink = CollectingSink::new();
    lease.execute(req, &mut sink).await.unwrap();
    let r = sink.into_results();
    assert_eq!(rows_of(&r[1]).rows[0][0], Value::Int(3));
    lease.close().await.unwrap();
}

#[tokio::test]
async fn prefer_falls_back_to_http_and_locked_profile_uses_json_fallback() {
    let Some(s) = session().await else { return };
    let mut cfg = config(http_port());
    cfg.tls.mode = TlsMode::Prefer;
    let p = connect_cfg(&cfg).await.unwrap();
    assert_eq!(p.server_info().await.unwrap().extra["protocol"], "http");

    // A read-only profile cannot send settings: JSON columns and time zones still come out right.
    run(
        s.as_ref(),
        "CREATE USER IF NOT EXISTS dbstudio_it_lock IDENTIFIED BY 'lk' SETTINGS readonly = 1",
    )
    .await;
    run(s.as_ref(), "GRANT SELECT ON *.* TO dbstudio_it_lock").await;
    let mut cfg = config(http_port());
    cfg.username = Some("dbstudio_it_lock".into());
    use dbstudio_core::{Driver, Secret};
    let ro = dbstudio_driver_clickhouse::ClickHouseDriver::new()
        .connect(&cfg, Some(&Secret::new("lk")))
        .await
        .unwrap();
    let r = run(
        ro.as_ref(),
        "SELECT '{\"a\":1}'::JSON AS j, toDateTime('2024-01-01 12:00:00', 'Europe/Paris') AS p, \
         toDateTime('2024-01-01 12:00:00', 'UTC') AS u",
    )
    .await;
    let row = &rows_of(&r[0]).rows[0];
    assert!(
        matches!(&row[0], Value::Json(t) if t.contains("\"a\"")),
        "{row:?}"
    );
    // No conversion possible for the Paris column: a plain local date-time, never a wrong instant.
    assert_eq!(row[1], Value::DateTime("2024-01-01 12:00:00".into()));
    assert_eq!(row[2], Value::Timestamp("2024-01-01T12:00:00Z".into()));
    run(s.as_ref(), "DROP USER IF EXISTS dbstudio_it_lock").await;
}
