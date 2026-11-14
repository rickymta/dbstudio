//! Oracle rules (design section 4.4).
//!
//! Plan rows carry `ACCESS_PREDICATES` / `FILTER_PREDICATES` (the parser turns them into
//! [`PredicateRole`]s) and, for a plan that ran, `LAST_STARTS`, `LAST_OUTPUT_ROWS` and the
//! work-area columns. A full scan does not report how many rows it read, so the selectivity of a
//! filter needs `NUM_ROWS` from the catalog (`TableMeta::estimated_rows`).

use std::collections::BTreeMap;

use super::common::{
    candidate, check_action, columns_text, conjuncts, ev, ev_text, index_columns, num, table_ref,
};
use super::model::{
    ActionKind, ActionScope, AdviceAction, AdviceRule, Caveat, Confidence, EvidenceUnit, LockRisk,
    PredicateRole, TableRef,
};
use super::predicate::{ColRef, CondKind};
use super::{Cx, Draft, ShareSpec};
use crate::json::fmt_num;
use crate::{PlanNode, PlanNodeKind, PlanRule, Severity};

/// Share of the statement from which an index skip / full scan is worth an advice.
const SKIP_SCAN_MIN_SHARE: f64 = 0.20;

fn caveat(key: &str, verify: bool) -> Caveat {
    Caveat {
        key: key.into(),
        verify,
        params: BTreeMap::new(),
    }
}

