//! MySQL: `EXPLAIN FORMAT=JSON` (v1 and the 8.3+ v2 format) and the `FORMAT=TREE` /
//! `EXPLAIN ANALYZE` text.

use dbstudio_core::PlanFormat;

use crate::build::{finish, flatten_scalars, inherit_cost, join_conditions, object};
use crate::json::{self, Js};
use crate::{
    NodePredicate, PlanError, PlanNode, PlanNodeKind, PlanObject, PlanTotals, PlanTree,
    PredicateRole, RawRef,
};

/// Comma separated pieces of `text`, ignoring commas inside brackets and quotes. Pieces are
/// trimmed and empty ones dropped.
pub(crate) fn split_top(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if let Some(q) = quote {
            if c == '\\' && q != '`' {
                chars.next();
            } else if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' | '"' | '`' => quote = Some(c),
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => {
                out.push(text[start..i].trim().to_string());
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(text[start..].trim().to_string());
    out.retain(|p| !p.is_empty());
    out
}

/// Add a predicate with `role` when `text` is present.
pub(crate) fn push_pred(n: &mut PlanNode, role: PredicateRole, text: Option<&str>) {
    if let Some(t) = text.map(str::trim).filter(|t| !t.is_empty()) {
        n.predicates.push(NodePredicate {
            role,
            text: t.to_string(),
        });
    }
}

// ---------------------------------------------------------------------------------------------
// JSON
// ---------------------------------------------------------------------------------------------

pub(crate) fn parse_json(text: &str, declared: PlanFormat) -> Result<PlanTree, PlanError> {
    let doc = json::parse(text).map_err(PlanError::Invalid)?;
    if doc.get("query_block").is_some() {
        return parse_v1(&doc);
    }
    if doc.get("inputs").is_some() || doc.get("operation").is_some() {
        return parse_v2(&doc);
    }
    let _ = declared;
    Err(PlanError::Invalid(
        "neither \"query_block\" nor \"operation\" found in the MySQL plan".into(),
    ))
}

const WRAPPERS: &[(&str, &str, PlanNodeKind)] = &[
    ("ordering_operation", "Ordering", PlanNodeKind::Sort),
    ("grouping_operation", "Grouping", PlanNodeKind::Aggregate),
    (
        "duplicates_removal",
        "Duplicates removal",
        PlanNodeKind::Aggregate,
    ),
    ("buffer_result", "Buffer result", PlanNodeKind::Materialize),
    ("windowing", "Windowing", PlanNodeKind::Window),
];

fn parse_v1(doc: &Js) -> Result<PlanTree, PlanError> {
    let qb = doc
        .get("query_block")
        .filter(|q| q.is_obj())
        .ok_or_else(|| PlanError::Invalid("\"query_block\" is not an object".into()))?;
    let mut root = block(qb);
    inherit_cost(&mut root);
    let query_cost = qb
        .get("cost_info")
        .and_then(|c| c.num("query_cost"))
        .or(root.est_cost);
    if root.est_cost.is_none() {
        root.est_cost = query_cost;
    }
    let totals = PlanTotals {
        total_cost: query_cost,
        ..PlanTotals::default()
    };
    Ok(finish(PlanFormat::MySqlJson, false, vec![root], totals))
}

/// A `query_block` (or `query_specifications` entry).
fn block(qb: &Js) -> PlanNode {
    let mut n = body(qb);
    if let Some(id) = qb.num("select_id") {
        n.put("select_id", json::fmt_num(id));
    }
    if n.raw == RawRef::None {
        n.lines(qb);
    }
    n
}

/// The structural part of a block: wrappers, a join, a table, a union or just a message.
fn body(obj: &Js) -> PlanNode {
    for (key, op, kind) in WRAPPERS {
        if let Some(inner) = obj.get(key).filter(|v| v.is_obj()) {
            let mut n = PlanNode::new(*op, *kind);
            n.lines(inner);
            flatten_scalars(&mut n, inner, &[], "");
            if *key == "ordering_operation"
                && inner.get("using_filesort").and_then(Js::as_bool) == Some(false)
            {
                n.op = "Ordering (index)".into();
            }
            n.children.push(body(inner));
            return n;
        }
    }
    if let Some(items) = obj.get("nested_loop") {
        let parts: Vec<PlanNode> = items.items().iter().map(body).collect();
        return join_chain(parts, items);
    }
    if let Some(table) = obj.get("table").filter(|v| v.is_obj()) {
        return table_node(table);
    }
    if let Some(union) = obj.get("union_result").filter(|v| v.is_obj()) {
        let mut n = PlanNode::new("Union", PlanNodeKind::Union);
        n.lines(union);
        flatten_scalars(&mut n, union, &[], "");
        n.children = union
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
        return n;
    }
    // A block without tables: "No tables used", "Impossible WHERE", ...
    let mut n = PlanNode::new("Result", PlanNodeKind::Result);
    n.lines(obj);
    if let Some(m) = obj.text("message") {
        n.condition = Some(m.to_string());
    }
    n
}

/// Left-deep nested loop over the joined tables (MySQL lists them in join order).
fn join_chain(mut parts: Vec<PlanNode>, raw: &Js) -> PlanNode {
    if parts.is_empty() {
        let mut n = PlanNode::new("Result", PlanNodeKind::Result);
        n.lines(raw);
        return n;
    }
    let mut acc = parts.remove(0);
    for next in parts {
        let mut join = PlanNode::new("Nested loop join", PlanNodeKind::NestedLoopJoin);
        join.raw = acc.raw.clone();
        join.est_cost = next.extra.get("prefix_cost").and_then(|v| v.parse().ok());
        join.est_rows = next
            .extra
            .get("rows_produced_per_join")
            .and_then(|v| v.parse().ok());
        join.children = vec![acc, next];
        acc = join;
    }
    acc
}

fn table_node(t: &Js) -> PlanNode {
    let access = t.text("access_type").unwrap_or("");
    let covering = t.get("using_index").and_then(Js::as_bool) == Some(true);
    let (op, kind) = match access {
        "ALL" => ("Table scan", PlanNodeKind::FullScan),
        "index" if covering => ("Covering index scan", PlanNodeKind::IndexOnlyScan),
        "index" => ("Index scan", PlanNodeKind::IndexScan),
        "range" if covering => ("Covering index range scan", PlanNodeKind::IndexOnlyScan),
        "range" => ("Index range scan", PlanNodeKind::IndexScan),
        "index_merge" => ("Index merge", PlanNodeKind::IndexScan),
        "eq_ref" | "const" | "system" => ("Single-row index lookup", PlanNodeKind::IndexSeek),
        "ref" | "ref_or_null" | "unique_subquery" | "index_subquery" => {
            ("Index lookup", PlanNodeKind::IndexSeek)
        }
        "fulltext" => ("Full-text index search", PlanNodeKind::IndexSeek),
        _ => ("Table access", PlanNodeKind::Other),
    };
    let mut n = PlanNode::new(op, kind);
    n.lines(t);
    let mut obj = object(t.text("table_name").unwrap_or("?"));
    obj.index = t.text("key").map(str::to_string);
    n.object = Some(obj);
    n.condition = join_conditions(
        [
            "attached_condition",
            "index_condition",
            "attached_condition_before_filter",
        ]
        .iter()
        .filter_map(|k| t.text(k).map(str::to_string))
        .collect(),
    );
    push_pred(&mut n, PredicateRole::Filter, t.text("attached_condition"));
    push_pred(&mut n, PredicateRole::IndexCond, t.text("index_condition"));
    let ci = t.get("cost_info");
    let read = ci.and_then(|c| c.num("read_cost"));
    let eval = ci.and_then(|c| c.num("eval_cost"));
    let prefix = ci.and_then(|c| c.num("prefix_cost"));
    n.est_cost = match (read, eval) {
        (Some(r), Some(e)) => Some(r + e),
        _ => prefix,
    };
    let examined = t.num("rows_examined_per_scan");
    let filtered = t.num("filtered").unwrap_or(100.0) / 100.0;
    n.est_rows = examined.map(|r| r * filtered);
    flatten_scalars(
        &mut n,
        t,
        &[
            "table_name",
            "access_type",
            "attached_condition",
            "index_condition",
            "key",
        ],
        "",
    );
    if let Some(c) = ci {
        flatten_scalars(&mut n, c, &[], "");
    }
    let mut subs = Vec::new();
    if let Some(m) = t.get("materialized_from_subquery").filter(|m| m.is_obj()) {
        let mut mat = PlanNode::new("Materialize", PlanNodeKind::Materialize);
        mat.lines(m);
        if let Some(qb) = m.get("query_block") {
            mat.children.push(block(qb));
        }
        subs.push(mat);
    }
    for key in ["attached_subqueries", "optimized_away_subqueries"] {
        if let Some(list) = t.get(key) {
            for s in list.items() {
                let mut sub = PlanNode::new("Subquery", PlanNodeKind::Subquery);
                sub.lines(s);
                if key == "optimized_away_subqueries" {
                    sub.put("optimized_away", "true");
                }
                if let Some(qb) = s.get("query_block") {
                    sub.children.push(block(qb));
                }
                subs.push(sub);
            }
        }
    }
    n.children = subs;
    n
}

// ---------------------------------------------------------------------------------------------
// JSON v2 (explain_json_format_version = 2)
// ---------------------------------------------------------------------------------------------

fn parse_v2(doc: &Js) -> Result<PlanTree, PlanError> {
    let mut root = node_v2(doc);
    inherit_cost(&mut root);
    let analyzed = any_actual(&root);
    let totals = PlanTotals {
        execution_time_ms: root.actual_time_ms.filter(|_| analyzed),
        ..PlanTotals::default()
    };
    Ok(finish(
        PlanFormat::MySqlJsonV2,
        analyzed,
        vec![root],
        totals,
    ))
}

fn any_actual(n: &PlanNode) -> bool {
    n.actual_time_ms.is_some() || n.children.iter().any(any_actual)
}

fn node_v2(j: &Js) -> PlanNode {
    let operation = j.text("operation").unwrap_or("Query");
    let (kind, op) = classify(operation);
    let mut n = PlanNode::new(op, kind);
    n.lines(j);
    if let (Some(table), false) = (j.text("table_name"), operation.contains("<temporary>")) {
        n.object = Some(PlanObject {
            schema: j.text("schema_name").map(str::to_string),
            name: table.to_string(),
            alias: j.text("alias").filter(|a| *a != table).map(str::to_string),
            index: j.text("index_name").map(str::to_string),
        });
    }
    n.condition = j
        .text("condition")
        .or_else(|| j.text("lookup_condition"))
        .or_else(|| j.text("join_condition"))
        .map(str::to_string);
    if n.condition.is_none() {
        n.condition = condition_of(operation);
    }
    let is_join = matches!(
        kind,
        PlanNodeKind::HashJoin | PlanNodeKind::NestedLoopJoin | PlanNodeKind::MergeJoin
    );
    push_pred(
        &mut n,
        if is_join {
            PredicateRole::JoinCond
        } else {
            PredicateRole::Filter
        },
        j.text("condition").or_else(|| j.text("join_condition")),
    );
    if is_join && n.predicates.is_empty() {
        push_pred(
            &mut n,
            PredicateRole::JoinCond,
            condition_of(operation).as_deref(),
        );
    }
    push_pred(&mut n, PredicateRole::IndexCond, j.text("lookup_condition"));
    for r in j.get("ranges").map(Js::items).unwrap_or_default() {
        push_pred(&mut n, PredicateRole::IndexCond, Some(&r.display()));
    }
    for f in j.get("sort_fields").map(Js::items).unwrap_or_default() {
        push_pred(&mut n, PredicateRole::SortKey, Some(&f.display()));
    }
    n.est_cost = j.num("estimated_total_cost");
    n.est_rows = j.num("estimated_rows");
    if let Some(rows) = j.num("actual_rows") {
        let loops = j.num("actual_loops").unwrap_or(1.0) as u64;
        n.set_actual(rows, loops);
        // `actual_last_row_ms` is per execution.
        n.actual_time_ms = j.num("actual_last_row_ms").map(|t| t * loops as f64);
    }
    flatten_scalars(
        &mut n,
        j,
        &[
            "operation",
            "table_name",
            "schema_name",
            "alias",
            "index_name",
            "condition",
            "estimated_total_cost",
            "estimated_rows",
            "actual_rows",
            "actual_loops",
            "actual_last_row_ms",
            "actual_first_row_ms",
            "query",
        ],
        "",
    );
    if let Some(access) = j.text("join_algorithm") {
        if access.contains("hash") {
            n.kind = PlanNodeKind::HashJoin;
        }
    }
    n.children = j
        .get("inputs")
        .map(|i| i.items().iter().map(node_v2).collect())
        .unwrap_or_default();
    n
}

// ---------------------------------------------------------------------------------------------
// Tree text
// ---------------------------------------------------------------------------------------------

pub(crate) fn parse_tree(text: &str) -> Result<PlanTree, PlanError> {
    struct Frame {
        depth: usize,
        node: PlanNode,
    }
    let mut roots: Vec<PlanNode> = Vec::new();
    let mut stack: Vec<Frame> = Vec::new();

    fn pop_to(stack: &mut Vec<Frame>, roots: &mut Vec<PlanNode>, depth: usize) {
        while stack.last().is_some_and(|f| f.depth >= depth) {
            let f = stack.pop().expect("checked non-empty");
            match stack.last_mut() {
                Some(parent) => parent.node.children.push(f.node),
                None => roots.push(f.node),
            }
        }
    }

    let mut saw_actual = false;
    for (line_no, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix("-> ") else {
            continue;
        };
        let indent = line.len() - trimmed.len();
        let depth = indent / 4;
        let mut n = parse_tree_line(rest);
        saw_actual |= n.actual_time_ms.is_some() || n.loops == Some(0);
        n.raw = RawRef::Lines {
            start: line_no as u32,
            end: line_no as u32,
        };
        pop_to(&mut stack, &mut roots, depth);
        if stack.len() >= crate::json::MAX_DEPTH as usize {
            return Err(PlanError::Invalid(format!(
                "the MySQL tree plan is nested deeper than {} levels (line {})",
                crate::json::MAX_DEPTH,
                line_no + 1
            )));
        }
        stack.push(Frame { depth, node: n });
    }
    pop_to(&mut stack, &mut roots, 0);
    if roots.is_empty() {
        return Err(PlanError::Invalid(
            "no \"-> \" operator lines found in the MySQL tree plan".into(),
        ));
    }
    for r in &mut roots {
        inherit_cost(r);
    }
    let totals = PlanTotals {
        execution_time_ms: roots
            .first()
            .and_then(|r| r.actual_time_ms)
            .filter(|_| saw_actual),
        ..PlanTotals::default()
    };
    Ok(finish(PlanFormat::MySqlTree, saw_actual, roots, totals))
}

/// Number following `key` (`cost=27646`), ending at a space, `)` or `.` followed by `.`.
fn num_after(s: &str, key: &str) -> Option<f64> {
    let start = s.find(key)? + key.len();
    let rest = &s[start..];
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '-' | '+')))
        .unwrap_or(rest.len());
    let mut token = &rest[..end];
    // `0.5..12` is two numbers separated by `..`; cut at the first `..`.
    if let Some(i) = token.find("..") {
        token = &token[..i];
    }
    token.parse().ok()
}

