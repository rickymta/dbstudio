//! Rules shared by every dialect (design section 4.0) and the helpers the dialect modules use.

use std::collections::BTreeMap;

use dbstudio_core::{PlanFormat, ScriptDialect};

use super::index::{IndexCandidate, IndexKey, MAX_KEY_COLUMNS, quote_ident};
use super::model::{
    ActionKind, ActionScope, AdviceAction, AdviceRule, Caveat, Confidence, Evidence, EvidenceUnit,
    LockRisk, PredicateRole, TableRef,
};
use super::predicate::{
    ColRef, CondKind, Conjunct, parse_conjuncts, parse_mongo_filter, parse_sort_key,
};
use super::{Cx, Draft, ShareSpec};
use crate::json::fmt_num;
use crate::{PlanNode, PlanNodeKind, PlanRule, PlanWarning, Severity};

// ------------------------------------------------------------------------------------------
// Helpers
// ------------------------------------------------------------------------------------------

/// Numeric attribute of a node.
pub(crate) fn num(node: &PlanNode, key: &str) -> Option<f64> {
    node.extra
        .get(key)
        .and_then(|v| v.trim().parse::<f64>().ok())
}

/// Evidence figure.
pub(crate) fn ev(
    key: &str,
    node: Option<u32>,
    value: f64,
    unit: EvidenceUnit,
    actual: bool,
) -> Evidence {
    Evidence {
        key: key.into(),
        node_id: node,
        value: Some(value),
        text: None,
        unit: Some(unit),
        actual,
    }
}

/// Evidence text.
pub(crate) fn ev_text(key: &str, node: Option<u32>, text: impl Into<String>) -> Evidence {
    Evidence {
        key: key.into(),
        node_id: node,
        value: None,
        text: Some(text.into()),
        unit: None,
        actual: false,
    }
}

/// Table a node reads, as named by the plan.
pub(crate) fn table_ref(node: &PlanNode) -> Option<TableRef> {
    let o = node.object.as_ref()?;
    // Indexes without a relation (`Bitmap Index Scan`) only have the index name.
    if o.index.is_some()
        && o.schema.is_none()
        && node.kind == PlanNodeKind::IndexScan
        && node.op == "Bitmap Index Scan"
    {
        return None;
    }
    Some(TableRef {
        database: None,
        schema: o.schema.clone(),
        name: o.name.clone(),
        alias: o.alias.clone(),
        path: None,
    })
}

/// Names a node's table goes by in conditions: alias and relation name.
pub(crate) fn own_names(node: &PlanNode) -> Vec<String> {
    let mut v = Vec::new();
    if let Some(o) = &node.object {
        if let Some(a) = &o.alias {
            v.push(a.clone());
        }
        v.push(o.name.clone());
    }
    v
}

/// Conjuncts of the node's predicates with one of `roles`, classified against its own table.
pub(crate) fn conjuncts(cx: &Cx<'_>, node: &PlanNode, roles: &[PredicateRole]) -> Vec<Conjunct> {
    let own = own_names(node);
    let mut out = Vec::new();
    for p in node.predicates.iter().filter(|p| roles.contains(&p.role)) {
        if cx.tree.format == PlanFormat::MongoExplain {
            out.extend(parse_mongo_filter(&p.text));
        } else {
            out.extend(parse_conjuncts(cx.ctx.dialect, &p.text, &own));
        }
    }
    out
}

/// Equality columns and the first range column of `conj` (join columns count as equality when
/// `with_join`), without duplicates.
pub(crate) fn index_columns(conj: &[Conjunct], with_join: bool) -> (Vec<ColRef>, Option<ColRef>) {
    let mut eq: Vec<ColRef> = Vec::new();
    let mut range: Option<ColRef> = None;
    for c in conj {
        let Some(col) = &c.col else { continue };
        match c.kind {
            CondKind::Eq => push_unique(&mut eq, col),
            CondKind::Join if with_join => push_unique(&mut eq, col),
            CondKind::Range if range.is_none() => range = Some(col.clone()),
            _ => {}
        }
    }
    if let Some(r) = &range {
        if eq.iter().any(|e| e.key() == r.key()) {
            range = None;
        }
    }
    (eq, range)
}

fn push_unique(list: &mut Vec<ColRef>, col: &ColRef) {
    if !list.iter().any(|c| c.key() == col.key()) {
        list.push(col.clone());
    }
}

