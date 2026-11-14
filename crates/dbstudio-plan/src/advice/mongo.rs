//! MongoDB rules (design section 4.5).
//!
//! Indexes follow the ESR rule (Equality, Sort, Range). Rules never write the shell command
//! themselves: they hand an [`IndexCandidate`] to the engine (`index::render`).
//!
//! The parser keeps no [`crate::NodePredicate`] for MongoDB: the filter document of a `COLLSCAN`
//! or `FETCH` stage is the node's `condition`, the key pattern and sort pattern are in `extra`.
//! That keeps the generic `indexForFilter` rule out of the way: `mongoCollscan` is its MongoDB
//! counterpart and also knows about the sort above the scan.

use super::common::{ev, ev_text, index_columns, num, row_stats, table_ref};
use super::index::{IndexCandidate, IndexKey, MAX_KEY_COLUMNS};
use super::model::{
    ActionKind, ActionScope, AdviceAction, AdviceRule, Confidence, EvidenceUnit, TableRef,
};
use super::predicate::{ColRef, parse_mongo_filter};
use super::{Cx, Draft};
use crate::json::{self, JVal, Js, fmt_num};
use crate::{PlanNode, PlanNodeKind, PlanRule, Severity};

/// Rules evaluated for every node of a plan of this family.
pub(crate) fn node_rules(cx: &Cx<'_>, node: &PlanNode, out: &mut Vec<Draft>) {
    if node.loops == Some(0) {
        return;
    }
    collscan(cx, node, out);
    examined_ratio(cx, node, out);
    blocking_sort(cx, node, out);
    lookup_no_index(cx, node, out);
    unanchored_regex(node, out);
}

/// Rules evaluated once per plan.
pub(crate) fn tree_rules(_cx: &Cx<'_>, _out: &mut Vec<Draft>) {}

// ------------------------------------------------------------------------------------------
// Helpers
// ------------------------------------------------------------------------------------------

/// The collection a stage reads. The database goes in both `database` and `schema`: the
/// parser (and so the catalog lookup) names it `schema`, the shell command wants a database.
fn mongo_table(node: &PlanNode) -> Option<TableRef> {
    let mut t = table_ref(node)?;
    t.database = t.schema.clone();
    Some(t)
}

/// `{"ts": -1, "a": 1}` as (field, descending); empty when any member is not a plain direction.
fn parse_pattern(text: &str) -> Vec<(String, bool)> {
    let Ok(doc) = json::parse(text) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (k, v) in doc.members() {
        match v.as_f64() {
            Some(n) => out.push((k.clone(), n < 0.0)),
            None => return Vec::new(),
        }
    }
    out
}

/// Fields of an `IXSCAN` bound by a single point (`[42, 42]`): they act as equality fields.
fn point_fields(ixscan: &PlanNode) -> Vec<String> {
    let Some(bounds) = ixscan
        .condition
        .as_deref()
        .and_then(|c| json::parse(c).ok())
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (field, intervals) in bounds.members() {
        let list = intervals.items();
        let [one] = list else { continue };
        let Some(text) = one.as_str() else { continue };
        let inner = text.get(1..text.len().saturating_sub(1)).unwrap_or("");
        if let Some((lo, hi)) = inner.split_once(", ") {
            if lo == hi && !lo.contains("MinKey") && !lo.contains("MaxKey") {
                out.push(field.clone());
            }
        }
    }
    out
}

/// Filter document of a stage that filters (`COLLSCAN`, `FETCH`): its `condition`.
fn filter_text(node: &PlanNode) -> Option<&str> {
    match node.kind {
        PlanNodeKind::FullScan | PlanNodeKind::Fetch => {
            node.condition.as_deref().filter(|c| c.starts_with('{'))
        }
        _ => None,
    }
}

/// Equality fields then the first range field of a filter, without duplicates.
fn filter_columns(text: Option<&str>) -> (Vec<ColRef>, Option<ColRef>) {
    match text {
        Some(t) => index_columns(&parse_mongo_filter(t), false),
        None => (Vec::new(), None),
    }
}

/// Candidate in ESR order. `eq_count` counts the leading equality keys.
fn esr_candidate(
    cx: &Cx<'_>,
    table: &TableRef,
    eq: &[String],
    sort: &[(String, bool)],
    range: Option<&str>,
) -> Option<IndexCandidate> {
    let mut keys: Vec<IndexKey> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let mut push = |name: &str, desc: bool, keys: &mut Vec<IndexKey>| {
        let k = name.to_lowercase();
        if !seen.contains(&k) {
            seen.push(k);
            keys.push(IndexKey::column(cx.ctx, name, desc));
        }
    };
    for e in eq {
        push(e, false, &mut keys);
    }
    let eq_count = keys.len();
    for (name, desc) in sort {
        push(name, *desc, &mut keys);
    }
    if let Some(r) = range {
        push(r, false, &mut keys);
    }
    keys.truncate(MAX_KEY_COLUMNS);
    if keys.is_empty() {
        return None;
    }
    Some(IndexCandidate {
        table: table.clone(),
        eq_count: eq_count.min(keys.len()),
        keys,
        include: Vec::new(),
    })
}

fn columns_param(c: &IndexCandidate) -> String {
    super::common::columns_text(c)
}

fn fingerprint_of(c: &IndexCandidate) -> String {
    c.keys
        .iter()
        .filter_map(|k| k.column.clone())
        .collect::<Vec<_>>()
        .join(",")
}

/// The sort that sits above `scan` and is fed by it (through projections and the like).
fn sort_above<'a>(cx: &Cx<'a>, scan: &PlanNode) -> Option<(&'a PlanNode, Vec<(String, bool)>)> {
    let mut cur = cx.parent_of(scan.id);
    while let Some(p) = cur {
        match p.kind {
            PlanNodeKind::Sort => {
                let pattern = p
                    .extra
                    .get("sortPattern")
                    .map(|t| parse_pattern(t))
                    .unwrap_or_default();
                if !pattern.is_empty() {
                    return Some((p, pattern));
                }
            }
            PlanNodeKind::Other
            | PlanNodeKind::Fetch
            | PlanNodeKind::Gather
            | PlanNodeKind::Filter => {}
            _ => return None,
        }
        cur = cx.parent_of(p.id);
    }
    None
}

