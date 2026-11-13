//! PostgreSQL `EXPLAIN (FORMAT JSON)`.

use dbstudio_core::PlanFormat;

use crate::build::{finish, flatten_scalars, join_conditions};
use crate::json::{self, Js};
use crate::{PlanError, PlanNode, PlanNodeKind, PlanObject, PlanTotals, PlanTree};

/// Members consumed explicitly (not copied to `extra`).
const CONSUMED: &[&str] = &[
    "Node Type",
    "Relation Name",
    "Schema",
    "Alias",
    "Index Name",
    "Startup Cost",
    "Total Cost",
    "Plan Rows",
    "Actual Rows",
    "Actual Loops",
    "Actual Total Time",
    "Actual Startup Time",
    "Plans",
    "Plan Width",
];

const CONDITIONS: &[&str] = &[
    "Index Cond",
    "Recheck Cond",
    "Hash Cond",
    "Merge Cond",
    "Join Filter",
    "Filter",
    "TID Cond",
    "One-Time Filter",
];

pub(crate) fn parse(text: &str) -> Result<PlanTree, PlanError> {
    let doc = json::parse(text).map_err(PlanError::Invalid)?;
    // `EXPLAIN (FORMAT JSON)` returns `[{"Plan": {...}, ...}]`; accept the bare object too.
    let top = match doc.items().first() {
        Some(first) => first,
        None => &doc,
    };
    let plan = top
        .get("Plan")
        .ok_or_else(|| PlanError::Invalid("no \"Plan\" member in the PostgreSQL plan".into()))?;
    let root = node(plan, 1.0);
    let analyzed = top.get("Execution Time").is_some() || any_actual(&root);
    let totals = PlanTotals {
        total_cost: root.est_cost,
        planning_time_ms: top.num("Planning Time"),
        execution_time_ms: top.num("Execution Time"),
        rows: None,
    };
    Ok(finish(
        PlanFormat::PostgresJson,
        analyzed,
        vec![root],
        totals,
    ))
}

fn any_actual(n: &PlanNode) -> bool {
    n.actual_time_ms.is_some() || n.children.iter().any(any_actual)
}

/// Number of processes that share the work below a `Gather`: the launched workers plus the
/// leader (when it takes part in the scan, i.e. a parallel-aware node ran in every process).
fn gather_factor(p: &Js) -> f64 {
    if p.get("Single Copy").and_then(Js::as_bool) == Some(true) {
        return 1.0;
    }
    let launched = p.num("Workers Launched").unwrap_or(0.0).max(0.0);
    let leader_runs_too = !parallel_aware_loops(p).is_some_and(|l| l == launched);
    if leader_runs_too {
        launched + 1.0
    } else {
        launched.max(1.0)
    }
}

/// `Actual Loops` of the first parallel-aware node below `p`.
fn parallel_aware_loops(p: &Js) -> Option<f64> {
    for c in p.get("Plans").map(Js::items).unwrap_or_default() {
        if c.get("Parallel Aware").and_then(Js::as_bool) == Some(true) {
            return c.num("Actual Loops");
        }
        if let Some(l) = parallel_aware_loops(c) {
            return Some(l);
        }
    }
    None
}

