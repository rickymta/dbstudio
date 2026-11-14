//! SQL Server rules (design section 4.3).
//!
//! The parser keeps what the showplan says about missing indexes, implicit conversions, spills,
//! statistics and memory grants in the `extra` of the node that carries it (`missingIndexes`,
//! `planAffectingConvert`, `spill*`, `columnsWithNoStatistics`, `memoryGrantWarning`,
//! `NoJoinPredicate`), and the output columns of every operator in `Output`.
//!
//! Index advice goes through [`IndexCandidate`] only; the DDL (`ONLINE`, `DROP_EXISTING`,
//! naming) is the business of `index::render`.

use std::collections::BTreeMap;

use serde_json::Value;

use super::common::{
    check_action, columns_text, conjuncts, ev, ev_text, first_scan, index_columns, num,
    statistics_action, table_ref,
};
use super::index::{IndexCandidate, IndexKey, MAX_INCLUDE_COLUMNS, MAX_KEY_COLUMNS, quote_ident};
use super::model::{
    ActionKind, ActionScope, AdviceAction, AdviceRule, Caveat, Confidence, EvidenceUnit, LockRisk,
    PredicateRole, TableRef,
};
use super::{Cx, Draft, ShareSpec};
use crate::json::fmt_num;
use crate::{PlanNode, PlanNodeKind, PlanRule, Severity};

/// Missing-index groups below this impact (percent) are not worth an advice.
const MIN_MISSING_INDEX_IMPACT: f64 = 30.0;
/// A key lookup run at least this many times is a problem on its own.
const LOOKUP_MIN_EXECUTIONS: u64 = 1_000;
/// Share of the statement from which a key lookup counts, whatever its executions.
const LOOKUP_MIN_SHARE: f64 = 0.10;
/// More columns than this make an INCLUDE list pointless.
const INCLUDE_GIVE_UP: usize = 8;
/// Tables above this many rows get `WITH SAMPLE` instead of `WITH FULLSCAN`.
const SAMPLE_ABOVE_ROWS: f64 = 10_000_000.0;

fn caveat(key: &str, verify: bool) -> Caveat {
    Caveat {
        key: key.into(),
        verify,
        params: BTreeMap::new(),
    }
}

fn json_list(node: &PlanNode, key: &str) -> Vec<Value> {
    node.extra
        .get(key)
        .and_then(|v| serde_json::from_str::<Value>(v).ok())
        .and_then(|v| match v {
            Value::Array(a) => Some(a),
            _ => None,
        })
        .unwrap_or_default()
}