fn parse_tree_line(rest: &str) -> PlanNode {
    // Label ends where the first statistics group starts.
    let cut = ["  (cost=", "  (actual time=", "  (never executed)"]
        .iter()
        .filter_map(|m| rest.find(m))
        .min()
        .unwrap_or(rest.len());
    let label = rest[..cut].trim_end();
    let stats = &rest[cut..];
    let (kind, op) = classify(label);
    let mut n = PlanNode::new(op, kind);
    n.object = object_of(label);
    n.condition = condition_of(label);
    if let Some(g) = stats.find("(cost=") {
        let g = &stats[g..];
        n.est_cost = num_after(g, "cost=");
        n.est_rows = num_after(g, "rows=");
    }
    if stats.contains("(never executed)") {
        n.set_actual(0.0, 0);
        n.actual_time_ms = Some(0.0);
    } else if let Some(a) = stats.find("(actual time=") {
        let a = &stats[a..];
        let rows = num_after(a, "rows=").unwrap_or(0.0);
        let loops = num_after(a, "loops=").unwrap_or(1.0).max(0.0) as u64;
        n.set_actual(rows, loops);
        // `actual time=first..last` is per execution, in milliseconds.
        let last = a
            .find("..")
            .and_then(|i| num_after(&a[i + 2..], ""))
            .or_else(|| num_after(a, "time="));
        n.actual_time_ms = last.map(|t| t * loops as f64);
    }
    if label.starts_with("Sort:") {
        n.put("Sort Key", label.trim_start_matches("Sort:").trim());
    }
    n.predicates = tree_predicates(label, kind);
    n
}

