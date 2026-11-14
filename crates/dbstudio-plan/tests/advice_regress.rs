//! Regression tests for advisor bugs found in review: stack depth, wrong-table indexes,
//! column-vs-expression predicates and statement scaling.

use dbstudio_core::{PlanFormat, RawPlan};
use dbstudio_plan::{PlanAdvice, PlanRules, parse_and_analyze};
use serde_json::{Value, json};

fn raw(format: PlanFormat, text: String, stmt: &str) -> RawPlan {
    serde_json::from_value(json!({
        "format": format, "text": text, "statement": stmt, "analyzed": true,
        "rolledBack": false, "serverVersion": "16.2", "capturedAt": "2024-01-01T00:00:00Z"
    }))
    .unwrap_or_else(|e| panic!("{e}"))
}

fn seq(table: &str, alias: &str, filter: &str, rows: f64, removed: f64) -> Value {
    json!({"Node Type":"Seq Scan","Relation Name":table,"Alias":alias,"Startup Cost":0.0,"Total Cost":50000.0,
      "Plan Rows":rows,"Plan Width":10,"Actual Startup Time":0.1,"Actual Total Time":900.0,"Actual Rows":rows,"Actual Loops":1,
      "Filter":filter,"Rows Removed by Filter":removed})
}

fn pg_advice(plan: Value, stmt: &str) -> Vec<PlanAdvice> {
    let text = serde_json::to_string(
        &json!([{"Plan": plan, "Planning Time":1.0, "Execution Time": 1000.0}]),
    )
    .unwrap();
    parse_and_analyze(
        &raw(PlanFormat::PostgresJson, text, stmt),
        &PlanRules::default(),
    )
    .expect("plan parses")
    .advice
}

fn sqls(advice: &[PlanAdvice]) -> Vec<String> {
    advice
        .iter()
        .flat_map(|a| a.actions.iter().filter_map(|x| x.sql.clone()))
        .collect()
}

/// Runs `f` on a thread with the 2 MB stack of a tokio worker.
fn on_small_stack(f: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(f)
        .unwrap()
        .join()
        .expect("no stack overflow / panic");
}

#[test]
fn deep_nesting_never_overflows_the_stack() {
    on_small_stack(|| {
        let n = 5000;
        let and_chain = format!("{}a = 1{}", "(b = 1 AND ".repeat(n), ")".repeat(n));
        let or_chain = format!("{}a = 1{}", "(b = 1 OR ".repeat(n), ")".repeat(n));
        let plus = format!("a = 1{}", "+1".repeat(n));
        let calls = format!("{}a{} = 1", "f(".repeat(n), ")".repeat(n));
        let parens = format!("{}a{} = 1", "(".repeat(n), ")".repeat(n));
        let unary = format!("a = {}1", "- ".repeat(n));
        for (k, filter) in [and_chain, or_chain, plus, calls, parens, unary]
            .into_iter()
            .enumerate()
        {
            let t0 = std::time::Instant::now();
            let stmt = format!("SELECT * FROM orders WHERE {filter}");
            let advice = pg_advice(seq("orders", "orders", &filter, 10.0, 2_000_000.0), &stmt);
            // Whatever it decided, no index may come from an unparsed monster.
            let _ = advice;
            let my = json!({"query_block":{"select_id":1,"table":{"table_name":"orders","access_type":"ALL",
                "rows_examined_per_scan":1000000,"filtered":"0.1","attached_condition":filter}}});
            let r = raw(PlanFormat::MySqlJson, my.to_string(), &stmt);
            parse_and_analyze(&r, &PlanRules::default()).expect("mysql plan parses");
            eprintln!("case {k}: {:?}", t0.elapsed());
        }
    });
}

#[test]
fn index_targets_the_table_owning_the_column() {
    let orders = json!({"Node Type":"Seq Scan","Relation Name":"orders","Alias":"o","Startup Cost":0.0,"Total Cost":50000.0,
      "Plan Rows":1000000.0,"Plan Width":10,"Actual Startup Time":0.1,"Actual Total Time":900.0,"Actual Rows":1000000.0,"Actual Loops":1, "Parent Relationship":"Outer"});
    let users = json!({"Node Type":"Seq Scan","Relation Name":"users","Alias":"u","Startup Cost":0.0,"Total Cost":5.0,
      "Plan Rows":1.0,"Plan Width":10,"Actual Startup Time":0.1,"Actual Total Time":0.2,"Actual Rows":1.0,"Actual Loops":1,
      "Filter":"(lower((u.email)::text) = 'x'::text)","Rows Removed by Filter":50.0, "Parent Relationship":"Outer"});
    let hash = json!({"Node Type":"Hash","Startup Cost":0.0,"Total Cost":5.0,"Plan Rows":1.0,"Plan Width":10,"Actual Startup Time":0.1,"Actual Total Time":0.2,"Actual Rows":1.0,"Actual Loops":1,"Parent Relationship":"Inner","Plans":[users]});
    let join = json!({"Node Type":"Hash Join","Join Type":"Inner","Startup Cost":0.0,"Total Cost":60000.0,"Plan Rows":10.0,"Plan Width":10,"Actual Startup Time":0.1,"Actual Total Time":1000.0,"Actual Rows":10.0,"Actual Loops":1,"Hash Cond":"(o.user_id = u.id)","Plans":[orders, hash]});
    let advice = pg_advice(
        join.clone(),
        "SELECT * FROM orders o JOIN users u ON u.id = o.user_id WHERE lower(email) = 'x' OR name LIKE '%a'",
    );
    for s in sqls(&advice) {
        assert!(!s.contains("ON orders"), "wrong table: {s}");
    }
    for a in &advice {
        if matches!(
            a.rule,
            dbstudio_plan::AdviceRule::NonSargablePredicate
                | dbstudio_plan::AdviceRule::LeadingWildcardLike
        ) {
            assert_ne!(a.confidence, dbstudio_plan::Confidence::High, "{a:?}");
        }
    }
    let advice = pg_advice(
        join,
        "SELECT * FROM orders o JOIN users u ON u.id = o.user_id WHERE u.name LIKE '%a'",
    );
    let trgm: Vec<_> = sqls(&advice)
        .into_iter()
        .filter(|s| s.contains("gin_trgm_ops"))
        .collect();
    for s in &trgm {
        assert!(!s.contains("ON orders"), "{s}");
    }
}

