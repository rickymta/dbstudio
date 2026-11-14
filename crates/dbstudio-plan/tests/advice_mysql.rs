//! Query Advisor on the MySQL, MariaDB and TiDB plans: golden advice per fixture
//! (`advice_fixtures/<fixture>.advice.json`, rewritten with `UPDATE_FIXTURES=1`) and rule tests
//! on inline plans. The invariants of design section 11.1 run over the whole corpus in
//! `tests/advice.rs`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use dbstudio_core::{PlanFormat, RawPlan};
use dbstudio_plan::{PlanAdvice, PlanRules, PlanTree, parse_and_analyze};
use serde_json::{Value, json};

struct Case {
    name: String,
    format: PlanFormat,
    version: &'static str,
    text: String,
    statement: String,
}

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests")
}

fn corpus() -> Vec<Case> {
    let mut out = Vec::new();
    for (dir, version) in [
        ("fixtures", "16.2"),
        ("fixtures_tidb", "8.0.11-TiDB-v8.5.7"),
    ] {
        let mut names: Vec<String> = std::fs::read_dir(root().join(dir))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        for name in names {
            let format = match name.as_str() {
                n if n.starts_with("mysql_jsonv2") => PlanFormat::MySqlJsonV2,
                n if n.starts_with("mysql_json") => PlanFormat::MySqlJson,
                n if n.starts_with("mysql_tree") => PlanFormat::MySqlTree,
                n if n.starts_with("mariadb_") => PlanFormat::MariaDbJson,
                n if n.starts_with("tidb_") => PlanFormat::TiDbRows,
                _ => continue,
            };
            let stem = name.rsplit_once('.').map_or(name.as_str(), |(s, _)| s);
            let statement =
                std::fs::read_to_string(root().join("advice_fixtures").join(format!("{stem}.sql")))
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default();
            out.push(Case {
                text: std::fs::read_to_string(root().join(dir).join(&name)).unwrap(),
                statement,
                name: stem.to_string(),
                format,
                version,
            });
        }
    }
    out
}

fn raw(format: PlanFormat, version: &str, text: &str, statement: &str) -> RawPlan {
    RawPlan {
        format,
        text: text.into(),
        statement: statement.into(),
        analyzed: false,
        rolled_back: false,
        server_version: version.into(),
        captured_at: "2025-01-01T00:00:00Z".into(),
        extra: BTreeMap::new(),
    }
}

fn tree_of(c: &Case) -> PlanTree {
    parse_and_analyze(
        &raw(c.format, c.version, &c.text, &c.statement),
        &PlanRules::default(),
    )
    .unwrap_or_else(|e| panic!("{}: {e}", c.name))
}

/// What the golden file pins about one advice (same shape as `tests/advice.rs`).
fn summary(a: &PlanAdvice) -> Value {
    let actions: Vec<Value> = a
        .actions
        .iter()
        .map(|x| {
            json!({
                "kind": x.kind, "label": x.label_key, "scope": x.scope, "template": x.template,
                "lock": x.lock, "sql": x.sql,
            })
        })
        .collect();
    json!({
        "rule": a.rule,
        "severity": a.severity,
        "confidence": a.confidence,
        "nodeIds": a.node_ids,
        "table": a.table.as_ref().map(|t| match &t.schema {
            Some(s) => format!("{s}.{}", t.name),
            None => t.name.clone(),
        }),
        "params": a.params,
        "evidence": a.evidence.iter().map(|e| json!({
            "key": e.key, "nodeId": e.node_id, "value": e.value, "text": e.text,
            "unit": e.unit, "actual": e.actual,
        })).collect::<Vec<_>>(),
        "actions": actions,
        "caveats": a.caveats.iter().map(|c| &c.key).collect::<Vec<_>>(),
        "basis": a.impact.basis,
        "share": a.impact.share,
        "fromWarnings": a.from_warnings,
        "statementRange": a.statement_range,
    })
}

