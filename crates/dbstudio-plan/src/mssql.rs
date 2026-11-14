//! SQL Server showplan XML (`SET SHOWPLAN_XML ON` / `SET STATISTICS XML ON`).

use dbstudio_core::PlanFormat;
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

use serde_json::{Value, json};

use crate::build::{finish, join_conditions};
use crate::json::fmt_num;
use crate::{
    NodePredicate, PlanError, PlanNode, PlanNodeKind, PlanObject, PlanTotals, PlanTree,
    PredicateRole, RawRef,
};

/// A minimal element tree (text nodes are not needed: showplan keeps everything in attributes).
#[derive(Debug, Default)]
struct X {
    name: String,
    attrs: Vec<(String, String)>,
    kids: Vec<X>,
}

impl X {
    fn attr(&self, key: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    fn num(&self, key: &str) -> Option<f64> {
        self.attr(key).and_then(|v| v.trim().parse::<f64>().ok())
    }

    fn child(&self, name: &str) -> Option<&X> {
        self.kids.iter().find(|k| k.name == name)
    }

    /// First descendant named `name` that is not below a `RelOp` boundary.
    fn find_local(&self, name: &str) -> Option<&X> {
        for k in &self.kids {
            if k.name == name {
                return Some(k);
            }
            if k.name != "RelOp" {
                if let Some(f) = k.find_local(name) {
                    return Some(f);
                }
            }
        }
        None
    }

    /// Every descendant named one of `names` that is not below a `RelOp` boundary.
    fn find_all_local<'a>(&'a self, names: &[&str], out: &mut Vec<&'a X>) {
        for k in &self.kids {
            if names.contains(&k.name.as_str()) {
                out.push(k);
            } else if k.name != "RelOp" {
                k.find_all_local(names, out);
            }
        }
    }

    fn find_any(&self, name: &str) -> Option<&X> {
        for k in &self.kids {
            if k.name == name {
                return Some(k);
            }
            if let Some(f) = k.find_any(name) {
                return Some(f);
            }
        }
        None
    }
}

fn start_to_x(e: &BytesStart<'_>) -> Result<X, PlanError> {
    let name = e.local_name().as_ref().to_string();
    let mut attrs = Vec::new();
    for a in e.attributes() {
        let a = a.map_err(|err| PlanError::Invalid(format!("bad XML attribute: {err}")))?;
        let key = a.key.local_name().as_ref().to_string();
        let value = a
            .normalized_value(quick_xml::XmlVersion::Implicit1_0)
            .map_err(|err| PlanError::Invalid(format!("bad XML attribute value: {err}")))?
            .into_owned();
        attrs.push((key, value));
    }
    Ok(X {
        name,
        attrs,
        kids: Vec::new(),
    })
}

/// Deepest element nesting and largest element count accepted (bounds every recursive pass).
const MAX_XML_DEPTH: usize = 256;
const MAX_XML_ELEMENTS: usize = 500_000;

