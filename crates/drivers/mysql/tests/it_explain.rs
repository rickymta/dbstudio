//! EXPLAIN / EXPLAIN ANALYZE integration tests. Run with `DBSTUDIO_IT=1`.

mod common;

use common::*;
use dbstudio_core::{Error, ExplainOptions, ExplainRequest, PlanFormat, Session, Value};
use dbstudio_plan::{PlanRules, parse_and_analyze};
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

/// Scratch database with table `t` (50 rows) and a session positioned in it.
async fn setup(t: Target, name: &str) -> (std::sync::Arc<dyn Session>, ScratchDb) {
    ensure_fixture(t).await;
    let db = scratch_db(t, name).await;
    let s = user_session(t).await;
    run(
        &*s,
        &format!(
            "CREATE TABLE `{db}`.t (id INT PRIMARY KEY, v INT NOT NULL) ENGINE=InnoDB; \
             INSERT INTO `{db}`.t SELECT seq, seq FROM (SELECT 1 AS seq UNION SELECT 2 UNION SELECT 3 \
             UNION SELECT 4 UNION SELECT 5) x"
        ),
    )
    .await;
    (s, db)
}

async fn snapshot(s: &dyn Session, db: &str) -> Vec<Vec<Value>> {
    query(s, &format!("SELECT id, v FROM `{db}`.t ORDER BY id"))
        .await
        .rows
}

