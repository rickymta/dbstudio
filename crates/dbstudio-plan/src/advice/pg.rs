//! PostgreSQL rules (design section 4.1). YugabyteDB YSQL plans come through here as well.

use std::collections::BTreeMap;

use super::common::{
    check_action, columns_text, conjuncts, ev, ev_text, index_columns, num, row_stats, table_ref,
};
use super::index::{IndexCandidate, IndexKey, quote_ident};
use super::model::{
    ActionKind, ActionScope, AdviceAction, AdviceRule, Caveat, Confidence, EvidenceUnit, LockRisk,
    PredicateRole, TableRef,
};
use super::predicate::{CondKind, Conjunct, cast_is_harmful};
use super::{Cx, Draft, ShareSpec};
use crate::json::fmt_num;
use crate::{PlanNode, PlanNodeKind, PlanRule, Severity};

fn action(
    kind: ActionKind,
    label: &str,
    sql: Option<String>,
    template: bool,
    scope: ActionScope,
    lock: Option<LockRisk>,
) -> AdviceAction {
    AdviceAction {
        kind,
        label_key: label.into(),
        sql,
        template,
        scope,
        lock,
        params: BTreeMap::new(),
    }
}

fn caveat(key: &str, verify: bool) -> Caveat {
    Caveat {
        key: key.into(),
        verify,
        params: BTreeMap::new(),
    }
}

/// Smallest power of two that is at least `n`.
fn pow2_at_least(n: f64) -> u64 {
    let mut p = 1u64;
    while (p as f64) < n && p < (1 << 40) {
        p <<= 1;
    }
    p
}

fn table_sql(t: &TableRef) -> String {
    let q = |n: &str| quote_ident(dbstudio_core::ScriptDialect::Postgres, n);
    match &t.schema {
        Some(s) => format!("{}.{}", q(s), q(&t.name)),
        None => q(&t.name),
    }
}

fn set_work_mem(cx: &Cx<'_>, value: &str, label: &str) -> Vec<AdviceAction> {
    let mut v = vec![action(
        ActionKind::SessionSetting,
        label,
        Some(format!("SET work_mem = '{value}';")),
        false,
        ActionScope::Session,
        Some(LockRisk::None),
    )];
    if cx.ctx.server.in_transaction {
        v.push(action(
            ActionKind::SessionSetting,
            "setLocalWorkMem",
            Some(format!("SET LOCAL work_mem = '{value}';")),
            false,
            ActionScope::Session,
            Some(LockRisk::None),
        ));
    }
    v
}

pub(crate) fn node_rules(cx: &Cx<'_>, node: &PlanNode, out: &mut Vec<Draft>) {
    if node.loops == Some(0) {
        return;
    }
    function_on_column(cx, node, out);
    implicit_conversion(cx, node, out);
    bitmap_lossy(cx, node, out);
    heap_fetches(cx, node, out);
    spill(cx, node, out);
    parallel_not_launched(node, out);
    not_in_subplan(node, out);
}

pub(crate) fn tree_rules(cx: &Cx<'_>, out: &mut Vec<Draft>) {
    jit(cx, out);
    cold_cache(cx, out);
}

/// Extra statistics actions of a misestimated scan: multi-column statistics or a higher target.
pub(crate) fn extra_statistics_actions(
    cx: &Cx<'_>,
    scan: &PlanNode,
    table: &TableRef,
) -> Vec<AdviceAction> {
    let conj = conjuncts(cx, scan, &[PredicateRole::Filter, PredicateRole::IndexCond]);
    let (eq, _) = index_columns(&conj, false);
    let t = table_sql(table);
    let q = |n: &str| quote_ident(dbstudio_core::ScriptDialect::Postgres, n);
    match eq.len() {
        0 => Vec::new(),
        1 => vec![action(
            ActionKind::Statistics,
            "raiseStatisticsTarget",
            Some(format!(
                "ALTER TABLE {t} ALTER COLUMN {} SET STATISTICS 1000;\nANALYZE {t};",
                q(&eq[0].name)
            )),
            false,
            ActionScope::Object,
            Some(LockRisk::ShortLock),
        )],
        _ => {
            let cols: Vec<String> = eq.iter().take(4).map(|c| q(&c.name)).collect();
            let name = format!(
                "st_{}_{}",
                table
                    .name
                    .to_lowercase()
                    .replace(|c: char| !c.is_ascii_alphanumeric(), "_"),
                eq.iter()
                    .take(4)
                    .map(|c| c.key().replace(|c: char| !c.is_ascii_alphanumeric(), "_"))
                    .collect::<Vec<_>>()
                    .join("_")
            );
            vec![action(
                ActionKind::Statistics,
                "createExtendedStatistics",
                Some(format!(
                    "CREATE STATISTICS IF NOT EXISTS {} (dependencies, ndistinct) ON {} FROM {t};\nANALYZE {t};",
                    q(&name),
                    cols.join(", ")
                )),
                false,
                ActionScope::Object,
                Some(LockRisk::ShortLock),
            )]
        }
    }
}