/// The chain of single-child stages below `node` (excluding it).
fn chain_below(node: &PlanNode) -> Vec<&PlanNode> {
    let mut out = Vec::new();
    let mut cur = node;
    while let [only] = cur.children.as_slice() {
        out.push(only);
        cur = only;
    }
    out
}

/// What `mongoCollscan` would do for a scan, shared with `mongoBlockingSort` so the two never
/// suggest the same index twice.
struct CollscanPlan<'a> {
    table: TableRef,
    candidate: IndexCandidate,
    examined: Option<f64>,
    kept: Option<f64>,
    selectivity: Option<f64>,
    actual: bool,
    sort: Option<(&'a PlanNode, Vec<(String, bool)>)>,
}

fn collscan_plan<'a>(cx: &Cx<'a>, node: &PlanNode) -> Option<CollscanPlan<'a>> {
    if node.kind != PlanNodeKind::FullScan || !matches!(node.op.as_str(), "COLLSCAN" | "scan") {
        return None;
    }
    let table = mongo_table(node)?;
    let (eq, range) = filter_columns(filter_text(node));
    let sort = sort_above(cx, node);
    if eq.is_empty() && range.is_none() && sort.is_none() {
        return None;
    }
    let stats = row_stats(cx, node, &table);
    let rules = cx.ctx.rules;
    let selectivity = match (stats.kept, stats.examined) {
        (Some(k), Some(e)) if e > 0.0 => Some((k / e).min(1.0)),
        _ => None,
    };
    if stats.actual {
        let examined = stats.examined?;
        if examined < rules.filter_min_examined {
            return None;
        }
        // An index does not pay off when the filter keeps most of the collection, unless it
        // also removes a sort.
        if sort.is_none() && selectivity.is_some_and(|s| s > rules.filter_max_selectivity) {
            return None;
        }
    }
    let eq_names: Vec<String> = eq.iter().map(|c| c.name.clone()).collect();
    let sort_fields = sort.as_ref().map(|(_, s)| s.as_slice()).unwrap_or(&[]);
    let candidate = esr_candidate(
        cx,
        &table,
        &eq_names,
        sort_fields,
        range.as_ref().map(|r| r.name.as_str()),
    )?;
    Some(CollscanPlan {
        table,
        candidate,
        examined: stats.examined,
        kept: stats.kept,
        selectivity,
        actual: stats.actual,
        sort,
    })
}

