//! Oracle plans as a JSON array of `PLAN_TABLE` / `V$SQL_PLAN_STATISTICS_ALL` rows.

use std::collections::BTreeMap;

use dbstudio_core::PlanFormat;

use crate::build::{finish, join_conditions};
use crate::json::{self, Js};
use crate::{
    NodePredicate, PlanError, PlanNode, PlanNodeKind, PlanObject, PlanTotals, PlanTree,
    PredicateRole, RawRef,
};

/// Row columns copied to `extra` when present.
const EXTRA_COLUMNS: &[&str] = &[
    "BYTES",
    "CPU_COST",
    "IO_COST",
    "TEMP_SPACE",
    "PARTITION_START",
    "PARTITION_STOP",
    "OBJECT_TYPE",
    "OPTIMIZER",
    "STARTS",
    "LAST_STARTS",
    "LAST_CR_BUFFER_GETS",
    "LAST_DISK_READS",
    "LAST_MEMORY_USED",
    "LAST_EXECUTION",
    "LAST_TEMPSEG_SIZE",
    "TEMPSEG_SIZE",
    "ESTIMATED_OPTIMAL_SIZE",
    "ESTIMATED_ONEPASS_SIZE",
    "OBJECT_ALIAS",
    "QBLOCK_NAME",
];

struct Row<'a> {
    id: u32,
    parent: Option<u32>,
    position: f64,
    j: &'a Js,
}

pub(crate) fn parse(text: &str) -> Result<PlanTree, PlanError> {
    let doc = json::parse(text).map_err(PlanError::Invalid)?;
    let list = match (&doc.val, doc.get("rows")) {
        (_, Some(rows)) => rows,
        (json::JVal::Arr(_), None) => &doc,
        _ => {
            return Err(PlanError::Invalid(
                "an Oracle plan must be a JSON array of plan rows".into(),
            ));
        }
    };
    let mut rows: Vec<Row> = Vec::new();
    for j in list.items() {
        let id = j
            .get_ci("ID")
            .and_then(Js::as_f64)
            .ok_or_else(|| PlanError::Invalid("a plan row has no ID".into()))?;
        rows.push(Row {
            id: id as u32,
            parent: j.get_ci("PARENT_ID").and_then(Js::as_f64).map(|p| p as u32),
            position: j.get_ci("POSITION").and_then(Js::as_f64).unwrap_or(0.0),
            j,
        });
    }
    if rows.is_empty() {
        return Err(PlanError::Invalid("the Oracle plan has no rows".into()));
    }
    let ids: BTreeMap<u32, usize> = rows.iter().enumerate().map(|(i, r)| (r.id, i)).collect();
    // Children per parent, ordered by POSITION then ID.
    let mut kids: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
    let mut roots: Vec<usize> = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        match r.parent.filter(|p| ids.contains_key(p) && *p != r.id) {
            Some(p) => kids.entry(p).or_default().push(i),
            None => roots.push(i),
        }
    }
    for v in kids.values_mut() {
        v.sort_by(|a, b| {
            rows[*a]
                .position
                .partial_cmp(&rows[*b].position)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(rows[*a].id.cmp(&rows[*b].id))
        });
    }
    roots.sort_by_key(|i| rows[*i].id);
    let mut seen = vec![false; rows.len()];
    let nodes: Vec<PlanNode> = roots
        .iter()
        .map(|i| build(*i, &rows, &ids, &kids, &mut seen, 0))
        .collect::<Result<_, _>>()?;
    let analyzed = nodes.iter().any(any_actual);
    let totals = PlanTotals {
        execution_time_ms: nodes
            .first()
            .and_then(|n| n.actual_time_ms)
            .filter(|_| analyzed),
        ..PlanTotals::default()
    };
    Ok(finish(PlanFormat::OraclePlanRows, analyzed, nodes, totals))
}

fn any_actual(n: &PlanNode) -> bool {
    n.loops.is_some() || n.children.iter().any(any_actual)
}