/// Conditions of a tree line with their roles.
fn tree_predicates(label: &str, kind: PlanNodeKind) -> Vec<NodePredicate> {
    let mut n = PlanNode::new("x", kind);
    if let Some(c) = label.strip_prefix("Filter: ") {
        push_pred(&mut n, PredicateRole::Filter, Some(c));
    } else if let Some(rest) = label.strip_prefix("Sort: ") {
        for key in split_top(rest) {
            if !key.starts_with("limit input") {
                push_pred(&mut n, PredicateRole::SortKey, Some(&key));
            }
        }
    } else if matches!(
        kind,
        PlanNodeKind::IndexSeek | PlanNodeKind::IndexScan | PlanNodeKind::IndexOnlyScan
    ) {
        if let Some(i) = label.find(" over ") {
            push_pred(&mut n, PredicateRole::IndexCond, Some(&label[i + 6..]));
        } else if label.ends_with(')') {
            if let Some(i) = label.find(" (") {
                for c in split_top(&label[i + 2..label.len() - 1]) {
                    push_pred(&mut n, PredicateRole::IndexCond, Some(&c));
                }
            }
        }
    } else if matches!(kind, PlanNodeKind::HashJoin | PlanNodeKind::NestedLoopJoin)
        && label.ends_with(')')
    {
        if let Some(i) = label.find(" (") {
            push_pred(
                &mut n,
                PredicateRole::JoinCond,
                Some(&label[i + 2..label.len() - 1]),
            );
        }
    }
    n.predicates
}