fn warning_on(cx: &Cx<'_>, node: u32, rule: PlanRule) -> Option<Severity> {
    cx.tree
        .warnings
        .iter()
        .find(|w| w.node_id == node && w.rule == rule)
        .map(|w| w.severity)
}

fn sort_key_text(sort: &[(String, bool)]) -> String {
    sort.iter()
        .map(|(n, desc)| format!("{n}{}", if *desc { " DESC" } else { "" }))
        .collect::<Vec<_>>()
        .join(", ")
}

// ------------------------------------------------------------------------------------------
// mongoCollscan
// ------------------------------------------------------------------------------------------

fn collscan(cx: &Cx<'_>, node: &PlanNode, out: &mut Vec<Draft>) {
    let Some(plan) = collscan_plan(cx, node) else {
        return;
    };
    let warn = warning_on(cx, node.id, PlanRule::FullScanLargeTable);
    let mut d = Draft::new(
        AdviceRule::MongoCollscan,
        warn.unwrap_or(Severity::Warning),
        if plan.actual {
            Confidence::High
        } else {
            Confidence::Low
        },
    )
    .node(node.id)
    .param("columns", columns_param(&plan.candidate));
    // Planner-only plans have no document counts: the confidence is already `low`; this adds the
    // "run Analyze to confirm" caveat.
    d.needs_actual = true;
    d.fingerprint_key = fingerprint_of(&plan.candidate);
    d.examined = plan.examined;
    if let Some(e) = plan.examined {
        d.evidence.push(ev(
            "rowsRead",
            Some(node.id),
            e,
            EvidenceUnit::Rows,
            plan.actual,
        ));
        d.params.insert("examined".into(), fmt_num(e));
    }
    if let Some(k) = plan.kept {
        d.evidence.push(ev(
            "rowsKept",
            Some(node.id),
            k,
            EvidenceUnit::Rows,
            plan.actual,
        ));
        d.params.insert("kept".into(), fmt_num(k));
    }
    if let Some(s) = plan.selectivity {
        d.evidence.push(ev(
            "selectivity",
            Some(node.id),
            s * 100.0,
            EvidenceUnit::Pct,
            plan.actual,
        ));
    }
    if let Some(t) = node.self_time_ms {
        d.evidence
            .push(ev("selfTime", Some(node.id), t, EvidenceUnit::Ms, true));
    }
    if warn.is_some() {
        d.from_warnings.push(PlanRule::FullScanLargeTable);
    }
    if let Some((sort, pattern)) = &plan.sort {
        // The index replaces the in-memory sort too.
        d = d.node(sort.id);
        d.evidence
            .push(ev_text("sortKey", Some(sort.id), sort_key_text(pattern)));
        if warning_on(cx, sort.id, PlanRule::SpillToDisk).is_some() {
            d.from_warnings.push(PlanRule::SpillToDisk);
        }
    }
    d.table = Some(plan.table);
    d.candidate = Some(plan.candidate);
    out.push(d);
}

// ------------------------------------------------------------------------------------------
// mongoExaminedRatio
// ------------------------------------------------------------------------------------------

