//! MariaDB `EXPLAIN FORMAT=JSON` and `ANALYZE FORMAT=JSON`.
//!
//! MariaDB writes several `"table"` members into one object for a join (duplicate keys), which
//! is why this module reads the text with the order-preserving reader of [`crate::json`].
//! Times of ANALYZE output are *exclusive* (`r_table_time_ms`, the time of one sort...); they
//! are converted to inclusive subtree times so all formats mean the same thing.

use dbstudio_core::PlanFormat;

use crate::build::{finish, flatten_scalars, object};
use crate::json::{self, Js};
use crate::mysql::{push_pred, split_top};
use crate::{PlanError, PlanNode, PlanNodeKind, PlanTotals, PlanTree, PredicateRole};

const BLOCK_TOTAL: &str = "block_total_ms";

pub(crate) fn parse(text: &str) -> Result<PlanTree, PlanError> {
    let doc = json::parse(text).map_err(PlanError::Invalid)?;
    let qb = doc
        .get("query_block")
        .filter(|q| q.is_obj())
        .ok_or_else(|| PlanError::Invalid("no \"query_block\" in the MariaDB plan".into()))?;
    let analyzed = text.contains("\"r_loops\"") || text.contains("\"r_total_time_ms\"");
    let mut root = block(qb);
    inclusive(&mut root, None);
    let totals = PlanTotals {
        execution_time_ms: qb.num("r_total_time_ms").filter(|_| analyzed),
        ..PlanTotals::default()
    };
    Ok(finish(
        PlanFormat::MariaDbJson,
        analyzed,
        vec![root],
        totals,
    ))
}

fn block(qb: &Js) -> PlanNode {
    let mut n = body(qb);
    if let Some(id) = qb.num("select_id") {
        n.put("select_id", json::fmt_num(id));
    }
    if let Some(op) = qb.text("operation") {
        n.put("operation", op);
    }
    if let Some(t) = qb.num("r_total_time_ms") {
        n.put(BLOCK_TOTAL, json::fmt_num(t));
    }
    if n.loops.is_none() {
        if let Some(l) = qb.num("r_loops") {
            n.loops = Some(l as u64);
        }
    }
    n
}

fn wrapper(op: &str, kind: PlanNodeKind, v: &Js) -> PlanNode {
    let mut n = PlanNode::new(op, kind);
    n.lines(v);
    flatten_scalars(&mut n, v, &["r_total_time_ms", "r_loops"], "");
    // Exclusive time of this step (inclusive time is computed later).
    n.actual_time_ms = v.num("r_total_time_ms");
    if let Some(l) = v.num("r_loops") {
        n.loops = Some(l as u64);
    }
    if let Some(r) = v.num("r_output_rows").or_else(|| v.num("r_rows")) {
        let loops = n.loops.unwrap_or(1);
        n.set_actual(r, loops);
    }
    for key in v.text("sort_key").map(split_top).unwrap_or_default() {
        push_pred(&mut n, PredicateRole::SortKey, Some(&key));
    }
    n.children.push(body(v));
    n
}