fn is_unsupported(e: &Error, what: &str) -> bool {
    matches!(e, Error::Unsupported(m) if m == what)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plain_explain_parses() {
    if !it_enabled() {
        return;
    }
    for t in targets() {
        let (s, db) = setup(t, "xpl_plain").await;
        let mut r = req(&format!("SELECT * FROM `{db}`.t WHERE id = 3;"), false);
        let raw = s
            .explain(r.clone())
            .await
            .unwrap_or_else(|e| panic!("[{}] {e}", t.name));
        let expected = if t.mariadb {
            PlanFormat::MariaDbJson
        } else if t.tidb {
            PlanFormat::TiDbRows
        } else {
            PlanFormat::MySqlJson
        };
        assert_eq!(raw.format, expected, "[{}]", t.name);
        assert!(!raw.analyzed && !raw.rolled_back);
        assert!(!raw.server_version.is_empty());
        let tree = parse_and_analyze(&raw, &PlanRules::default())
            .unwrap_or_else(|e| panic!("[{}] {e}\n{}", t.name, raw.text));
        assert!(!tree.analyzed);
        assert!(tree.node_count >= 1);
        // Plain EXPLAIN of a write never runs it.
        r.text = format!("UPDATE `{db}`.t SET v = 0");
        let before = snapshot(&*s, &db).await;
        let raw = s
            .explain(r)
            .await
            .unwrap_or_else(|e| panic!("[{}] {e}", t.name));
        parse_and_analyze(&raw, &PlanRules::default()).unwrap();
        assert_eq!(snapshot(&*s, &db).await, before);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn analyze_select_works_where_the_server_supports_it() {
    if !it_enabled() {
        return;
    }
    for t in targets() {
        let (s, db) = setup(t, "xpl_sel").await;
        let r = req(&format!("SELECT * FROM `{db}`.t WHERE id < 4"), true);
        match s.explain(r).await {
            Ok(raw) => {
                assert!(raw.analyzed && raw.rolled_back, "[{}]", t.name);
                let expected = if t.mariadb {
                    PlanFormat::MariaDbJson
                } else if t.tidb {
                    PlanFormat::TiDbRows
                } else {
                    PlanFormat::MySqlTree
                };
                assert_eq!(raw.format, expected);
                let tree = parse_and_analyze(&raw, &PlanRules::default())
                    .unwrap_or_else(|e| panic!("[{}] {e}\n{}", t.name, raw.text));
                assert!(tree.analyzed, "[{}]\n{}", t.name, raw.text);
            }
            // MySQL before 8.0.18 has no EXPLAIN ANALYZE.
            Err(e) => assert!(
                !t.mariadb && t.version.starts_with("5.") && is_unsupported(&e, "explain_analyze"),
                "[{}] {e:?}",
                t.name
            ),
        }
        // The connection is usable (no transaction left open).
        assert_eq!(snapshot(&*s, &db).await.len(), 5);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn analyze_writes_are_unsupported_and_change_nothing() {
    if !it_enabled() {
        return;
    }
    for t in targets() {
        let (s, db) = setup(t, "xpl_write").await;
        let before = snapshot(&*s, &db).await;
        for sql in [
            format!("UPDATE `{db}`.t SET v = v + 100"),
            format!("DELETE FROM `{db}`.t"),
            format!("INSERT INTO `{db}`.t SELECT id + 100, v FROM `{db}`.t"),
            format!("WITH c AS (SELECT 1) UPDATE `{db}`.t SET v = 0"),
        ] {
            let e = s.explain(req(&sql, true)).await.unwrap_err();
            assert!(
                is_unsupported(&e, "explain_analyze_writes"),
                "[{}] {sql}: {e:?}",
                t.name
            );
        }
        let e = s
            .explain(req(&format!("CREATE TABLE `{db}`.x (a INT)"), true))
            .await
            .unwrap_err();
        assert!(
            is_unsupported(&e, "explain_analyze_ddl"),
            "[{}] {e:?}",
            t.name
        );
        assert_eq!(snapshot(&*s, &db).await, before, "[{}]", t.name);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn database_override_is_restored_and_errors_are_query_errors() {
    if !it_enabled() {
        return;
    }
    for t in targets() {
        let (s, db) = setup(t, "xpl_db").await;
        // The table only exists in the scratch database.
        let mut r = req("SELECT * FROM t WHERE id = 1", false);
        r.database = Some(db.clone());
        let raw = s
            .explain(r)
            .await
            .unwrap_or_else(|e| panic!("[{}] {e}", t.name));
        parse_and_analyze(&raw, &PlanRules::default()).unwrap();
        // Without the override the same text must not resolve in the session's database.
        let e = s
            .explain(req("SELECT * FROM t WHERE id = 1", false))
            .await
            .unwrap_err();
        assert!(matches!(e, Error::Query { .. }), "[{}] {e:?}", t.name);
        let e = s.explain(req("SELEKT 1", false)).await.unwrap_err();
        assert!(matches!(e, Error::Query { .. }), "[{}] {e:?}", t.name);
        assert!(matches!(
            s.explain(req(" ; ", false)).await.unwrap_err(),
            Error::Query { .. }
        ));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn analyze_timeout_kills_the_statement() {
    if !it_enabled() {
        return;
    }
    for t in targets() {
        // EXPLAIN ANALYZE needs 8.0.18+ on MySQL; MariaDB can analyse any version here.
        if !t.mariadb && t.version.starts_with("5.") {
            continue;
        }
        let (s, _db) = setup(t, "xpl_to").await;
        let mut r = req("SELECT SLEEP(10)", true);
        r.timeout_ms = Some(400);
        let started = std::time::Instant::now();
        let e = s.explain(r).await.unwrap_err();
        assert!(matches!(e, Error::Timeout), "[{}] {e:?}", t.name);
        assert!(started.elapsed() < slack(t, std::time::Duration::from_secs(8)));
        // The pool still works.
        assert_eq!(
            query(&*s, "SELECT 1").await.rows,
            vec![vec![Value::Int(1)]],
            "[{}]",
            t.name
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn analyze_cancel_by_query_id() {
    if !it_enabled() {
        return;
    }
    for t in targets() {
        if !t.mariadb && t.version.starts_with("5.") {
            continue;
        }
        let (s, _db) = setup(t, "xpl_cancel").await;
        let r = req("SELECT SLEEP(30)", true);
        let id = r.query_id;
        let s2 = s.clone();
        let started = std::time::Instant::now();
        let task = tokio::spawn(async move { s2.explain(r).await });
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        s.cancel(id).await.unwrap();
        let e = task.await.unwrap().unwrap_err();
        assert!(matches!(e, Error::Cancelled), "[{}] {e:?}", t.name);
        assert!(
            started.elapsed() < slack(t, std::time::Duration::from_secs(2)),
            "[{}] {:?}",
            t.name,
            started.elapsed()
        );
        assert_eq!(query(&*s, "SELECT 1").await.rows, vec![vec![Value::Int(1)]]);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn smuggled_statements_are_refused() {
    if !it_enabled() {
        return;
    }
    for t in targets() {
        let (s, db) = setup(t, "xpl_multi").await;
        let before = snapshot(&*s, &db).await;
        for (sql, analyze) in [
            (format!("SELECT 1; DROP TABLE `{db}`.t"), false),
            (format!("SELECT 1; DROP TABLE `{db}`.t"), true),
            (
                format!("select 1; commit; /*!update `{db}`.t set v=2*/"),
                false,
            ),
            (format!("select /*!50000 1; */ from `{db}`.t"), false),
            (
                format!("select 1 from `{db}`.t;\ndelete from `{db}`.t"),
                false,
            ),
            (format!("update `{db}`.t set v = 2; commit"), true),
        ] {
            let e = s.explain(req(&sql, analyze)).await.unwrap_err();
            assert!(
                matches!(e, Error::Query { .. }),
                "[{}] {sql}: {e:?}",
                t.name
            );
            assert_eq!(snapshot(&*s, &db).await, before, "[{}] {sql}", t.name);
        }
        // The table is still there and nothing was committed behind our back.
        assert_eq!(
            query(&*s, &format!("SELECT COUNT(*) FROM `{db}`.t"))
                .await
                .rows,
            vec![vec![Value::Int(5)]],
            "[{}]",
            t.name
        );
        // A semicolon inside a string literal is just text.
        let raw = s
            .explain(req(&format!("SELECT ';' AS x FROM `{db}`.t"), false))
            .await
            .unwrap_or_else(|e| panic!("[{}] {e:?}", t.name));
        parse_and_analyze(&raw, &PlanRules::default()).unwrap();
        // Lock functions are not pure reads.
        let e = s
            .explain(req("SELECT GET_LOCK('xpl_lock', 0)", true))
            .await
            .unwrap_err();
        assert!(
            is_unsupported(&e, "explain_analyze_writes") || is_unsupported(&e, "explain_analyze"),
            "[{}] {e:?}",
            t.name
        );
    }
}
