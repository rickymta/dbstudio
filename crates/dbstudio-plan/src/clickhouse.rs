//! Parser for the ClickHouse `EXPLAIN json = 1, indexes = 1, actions = 1 PLAN` output.
//!
//! The plan is a tree of `"Node Type"` / `"Description"` / `"Plans"` objects whose root is the
//! last step of the pipeline (the leaves read the tables). Read nodes (`ReadFromMergeTree`)
//! carry the data skipping stages in `"Indexes"` (`Initial` / `Selected Parts` and `Granules`).
//! ClickHouse reports no cost and no actual statistics: the only estimate is the number of
//! granules a read selects, converted to rows with the default granularity of 8192 rows.
//!
//! A read whose index stages select every granule is a [`PlanNodeKind::FullScan`]; with a large
//! enough granule count the generic analyser then raises `FullScanLargeTable`.

use dbstudio_core::PlanFormat;

use crate::build::{finish, flatten_scalars, join_conditions, object};
use crate::json::{self, Js};
use crate::{PlanError, PlanNode, PlanNodeKind, PlanObject, PlanTotals, PlanTree};

/// Rows per granule (the default `index_granularity`; the plan does not tell the real value).
const GRANULE_ROWS: f64 = 8192.0;

pub(crate) fn parse(text: &str) -> Result<PlanTree, PlanError> {
    let doc = json::parse(text).map_err(PlanError::Invalid)?;
    // `EXPLAIN json = 1` prints `[ {"Plan": {...}} ]`.
    let entry = match &doc.val {
        json::JVal::Arr(items) => items.first(),
        _ => Some(&doc),
    };
    let plan = entry
        .and_then(|e| e.get("Plan"))
        .filter(|p| p.is_obj())
        .ok_or_else(|| PlanError::Invalid("no \"Plan\" object in the ClickHouse plan".into()))?;
    let root = node(plan);
    Ok(finish(
        PlanFormat::ClickHouseJson,
        false,
        vec![root],
        PlanTotals::default(),
    ))
}

fn node(js: &Js) -> PlanNode {
    let node_type = js.text("Node Type").unwrap_or("?");
    let desc = js.text("Description").unwrap_or("");
    let mut n = if node_type.starts_with("ReadFrom") {
        read_node(js, node_type, desc)
    } else {
        operator(js, node_type, desc)
    };
    n.lines(js);
    if let Some(id) = js.text("Node Id") {
        n.put("node_id", id);
    }
    if !desc.is_empty() && n.object.is_none() {
        n.put("description", desc);
    }
    n.children = js
        .get("Plans")
        .map(|p| p.items().iter().map(node).collect())
        .unwrap_or_default();
    n
}