/// Operator label to `(kind, short op name)`.
fn classify(label: &str) -> (PlanNodeKind, String) {
    use PlanNodeKind::*;
    let lower = label.to_ascii_lowercase();
    let op = op_name(label);
    let kind = if lower.contains("<temporary>") && lower.starts_with("table scan") {
        Other
    } else if lower.starts_with("table scan") || lower.starts_with("full table scan") {
        FullScan
    } else if lower.starts_with("covering index") {
        IndexOnlyScan
    } else if lower.starts_with("single-row")
        || lower.starts_with("index lookup")
        || lower.starts_with("constant row")
        || lower.starts_with("full-text")
        || lower.starts_with("unique index")
    {
        IndexSeek
    } else if lower.starts_with("index range scan")
        || lower.starts_with("index scan")
        || lower.starts_with("index skip scan")
        || lower.starts_with("multi-range")
        || lower.starts_with("intersect rows")
        || lower.starts_with("sort-union")
    {
        IndexScan
    } else if lower.contains("hash join") || lower.starts_with("hash") && lower.contains("join") {
        HashJoin
    } else if lower.starts_with("nested loop") || lower.starts_with("batched key access") {
        NestedLoopJoin
    } else if lower.starts_with("sort") {
        Sort
    } else if lower.starts_with("aggregate") || lower.starts_with("group aggregate") {
        Aggregate
    } else if lower.starts_with("filter") {
        Filter
    } else if lower.starts_with("limit") {
        Limit
    } else if lower.starts_with("window") {
        Window
    } else if lower.starts_with("materialize") || lower.starts_with("temporary table") {
        Materialize
    } else if lower.starts_with("append") || lower.starts_with("union") {
        Union
    } else if lower.starts_with("select #") || lower.starts_with("subquery") {
        Subquery
    } else if lower.starts_with("update ")
        || lower.starts_with("insert ")
        || lower.starts_with("delete ")
    {
        Modify
    } else if lower.starts_with("rows fetched before")
        || lower.starts_with("zero rows")
        || lower.starts_with("no tables")
    {
        Result
    } else {
        Other
    };
    (kind, op)
}

