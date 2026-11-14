//! CockroachDB `EXPLAIN (VERBOSE)` fixtures captured from a v25.4 container
//! (`tests/fixtures_cockroach`): shape, figures and warnings.

use std::collections::BTreeMap;
use std::path::PathBuf;

use dbstudio_core::{PlanFormat, RawPlan};
use dbstudio_plan::{
    PlanNode, PlanNodeKind, PlanRule, PlanRules, PlanTree, RawRef, parse, parse_and_analyze,
};

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures_cockroach")
}

fn raw(text: String) -> RawPlan {
    RawPlan {
        format: PlanFormat::CockroachText,
        text,
        statement: String::new(),
        analyzed: false,
        rolled_back: false,
        server_version: "25.4.20".into(),
        captured_at: "2025-01-01T00:00:00Z".into(),
        extra: BTreeMap::new(),
    }
}

fn load(name: &str) -> PlanTree {
    let text = std::fs::read_to_string(dir().join(name)).expect("fixture");
    parse_and_analyze(&raw(text), &PlanRules::default()).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn nodes(t: &PlanTree) -> Vec<&PlanNode> {
    fn walk<'a>(n: &'a PlanNode, out: &mut Vec<&'a PlanNode>) {
        out.push(n);
        n.children.iter().for_each(|c| walk(c, out));
    }
    let mut v = Vec::new();
    walk(&t.root, &mut v);
    v
}

fn find<'a>(t: &'a PlanTree, op: &str) -> &'a PlanNode {
    nodes(t)
        .into_iter()
        .find(|n| n.op == op)
        .unwrap_or_else(|| panic!("no node {op}"))
}

/// (file, analyzed, node count, root op)
const EXPECTED: &[(&str, bool, u32, &str)] = &[
    ("cockroach_01_full_scan.txt", false, 2, "filter"),
    (
        "cockroach_02_lookup_join.txt",
        false,
        2,
        "lookup join (inner)",
    ),
    (
        "cockroach_03_hash_join_analyze.txt",
        true,
        6,
        "group (hash)",
    ),
    ("cockroach_04_sort_analyze.txt", true, 2, "top-k"),
    ("cockroach_05_insert_analyze.txt", true, 4, "insert"),
    ("cockroach_06_index_scan_analyze.txt", true, 0, ""),
    ("cockroach_07_aggregate.txt", false, 0, ""),
    ("cockroach_08_sort_analyze.txt", true, 2, "sort"),
];