fn golden(name: &str, value: &Value) {
    let path = root().join("advice_fixtures").join(name);
    let mut text = serde_json::to_string_pretty(value).unwrap();
    text.push('\n');
    if dbstudio_core::testing::updating() {
        std::fs::write(&path, &text).unwrap();
        return;
    }
    let old = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "{} is missing; run `UPDATE_FIXTURES=1 cargo test -p dbstudio-plan --test advice_mysql`",
            path.display()
        )
    });
    assert_eq!(
        old, text,
        "golden {name} differs; review and run with UPDATE_FIXTURES=1"
    );
}

#[test]
fn golden_advice_of_every_mysql_mariadb_and_tidb_fixture() {
    let (mut mysql, mut mariadb, mut tidb) = (0, 0, 0);
    for c in corpus() {
        let tree = tree_of(&c);
        let value = Value::Array(tree.advice.iter().map(summary).collect());
        golden(&format!("{}.advice.json", c.name), &value);
        match c.format {
            PlanFormat::TiDbRows => tidb += 1,
            PlanFormat::MariaDbJson => mariadb += 1,
            _ => mysql += 1,
        }
    }
    assert_eq!((mysql, mariadb, tidb), (16, 7, 8));
}

// ------------------------------------------------------------------------------------------
// Rules on inline plans
// ------------------------------------------------------------------------------------------

use dbstudio_core::{ColumnDetail, IndexDetail, ObjectPath, ValueKind};
use dbstudio_plan::advice::{
    ActionKind, AdviceRule, AdvisorContext, MetadataState, TableKey, TableMeta, advise,
};

const MYSQL8: &str = "8.0.36";
const MARIADB: &str = "10.11.6-MariaDB";

fn advice(format: PlanFormat, version: &str, plan: &str, statement: &str) -> Vec<PlanAdvice> {
    parse_and_analyze(
        &raw(format, version, plan, statement),
        &PlanRules::default(),
    )
    .expect("plan parses")
    .advice
}

fn find(list: &[PlanAdvice], rule: AdviceRule) -> &PlanAdvice {
    list.iter().find(|a| a.rule == rule).unwrap_or_else(|| {
        panic!(
            "no {rule:?} in {:#?}",
            list.iter().map(|a| a.rule).collect::<Vec<_>>()
        )
    })
}

fn columns(a: &PlanAdvice) -> &str {
    a.params.get("columns").map_or("", String::as_str)
}

#[test]
fn alias_of_json_v1_becomes_the_real_table() {
    let plan = r#"{"query_block":{"select_id":1,"table":{"table_name":"o","access_type":"ALL",
        "rows_examined_per_scan":100000,"rows_produced_per_join":1000,"filtered":"1.00",
        "attached_condition":"(`shop`.`o`.`status` = 'void')"}}}"#;
    let a = advice(
        PlanFormat::MySqlJson,
        MYSQL8,
        plan,
        "SELECT * FROM shop.orders AS o WHERE o.status = 'void'",
    );
    let f = find(&a, AdviceRule::IndexForFilter);
    let t = f.table.as_ref().unwrap();
    assert_eq!(
        (t.schema.as_deref(), t.name.as_str(), t.alias.as_deref()),
        (Some("shop"), "orders", Some("o"))
    );
    assert_eq!(columns(f), "status");
}

#[test]
fn filesort_of_json_v1_takes_the_columns_from_the_statement() {
    let plan = r#"{"query_block":{"select_id":1,"ordering_operation":{"using_filesort":true,
        "table":{"table_name":"orders","access_type":"ref","key":"ix_status",
        "used_key_parts":["status"],"rows_examined_per_scan":50000,"rows_produced_per_join":50000,
        "filtered":"100.00"}}}}"#;
    let sql = "SELECT * FROM orders WHERE status = 'open' ORDER BY created DESC";
    let a = advice(PlanFormat::MySqlJson, MYSQL8, plan, sql);
    let s = find(&a, AdviceRule::IndexForSort);
    assert_eq!(columns(s), "status, created DESC");
    assert!(s.caveats.iter().all(|c| c.key != "descendingIndexIgnored"));
    // MySQL 5.7 parses DESC in an index and ignores it.
    let old = advice(PlanFormat::MySqlJson, "5.7.44", plan, sql);
    assert!(
        find(&old, AdviceRule::IndexForSort)
            .caveats
            .iter()
            .any(|c| c.key == "descendingIndexIgnored")
    );
    // A join below the sort: no single table to index.
    let join = r#"{"query_block":{"select_id":1,"ordering_operation":{"using_filesort":true,
        "nested_loop":[{"table":{"table_name":"a","access_type":"ALL","rows_examined_per_scan":50000,
        "rows_produced_per_join":50000,"filtered":"100.00"}},
        {"table":{"table_name":"b","access_type":"ALL","rows_examined_per_scan":50000,
        "rows_produced_per_join":50000,"filtered":"100.00"}}]}}}"#;
    let none = advice(
        PlanFormat::MySqlJson,
        MYSQL8,
        join,
        "SELECT * FROM a, b ORDER BY a.x",
    );
    assert!(none.iter().all(|a| a.rule != AdviceRule::IndexForSort));
}

