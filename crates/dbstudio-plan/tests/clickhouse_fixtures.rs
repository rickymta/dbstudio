//! Plans captured from a ClickHouse 26.3 server (`EXPLAIN json = 1, indexes = 1, actions = 1`)
//! parse into the pinned shape and fire the pinned warnings.

use std::collections::BTreeMap;
use std::path::PathBuf;

use dbstudio_core::{PlanFormat, RawPlan};
use dbstudio_plan::{PlanNode, PlanNodeKind, PlanRule, PlanRules, PlanTree, parse_and_analyze};

fn load(name: &str) -> PlanTree {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/clickhouse")
        .join(name);
    let raw = RawPlan {
        format: PlanFormat::ClickHouseJson,
        text: std::fs::read_to_string(path).expect("fixture readable"),
        statement: String::new(),
        analyzed: false,
        rolled_back: false,
        server_version: "26.3".into(),
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

fn reads(t: &PlanTree) -> Vec<&PlanNode> {
    nodes(t)
        .into_iter()
        .filter(|n| n.op.starts_with("ReadFrom"))
        .collect()
}

fn rules(t: &PlanTree) -> Vec<(u32, PlanRule)> {
    t.warnings.iter().map(|w| (w.node_id, w.rule)).collect()
}

#[test]
fn primary_key_narrows_the_read() {
    let t = load("01_pk_index.json");
    assert_eq!(t.node_count, 5);
    let read = reads(&t)[0];
    assert_eq!(read.kind, PlanNodeKind::IndexScan);
    let o = read.object.as_ref().unwrap();
    assert_eq!(
        (o.schema.as_deref(), o.name.as_str(), o.index.as_deref()),
        (Some("plan_fx"), "events", Some("PRIMARY KEY"))
    );
    assert_eq!(read.extra["granules_initial"], "247");
    assert_eq!(read.extra["granules_selected"], "10");
    assert!(read.condition.as_deref().unwrap().contains("PRIMARY KEY"));
    assert!(read.extra.keys().any(|k| k == "index PrimaryKey"));
    assert!(t.warnings.is_empty());
}

#[test]
fn unselective_filter_is_a_full_scan_with_a_warning() {
    let t = load("02_full_scan.json");
    let read = reads(&t)[0];
    assert_eq!(read.kind, PlanNodeKind::FullScan);
    assert_eq!(read.est_rows, Some(247.0 * 8192.0));
    assert_eq!(rules(&t), vec![(read.id, PlanRule::FullScanLargeTable)]);
}

#[test]
fn skipping_index_stage_is_reported() {
    let t = load("03_skip_index.json");
    let read = reads(&t)[0];
    assert!(
        read.extra.keys().any(|k| k.starts_with("index ")),
        "{:?}",
        read.extra
    );
    assert!(read.condition.is_none() || read.kind != PlanNodeKind::Other);
}

#[test]
fn join_has_two_reads_and_a_hash_join() {
    let t = load("04_join.json");
    let all = nodes(&t);
    let join = all
        .iter()
        .find(|n| n.kind == PlanNodeKind::HashJoin)
        .expect("join");
    assert_eq!(join.op, "Join");
    assert!(join.condition.as_deref().unwrap().contains("user_id"));
    assert_eq!(reads(&t).len(), 2);
    assert!(all.iter().any(|n| n.kind == PlanNodeKind::Sort));
    assert!(all.iter().any(|n| n.kind == PlanNodeKind::Limit));
    // The big table is narrowed by the primary key, the small one is read whole.
    let events = reads(&t)
        .into_iter()
        .find(|n| n.object.as_ref().unwrap().name == "events")
        .unwrap();
    assert_eq!(events.kind, PlanNodeKind::IndexScan);
}

#[test]
fn group_by_scans_everything() {
    let t = load("05_group_by.json");
    let all = nodes(&t);
    let agg = all
        .iter()
        .find(|n| n.kind == PlanNodeKind::HashAggregate)
        .expect("aggregate");
    assert!(agg.condition.as_deref().unwrap().starts_with("GROUP BY"));
    assert_eq!(rules(&t).len(), 1);
}

#[test]
fn final_on_a_replacing_table_parses() {
    let t = load("06_final.json");
    assert_eq!(reads(&t)[0].object.as_ref().unwrap().name, "rep");
    assert!(t.hot_path.is_empty() || t.hot_path[0] == 0);
}

#[test]
fn materialized_view_target_table_read() {
    let t = load("07_mv_read.json");
    assert_eq!(reads(&t)[0].object.as_ref().unwrap().name, "daily");
    assert!(t.warnings.is_empty());
}
