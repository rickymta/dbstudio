//! Every captured / documented plan in `tests/fixtures` must parse, produce the pinned shape
//! and totals and fire exactly the pinned warnings.

use std::collections::BTreeMap;
use std::path::PathBuf;

use dbstudio_core::{PlanFormat, RawPlan};
use dbstudio_plan::{
    PlanNode, PlanNodeKind, PlanRule, PlanRules, PlanTree, RawRef, Severity, parse,
    parse_and_analyze,
};

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn format_of(name: &str) -> PlanFormat {
    match name {
        n if n.starts_with("pg_") => PlanFormat::PostgresJson,
        n if n.starts_with("mysql_jsonv2") => PlanFormat::MySqlJsonV2,
        n if n.starts_with("mysql_json") => PlanFormat::MySqlJson,
        n if n.starts_with("mysql_tree") => PlanFormat::MySqlTree,
        n if n.starts_with("mariadb_") => PlanFormat::MariaDbJson,
        n if n.starts_with("mssql_") => PlanFormat::MsSqlShowplanXml,
        n if n.starts_with("oracle_") => PlanFormat::OraclePlanRows,
        n if n.starts_with("mongo_") => PlanFormat::MongoExplain,
        n => panic!("unknown fixture prefix: {n}"),
    }
}

fn raw_of(name: &str) -> RawPlan {
    RawPlan {
        format: format_of(name),
        text: std::fs::read_to_string(dir().join(name)).expect("fixture readable"),
        statement: String::new(),
        analyzed: false,
        rolled_back: false,
        server_version: String::new(),
        captured_at: "2025-01-01T00:00:00Z".into(),
        extra: BTreeMap::new(),
    }
}

fn load(name: &str) -> PlanTree {
    parse_and_analyze(&raw_of(name), &PlanRules::default())
        .unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn walk<'a>(n: &'a PlanNode, out: &mut Vec<&'a PlanNode>) {
    out.push(n);
    for c in &n.children {
        walk(c, out);
    }
}

fn nodes(t: &PlanTree) -> Vec<&PlanNode> {
    let mut v = Vec::new();
    walk(&t.root, &mut v);
    v
}

fn warning_keys(t: &PlanTree) -> Vec<String> {
    t.warnings
        .iter()
        .map(|w| {
            let rule = serde_json::to_value(w.rule).unwrap();
            let sev = serde_json::to_value(w.severity).unwrap();
            format!(
                "{}:{}:{}",
                w.node_id,
                rule.as_str().unwrap(),
                sev.as_str().unwrap()
            )
        })
        .collect()
}

/// (file, analyzed, node count, total cost, warnings as `node:rule:severity`)
type Expectation = (
    &'static str,
    bool,
    u32,
    Option<f64>,
    &'static [&'static str],
);

