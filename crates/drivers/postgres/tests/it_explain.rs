//! EXPLAIN / EXPLAIN ANALYZE integration tests. Only run with `DBSTUDIO_IT=1`.

mod common;

use common::*;
use dbstudio_core::{Error, ExplainOptions, ExplainRequest, PlanFormat, RawPlan, Session, Value};
use dbstudio_plan::{PlanNodeKind, PlanRules, parse_and_analyze};
use uuid::Uuid;

fn req(text: &str, analyze: bool) -> ExplainRequest {
    ExplainRequest {
        query_id: Uuid::new_v4(),
        text: text.into(),
        database: None,
        analyze,
        options: ExplainOptions::default(),
        timeout_ms: None,
    }
}

async fn make(s: &dyn Session, schema: &str) {
    exec(
        s,
        &format!(
            "CREATE SCHEMA IF NOT EXISTS {schema}; \
             DROP TABLE IF EXISTS {schema}.t; \
             CREATE TABLE {schema}.t (id int PRIMARY KEY, v int NOT NULL); \
             INSERT INTO {schema}.t SELECT g, g FROM generate_series(1, 50) g"
        ),
    )
    .await;
}

async fn snapshot(s: &dyn Session, schema: &str) -> Vec<Vec<Value>> {
    rows(s, &format!("SELECT id, v FROM {schema}.t ORDER BY id")).await
}

async fn plain_explain_parses(port: u16) {
    let s = connect(port).await;
    make(&*s, "it_explain_plain").await;
    let raw = s
        .explain(req("SELECT * FROM it_explain_plain.t WHERE id = 7;", false))
        .await
        .expect("explain");
    assert_eq!(raw.format, PlanFormat::PostgresJson);
    assert!(!raw.analyzed && !raw.rolled_back);
    assert_eq!(
        raw.statement,
        "SELECT * FROM it_explain_plain.t WHERE id = 7"
    );
    assert!(!raw.server_version.is_empty());
    assert!(raw.captured_at.ends_with('Z'));
    let tree = parse_and_analyze(&raw, &PlanRules::default()).expect("parse");
    assert!(!tree.analyzed);
    assert!(tree.root.est_cost.is_some());
    // Options that do not apply to a plain EXPLAIN are accepted.
    let mut r = req("SELECT 1", false);
    r.options = ExplainOptions {
        buffers: true,
        verbose: true,
        settings: true,
    };
    let raw = s.explain(r).await.expect("explain with options");
    assert!(
        raw.text.contains("\"Output\""),
        "VERBOSE shows the output list"
    );
}

async fn analyze_dml_is_rolled_back(port: u16) {
    let s = connect(port).await;
    make(&*s, "it_explain_dml").await;
    let before = snapshot(&*s, "it_explain_dml").await;
    for sql in [
        "UPDATE it_explain_dml.t SET v = v + 100 WHERE id <= 10",
        "DELETE FROM it_explain_dml.t WHERE id > 40",
        "INSERT INTO it_explain_dml.t VALUES (1000, 1)",
    ] {
        let raw: RawPlan = s.explain(req(sql, true)).await.expect(sql);
        assert!(raw.analyzed && raw.rolled_back, "{sql}");
        let tree = parse_and_analyze(&raw, &PlanRules::default()).expect("parse");
        assert!(tree.analyzed, "{sql}");
        assert_eq!(tree.root.kind, PlanNodeKind::Modify, "{sql}");
        assert!(tree.totals.execution_time_ms.is_some());
        assert_eq!(
            snapshot(&*s, "it_explain_dml").await,
            before,
            "{sql}: data changed"
        );
    }
    // ANALYZE of a SELECT also runs in a rolled back transaction and returns actual rows.
    let raw = s
        .explain(req("SELECT * FROM it_explain_dml.t WHERE id <= 5", true))
        .await
        .unwrap();
    assert!(raw.rolled_back);
    let tree = parse_and_analyze(&raw, &PlanRules::default()).unwrap();
    assert_eq!(tree.totals.rows, Some(5.0));
    // The pooled connection is clean afterwards (no open transaction left behind).
    assert_eq!(
        scalar(&*s, "SELECT count(*) FROM it_explain_dml.t").await,
        Value::Int(50)
    );
    assert_eq!(
        scalar(&*s, "SELECT txid_current_if_assigned() IS NULL").await,
        Value::Bool(true)
    );
}

