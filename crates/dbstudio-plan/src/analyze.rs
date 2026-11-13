//! Post-parse analysis: self cost/time, hot path and warning rules (PLAN-04).

use std::collections::BTreeMap;

use dbstudio_core::PlanFormat;

use crate::json::fmt_num;
use crate::{PlanNode, PlanNodeKind, PlanRule, PlanRules, PlanTree, PlanWarning, Severity};

/// Fill warnings, hot path and self cost/time. Safe to call again (results are replaced).
pub fn analyze(tree: &mut PlanTree, rules: &PlanRules) {
    fill_self(&mut tree.root);
    let mut warnings = Vec::new();
    collect_warnings(&tree.root, tree.format, rules, &mut warnings);
    warnings.sort_by_key(|w| (w.node_id, rule_order(w.rule)));
    tree.warnings = warnings;
    tree.hot_path = hot_path(&tree.root);
}

fn rule_order(r: PlanRule) -> u8 {
    match r {
        PlanRule::FullScanLargeTable => 0,
        PlanRule::Misestimate => 1,
        PlanRule::SpillToDisk => 2,
        PlanRule::NestedLoopManyRows => 3,
    }
}

fn fill_self(node: &mut PlanNode) {
    for c in &mut node.children {
        fill_self(c);
    }
    node.self_cost = node.est_cost.map(|total| {
        let kids: f64 = node.children.iter().filter_map(|c| c.est_cost).sum();
        (total - kids).max(0.0)
    });
    node.self_time_ms = node.actual_time_ms.map(|total| {
        let kids: f64 = node.children.iter().filter_map(|c| c.actual_time_ms).sum();
        (total - kids).max(0.0)
    });
}

/// Ranking value of a node for the hot path: inclusive time when the plan has times, else
/// inclusive cost, else estimated rows.
fn weight(node: &PlanNode, has_time: bool, has_cost: bool) -> f64 {
    let v = if has_time {
        node.actual_time_ms
    } else if has_cost {
        node.est_cost
    } else {
        node.est_rows
    };
    v.unwrap_or(f64::NEG_INFINITY)
}

fn any(node: &PlanNode, f: &impl Fn(&PlanNode) -> bool) -> bool {
    f(node) || node.children.iter().any(|c| any(c, f))
}

fn hot_path(root: &PlanNode) -> Vec<u32> {
    let has_time = any(root, &|n| n.actual_time_ms.is_some());
    let has_cost = any(root, &|n| n.est_cost.is_some());
    let mut path = vec![root.id];
    let mut cur = root;
    while let Some(next) = cur
        .children
        .iter()
        .fold(None::<&PlanNode>, |best, c| match best {
            Some(b) if weight(b, has_time, has_cost) >= weight(c, has_time, has_cost) => Some(b),
            _ => Some(c),
        })
    {
        path.push(next.id);
        cur = next;
    }
    path
}

fn collect_warnings(
    node: &PlanNode,
    format: PlanFormat,
    rules: &PlanRules,
    out: &mut Vec<PlanWarning>,
) {
    full_scan(node, rules, out);
    misestimate(node, rules, out);
    spill(node, format, rules, out);
    nested_loop(node, rules, out);
    for c in &node.children {
        collect_warnings(c, format, rules, out);
    }
}

fn warn(
    node: &PlanNode,
    rule: PlanRule,
    severity: Severity,
    params: Vec<(&str, String)>,
) -> PlanWarning {
    PlanWarning {
        node_id: node.id,
        rule,
        severity,
        params: params
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect::<BTreeMap<_, _>>(),
    }
}

fn never_ran(node: &PlanNode) -> bool {
    node.loops == Some(0)
}

fn full_scan(node: &PlanNode, rules: &PlanRules, out: &mut Vec<PlanWarning>) {
    if node.kind != PlanNodeKind::FullScan || never_ran(node) {
        return;
    }
    // MongoDB reports the documents examined, MySQL JSON the rows examined per scan (the
    // output estimate is already reduced by the filter).
    let examined = ["docsExamined", "rows_examined_per_scan"]
        .iter()
        .filter_map(|k| node.extra.get(*k).and_then(|v| v.parse::<f64>().ok()))
        .fold(None, |a: Option<f64>, v| Some(a.map_or(v, |a| a.max(v))));
    let rows = [node.actual_rows_total, node.est_rows, examined]
        .into_iter()
        .flatten()
        .fold(f64::NEG_INFINITY, f64::max);
    if rows >= rules.large_table_rows && rows.is_finite() {
        let severity = if rows >= rules.large_table_rows * 10.0 {
            Severity::Critical
        } else {
            Severity::Warning
        };
        let mut params = vec![("rows", fmt_num(rows))];
        if let Some(o) = &node.object {
            params.push(("table", o.name.clone()));
        }
        out.push(warn(node, PlanRule::FullScanLargeTable, severity, params));
    }
}