const EXPECTED: &[Expectation] = &[
    (
        "mariadb_json_01_fullscan.json",
        false,
        1,
        None,
        &["0:fullScanLargeTable:warning"],
    ),
    ("mariadb_json_02_join_group.json", false, 5, None, &[]),
    ("mariadb_json_03_nested.json", false, 3, None, &[]),
    (
        "mariadb_json_04_analyze_join_limit.json",
        true,
        5,
        None,
        &["2:nestedLoopManyRows:warning"],
    ),
    ("mariadb_json_05_analyze_union.json", true, 3, None, &[]),
    ("mariadb_json_06_analyze_group.json", true, 5, None, &[]),
    (
        "mariadb_json_07_analyze_filesort.json",
        true,
        3,
        None,
        &["2:fullScanLargeTable:warning"],
    ),
    ("mongo_01_collscan_planner.json", false, 1, None, &[]),
    ("mongo_02_ixscan_fetch_stats.json", true, 2, None, &[]),
    (
        "mongo_03_collscan_sort_disk_stats.json",
        true,
        2,
        None,
        &["0:spillToDisk:warning", "1:fullScanLargeTable:critical"],
    ),
    ("mongo_04_aggregate_group_stats.json", true, 4, None, &[]),
    ("mongo_05_idhack_stats.json", true, 1, None, &[]),
    ("mongo_06_or_inputstages_planner.json", false, 5, None, &[]),
    (
        "mssql_01_clustered_scan_estimated.xml",
        false,
        1,
        Some(1.4412),
        &[],
    ),
    (
        "mssql_02_seek_keylookup_actual.xml",
        true,
        3,
        Some(0.0482),
        &[],
    ),
    (
        "mssql_03_parallel_hash_join_actual.xml",
        true,
        6,
        Some(6.52),
        &[],
    ),
    (
        "mssql_04_sort_spill_actual.xml",
        true,
        2,
        Some(98.2),
        &["0:spillToDisk:warning", "1:fullScanLargeTable:warning"],
    ),
    (
        "mssql_05_nested_loops_many_actual.xml",
        true,
        5,
        Some(82.7),
        &["2:nestedLoopManyRows:warning"],
    ),
    (
        "mssql_06_update_and_select_estimated.xml",
        false,
        4,
        Some(0.1675),
        &[],
    ),
    (
        "mssql_07_missing_index_convert.xml",
        true,
        3,
        Some(38.6),
        &["0:nestedLoopManyRows:warning"],
    ),
    (
        "mysql_json_01_fullscan.json",
        false,
        1,
        Some(10099.25),
        &["0:fullScanLargeTable:warning"],
    ),
    (
        "mysql_json_02_join_group.json",
        false,
        5,
        Some(27646.5),
        &["3:fullScanLargeTable:warning"],
    ),
    ("mysql_json_03_nested.json", false, 3, Some(876.82), &[]),
    (
        "mysql_json_04_join_limit.json",
        false,
        5,
        Some(55248.27),
        &[],
    ),
    ("mysql_json_05_union.json", false, 3, Some(2.06), &[]),
    (
        "mysql_jsonv2_01_join.json",
        false,
        4,
        Some(876.8205808232831),
        &[],
    ),
    (
        "mysql_jsonv2_02_analyze.json",
        true,
        7,
        Some(27646.5),
        &[
            "3:nestedLoopManyRows:warning",
            "5:fullScanLargeTable:warning",
        ],
    ),
    (
        "mysql_jsonv2_03_fullscan.json",
        false,
        2,
        Some(10099.25),
        &["1:fullScanLargeTable:warning"],
    ),
    (
        "mysql_jsonv2_04_union_analyze.json",
        true,
        7,
        Some(4.124719170822088),
        &[],
    ),
    (
        "mysql_jsonv2_05_group_limit.json",
        false,
        7,
        Some(75302.26646542261),
        &[],
    ),
    (
        "mysql_tree_01_analyze_join.txt",
        true,
        7,
        Some(27646.0),
        &[
            "3:nestedLoopManyRows:warning",
            "5:fullScanLargeTable:warning",
        ],
    ),
    (
        "mysql_tree_02_analyze_nested.txt",
        true,
        4,
        Some(877.0),
        &[],
    ),
    (
        "mysql_tree_03_analyze_limit.txt",
        true,
        7,
        Some(75302.0),
        &["4:nestedLoopManyRows:warning"],
    ),
    (
        "mysql_tree_04_tree_fullscan.txt",
        false,
        2,
        Some(10099.0),
        &["1:fullScanLargeTable:warning"],
    ),
    ("mysql_tree_05_analyze_union.txt", true, 7, Some(4.12), &[]),
    (
        "mysql_tree_06_analyze_filesort.txt",
        true,
        4,
        Some(20111.0),
        &["1:misestimate:warning", "3:fullScanLargeTable:warning"],
    ),
    (
        "oracle_01_full_scan_estimated.json",
        false,
        2,
        Some(4812.0),
        &["1:fullScanLargeTable:critical"],
    ),
    (
        "oracle_02_hash_join_actual.json",
        true,
        4,
        Some(6210.0),
        &["3:fullScanLargeTable:critical"],
    ),
    (
        "oracle_03_nested_loops_index_actual.json",
        true,
        5,
        Some(40210.0),
        &["1:nestedLoopManyRows:warning"],
    ),
    (
        "oracle_04_group_by_spill_actual.json",
        true,
        4,
        Some(30221.0),
        &[
            "0:misestimate:critical",
            "1:misestimate:critical",
            "1:spillToDisk:warning",
            "3:fullScanLargeTable:critical",
        ],
    ),
    ("oracle_05_update_estimated.json", false, 3, Some(3.0), &[]),
    ("oracle_06_union_view_actual.json", true, 5, Some(95.0), &[]),
    ("oracle_07_skip_scan.json", true, 3, Some(48200.0), &[]),
    ("pg_01_seqscan_filter.json", false, 1, Some(2923.84), &[]),
    (
        "pg_02_hashjoin_agg_analyze.json",
        true,
        6,
        Some(3888.7),
        &[],
    ),
    (
        "pg_03_hashjoin_index_analyze.json",
        true,
        4,
        Some(2953.54),
        &["1:fullScanLargeTable:warning"],
    ),
    (
        "pg_04_sort_spill_analyze.json",
        true,
        2,
        Some(57564.9),
        &["0:spillToDisk:warning", "1:fullScanLargeTable:warning"],
    ),
    (
        "pg_05_misestimate_analyze.json",
        true,
        4,
        Some(358.19),
        &["0:misestimate:warning", "1:misestimate:warning"],
    ),
    (
        "pg_06_mergejoin_limit_analyze.json",
        true,
        6,
        Some(29095.49),
        &["1:misestimate:critical"],
    ),
    ("pg_07_update_analyze.json", true, 3, Some(242.02), &[]),
    (
        "pg_08_cte_subplan.json",
        false,
        8,
        Some(124409.63),
        &["2:fullScanLargeTable:warning"],
    ),
    ("pg_09_union_window.json", false, 7, Some(24.83), &[]),
    (
        "pg_10_nestedloop_many_analyze.json",
        true,
        5,
        Some(27050.98),
        &["1:nestedLoopManyRows:warning"],
    ),
    (
        "pg_11_parallel_analyze.json",
        true,
        5,
        Some(4235.46),
        &["4:fullScanLargeTable:warning"],
    ),
    // Added with the Query Advisor: one plan per advisor rule that needs figures the older
    // fixtures lack.
    ("pg_12_index_residual.json", true, 1, Some(3812.55), &[]),
    ("pg_13_heap_fetches.json", true, 2, Some(8120.41), &[]),
    ("pg_14_bitmap_lossy.json", true, 2, Some(212480.9), &[]),
    (
        "pg_15_function_filter.json",
        true,
        1,
        Some(28850.0),
        &["0:misestimate:critical"],
    ),
    (
        "pg_16_estimate_topn_parallel.json",
        false,
        4,
        Some(4233.35),
        &[],
    ),
];