#[test]
fn index_in_use_gets_the_filter_column_when_most_rows_are_dropped() {
    let plan = r#"{"query_block":{"select_id":1,"r_loops":1,"r_total_time_ms":50,
        "table":{"table_name":"orders","access_type":"ref","possible_keys":["ix_customer"],
        "key":"ix_customer","key_length":"5","used_key_parts":["customer_id"],"ref":["const"],
        "r_loops":1,"rows":50000,"r_rows":50000,"r_table_time_ms":40,"r_other_time_ms":5,
        "filtered":100,"r_filtered":2,"attached_condition":"orders.status = 'void'"}}}"#;
    let a = advice(
        PlanFormat::MariaDbJson,
        MARIADB,
        plan,
        "SELECT * FROM orders WHERE customer_id = 7 AND status = 'void'",
    );
    let r = find(&a, AdviceRule::IndexResidualFilter);
    assert_eq!(columns(r), "customer_id, status");
    assert_eq!(
        r.params.get("usedIndex").map(String::as_str),
        Some("ix_customer")
    );
}

#[test]
fn group_by_on_the_driving_table_with_a_temporary_table() {
    let plan = r#"{"query_block":{"select_id":1,"grouping_operation":{"using_temporary_table":true,
        "using_filesort":false,"table":{"table_name":"o","access_type":"ALL",
        "rows_examined_per_scan":100000,"rows_produced_per_join":100000,"filtered":"100.00"}}}}"#;
    let a = advice(
        PlanFormat::MySqlJson,
        MYSQL8,
        plan,
        "SELECT o.status, COUNT(*) FROM orders o GROUP BY o.status",
    );
    let g = find(&a, AdviceRule::MysqlTempTableGroup);
    assert_eq!(columns(g), "status");
    assert_eq!(g.table.as_ref().unwrap().name, "orders");
    // Grouping by a column of the inner table cannot be helped by an index.
    let join = r#"{"query_block":{"select_id":1,"grouping_operation":{"using_temporary_table":true,
        "nested_loop":[{"table":{"table_name":"o","access_type":"ALL","rows_examined_per_scan":100000,
        "rows_produced_per_join":100000,"filtered":"100.00"}},
        {"table":{"table_name":"c","access_type":"eq_ref","key":"PRIMARY","rows_examined_per_scan":1,
        "rows_produced_per_join":100000,"filtered":"100.00"}}]}}}"#;
    let none = advice(
        PlanFormat::MySqlJson,
        MYSQL8,
        join,
        "SELECT c.country, COUNT(*) FROM orders o JOIN customers c ON c.id = o.customer_id GROUP BY c.country",
    );
    assert!(
        none.iter()
            .all(|a| a.rule != AdviceRule::MysqlTempTableGroup)
    );
}

#[test]
fn join_buffer_on_a_scanned_table_wants_the_join_column_indexed() {
    let plan = r#"{"query_block":{"select_id":1,"nested_loop":[
        {"table":{"table_name":"c","access_type":"ALL","rows_examined_per_scan":2000,
         "rows_produced_per_join":2000,"filtered":"100.00"}},
        {"table":{"table_name":"o","access_type":"ALL","using_join_buffer":"hash join",
         "rows_examined_per_scan":100000,"rows_produced_per_join":200000000,"filtered":"100.00",
         "attached_condition":"(`shop`.`o`.`customer_id` = `shop`.`c`.`id`)"}}]}}"#;
    let a = advice(
        PlanFormat::MySqlJson,
        MYSQL8,
        plan,
        "SELECT * FROM customers c JOIN orders o ON o.customer_id = c.id",
    );
    let j = find(&a, AdviceRule::MysqlJoinBuffer);
    assert_eq!(columns(j), "customer_id");
    assert_eq!(j.table.as_ref().unwrap().name, "orders");
    assert_eq!(
        j.params.get("algorithm").map(String::as_str),
        Some("hash join")
    );
}