fn misestimate(node: &PlanNode, rules: &PlanRules, out: &mut Vec<PlanWarning>) {
    if never_ran(node) {
        return;
    }
    let (Some(actual), Some(est)) = (node.actual_rows, node.est_rows) else {
        return;
    };
    let hi = actual.max(est);
    let lo = actual.min(est).max(1.0);
    let factor = hi / lo;
    if factor >= rules.misestimate_factor && hi >= rules.misestimate_min_rows {
        let severity = if factor >= 100.0 {
            Severity::Critical
        } else {
            Severity::Warning
        };
        out.push(warn(
            node,
            PlanRule::Misestimate,
            severity,
            vec![
                ("factor", fmt_num((factor * 10.0).round() / 10.0)),
                ("estimated", fmt_num(est)),
                ("actual", fmt_num(actual)),
                (
                    "direction",
                    if actual > est { "under" } else { "over" }.to_string(),
                ),
            ],
        ));
    }
}

fn num(node: &PlanNode, key: &str) -> Option<f64> {
    node.extra.get(key).and_then(|v| v.parse::<f64>().ok())
}

/// Returns `(severity, detail)` when the node's source attributes show a spill.
fn spill_signal(
    node: &PlanNode,
    format: PlanFormat,
    rules: &PlanRules,
) -> Option<(Severity, String)> {
    let ex = &node.extra;
    match format {
        PlanFormat::PostgresJson => {
            if let Some(m) = ex.get("Sort Method").filter(|m| m.starts_with("external")) {
                let disk = num(node, "Sort Space Used").map(|k| format!(" {}kB", fmt_num(k)));
                return Some((
                    Severity::Warning,
                    format!("{m}{}", disk.unwrap_or_default()),
                ));
            }
            if ex.get("Sort Space Type").map(String::as_str) == Some("Disk") {
                return Some((Severity::Warning, "Disk".into()));
            }
            if num(node, "Hash Batches").is_some_and(|b| b > 1.0) {
                return Some((
                    Severity::Warning,
                    format!(
                        "batches {}",
                        ex.get("Hash Batches").cloned().unwrap_or_default()
                    ),
                ));
            }
            if num(node, "HashAgg Batches").is_some_and(|b| b > 1.0)
                || num(node, "Disk Usage").is_some_and(|b| b > 0.0)
            {
                return Some((Severity::Warning, "hash aggregate".into()));
            }
            None
        }
        PlanFormat::MsSqlShowplanXml => ex.get("spill").map(|d| (Severity::Warning, d.clone())),
        PlanFormat::OraclePlanRows => {
            if num(node, "LAST_TEMPSEG_SIZE").is_some_and(|b| b > 0.0)
                || num(node, "TEMPSEG_SIZE").is_some_and(|b| b > 0.0)
            {
                let size = num(node, "LAST_TEMPSEG_SIZE")
                    .or_else(|| num(node, "TEMPSEG_SIZE"))
                    .unwrap_or(0.0);
                return Some((
                    Severity::Warning,
                    format!("tempseg {} bytes", fmt_num(size)),
                ));
            }
            ex.get("LAST_EXECUTION")
                .filter(|e| {
                    let e = e.to_ascii_uppercase();
                    e.contains("PASS")
                })
                .map(|e| (Severity::Warning, e.clone()))
        }
        PlanFormat::MongoExplain => (ex.get("usedDisk").map(String::as_str) == Some("true"))
            .then(|| (Severity::Warning, "usedDisk".to_string())),
        PlanFormat::MariaDbJson => {
            // A filesort that could not use the priority queue and produced many rows.
            let no_queue = ex.get("r_used_priority_queue").map(String::as_str) == Some("false");
            let rows = num(node, "r_output_rows").unwrap_or(0.0);
            (no_queue && rows >= rules.large_table_rows / 10.0)
                .then(|| (Severity::Info, format!("filesort {} rows", fmt_num(rows))))
        }
        PlanFormat::MySqlJson | PlanFormat::MySqlJsonV2 | PlanFormat::MySqlTree => None,
    }
}