/// Candidate index from equality columns, optional sort columns and a range column
/// (equality, then sort, then range, as in the design's ESR order).
pub(crate) fn candidate(
    cx: &Cx<'_>,
    table: &TableRef,
    eq: &[ColRef],
    sort: &[(String, bool)],
    range: Option<&ColRef>,
) -> Option<IndexCandidate> {
    let mut keys: Vec<IndexKey> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for c in eq {
        if !seen.contains(&c.key()) {
            seen.push(c.key());
            keys.push(IndexKey::column(cx.ctx, &c.name, false));
        }
    }
    let eq_count = keys.len();
    for (name, desc) in sort {
        let k = name.to_lowercase();
        if !seen.contains(&k) {
            seen.push(k);
            keys.push(IndexKey::column(cx.ctx, name, *desc));
        }
    }
    if sort.is_empty() {
        if let Some(r) = range {
            if !seen.contains(&r.key()) {
                keys.push(IndexKey::column(cx.ctx, &r.name, false));
            }
        }
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

/// Comma separated column list for `params.columns`.
pub(crate) fn columns_text(c: &IndexCandidate) -> String {
    c.keys
        .iter()
        .map(|k| {
            k.column.clone().unwrap_or_else(|| k.sql.clone()) + if k.desc { " DESC" } else { "" }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn warning<'a>(cx: &'a Cx<'_>, node: u32, rule: PlanRule) -> Option<&'a PlanWarning> {
    cx.tree
        .warnings
        .iter()
        .find(|w| w.node_id == node && w.rule == rule)
}

/// A statistics refresh for `table` in the connection's dialect.
pub(crate) fn statistics_action(
    ctx: &super::AdvisorContext<'_>,
    table: Option<&TableRef>,
) -> Option<AdviceAction> {
    let t = table?;
    let q = |n: &str| quote_ident(ctx.dialect, n);
    let name = match &t.schema {
        Some(s) => format!("{}.{}", q(s), q(&t.name)),
        None => q(&t.name),
    };
    let sql = match ctx.dialect {
        ScriptDialect::Postgres => format!("ANALYZE {name};"),
        ScriptDialect::MySql { .. } => format!("ANALYZE TABLE {name};"),
        ScriptDialect::MsSql => format!("UPDATE STATISTICS {name};"),
        _ => return None,
    };
    let mut params = BTreeMap::new();
    params.insert("table".to_string(), t.name.clone());
    Some(AdviceAction {
        kind: ActionKind::Statistics,
        label_key: "refreshStatistics".into(),
        sql: Some(sql),
        template: false,
        scope: ActionScope::Object,
        lock: Some(LockRisk::None),
        params,
    })
}

/// A `check` action: something to look at, optionally with a query that shows it.
pub(crate) fn check_action(label_key: &str, sql: Option<&str>, scope: ActionScope) -> AdviceAction {
    AdviceAction {
        kind: ActionKind::Check,
        label_key: label_key.into(),
        sql: sql.map(str::to_string),
        template: false,
        scope,
        lock: None,
        params: BTreeMap::new(),
    }
}

/// Descend from `node` through single-child wrappers to the first node that reads a table.
pub(crate) fn first_scan(node: &PlanNode) -> Option<&PlanNode> {
    if node.object.is_some()
        && matches!(
            node.kind,
            PlanNodeKind::FullScan
                | PlanNodeKind::IndexScan
                | PlanNodeKind::IndexOnlyScan
                | PlanNodeKind::IndexSeek
                | PlanNodeKind::KeyLookup
        )
        && table_ref(node).is_some()
    {
        return Some(node);
    }
    if node.children.len() == 1 || node.kind == PlanNodeKind::Materialize {
        return node.children.first().and_then(first_scan);
    }
    // Bitmap Heap Scan has a single Bitmap Index Scan child; joins have two.
    None
}

fn collect_scans<'a>(node: &'a PlanNode, out: &mut Vec<&'a PlanNode>) {
    if node.object.is_some() {
        out.push(node);
    }
    for c in &node.children {
        collect_scans(c, out);
    }
}

fn has_join(node: &PlanNode) -> bool {
    matches!(
        node.kind,
        PlanNodeKind::NestedLoopJoin | PlanNodeKind::HashJoin | PlanNodeKind::MergeJoin
    ) || node.children.iter().any(has_join)
}

/// Rows read and kept by a scan node.
pub(crate) struct RowStats {
    pub kept: Option<f64>,
    pub examined: Option<f64>,
    /// Figures are measured.
    pub actual: bool,
}

/// Row figures of a scan: measured when the plan has them, else estimates (with the table size
/// from the catalog when known).
pub(crate) fn row_stats(cx: &Cx<'_>, node: &PlanNode, table: &TableRef) -> RowStats {
    let loops = node.loops.map_or(1.0, |l| l as f64);
    if let Some(kept) = node.actual_rows_total {
        let removed = num(node, "Rows Removed by Filter").map(|r| r * loops);
        let docs = num(node, "docsExamined").or_else(|| num(node, "rows_examined_per_scan"));
        let examined = match (removed, docs) {
            (Some(r), _) => Some(kept + r),
            (None, Some(d)) => Some(d.max(kept)),
            _ => Some(kept),
        };
        return RowStats {
            kept: Some(kept),
            examined,
            actual: true,
        };
    }
    let kept = node.est_rows.map(|r| r * loops);
    let total = cx
        .ctx
        .table_meta(table.schema.as_deref(), &table.name)
        .and_then(|m| m.estimated_rows)
        .map(|r| r as f64)
        .or_else(|| num(node, "rows_examined_per_scan"))
        .or_else(|| num(node, "docsExamined"));
    RowStats {
        kept,
        examined: total,
        actual: false,
    }
}

fn pct(p: f64) -> String {
    format!("{}%", fmt_num((p * 1000.0).round() / 10.0))
}

// ------------------------------------------------------------------------------------------
// Node rules
// ------------------------------------------------------------------------------------------

pub(crate) fn node_rules(cx: &Cx<'_>, node: &PlanNode, out: &mut Vec<Draft>) {
    if node.loops == Some(0) {
        return;
    }
    index_for_filter(cx, node, out);
    index_residual_filter(cx, node, out);
    index_for_sort(cx, node, out);
    nested_loop(cx, node, out);
    misestimate(cx, node, out);
}

fn index_for_filter(cx: &Cx<'_>, node: &PlanNode, out: &mut Vec<Draft>) {
    if node.kind != PlanNodeKind::FullScan {
        return;
    }
    let Some(table) = table_ref(node) else { return };
    let conj = conjuncts(cx, node, &[PredicateRole::Filter]);
    let (eq, range) = index_columns(&conj, false);
    if eq.is_empty() && range.is_none() {
        return;
    }
    let stats = row_stats(cx, node, &table);
    let rules = cx.ctx.rules;
    let selectivity = match (stats.kept, stats.examined) {
        (Some(k), Some(e)) if e > 0.0 => Some((k / e).min(1.0)),
        _ => None,
    };
    if stats.actual {
        let Some(examined) = stats.examined else {
            return;
        };
        if examined < rules.filter_min_examined {
            return;
        }
        if selectivity.is_some_and(|s| s > rules.filter_max_selectivity) {
            return;
        }
    } else {
        // Estimates: the output estimate must be sizeable, and a known table size must make the
        // filter selective.
        if let Some(sel) = selectivity {
            if stats
                .examined
                .is_some_and(|e| e < rules.filter_min_examined)
                || sel > rules.filter_max_selectivity
            {
                return;
            }
        } else if stats.kept.is_none_or(|k| k < rules.filter_min_examined) {
            return;
        }
    }
    let Some(cand) = candidate(cx, &table, &eq, &[], range.as_ref()) else {
        return;
    };
    let mut d = Draft::new(
        AdviceRule::IndexForFilter,
        warning(cx, node.id, PlanRule::FullScanLargeTable)
            .map_or(Severity::Warning, |w| w.severity),
        Confidence::High,
    )
    .node(node.id)
    .param("columns", columns_text(&cand));
    d.needs_actual = true;
    d.needs_metadata = !stats.actual && selectivity.is_none();
    d.examined = stats.examined.or(stats.kept);
    d.fingerprint_key = cand
        .keys
        .iter()
        .filter_map(|k| k.column.clone())
        .collect::<Vec<_>>()
        .join(",");
    if let Some(e) = stats.examined {
        d.evidence.push(ev(
            "rowsRead",
            Some(node.id),
            e,
            EvidenceUnit::Rows,
            stats.actual,
        ));
        d.params.insert("examined".into(), fmt_num(e));
    }
    if let Some(k) = stats.kept {
        d.evidence.push(ev(
            "rowsKept",
            Some(node.id),
            k,
            EvidenceUnit::Rows,
            stats.actual,
        ));
        d.params.insert("kept".into(), fmt_num(k));
    }
    if let Some(s) = selectivity {
        d.evidence.push(ev(
            "selectivity",
            Some(node.id),
            s * 100.0,
            EvidenceUnit::Pct,
            stats.actual,
        ));
        d.params.insert("selectivity".into(), pct(s));
    }
    if let Some(t) = node.self_time_ms {
        d.evidence
            .push(ev("selfTime", Some(node.id), t, EvidenceUnit::Ms, true));
    }
    if warning(cx, node.id, PlanRule::FullScanLargeTable).is_some() {
        d.from_warnings.push(PlanRule::FullScanLargeTable);
    }
    d.table = Some(table);
    d.candidate = Some(cand);
    out.push(d);
}

/// Index columns named by the index conditions of a node (and of its bitmap index child).
fn index_cond_columns(cx: &Cx<'_>, node: &PlanNode) -> (Vec<ColRef>, Option<ColRef>) {
    let mut conj = conjuncts(cx, node, &[PredicateRole::IndexCond]);
    let own = own_names(node);
    for c in &node.children {
        if c.op == "Bitmap Index Scan" {
            for p in c
                .predicates
                .iter()
                .filter(|p| p.role == PredicateRole::IndexCond)
            {
                conj.extend(parse_conjuncts(cx.ctx.dialect, &p.text, &own));
            }
        }
    }
    index_columns(&conj, true)
}

fn index_residual_filter(cx: &Cx<'_>, node: &PlanNode, out: &mut Vec<Draft>) {
    if !matches!(
        node.kind,
        PlanNodeKind::IndexScan | PlanNodeKind::IndexOnlyScan | PlanNodeKind::IndexSeek
    ) || node.op == "Bitmap Index Scan"
    {
        return;
    }
    let Some(table) = table_ref(node) else { return };
    let Some(removed) =
        num(node, "Rows Removed by Filter").map(|r| r * node.loops.map_or(1.0, |l| l as f64))
    else {
        return;
    };
    let kept = node.actual_rows_total.unwrap_or(0.0);
    let rules = cx.ctx.rules;
    let examined = kept + removed;
    if examined < rules.filter_min_examined || removed < rules.residual_ratio * kept.max(1.0) {
        return;
    }
    let conj = conjuncts(cx, node, &[PredicateRole::Filter]);
    let (feq, frange) = index_columns(&conj, false);
    if feq.is_empty() && frange.is_none() {
        return;
    }
    let (ieq, irange) = index_cond_columns(cx, node);
    let mut eq = ieq.clone();
    for c in &feq {
        push_unique(&mut eq, c);
    }
    let range = irange.clone().or(frange.clone());
    let new_cols = eq.len() > ieq.len() || (irange.is_none() && frange.is_some());
    if !new_cols {
        return;
    }
    let Some(cand) = candidate(cx, &table, &eq, &[], range.as_ref()) else {
        return;
    };
    let mut d = Draft::new(
        AdviceRule::IndexResidualFilter,
        Severity::Warning,
        Confidence::Medium,
    )
    .node(node.id)
    .param("columns", columns_text(&cand));
    d.needs_metadata = ieq.is_empty() && irange.is_none();
    d.examined = Some(examined);
    d.fingerprint_key = cand
        .keys
        .iter()
        .filter_map(|k| k.column.clone())
        .collect::<Vec<_>>()
        .join(",");
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
        kept,
        EvidenceUnit::Rows,
        true,
    ));
    d.evidence.push(ev(
        "rowsRemovedByFilter",
        Some(node.id),
        removed,
        EvidenceUnit::Rows,
        true,
    ));
    d.params.insert("examined".into(), fmt_num(examined));
    d.params.insert("kept".into(), fmt_num(kept));
    if let Some(ix) = node.object.as_ref().and_then(|o| o.index.clone()) {
        d.params.insert("usedIndex".into(), ix);
    }
    d.table = Some(table);
    d.candidate = Some(cand);
    out.push(d);
}

fn index_for_sort(cx: &Cx<'_>, node: &PlanNode, out: &mut Vec<Draft>) {
    if node.kind != PlanNodeKind::Sort {
        return;
    }
    let spilled = warning(cx, node.id, PlanRule::SpillToDisk).is_some();
    let rows = node.actual_rows_total.or(node.est_rows);
    let limit_rows = cx
        .parent_of(node.id)
        .filter(|p| p.kind == PlanNodeKind::Limit)
        .and_then(|p| p.actual_rows_total.or(p.est_rows));
    let rules = cx.ctx.rules;
    let big_input = matches!((rows, limit_rows), (Some(r), Some(l))
        if r >= 10.0 * l.max(1.0) && r >= rules.filter_min_examined);
    if !big_input && !spilled {
        return;
    }
    if has_join(node) {
        return;
    }
    let mut scans = Vec::new();
    for c in &node.children {
        collect_scans(c, &mut scans);
    }
    let mut tables: Vec<TableRef> = Vec::new();
    for s in &scans {
        if let Some(t) = table_ref(s) {
            if !tables
                .iter()
                .any(|x| x.name == t.name && x.schema == t.schema)
            {
                tables.push(t);
            }
        }
    }
    let [table] = tables.as_slice() else { return };
    let own: Vec<String> = scans.iter().flat_map(|s| own_names(s)).collect();
    let mut sort_cols: Vec<(String, bool)> = Vec::new();
    for p in node
        .predicates
        .iter()
        .filter(|p| p.role == PredicateRole::SortKey)
    {
        let Some(k) = parse_sort_key(cx.ctx.dialect, &p.text) else {
            return;
        };
        let own_col = k
            .col
            .qualifier
            .as_ref()
            .is_none_or(|q| own.iter().any(|o| o.eq_ignore_ascii_case(q)));
        if k.computed || !own_col {
            return;
        }
        sort_cols.push((k.col.name.clone(), k.desc));
    }
    if sort_cols.is_empty() {
        return;
    }
    let mut eq: Vec<ColRef> = Vec::new();
    for s in &scans {
        let conj = conjuncts(cx, s, &[PredicateRole::Filter, PredicateRole::IndexCond]);
        let (e, _) = index_columns(&conj, false);
        for c in e {
            if !sort_cols
                .iter()
                .any(|(n, _)| n.eq_ignore_ascii_case(&c.name))
            {
                push_unique(&mut eq, &c);
            }
        }
    }
    let Some(cand) = candidate(cx, table, &eq, &sort_cols, None) else {
        return;
    };
    let mut d = Draft::new(
        AdviceRule::IndexForSort,
        Severity::Warning,
        Confidence::Medium,
    )
    .node(node.id)
    .param("columns", columns_text(&cand));
    d.fingerprint_key = cand
        .keys
        .iter()
        .filter_map(|k| k.column.clone())
        .collect::<Vec<_>>()
        .join(",");
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
    if let Some(l) = limit_rows {
        d.evidence
            .push(ev("limitRows", Some(node.id), l, EvidenceUnit::Rows, true));
    }
    d.evidence.push(ev_text(
        "sortKey",
        Some(node.id),
        sort_cols
            .iter()
            .map(|(n, desc)| format!("{n}{}", if *desc { " DESC" } else { "" }))
            .collect::<Vec<_>>()
            .join(", "),
    ));
    d.needs_actual = true;
    if spilled {
        d.from_warnings.push(PlanRule::SpillToDisk);
        d.caveats.push(Caveat {
            key: "indexForSortSpill".into(),
            verify: false,
            params: BTreeMap::new(),
        });
    }
    d.table = Some(table.clone());
    d.candidate = Some(cand);
    out.push(d);
}

fn nested_loop(cx: &Cx<'_>, node: &PlanNode, out: &mut Vec<Draft>) {
    if node.kind != PlanNodeKind::NestedLoopJoin {
        return;
    }
    let Some(w) = warning(cx, node.id, PlanRule::NestedLoopManyRows) else {
        return;
    };
    let (outer, inner) = (node.children.first(), node.children.get(1));
    // Root cause is the statistics when the outer side was badly under-estimated.
    if let Some(o) = outer {
        if let Some(mw) = warning(cx, o.id, PlanRule::Misestimate) {
            if mw.params.get("direction").map(String::as_str) == Some("under") {
                let mut d = stale_statistics(cx, o, mw);
                d.from_warnings.push(PlanRule::NestedLoopManyRows);
                d.node_ids.push(node.id);
                out.push(d);
                return;
            }
        }
    }
    let loops = w.params.get("loops").cloned().unwrap_or_default();
    let mut d = Draft::new(
        AdviceRule::NestedLoopInnerScan,
        w.severity,
        Confidence::Medium,
    )
    .node(node.id)
    .param("loops", loops.clone());
    d.from_warnings.push(PlanRule::NestedLoopManyRows);
    d.share = ShareSpec::Inclusive(node.id);
    if let Ok(l) = loops.parse::<f64>() {
        d.evidence.push(ev(
            "loops",
            inner.map(|n| n.id),
            l,
            EvidenceUnit::Count,
            true,
        ));
    }
    if let Some(i) = inner.and_then(first_scan) {
        let table = table_ref(i);
        if let Some(t) = i.self_time_ms {
            d.evidence
                .push(ev("innerTime", Some(i.id), t, EvidenceUnit::Ms, true));
        }
        // An unindexed inner scan: index the join column (plus equality filters).
        if i.kind == PlanNodeKind::FullScan {
            let mut conj = conjuncts(cx, i, &[PredicateRole::Filter, PredicateRole::IndexCond]);
            let own = own_names(i);
            for p in node
                .predicates
                .iter()
                .filter(|p| matches!(p.role, PredicateRole::JoinFilter | PredicateRole::Filter))
            {
                conj.extend(parse_conjuncts(cx.ctx.dialect, &p.text, &own));
            }
            let (eq, range) = index_columns(&conj, true);
            if let (Some(t), false) = (&table, eq.is_empty() && range.is_none()) {
                if let Some(cand) = candidate(cx, t, &eq, &[], range.as_ref()) {
                    d.params.insert("columns".into(), columns_text(&cand));
                    d.fingerprint_key = cand
                        .keys
                        .iter()
                        .filter_map(|k| k.column.clone())
                        .collect::<Vec<_>>()
                        .join(",");
                    d.node_ids.insert(0, i.id);
                    d.table = table.clone();
                    d.candidate = Some(cand);
                    d.needs_actual = true;
                    out.push(d);
                    return;
                }
            }
        }
    }
    // The inner side is already indexed (or unknowable): only a pointer.
    d.confidence = Confidence::Low;
    d.severity = Severity::Info;
    d.actions
        .push(check_action("reviewJoinStrategy", None, ActionScope::Query));
    out.push(d);
}

/// Statistics advice for a misestimated node.
pub(crate) fn stale_statistics(cx: &Cx<'_>, node: &PlanNode, w: &PlanWarning) -> Draft {
    // The table: the node's own, else the first scan below it.
    let scan = if node.object.is_some() && table_ref(node).is_some() {
        Some(node)
    } else {
        let mut scans = Vec::new();
        collect_scans(node, &mut scans);
        scans
            .into_iter()
            .find(|s| table_ref(s).is_some() && !matches!(s.op.as_str(), "Bitmap Index Scan"))
    };
    let table = scan.and_then(table_ref);
    // Statistics cannot describe a function of a column or the result of a CTE / function.
    let function_filter = scan.is_some_and(|s| {
        conjuncts(cx, s, &[PredicateRole::Filter])
            .iter()
            .any(|c| c.kind == CondKind::NonSargable && c.func.as_deref() != Some("like"))
    });
    let expression_node = function_filter
        || matches!(
            node.op.as_str(),
            "Function Scan" | "CTE Scan" | "Subquery Scan" | "Values Scan" | "WorkTable Scan"
        );
    let mut d = Draft::new(
        AdviceRule::StaleStatistics,
        w.severity,
        if expression_node {
            Confidence::Low
        } else {
            Confidence::Medium
        },
    )
    .node(node.id);
    d.from_warnings.push(PlanRule::Misestimate);
    d.share = ShareSpec::Inclusive(cx.parent_of(node.id).map_or(node.id, |p| p.id));
    for k in ["factor", "estimated", "actual", "direction"] {
        if let Some(v) = w.params.get(k) {
            d.params.insert(k.to_string(), v.clone());
        }
    }
    if let Some(e) = node.est_rows {
        d.evidence.push(ev(
            "estimatedRows",
            Some(node.id),
            e,
            EvidenceUnit::Rows,
            false,
        ));
    }
    if let Some(a) = node.actual_rows {
        d.evidence
            .push(ev("actualRows", Some(node.id), a, EvidenceUnit::Rows, true));
    }
    if let Some(f) = w.params.get("factor").and_then(|f| f.parse::<f64>().ok()) {
        d.evidence.push(ev(
            "misestimateFactor",
            Some(node.id),
            f,
            EvidenceUnit::Ratio,
            true,
        ));
    }
    if expression_node {
        d.caveats.push(Caveat {
            key: "estimateFromExpression".into(),
            verify: false,
            params: BTreeMap::new(),
        });
    }
    match (&table, scan) {
        (Some(t), Some(s)) => {
            d.fingerprint_key = String::new();
            if let Some(a) = statistics_action(cx.ctx, Some(t)) {
                d.actions.push(a);
            }
            if cx.tree.format == PlanFormat::PostgresJson {
                d.actions
                    .extend(super::pg::extra_statistics_actions(cx, s, t));
            }
            d.table = Some(t.clone());
            if d.actions.is_empty() {
                d.actions
                    .push(check_action("reviewStatistics", None, ActionScope::Object));
            }
        }
        _ => {
            d.fingerprint_key = format!("node:{}", node.op);
            d.actions
                .push(check_action("reviewStatistics", None, ActionScope::Object));
            d.confidence = Confidence::Low;
        }
    }
    d
}

fn misestimate(cx: &Cx<'_>, node: &PlanNode, out: &mut Vec<Draft>) {
    if let Some(w) = warning(cx, node.id, PlanRule::Misestimate) {
        out.push(stale_statistics(cx, node, w));
    }
}

// ------------------------------------------------------------------------------------------
// Warnings that nobody answered
// ------------------------------------------------------------------------------------------

fn answered(drafts: &[Draft], node: u32, rule: PlanRule) -> bool {
    drafts
        .iter()
        .any(|d| d.from_warnings.contains(&rule) && d.node_ids.contains(&node))
}

/// Make sure every warning has at least one advice that names it (design section 2.2).
pub(crate) fn warning_answers(cx: &Cx<'_>, drafts: &mut Vec<Draft>) {
    for w in &cx.tree.warnings {
        if answered(drafts, w.node_id, w.rule) {
            continue;
        }
        let Some(node) = cx.node(w.node_id) else {
            continue;
        };
        let table = table_ref(node);
        let mut d = match w.rule {
            PlanRule::FullScanLargeTable => {
                let mut d = Draft::new(AdviceRule::FullScanReview, w.severity, Confidence::Low);
                if let Some(r) = w.params.get("rows") {
                    d.params.insert("rows".into(), r.clone());
                    if let Ok(v) = r.parse::<f64>() {
                        d.evidence.push(ev(
                            "rowsRead",
                            Some(node.id),
                            v,
                            EvidenceUnit::Rows,
                            node.actual_rows_total.is_some(),
                        ));
                    }
                }
                d.actions
                    .push(check_action("reviewFilter", None, ActionScope::Query));
                d
            }
            PlanRule::Misestimate => stale_statistics(cx, node, w),
            PlanRule::SpillToDisk => {
                let rule = if node.kind == PlanNodeKind::Sort {
                    AdviceRule::SortSpill
                } else {
                    AdviceRule::HashSpill
                };
                let mut d = Draft::new(rule, w.severity, Confidence::Low);
                if let Some(detail) = w.params.get("detail") {
                    d.evidence
                        .push(ev_text("spillDetail", Some(node.id), detail.clone()));
                }
                d.actions
                    .push(check_action("reviewMemory", None, ActionScope::Session));
                d
            }
            PlanRule::NestedLoopManyRows => {
                let mut d =
                    Draft::new(AdviceRule::NestedLoopInnerScan, w.severity, Confidence::Low);
                if let Some(l) = w.params.get("loops") {
                    d.params.insert("loops".into(), l.clone());
                }
                d.share = ShareSpec::Inclusive(node.id);
                d.actions
                    .push(check_action("reviewJoinStrategy", None, ActionScope::Query));
                d
            }
        };
        if d.node_ids.is_empty() {
            d.node_ids.push(node.id);
        }
        if !d.from_warnings.contains(&w.rule) {
            d.from_warnings.push(w.rule);
        }
        if d.table.is_none() {
            d.table = table;
        }
        if d.fingerprint_key.is_empty() && d.table.is_none() {
            d.fingerprint_key = format!("node:{}", node.op);
        }
        drafts.push(d);
    }
}