fn text_of(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn strings_of(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn same_table(a: &TableRef, b: &TableRef) -> bool {
    a.name.eq_ignore_ascii_case(&b.name)
        && match (&a.schema, &b.schema) {
            (Some(x), Some(y)) => x.eq_ignore_ascii_case(y),
            _ => true,
        }
}

fn is_scan(kind: PlanNodeKind) -> bool {
    matches!(
        kind,
        PlanNodeKind::FullScan
            | PlanNodeKind::IndexScan
            | PlanNodeKind::IndexOnlyScan
            | PlanNodeKind::IndexSeek
            | PlanNodeKind::KeyLookup
    )
}

/// Nodes of the plan that read `table`, the most expensive first.
fn nodes_of_table(cx: &Cx<'_>, table: &TableRef) -> Vec<u32> {
    let mut v: Vec<&PlanNode> = cx
        .nodes
        .iter()
        .copied()
        .filter(|n| is_scan(n.kind) && table_ref(n).is_some_and(|t| same_table(&t, table)))
        .collect();
    v.sort_by(|a, b| {
        let key = |n: &PlanNode| (n.self_time_ms.unwrap_or(0.0), n.self_cost.unwrap_or(0.0));
        let (ka, kb) = (key(a), key(b));
        kb.partial_cmp(&ka).unwrap_or(std::cmp::Ordering::Equal)
    });
    v.into_iter().map(|n| n.id).collect()
}

/// Distinct tables read below `node` (at most `max`).
fn tables_below(node: &PlanNode, max: usize) -> Vec<TableRef> {
    fn walk(n: &PlanNode, out: &mut Vec<TableRef>) {
        if is_scan(n.kind) {
            if let Some(t) = table_ref(n) {
                if !out.iter().any(|x| same_table(x, &t)) {
                    out.push(t);
                }
            }
        }
        for c in &n.children {
            walk(c, out);
        }
    }
    let mut v = Vec::new();
    walk(node, &mut v);
    v.truncate(max);
    v
}

fn warning_of(cx: &Cx<'_>, node: u32, rule: PlanRule) -> Option<Severity> {
    cx.tree
        .warnings
        .iter()
        .find(|w| w.node_id == node && w.rule == rule)
        .map(|w| w.severity)
}

/// Share of the statement spent in `node` itself (time when the plan has it, else cost).
fn self_share(cx: &Cx<'_>, node: &PlanNode) -> Option<f64> {
    if let (Some(total), Some(t)) = (
        cx.tree.totals.execution_time_ms.filter(|t| *t > 0.0),
        node.self_time_ms,
    ) {
        return Some(t / total);
    }
    match (
        cx.tree.totals.total_cost.filter(|c| *c > 0.0),
        node.self_cost,
    ) {
        (Some(total), Some(c)) => Some(c / total),
        _ => None,
    }
}

/// Column names of a `Output` extra (`o.Total, o.Note`), qualifier dropped.
fn output_columns(node: &PlanNode) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for item in node
        .extra
        .get("Output")
        .map(String::as_str)
        .unwrap_or("")
        .split(", ")
    {
        let col = item.rsplit_once('.').map_or(item, |(_, c)| c).trim();
        if !col.is_empty() && !out.iter().any(|c| c.eq_ignore_ascii_case(col)) {
            out.push(col.to_string());
        }
    }
    out
}

fn kb_to_bytes(kb: f64) -> f64 {
    kb * 1024.0
}

// ------------------------------------------------------------------------------------------
// Entry points
// ------------------------------------------------------------------------------------------

/// Rules evaluated for every node of a plan of this family.
pub(crate) fn node_rules(cx: &Cx<'_>, node: &PlanNode, out: &mut Vec<Draft>) {
    if node.loops == Some(0) {
        return;
    }
    key_lookup(cx, node, out);
    implicit_conversion(cx, node, out);
    spill(cx, node, out);
    no_join_predicate(node, out);
    no_statistics(cx, node, out);
    excessive_grant(cx, node, out);
}

/// Rules evaluated once per plan.
pub(crate) fn tree_rules(cx: &Cx<'_>, out: &mut Vec<Draft>) {
    missing_indexes(cx, out);
    fold_key_lookup_into_nested_loop(out);
    statistics_with_fullscan(cx, out);
}

// ------------------------------------------------------------------------------------------
// Missing index
// ------------------------------------------------------------------------------------------

fn missing_indexes(cx: &Cx<'_>, out: &mut Vec<Draft>) {
    let mut found: Vec<Draft> = Vec::new();
    for node in &cx.nodes {
        for g in json_list(node, "missingIndexes") {
            let impact = g.get("impact").and_then(Value::as_f64).unwrap_or(0.0);
            if impact < MIN_MISSING_INDEX_IMPACT {
                continue;
            }
            let Some(name) = text_of(&g, "table") else {
                continue;
            };
            let table = TableRef {
                database: text_of(&g, "database"),
                schema: text_of(&g, "schema"),
                name,
                alias: None,
                path: None,
            };
            let mut key_names = strings_of(&g, "equality");
            let eq_len = key_names.len().min(MAX_KEY_COLUMNS);
            for c in strings_of(&g, "inequality") {
                if !key_names.iter().any(|k| k.eq_ignore_ascii_case(&c)) {
                    key_names.push(c);
                }
            }
            key_names.truncate(MAX_KEY_COLUMNS);
            if key_names.is_empty() {
                continue;
            }
            let mut include: Vec<String> = strings_of(&g, "include")
                .into_iter()
                .filter(|c| !key_names.iter().any(|k| k.eq_ignore_ascii_case(c)))
                .collect();
            let mut caveats = vec![caveat("missingIndexHint", false)];
            if include.len() > INCLUDE_GIVE_UP {
                include.clear();
                caveats.push(caveat("includeTrimmed", false));
            } else if include.len() > MAX_INCLUDE_COLUMNS {
                include.truncate(MAX_INCLUDE_COLUMNS);
                caveats.push(caveat("includeTrimmed", false));
            }
            let cand = IndexCandidate {
                table: table.clone(),
                keys: key_names
                    .iter()
                    .map(|c| IndexKey::column(cx.ctx, c, false))
                    .collect(),
                eq_count: eq_len.min(key_names.len()),
                include,
            };
            let node_ids = nodes_of_table(cx, &table);
            let mut d = Draft::new(
                AdviceRule::MssqlMissingIndex,
                Severity::Warning,
                Confidence::Medium,
            )
            .param("columns", columns_text(&cand))
            .param("impact", fmt_num((impact * 10.0).round() / 10.0));
            if !cand.include.is_empty() {
                d.params.insert("include".into(), cand.include.join(", "));
            }
            d.node_ids = node_ids.clone();
            // The scans of this table that carry a "large table" warning are answered here.
            for id in &node_ids {
                if warning_of(cx, *id, PlanRule::FullScanLargeTable).is_some()
                    && !d.from_warnings.contains(&PlanRule::FullScanLargeTable)
                {
                    d.from_warnings.push(PlanRule::FullScanLargeTable);
                }
            }
            d.examined = node_ids
                .iter()
                .filter_map(|i| cx.node(*i))
                .filter(|n| n.kind == PlanNodeKind::FullScan)
                .filter_map(|n| n.actual_rows_total.or(n.est_rows))
                .fold(None, |a: Option<f64>, v| Some(a.map_or(v, |a| a.max(v))));
            d.evidence.push(ev(
                "missingIndexImpact",
                node_ids.first().copied(),
                impact,
                EvidenceUnit::Pct,
                false,
            ));
            d.fingerprint_key = format!(
                "{}|{}",
                cand.keys
                    .iter()
                    .filter_map(|k| k.column.clone())
                    .collect::<Vec<_>>()
                    .join(","),
                cand.include.join(",")
            );
            d.needs_metadata = true;
            d.caveats = caveats;
            d.table = Some(table);
            d.candidate = Some(cand);
            found.push(d);
        }
    }
    // The optimizer's hint supersedes the generic "index for this filter" advice of the table.
    for d in &found {
        if let Some(t) = &d.table {
            out.retain(|o| {
                !(o.rule == AdviceRule::IndexForFilter
                    && o.table.as_ref().is_some_and(|ot| same_table(ot, t)))
            });
        }
    }
    out.extend(found);
}

// ------------------------------------------------------------------------------------------
// Key lookup
// ------------------------------------------------------------------------------------------

fn key_lookup(cx: &Cx<'_>, node: &PlanNode, out: &mut Vec<Draft>) {
    if node.kind != PlanNodeKind::KeyLookup {
        return;
    }
    let Some(table) = table_ref(node) else { return };
    let executions = node.loops.unwrap_or(0);
    let share = self_share(cx, node);
    if executions < LOOKUP_MIN_EXECUTIONS && share.is_none_or(|s| s < LOOKUP_MIN_SHARE) {
        return;
    }
    let parent = cx.parent_of(node.id);
    // The index the plan seeks before the lookup: the sibling that reads the same table.
    let sibling = parent.and_then(|p| {
        p.children
            .iter()
            .filter(|c| c.id != node.id)
            .filter_map(|c| first_scan(c))
            .find(|s| table_ref(s).is_some_and(|t| same_table(&t, &table)))
    });
    let index_name = sibling.and_then(|s| s.object.as_ref().and_then(|o| o.index.clone()));
    // Key columns of that index: the catalog when read, else the seek keys of the plan.
    let meta_cols = index_name.as_deref().and_then(|ix| {
        cx.ctx
            .table_meta(table.schema.as_deref(), &table.name)
            .and_then(|m| m.indexes.iter().find(|i| i.name.eq_ignore_ascii_case(ix)))
            .map(|i| i.columns.clone())
            .filter(|c| !c.is_empty())
    });
    let from_catalog = meta_cols.is_some();
    let keys: Vec<String> = meta_cols.unwrap_or_else(|| {
        sibling
            .map(|s| {
                let conj = conjuncts(cx, s, &[PredicateRole::IndexCond]);
                let (eq, range) = index_columns(&conj, true);
                eq.into_iter()
                    .chain(range)
                    .map(|c| c.name)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    });
    let mut include: Vec<String> = output_columns(node)
        .into_iter()
        .filter(|c| !keys.iter().any(|k| k.eq_ignore_ascii_case(c)))
        .collect();
    let mut d = Draft::new(
        AdviceRule::MssqlKeyLookup,
        Severity::Warning,
        if from_catalog {
            Confidence::High
        } else {
            Confidence::Medium
        },
    )
    .node(node.id);
    if let Some(s) = sibling {
        d.node_ids.push(s.id);
    }
    d.table = Some(table.clone());
    d.params.insert("loops".into(), executions.to_string());
    if let Some(ix) = &index_name {
        d.params.insert("usedIndex".into(), ix.clone());
    }
    d.evidence.push(ev(
        "lookupExecutions",
        Some(node.id),
        executions as f64,
        EvidenceUnit::Count,
        node.loops.is_some(),
    ));
    if let Some(t) = node.self_time_ms {
        d.evidence
            .push(ev("selfTime", Some(node.id), t, EvidenceUnit::Ms, true));
    }
    if !include.is_empty() {
        d.evidence
            .push(ev_text("lookupColumns", Some(node.id), include.join(", ")));
    }
    // The warning on the nested loop above is answered here as well.
    if let Some(p) = parent {
        if warning_of(cx, p.id, PlanRule::NestedLoopManyRows).is_some() {
            d.from_warnings.push(PlanRule::NestedLoopManyRows);
            d.node_ids.push(p.id);
        }
    }
    let mut cand = None;
    if include.len() > INCLUDE_GIVE_UP {
        d.caveats.push(caveat("includeTrimmed", false));
        d.actions
            .push(check_action("reviewSelectList", None, ActionScope::Query));
    } else if keys.is_empty() || include.is_empty() {
        d.confidence = Confidence::Low;
        if !include.is_empty() {
            d.params.insert("include".into(), include.join(", "));
        }
        d.actions.push(check_action(
            "reviewLookupColumns",
            None,
            ActionScope::Object,
        ));
    } else {
        if include.len() > MAX_INCLUDE_COLUMNS {
            include.truncate(MAX_INCLUDE_COLUMNS);
            d.caveats.push(caveat("includeTrimmed", false));
        }
        let c = IndexCandidate {
            table: table.clone(),
            keys: keys
                .iter()
                .take(MAX_KEY_COLUMNS)
                .map(|k| IndexKey::column(cx.ctx, k, false))
                .collect(),
            eq_count: keys.len().min(MAX_KEY_COLUMNS),
            include,
        };
        d.params.insert("columns".into(), columns_text(&c));
        d.params.insert("include".into(), c.include.join(", "));
        d.fingerprint_key = format!(
            "{}|{}",
            keys.join(",").to_lowercase(),
            c.include.join(",").to_lowercase()
        );
        if !from_catalog {
            d.caveats.push(caveat("indexKeysFromSeek", true));
        }
        cand = Some(c);
    }
    if d.fingerprint_key.is_empty() {
        d.fingerprint_key = format!("lookup:{}", index_name.unwrap_or_default().to_lowercase());
    }
    d.candidate = cand;
    out.push(d);
}

/// The nested loop above a key lookup that already has its own advice does not need the generic
/// "review the join strategy" pointer.
fn fold_key_lookup_into_nested_loop(out: &mut Vec<Draft>) {
    let covered: Vec<u32> = out
        .iter()
        .filter(|d| d.rule == AdviceRule::MssqlKeyLookup)
        .filter(|d| d.from_warnings.contains(&PlanRule::NestedLoopManyRows))
        .flat_map(|d| d.node_ids.iter().copied())
        .collect();
    out.retain(|d| {
        !(d.rule == AdviceRule::NestedLoopInnerScan
            && d.candidate.is_none()
            && !d.node_ids.is_empty()
            && d.node_ids.iter().all(|n| covered.contains(n)))
    });
}

// ------------------------------------------------------------------------------------------
// Implicit conversion
// ------------------------------------------------------------------------------------------

/// What `CONVERT_IMPLICIT(type, operand, style)` converts.
#[derive(Debug, PartialEq)]
struct Convert {
    /// Target type (`nvarchar(50)`).
    to: String,
    /// Dotted identifier parts of the operand without brackets (`shop`, `dbo`, `Orders`,
    /// `Code`); a parameter is a single part starting with `@`.
    parts: Vec<String>,
}

impl Convert {
    fn column(&self) -> Option<&str> {
        let last = self.parts.last()?;
        (self.parts.len() >= 2 || !last.starts_with('@')).then_some(last.as_str())
    }

    fn table(&self) -> Option<&str> {
        let n = self.parts.len();
        (n >= 2 && self.column().is_some()).then(|| self.parts[n - 2].as_str())
    }

    fn schema(&self) -> Option<&str> {
        let n = self.parts.len();
        (n >= 3 && self.column().is_some()).then(|| self.parts[n - 3].as_str())
    }
}

fn parse_convert(expr: &str) -> Option<Convert> {
    let lower = expr.to_ascii_lowercase();
    let start = lower.find("convert_implicit(")? + "convert_implicit(".len();
    let rest = &expr[start..];
    // Target type: up to the first comma outside parentheses.
    let mut depth = 0i32;
    let mut comma = None;
    for (i, ch) in rest.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' if depth == 0 => break,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                comma = Some(i);
                break;
            }
            _ => {}
        }
    }
    let comma = comma?;
    let to = rest[..comma].trim().to_string();
    // Operand: bracketed identifiers joined by dots, up to ` as `, `,` or `)`.
    let operand = &rest[comma + 1..];
    let mut parts: Vec<String> = Vec::new();
    let mut chars = operand.trim_start().chars().peekable();
    while chars.peek() == Some(&'[') {
        chars.next();
        let mut name = String::new();
        while let Some(c) = chars.next() {
            if c == ']' {
                if chars.peek() == Some(&']') {
                    chars.next();
                    name.push(']');
                    continue;
                }
                break;
            }
            name.push(c);
        }
        parts.push(name);
        if chars.peek() == Some(&'.') {
            chars.next();
        } else {
            break;
        }
    }
    (!parts.is_empty() && !to.is_empty()).then_some(Convert { to, parts })
}