/// Short operator name: label up to `: `, ` on ` or ` (`.
fn op_name(label: &str) -> String {
    let mut end = label.len();
    for m in [": ", " on ", " ("] {
        if let Some(i) = label.find(m) {
            end = end.min(i);
        }
    }
    label[..end].trim().to_string()
}

/// `Table scan on o` / `Index lookup on c using PRIMARY (id=o.customer_id)`.
fn object_of(label: &str) -> Option<PlanObject> {
    let i = label.find(" on ")?;
    let after = &label[i + 4..];
    let lower = label.to_ascii_lowercase();
    if !(lower.contains("scan") || lower.contains("lookup") || lower.contains("search")) {
        return None;
    }
    let end = [" using ", " over ", " (", " "]
        .iter()
        .filter_map(|m| after.find(m))
        .min()
        .unwrap_or(after.len());
    let name = after[..end].trim();
    if name.is_empty() || name.starts_with('<') {
        return None;
    }
    let index = after.find(" using ").map(|u| {
        let s = &after[u + 7..];
        let e = [" over ", " (", " "]
            .iter()
            .filter_map(|m| s.find(m))
            .min()
            .unwrap_or(s.len());
        s[..e].to_string()
    });
    let mut o = object(name);
    o.index = index;
    Some(o)
}

