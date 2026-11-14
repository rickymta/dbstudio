//! TiDB `EXPLAIN [ANALYZE] FORMAT='row'` fixtures (rows packed as JSON by the driver). The first
//! seven were captured from a `pingcap/tidb:v8.5.7` container (unistore); `tidb_08_mpp_documented`
//! is written by hand in the shape the TiDB documentation shows for an MPP plan, because the test
//! stack has no TiFlash.

use std::collections::BTreeMap;
use std::path::PathBuf;

use dbstudio_core::{PlanFormat, RawPlan};
use dbstudio_plan::{PlanNode, PlanNodeKind, PlanRule, PlanRules, PlanTree, parse_and_analyze};

fn load(name: &str) -> PlanTree {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures_tidb")
        .join(format!("{name}.json"));
    let raw = RawPlan {
        format: PlanFormat::TiDbRows,
        text: std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path:?}: {e}")),
        statement: String::new(),
        analyzed: false,
        rolled_back: false,
        server_version: "8.0.11-TiDB-v8.5.7".into(),
        captured_at: "2026-11-14T00:00:00Z".into(),
        extra: BTreeMap::new(),
    };
    parse_and_analyze(&raw, &PlanRules::default()).unwrap_or_else(|e| panic!("{name}: {e}"))
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

fn ops(t: &PlanTree) -> Vec<&str> {
    nodes(t).iter().map(|n| n.op.as_str()).collect()
}

fn rules(t: &PlanTree, rule: PlanRule) -> Vec<u32> {
    t.warnings
        .iter()
        .filter(|w| w.rule == rule)
        .map(|w| w.node_id)
        .collect()
}

const ALL: &[&str] = &[
    "tidb_01_fullscan_pseudo",
    "tidb_02_indexlookup_analyze",
    "tidb_03_hashjoin_agg_analyze",
    "tidb_04_indexjoin_limit",
    "tidb_05_pointget_analyze",
    "tidb_06_sort_spill_analyze",
    "tidb_07_update_plan",
    "tidb_08_mpp_documented",
];

#[test]
fn every_fixture_parses_into_a_consistent_tree() {
    for name in ALL {
        let t = load(name);
        assert_eq!(t.format, PlanFormat::TiDbRows, "{name}");
        let all = nodes(&t);
        assert_eq!(all.len() as u32, t.node_count, "{name}");
        // Pre-order ids, one root, hot path starts at the root.
        for (i, n) in all.iter().enumerate() {
            assert_eq!(n.id as usize, i, "{name}");
        }
        assert_eq!(t.hot_path.first(), Some(&0), "{name}");
    }
}

#[test]
fn full_scan_with_pseudo_stats() {
    let t = load("tidb_01_fullscan_pseudo");
    assert!(!t.analyzed);
    assert_eq!(ops(&t), ["TableReader", "Selection", "TableFullScan"]);
    let scan = nodes(&t)[2];
    assert_eq!(scan.kind, PlanNodeKind::FullScan);
    assert_eq!(scan.object.as_ref().unwrap().name, "customers");
    assert_eq!(scan.est_rows, Some(1000.0));
    // 1 000 estimated rows is below the large-table threshold.
    assert!(t.warnings.is_empty(), "{:?}", t.warnings);
}

#[test]
fn index_lookup_with_cop_tasks() {
    let t = load("tidb_02_indexlookup_analyze");
    assert!(t.analyzed);
    assert_eq!(
        ops(&t),
        [
            "IndexLookUp",
            "IndexRangeScan",
            "Selection",
            "TableRowIDScan"
        ]
    );
    let n = nodes(&t);
    assert_eq!(n[0].kind, PlanNodeKind::KeyLookup);
    let scan = n[1];
    assert_eq!(scan.extra.get("role").map(String::as_str), Some("build"));
    let o = scan.object.as_ref().unwrap();
    assert_eq!(
        (o.name.as_str(), o.index.as_deref()),
        ("orders", Some("idx_customer"))
    );
    assert_eq!(scan.actual_rows, Some(100.0));
    assert!(n[0].actual_time_ms.unwrap() > 0.0);
    assert!(t.totals.execution_time_ms.is_some());
}