fn examined_ratio(cx: &Cx<'_>, node: &PlanNode, out: &mut Vec<Draft>) {
    if node.kind != PlanNodeKind::Fetch || node.op != "FETCH" {
        return;
    }
    let Some(table) = mongo_table(node) else {
        return;
    };
    let (feq, frange) = filter_columns(filter_text(node));
    if feq.is_empty() && frange.is_none() {
        return;
    }
    let Some(returned) = node.actual_rows_total else {
        return;
    };
    let ix = node
        .children
        .iter()
        .find(|c| c.kind == PlanNodeKind::IndexScan && c.op == "IXSCAN");
    let docs = num(node, "docsExamined").unwrap_or(0.0);
    let keys = ix.and_then(|i| num(i, "keysExamined")).unwrap_or(0.0);
    let examined = docs.max(keys);
    let rules = cx.ctx.rules;
    if examined < rules.filter_min_examined || examined < rules.residual_ratio * returned.max(1.0) {
        return;
    }
    let Some(ix) = ix else { return };
    let Some(pattern) = ix.extra.get("keyPattern").map(|p| parse_pattern(p)) else {
        return;
    };
    if pattern.is_empty() {
        return;
    }
    let have: Vec<String> = pattern.iter().map(|(n, _)| n.to_lowercase()).collect();
    let new_eq: Vec<&ColRef> = feq.iter().filter(|c| !have.contains(&c.key())).collect();
    let new_range = frange.as_ref().filter(|r| !have.contains(&r.key()));
    if new_eq.is_empty() && new_range.is_none() {
        return;
    }
    // The existing key fields keep their place and direction; the filter fields come after.
    let mut keys_out: Vec<IndexKey> = pattern
        .iter()
        .map(|(n, desc)| IndexKey::column(cx.ctx, n, *desc))
        .collect();
    let points = point_fields(ix);
    let all_points = pattern.iter().all(|(n, _)| points.contains(n));
    for c in &new_eq {
        keys_out.push(IndexKey::column(cx.ctx, &c.name, false));
    }
    let eq_count = if all_points {
        pattern.len() + new_eq.len()
    } else {
        0
    };
    if let Some(r) = new_range {
        keys_out.push(IndexKey::column(cx.ctx, &r.name, false));
    }
    keys_out.truncate(MAX_KEY_COLUMNS);
    let cand = IndexCandidate {
        table: table.clone(),
        eq_count: eq_count.min(keys_out.len()),
        keys: keys_out,
        include: Vec::new(),
    };
    let ratio = examined / returned.max(1.0);
    let mut d = Draft::new(
        AdviceRule::MongoExaminedRatio,
        Severity::Warning,
        Confidence::Medium,
    )
    .node(node.id)
    .node(ix.id)
    .param("columns", columns_param(&cand))
    .param("ratio", fmt_num((ratio * 10.0).round() / 10.0));
    d.fingerprint_key = fingerprint_of(&cand);
    d.examined = Some(examined);
    d.evidence.push(ev(
        "rowsRead",
        Some(node.id),
        examined,
        EvidenceUnit::Rows,
        true,
    ));
    d.evidence.push(ev(
        "rowsKept",
        Some(node.id),
        returned,
        EvidenceUnit::Rows,
        true,
    ));
    d.evidence.push(ev(
        "examinedRatio",
        Some(node.id),
        ratio,
        EvidenceUnit::Ratio,
        true,
    ));
    if let Some(name) = ix.object.as_ref().and_then(|o| o.index.clone()) {
        d.params.insert("usedIndex".into(), name);
    }
    d.table = Some(table);
    d.candidate = Some(cand);
    out.push(d);
}

// ------------------------------------------------------------------------------------------
// mongoBlockingSort
// ------------------------------------------------------------------------------------------