fn spill(node: &PlanNode, format: PlanFormat, rules: &PlanRules, out: &mut Vec<PlanWarning>) {
    if let Some((severity, detail)) = spill_signal(node, format, rules) {
        out.push(warn(
            node,
            PlanRule::SpillToDisk,
            severity,
            vec![("detail", detail)],
        ));
    }
}

fn nested_loop(node: &PlanNode, rules: &PlanRules, out: &mut Vec<PlanWarning>) {
    if node.kind != PlanNodeKind::NestedLoopJoin {
        return;
    }
    // The inner side is the second child; fall back to the busiest child.
    let inner = node
        .children
        .get(1)
        .and_then(|c| c.loops)
        .or_else(|| node.children.iter().filter_map(|c| c.loops).max());
    let Some(loops) = inner else { return };
    let loops = loops as f64;
    if loops >= rules.nested_loop_rows {
        let severity = if loops >= rules.nested_loop_rows * 100.0 {
            Severity::Critical
        } else {
            Severity::Warning
        };
        out.push(warn(
            node,
            PlanRule::NestedLoopManyRows,
            severity,
            vec![("loops", fmt_num(loops))],
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: u32, kind: PlanNodeKind) -> PlanNode {
        let mut n = PlanNode::new("x", kind);
        n.id = id;
        n
    }

    #[test]
    fn self_values_and_hot_path_by_time() {
        let mut root = node(0, PlanNodeKind::Sort);
        root.actual_time_ms = Some(10.0);
        let mut a = node(1, PlanNodeKind::FullScan);
        a.actual_time_ms = Some(2.0);
        let mut b = node(2, PlanNodeKind::HashJoin);
        b.actual_time_ms = Some(7.0);
        let mut c = node(3, PlanNodeKind::FullScan);
        c.actual_time_ms = Some(5.0);
        b.children.push(c);
        root.children = vec![a, b];
        fill_self(&mut root);
        assert_eq!(root.self_time_ms, Some(1.0));
        assert_eq!(root.children[1].self_time_ms, Some(2.0));
        assert_eq!(hot_path(&root), vec![0, 2, 3]);
    }

    #[test]
    fn hot_path_falls_back_to_cost() {
        let mut root = node(0, PlanNodeKind::Sort);
        root.est_cost = Some(100.0);
        let mut a = node(1, PlanNodeKind::FullScan);
        a.est_cost = Some(80.0);
        let mut b = node(2, PlanNodeKind::FullScan);
        b.est_cost = Some(10.0);
        root.children = vec![a, b];
        assert_eq!(hot_path(&root), vec![0, 1]);
    }

    #[test]
    fn misestimate_thresholds() {
        let rules = PlanRules::default();
        let mut n = node(1, PlanNodeKind::IndexScan);
        n.est_rows = Some(100.0);
        n.actual_rows = Some(1000.0);
        let mut out = vec![];
        misestimate(&n, &rules, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].severity, Severity::Warning);
        assert_eq!(out[0].params["direction"], "under");
        // 9x is below the factor, tiny row counts are ignored.
        n.actual_rows = Some(900.0);
        out.clear();
        misestimate(&n, &rules, &mut out);
        assert!(out.is_empty());
        n.est_rows = Some(1.0);
        n.actual_rows = Some(50.0);
        misestimate(&n, &rules, &mut out);
        assert!(out.is_empty());
        // 1000x is critical.
        n.est_rows = Some(1000.0);
        n.actual_rows = Some(1.0);
        misestimate(&n, &rules, &mut out);
        assert_eq!(out[0].severity, Severity::Critical);
        assert_eq!(out[0].params["direction"], "over");
    }

    #[test]
    fn full_scan_and_nested_loop_rules() {
        let rules = PlanRules::default();
        let mut scan = node(1, PlanNodeKind::FullScan);
        scan.est_rows = Some(99_999.0);
        let mut out = vec![];
        full_scan(&scan, &rules, &mut out);
        assert!(out.is_empty());
        scan.actual_rows_total = Some(150_000.0);
        full_scan(&scan, &rules, &mut out);
        assert_eq!(out.len(), 1);

        let mut nl = node(0, PlanNodeKind::NestedLoopJoin);
        let outer = node(1, PlanNodeKind::FullScan);
        let mut inner = node(2, PlanNodeKind::IndexSeek);
        inner.loops = Some(10_000);
        nl.children = vec![outer, inner];
        out.clear();
        nested_loop(&nl, &rules, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].params["loops"], "10000");
        nl.children[1].loops = Some(9_999);
        out.clear();
        nested_loop(&nl, &rules, &mut out);
        assert!(out.is_empty());
    }
}