// ------------------------------------------------------------------------------------------

/// Functions whose result can be indexed directly (`CREATE INDEX ... ((lower(email)))`).
const INDEXABLE_FUNCS: &[&str] = &[
    "lower",
    "upper",
    "abs",
    "trim",
    "btrim",
    "ltrim",
    "rtrim",
    "length",
    "char_length",
];

fn is_date_cast(func: &str) -> bool {
    matches!(
        func,
        "::date"
            | "::timestamp"
            | "::timestamptz"
            | "::time"
            | "::timestamp without time zone"
            | "::timestamp with time zone"
    )
}

fn node_conjuncts(cx: &Cx<'_>, node: &PlanNode) -> Vec<(PredicateRole, Conjunct)> {
    let mut out = Vec::new();
    for role in [
        PredicateRole::Filter,
        PredicateRole::IndexCond,
        PredicateRole::JoinCond,
        PredicateRole::JoinFilter,
        PredicateRole::Recheck,
    ] {
        for c in conjuncts(cx, node, &[role]) {
            out.push((role, c));
        }
    }
    out
}

fn function_on_column(cx: &Cx<'_>, node: &PlanNode, out: &mut Vec<Draft>) {
    if node.kind != PlanNodeKind::FullScan {
        return;
    }
    let Some(table) = table_ref(node) else { return };
    let rules = cx.ctx.rules;
    let stats = row_stats(cx, node, &table);
    let big = stats
        .examined
        .is_some_and(|e| e >= rules.filter_min_examined)
        || stats.kept.is_some_and(|k| k >= rules.filter_min_examined)
        || node.est_cost.is_some_and(|c| c >= 1_000.0);
    if !big {
        return;
    }
    let mut seen: Vec<String> = Vec::new();
    for c in conjuncts(cx, node, &[PredicateRole::Filter]) {
        let (CondKind::NonSargable, Some(col), Some(func)) =
            (c.kind, c.col.clone(), c.func.clone())
        else {
            continue;
        };
        let date_cast = is_date_cast(&func);
        if func.starts_with("::") && !date_cast {
            continue; // implicitConversion
        }
        if func == "like"
            || func == "+"
            || func == "-"
            || func == "*"
            || func == "/"
            || func == "||"
            || func == "%"
        {
            if func == "like" {
                continue;
            }
        }
        let display = if func.starts_with("::") {
            format!("{}{}", col.name, func)
        } else {
            format!("{func}({})", col.name)
        };
        if seen.contains(&display) {
            continue;
        }
        seen.push(display.clone());
        let mut d = Draft::new(
            AdviceRule::PgFunctionOnColumn,
            Severity::Warning,
            Confidence::High,
        )
        .node(node.id)
        .param("function", display.clone())
        .param("column", col.name.clone());
        d.needs_actual = true;
        d.fingerprint_key = display.clone();
        d.table = Some(table.clone());
        d.examined = stats.examined.or(stats.kept);
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
        }
        if let Some(t) = node.self_time_ms {
            d.evidence
                .push(ev("selfTime", Some(node.id), t, EvidenceUnit::Ms, true));
        }
        d.evidence
            .push(ev_text("expression", Some(node.id), display.clone()));
        if warning_on(cx, node.id) {
            d.from_warnings.push(PlanRule::FullScanLargeTable);
        }
        // (1) Rewrite so the column stays bare.
        let is_trunc = func == "date_trunc" || date_cast || func == "date";
        if is_trunc {
            let c = quote_ident(dbstudio_core::ScriptDialect::Postgres, &col.name);
            d.actions.push(action(
                ActionKind::Rewrite,
                "rewriteAsRange",
                Some(format!(
                    "{c} >= '<start>' AND {c} < '<start>'::timestamp + INTERVAL '1 day'"
                )),
                true,
                ActionScope::Query,
                None,
            ));
        } else {
            d.actions.push(action(
                ActionKind::Rewrite,
                "moveFunctionToConstant",
                None,
                false,
                ActionScope::Query,
                None,
            ));
        }
        // (2) An expression index, when the expression is plain enough to write down.
        let q = |n: &str| quote_ident(dbstudio_core::ScriptDialect::Postgres, n);
        let expr = if date_cast && c.simple {
            Some((
                format!("(({})::{})", q(&col.name), func.trim_start_matches("::")),
                format!(
                    "{}_{}",
                    col.name,
                    func.trim_start_matches("::").replace(' ', "_")
                ),
            ))
        } else if c.simple && INDEXABLE_FUNCS.contains(&func.as_str()) {
            Some((
                format!("({func}({}))", q(&col.name)),
                format!("{func}_{}", col.name),
            ))
        } else {
            None
        };
        if let Some((sql, name_part)) = expr {
            d.candidate = Some(IndexCandidate {
                table: table.clone(),
                keys: vec![IndexKey::expression(sql, name_part)],
                eq_count: 0,
                include: Vec::new(),
            });
            if date_cast {
                d.caveats.push(caveat("expressionMustBeImmutable", true));
            }
        }
        d.params.insert(
            "columns".into(),
            d.candidate
                .as_ref()
                .map_or_else(|| col.name.clone(), columns_text),
        );
        out.push(d);
    }
}