fn implicit_conversion(cx: &Cx<'_>, node: &PlanNode, out: &mut Vec<Draft>) {
    for item in json_list(node, "planAffectingConvert") {
        let issue = text_of(&item, "issue").unwrap_or_default();
        let expression = text_of(&item, "expression").unwrap_or_default();
        let seek = issue.eq_ignore_ascii_case("Seek Plan");
        let Some(conv) = parse_convert(&expression) else {
            continue;
        };
        let (severity, confidence) = if seek {
            (Severity::Warning, Confidence::High)
        } else {
            (Severity::Info, Confidence::Medium)
        };
        let mut d = Draft::new(AdviceRule::ImplicitConversion, severity, confidence)
            .param("issue", issue.clone())
            .param("convertTo", conv.to.clone())
            .param("expression", expression.clone());
        let column = conv.column().map(str::to_string);
        let table = conv.table().map(|name| TableRef {
            database: None,
            schema: conv.schema().map(str::to_string),
            name: name.to_string(),
            alias: None,
            path: None,
        });
        if let Some(c) = &column {
            d.params.insert("column".into(), c.clone());
        }
        let mut column_type = None;
        if let (Some(t), Some(c)) = (&table, &column) {
            column_type = cx
                .ctx
                .table_meta(t.schema.as_deref(), &t.name)
                .and_then(|m| m.columns.iter().find(|x| x.name.eq_ignore_ascii_case(c)))
                .map(|x| x.type_name.clone());
        }
        if let Some(ty) = &column_type {
            d.params.insert("columnType".into(), ty.clone());
        }
        d.node_ids = table
            .as_ref()
            .map(|t| nodes_of_table(cx, t))
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| vec![node.id]);
        d.evidence.push(ev_text(
            "convertIssue",
            Some(node.id),
            if expression.is_empty() {
                issue.clone()
            } else {
                format!("{issue}: {expression}")
            },
        ));
        d.actions.push(AdviceAction {
            kind: ActionKind::Rewrite,
            label_key: if column.is_some() {
                "matchColumnType".into()
            } else {
                "matchParameterType".into()
            },
            sql: None,
            template: false,
            scope: ActionScope::Query,
            lock: None,
            params: d.params.clone(),
        });
        if let (Some(t), Some(c)) = (&table, &column) {
            let q = |s: &str| s.replace('\'', "''");
            let schema = t
                .schema
                .as_deref()
                .map(|s| format!("TABLE_SCHEMA = '{}' AND ", q(s)))
                .unwrap_or_default();
            d.actions.push(check_action(
                "showColumnType",
                Some(&format!(
                    "SELECT COLUMN_NAME, DATA_TYPE, CHARACTER_MAXIMUM_LENGTH FROM INFORMATION_SCHEMA.COLUMNS WHERE {schema}TABLE_NAME = '{}' AND COLUMN_NAME = '{}';",
                    q(&t.name),
                    q(c)
                )),
                ActionScope::Object,
            ));
        }
        d.fingerprint_key = format!(
            "{}|{}",
            column.clone().unwrap_or_else(|| expression.clone()),
            issue.to_lowercase()
        );
        d.table = table;
        out.push(d);
    }
}