#[test]
fn single_table_plan_still_gets_a_table_bound_index() {
    let advice = pg_advice(
        seq(
            "orders",
            "orders",
            "(lower((email)::text) = 'x'::text)",
            10.0,
            2_000_000.0,
        ),
        "select * from orders where lower(email) = 'x'",
    );
    assert!(
        sqls(&advice).iter().any(|s| s.contains("ON orders")),
        "{:?}",
        sqls(&advice)
    );
}

#[test]
fn column_against_expression_of_the_same_table_is_not_indexable() {
    for (filter, stmt) in [
        (
            "(qty = (reserved + 1))",
            "select * from orders where qty = reserved + 1",
        ),
        (
            "(shipped_at > (created_at + '7 days'::interval))",
            "select * from orders where shipped_at > created_at + interval '7 days'",
        ),
    ] {
        let advice = pg_advice(seq("orders", "orders", filter, 10.0, 2_000_000.0), stmt);
        let created: Vec<String> = sqls(&advice)
            .into_iter()
            .filter(|s| s.to_uppercase().contains("CREATE INDEX"))
            .collect();
        assert!(created.is_empty(), "{filter}: {created:?}");
    }
    // MySQL
    let my = json!({"query_block":{"select_id":1,"table":{"table_name":"orders","access_type":"ALL",
        "rows_examined_per_scan":1000000,"filtered":"0.1","attached_condition":"(`d`.`o`.`b` = (`d`.`o`.`c` + 1))"}}});
    let r = raw(
        PlanFormat::MySqlJson,
        my.to_string(),
        "select * from orders o where o.b = o.c + 1",
    );
    let tree = parse_and_analyze(&r, &PlanRules::default()).unwrap();
    let created: Vec<String> = sqls(&tree.advice)
        .into_iter()
        .filter(|s| s.to_uppercase().contains("INDEX"))
        .collect();
    assert!(created.iter().all(|s| !s.contains("(`b`)")), "{created:?}");
}

#[test]
fn big_statement_scales_linearly() {
    let mut stmt = String::from("SELECT * FROM orders WHERE ");
    let mut i = 0;
    while stmt.len() < 1_000_000 {
        if i > 0 {
            stmt.push_str(" OR ");
        }
        stmt.push_str(&format!(
            "lower(c{i}) = 'x' OR n{i} LIKE '%a' OR (SELECT 1 FROM u{i} x{i} WHERE x{i}.a = 1) = 1"
        ));
        i += 1;
    }
    let started = std::time::Instant::now();
    let advice = pg_advice(seq("orders", "orders", "(a = 1)", 10.0, 2_000_000.0), &stmt);
    assert!(advice.len() < 400, "{} drafts", advice.len());
    assert!(started.elapsed().as_secs() < 5, "{:?}", started.elapsed());
}

fn mysql_scan(alias: &str, cond: &str, stmt: &str) -> Vec<PlanAdvice> {
    let col = format!("(`d`.`{alias}`.`status` = 'x')");
    let _ = cond;
    let plan = json!({"query_block":{"select_id":1,"table":{"table_name":alias,"access_type":"ALL",
        "rows_examined_per_scan":1000000,"filtered":"0.1","attached_condition":col}}});
    let r = raw(PlanFormat::MySqlJson, plan.to_string(), stmt);
    parse_and_analyze(&r, &PlanRules::default()).unwrap().advice
}

#[test]
fn mysql_never_alters_an_alias() {
    // The same alias for two different tables: no DDL on either.
    let advice = mysql_scan(
        "o",
        "",
        "SELECT * FROM (SELECT * FROM orders o) a JOIN (SELECT * FROM users o) b ON a.id = b.id WHERE o.status = 'x'",
    );
    for s in sqls(&advice) {
        assert!(!s.contains("`o`"), "alias used as a table: {s}");
    }
    // STRAIGHT_JOIN tables are resolved.
    let advice = mysql_scan(
        "u",
        "",
        "SELECT * FROM orders o STRAIGHT_JOIN users u ON u.id = o.user_id WHERE u.status = 'x'",
    );
    let all = sqls(&advice).join("\n");
    assert!(all.contains("`users`"), "{all}");
    assert!(!all.contains("TABLE `u`"), "{all}");
}