fn warning_on(cx: &Cx<'_>, id: u32) -> bool {
    cx.tree
        .warnings
        .iter()
        .any(|w| w.node_id == id && w.rule == PlanRule::FullScanLargeTable)
}

fn implicit_conversion(cx: &Cx<'_>, node: &PlanNode, out: &mut Vec<Draft>) {
    let table = table_ref(node);
    let meta = table
        .as_ref()
        .and_then(|t| cx.ctx.table_meta(t.schema.as_deref(), &t.name));
    let mut seen: Vec<String> = Vec::new();
    for (role, c) in node_conjuncts(cx, node) {
        let (CondKind::NonSargable, Some(col), Some(func)) =
            (c.kind, c.col.clone(), c.func.clone())
        else {
            continue;
        };
        if !func.starts_with("::") || is_date_cast(&func) {
            continue;
        }
        let cast = func.trim_start_matches("::").to_string();
        // With the catalog at hand the column type decides whether the cast hurts.
        let column_type = meta
            .and_then(|m| {
                m.columns
                    .iter()
                    .find(|x| x.name.eq_ignore_ascii_case(&col.name))
            })
            .map(|x| x.type_name.clone());
        if let Some(ty) = &column_type {
            if !cast_is_harmful(&cast, &col.name, Some(ty)) {
                continue;
            }
        }
        let key = format!("{}::{cast}", col.key());
        if seen.contains(&key) {
            continue;
        }
        seen.push(key.clone());
        let scan = node.object.is_some();
        let mut d = Draft::new(
            AdviceRule::ImplicitConversion,
            if scan && role != PredicateRole::JoinCond {
                Severity::Warning
            } else {
                Severity::Info
            },
            Confidence::Medium,
        )
        .node(node.id)
        .param("column", col.name.clone())
        .param("cast", cast.clone());
        d.needs_metadata = column_type.is_none();
        d.fingerprint_key = key;
        d.table = table.clone();
        d.evidence.push(ev_text(
            "expression",
            Some(node.id),
            format!("({})::{cast}", col.name),
        ));
        if let Some(ty) = column_type {
            d.evidence.push(ev_text("columnType", Some(node.id), ty));
        }
        d.actions.push(action(
            ActionKind::Rewrite,
            "matchLiteralType",
            None,
            false,
            ActionScope::Query,
            None,
        ));
        out.push(d);
    }
}