#[test]
fn hash_join_of_the_tree_text_indexes_the_bigger_input() {
    let plan = "-> Inner hash join (o.customer_id = c.id)  (cost=900 rows=100000)\n    -> Table scan on o  (cost=500 rows=100000)\n    -> Hash\n        -> Table scan on c  (cost=100 rows=2000)\n";
    let a = advice(
        PlanFormat::MySqlTree,
        MYSQL8,
        plan,
        "SELECT * FROM customers c JOIN orders o ON o.customer_id = c.id",
    );
    let j = find(&a, AdviceRule::MysqlJoinBuffer);
    assert_eq!(columns(j), "customer_id");
    assert_eq!(j.table.as_ref().unwrap().name, "orders");
}

#[test]
fn intersect_of_two_indexes_wants_one_composite_index() {
    let plan = "-> Filter: ((t.a = 1) and (t.b = 2))  (cost=100 rows=10)\n    -> Intersect rows sorted by row ID  (cost=100 rows=10)\n        -> Index range scan on t using ix_a over (a = 1)  (cost=10 rows=5000)\n        -> Index range scan on t using ix_b over (b = 2)  (cost=10 rows=4000)\n";
    let a = advice(
        PlanFormat::MySqlTree,
        MYSQL8,
        plan,
        "SELECT * FROM t WHERE a = 1 AND b = 2",
    );
    let m = find(&a, AdviceRule::MysqlIndexMerge);
    assert_eq!(columns(m), "a, b");
    assert_eq!(
        m.params.get("usedIndex").map(String::as_str),
        Some("ix_a, ix_b")
    );
}

#[test]
fn big_derived_table_gets_a_pointer_not_an_index() {
    let plan = "-> Table scan on d  (cost=5000 rows=50000)\n    -> Materialize  (cost=5000 rows=50000)\n        -> Table scan on orders  (cost=4000 rows=50000)\n";
    let a = advice(
        PlanFormat::MySqlTree,
        MYSQL8,
        plan,
        "SELECT * FROM (SELECT * FROM orders) d WHERE d.status = 'x'",
    );
    let d = find(&a, AdviceRule::FullScanReview);
    assert!(
        d.actions
            .iter()
            .any(|x| x.label_key == "reviewDerivedTable")
    );
    assert!(
        a.iter()
            .all(|a| a.table.as_ref().is_none_or(|t| t.name != "d"))
    );
}

fn column(name: &str, ty: &str) -> ColumnDetail {
    ColumnDetail {
        name: name.into(),
        ordinal: 1,
        type_name: ty.into(),
        value_kind: ValueKind::Text,
        nullable: true,
        default: None,
        comment: None,
        primary_key: false,
        extra: None,
    }
}

#[test]
fn string_column_against_a_number_is_an_implicit_conversion() {
    let plan = r#"{"query_block":{"select_id":1,"table":{"table_name":"orders","access_type":"ALL",
        "rows_examined_per_scan":100000,"rows_produced_per_join":1,"filtered":"0.00",
        "attached_condition":"(`shop`.`orders`.`code` = 123)"}}}"#;
    let sql = "SELECT * FROM orders WHERE code = 123";
    let r = raw(PlanFormat::MySqlJson, MYSQL8, plan, sql);
    let rules = PlanRules::default();
    let tree = parse_and_analyze(&r, &rules).unwrap();
    // Without the catalog the column type is unknown: nothing is claimed.
    assert!(
        tree.advice
            .iter()
            .all(|a| a.rule != AdviceRule::ImplicitConversion)
    );
    let mut ctx = AdvisorContext::basic(&r, &rules);
    ctx.tables.insert(
        TableKey::new(None, "orders"),
        TableMeta {
            path: ObjectPath::root(),
            estimated_rows: Some(100_000),
            columns: vec![column("id", "int"), column("code", "varchar(20)")],
            indexes: vec![IndexDetail {
                name: "ix_code".into(),
                columns: vec!["code".into()],
                unique: false,
                primary: false,
                method: Some("btree".into()),
                definition: None,
                partial: false,
            }],
        },
    );
    ctx.metadata = MetadataState::Checked;
    let a = advise(&tree, sql, &ctx);
    let c = find(&a, AdviceRule::ImplicitConversion);
    assert_eq!(c.params.get("column").map(String::as_str), Some("code"));
    let fix = c
        .actions
        .iter()
        .find(|x| x.kind == ActionKind::Rewrite)
        .unwrap();
    assert_eq!(fix.sql.as_deref(), Some("`code` = '123'"));
    assert!(fix.template);
}