#[test]
fn every_fixture_has_an_expectation_and_vice_versa() {
    let mut on_disk: Vec<String> = std::fs::read_dir(dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with('.'))
        .collect();
    on_disk.sort();
    let mut listed: Vec<String> = EXPECTED.iter().map(|e| e.0.to_string()).collect();
    listed.sort();
    assert_eq!(on_disk, listed);
}

#[test]
fn at_least_five_fixtures_per_format() {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for (name, ..) in EXPECTED {
        *counts.entry(format!("{:?}", format_of(name))).or_default() += 1;
    }
    assert_eq!(counts.len(), 8, "{counts:?}");
    for (format, n) in counts {
        assert!(n >= 5, "{format}: only {n} fixtures");
    }
}

#[test]
fn all_fixtures_parse_with_pinned_shape_and_warnings() {
    for (name, analyzed, count, cost, warnings) in EXPECTED {
        let t = load(name);
        assert_eq!(t.analyzed, *analyzed, "{name}: analyzed");
        assert_eq!(t.node_count, *count, "{name}: node count");
        assert_eq!(nodes(&t).len() as u32, *count, "{name}: tree size");
        match (t.totals.total_cost, *cost) {
            (Some(a), Some(b)) => assert!((a - b).abs() < 1e-6, "{name}: cost {a} vs {b}"),
            (a, b) => assert_eq!(a, b, "{name}: total cost"),
        }
        assert_eq!(warning_keys(&t), *warnings, "{name}: warnings");
        // Invariants shared by every format.
        let all = nodes(&t);
        for (i, n) in all.iter().enumerate() {
            assert_eq!(n.id as usize, i, "{name}: pre-order ids");
            let synthetic =
                i == 0 && n.op == "Result" && n.children.len() > 1 && n.raw == RawRef::None;
            assert!(
                synthetic || n.raw != RawRef::None,
                "{name}: node {} has no raw reference",
                n.id
            );
            if let Some(s) = n.self_time_ms {
                assert!(s >= 0.0, "{name}: negative self time");
            }
        }
        assert_eq!(
            t.hot_path.first(),
            Some(&0),
            "{name}: hot path starts at the root"
        );
        let last = *t.hot_path.last().unwrap();
        assert!(
            all[last as usize].children.is_empty(),
            "{name}: hot path ends at a leaf"
        );
        // Serialises to the contract shape and back.
        let json = serde_json::to_value(&t).unwrap();
        assert_eq!(
            serde_json::from_value::<PlanTree>(json).unwrap(),
            t,
            "{name}: round trip"
        );
    }
}