// ------------------------------------------------------------------------------------------

fn bitmap_lossy(cx: &Cx<'_>, node: &PlanNode, out: &mut Vec<Draft>) {
    if node.op != "Bitmap Heap Scan" {
        return;
    }
    let loops = node.loops.map_or(1.0, |l| l as f64);
    let lossy = num(node, "Lossy Heap Blocks").unwrap_or(0.0);
    let exact = num(node, "Exact Heap Blocks").unwrap_or(0.0);
    let recheck = num(node, "Rows Removed by Index Recheck").unwrap_or(0.0) * loops;
    if lossy <= 0.0 || recheck < cx.ctx.rules.filter_min_examined {
        return;
    }
    // About 320 bytes of bitmap per exact page; leave room to grow.
    let needed_mb = ((exact + lossy) * 320.0 / (1024.0 * 1024.0) * 2.0).max(8.0);
    let mb = pow2_at_least(needed_mb).min(1024);
    let mut d = Draft::new(
        AdviceRule::PgBitmapLossy,
        Severity::Warning,
        Confidence::Medium,
    )
    .node(node.id);
    d.table = table_ref(node);
    d.evidence.push(ev(
        "lossyHeapBlocks",
        Some(node.id),
        lossy,
        EvidenceUnit::Blocks,
        true,
    ));
    d.evidence.push(ev(
        "exactHeapBlocks",
        Some(node.id),
        exact,
        EvidenceUnit::Blocks,
        true,
    ));
    d.evidence.push(ev(
        "rowsRemovedByRecheck",
        Some(node.id),
        recheck,
        EvidenceUnit::Rows,
        true,
    ));
    d.params.insert("lossy".into(), fmt_num(lossy));
    d.params.insert("recheck".into(), fmt_num(recheck));
    d.params.insert("workMem".into(), format!("{mb}MB"));
    d.actions
        .extend(set_work_mem(cx, &format!("{mb}MB"), "raiseWorkMem"));
    d.caveats.push(caveat("workMemEstimate", false));
    d.caveats.push(caveat("workMemMultiplies", false));
    d.fingerprint_key = "bitmap".into();
    out.push(d);
}

fn heap_fetches(cx: &Cx<'_>, node: &PlanNode, out: &mut Vec<Draft>) {
    if node.kind != PlanNodeKind::IndexOnlyScan {
        return;
    }
    let Some(fetches) = num(node, "Heap Fetches") else {
        return;
    };
    let rows = node.actual_rows_total.unwrap_or(0.0);
    if fetches < 1_000.0 || fetches < cx.ctx.rules.heap_fetch_ratio * rows.max(1.0) {
        return;
    }
    let Some(table) = table_ref(node) else { return };
    let t = table_sql(&table);
    let mut d = Draft::new(
        AdviceRule::PgHeapFetches,
        Severity::Warning,
        Confidence::Medium,
    )
    .node(node.id);
    d.evidence.push(ev(
        "heapFetches",
        Some(node.id),
        fetches,
        EvidenceUnit::Count,
        true,
    ));
    d.evidence.push(ev(
        "rowsKept",
        Some(node.id),
        rows,
        EvidenceUnit::Rows,
        true,
    ));
    d.params.insert("fetches".into(), fmt_num(fetches));
    d.actions.push(action(
        ActionKind::Maintenance,
        "vacuumTable",
        Some(format!("VACUUM (ANALYZE) {t};")),
        false,
        ActionScope::Object,
        Some(LockRisk::None),
    ));
    d.actions.push(action(
        ActionKind::Maintenance,
        "tuneAutovacuum",
        Some(format!(
            "ALTER TABLE {t} SET (autovacuum_vacuum_scale_factor = 0.02);"
        )),
        true,
        ActionScope::Object,
        Some(LockRisk::ShortLock),
    ));
    d.caveats.push(caveat("vacuumIo", false));
    d.caveats.push(caveat("heapFetchesReplica", false));
    d.table = Some(table);
    out.push(d);
}