fn warning_severity(cx: &Cx<'_>, node: u32, rule: PlanRule) -> Option<Severity> {
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

fn plain(name: &str) -> ColRef {
    ColRef {
        qualifier: None,
        name: name.to_string(),
        cast: None,
    }
}

/// Single quotes doubled, for a PL/SQL or SQL string literal.
fn lit(s: &str) -> String {
    s.replace('\'', "''")
}

// ------------------------------------------------------------------------------------------
// Entry points
// ------------------------------------------------------------------------------------------

/// Rules evaluated for every node of a plan of this family.
pub(crate) fn node_rules(cx: &Cx<'_>, node: &PlanNode, out: &mut Vec<Draft>) {
    if node.loops == Some(0) {
        return;
    }
    full_scan_filter(cx, node, out);
    cartesian(cx, node, out);
    skip_scan(cx, node, out);
    residual_filter(cx, node, out);
    implicit_conversion(cx, node, out);
    work_area_spill(cx, node, out);
}

/// Rules evaluated once per plan.
pub(crate) fn tree_rules(cx: &Cx<'_>, out: &mut [Draft]) {
    gather_statistics(cx, out);
}

// ------------------------------------------------------------------------------------------
// TABLE ACCESS FULL with a filter
// ------------------------------------------------------------------------------------------

/// The common rule decides on estimates; for a plan that ran, the rows kept are measured but
/// the rows read are `NUM_ROWS` of the catalog, so the decision is made here.
fn full_scan_filter(cx: &Cx<'_>, node: &PlanNode, out: &mut Vec<Draft>) {
    if node.kind != PlanNodeKind::FullScan {
        return;
    }
    let (Some(kept), Some(table)) = (node.actual_rows, table_ref(node)) else {
        return;
    };
    let Some(total) = cx
        .ctx
        .table_meta(table.schema.as_deref(), &table.name)
        .and_then(|m| m.estimated_rows)
        .map(|r| r as f64)
    else {
        return;
    };
    let rules = cx.ctx.rules;
    if total < rules.filter_min_examined || total <= 0.0 {
        return;
    }
    let selectivity = (kept / total).min(1.0);
    if selectivity > rules.filter_max_selectivity {
        return;
    }
    let conj = conjuncts(cx, node, &[PredicateRole::Filter]);
    let (eq, range) = index_columns(&conj, false);
    let Some(cand) = candidate(cx, &table, &eq, &[], range.as_ref()) else {
        return;
    };
    let severity =
        warning_severity(cx, node.id, PlanRule::FullScanLargeTable).unwrap_or(Severity::Warning);
    let mut d = Draft::new(AdviceRule::IndexForFilter, severity, Confidence::Medium)
        .node(node.id)
        .param("columns", columns_text(&cand))
        .param("examined", fmt_num(total))
        .param("kept", fmt_num(kept));
    d.examined = Some(total);
    d.fingerprint_key = cand
        .keys
        .iter()
        .filter_map(|k| k.column.clone())
        .collect::<Vec<_>>()
        .join(",");
    d.evidence.push(ev(
        "rowsRead",
        Some(node.id),
        total,
        EvidenceUnit::Rows,
        false,
    ));
    d.evidence.push(ev(
        "rowsKept",
        Some(node.id),
        kept,
        EvidenceUnit::Rows,
        true,
    ));
    d.evidence.push(ev(
        "selectivity",
        Some(node.id),
        selectivity * 100.0,
        EvidenceUnit::Pct,
        false,
    ));
    if let Some(t) = node.self_time_ms {
        d.evidence
            .push(ev("selfTime", Some(node.id), t, EvidenceUnit::Ms, true));
    }
    if warning_severity(cx, node.id, PlanRule::FullScanLargeTable).is_some() {
        d.from_warnings.push(PlanRule::FullScanLargeTable);
    }
    d.table = Some(table);
    d.candidate = Some(cand);
    out.push(d);
}

// ------------------------------------------------------------------------------------------
// MERGE JOIN CARTESIAN
// ------------------------------------------------------------------------------------------

fn cartesian(cx: &Cx<'_>, node: &PlanNode, out: &mut Vec<Draft>) {
    if !node.op.eq_ignore_ascii_case("MERGE JOIN CARTESIAN") {
        return;
    }
    let rows = |n: Option<&PlanNode>| n.and_then(|n| n.actual_rows.or(n.est_rows));
    let (left, right) = (rows(node.children.first()), rows(node.children.get(1)));
    // A one-row side is a deliberate product (a single-row lookup or `DUAL`).
    if !(left.is_some_and(|r| r > 1.0) && right.is_some_and(|r| r > 1.0)) {
        return;
    }
    let actual = node.actual_rows_total.is_some();
    let mut d = Draft::new(
        AdviceRule::OracleCartesian,
        warning_severity(cx, node.id, PlanRule::NestedLoopManyRows).unwrap_or(Severity::Warning),
        Confidence::High,
    )
    .node(node.id);
    d.share = ShareSpec::Inclusive(node.id);
    if let (Some(l), Some(r)) = (left, right) {
        d.evidence.push(ev(
            "leftRows",
            node.children.first().map(|n| n.id),
            l,
            EvidenceUnit::Rows,
            actual,
        ));
        d.evidence.push(ev(
            "rightRows",
            node.children.get(1).map(|n| n.id),
            r,
            EvidenceUnit::Rows,
            actual,
        ));
        d.params.insert("product".into(), fmt_num(l * r));
    }
    d.actions
        .push(check_action("addJoinPredicate", None, ActionScope::Query));
    d.caveats.push(caveat("cartesianMayBeIntended", false));
    d.fingerprint_key = node.op.clone();
    out.push(d);
}

// ------------------------------------------------------------------------------------------
// INDEX SKIP SCAN / INDEX FULL SCAN
// ------------------------------------------------------------------------------------------

fn skip_scan(cx: &Cx<'_>, node: &PlanNode, out: &mut Vec<Draft>) {
    let full = node.op == "INDEX FULL SCAN";
    if node.op != "INDEX SKIP SCAN" && !full {
        return;
    }
    if self_share(cx, node).is_none_or(|s| s < SKIP_SCAN_MIN_SHARE) {
        return;
    }
    let Some(table) = table_ref(node) else { return };
    let conj = conjuncts(cx, node, &[PredicateRole::IndexCond, PredicateRole::Filter]);
    let (eq, range) = index_columns(&conj, false);
    let cand = candidate(cx, &table, &eq, &[], range.as_ref());
    let used = node.object.as_ref().and_then(|o| o.index.clone());
    let mut d = Draft::new(
        AdviceRule::OracleIndexSkipScan,
        Severity::Warning,
        if cand.is_some() {
            Confidence::Medium
        } else {
            Confidence::Low
        },
    )
    .node(node.id)
    .param("scan", node.op.clone());
    if let Some(u) = &used {
        d.params.insert("usedIndex".into(), u.clone());
    }
    if let Some(r) = node.actual_rows_total.or(node.est_rows) {
        d.evidence.push(ev(
            "rowsRead",
            Some(node.id),
            r,
            EvidenceUnit::Rows,
            node.actual_rows_total.is_some(),
        ));
    }
    if let Some(t) = node.self_time_ms {
        d.evidence
            .push(ev("selfTime", Some(node.id), t, EvidenceUnit::Ms, true));
    }
    if full {
        // An ordered full scan can be there to avoid a sort.
        d.caveats.push(caveat("fullScanMayAvoidSort", false));
    }
    match cand {
        Some(c) => {
            d.params.insert("columns".into(), columns_text(&c));
            d.fingerprint_key = c
                .keys
                .iter()
                .filter_map(|k| k.column.clone())
                .collect::<Vec<_>>()
                .join(",");
            d.candidate = Some(c);
        }
        None => {
            d.fingerprint_key = format!("{}:{}", node.op, used.unwrap_or_default());
            d.actions
                .push(check_action("reviewIndexOrder", None, ActionScope::Object));
        }
    }
    d.table = Some(table);
    out.push(d);
}

// ------------------------------------------------------------------------------------------
// TABLE ACCESS BY INDEX ROWID with a filter
// ------------------------------------------------------------------------------------------

fn residual_filter(cx: &Cx<'_>, node: &PlanNode, out: &mut Vec<Draft>) {
    if node.kind != PlanNodeKind::KeyLookup {
        return;
    }
    let Some(table) = table_ref(node) else { return };
    let Some(idx) = node.children.first().filter(|c| {
        matches!(
            c.kind,
            PlanNodeKind::IndexScan | PlanNodeKind::IndexSeek | PlanNodeKind::IndexOnlyScan
        )
    }) else {
        return;
    };
    let (Some(read), Some(kept)) = (idx.actual_rows_total, node.actual_rows_total) else {
        return;
    };
    let rules = cx.ctx.rules;
    if read < rules.filter_min_examined || read < rules.residual_ratio * kept.max(1.0) {
        return;
    }
    let conj = conjuncts(cx, node, &[PredicateRole::Filter]);
    let (feq, frange) = index_columns(&conj, false);
    if feq.is_empty() && frange.is_none() {
        return;
    }
    // Columns of the index in use: the catalog when read, else its access predicates.
    let used = idx.object.as_ref().and_then(|o| o.index.clone());
    let meta_cols: Option<Vec<String>> = used.as_deref().and_then(|ix| {
        cx.ctx
            .table_meta(table.schema.as_deref(), &table.name)
            .and_then(|m| m.indexes.iter().find(|i| i.name.eq_ignore_ascii_case(ix)))
            .map(|i| i.columns.clone())
            .filter(|c| !c.is_empty())
    });
    let (mut eq, mut range): (Vec<ColRef>, Option<ColRef>) = match &meta_cols {
        Some(cols) => (cols.iter().map(|c| plain(c)).collect(), None),
        None => {
            let c = conjuncts(cx, idx, &[PredicateRole::IndexCond, PredicateRole::Filter]);
            index_columns(&c, true)
        }
    };
    let known = eq.len();
    let had_range = range.is_some();
    for c in &feq {
        if !eq.iter().any(|e| e.key() == c.key()) {
            eq.push(c.clone());
        }
    }
    if range.is_none() {
        range = frange.clone();
    }
    if eq.len() == known && (had_range || range.is_none()) {
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
    .param("columns", columns_text(&cand))
    .param("examined", fmt_num(read))
    .param("kept", fmt_num(kept));
    d.node_ids.push(idx.id);
    d.needs_metadata = known == 0;
    d.examined = Some(read);
    d.fingerprint_key = cand
        .keys
        .iter()
        .filter_map(|k| k.column.clone())
        .collect::<Vec<_>>()
        .join(",");
    d.evidence
        .push(ev("rowsRead", Some(idx.id), read, EvidenceUnit::Rows, true));
    d.evidence.push(ev(
        "rowsKept",
        Some(node.id),
        kept,
        EvidenceUnit::Rows,
        true,
    ));
    if let Some(u) = used {
        d.params.insert("usedIndex".into(), u);
    }
    d.table = Some(table);
    d.candidate = Some(cand);
    out.push(d);
}

// ------------------------------------------------------------------------------------------
// Implicit conversion
// ------------------------------------------------------------------------------------------

fn implicit_conversion(cx: &Cx<'_>, node: &PlanNode, out: &mut Vec<Draft>) {
    let Some(table) = table_ref(node) else { return };
    let conj = conjuncts(
        cx,
        node,
        &[
            PredicateRole::Filter,
            PredicateRole::IndexCond,
            PredicateRole::JoinFilter,
        ],
    );
    for c in conj.iter().filter(|c| c.kind == CondKind::NonSargable) {
        let (Some(func), Some(col)) = (c.func.as_deref(), c.col.as_ref()) else {
            continue;
        };
        // `INTERNAL_FUNCTION` also wraps IN-lists and expressions: only a bare column counts.
        let hit = match func {
            "to_number" | "sys_op_c2c" => true,
            "internal_function" => c.simple,
            _ => false,
        };
        if !hit {
            continue;
        }
        let mut d = Draft::new(
            AdviceRule::ImplicitConversion,
            Severity::Warning,
            Confidence::High,
        )
        .node(node.id)
        .param("column", col.name.clone())
        .param("function", func.to_ascii_uppercase());
        d.evidence.push(ev_text(
            "conversionFunction",
            Some(node.id),
            format!("{}({})", func.to_ascii_uppercase(), col.name),
        ));
        d.actions.push(AdviceAction {
            kind: ActionKind::Rewrite,
            label_key: "matchColumnType".into(),
            sql: None,
            template: false,
            scope: ActionScope::Query,
            lock: None,
            params: d.params.clone(),
        });
        let owner = table
            .schema
            .as_deref()
            .map(|s| format!("owner = '{}' AND ", lit(s)))
            .unwrap_or_default();
        d.actions.push(check_action(
            "showColumnType",
            Some(&format!(
                "SELECT column_name, data_type, data_length FROM all_tab_columns WHERE {owner}table_name = '{}' AND column_name = '{}';",
                lit(&table.name),
                lit(&col.name)
            )),
            ActionScope::Object,
        ));
        d.fingerprint_key = format!("{}|{}", col.name, func);
        d.table = Some(table.clone());
        out.push(d);
    }
}

// ------------------------------------------------------------------------------------------
// Work-area spill
// ------------------------------------------------------------------------------------------

fn work_area_spill(cx: &Cx<'_>, node: &PlanNode, out: &mut Vec<Draft>) {
    let size = num(node, "LAST_TEMPSEG_SIZE")
        .filter(|s| *s > 0.0)
        .or_else(|| num(node, "TEMPSEG_SIZE").filter(|s| *s > 0.0));
    let Some(size) = size else { return };
    let mode = node.extra.get("LAST_EXECUTION");
    // `OPTIMAL` ran in memory whatever the temporary segment says.
    if mode.is_some_and(|m| m.eq_ignore_ascii_case("OPTIMAL")) {
        return;
    }
    let rule = if node.kind == PlanNodeKind::Sort {
        AdviceRule::SortSpill
    } else {
        AdviceRule::HashSpill
    };
    let mut d = Draft::new(rule, Severity::Info, Confidence::Low)
        .node(node.id)
        .param("tempsegBytes", fmt_num(size));
    if let Some(m) = mode {
        d.params.insert("execution".into(), m.clone());
        d.evidence
            .push(ev_text("workAreaExecution", Some(node.id), m.clone()));
    }
    d.evidence.push(ev(
        "tempsegSize",
        Some(node.id),
        size,
        EvidenceUnit::Bytes,
        true,
    ));
    if let Some(o) = num(node, "ESTIMATED_OPTIMAL_SIZE") {
        d.evidence.push(ev(
            "optimalSize",
            Some(node.id),
            o,
            EvidenceUnit::Bytes,
            false,
        ));
    }
    if warning_severity(cx, node.id, PlanRule::SpillToDisk).is_some() {
        d.from_warnings.push(PlanRule::SpillToDisk);
    }
    // Words only: the target is a server parameter, never set from here.
    d.actions
        .push(check_action("explainPgaTarget", None, ActionScope::Server));
    let tables = base_tables(node, 2);
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
// Statistics
// ------------------------------------------------------------------------------------------

/// `DBMS_STATS` block for `table`; the trailing `/` ends the PL/SQL block for the splitter.
fn gather_stats_action(table: &TableRef) -> AdviceAction {
    let owner = table
        .schema
        .as_deref()
        .map_or_else(|| "USER".to_string(), |s| format!("'{}'", lit(s)));
    AdviceAction {
        kind: ActionKind::Statistics,
        label_key: "refreshStatistics".into(),
        sql: Some(format!(
            "BEGIN DBMS_STATS.GATHER_TABLE_STATS(ownname => {owner}, tabname => '{}', method_opt => 'FOR ALL COLUMNS SIZE AUTO', cascade => TRUE); END;\n/",
            lit(&table.name)
        )),
        template: false,
        scope: ActionScope::Object,
        lock: Some(LockRisk::None),
        params: BTreeMap::from([("table".to_string(), table.name.clone())]),
    }
}

/// Tables read below `node` (a view or other row source is not a table).
fn base_tables(node: &PlanNode, max: usize) -> Vec<TableRef> {
    fn walk(n: &PlanNode, out: &mut Vec<TableRef>) {
        if matches!(
            n.kind,
            PlanNodeKind::FullScan
                | PlanNodeKind::IndexScan
                | PlanNodeKind::IndexSeek
                | PlanNodeKind::IndexOnlyScan
                | PlanNodeKind::KeyLookup
        ) {
            if let Some(t) = table_ref(n) {
                if !out.iter().any(|x| x.name == t.name && x.schema == t.schema) {
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

/// The common statistics advice has no Oracle command: give it the `DBMS_STATS` block, for the
/// tables the misestimated node reads (a `VIEW` row names a row source, not a table).
fn gather_statistics(cx: &Cx<'_>, out: &mut [Draft]) {
    for d in out
        .iter_mut()
        .filter(|d| d.rule == AdviceRule::StaleStatistics)
    {
        let view = d.table.as_ref().and_then(|t| {
            cx.nodes.iter().copied().find(|n| {
                n.kind == PlanNodeKind::Subquery
                    && table_ref(n).is_some_and(|v| v.name == t.name && v.schema == t.schema)
            })
        });
        let tables: Vec<TableRef> = match view {
            Some(n) => base_tables(n, 2),
            None => d.table.clone().into_iter().collect(),
        };
        if tables.is_empty() {
            continue;
        }
        d.actions.retain(|a| a.label_key != "reviewStatistics");
        if !d.actions.iter().any(|a| a.kind == ActionKind::Statistics) {
            d.actions.extend(tables.iter().map(gather_stats_action));
        }
        d.table = tables.into_iter().next();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_block_ends_with_slash_and_quotes_names() {
        let t = TableRef {
            database: None,
            schema: Some("SH'OP".into()),
            name: "ORDERS".into(),
            alias: None,
            path: None,
        };
        let sql = gather_stats_action(&t).sql.unwrap();
        assert!(sql.contains("ownname => 'SH''OP'"), "{sql}");
        assert!(sql.ends_with("END;\n/"), "{sql}");
        let t = TableRef { schema: None, ..t };
        assert!(
            gather_stats_action(&t)
                .sql
                .unwrap()
                .contains("ownname => USER")
        );
    }
}