#[test]
fn mariadb_filesort_without_priority_queue_asks_for_a_bigger_sort_buffer() {
    let plan = r#"{"query_block":{"select_id":1,"r_loops":1,"r_total_time_ms":300,
        "filesort":{"sort_key":"orders.created","r_loops":1,"r_total_time_ms":200,
        "r_used_priority_queue":false,"r_output_rows":50000,"r_sort_mode":"sort_key,rowid",
        "table":{"table_name":"orders","access_type":"ALL","r_loops":1,"rows":50000,
        "r_rows":50000,"r_table_time_ms":60,"r_other_time_ms":10,"filtered":100,"r_filtered":100}}}}"#;
    let a = advice(
        PlanFormat::MariaDbJson,
        MARIADB,
        plan,
        "SELECT * FROM orders ORDER BY created",
    );
    let s = find(&a, AdviceRule::SortSpill);
    let set = s
        .actions
        .iter()
        .find(|x| x.kind == ActionKind::SessionSetting)
        .unwrap();
    assert_eq!(
        set.sql.as_deref(),
        Some("SET SESSION sort_buffer_size = 8388608;")
    );
}

fn tidb_rows(rows: &[Value]) -> String {
    serde_json::to_string(rows).unwrap()
}

#[test]
fn tidb_pseudo_statistics_ask_for_analyze() {
    let plan = tidb_rows(&[
        json!({"id": "TableReader_7", "estRows": "10.00", "task": "root", "access object": "",
               "operator info": "data:Selection_6"}),
        json!({"id": "└─Selection_6", "estRows": "10.00", "task": "cop[tikv]", "access object": "",
               "operator info": "eq(shop.customers.name, \"c5\")"}),
        json!({"id": "  └─TableFullScan_5", "estRows": "10000.00", "task": "cop[tikv]",
               "access object": "table:customers", "operator info": "keep order:false, stats:pseudo"}),
    ]);
    let a = advice(
        PlanFormat::TiDbRows,
        "8.0.11-TiDB-v8.5.7",
        &plan,
        "SELECT * FROM customers WHERE name = 'c5'",
    );
    let p = find(&a, AdviceRule::TidbPseudoStats);
    assert_eq!(p.table.as_ref().unwrap().name, "customers");
    assert_eq!(
        p.actions[0].sql.as_deref(),
        Some("ANALYZE TABLE `customers`;")
    );
    // 10 000 rows read, a tenth kept: also a filter without an index.
    let f = find(&a, AdviceRule::IndexForFilter);
    assert_eq!(columns(f), "name");
}

#[test]
fn tidb_table_side_selection_of_an_index_lookup_is_a_residual_filter() {
    let plan = tidb_rows(&[
        json!({"id": "IndexLookUp_11", "estRows": "100.00", "actRows": "100", "task": "root",
               "access object": "", "operator info": ""}),
        json!({"id": "├─IndexRangeScan_8(Build)", "estRows": "50000.00", "actRows": "50000",
               "task": "cop[tikv]", "access object": "table:orders, index:idx_customer(customer_id)",
               "operator info": "range:[7,7], keep order:false"}),
        json!({"id": "└─Selection_10(Probe)", "estRows": "100.00", "actRows": "100", "task": "cop[tikv]",
               "access object": "", "operator info": "gt(shop.orders.amount, 100)"}),
        json!({"id": "  └─TableRowIDScan_9", "estRows": "50000.00", "actRows": "50000",
               "task": "cop[tikv]", "access object": "table:orders", "operator info": "keep order:false"}),
    ]);
    let a = advice(
        PlanFormat::TiDbRows,
        "8.0.11-TiDB-v8.5.7",
        &plan,
        "SELECT * FROM orders WHERE customer_id = 7 AND amount > 100",
    );
    let r = find(&a, AdviceRule::IndexResidualFilter);
    assert_eq!(columns(r), "customer_id, amount");
}