fn spill(cx: &Cx<'_>, node: &PlanNode, out: &mut Vec<Draft>) {
    let Some(w) = cx
        .tree
        .warnings
        .iter()
        .find(|w| w.node_id == node.id && w.rule == PlanRule::SpillToDisk)
    else {
        return;
    };
    let is_sort =
        node.extra.contains_key("Sort Method") || node.extra.contains_key("Sort Space Type");
    if is_sort {
        let kb = num(node, "Sort Space Used").unwrap_or(0.0);
        let mb = pow2_at_least((kb * 2.0 / 1024.0).max(1.0)).min(4096);
        let mut d = Draft::new(AdviceRule::SortSpill, w.severity, Confidence::Medium).node(node.id);
        d.from_warnings.push(PlanRule::SpillToDisk);
        d.evidence
            .push(ev("sortSpaceKb", Some(node.id), kb, EvidenceUnit::Kb, true));
        if let Some(m) = node.extra.get("Sort Method") {
            d.evidence
                .push(ev_text("sortMethod", Some(node.id), m.clone()));
        }
        d.params.insert("workMem".into(), format!("{mb}MB"));
        d.params.insert("spaceKb".into(), fmt_num(kb));
        d.actions
            .extend(set_work_mem(cx, &format!("{mb}MB"), "raiseWorkMem"));
        d.caveats.push(caveat("workMemMultiplies", false));
        d.caveats.push(caveat("spillEstimate", false));
        d.fingerprint_key = "sort".into();
        out.push(d);
        return;
    }
    let batches = num(node, "Hash Batches")
        .or_else(|| num(node, "HashAgg Batches"))
        .unwrap_or(1.0);
    let peak = num(node, "Peak Memory Usage").unwrap_or(0.0);
    let disk = num(node, "Disk Usage").unwrap_or(0.0);
    let need_kb = if node.extra.contains_key("Hash Batches") {
        peak * batches.max(1.0)
    } else {
        peak + disk
    }
    .max(64.0)
    .ceil();
    let mut d = Draft::new(AdviceRule::HashSpill, w.severity, Confidence::Medium).node(node.id);
    d.from_warnings.push(PlanRule::SpillToDisk);
    d.evidence.push(ev(
        "hashBatches",
        Some(node.id),
        batches,
        EvidenceUnit::Count,
        true,
    ));
    d.evidence.push(ev(
        "peakMemoryKb",
        Some(node.id),
        peak,
        EvidenceUnit::Kb,
        true,
    ));
    if disk > 0.0 {
        d.evidence.push(ev(
            "diskUsageKb",
            Some(node.id),
            disk,
            EvidenceUnit::Kb,
            true,
        ));
    }
    d.params
        .insert("workMem".into(), format!("{}kB", fmt_num(need_kb)));
    d.params.insert("batches".into(), fmt_num(batches));
    d.actions.extend(set_work_mem(
        cx,
        &format!("{}kB", fmt_num(need_kb)),
        "raiseWorkMem",
    ));
    if cx.ctx.major_version().is_some_and(|v| v >= 13) {
        d.actions.push(action(
            ActionKind::SessionSetting,
            "raiseHashMemMultiplier",
            Some("SET hash_mem_multiplier = 2;".into()),
            false,
            ActionScope::Session,
            Some(LockRisk::None),
        ));
    }
    d.caveats.push(caveat("workMemMultiplies", false));
    d.fingerprint_key = "hash".into();
    out.push(d);
}