fn body(obj: &Js) -> PlanNode {
    let mut parts: Vec<PlanNode> = Vec::new();
    for (key, v) in obj.members() {
        match key.as_str() {
            "filesort" => parts.push(wrapper("Filesort", PlanNodeKind::Sort, v)),
            "temporary_table" => {
                parts.push(wrapper("Temporary table", PlanNodeKind::Materialize, v))
            }
            "duplicates_removal" => {
                parts.push(wrapper("Duplicate removal", PlanNodeKind::Aggregate, v))
            }
            "read_sorted_file" => parts.push(wrapper("Read sorted file", PlanNodeKind::Other, v)),
            "window_functions_computation" => {
                parts.push(wrapper("Window functions", PlanNodeKind::Window, v))
            }
            "buffer_result" => parts.push(wrapper("Buffer result", PlanNodeKind::Materialize, v)),
            "table" if v.is_obj() => parts.push(table_node(v, None)),
            "block-nl-join" if v.is_obj() => {
                if let Some(t) = v.get("table") {
                    parts.push(table_node(t, Some(v)));
                }
            }
            "nested_loop" => {
                for item in v.items() {
                    parts.push(body(item));
                }
            }
            "union_result" if v.is_obj() => parts.push(union_node(v)),
            _ => {}
        }
    }
    let mut node = match parts.len() {
        0 => {
            let mut n = PlanNode::new("Result", PlanNodeKind::Result);
            n.lines(obj);
            if let Some(m) = obj.text("message") {
                n.condition = Some(m.to_string());
            }
            return n;
        }
        1 => parts.remove(0),
        _ => chain(parts, obj),
    };
    if let Some(subs) = obj.get("subqueries") {
        for s in subs.items() {
            let mut sub = PlanNode::new("Subquery", PlanNodeKind::Subquery);
            sub.lines(s);
            if let Some(qb) = s.get("query_block") {
                sub.children.push(block(qb));
            }
            node.children.push(sub);
        }
    }
    node
}

/// Left-deep nested loop over tables listed in join order.
fn chain(mut parts: Vec<PlanNode>, raw: &Js) -> PlanNode {
    let mut acc = parts.remove(0);
    for next in parts {
        let mut join = PlanNode::new("Nested loop join", PlanNodeKind::NestedLoopJoin);
        join.raw = acc.raw.clone();
        if let crate::RawRef::None = join.raw {
            join.lines(raw);
        }
        join.children = vec![acc, next];
        acc = join;
    }
    acc
}

fn union_node(u: &Js) -> PlanNode {
    let mut n = PlanNode::new("Union", PlanNodeKind::Union);
    n.lines(u);
    n.children = u
        .get("query_specifications")
        .map(|s| {
            s.items()
                .iter()
                .map(|spec| match spec.get("query_block") {
                    Some(qb) => block(qb),
                    None => body(spec),
                })
                .collect()
        })
        .unwrap_or_default();
    n
}

fn table_node(t: &Js, join_buffer: Option<&Js>) -> PlanNode {
    let access = t.text("access_type").unwrap_or("").to_ascii_lowercase();
    let covering = t.get("using_index").and_then(Js::as_bool) == Some(true);
    let (op, kind) = match access.as_str() {
        "all" | "hash_all" => ("Table scan", PlanNodeKind::FullScan),
        "index" | "hash_index" if covering => ("Covering index scan", PlanNodeKind::IndexOnlyScan),
        "index" | "hash_index" => ("Index scan", PlanNodeKind::IndexScan),
        "range" | "hash_range" if covering => {
            ("Covering index range scan", PlanNodeKind::IndexOnlyScan)
        }
        "range" | "hash_range" => ("Index range scan", PlanNodeKind::IndexScan),
        "index_merge" => ("Index merge", PlanNodeKind::IndexScan),
        "eq_ref" | "const" | "system" => ("Single-row index lookup", PlanNodeKind::IndexSeek),
        "ref" | "ref_or_null" | "unique_subquery" | "index_subquery" => {
            ("Index lookup", PlanNodeKind::IndexSeek)
        }
        "fulltext" => ("Full-text index search", PlanNodeKind::IndexSeek),
        _ => ("Table access", PlanNodeKind::Other),
    };
    let mut n = PlanNode::new(op, kind);
    n.lines(join_buffer.unwrap_or(t));
    let mut obj = object(t.text("table_name").unwrap_or("?"));
    obj.index = t.text("key").map(str::to_string);
    n.object = Some(obj);
    n.condition = crate::build::join_conditions(
        ["attached_condition", "index_condition"]
            .iter()
            .filter_map(|k| t.text(k).map(str::to_string))
            .collect(),
    );
    push_pred(&mut n, PredicateRole::Filter, t.text("attached_condition"));
    push_pred(&mut n, PredicateRole::IndexCond, t.text("index_condition"));
    if let Some(jb) = join_buffer {
        push_pred(
            &mut n,
            PredicateRole::JoinCond,
            jb.text("attached_condition"),
        );
    }
    n.est_rows = t.num("rows");
    if let Some(r) = t.num("r_rows") {
        n.set_actual(r, t.num("r_loops").unwrap_or(1.0) as u64);
    } else if let Some(l) = t.num("r_loops") {
        n.loops = Some(l as u64);
    }
    let own = [t.num("r_table_time_ms"), t.num("r_other_time_ms")];
    if own.iter().any(Option::is_some) {
        n.actual_time_ms = Some(own.iter().flatten().sum());
    }
    flatten_scalars(
        &mut n,
        t,
        &[
            "table_name",
            "access_type",
            "key",
            "attached_condition",
            "index_condition",
            "rows",
            "r_rows",
            "r_loops",
            "r_table_time_ms",
            "r_other_time_ms",
        ],
        "",
    );
    if let Some(stats) = t.get("r_engine_stats") {
        flatten_scalars(&mut n, stats, &[], "");
    }
    if let Some(jb) = join_buffer {
        flatten_scalars(&mut n, jb, &["table"], "join_buffer.");
    }
    if let Some(mat) = t.get("materialized").and_then(|m| m.get("query_block")) {
        let mut m = PlanNode::new("Materialize", PlanNodeKind::Materialize);
        m.lines(mat);
        m.children.push(block(mat));
        n.children.push(m);
    }
    n
}