fn blocking_sort(cx: &Cx<'_>, node: &PlanNode, out: &mut Vec<Draft>) {
    if node.kind != PlanNodeKind::Sort {
        return;
    }
    // Only the query level SORT stage has a pattern; a `$sort` after `$group` cannot use an index.
    let pattern = node
        .extra
        .get("sortPattern")
        .map(|t| parse_pattern(t))
        .unwrap_or_default();
    if pattern.is_empty() {
        return;
    }
    let spilled = node.extra.get("usedDisk").map(String::as_str) == Some("true");
    let rows = node
        .children
        .first()
        .and_then(|c| c.actual_rows_total)
        .or(node.actual_rows_total);
    let rules = cx.ctx.rules;
    if !spilled && rows.is_none_or(|r| r < rules.filter_min_examined) {
        return;
    }
    let below = chain_below(node);
    // A full scan feeding the sort is `mongoCollscan`'s: it suggests one index for both.
    if below
        .iter()
        .any(|n| n.kind == PlanNodeKind::FullScan && collscan_plan(cx, n).is_some())
    {
        return;
    }
    let Some(table) = below.iter().find_map(|n| mongo_table(n)) else {
        return;
    };
    let mut eq: Vec<String> = Vec::new();
    let mut range: Option<String> = None;
    let sort_names: Vec<String> = pattern.iter().map(|(n, _)| n.to_lowercase()).collect();
    for n in &below {
        let mut found: Vec<String> = Vec::new();
        if n.kind == PlanNodeKind::IndexScan {
            found.extend(point_fields(n));
        }
        let (feq, frange) = filter_columns(filter_text(n));
        found.extend(feq.into_iter().map(|c| c.name));
        for f in found {
            if !sort_names.contains(&f.to_lowercase())
                && !eq.iter().any(|e| e.eq_ignore_ascii_case(&f))
            {
                eq.push(f);
            }
        }
        if range.is_none() {
            range = frange.map(|r| r.name);
        }
    }
    if let Some(r) = &range {
        if sort_names.contains(&r.to_lowercase()) || eq.iter().any(|e| e.eq_ignore_ascii_case(r)) {
            range = None;
        }
    }
    let Some(cand) = esr_candidate(cx, &table, &eq, &pattern, range.as_deref()) else {
        return;
    };
    let mut d = Draft::new(
        AdviceRule::MongoBlockingSort,
        Severity::Warning,
        Confidence::Medium,
    )
    .node(node.id)
    .param("columns", columns_param(&cand));
    d.fingerprint_key = fingerprint_of(&cand);
    d.examined = rows;
    if let Some(r) = rows {
        d.evidence.push(ev(
            "sortRows",
            Some(node.id),
            r,
            EvidenceUnit::Rows,
            node.actual_rows_total.is_some(),
        ));
        d.params.insert("rows".into(), fmt_num(r));
    }
    d.evidence
        .push(ev_text("sortKey", Some(node.id), sort_key_text(&pattern)));
    if spilled {
        d.evidence
            .push(ev_text("sortMethod", Some(node.id), "usedDisk"));
        if warning_on(cx, node.id, PlanRule::SpillToDisk).is_some() {
            d.from_warnings.push(PlanRule::SpillToDisk);
        }
    }
    d.table = Some(table);
    d.candidate = Some(cand);
    out.push(d);
}

// ------------------------------------------------------------------------------------------
// mongoLookupNoIndex
// ------------------------------------------------------------------------------------------

fn lookup_no_index(cx: &Cx<'_>, node: &PlanNode, out: &mut Vec<Draft>) {
    if !matches!(node.op.as_str(), "$lookup" | "EQ_LOOKUP") {
        return;
    }
    let (Some(from), Some(foreign)) = (node.extra.get("from"), node.extra.get("foreignField"))
    else {
        return;
    };
    let strategy = node.extra.get("strategy").map(String::as_str);
    let scans = num(node, "collectionScans").unwrap_or(0.0);
    // MongoDB 6.0 names the strategy; before that only the scan counter tells.
    let confidence = match strategy {
        Some("NestedLoopJoin") => Confidence::High,
        None if scans > 0.0 => Confidence::Low,
        _ => return,
    };
    let db = cx
        .tree
        .root
        .extra
        .get("namespace")
        .and_then(|ns| ns.split_once('.').map(|(d, _)| d.to_string()));
    let table = TableRef {
        database: db.clone(),
        schema: db,
        name: from.clone(),
        alias: None,
        path: None,
    };
    let Some(cand) = esr_candidate(cx, &table, std::slice::from_ref(foreign), &[], None) else {
        return;
    };
    let mut d = Draft::new(
        AdviceRule::MongoLookupNoIndex,
        Severity::Warning,
        confidence,
    )
    .node(node.id)
    .param("columns", columns_param(&cand))
    .param("from", from.clone())
    .param("foreignField", foreign.clone());
    d.fingerprint_key = fingerprint_of(&cand);
    if let Some(s) = strategy {
        d.evidence.push(ev_text("lookupStrategy", Some(node.id), s));
        d.params.insert("strategy".into(), s.to_string());
    }
    if let Some(docs) = num(node, "totalDocsExamined") {
        d.evidence.push(ev(
            "docsExamined",
            Some(node.id),
            docs,
            EvidenceUnit::Rows,
            true,
        ));
        d.examined = Some(docs);
    }
    if scans > 0.0 {
        d.evidence.push(ev(
            "collectionScans",
            Some(node.id),
            scans,
            EvidenceUnit::Count,
            true,
        ));
    }
    if let Some(t) = node.self_time_ms {
        d.evidence
            .push(ev("selfTime", Some(node.id), t, EvidenceUnit::Ms, true));
    }
    d.table = Some(table);
    d.candidate = Some(cand);
    out.push(d);
}