#[test]
fn every_fixture_is_listed() {
    let mut on_disk: Vec<String> = std::fs::read_dir(dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    on_disk.sort();
    let mut listed: Vec<String> = EXPECTED.iter().map(|e| e.0.to_string()).collect();
    listed.sort();
    assert_eq!(on_disk, listed);
    assert!(listed.len() >= 5);
}

#[test]
fn fixtures_parse_with_invariants() {
    for (name, analyzed, count, root_op) in EXPECTED {
        let t = load(name);
        assert_eq!(t.format, PlanFormat::CockroachText);
        assert_eq!(t.analyzed, *analyzed, "{name}: analyzed");
        let all = nodes(&t);
        assert_eq!(t.node_count as usize, all.len(), "{name}: node count");
        if *count > 0 {
            assert_eq!(t.node_count, *count, "{name}: node count");
            assert_eq!(t.root.op, *root_op, "{name}: root");
        }
        for (i, n) in all.iter().enumerate() {
            assert_eq!(n.id as usize, i, "{name}: pre-order ids");
            assert!(
                n.raw != RawRef::None,
                "{name}: node {} has no raw ref",
                n.id
            );
        }
        let last = *t.hot_path.last().unwrap();
        assert!(all[last as usize].children.is_empty(), "{name}: hot path");
        let json = serde_json::to_value(&t).unwrap();
        assert_eq!(serde_json::from_value::<PlanTree>(json).unwrap(), t);
    }
}

#[test]
fn scan_and_filter_estimates() {
    let t = load("cockroach_01_full_scan.txt");
    let scan = find(&t, "scan");
    assert_eq!(scan.kind, PlanNodeKind::FullScan);
    assert_eq!(scan.est_rows, Some(6000.0));
    let obj = scan.object.as_ref().unwrap();
    assert_eq!(
        (obj.name.as_str(), obj.index.as_deref()),
        ("big", Some("big_pkey"))
    );
    let filter = find(&t, "filter");
    assert_eq!(filter.kind, PlanNodeKind::Filter);
    assert_eq!(filter.condition.as_deref(), Some("name = 'n5'"));
    // Small table: no full scan warning with the default thresholds.
    assert!(t.warnings.is_empty());
    assert!(!t.analyzed && t.totals.execution_time_ms.is_none());
}

#[test]
fn lookup_join_has_table_and_condition() {
    let t = load("cockroach_02_lookup_join.txt");
    let j = find(&t, "lookup join (inner)");
    assert_eq!(j.kind, PlanNodeKind::NestedLoopJoin);
    assert_eq!(j.object.as_ref().unwrap().name, "big");
    assert_eq!(j.condition.as_deref(), Some("(bid) = (id)"));
}

#[test]
fn analyze_figures() {
    let t = load("cockroach_03_hash_join_analyze.txt");
    assert!(t.analyzed);
    assert_eq!(t.totals.planning_time_ms, Some(2.0));
    assert_eq!(t.totals.execution_time_ms, Some(22.0));
    assert_eq!(t.totals.rows, Some(50.0));
    assert_eq!(t.root.kind, PlanNodeKind::HashAggregate);
    assert_eq!(t.root.actual_rows, Some(50.0));
    let join = find(&t, "hash join (inner)");
    assert_eq!(join.kind, PlanNodeKind::HashJoin);
    assert_eq!(join.actual_rows_total, Some(2000.0));
    assert_eq!(join.actual_time_ms, Some(1.0));
    assert_eq!(join.condition.as_deref(), Some("(name) = (column16)"));
    assert_eq!(
        t.root.extra.get("distribution").map(String::as_str),
        Some("local")
    );
    let scans: Vec<_> = nodes(&t).into_iter().filter(|n| n.op == "scan").collect();
    assert_eq!(scans.len(), 2);
    assert_eq!(
        scans[0].extra.get("KV rows decoded").map(String::as_str),
        Some("6,000")
    );
}

#[test]
fn modify_and_sort_kinds() {
    let t = load("cockroach_05_insert_analyze.txt");
    assert_eq!(t.root.kind, PlanNodeKind::Modify);
    assert_eq!(t.root.object.as_ref().unwrap().name, "ords");
    let s = load("cockroach_08_sort_analyze.txt");
    assert_eq!(s.root.kind, PlanNodeKind::Sort);
    assert_eq!(s.root.extra.get("order").map(String::as_str), Some("+name"));
    assert_eq!(
        load("cockroach_04_sort_analyze.txt").root.kind,
        PlanNodeKind::Sort
    );
}

#[test]
fn index_scan_uses_spans() {
    let t = load("cockroach_06_index_scan_analyze.txt");
    let scans: Vec<_> = nodes(&t).into_iter().filter(|n| n.op == "scan").collect();
    assert!(!scans.is_empty());
    assert!(scans.iter().any(|s| s.kind != PlanNodeKind::FullScan));
}

/// A full scan of a big table and a 12x misestimate fire the generic rules.
#[test]
fn warnings_fire_for_full_scan_and_misestimate() {
    let text = "\
planning time: 1ms
execution time: 900ms
distribution: full
vectorized: true

• scan
  columns: (id, name)
  actual row count: 1,500,000
  estimated row count: 120,000 (100% of the table; stats collected 1 hour ago)
  table: big@big_pkey
  spans: FULL SCAN
";
    let t = parse_and_analyze(&raw(text.into()), &PlanRules::default()).unwrap();
    assert_eq!(t.root.kind, PlanNodeKind::FullScan);
    assert_eq!(
        t.root.extra.get("distribution").map(String::as_str),
        Some("full")
    );
    let rules: Vec<PlanRule> = t.warnings.iter().map(|w| w.rule).collect();
    assert!(rules.contains(&PlanRule::FullScanLargeTable));
    assert!(rules.contains(&PlanRule::Misestimate));
}

#[test]
fn bad_text_is_invalid() {
    assert!(parse(&raw("hello".into())).is_err());
}