fn condition_of(label: &str) -> Option<String> {
    if let Some(c) = label.strip_prefix("Filter: ") {
        return Some(c.to_string());
    }
    if let Some(i) = label.find(" over ") {
        return Some(label[i + 6..].to_string());
    }
    // A trailing `(cond)` group: lookups and hash joins.
    if label.ends_with(')') {
        if let Some(i) = label.find(" (") {
            let lower = label.to_ascii_lowercase();
            if lower.contains("lookup") || lower.contains("join") {
                return Some(label[i + 2..label.len() - 1].to_string());
            }
        }
    }
    if let Some(c) = label.strip_prefix("Limit: ") {
        return Some(c.to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_line_numbers() {
        let n = parse_tree_line(
            "Single-row index lookup on c using PRIMARY (id=o.customer_id)  (cost=0.25 rows=1) (actual time=566e-6..588e-6 rows=1 loops=58079)",
        );
        assert_eq!(n.kind, PlanNodeKind::IndexSeek);
        assert_eq!(n.op, "Single-row index lookup");
        assert_eq!(n.est_cost, Some(0.25));
        assert_eq!(n.loops, Some(58079));
        let t = n.actual_time_ms.unwrap();
        assert!((t - 588e-6 * 58079.0).abs() < 1e-9, "{t}");
        let o = n.object.unwrap();
        assert_eq!(
            (o.name.as_str(), o.index.as_deref()),
            ("c", Some("PRIMARY"))
        );
        assert_eq!(n.condition.as_deref(), Some("id=o.customer_id"));
    }

    #[test]
    fn never_executed_and_errors() {
        let n = parse_tree_line("Table scan on t  (cost=1 rows=2) (never executed)");
        assert_eq!(n.loops, Some(0));
        assert!(parse_tree("no plan here").is_err());
    }

    #[test]
    fn tree_nesting() {
        let t = parse_tree(
            "-> Limit: 5 row(s)  (cost=1 rows=5)\n    -> Table scan on t  (cost=1 rows=9)\n",
        )
        .unwrap();
        assert_eq!(t.root.children.len(), 1);
        assert_eq!(t.node_count, 2);
        assert!(!t.analyzed);
    }
}