fn find<'a>(t: &'a PlanTree, op: &str) -> &'a PlanNode {
    nodes(t)
        .into_iter()
        .find(|n| n.op == op)
        .unwrap_or_else(|| panic!("no node {op}"))
}

#[test]
fn postgres_details() {
    let t = load("pg_02_hashjoin_agg_analyze.json");
    assert_eq!(t.totals.planning_time_ms, Some(0.521));
    assert_eq!(t.totals.execution_time_ms, Some(34.696));
    assert_eq!(t.totals.rows, Some(4.0));
    let join = find(&t, "Hash Join");
    assert_eq!(
        join.condition.as_deref(),
        Some("Hash Cond: (o.customer_id = c.id)")
    );
    assert_eq!(join.est_rows, Some(89705.0));
    assert_eq!(join.actual_rows, Some(87118.0));
    // Hot path follows the most expensive child: sort, aggregate, join, scan of orders.
    let ops: Vec<&str> = t
        .hot_path
        .iter()
        .map(|id| nodes(&t)[*id as usize].op.as_str())
        .collect();
    assert_eq!(ops, ["Sort", "Aggregate", "Hash Join", "Seq Scan"]);
    // Self time is inclusive time minus the children.
    let hash = find(&t, "Hash");
    assert!((hash.self_time_ms.unwrap() - (0.332 - 0.143)).abs() < 1e-9);

    let sort = load("pg_04_sort_spill_analyze.json");
    let w = &sort.warnings[0];
    assert_eq!(
        (w.rule, w.severity),
        (PlanRule::SpillToDisk, Severity::Warning)
    );
    assert!(w.params["detail"].starts_with("external merge"));

    let mis = load("pg_05_misestimate_analyze.json");
    let w = mis
        .warnings
        .iter()
        .find(|w| w.rule == PlanRule::Misestimate)
        .unwrap();
    assert_eq!(w.severity, Severity::Warning);
    assert_eq!(w.params["direction"], "under");
    assert_eq!(w.params["estimated"], "1064");

    let nl = load("pg_10_nestedloop_many_analyze.json");
    assert_eq!(nl.warnings[0].rule, PlanRule::NestedLoopManyRows);
    assert_eq!(nl.warnings[0].params["loops"], "60000");

    let upd = load("pg_07_update_analyze.json");
    assert_eq!(upd.root.kind, PlanNodeKind::Modify);
    assert!(upd.analyzed);

    let est = load("pg_01_seqscan_filter.json");
    assert!(!est.analyzed);
    assert!(est.root.actual_rows.is_none());
}

#[test]
fn mysql_details() {
    // Tree text: the time is per loop, the node total multiplies by the loops.
    let t = load("mysql_tree_01_analyze_join.txt");
    let lookup = find(&t, "Single-row index lookup");
    assert_eq!(lookup.loops, Some(58079));
    assert!((lookup.actual_time_ms.unwrap() - 588e-6 * 58079.0).abs() < 1e-6);
    assert_eq!(
        lookup.object.as_ref().unwrap().index.as_deref(),
        Some("PRIMARY")
    );
    assert_eq!(t.totals.execution_time_ms, Some(77.5));
    assert!(matches!(lookup.raw, RawRef::Lines { .. }));

    // JSON v1: the ordering/grouping wrappers and the left-deep join.
    let j = load("mysql_json_02_join_group.json");
    assert_eq!(j.root.op, "Ordering");
    assert_eq!(j.root.children[0].op, "Grouping");
    assert_eq!(j.totals.total_cost, Some(27646.5));
    let scan = find(&j, "Table scan");
    assert_eq!(scan.object.as_ref().unwrap().name, "o");
    assert_eq!(scan.extra["rows_examined_per_scan"], "100270");

    let u = load("mysql_json_05_union.json");
    assert_eq!(u.root.kind, PlanNodeKind::Union);
    assert_eq!(u.root.children.len(), 2);

    // JSON v2 carries actual figures for EXPLAIN ANALYZE.
    let v2 = load("mysql_jsonv2_02_analyze.json");
    assert!(v2.analyzed);
    let lookup = find(&v2, "Single-row index lookup");
    assert_eq!(lookup.loops, Some(58079));
}