#[test]
fn hash_join_aggregate_tree_shape() {
    let t = load("tidb_03_hashjoin_agg_analyze");
    assert!(t.analyzed);
    let join = nodes(&t)
        .into_iter()
        .find(|n| n.kind == PlanNodeKind::HashJoin)
        .unwrap();
    assert_eq!(join.children.len(), 2);
    assert_eq!(
        join.children[0].extra.get("role").map(String::as_str),
        Some("build")
    );
    assert_eq!(
        join.children[1].extra.get("role").map(String::as_str),
        Some("probe")
    );
    assert!(ops(&t).contains(&"HashAgg"));
    // The 100 000 row scan is exactly at the large-table threshold.
    assert_eq!(rules(&t, PlanRule::FullScanLargeTable).len(), 1);
}

#[test]
fn estimated_index_join_has_no_actuals() {
    let t = load("tidb_04_indexjoin_limit");
    assert!(!t.analyzed);
    assert_eq!(t.root.op, "TopN");
    assert_eq!(t.root.kind, PlanNodeKind::Sort);
    let join = &t.root.children[0];
    assert_eq!(join.kind, PlanNodeKind::NestedLoopJoin);
    assert!(nodes(&t).iter().all(|n| n.actual_rows.is_none()));
}

#[test]
fn point_get_is_a_seek() {
    let t = load("tidb_05_pointget_analyze");
    assert_eq!(t.node_count, 1);
    assert_eq!(t.root.kind, PlanNodeKind::IndexSeek);
    assert_eq!(t.root.actual_rows, Some(1.0));
}

#[test]
fn spill_to_disk_is_reported() {
    let t = load("tidb_06_sort_spill_analyze");
    assert_eq!(t.root.op, "Sort");
    let spills = rules(&t, PlanRule::SpillToDisk);
    assert_eq!(spills, vec![0]);
    let w = t
        .warnings
        .iter()
        .find(|w| w.rule == PlanRule::SpillToDisk)
        .unwrap();
    assert!(w.params["detail"].starts_with("disk "), "{:?}", w.params);
    // The full scan also reaches the threshold.
    assert_eq!(rules(&t, PlanRule::FullScanLargeTable).len(), 1);
}

#[test]
fn update_plan_has_a_modify_root() {
    let t = load("tidb_07_update_plan");
    assert_eq!(t.root.op, "Update");
    assert_eq!(t.root.kind, PlanNodeKind::Modify);
    // `estRows` is N/A for the statement node.
    assert_eq!(t.root.est_rows, None);
    assert!(!t.analyzed);
}

#[test]
fn mpp_plan_with_exchange_operators() {
    let t = load("tidb_08_mpp_documented");
    assert!(t.analyzed);
    assert_eq!(t.node_count, 8);
    let kinds: Vec<_> = nodes(&t).iter().map(|n| n.kind).collect();
    assert_eq!(kinds[0], PlanNodeKind::Gather);
    assert_eq!(kinds[1], PlanNodeKind::Gather);
    assert_eq!(kinds[3], PlanNodeKind::HashAggregate);
    assert_eq!(kinds[7], PlanNodeKind::FullScan);
    let scan = nodes(&t)[7];
    assert_eq!(
        scan.extra.get("task").map(String::as_str),
        Some("mpp[tiflash]")
    );
    // The MPP scan reads 10 M rows: a critical full scan.
    assert_eq!(rules(&t, PlanRule::FullScanLargeTable), vec![7]);
}

#[test]
fn misestimate_fires_on_a_tenfold_gap() {
    let text = r#"[
      {"id":"Selection_5","estRows":"10.00","actRows":"5000","task":"root","access object":"","execution info":"time:3ms, loops:6","operator info":"gt(t.a.v, 1)","memory":"1 KB","disk":"N/A"}
    ]"#;
    let raw = RawPlan {
        format: PlanFormat::TiDbRows,
        text: text.into(),
        statement: String::new(),
        analyzed: true,
        rolled_back: true,
        server_version: String::new(),
        captured_at: String::new(),
        extra: BTreeMap::new(),
    };
    let t = parse_and_analyze(&raw, &PlanRules::default()).unwrap();
    assert_eq!(rules(&t, PlanRule::Misestimate), vec![0]);
}