/// Turn exclusive step times into inclusive subtree times (bottom-up). The `r_total_time_ms`
/// of the enclosing block is both a lower bound for its node and an upper bound for everything
/// below (a filesort that reads its input already includes the scan).
fn inclusive(node: &mut PlanNode, cap: Option<f64>) {
    let block_total = node
        .extra
        .remove(BLOCK_TOTAL)
        .and_then(|v| v.parse::<f64>().ok());
    let cap = block_total.or(cap);
    for c in &mut node.children {
        inclusive(c, cap);
    }
    let kids: Option<f64> = node
        .children
        .iter()
        .filter_map(|c| c.actual_time_ms)
        .reduce(|a, b| a + b);
    let sum = match (node.actual_time_ms, kids) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
    };
    let total = match (sum, block_total) {
        (Some(s), Some(b)) => Some(s.max(b)),
        (s, b) => s.or(b),
    };
    node.actual_time_ms = match (total, cap) {
        (Some(t), Some(c)) => Some(t.min(c)),
        (t, _) => t,
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_table_keys_become_a_join() {
        let t = parse(
            r#"{"query_block":{"select_id":1,
              "table":{"table_name":"c","access_type":"range","rows":49},
              "table":{"table_name":"o","access_type":"ref","key":"customer_id","rows":25}}}"#,
        )
        .unwrap();
        assert_eq!(t.root.kind, PlanNodeKind::NestedLoopJoin);
        assert_eq!(t.root.children.len(), 2);
        assert!(!t.analyzed);
        assert_eq!(t.root.children[1].object.as_ref().unwrap().name, "o");
    }

    #[test]
    fn exclusive_times_are_summed() {
        let t = parse(
            r#"{"query_block":{"select_id":1,"r_loops":1,"r_total_time_ms":20,
              "filesort":{"r_loops":1,"r_total_time_ms":2,"r_used_priority_queue":false,"r_output_rows":7,
                "table":{"table_name":"t","access_type":"ALL","r_loops":1,"rows":10,"r_rows":10,
                         "r_table_time_ms":5,"r_other_time_ms":1}}}}"#,
        )
        .unwrap();
        assert!(t.analyzed);
        assert_eq!(t.root.op, "Filesort");
        // 2 (sort) + 6 (table) = 8, below the 20 ms block total.
        assert_eq!(t.root.actual_time_ms, Some(20.0));
        assert_eq!(t.root.children[0].actual_time_ms, Some(6.0));
        assert_eq!(t.totals.execution_time_ms, Some(20.0));
    }
}