// ------------------------------------------------------------------------------------------
// mongoUnanchoredRegex
// ------------------------------------------------------------------------------------------

/// `(field, pattern, options)` of every regular expression in a filter document.
fn find_regexes(js: &Js, field: Option<&str>, depth: u32, out: &mut Vec<(String, String, String)>) {
    if depth > 16 {
        return;
    }
    let JVal::Obj(members) = &js.val else {
        if let JVal::Arr(items) = &js.val {
            for i in items {
                find_regexes(i, field, depth + 1, out);
            }
        }
        return;
    };
    let options = js.text("$options").unwrap_or("").to_string();
    for (k, v) in members {
        match k.as_str() {
            "$regex" => {
                if let Some(p) = v.as_str() {
                    out.push((
                        field.unwrap_or("").to_string(),
                        p.to_string(),
                        options.clone(),
                    ));
                }
            }
            "$regularExpression" => {
                let p = v.text("pattern").unwrap_or("").to_string();
                let o = v.text("options").unwrap_or("").to_string();
                out.push((field.unwrap_or("").to_string(), p, o));
            }
            "$options" => {}
            _ if k.starts_with('$') => find_regexes(v, field, depth + 1, out),
            _ => find_regexes(v, Some(k), depth + 1, out),
        }
    }
}

