//! TiDB rules (design section 4.7). The generic ones of the MySQL family (`mysql.rs`) already
//! run on TiDB plans; this module adds what only TiDB reports: pseudo statistics, a residual
//! filter on the table side of an `IndexLookUp`, and the memory quota behind a spill.

use std::collections::BTreeMap;

use super::common::{check_action, ev, ev_text, num, statistics_action, table_ref};
use super::model::{
    ActionKind, ActionScope, AdviceAction, AdviceRule, Caveat, Confidence, EvidenceUnit, LockRisk,
    PredicateRole,
};
use super::mysql::{Residual, residual_draft, resolve_table, stmt_info};
use super::{Cx, Draft};
use crate::json::fmt_num;
use crate::{PlanNode, PlanNodeKind, PlanRule, Severity};

fn caveat(key: &str, verify: bool) -> Caveat {
    Caveat {
        key: key.into(),
        verify,
        params: BTreeMap::new(),
    }
}

/// Rules evaluated for every node of a TiDB plan.
pub(crate) fn node_rules(cx: &Cx<'_>, node: &PlanNode, out: &mut Vec<Draft>) {
    if node.loops == Some(0) {
        return;
    }
    pseudo_stats(cx, node, out);
    lookup_residual(cx, node, out);
    spill(cx, node, out);
}

/// Rules evaluated once per TiDB plan.
pub(crate) fn tree_rules(_cx: &Cx<'_>, _out: &mut Vec<Draft>) {}

/// `stats:pseudo` in the operator info: the table was never analysed, the optimiser guesses.
fn pseudo_stats(cx: &Cx<'_>, node: &PlanNode, out: &mut Vec<Draft>) {
    let pseudo = node
        .condition
        .as_deref()
        .is_some_and(|c| c.contains("stats:pseudo"));
    if !pseudo {
        return;
    }
    let Some(t) = table_ref(node) else { return };
    let table = resolve_table(&stmt_info(cx), &t);
    let mut d = Draft::new(
        AdviceRule::TidbPseudoStats,
        Severity::Warning,
        Confidence::High,
    )
    .node(node.id);
    d.fingerprint_key = "pseudo".into();
    if let Some(r) = node.est_rows {
        d.evidence.push(ev(
            "estimatedRows",
            Some(node.id),
            r,
            EvidenceUnit::Rows,
            false,
        ));
    }
    d.evidence
        .push(ev_text("statsState", Some(node.id), "pseudo"));
    if let Some(a) = statistics_action(cx.ctx, Some(&table)) {
        d.actions.push(a);
    } else {
        d.actions
            .push(check_action("reviewStatistics", None, ActionScope::Object));
    }
    d.table = Some(table);
    out.push(d);
}

/// `IndexLookUp`: a `Selection` over `TableRowIDScan` drops most of the rows the index scan
/// found, after paying a table lookup for each of them.
fn lookup_residual(cx: &Cx<'_>, node: &PlanNode, out: &mut Vec<Draft>) {
    if node.kind != PlanNodeKind::Filter {
        return;
    }
    let Some(rowid) = node
        .children
        .first()
        .filter(|c| c.kind == PlanNodeKind::KeyLookup && c.object.is_some())
    else {
        return;
    };
    // The index side is the sibling of this Selection under the IndexLookUp.
    let index_columns = cx
        .parent_of(node.id)
        .and_then(|p| p.children.iter().find(|c| c.id != node.id))
        .and_then(|c| c.extra.get("index_columns"))
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    // The conditions sit on the Selection, the table on its child.
    let own = super::common::own_names(rowid);
    let filter = node
        .predicates
        .iter()
        .filter(|p| p.role == PredicateRole::Filter)
        .flat_map(|p| super::predicate::parse_conjuncts(cx.ctx.dialect, &p.text, &own))
        .collect::<Vec<_>>();
    let actual = rowid.actual_rows_total.is_some();
    let (examined, kept) = if actual {
        (rowid.actual_rows_total, node.actual_rows_total)
    } else {
        (rowid.est_rows, node.est_rows)
    };
    let (Some(examined), Some(kept)) = (examined, kept) else {
        return;
    };
    let r = Residual {
        node,
        scan: rowid,
        filter,
        index_columns,
        examined,
        kept,
        actual,
    };
    let info = stmt_info(cx);
    out.extend(residual_draft(cx, &info, &r));
}

/// A sort or hash that wrote to disk: raise the per-query memory quota for the session.
fn spill(cx: &Cx<'_>, node: &PlanNode, out: &mut Vec<Draft>) {
    let Some(w) = cx
        .tree
        .warnings
        .iter()
        .find(|w| w.node_id == node.id && w.rule == PlanRule::SpillToDisk)
    else {
        return;
    };
    let disk = crate::tidb::spilled_bytes(&node.extra).unwrap_or(0.0);
    let mem = num(node, "memory_bytes").unwrap_or(0.0);
    let need = ((mem + disk) * 2.0).max(1.0);
    let mut quota: u64 = 8 << 20;
    while (quota as f64) < need && quota < (1 << 40) {
        quota <<= 1;
    }
    let is_sort = node.kind == PlanNodeKind::Sort;
    let rule = if is_sort {
        AdviceRule::SortSpill
    } else {
        AdviceRule::HashSpill
    };
    let mut d = Draft::new(rule, w.severity, Confidence::Medium).node(node.id);
    d.from_warnings.push(PlanRule::SpillToDisk);
    d.evidence.push(ev(
        "diskBytes",
        Some(node.id),
        disk,
        EvidenceUnit::Bytes,
        true,
    ));
    if mem > 0.0 {
        d.evidence.push(ev(
            "memoryBytes",
            Some(node.id),
            mem,
            EvidenceUnit::Bytes,
            true,
        ));
    }
    d.params.insert("quota".into(), fmt_num(quota as f64));
    d.params.insert("spaceBytes".into(), fmt_num(disk));
    d.actions.push(AdviceAction {
        kind: ActionKind::SessionSetting,
        label_key: "raiseMemQuota".into(),
        sql: Some(format!("SET SESSION tidb_mem_quota_query = {quota};")),
        template: false,
        scope: ActionScope::Session,
        lock: Some(LockRisk::None),
        params: BTreeMap::new(),
    });
    d.caveats.push(caveat("memoryQuotaPerQuery", false));
    d.caveats.push(caveat("spillEstimate", false));
    d.fingerprint_key = if is_sort { "sort" } else { "hash" }.into();
    out.push(d);
}