// ------------------------------------------------------------------------------------------
// Spills
// ------------------------------------------------------------------------------------------

/// Hint appended to the statement; never "ready to run" on its own.
fn hint_action(label: &str, sql: &str) -> AdviceAction {
    AdviceAction {
        kind: ActionKind::Hint,
        label_key: label.into(),
        sql: Some(sql.into()),
        template: true,
        scope: ActionScope::Query,
        lock: Some(LockRisk::None),
        params: BTreeMap::new(),
    }
}

fn spill(cx: &Cx<'_>, node: &PlanNode, out: &mut Vec<Draft>) {
    let Some(detail) = node.extra.get("spill") else {
        return;
    };
    let sort = node.kind == PlanNodeKind::Sort || node.op.to_ascii_lowercase().contains("sort");
    let rule = if sort {
        AdviceRule::SortSpill
    } else {
        AdviceRule::HashSpill
    };
    let severity = warning_of(cx, node.id, PlanRule::SpillToDisk).unwrap_or(Severity::Warning);
    let mut d = Draft::new(rule, severity, Confidence::Medium)
        .node(node.id)
        .param("detail", detail.clone());
    d.from_warnings.push(PlanRule::SpillToDisk);
    let tables = tables_below(node, 2);
    if let Some(l) = num(node, "spillLevel") {
        d.evidence.push(ev(
            "spillLevel",
            Some(node.id),
            l,
            EvidenceUnit::Count,
            true,
        ));
    }
    if let Some(w) = num(node, "spillWritesToTempDb") {
        d.evidence.push(ev(
            "tempdbWrites",
            Some(node.id),
            w,
            EvidenceUnit::Blocks,
            true,
        ));
        d.params.insert("tempdbWrites".into(), fmt_num(w));
    }
    if let Some(g) = num(node, "spillGrantedMemoryKb") {
        d.evidence.push(ev(
            "memoryGranted",
            Some(node.id),
            kb_to_bytes(g),
            EvidenceUnit::Bytes,
            true,
        ));
    }
    if let Some(u) = num(node, "spillUsedMemoryKb") {
        d.evidence.push(ev(
            "memoryUsed",
            Some(node.id),
            kb_to_bytes(u),
            EvidenceUnit::Bytes,
            true,
        ));
    }
    for t in &tables {
        if let Some(a) = statistics_action(cx.ctx, Some(t)) {
            d.actions.push(a);
        }
    }
    d.actions.push(hint_action(
        "minGrantPercentHint",
        "OPTION (MIN_GRANT_PERCENT = 10)",
    ));
    d.caveats.push(caveat("memoryGrantFeedback", false));
    d.fingerprint_key = format!(
        "{}:{}",
        node.op,
        tables
            .iter()
            .map(|t| t.name.as_str())
            .collect::<Vec<_>>()
            .join(",")
    );
    d.table = tables.into_iter().next();
    out.push(d);
}