fn unanchored_regex(node: &PlanNode, out: &mut Vec<Draft>) {
    let Some(text) = filter_text(node) else {
        return;
    };
    let Ok(doc) = json::parse(text) else { return };
    let mut found = Vec::new();
    find_regexes(&doc, None, 0, &mut found);
    for (field, pattern, options) in found {
        let anchored = pattern.starts_with('^') && !pattern.starts_with("^.*");
        let insensitive = options.contains('i');
        if anchored && !insensitive {
            continue;
        }
        let mut d = Draft::new(
            AdviceRule::MongoUnanchoredRegex,
            Severity::Info,
            Confidence::Low,
        )
        .node(node.id)
        .param("field", field.clone())
        .param("pattern", pattern.clone());
        d.fingerprint_key = format!("{field}:{pattern}:{options}");
        d.evidence.push(ev_text(
            "regexPattern",
            Some(node.id),
            if options.is_empty() {
                pattern
            } else {
                format!("{pattern} ({options})")
            },
        ));
        d.actions.push(AdviceAction {
            kind: ActionKind::Check,
            label_key: "useTextSearch".into(),
            sql: None,
            template: false,
            scope: ActionScope::Query,
            lock: None,
            params: Default::default(),
        });
        d.table = mongo_table(node);
        out.push(d);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::advice::model::PlanAdvice;
    use crate::{PlanRules, parse_and_analyze};
    use dbstudio_core::{PlanFormat, RawPlan};
    use std::collections::BTreeMap;

    fn advise(text: &str) -> Vec<PlanAdvice> {
        let raw = RawPlan {
            format: PlanFormat::MongoExplain,
            text: text.into(),
            statement: String::new(),
            analyzed: false,
            rolled_back: false,
            server_version: "7.0.14".into(),
            captured_at: "2025-01-01T00:00:00Z".into(),
            extra: BTreeMap::new(),
        };
        parse_and_analyze(&raw, &PlanRules::default())
            .expect("plan parses")
            .advice
    }

    fn rules_of(a: &[PlanAdvice]) -> Vec<AdviceRule> {
        a.iter().map(|x| x.rule).collect()
    }

    const COLLSCAN_STATS: &str = r#"{"queryPlanner":{"namespace":"shop.orders","winningPlan":
        {"stage":"COLLSCAN","filter":{"status":{"$eq":"late"},"total":{"$gt":10}}}},
        "executionStats":{"nReturned":50,"executionTimeMillis":400,"totalDocsExamined":500000,
        "totalKeysExamined":0,"executionStages":{"stage":"COLLSCAN","nReturned":50,
        "executionTimeMillisEstimate":380,"docsExamined":500000,
        "filter":{"status":{"$eq":"late"},"total":{"$gt":10}}}}}"#;

    #[test]
    fn collscan_with_stats_is_high_and_esr() {
        let a = advise(COLLSCAN_STATS);
        let c = a
            .iter()
            .find(|x| x.rule == AdviceRule::MongoCollscan)
            .unwrap();
        assert_eq!(c.confidence, Confidence::High);
        assert_eq!(
            c.params.get("columns").map(String::as_str),
            Some("status, total")
        );
        assert!(c.from_warnings.contains(&PlanRule::FullScanLargeTable));
    }

    #[test]
    fn small_or_unselective_collscan_is_ignored() {
        let small = COLLSCAN_STATS.replace("500000", "900");
        assert!(!rules_of(&advise(&small)).contains(&AdviceRule::MongoCollscan));
        // Keeps 60% of the collection: an index would not help.
        let wide = COLLSCAN_STATS
            .replace("\"nReturned\":50,", "\"nReturned\":300000,")
            .replace("\"nReturned\":50", "\"nReturned\":300000");
        assert!(!rules_of(&advise(&wide)).contains(&AdviceRule::MongoCollscan));
    }

    #[test]
    fn unusable_filters_give_no_index() {
        let t = COLLSCAN_STATS.replace(
            r#""filter":{"status":{"$eq":"late"},"total":{"$gt":10}}"#,
            r#""filter":{"status":{"$ne":"late"}}"#,
        );
        assert!(!rules_of(&advise(&t)).contains(&AdviceRule::MongoCollscan));
    }

    #[test]
    fn lookup_nested_loop_asks_for_a_foreign_index() {
        let text = r#"{"stages":[{"$cursor":{"queryPlanner":{"namespace":"shop.orders",
          "winningPlan":{"stage":"COLLSCAN"}}}},
          {"$lookup":{"from":"customers","as":"c","localField":"customerId","foreignField":"cid"},
           "strategy":"NestedLoopJoin","totalDocsExamined":9000,"collectionScans":120,
           "nReturned":120,"executionTimeMillisEstimate":90}]}"#;
        let a = advise(text);
        let l = a
            .iter()
            .find(|x| x.rule == AdviceRule::MongoLookupNoIndex)
            .unwrap();
        assert_eq!(l.confidence, Confidence::High);
        assert_eq!(l.table.as_ref().unwrap().name, "customers");
        assert_eq!(l.params.get("columns").map(String::as_str), Some("cid"));
        // Without the strategy and without scans nothing is said.
        let none = text
            .replace(r#""strategy":"NestedLoopJoin","#, "")
            .replace("\"collectionScans\":120,", "");
        assert!(!rules_of(&advise(&none)).contains(&AdviceRule::MongoLookupNoIndex));
        // Pre-6.0: only the scan counter, low confidence.
        let old = text.replace(r#""strategy":"NestedLoopJoin","#, "");
        let a = advise(&old);
        let l = a
            .iter()
            .find(|x| x.rule == AdviceRule::MongoLookupNoIndex)
            .unwrap();
        assert_eq!(l.confidence, Confidence::Low);
    }

    #[test]
    fn regex_rules() {
        let plan = |filter: &str| {
            format!(
                r#"{{"queryPlanner":{{"namespace":"shop.users","winningPlan":
                {{"stage":"COLLSCAN","filter":{filter}}}}}}}"#
            )
        };
        let unanchored = advise(&plan(r#"{"name":{"$regex":"smith"}}"#));
        assert!(rules_of(&unanchored).contains(&AdviceRule::MongoUnanchoredRegex));
        let ci = advise(&plan(r#"{"name":{"$regex":"^smith","$options":"i"}}"#));
        assert!(rules_of(&ci).contains(&AdviceRule::MongoUnanchoredRegex));
        let canonical = advise(&plan(
            r#"{"name":{"$regularExpression":{"pattern":"smith","options":""}}}"#,
        ));
        assert!(rules_of(&canonical).contains(&AdviceRule::MongoUnanchoredRegex));
        let anchored = advise(&plan(r#"{"name":{"$regex":"^smith"}}"#));
        assert!(!rules_of(&anchored).contains(&AdviceRule::MongoUnanchoredRegex));
    }

    #[test]
    fn examined_ratio_extends_the_used_index() {
        let text = r#"{"queryPlanner":{"namespace":"shop.orders","winningPlan":{"stage":"FETCH",
          "filter":{"total":{"$gt":100}},"inputStage":{"stage":"IXSCAN","keyPattern":{"customerId":1},
          "indexName":"customerId_1","indexBounds":{"customerId":["[42, 42]"]}}}},
          "executionStats":{"nReturned":10,"executionTimeMillis":80,"totalKeysExamined":60000,
          "totalDocsExamined":60000,"executionStages":{"stage":"FETCH","nReturned":10,
          "executionTimeMillisEstimate":70,"docsExamined":60000,"filter":{"total":{"$gt":100}},
          "inputStage":{"stage":"IXSCAN","nReturned":60000,"executionTimeMillisEstimate":30,
          "keyPattern":{"customerId":1},"indexName":"customerId_1","keysExamined":60000,
          "indexBounds":{"customerId":["[42, 42]"]}}}}}"#;
        let a = advise(text);
        let r = a
            .iter()
            .find(|x| x.rule == AdviceRule::MongoExaminedRatio)
            .unwrap();
        assert_eq!(
            r.params.get("columns").map(String::as_str),
            Some("customerId, total")
        );
        assert_eq!(
            r.params.get("usedIndex").map(String::as_str),
            Some("customerId_1")
        );
    }

    #[test]
    fn blocking_sort_over_an_index_adds_the_sort_field() {
        let text = r#"{"queryPlanner":{"namespace":"shop.orders","winningPlan":{"stage":"SORT",
          "sortPattern":{"createdAt":-1},"inputStage":{"stage":"FETCH","inputStage":{"stage":"IXSCAN",
          "keyPattern":{"customerId":1},"indexName":"customerId_1",
          "indexBounds":{"customerId":["[42, 42]"]}}}}},
          "executionStats":{"nReturned":40000,"executionTimeMillis":300,"totalKeysExamined":40000,
          "totalDocsExamined":40000,"executionStages":{"stage":"SORT","nReturned":40000,
          "executionTimeMillisEstimate":280,"sortPattern":{"createdAt":-1},"usedDisk":false,
          "inputStage":{"stage":"FETCH","nReturned":40000,"executionTimeMillisEstimate":120,
          "docsExamined":40000,"inputStage":{"stage":"IXSCAN","nReturned":40000,
          "executionTimeMillisEstimate":40,"keyPattern":{"customerId":1},"indexName":"customerId_1",
          "keysExamined":40000,"indexBounds":{"customerId":["[42, 42]"]}}}}}}"#;
        let a = advise(text);
        let s = a
            .iter()
            .find(|x| x.rule == AdviceRule::MongoBlockingSort)
            .unwrap();
        assert_eq!(
            s.params.get("columns").map(String::as_str),
            Some("customerId, createdAt DESC")
        );
    }
}