fn build(
    i: usize,
    rows: &[Row],
    ids: &BTreeMap<u32, usize>,
    kids: &BTreeMap<u32, Vec<usize>>,
    seen: &mut [bool],
    depth: u32,
) -> Result<PlanNode, PlanError> {
    if depth > crate::json::MAX_DEPTH {
        return Err(PlanError::Invalid(format!(
            "the Oracle plan is nested deeper than {} levels",
            crate::json::MAX_DEPTH
        )));
    }
    seen[i] = true;
    let r = &rows[i];
    let j = r.j;
    let operation = j.get_ci("OPERATION").and_then(Js::as_str).unwrap_or("?");
    let options = j
        .get_ci("OPTIONS")
        .and_then(Js::as_str)
        .filter(|o| !o.is_empty());
    let op = match options {
        Some(o) => format!("{operation} {o}"),
        None => operation.to_string(),
    };
    let kind = kind_of(operation, options);
    let mut n = PlanNode::new(op, kind);
    n.raw = RawRef::Row { id: r.id };
    let object_name = j
        .get_ci("OBJECT_NAME")
        .and_then(Js::as_str)
        .filter(|s| !s.is_empty());
    let owner = j
        .get_ci("OBJECT_OWNER")
        .and_then(Js::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if let Some(name) = object_name {
        let is_index = operation.starts_with("INDEX") || operation.contains("BITMAP INDEX");
        if is_index {
            // The row names the index; the table is the alias (`ORDERS@SEL$1`), unless the
            // `TABLE ACCESS` row above it (same alias) names the real table.
            let alias = j
                .get_ci("OBJECT_ALIAS")
                .and_then(Js::as_str)
                .filter(|a| !a.is_empty());
            let alias_name = alias
                .and_then(|a| a.split('@').next())
                .filter(|t| !t.is_empty());
            let parent = r
                .parent
                .and_then(|p| ids.get(&p))
                .map(|p| rows[*p].j)
                .filter(|p| {
                    p.get_ci("OPERATION").and_then(Js::as_str) == Some("TABLE ACCESS")
                        && p.get_ci("OBJECT_ALIAS").and_then(Js::as_str) == alias
                });
            let parent_table = parent
                .and_then(|p| p.get_ci("OBJECT_NAME").and_then(Js::as_str))
                .filter(|t| !t.is_empty());
            let table = parent_table.or(alias_name).unwrap_or(name);
            let table = table.trim_matches('"').to_string();
            let owner = owner.or_else(|| {
                parent
                    .and_then(|p| p.get_ci("OBJECT_OWNER").and_then(Js::as_str))
                    .filter(|o| !o.is_empty())
                    .map(str::to_string)
            });
            n.object = Some(PlanObject {
                schema: owner,
                alias: alias_name
                    .filter(|a| parent_table.is_some() && !a.eq_ignore_ascii_case(&table))
                    .map(str::to_string),
                name: table,
                index: Some(name.to_string()),
            });
        } else {
            n.object = Some(PlanObject {
                schema: owner,
                name: name.to_string(),
                alias: j
                    .get_ci("OBJECT_ALIAS")
                    .and_then(Js::as_str)
                    .and_then(|a| a.split('@').next())
                    .filter(|a| !a.is_empty() && *a != name)
                    .map(str::to_string),
                index: None,
            });
        }
    }
    let access_text = j
        .get_ci("ACCESS_PREDICATES")
        .and_then(Js::as_str)
        .filter(|s| !s.trim().is_empty());
    let filter_text = j
        .get_ci("FILTER_PREDICATES")
        .and_then(Js::as_str)
        .filter(|s| !s.trim().is_empty());
    // Access predicates of a join are its join condition, of an index operation the seek keys.
    let joins = matches!(
        kind,
        PlanNodeKind::NestedLoopJoin | PlanNodeKind::HashJoin | PlanNodeKind::MergeJoin
    ) || (operation == "SORT" && options.is_some_and(|o| o.contains("JOIN")));
    if let Some(t) = access_text {
        n.predicates.push(NodePredicate {
            role: if joins {
                PredicateRole::JoinCond
            } else {
                PredicateRole::IndexCond
            },
            text: t.to_string(),
        });
    }
    if let Some(t) = filter_text {
        n.predicates.push(NodePredicate {
            role: if joins {
                PredicateRole::JoinFilter
            } else {
                PredicateRole::Filter
            },
            text: t.to_string(),
        });
    }
    let access = access_text.map(|s| format!("access: {s}"));
    let filter = filter_text.map(|s| format!("filter: {s}"));
    n.condition = join_conditions(access.into_iter().chain(filter).collect());
    n.est_cost = j.get_ci("COST").and_then(Js::as_f64);
    n.est_rows = j.get_ci("CARDINALITY").and_then(Js::as_f64);
    let starts = j
        .get_ci("LAST_STARTS")
        .and_then(Js::as_f64)
        .or_else(|| j.get_ci("STARTS").and_then(Js::as_f64));
    let out = j
        .get_ci("LAST_OUTPUT_ROWS")
        .and_then(Js::as_f64)
        .or_else(|| j.get_ci("OUTPUT_ROWS").and_then(Js::as_f64));
    if let (Some(starts), Some(out)) = (starts, out) {
        n.set_actual_total(out, starts.max(0.0) as u64);
    }
    // Microseconds, inclusive of the children.
    n.actual_time_ms = j
        .get_ci("LAST_ELAPSED_TIME")
        .and_then(Js::as_f64)
        .or_else(|| j.get_ci("ELAPSED_TIME").and_then(Js::as_f64))
        .map(|us| us / 1000.0);
    for col in EXTRA_COLUMNS {
        if let Some(v) = j.get_ci(col) {
            if !matches!(v.val, json::JVal::Null) {
                n.put(col, v.display());
            }
        }
    }
    if let Some(children) = kids.get(&r.id) {
        for c in children {
            if !seen[*c] {
                let child = build(*c, rows, ids, kids, seen, depth + 1)?;
                n.children.push(child);
            }
        }
    }
    Ok(n)
}

fn kind_of(operation: &str, options: Option<&str>) -> PlanNodeKind {
    use PlanNodeKind::*;
    let opts = options.unwrap_or("");
    match operation {
        "TABLE ACCESS" => {
            if opts.starts_with("FULL") || opts.contains("SAMPLE") {
                FullScan
            } else if opts.contains("BY INDEX ROWID") || opts.contains("BY USER ROWID") {
                KeyLookup
            } else {
                Other
            }
        }
        "INDEX" | "BITMAP INDEX" => {
            if opts.contains("UNIQUE") {
                IndexSeek
            } else if opts.contains("FAST FULL") {
                IndexOnlyScan
            } else {
                IndexScan
            }
        }
        "NESTED LOOPS" => NestedLoopJoin,
        "HASH JOIN" => HashJoin,
        "MERGE JOIN" | "SORT MERGE JOIN" => MergeJoin,
        "SORT" => {
            if opts.contains("AGGREGATE") || opts.contains("GROUP BY") {
                Aggregate
            } else {
                Sort
            }
        }
        "HASH" => HashAggregate,
        "WINDOW" => Window,
        "COUNT" => Limit,
        "FILTER" => Filter,
        "VIEW" => Subquery,
        "UNION-ALL" | "UNION ALL" | "CONCATENATION" | "MINUS" | "INTERSECTION" | "UNION" => Union,
        "PX COORDINATOR" => Gather,
        "BUFFER" | "TEMP TABLE TRANSFORMATION" => Materialize,
        "SELECT STATEMENT" | "FAST DUAL" => Result,
        "UPDATE STATEMENT"
        | "INSERT STATEMENT"
        | "DELETE STATEMENT"
        | "MERGE STATEMENT"
        | "UPDATE"
        | "INSERT"
        | "DELETE"
        | "MERGE"
        | "LOAD TABLE CONVENTIONAL" => Modify,
        _ => Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_tree_from_parent_ids() {
        let t = parse(
            r#"[
          {"ID":0,"OPERATION":"SELECT STATEMENT","COST":10,"CARDINALITY":5},
          {"ID":1,"PARENT_ID":0,"POSITION":1,"OPERATION":"HASH JOIN","COST":10,"CARDINALITY":5,
           "LAST_STARTS":1,"LAST_OUTPUT_ROWS":5,"LAST_ELAPSED_TIME":2500},
          {"ID":3,"PARENT_ID":1,"POSITION":2,"OPERATION":"INDEX","OPTIONS":"RANGE SCAN","OBJECT_NAME":"I1","OBJECT_ALIAS":"T2@SEL$1"},
          {"ID":2,"PARENT_ID":1,"POSITION":1,"OPERATION":"TABLE ACCESS","OPTIONS":"FULL","OBJECT_NAME":"T1","TEMPSEG_SIZE":1024}
        ]"#,
        )
        .unwrap();
        assert_eq!(t.node_count, 4);
        assert!(t.analyzed);
        let join = &t.root.children[0];
        assert_eq!(join.actual_time_ms, Some(2.5));
        assert_eq!(join.children[0].object.as_ref().unwrap().name, "T1");
        let idx = join.children[1].object.as_ref().unwrap();
        assert_eq!(
            (idx.name.as_str(), idx.index.as_deref()),
            ("T2", Some("I1"))
        );
    }

    #[test]
    fn errors() {
        assert!(parse("{}").is_err());
        assert!(parse("[]").is_err());
        assert!(parse("[{\"OPERATION\":\"x\"}]").is_err());
    }
}