// ------------------------------------------------------------------------------------------
// Warnings of the showplan
// ------------------------------------------------------------------------------------------

fn no_join_predicate(node: &PlanNode, out: &mut Vec<Draft>) {
    if node.extra.get("NoJoinPredicate").map(String::as_str) != Some("true") {
        return;
    }
    let mut d = Draft::new(
        AdviceRule::MssqlNoJoinPredicate,
        Severity::Critical,
        Confidence::High,
    )
    .node(node.id);
    d.share = ShareSpec::Inclusive(node.id);
    if let Some(r) = node.actual_rows_total.or(node.est_rows) {
        d.evidence.push(ev(
            "outputRows",
            Some(node.id),
            r,
            EvidenceUnit::Rows,
            node.actual_rows_total.is_some(),
        ));
    }
    d.actions
        .push(check_action("addJoinPredicate", None, ActionScope::Query));
    d.fingerprint_key = node.op.clone();
    out.push(d);
}

fn statistics_name(table: &str, column: &str) -> String {
    let clean = |s: &str| {
        s.chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect::<String>()
    };
    let mut name = format!("st_{}_{}", clean(table), clean(column));
    name.truncate(128);
    name
}

fn no_statistics(cx: &Cx<'_>, node: &PlanNode, out: &mut Vec<Draft>) {
    let items = json_list(node, "columnsWithNoStatistics");
    if items.is_empty() {
        return;
    }
    // One advice per table; columns without a table go to the first table below the node.
    let fallback = tables_below(node, 1).into_iter().next();
    let mut groups: Vec<(TableRef, Vec<String>)> = Vec::new();
    for it in &items {
        let Some(column) = text_of(it, "column") else {
            continue;
        };
        let table = match text_of(it, "table") {
            Some(name) => TableRef {
                database: text_of(it, "database"),
                schema: text_of(it, "schema"),
                name,
                alias: None,
                path: None,
            },
            None => match &fallback {
                Some(t) => t.clone(),
                None => continue,
            },
        };
        match groups.iter_mut().find(|(t, _)| same_table(t, &table)) {
            Some((_, cols)) => {
                if !cols.iter().any(|c| c.eq_ignore_ascii_case(&column)) {
                    cols.push(column);
                }
            }
            None => groups.push((table, vec![column])),
        }
    }
    for (table, cols) in groups {
        let mut d = Draft::new(
            AdviceRule::MssqlNoStatistics,
            Severity::Warning,
            Confidence::High,
        )
        .node(node.id)
        .param("columns", cols.join(", "));
        d.share = ShareSpec::Inclusive(node.id);
        let q = |s: &str| quote_ident(cx.ctx.dialect, s);
        let name = match &table.schema {
            Some(s) => format!("{}.{}", q(s), q(&table.name)),
            None => q(&table.name),
        };
        for c in &cols {
            d.actions.push(AdviceAction {
                kind: ActionKind::Statistics,
                label_key: "createStatistics".into(),
                sql: Some(format!(
                    "CREATE STATISTICS {} ON {name} ({});",
                    q(&statistics_name(&table.name, c)),
                    q(c)
                )),
                template: false,
                scope: ActionScope::Object,
                lock: Some(LockRisk::None),
                params: BTreeMap::from([("column".to_string(), c.clone())]),
            });
        }
        d.actions.push(check_action(
            "autoCreateStatistics",
            None,
            ActionScope::Database,
        ));
        d.evidence.push(ev_text(
            "columnsWithoutStatistics",
            Some(node.id),
            cols.join(", "),
        ));
        d.fingerprint_key = cols.join(",").to_lowercase();
        d.table = Some(table);
        out.push(d);
    }
}