async fn analyze_errors(port: u16) {
    let s = connect(port).await;
    make(&*s, "it_explain_err").await;
    // DDL cannot be analysed.
    let e = s
        .explain(req("CREATE INDEX i ON it_explain_err.t (v)", true))
        .await
        .unwrap_err();
    assert!(
        matches!(&e, Error::Unsupported(m) if m == "explain_analyze_ddl"),
        "{e:?}"
    );
    // A runtime failure of the analysed statement leaves the connection usable.
    let e = s
        .explain(req("UPDATE it_explain_err.t SET v = NULL", true))
        .await
        .unwrap_err();
    assert!(matches!(e, Error::Query { .. }), "{e:?}");
    assert_eq!(
        scalar(&*s, "SELECT count(*) FROM it_explain_err.t").await,
        Value::Int(50)
    );
    // Syntax errors report a position inside the user's statement.
    let e = s.explain(req("SELEKT 1", false)).await.unwrap_err();
    match e {
        Error::Query { position, .. } => assert!(position.is_none_or(|p| p < 8), "{position:?}"),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        s.explain(req("  ;  ", false)).await.unwrap_err(),
        Error::Query { .. }
    ));
}

async fn analyze_timeout(port: u16) {
    let s = connect(port).await;
    let mut r = req("SELECT pg_sleep(10)", true);
    r.timeout_ms = Some(300);
    let started = std::time::Instant::now();
    let e = s.explain(r).await.unwrap_err();
    assert!(matches!(e, Error::Timeout | Error::Query { .. }), "{e:?}");
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert_eq!(scalar(&*s, "SELECT 1").await, Value::Int(1));
}

async fn other_database(port: u16) {
    let s = connect(port).await;
    let mut r = req("SELECT current_database()", false);
    r.database = Some("postgres".into());
    // The plan is produced in the requested database (it only needs to connect there).
    let raw = s.explain(r).await;
    assert!(
        raw.is_ok()
            || matches!(
                raw,
                Err(Error::Permission(_) | Error::Authentication(_) | Error::Query { .. })
            )
    );
}

async fn analyze_cancel(port: u16) {
    let s: std::sync::Arc<dyn Session> = std::sync::Arc::from(connect(port).await);
    let r = req("SELECT pg_sleep(30)", true);
    let id = r.query_id;
    let s2 = s.clone();
    let started = std::time::Instant::now();
    let task = tokio::spawn(async move { s2.explain(r).await });
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    s.cancel(id).await.unwrap();
    let e = task.await.unwrap().unwrap_err();
    assert!(matches!(e, Error::Cancelled), "{e:?}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(scalar(&*s, "SELECT 1").await, Value::Int(1));
}

macro_rules! both {
    ($f:ident, $a:ident, $b:ident) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $a() {
            if enabled() {
                $f(PG16).await
            }
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $b() {
            if enabled() {
                $f(PG12).await
            }
        }
    };
}

both!(plain_explain_parses, explain_plain_pg16, explain_plain_pg12);
both!(
    analyze_dml_is_rolled_back,
    analyze_rollback_pg16,
    analyze_rollback_pg12
);
both!(analyze_errors, analyze_errors_pg16, analyze_errors_pg12);
both!(analyze_timeout, analyze_timeout_pg16, analyze_timeout_pg12);
both!(other_database, other_database_pg16, other_database_pg12);
both!(analyze_cancel, analyze_cancel_pg16, analyze_cancel_pg12);