#[test]
fn mysql_json_format_is_detected_from_content() {
    // A v2 document declared as v1 (and the other way round) still parses.
    let mut raw = raw_of("mysql_jsonv2_01_join.json");
    raw.format = PlanFormat::MySqlJson;
    assert!(parse(&raw).is_ok());
    let mut raw = raw_of("mysql_json_01_fullscan.json");
    raw.format = PlanFormat::MySqlJsonV2;
    assert!(parse(&raw).is_ok());
}

#[test]
fn mariadb_details() {
    let t = load("mariadb_json_03_nested.json");
    assert_eq!(t.root.kind, PlanNodeKind::NestedLoopJoin);
    assert_eq!(
        t.root.children.len(),
        2,
        "duplicate \"table\" members form a join"
    );
    let a = load("mariadb_json_04_analyze_join_limit.json");
    assert_eq!(a.totals.execution_time_ms, Some(109.045));
    let i = find(&a, "Index lookup");
    assert_eq!(i.loops, Some(100000));
    // Exclusive step times become inclusive: table 74.126 + other 19.081.
    assert!((i.actual_time_ms.unwrap() - 93.207).abs() < 1e-9);
    // A filesort never takes longer than the whole statement.
    let f = load("mariadb_json_07_analyze_filesort.json");
    assert!(
        nodes(&f)
            .iter()
            .all(|n| n.actual_time_ms.unwrap_or(0.0) <= 36.326 + 1e-9)
    );
}

#[test]
fn mssql_details() {
    let t = load("mssql_03_parallel_hash_join_actual.xml");
    assert_eq!(t.totals.total_cost, Some(6.52));
    assert_eq!(t.totals.execution_time_ms, Some(176.0));
    assert_eq!(t.totals.planning_time_ms, Some(4.0));
    // Rows add up over the threads, the elapsed time is the slowest thread.
    let scan = find(&t, "Index Scan");
    assert_eq!(scan.actual_rows_total, Some(121600.0));
    assert_eq!(scan.actual_time_ms, Some(90.0));
    assert_eq!(scan.loops, Some(2));
    assert!(matches!(&scan.raw, RawRef::XmlNode { node_id } if node_id == "6"));

    let s = load("mssql_04_sort_spill_actual.xml");
    assert!(
        s.warnings
            .iter()
            .any(|w| w.rule == PlanRule::SpillToDisk && w.node_id == 0)
    );

    let k = load("mssql_02_seek_keylookup_actual.xml");
    assert_eq!(find(&k, "Key Lookup").kind, PlanNodeKind::KeyLookup);

    let m = load("mssql_06_update_and_select_estimated.xml");
    assert_eq!(
        m.root.kind,
        PlanNodeKind::Result,
        "several statements get a synthetic root"
    );
    assert_eq!(m.root.children.len(), 2);
}

#[test]
fn oracle_details() {
    let t = load("oracle_02_hash_join_actual.json");
    // LAST_ELAPSED_TIME is in microseconds.
    assert_eq!(t.totals.execution_time_ms, Some(310.0));
    let nl = load("oracle_03_nested_loops_index_actual.json");
    assert_eq!(nl.warnings[0].rule, PlanRule::NestedLoopManyRows);
    let sp = load("oracle_04_group_by_spill_actual.json");
    assert!(sp.warnings.iter().any(|w| w.rule == PlanRule::SpillToDisk));
    assert!(
        sp.warnings
            .iter()
            .any(|w| w.rule == PlanRule::Misestimate && w.severity == Severity::Critical)
    );
    assert!(matches!(sp.root.raw, RawRef::Row { id: 0 }));
}