fn excessive_grant(cx: &Cx<'_>, node: &PlanNode, out: &mut Vec<Draft>) {
    let Some(kind) = node.extra.get("memoryGrantWarning") else {
        return;
    };
    if !kind.to_ascii_lowercase().contains("excessive") {
        return;
    }
    let mut d = Draft::new(
        AdviceRule::MssqlExcessiveGrant,
        Severity::Info,
        Confidence::Low,
    )
    .node(node.id);
    d.share = ShareSpec::Unknown;
    if let Some(g) = num(node, "grantedMemoryKb") {
        d.evidence.push(ev(
            "memoryGranted",
            Some(node.id),
            kb_to_bytes(g),
            EvidenceUnit::Bytes,
            true,
        ));
    }
    if let Some(u) = num(node, "maxUsedMemoryKb") {
        d.evidence.push(ev(
            "memoryUsed",
            Some(node.id),
            kb_to_bytes(u),
            EvidenceUnit::Bytes,
            true,
        ));
    }
    let tables = tables_below(node, 2);
    let tables = if tables.is_empty() {
        tables_below(&cx.tree.root, 2)
    } else {
        tables
    };
    for t in &tables {
        if let Some(a) = statistics_action(cx.ctx, Some(t)) {
            d.actions.push(a);
        }
    }
    d.actions.push(hint_action(
        "maxGrantPercentHint",
        "OPTION (MAX_GRANT_PERCENT = 5)",
    ));
    d.caveats.push(caveat("memoryGrantFeedback", false));
    d.fingerprint_key = "excessiveGrant".into();
    out.push(d);
}