fn read_xml(text: &str) -> Result<X, PlanError> {
    let mut reader = Reader::from_str(text);
    let mut stack: Vec<X> = vec![X::default()];
    let mut elements = 0usize;
    loop {
        let ev = reader.read_event().map_err(|e| {
            PlanError::Invalid(format!(
                "invalid XML at byte {}: {e}",
                reader.buffer_position()
            ))
        })?;
        match ev {
            Event::Start(e) => {
                elements += 1;
                if stack.len() > MAX_XML_DEPTH || elements > MAX_XML_ELEMENTS {
                    return Err(too_big());
                }
                stack.push(start_to_x(&e)?)
            }
            Event::Empty(e) => {
                elements += 1;
                if elements > MAX_XML_ELEMENTS {
                    return Err(too_big());
                }
                let x = start_to_x(&e)?;
                stack.last_mut().expect("root frame").kids.push(x);
            }
            Event::End(_) => {
                if stack.len() < 2 {
                    return Err(PlanError::Invalid("unbalanced XML".into()));
                }
                let x = stack.pop().expect("checked length");
                stack.last_mut().expect("root frame").kids.push(x);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if stack.len() != 1 {
        return Err(PlanError::Invalid("unterminated XML element".into()));
    }
    Ok(stack.remove(0))
}

fn too_big() -> PlanError {
    PlanError::Invalid(format!(
        "the showplan XML is nested deeper than {MAX_XML_DEPTH} levels or has more than {MAX_XML_ELEMENTS} elements"
    ))
}

pub(crate) fn parse(text: &str) -> Result<PlanTree, PlanError> {
    let doc = read_xml(text)?;
    let mut plans: Vec<(&X, Option<&X>)> = Vec::new();
    collect_plans(&doc, None, &mut plans);
    if plans.is_empty() {
        return Err(PlanError::Invalid(
            "no QueryPlan element in the showplan XML".into(),
        ));
    }
    let mut roots = Vec::new();
    let mut analyzed = false;
    let mut first: Option<(&X, Option<&X>)> = None;
    for (qp, stmt) in &plans {
        let Some(rel) = qp.child("RelOp") else {
            continue;
        };
        let mut n = rel_op(rel);
        analyzed |= any_actual(&n);
        if let Some(stmt) = stmt {
            if let Some(t) = stmt.attr("StatementText") {
                n.put("Statement", t.trim());
            }
        }
        if let Some(w) = qp.child("Warnings") {
            let names: Vec<&str> = w.kids.iter().map(|k| k.name.as_str()).collect();
            n.put("Plan warnings", names.join(", "));
            warning_details(&mut n, w);
        }
        if let Some(m) = qp.child("MissingIndexes") {
            let list = missing_indexes(m);
            if !list.is_empty() {
                n.put("missingIndexes", Value::Array(list).to_string());
            }
        }
        first.get_or_insert((qp, *stmt));
        roots.push(n);
    }
    if roots.is_empty() {
        return Err(PlanError::Invalid(
            "the showplan XML has no RelOp (no executable plan)".into(),
        ));
    }
    let mut totals = PlanTotals::default();
    if roots.len() == 1 {
        if let Some((qp, stmt)) = first {
            totals.total_cost = stmt
                .and_then(|s| s.num("StatementSubTreeCost"))
                .or(roots[0].est_cost);
            totals.planning_time_ms = qp.num("CompileTime");
            totals.execution_time_ms = qp
                .child("QueryTimeStats")
                .and_then(|q| q.num("ElapsedTime"))
                .filter(|_| analyzed);
            totals.rows = roots[0]
                .actual_rows_total
                .or_else(|| stmt.and_then(|s| s.num("StatementEstRows")));
        }
    }
    Ok(finish(
        PlanFormat::MsSqlShowplanXml,
        analyzed,
        roots,
        totals,
    ))
}

fn collect_plans<'a>(x: &'a X, stmt: Option<&'a X>, out: &mut Vec<(&'a X, Option<&'a X>)>) {
    let stmt = if x.name.starts_with("Stmt") {
        Some(x)
    } else {
        stmt
    };
    for k in &x.kids {
        if k.name == "QueryPlan" {
            out.push((k, stmt));
        } else {
            collect_plans(k, stmt, out);
        }
    }
}

fn any_actual(n: &PlanNode) -> bool {
    n.loops.is_some() || n.children.iter().any(any_actual)
}

/// Child elements of a `RelOp` that describe the operator itself.
const NON_OPERATOR: &[&str] = &[
    "OutputList",
    "RunTimeInformation",
    "Warnings",
    "MemoryFractions",
    "RunTimePartitionSummary",
    "InternalInfo",
    "DefinedValues",
];

fn nested_rel_ops<'a>(x: &'a X, out: &mut Vec<&'a X>) {
    for k in &x.kids {
        if k.name == "RelOp" {
            out.push(k);
        } else if !NON_OPERATOR.contains(&k.name.as_str()) {
            nested_rel_ops(k, out);
        }
    }
}

fn unbracket(s: &str) -> String {
    s.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string()
}

/// `MissingIndexes` of a query plan as `[{impact, database, schema, table, equality[],
/// inequality[], include[]}]` (names without brackets).
fn missing_indexes(m: &X) -> Vec<Value> {
    let mut out = Vec::new();
    for g in m.kids.iter().filter(|k| k.name == "MissingIndexGroup") {
        let impact = g.num("Impact").unwrap_or(0.0);
        for mi in g.kids.iter().filter(|k| k.name == "MissingIndex") {
            let cols = |usage: &str| -> Vec<String> {
                mi.kids
                    .iter()
                    .filter(|c| c.name == "ColumnGroup" && c.attr("Usage") == Some(usage))
                    .flat_map(|c| c.kids.iter().filter(|x| x.name == "Column"))
                    .filter_map(|c| c.attr("Name").map(unbracket))
                    .collect()
            };
            out.push(json!({
                "impact": impact,
                "database": mi.attr("Database").map(unbracket),
                "schema": mi.attr("Schema").map(unbracket),
                "table": mi.attr("Table").map(unbracket).unwrap_or_default(),
                "equality": cols("EQUALITY"),
                "inequality": cols("INEQUALITY"),
                "include": cols("INCLUDE"),
            }));
        }
    }
    out
}

/// Details of a `Warnings` element (query level or operator level) that the advisor needs:
/// the attributes of `PlanAffectingConvert`, `ColumnsWithNoStatistics`, `MemoryGrantWarning`,
/// the spill counters and the `NoJoinPredicate` flag.
fn warning_details(n: &mut PlanNode, w: &X) {
    if matches!(w.attr("NoJoinPredicate"), Some("1" | "true")) {
        n.put("NoJoinPredicate", "true");
    }
    let converts: Vec<Value> = w
        .kids
        .iter()
        .filter(|k| k.name == "PlanAffectingConvert")
        .map(|k| {
            json!({
                "issue": k.attr("ConvertIssue").unwrap_or_default(),
                "expression": k.attr("Expression").unwrap_or_default(),
            })
        })
        .collect();
    if !converts.is_empty() {
        n.put("planAffectingConvert", Value::Array(converts).to_string());
    }
    let no_stats: Vec<Value> = w
        .kids
        .iter()
        .filter(|k| k.name == "ColumnsWithNoStatistics")
        .flat_map(|k| k.kids.iter().filter(|c| c.name == "ColumnReference"))
        .map(|c| {
            json!({
                "database": c.attr("Database").map(unbracket),
                "schema": c.attr("Schema").map(unbracket),
                "table": c.attr("Table").map(unbracket),
                "column": c.attr("Column").map(unbracket).unwrap_or_default(),
            })
        })
        .collect();
    if !no_stats.is_empty() {
        n.put(
            "columnsWithNoStatistics",
            Value::Array(no_stats).to_string(),
        );
    }
    if let Some(g) = w.child("MemoryGrantWarning") {
        n.put(
            "memoryGrantWarning",
            g.attr("GrantWarningKind").unwrap_or(""),
        );
        for (attr, key) in [
            ("RequestedMemory", "requestedMemoryKb"),
            ("GrantedMemory", "grantedMemoryKb"),
            ("MaxUsedMemory", "maxUsedMemoryKb"),
        ] {
            if let Some(v) = g.attr(attr) {
                n.put(key, v);
            }
        }
    }
    if let Some(s) = w.child("SpillToTempDb") {
        if let Some(l) = s.attr("SpillLevel") {
            n.put("spillLevel", l);
        }
    }
    for kind in [
        "SortSpillDetails",
        "HashSpillDetails",
        "ExchangeSpillDetails",
    ] {
        if let Some(d) = w.child(kind) {
            n.put("spillKind", kind);
            for (attr, key) in [
                ("WritesToTempDb", "spillWritesToTempDb"),
                ("ReadsFromTempDb", "spillReadsFromTempDb"),
                ("GrantedMemoryKb", "spillGrantedMemoryKb"),
                ("UsedMemoryKb", "spillUsedMemoryKb"),
            ] {
                if let Some(v) = d.attr(attr) {
                    n.put(key, v);
                }
            }
        }
    }
}

fn bracket(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}

/// `[alias].[Column]` for a `ColumnReference` (alias, else table, as qualifier).
fn column_text(c: &X) -> Option<String> {
    let col = c.attr("Column").map(unbracket)?;
    let q = c
        .attr("Alias")
        .or_else(|| c.attr("Table"))
        .map(unbracket)
        .filter(|q| !q.is_empty());
    Some(match q {
        Some(q) => format!("{}.{}", bracket(&q), bracket(&col)),
        None => bracket(&col),
    })
}

fn is_computed(c: &X) -> bool {
    matches!(c.attr("ComputedColumn"), Some("1" | "true"))
}

fn scalar_string(x: &X) -> Option<String> {
    let op = if x.name == "ScalarOperator" {
        Some(x)
    } else {
        x.find_any("ScalarOperator")
    };
    op.and_then(|s| s.attr("ScalarString")).map(str::to_string)
}

/// `col op value` texts of the seek keys of an index operator (`Prefix`, `StartRange`,
/// `EndRange`); ranges without column names (older plans) give nothing.
fn seek_texts(op_el: &X) -> Vec<String> {
    let Some(sp) = op_el.find_local("SeekPredicates") else {
        return Vec::new();
    };
    let mut parts = Vec::new();
    sp.find_all_local(&["Prefix", "StartRange", "EndRange"], &mut parts);
    let mut out = Vec::new();
    for p in parts {
        let op = match p.attr("ScanType").unwrap_or("EQ") {
            "EQ" => "=",
            "GT" => ">",
            "GE" => ">=",
            "LT" => "<",
            "LE" => "<=",
            _ => continue,
        };
        let cols: Vec<&X> = p
            .child("RangeColumns")
            .map(|c| {
                c.kids
                    .iter()
                    .filter(|k| k.name == "ColumnReference")
                    .collect()
            })
            .unwrap_or_default();
        let exprs: Vec<String> = p
            .child("RangeExpressions")
            .map(|c| c.kids.iter().filter_map(scalar_string).collect())
            .unwrap_or_default();
        for (c, e) in cols.iter().zip(exprs.iter()) {
            let Some(col) = column_text(c) else { continue };
            // `[db].[s].[t].[c] as [a].[c]`: a column of the outer side of a join.
            let value = e.rsplit_once(" as ").map_or(e.as_str(), |(_, a)| a);
            let text = format!("{col}{op}{value}");
            if !out.contains(&text) {
                out.push(text);
            }
        }
    }
    out
}

/// Conditions of the operator element with their roles.
fn node_predicates(op_el: &X, kind: PlanNodeKind) -> Vec<NodePredicate> {
    let mut out = Vec::new();
    let mut add = |role: PredicateRole, text: String| {
        if !text.is_empty() && !out.iter().any(|p: &NodePredicate| p.text == text) {
            out.push(NodePredicate { role, text });
        }
    };
    let join = matches!(
        kind,
        PlanNodeKind::NestedLoopJoin | PlanNodeKind::HashJoin | PlanNodeKind::MergeJoin
    );
    for t in seek_texts(op_el) {
        add(PredicateRole::IndexCond, t);
    }
    let own = |key: &str| {
        op_el
            .find_local(key)
            .and_then(scalar_string)
            .filter(|s| !s.is_empty())
    };
    if let Some(t) = own("Predicate") {
        add(
            if join {
                PredicateRole::JoinFilter
            } else {
                PredicateRole::Filter
            },
            t,
        );
    }
    for key in ["ProbeResidual", "Residual"] {
        if let Some(t) = own(key) {
            add(PredicateRole::JoinFilter, t);
        }
    }
    // Equi join keys: probe = build (hash), outer = inner (merge).
    let key_columns = |name: &str| -> Vec<&X> {
        op_el
            .find_local(name)
            .map(|k| {
                k.kids
                    .iter()
                    .filter(|c| c.name == "ColumnReference")
                    .collect()
            })
            .unwrap_or_default()
    };
    for (left, right) in [
        ("HashKeysProbe", "HashKeysBuild"),
        ("OuterSideJoinColumns", "InnerSideJoinColumns"),
    ] {
        for (l, r) in key_columns(left).iter().zip(key_columns(right).iter()) {
            if let (Some(l), Some(r)) = (column_text(l), column_text(r)) {
                add(PredicateRole::JoinCond, format!("{l}={r}"));
            }
        }
    }
    if let Some(ob) = op_el.find_local("OrderBy") {
        for c in ob.kids.iter().filter(|k| k.name == "OrderByColumn") {
            let Some(cr) = c.child("ColumnReference") else {
                continue;
            };
            let Some(col) = column_text(cr) else { continue };
            // A computed column is not a column of a table: spell it as an expression.
            let col = if is_computed(cr) {
                format!("computed({col})")
            } else {
                col
            };
            let dir = if c.attr("Ascending") == Some("0") {
                " DESC"
            } else {
                ""
            };
            add(PredicateRole::SortKey, format!("{col}{dir}"));
        }
    }
    if let Some(g) = op_el.find_local("GroupBy") {
        for c in g.kids.iter().filter(|k| k.name == "ColumnReference") {
            if let Some(col) = column_text(c) {
                add(PredicateRole::GroupKey, col);
            }
        }
    }
    out
}

/// `alias.Column, ...` of an `OutputList` (columns only).
fn output_list(rel: &X) -> String {
    let mut seen: Vec<String> = Vec::new();
    for c in rel
        .child("OutputList")
        .map(|o| o.kids.iter().filter(|k| k.name == "ColumnReference"))
        .into_iter()
        .flatten()
    {
        let Some(col) = c.attr("Column").map(unbracket) else {
            continue;
        };
        let q = c
            .attr("Alias")
            .or_else(|| c.attr("Table"))
            .map(unbracket)
            .filter(|q| !q.is_empty());
        let text = match q {
            Some(q) => format!("{q}.{col}"),
            None => col,
        };
        if !seen.contains(&text) {
            seen.push(text);
        }
    }
    seen.join(", ")
}

fn rel_op(rel: &X) -> PlanNode {
    let physical = rel.attr("PhysicalOp").unwrap_or("?");
    let logical = rel.attr("LogicalOp").unwrap_or("");
    let operator = rel
        .kids
        .iter()
        .find(|k| !NON_OPERATOR.contains(&k.name.as_str()) && k.name != "RelOp");
    let lookup = operator
        .filter(|o| o.name == "IndexScan")
        .and_then(|o| o.attr("Lookup"))
        .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
    let (kind, op) = if lookup {
        (PlanNodeKind::KeyLookup, "Key Lookup".to_string())
    } else {
        (kind_of(physical, logical), physical.to_string())
    };
    let mut n = PlanNode::new(op, kind);
    if let Some(id) = rel.attr("NodeId") {
        n.raw = RawRef::XmlNode {
            node_id: id.to_string(),
        };
    }
    n.est_cost = rel.num("EstimatedTotalSubtreeCost");
    n.est_rows = rel.num("EstimateRows");
    if let Some(op_el) = operator {
        if let Some(o) = op_el.find_local("Object") {
            n.object = Some(PlanObject {
                schema: o.attr("Schema").map(unbracket),
                name: o.attr("Table").map(unbracket).unwrap_or_default(),
                alias: o.attr("Alias").map(unbracket),
                index: o.attr("Index").map(unbracket),
            });
        }
        let mut conds = Vec::new();
        for key in [
            "SeekPredicates",
            "SeekPredicateNew",
            "Predicate",
            "ProbeResidual",
            "Residual",
        ] {
            if let Some(c) = op_el.find_local(key) {
                if let Some(s) = c
                    .find_any("ScalarOperator")
                    .and_then(|s| s.attr("ScalarString"))
                {
                    if !conds.iter().any(|x| x == s) {
                        conds.push(s.to_string());
                    }
                }
            }
        }
        n.condition = join_conditions(conds);
        n.predicates = node_predicates(op_el, kind);
        n.put("Operator", op_el.name.clone());
        for (k, v) in &op_el.attrs {
            n.put(k, v.clone());
        }
    }
    if !logical.is_empty() && logical != physical {
        n.put("LogicalOp", logical);
    }
    for key in [
        "EstimateIO",
        "EstimateCPU",
        "AvgRowSize",
        "TableCardinality",
        "Parallel",
        "EstimateRebinds",
        "EstimateRewinds",
        "EstimatedExecutionMode",
    ] {
        if let Some(v) = rel.attr(key) {
            n.put(key, v);
        }
    }
    // Run-time counters: rows and executions add up over threads, elapsed time is the longest
    // thread (wall clock; it already includes the children).
    if let Some(rt) = rel.child("RunTimeInformation") {
        let threads: Vec<&X> = rt
            .kids
            .iter()
            .filter(|k| k.name == "RunTimeCountersPerThread")
            .collect();
        if !threads.is_empty() {
            let sum = |key: &str| threads.iter().filter_map(|t| t.num(key)).sum::<f64>();
            let rows = sum("ActualRows");
            let loops = sum("ActualExecutions");
            n.set_actual_total(rows, loops as u64);
            n.actual_time_ms = threads
                .iter()
                .filter_map(|t| t.num("ActualElapsedms"))
                .reduce(f64::max);
            for key in [
                "ActualCPUms",
                "ActualLogicalReads",
                "ActualPhysicalReads",
                "ActualRowsRead",
            ] {
                if threads.iter().any(|t| t.attr(key).is_some()) {
                    n.put(key, crate::json::fmt_num(sum(key)));
                }
            }
            // Rows read and thrown away by a predicate, per execution (the same unit as the
            // other engines' "Rows Removed by Filter").
            if threads.iter().any(|t| t.attr("ActualRowsRead").is_some()) {
                let removed = sum("ActualRowsRead") - rows;
                if removed > 0.0 && loops > 0.0 {
                    n.put("Rows Removed by Filter", fmt_num(removed / loops));
                }
            }
        }
    }
    if let Some(w) = rel.child("Warnings") {
        let spills: Vec<&str> = w
            .kids
            .iter()
            .filter(|k| k.name.contains("Spill"))
            .map(|k| k.name.as_str())
            .collect();
        if !spills.is_empty() {
            n.put("spill", spills.join(", "));
        }
        let all: Vec<&str> = w.kids.iter().map(|k| k.name.as_str()).collect();
        n.put("Warnings", all.join(", "));
        warning_details(&mut n, w);
    }
    n.put("Output", output_list(rel));
    let mut kids = Vec::new();
    for k in &rel.kids {
        if k.name != "RelOp" && !NON_OPERATOR.contains(&k.name.as_str()) {
            nested_rel_ops(k, &mut kids);
        }
    }
    n.children = kids.into_iter().map(rel_op).collect();
    n
}

fn kind_of(physical: &str, logical: &str) -> PlanNodeKind {
    use PlanNodeKind::*;
    let p = physical.to_ascii_lowercase();
    match p.as_str() {
        "table scan" | "clustered index scan" => FullScan,
        "index scan" | "columnstore index scan" => IndexScan,
        "index seek" | "clustered index seek" => IndexSeek,
        "key lookup" | "rid lookup" => KeyLookup,
        "nested loops" => NestedLoopJoin,
        "merge join" => MergeJoin,
        "adaptive join" => HashJoin,
        "hash match" => {
            if logical.to_ascii_lowercase().contains("join") {
                HashJoin
            } else {
                HashAggregate
            }
        }
        "sort" | "top n sort" | "distinct sort" => Sort,
        "stream aggregate" | "aggregate" => Aggregate,
        "window aggregate" | "sequence project" | "segment" => Window,
        "top" => Limit,
        "filter" => Filter,
        "table spool" | "index spool" | "row count spool" | "eager spool" | "lazy spool"
        | "spool" => Materialize,
        "concatenation" | "merge interval" => Union,
        "parallelism" => Gather,
        "constant scan" => Result,
        _ if p.contains("insert")
            || p.contains("update")
            || p.contains("delete")
            || p.contains("merge") =>
        {
            Modify
        }
        _ => Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINI: &str = r#"<?xml version="1.0"?>
<ShowPlanXML xmlns="http://schemas.microsoft.com/sqlserver/2004/07/showplan" Version="1.6">
 <BatchSequence><Batch><Statements>
  <StmtSimple StatementText="select 1" StatementSubTreeCost="0.5" StatementEstRows="10">
   <QueryPlan CompileTime="3">
    <RelOp NodeId="0" PhysicalOp="Nested Loops" LogicalOp="Inner Join" EstimateRows="10" EstimatedTotalSubtreeCost="0.5">
     <NestedLoops>
      <RelOp NodeId="1" PhysicalOp="Clustered Index Scan" LogicalOp="Clustered Index Scan" EstimateRows="4" EstimatedTotalSubtreeCost="0.2">
       <IndexScan><Object Schema="[dbo]" Table="[t]" Index="[PK]" Alias="[a]"/></IndexScan>
       <RunTimeInformation>
        <RunTimeCountersPerThread Thread="0" ActualRows="0" ActualExecutions="0" ActualElapsedms="0"/>
        <RunTimeCountersPerThread Thread="1" ActualRows="40" ActualExecutions="1" ActualElapsedms="3"/>
       </RunTimeInformation>
      </RelOp>
      <RelOp NodeId="2" PhysicalOp="Sort" LogicalOp="Sort" EstimateRows="1" EstimatedTotalSubtreeCost="0.3">
       <Warnings><SpillToTempDb SpillLevel="1"/></Warnings>
       <Sort/>
      </RelOp>
     </NestedLoops>
    </RelOp>
   </QueryPlan>
  </StmtSimple>
 </Statements></Batch></BatchSequence>
</ShowPlanXML>"#;

    #[test]
    fn minimal_plan() {
        let t = parse(MINI).unwrap();
        assert_eq!(t.node_count, 3);
        assert_eq!(t.root.kind, PlanNodeKind::NestedLoopJoin);
        assert_eq!(t.totals.total_cost, Some(0.5));
        assert_eq!(t.totals.planning_time_ms, Some(3.0));
        let scan = &t.root.children[0];
        assert_eq!(scan.kind, PlanNodeKind::FullScan);
        assert_eq!(scan.actual_rows_total, Some(40.0));
        assert_eq!(scan.actual_time_ms, Some(3.0));
        assert_eq!(scan.object.as_ref().unwrap().name, "t");
        assert_eq!(scan.object.as_ref().unwrap().index.as_deref(), Some("PK"));
        assert!(t.root.children[1].extra.contains_key("spill"));
    }

    #[test]
    fn invalid_xml() {
        assert!(parse("<a><b></a>").is_err());
        assert!(parse("<ShowPlanXML/>").is_err());
    }
}