#[test]
fn mongo_details() {
    let t = load("mongo_03_collscan_sort_disk_stats.json");
    assert_eq!(t.totals.execution_time_ms, Some(2480.0));
    assert!(
        t.warnings
            .iter()
            .any(|w| w.rule == PlanRule::SpillToDisk && w.node_id == 0)
    );
    assert!(
        t.warnings
            .iter()
            .any(|w| w.rule == PlanRule::FullScanLargeTable)
    );
    // Planner-only output has no actual figures.
    let p = load("mongo_01_collscan_planner.json");
    assert!(!p.analyzed);
    let a = load("mongo_04_aggregate_group_stats.json");
    assert_eq!(a.root.op, "$sort");
    assert_eq!(
        find(&a, "IXSCAN").object.as_ref().unwrap().index.as_deref(),
        Some("customerId_1_total_1")
    );
}

#[test]
fn custom_rules_change_the_warnings() {
    let raw = raw_of("pg_05_misestimate_analyze.json");
    let strict = PlanRules {
        misestimate_factor: 1000.0,
        ..PlanRules::default()
    };
    let t = parse_and_analyze(&raw, &strict).unwrap();
    assert!(t.warnings.is_empty());
    let loose = PlanRules {
        large_table_rows: 1000.0,
        ..PlanRules::default()
    };
    let t = parse_and_analyze(&raw, &loose).unwrap();
    assert!(
        t.warnings
            .iter()
            .any(|w| w.rule == PlanRule::FullScanLargeTable)
    );
}

#[test]
fn damaged_inputs_are_errors_not_panics() {
    for name in [
        "pg_02_hashjoin_agg_analyze.json",
        "mysql_tree_01_analyze_join.txt",
        "mssql_03_parallel_hash_join_actual.xml",
        "oracle_02_hash_join_actual.json",
        "mongo_02_ixscan_fetch_stats.json",
        "mariadb_json_04_analyze_join_limit.json",
        "mysql_json_02_join_group.json",
    ] {
        let mut raw = raw_of(name);
        let cut = raw.text.len() / 2;
        // Cut on a char boundary.
        let cut = (0..=cut)
            .rev()
            .find(|i| raw.text.is_char_boundary(*i))
            .unwrap();
        raw.text.truncate(cut);
        let _ = parse(&raw); // must not panic; most are Err
    }
    let mut raw = raw_of("pg_02_hashjoin_agg_analyze.json");
    raw.text = raw.text.replace("\"Plans\"", "\"Plans\": 5, \"X\"");
    assert!(parse(&raw).is_ok());
}

#[test]
fn parallel_plans_report_wall_time_not_summed_worker_time() {
    let t = load("pg_11_parallel_analyze.json");
    let total = t.totals.execution_time_ms.unwrap();
    let all = nodes(&t);
    // Everything runs inside the statement, so no node can take longer than it (small slack
    // for the leader's own startup).
    for n in &all {
        assert!(
            n.actual_time_ms.unwrap() <= total * 1.05,
            "{} took {:?} of {total}",
            n.op,
            n.actual_time_ms
        );
    }
    // The Sort ran in 3 processes (2 workers and the leader) at ~40 ms each, concurrently.
    let sort = find(&t, "Sort");
    assert_eq!(sort.loops, Some(3));
    let t_sort = sort.actual_time_ms.unwrap();
    assert!((30.0..=50.0).contains(&t_sort), "{t_sort}");
    // Rows still add up over the processes.
    assert!(sort.actual_rows_total.unwrap() > sort.actual_rows.unwrap() * 2.9);
    // Self time of the Gather Merge is small, not negative-clamped garbage.
    let gm = find(&t, "Gather Merge");
    assert!(gm.self_time_ms.unwrap() < 10.0);
}

