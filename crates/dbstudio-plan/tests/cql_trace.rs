//! CQL tracing plans captured from `scylla` (2026.1) and `cassandra5` (5.0.9) by the driver's
//! integration test (`DBSTUDIO_CAPTURE_DIR`). Every one must parse, keep its shape and flag
//! tombstones where the trace shows them.

use std::collections::BTreeMap;
use std::path::PathBuf;

use dbstudio_core::{PlanFormat, RawPlan};
use dbstudio_plan::{PlanNode, PlanNodeKind, PlanRules, PlanTree, parse_and_analyze};

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/cql_fixtures")
}

fn names() -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .filter(|n| n.ends_with(".json"))
        .collect();
    v.sort();
    v
}

fn load(name: &str) -> PlanTree {
    let raw = RawPlan {
        format: PlanFormat::CqlTrace,
        text: std::fs::read_to_string(dir().join(name)).unwrap(),
        statement: String::new(),
        analyzed: true,
        rolled_back: false,
        server_version: String::new(),
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

fn kinds(t: &PlanTree) -> Vec<PlanNodeKind> {
    let mut v = Vec::new();
    walk(&t.root, &mut v);
    v.iter().map(|n| n.kind).collect()
}

#[test]
fn at_least_five_fixtures_per_server() {
    let n = names();
    for prefix in ["scylla_", "cassandra5_"] {
        assert!(
            n.iter().filter(|x| x.starts_with(prefix)).count() >= 5,
            "{prefix}"
        );
    }
}

#[test]
fn every_fixture_parses_with_a_request_root() {
    for name in names() {
        let t = load(&name);
        assert_eq!(t.root.op, "CQL request", "{name}");
        assert!(
            t.analyzed && t.node_count > 3,
            "{name}: {} nodes",
            t.node_count
        );
        assert!(t.totals.execution_time_ms.is_some(), "{name}");
        assert_eq!(t.hot_path[0], 0, "{name}");
        assert!(t.hot_path.len() >= 2, "{name}");
        assert!(
            t.root
                .condition
                .as_deref()
                .is_some_and(|q| q.contains("nums") || q.contains("SELECT")),
            "{name}"
        );
        assert_eq!(t.root.object.as_ref().unwrap().name, "nums", "{name}");
    }
}

#[test]
fn point_lookups_seek_and_full_reads_scan() {
    for name in names() {
        let t = load(&name);
        let k = kinds(&t);
        if name.contains("_01_pk_lookup") {
            assert!(k.contains(&PlanNodeKind::IndexSeek), "{name}");
            assert!(!k.contains(&PlanNodeKind::FullScan), "{name}");
            assert_eq!(t.root.actual_rows, Some(1.0), "{name}");
        }
        if name.contains("_04_full_table") || name.contains("_02_filtering_scan") {
            assert!(k.contains(&PlanNodeKind::FullScan), "{name}");
        }
    }
}

#[test]
fn tombstones_are_reported_only_where_present() {
    for name in names() {
        let t = load(&name);
        let flagged = t
            .root
            .extra
            .get("warning")
            .is_some_and(|w| w.contains("tombstones"));
        if name.contains("_03_tombstones") {
            assert!(flagged, "{name}");
            assert_eq!(t.root.extra["tombstones"], "20", "{name}");
        } else if name.contains("_01_pk_lookup") || name.contains("_05_count") {
            assert!(!flagged, "{name}: {:?}", t.root.extra.get("warning"));
        }
    }
}