fn parallel_not_launched(node: &PlanNode, out: &mut Vec<Draft>) {
    if node.kind != PlanNodeKind::Gather {
        return;
    }
    let (Some(planned), Some(launched)) =
        (num(node, "Workers Planned"), num(node, "Workers Launched"))
    else {
        return;
    };
    if launched >= planned {
        return;
    }
    let mut d = Draft::new(
        AdviceRule::PgParallelNotLaunched,
        Severity::Info,
        Confidence::Low,
    )
    .node(node.id);
    d.evidence.push(ev(
        "workersPlanned",
        Some(node.id),
        planned,
        EvidenceUnit::Count,
        false,
    ));
    d.evidence.push(ev(
        "workersLaunched",
        Some(node.id),
        launched,
        EvidenceUnit::Count,
        true,
    ));
    d.params.insert("planned".into(), fmt_num(planned));
    d.params.insert("launched".into(), fmt_num(launched));
    d.actions.push(check_action(
        "checkParallelLimits",
        Some("SHOW max_parallel_workers;\nSHOW max_worker_processes;"),
        ActionScope::Server,
    ));
    d.caveats.push(caveat("transientLoad", false));
    d.share = ShareSpec::Unknown;
    d.fingerprint_key = "gather".into();
    out.push(d);
}

fn not_in_subplan(node: &PlanNode, out: &mut Vec<Draft>) {
    let hit = node
        .predicates
        .iter()
        .filter(|p| matches!(p.role, PredicateRole::Filter | PredicateRole::JoinFilter))
        .any(|p| p.text.contains("NOT (SubPlan") || p.text.contains("NOT (alternatives"));
    if !hit {
        return;
    }
    let mut d = Draft::new(
        AdviceRule::PgNotInSubplan,
        Severity::Warning,
        Confidence::High,
    )
    .node(node.id);
    d.table = table_ref(node);
    d.actions.push(action(
        ActionKind::Rewrite,
        "rewriteNotExists",
        Some("NOT EXISTS (SELECT 1 FROM /* subquery table */ s WHERE s./* column */ = /* outer column */)".into()),
        true,
        ActionScope::Query,
        None,
    ));
    d.caveats.push(caveat("semanticsDiffer", true));
    d.fingerprint_key = "notin".into();
    out.push(d);
}

// ------------------------------------------------------------------------------------------

fn jit(cx: &Cx<'_>, out: &mut Vec<Draft>) {
    let root = &cx.tree.root;
    let (Some(jit), Some(exec)) = (num(root, "JIT Total"), cx.tree.totals.execution_time_ms) else {
        return;
    };
    if jit < 50.0 || jit < 0.2 * exec {
        return;
    }
    let mut d = Draft::new(
        AdviceRule::PgJitOverhead,
        Severity::Warning,
        Confidence::Medium,
    )
    .node(root.id);
    d.evidence
        .push(ev("jitTime", Some(root.id), jit, EvidenceUnit::Ms, true));
    d.evidence.push(ev(
        "executionTime",
        Some(root.id),
        exec,
        EvidenceUnit::Ms,
        true,
    ));
    d.params.insert("jit".into(), fmt_num(jit));
    d.actions.push(action(
        ActionKind::SessionSetting,
        "disableJit",
        Some("SET jit = off;".into()),
        false,
        ActionScope::Session,
        Some(LockRisk::None),
    ));
    d.actions.push(action(
        ActionKind::ServerSetting,
        "raiseJitAboveCost",
        None,
        false,
        ActionScope::Server,
        None,
    ));
    d.caveats.push(caveat("jitHelpsOlap", false));
    d.share = ShareSpec::Unknown;
    d.fingerprint_key = "jit".into();
    out.push(d);
}

fn cold_cache(cx: &Cx<'_>, out: &mut Vec<Draft>) {
    let root = &cx.tree.root;
    let (Some(read), Some(hit)) = (
        num(root, "Shared Read Blocks"),
        num(root, "Shared Hit Blocks"),
    ) else {
        return;
    };
    if read < 10_000.0 || read < 10.0 * hit {
        return;
    }
    let mut d = Draft::new(AdviceRule::PgColdCache, Severity::Info, Confidence::Low).node(root.id);
    d.evidence.push(ev(
        "sharedReadBlocks",
        Some(root.id),
        read,
        EvidenceUnit::Blocks,
        true,
    ));
    d.evidence.push(ev(
        "sharedHitBlocks",
        Some(root.id),
        hit,
        EvidenceUnit::Blocks,
        true,
    ));
    d.actions
        .push(check_action("rerunWarm", None, ActionScope::Query));
    d.share = ShareSpec::Unknown;
    d.fingerprint_key = "cache".into();
    out.push(d);
}