fn nested_xml(depth: usize) -> String {
    let mut s =
        String::from("<ShowPlanXML><BatchSequence><Batch><Statements><StmtSimple><QueryPlan>");
    for i in 0..depth {
        s.push_str(&format!("<RelOp NodeId=\"{i}\" PhysicalOp=\"Sort\"><Sort>"));
    }
    for _ in 0..depth {
        s.push_str("</Sort></RelOp>");
    }
    s.push_str("</QueryPlan></StmtSimple></Statements></Batch></BatchSequence></ShowPlanXML>");
    s
}

fn raw_text(format: PlanFormat, text: String) -> RawPlan {
    RawPlan {
        text,
        ..raw_of_format(format)
    }
}

fn raw_of_format(format: PlanFormat) -> RawPlan {
    RawPlan {
        format,
        text: String::new(),
        statement: String::new(),
        analyzed: false,
        rolled_back: false,
        server_version: String::new(),
        captured_at: String::new(),
        extra: BTreeMap::new(),
    }
}

#[test]
fn deeply_nested_input_is_rejected_not_a_stack_overflow() {
    // 500 nested RelOp elements.
    let err = parse(&raw_text(PlanFormat::MsSqlShowplanXml, nested_xml(500))).unwrap_err();
    assert!(err.to_string().contains("nested deeper"), "{err}");
    // 100 levels (200 elements deep) are fine.
    assert!(parse(&raw_text(PlanFormat::MsSqlShowplanXml, nested_xml(100))).is_ok());

    // MySQL tree with 1500 nested lines.
    let mut tree = String::new();
    for i in 0..1500 {
        tree.push_str(&format!(
            "{}-> Filter: (a = {i})  (cost=1 rows=1)\n",
            "    ".repeat(i)
        ));
    }
    let err = parse(&raw_text(PlanFormat::MySqlTree, tree)).unwrap_err();
    assert!(err.to_string().contains("nested deeper"), "{err}");

    // PostgreSQL JSON nested 1000 plan levels.
    let mut pg = String::from("[{\"Plan\":");
    for _ in 0..1000 {
        pg.push_str("{\"Node Type\":\"Result\",\"Plans\":[");
    }
    pg.push_str("{\"Node Type\":\"Result\"}");
    for _ in 0..1000 {
        pg.push_str("]}");
    }
    pg.push_str("}]");
    assert!(parse(&raw_text(PlanFormat::PostgresJson, pg)).is_err());

    // Oracle: a chain of 20000 parent links.
    let rows: Vec<String> = (0..20_000)
        .map(|i| {
            let parent = if i == 0 {
                "null".to_string()
            } else {
                (i - 1).to_string()
            };
            format!("{{\"ID\":{i},\"PARENT_ID\":{parent},\"OPERATION\":\"SORT\"}}")
        })
        .collect();
    let err = parse(&raw_text(
        PlanFormat::OraclePlanRows,
        format!("[{}]", rows.join(",")),
    ))
    .unwrap_err();
    assert!(err.to_string().contains("nested deeper"), "{err}");

    // MongoDB: 1000 nested inputStage levels.
    let mut mongo = String::from("{\"queryPlanner\":{\"winningPlan\":");
    for _ in 0..1000 {
        mongo.push_str("{\"stage\":\"FETCH\",\"inputStage\":");
    }
    mongo.push_str("{\"stage\":\"COLLSCAN\"}");
    for _ in 0..1000 {
        mongo.push('}');
    }
    mongo.push_str("}}");
    assert!(parse(&raw_text(PlanFormat::MongoExplain, mongo)).is_err());
}

#[test]
fn very_wide_plans_parse() {
    // 20 000 sibling elements: large but flat, must not abort.
    let mut s = String::from(
        "<ShowPlanXML><BatchSequence><Batch><Statements><StmtSimple><QueryPlan><RelOp NodeId=\"0\" PhysicalOp=\"Concatenation\"><Concat>",
    );
    for i in 1..=20_000 {
        s.push_str(&format!(
            "<RelOp NodeId=\"{i}\" PhysicalOp=\"Constant Scan\"/>"
        ));
    }
    s.push_str("</Concat></RelOp></QueryPlan></StmtSimple></Statements></Batch></BatchSequence></ShowPlanXML>");
    let t = parse_and_analyze(
        &raw_text(PlanFormat::MsSqlShowplanXml, s),
        &PlanRules::default(),
    )
    .unwrap();
    assert_eq!(t.node_count, 20_001);
}