fn operator(js: &Js, node_type: &str, desc: &str) -> PlanNode {
    let (kind, condition) = match node_type {
        "Expression" => {
            let kind = if desc.contains("WHERE") || desc.contains("PREWHERE") || desc.contains("HAVING")
            {
                PlanNodeKind::Filter
            } else {
                PlanNodeKind::Other
            };
            (kind, None)
        }
        "Filter" => (PlanNodeKind::Filter, js.text("Filter Column").map(str::to_string)),
        "Aggregating" | "MergingAggregated" => {
            let keys: Vec<String> = js
                .get("Keys")
                .map(|k| k.items().iter().map(Js::display).collect())
                .unwrap_or_default();
            let kind = if keys.is_empty() {
                PlanNodeKind::Aggregate
            } else {
                PlanNodeKind::HashAggregate
            };
            ((kind), (!keys.is_empty()).then(|| format!("GROUP BY {}", keys.join(", "))))
        }
        "Distinct" | "PreDistinct" => (PlanNodeKind::Aggregate, None),
        "Sorting" | "MergeSorting" | "PartialSorting" | "FinishSorting" | "MergingSorted" => {
            let order: Vec<String> = js
                .get("Sort Description")
                .map(|d| {
                    d.items()
                        .iter()
                        .map(|k| {
                            format!(
                                "{} {}",
                                k.text("Column").unwrap_or("?"),
                                if k.get("Ascending").and_then(Js::as_bool) == Some(false) {
                                    "DESC"
                                } else {
                                    "ASC"
                                }
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            (PlanNodeKind::Sort, (!order.is_empty()).then(|| order.join(", ")))
        }
        "Limit" | "Offset" | "LimitBy" => {
            let mut parts = Vec::new();
            if let Some(l) = js.num("Limit") {
                parts.push(format!("LIMIT {}", json::fmt_num(l)));
            }
            if let Some(o) = js.num("Offset").filter(|o| *o > 0.0) {
                parts.push(format!("OFFSET {}", json::fmt_num(o)));
            }
            (PlanNodeKind::Limit, (!parts.is_empty()).then(|| parts.join(" ")))
        }
        "Join" | "FilledJoin" => {
            let algo = js.text("Algorithm").unwrap_or("");
            let kind = if algo.contains("Merge") {
                PlanNodeKind::MergeJoin
            } else if algo.contains("Loop") {
                PlanNodeKind::NestedLoopJoin
            } else {
                PlanNodeKind::HashJoin
            };
            (kind, js.text("Clauses").map(str::to_string))
        }
        "Union" => (PlanNodeKind::Union, None),
        "CreatingSets" | "CreatingSet" => (PlanNodeKind::Subquery, None),
        "Window" => (PlanNodeKind::Window, None),
        "Materializing" => (PlanNodeKind::Materialize, None),
        "Totals" | "Rollup" | "Cube" | "ArrayJoin" | "BuildRuntimeFilter" | "Extremes"
        | "Fill" | "Interpolate" | "Watermark" => (PlanNodeKind::Other, None),
        "Insert" | "Delete" | "Update" => (PlanNodeKind::Modify, None),
        _ => (PlanNodeKind::Other, None),
    };
    let mut n = PlanNode::new(node_type, kind);
    n.condition = condition;
    flatten_scalars(
        &mut n,
        js,
        &["Node Type", "Node Id", "Description", "Filter Column", "Clauses", "Limit", "Offset"],
        "",
    );
    if node_type == "Aggregating" || node_type == "MergingAggregated" {
        if let Some(aggs) = js.get("Aggregates") {
            let names: Vec<String> = aggs
                .items()
                .iter()
                .filter_map(|a| a.text("Name").map(str::to_string))
                .collect();
            n.put("aggregates", names.join(", "));
        }
    }
    n
}

/// `db.table` split into an object (names containing dots are rare; the first dot splits).
fn object_of(desc: &str) -> Option<PlanObject> {
    let name = desc.trim();
    if name.is_empty() {
        return None;
    }
    Some(match name.split_once('.') {
        Some((db, table)) if !db.is_empty() && !table.is_empty() => PlanObject {
            schema: Some(db.to_string()),
            ..object(table)
        },
        _ => object(name),
    })
}

fn read_node(js: &Js, node_type: &str, desc: &str) -> PlanNode {
    let stages: Vec<&Js> = js.get("Indexes").map(|i| i.items().iter().collect()).unwrap_or_default();
    let initial = stages.first().and_then(|s| s.num("Initial Granules"));
    let selected = stages.last().and_then(|s| s.num("Selected Granules"));
    // The index that narrowed the read the most (a skipping index by its name).
    let narrowing = stages.iter().rev().find(|s| {
        s.num("Selected Granules").zip(s.num("Initial Granules")).is_some_and(|(sel, ini)| sel < ini)
    });
    let kind = match (initial, selected) {
        (Some(i), Some(s)) if s < i => PlanNodeKind::IndexScan,
        (Some(_), Some(_)) => PlanNodeKind::FullScan,
        _ if node_type == "ReadFromMergeTree" => PlanNodeKind::Other,
        _ => PlanNodeKind::FullScan,
    };
    let mut n = PlanNode::new(node_type, kind);
    n.object = object_of(desc);
    if let (Some(obj), Some(s)) = (n.object.as_mut(), narrowing) {
        obj.index = Some(match (s.text("Type"), s.text("Name")) {
            (Some("Skip"), Some(name)) => name.to_string(),
            (Some("PrimaryKey"), _) => "PRIMARY KEY".to_string(),
            (Some(t), _) => t.to_string(),
            _ => "index".to_string(),
        });
    }
    let granules = selected.or_else(|| js.num("Granules"));
    n.est_rows = granules.map(|g| g * GRANULE_ROWS);
    // Prewhere filter and the primary key condition.
    let mut conditions = Vec::new();
    if let Some(c) = js
        .get("Prewhere info")
        .and_then(|p| p.get("Prewhere filter"))
        .and_then(|f| f.text("Prewhere filter column"))
    {
        conditions.push(format!("PREWHERE {c}"));
    }
    for s in &stages {
        if s.text("Type") == Some("PrimaryKey") {
            if let Some(c) = s.text("Condition").filter(|c| *c != "true") {
                conditions.push(format!("PRIMARY KEY {c}"));
            }
        }
    }
    n.condition = join_conditions(conditions);
    flatten_scalars(
        &mut n,
        js,
        &["Node Type", "Node Id", "Description"],
        "",
    );
    for s in &stages {
        let ty = s.text("Type").unwrap_or("?");
        let label = match s.text("Name") {
            Some(name) => format!("index {ty} {name}"),
            None => format!("index {ty}"),
        };
        let mut text = format!(
            "granules {}/{}, parts {}/{}",
            s.num("Selected Granules").map_or("?".into(), json::fmt_num),
            s.num("Initial Granules").map_or("?".into(), json::fmt_num),
            s.num("Selected Parts").map_or("?".into(), json::fmt_num),
            s.num("Initial Parts").map_or("?".into(), json::fmt_num),
        );
        if let Some(keys) = s.get("Keys").filter(|k| !k.items().is_empty()) {
            text.push_str(&format!(", keys {}", keys.items().iter().map(Js::display).collect::<Vec<_>>().join(", ")));
        }
        if let Some(c) = s.text("Condition") {
            text.push_str(&format!(", condition {c}"));
        }
        n.put(&label, text);
    }
    if let (Some(i), Some(s)) = (initial, selected) {
        n.put("granules_initial", json::fmt_num(i));
        n.put("granules_selected", json::fmt_num(s));
        n.put("rows_estimate_basis", "selected granules x 8192 rows");
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PlanRule, PlanRules, parse_and_analyze};
    use dbstudio_core::RawPlan;
    use std::collections::BTreeMap;

    fn raw(text: &str) -> RawPlan {
        RawPlan {
            format: PlanFormat::ClickHouseJson,
            text: text.to_string(),
            statement: String::new(),
            analyzed: false,
            rolled_back: false,
            server_version: "26.3".into(),
            captured_at: "2026-11-14T00:00:00Z".into(),
            extra: BTreeMap::new(),
        }
    }

    fn walk<'a>(n: &'a PlanNode, out: &mut Vec<&'a PlanNode>) {
        out.push(n);
        for c in &n.children {
            walk(c, out);
        }
    }

    #[test]
    fn minimal_read_with_index() {
        let text = r#"[{"Plan": {"Node Type": "Expression", "Description": "Project names",
          "Plans": [{"Node Type": "ReadFromMergeTree", "Description": "db.t", "Parts": 2, "Granules": 5,
            "Indexes": [{"Type": "PrimaryKey", "Keys": ["a"], "Condition": "(a in [1, 1])",
              "Initial Parts": 2, "Selected Parts": 1, "Initial Granules": 100, "Selected Granules": 5}]}]}}]"#;
        let tree = parse_and_analyze(&raw(text), &PlanRules::default()).unwrap();
        assert_eq!(tree.node_count, 2);
        let read = &tree.root.children[0];
        assert_eq!(read.kind, PlanNodeKind::IndexScan);
        assert_eq!(read.object.as_ref().unwrap().schema.as_deref(), Some("db"));
        assert_eq!(read.object.as_ref().unwrap().index.as_deref(), Some("PRIMARY KEY"));
        assert_eq!(read.est_rows, Some(5.0 * 8192.0));
        assert!(tree.warnings.is_empty());
    }

    #[test]
    fn full_scan_of_a_big_table_warns() {
        let text = r#"{"Plan": {"Node Type": "ReadFromMergeTree", "Description": "db.big",
            "Indexes": [{"Type": "PrimaryKey", "Condition": "true",
              "Initial Parts": 3, "Selected Parts": 3, "Initial Granules": 400, "Selected Granules": 400}]}}"#;
        let tree = parse_and_analyze(&raw(text), &PlanRules::default()).unwrap();
        assert_eq!(tree.root.kind, PlanNodeKind::FullScan);
        assert_eq!(tree.warnings.len(), 1);
        assert_eq!(tree.warnings[0].rule, PlanRule::FullScanLargeTable);
        // A small table does not warn.
        let small = text.replace("400", "2");
        let tree = parse_and_analyze(&raw(&small), &PlanRules::default()).unwrap();
        assert!(tree.warnings.is_empty());
    }

    #[test]
    fn operators_map_to_kinds() {
        let text = r#"[{"Plan": {"Node Type": "Limit", "Limit": 5, "Offset": 2,
          "Plans": [{"Node Type": "Sorting", "Sort Description": [{"Column": "n", "Ascending": false}],
          "Plans": [{"Node Type": "Aggregating", "Keys": ["k"], "Aggregates": [{"Name": "count()"}],
          "Plans": [{"Node Type": "Join", "Algorithm": "ConcurrentHashJoin", "Clauses": "[(a) = (b)]",
          "Plans": [{"Node Type": "ReadFromMemoryStorage", "Description": "t1"}, {"Node Type": "CreatingSets"}]}]}]}]}}]"#;
        let tree = crate::parse(&raw(text)).unwrap();
        let mut all = Vec::new();
        walk(&tree.root, &mut all);
        let kinds: Vec<PlanNodeKind> = all.iter().map(|n| n.kind).collect();
        assert_eq!(
            kinds,
            vec![
                PlanNodeKind::Limit,
                PlanNodeKind::Sort,
                PlanNodeKind::HashAggregate,
                PlanNodeKind::HashJoin,
                PlanNodeKind::FullScan,
                PlanNodeKind::Subquery
            ]
        );
        assert_eq!(all[0].condition.as_deref(), Some("LIMIT 5 OFFSET 2"));
        assert_eq!(all[1].condition.as_deref(), Some("n DESC"));
        assert_eq!(all[2].condition.as_deref(), Some("GROUP BY k"));
        assert_eq!(all[2].extra.get("aggregates").map(String::as_str), Some("count()"));
        assert_eq!(all[3].condition.as_deref(), Some("[(a) = (b)]"));
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(crate::parse(&raw("not json")).is_err());
        assert!(crate::parse(&raw("[]")).is_err());
        assert!(crate::parse(&raw("{\"x\": 1}")).is_err());
    }
}