#[test]
fn tidb_prefix_expressions_become_sql_predicates() {
    let tree = parse_and_analyze(
        &raw(
            PlanFormat::TiDbRows,
            "8.0.11-TiDB-v8.5.7",
            &tidb_rows(&[
                json!({"id": "Selection_6", "estRows": "1.00", "task": "cop[tikv]", "access object": "",
                       "operator info": "eq(db.t.a, \"x\"), not(isnull(db.t.b)), or(lt(db.t.c, 5), in(db.t.d, 1, 2)), like(db.t.e, \"p%\", 92)"}),
            ]),
            "",
        ),
        &PlanRules::default(),
    )
    .unwrap();
    let texts: Vec<&str> = tree
        .root
        .predicates
        .iter()
        .map(|p| p.text.as_str())
        .collect();
    assert_eq!(
        texts,
        [
            "db.t.a = \"x\"",
            "db.t.b IS NOT NULL",
            "(db.t.c < 5) OR (db.t.d IN (1, 2))",
            "db.t.e LIKE \"p%\"",
        ]
    );
}

fn predicates_of(name: &str) -> Vec<(String, String)> {
    let c = corpus().into_iter().find(|c| c.name == name).unwrap();
    let tree = dbstudio_plan::parse(&raw(c.format, c.version, &c.text, "")).unwrap();
    fn walk(n: &dbstudio_plan::PlanNode, out: &mut Vec<(String, String)>) {
        for p in &n.predicates {
            out.push((format!("{:?}", p.role), p.text.clone()));
        }
        n.children.iter().for_each(|c| walk(c, out));
    }
    let mut out = Vec::new();
    walk(&tree.root, &mut out);
    out
}

#[test]
fn parsers_fill_the_predicates_the_rules_read() {
    // JSON v1: attached condition on the table node.
    let v1 = predicates_of("mysql_json_01_fullscan");
    assert_eq!(v1.len(), 1);
    assert_eq!(v1[0].0, "Filter");
    // JSON v2 and tree text: Filter node, lookup condition, sort keys, join condition.
    let v2 = predicates_of("mysql_jsonv2_01_join");
    assert!(v2.contains(&("Filter".into(), "(c.id < 50)".into())));
    assert!(v2.contains(&("IndexCond".into(), "customer_id=c.id".into())));
    let tree = predicates_of("mysql_tree_06_analyze_filesort");
    assert!(tree.contains(&("SortKey".into(), "plan_fx_items.sku".into())));
    assert!(tree.contains(&("Filter".into(), "(plan_fx_items.qty = 1)".into())));
    let range = predicates_of("mysql_tree_02_analyze_nested");
    assert!(range.contains(&("IndexCond".into(), "(id < 50)".into())));
    // MariaDB: filesort key and attached condition.
    let maria = predicates_of("mariadb_json_07_analyze_filesort");
    assert!(maria.contains(&("SortKey".into(), "plan_fx_items.sku".into())));
    assert!(maria.contains(&("Filter".into(), "plan_fx_items.qty = 1".into())));
    // TiDB: Selection and join keys in infix form, sort direction folded in.
    let tidb = predicates_of("tidb_03_hashjoin_agg_analyze");
    assert!(tidb.contains(&(
        "JoinCond".into(),
        "tidbfx.customers.id = tidbfx.orders.customer_id".into()
    )));
    assert!(tidb.contains(&(
        "Filter".into(),
        "tidbfx.orders.customer_id IS NOT NULL".into()
    )));
    let sort = predicates_of("tidb_06_sort_spill_analyze");
    assert!(sort.contains(&("SortKey".into(), "tidbfx.orders.created DESC".into())));
}