// ------------------------------------------------------------------------------------------
// Statistics refresh with the right sampling
// ------------------------------------------------------------------------------------------

/// `UPDATE STATISTICS t;` becomes `... WITH FULLSCAN;` (`WITH SAMPLE 25 PERCENT` above ten
/// million rows).
fn statistics_with_fullscan(cx: &Cx<'_>, out: &mut [Draft]) {
    for d in out
        .iter_mut()
        .filter(|d| d.rule == AdviceRule::StaleStatistics)
    {
        let rows = d
            .node_ids
            .iter()
            .filter_map(|i| cx.node(*i))
            .filter_map(|n| num(n, "TableCardinality"))
            .fold(None, |a: Option<f64>, v| Some(a.map_or(v, |a| a.max(v))))
            .or_else(|| {
                d.table.as_ref().and_then(|t| {
                    cx.ctx
                        .table_meta(t.schema.as_deref(), &t.name)
                        .and_then(|m| m.estimated_rows)
                        .map(|r| r as f64)
                })
            });
        let sample = rows.is_some_and(|r| r > SAMPLE_ABOVE_ROWS);
        let mut changed = false;
        for a in d
            .actions
            .iter_mut()
            .filter(|a| a.kind == ActionKind::Statistics)
        {
            if let Some(sql) = a.sql.as_mut() {
                if sql.starts_with("UPDATE STATISTICS ")
                    && sql.ends_with(';')
                    && !sql.contains(" WITH ")
                {
                    sql.pop();
                    sql.push_str(if sample {
                        " WITH SAMPLE 25 PERCENT;"
                    } else {
                        " WITH FULLSCAN;"
                    });
                    changed = true;
                }
            }
        }
        if changed && !sample && !d.caveats.iter().any(|c| c.key == "fullscanIo") {
            d.caveats.push(caveat("fullscanIo", false));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_convert_implicit_on_a_column() {
        let c = parse_convert(
            "CONVERT_IMPLICIT(nvarchar(50),[shop].[dbo].[Orders].[Code] as [o].[Code],0)",
        )
        .unwrap();
        assert_eq!(c.to, "nvarchar(50)");
        assert_eq!(
            (c.database_less()),
            (Some("Code"), Some("Orders"), Some("dbo"))
        );
    }

    #[test]
    fn convert_of_a_parameter_has_no_column() {
        let c = parse_convert("CONVERT_IMPLICIT(varchar(8000),[@1],0)").unwrap();
        assert_eq!(c.column(), None);
        assert_eq!(c.to, "varchar(8000)");
    }

    #[test]
    fn convert_embedded_in_a_comparison() {
        let c = parse_convert("[x].[a]=CONVERT_IMPLICIT(int,[t].[b],0)").unwrap();
        assert_eq!((c.column(), c.table()), (Some("b"), Some("t")));
        assert!(parse_convert("1=1").is_none());
        assert!(parse_convert("CONVERT_IMPLICIT(").is_none());
    }

    impl Convert {
        fn database_less(&self) -> (Option<&str>, Option<&str>, Option<&str>) {
            (self.column(), self.table(), self.schema())
        }
    }
}