/// `factor` is the number of processes that ran this node concurrently (1 outside parallel
/// plans): their times overlap, so wall time = time summed over loops / factor.
fn node(p: &Js, factor: f64) -> PlanNode {
    let node_type = p.text("Node Type").unwrap_or("?");
    let strategy = p.text("Strategy");
    let kind = kind_of(node_type, strategy);
    let parallel = p.get("Parallel Aware").and_then(Js::as_bool) == Some(true);
    let op = if parallel && node_type != "Gather" && node_type != "Gather Merge" {
        format!("Parallel {node_type}")
    } else {
        node_type.to_string()
    };
    let mut n = PlanNode::new(op, kind);
    n.lines(p);
    if let Some(name) = p.text("Relation Name") {
        n.object = Some(PlanObject {
            schema: p.text("Schema").map(str::to_string),
            name: name.to_string(),
            alias: p.text("Alias").filter(|a| *a != name).map(str::to_string),
            index: p.text("Index Name").map(str::to_string),
        });
    } else if let Some(index) = p.text("Index Name") {
        n.object = Some(PlanObject {
            schema: None,
            name: p.text("Alias").unwrap_or(index).to_string(),
            alias: None,
            index: Some(index.to_string()),
        });
    } else if let Some(cte) = p.text("CTE Name") {
        n.object = Some(PlanObject {
            schema: None,
            name: cte.to_string(),
            alias: p.text("Alias").filter(|a| *a != cte).map(str::to_string),
            index: None,
        });
    }
    n.condition = join_conditions(
        CONDITIONS
            .iter()
            .filter_map(|k| p.text(k).map(|v| format!("{k}: {v}")))
            .collect(),
    );
    n.est_startup_cost = p.num("Startup Cost");
    n.est_cost = p.num("Total Cost");
    n.est_rows = p.num("Plan Rows");
    if let Some(rows) = p.num("Actual Rows") {
        let loops = p.num("Actual Loops").unwrap_or(1.0).max(0.0);
        n.set_actual(rows, loops as u64);
        if let Some(t) = p.num("Actual Total Time") {
            // PostgreSQL reports the time of one loop; the total over loops is what counts, but
            // loops run by parallel workers overlap in time.
            n.actual_time_ms = Some(t * loops / factor.max(1.0));
        }
    }
    flatten_scalars(&mut n, p, CONSUMED, "");
    for c in CONDITIONS {
        n.extra.remove(*c);
    }
    let child_factor = match node_type {
        "Gather" | "Gather Merge" => gather_factor(p),
        _ => factor,
    };
    n.children = p
        .get("Plans")
        .map(|plans| {
            plans
                .items()
                .iter()
                .map(|c| node(c, child_factor))
                .collect()
        })
        .unwrap_or_default();
    n
}

fn kind_of(node_type: &str, strategy: Option<&str>) -> PlanNodeKind {
    use PlanNodeKind::*;
    match node_type {
        "Seq Scan" | "Sample Scan" => FullScan,
        "Index Scan" | "Bitmap Heap Scan" | "Bitmap Index Scan" | "Tid Scan" | "Tid Range Scan" => {
            IndexScan
        }
        "Index Only Scan" => IndexOnlyScan,
        "Nested Loop" => NestedLoopJoin,
        "Hash Join" => HashJoin,
        "Merge Join" => MergeJoin,
        "Sort" | "Incremental Sort" => Sort,
        "Aggregate" => match strategy {
            Some("Hashed") => HashAggregate,
            _ => Aggregate,
        },
        "HashAggregate" => HashAggregate,
        "GroupAggregate" | "Group" | "Unique" | "SetOp" => Aggregate,
        "WindowAgg" => Window,
        "Limit" => PlanNodeKind::Limit,
        "Materialize" | "Memoize" => Materialize,
        "Subquery Scan" | "CTE Scan" | "WorkTable Scan" | "Named Tuplestore Scan" => Subquery,
        "Append" | "Merge Append" | "Recursive Union" | "BitmapAnd" | "BitmapOr" => Union,
        "Gather" | "Gather Merge" => Gather,
        "ModifyTable" | "Insert" | "Update" | "Delete" | "Merge" => Modify,
        "Result" | "ProjectSet" => PlanNodeKind::Result,
        _ => Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_plans() {
        assert!(parse("[]").is_err());
        assert!(parse("{\"x\":1}").is_err());
        assert!(parse("nope").is_err());
    }

    #[test]
    fn per_loop_time_is_multiplied() {
        let t = parse(
            r#"[{"Plan":{"Node Type":"Index Scan","Relation Name":"t","Total Cost":1.0,
            "Plan Rows":2,"Actual Rows":3,"Actual Loops":4,"Actual Total Time":0.5},
            "Execution Time":2.5}]"#,
        )
        .unwrap();
        assert!(t.analyzed);
        assert_eq!(t.root.actual_time_ms, Some(2.0));
        assert_eq!(t.root.actual_rows_total, Some(12.0));
        assert_eq!(t.totals.execution_time_ms, Some(2.5));
    }
}
