//! MySQL / MariaDB rules (design section 4.2). TiDB goes through the same generic rules (full
//! scan under a filter, residual filter, filesort) and adds its own in `tidb.rs`.
//!
//! Plans of this family differ a lot: JSON v1 puts the condition on the table node and gives no
//! sort columns, JSON v2 and the tree text wrap a scan in a `Filter` node, MariaDB reports
//! `r_rows` / `r_filtered`, TiDB wraps the scan in a coprocessor `Selection`. The helpers below
//! hide those differences ([`scan_stats`], [`filter_conjuncts`]) and read the statement text for
//! what the plan does not say (aliases, `ORDER BY` and `GROUP BY` columns).

use std::collections::BTreeMap;

use dbstudio_core::{PlanFormat, ScriptDialect};
use dbstudio_sql::lexer::{TokenKind, tokenize};

use super::common::{
    candidate, check_action, columns_text, ev, ev_text, num, own_names, statistics_action,
    table_ref,
};
use super::index::quote_ident;
use super::model::{
    ActionKind, ActionScope, AdviceAction, AdviceRule, Caveat, Confidence, EvidenceUnit, LockRisk,
    PredicateRole, TableRef,
};
use super::predicate::{ColRef, CondKind, Conjunct, parse_conjuncts, parse_sort_key};
use super::{Cx, Draft};
use crate::json::fmt_num;
use crate::{PlanNode, PlanNodeKind, PlanRule, Severity};

// ------------------------------------------------------------------------------------------
// Statement text
// ------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Sig {
    kind: TokenKind,
    text: String,
    lower: String,
    depth: u32,
}

impl Sig {
    fn is_punct(&self, c: char) -> bool {
        self.kind == TokenKind::Punct && self.text.len() == 1 && self.text.starts_with(c)
    }

    fn is_word(&self, w: &str) -> bool {
        self.kind == TokenKind::Word && self.lower == w
    }

    fn is_ident(&self) -> bool {
        self.kind == TokenKind::QuotedIdent || self.kind == TokenKind::Word
    }
}

fn sigs_of(cx: &Cx<'_>) -> Vec<Sig> {
    let chars: Vec<char> = cx.statement.chars().collect();
    let mut out = Vec::new();
    let mut depth = 0u32;
    for t in tokenize(cx.ctx.dialect, cx.statement) {
        if t.kind.is_trivia() {
            continue;
        }
        let raw: String = chars
            .get(t.range.start as usize..t.range.end as usize)
            .map(|c| c.iter().collect())
            .unwrap_or_default();
        let text = if t.kind == TokenKind::QuotedIdent && raw.chars().count() >= 2 {
            let inner: String = raw.chars().skip(1).take(raw.chars().count() - 2).collect();
            inner.replace("``", "`")
        } else {
            raw
        };
        let mut sig = Sig {
            kind: t.kind,
            lower: text.to_lowercase(),
            text,
            depth,
        };
        if sig.is_punct('(') {
            depth += 1;
        } else if sig.is_punct(')') {
            depth = depth.saturating_sub(1);
            sig.depth = depth;
        }
        out.push(sig);
    }
    out
}

/// Words that end a table reference (never an alias).
const NOT_ALIAS: &[&str] = &[
    "where",
    "on",
    "using",
    "join",
    "inner",
    "left",
    "right",
    "cross",
    "natural",
    "straight_join",
    "group",
    "order",
    "limit",
    "having",
    "union",
    "partition",
    "force",
    "use",
    "ignore",
    "set",
    "window",
    "for",
    "lock",
    "into",
    "as",
    "from",
    "select",
    "outer",
    "full",
    "values",
];

/// A table reference of the statement: `[db.]table [AS] alias`.
#[derive(Debug, Clone)]
pub(super) struct Alias {
    /// Name the statement uses for the table (alias when given, else the table name).
    pub alias: String,
    pub db: Option<String>,
    pub table: String,
}

/// One `ORDER BY` item that is a plain column.
#[derive(Debug, Clone)]
struct OrderItem {
    qualifier: Option<String>,
    column: String,
    desc: bool,
}

/// What the statement text says that the plan does not.
#[derive(Debug, Clone, Default)]
pub(super) struct StmtInfo {
    pub aliases: Vec<Alias>,
    /// Top level `ORDER BY` when every item is a plain column.
    order_by: Option<Vec<OrderItem>>,
    /// Top level `GROUP BY` when every item is a plain column.
    group_by: Option<Vec<OrderItem>>,
}

fn table_ref_at(sigs: &[Sig], j: usize) -> Option<(Option<String>, String, Option<String>, usize)> {
    let a = sigs.get(j).filter(|s| s.is_ident())?;
    if a.kind == TokenKind::Word && NOT_ALIAS.contains(&a.lower.as_str()) {
        return None;
    }
    let mut next = j + 1;
    let (db, table) = if sigs.get(next).is_some_and(|s| s.is_punct('.')) {
        let b = sigs.get(next + 1).filter(|s| s.is_ident())?;
        next += 2;
        (Some(a.text.clone()), b.text.clone())
    } else {
        (None, a.text.clone())
    };
    if sigs.get(next).is_some_and(|s| s.is_word("as")) {
        next += 1;
    }
    let alias = sigs.get(next).filter(|s| {
        s.is_ident() && !(s.kind == TokenKind::Word && NOT_ALIAS.contains(&s.lower.as_str()))
    });
    match alias {
        Some(al) => Some((db, table, Some(al.text.clone()), next + 1)),
        None => Some((db, table, None, next)),
    }
}

fn aliases_of(sigs: &[Sig]) -> Vec<Alias> {
    let mut out: Vec<Alias> = Vec::new();
    let mut in_from: Vec<u32> = Vec::new();
    let mut i = 0;
    let take = |at: usize, out: &mut Vec<Alias>| -> usize {
        match table_ref_at(sigs, at) {
            Some((db, table, alias, next)) => {
                out.push(Alias {
                    alias: alias.unwrap_or_else(|| table.clone()),
                    db,
                    table,
                });
                next
            }
            None => at,
        }
    };
    while i < sigs.len() {
        let s = &sigs[i];
        if s.kind == TokenKind::Word {
            match s.lower.as_str() {
                "from" | "join" | "update" => {
                    if !in_from.contains(&s.depth) {
                        in_from.push(s.depth);
                    }
                    let next = take(i + 1, &mut out);
                    i = next.max(i + 1);
                    continue;
                }
                "where" | "group" | "order" | "having" | "limit" | "union" | "set" | "window"
                | "select" | "for" | "lock" | "into" => {
                    in_from.retain(|d| *d != s.depth);
                }
                _ => {}
            }
        } else if s.is_punct(',') && in_from.contains(&s.depth) {
            let next = take(i + 1, &mut out);
            i = next.max(i + 1);
            continue;
        } else if s.is_punct(')') {
            in_from.retain(|d| *d <= s.depth);
        }
        i += 1;
    }
    out
}

/// Plain column items of a clause that starts after the words `a b` at depth 0 and stops at a
/// word of `stop`; `None` when the clause is absent or an item is anything but `[q.]col [asc|desc]`.
fn clause_items(sigs: &[Sig], a: &str, b: &str, stop: &[&str]) -> Option<Vec<OrderItem>> {
    let start = (0..sigs.len().saturating_sub(1)).find(|&i| {
        sigs[i].depth == 0 && sigs[i].is_word(a) && sigs[i + 1].depth == 0 && sigs[i + 1].is_word(b)
    })? + 2;
    let mut items: Vec<Vec<&Sig>> = vec![Vec::new()];
    for s in &sigs[start..] {
        if s.depth == 0 && s.kind == TokenKind::Word && stop.contains(&s.lower.as_str()) {
            break;
        }
        if s.depth == 0 && s.is_punct(';') {
            break;
        }
        if s.depth == 0 && s.is_punct(',') {
            items.push(Vec::new());
        } else if let Some(last) = items.last_mut() {
            last.push(s);
        }
    }
    let mut out = Vec::new();
    for it in items {
        let mut desc = false;
        let mut toks = it.as_slice();
        if let Some((last, rest)) = toks.split_last() {
            if last.is_word("desc") {
                desc = true;
                toks = rest;
            } else if last.is_word("asc") {
                toks = rest;
            }
        }
        match toks {
            [c] if c.is_ident() => out.push(OrderItem {
                qualifier: None,
                column: c.text.clone(),
                desc,
            }),
            [q, d, c] if q.is_ident() && d.is_punct('.') && c.is_ident() => out.push(OrderItem {
                qualifier: Some(q.text.clone()),
                column: c.text.clone(),
                desc,
            }),
            [_, d1, q, d2, c]
                if d1.is_punct('.') && d2.is_punct('.') && q.is_ident() && c.is_ident() =>
            {
                out.push(OrderItem {
                    qualifier: Some(q.text.clone()),
                    column: c.text.clone(),
                    desc,
                })
            }
            _ => return None,
        }
    }
    (!out.is_empty()).then_some(out)
}

pub(super) fn stmt_info(cx: &Cx<'_>) -> StmtInfo {
    if cx.statement.trim().is_empty() {
        return StmtInfo::default();
    }
    let sigs = sigs_of(cx);
    let has_union = sigs
        .iter()
        .any(|s| s.depth == 0 && (s.is_word("union") || s.is_word("intersect")));
    StmtInfo {
        aliases: aliases_of(&sigs),
        order_by: if has_union {
            None
        } else {
            clause_items(
                &sigs,
                "order",
                "by",
                &["limit", "for", "lock", "into", "procedure"],
            )
        },
        group_by: if has_union {
            None
        } else {
            clause_items(
                &sigs,
                "group",
                "by",
                &[
                    "having", "order", "limit", "window", "with", "for", "lock", "into",
                ],
            )
        },
    }
}

/// The table behind a name the plan printed (JSON v1 and the tree text print the alias).
pub(super) fn resolve_table(info: &StmtInfo, t: &TableRef) -> TableRef {
    let Some(a) = info
        .aliases
        .iter()
        .find(|a| a.alias.eq_ignore_ascii_case(&t.name))
    else {
        return t.clone();
    };
    let mut r = t.clone();
    if r.schema.is_none() {
        r.schema = a.db.clone();
    }
    if !a.table.eq_ignore_ascii_case(&t.name) {
        r.alias = Some(t.name.clone());
        r.name = a.table.clone();
    }
    r
}

// ------------------------------------------------------------------------------------------
// Plan helpers
// ------------------------------------------------------------------------------------------

fn is_scan_kind(k: PlanNodeKind) -> bool {
    matches!(
        k,
        PlanNodeKind::FullScan
            | PlanNodeKind::IndexScan
            | PlanNodeKind::IndexOnlyScan
            | PlanNodeKind::IndexSeek
            | PlanNodeKind::KeyLookup
    )
}

/// A scan of a materialised derived table / CTE: its name is an alias, not a table.
fn is_derived(node: &PlanNode) -> bool {
    node.object
        .as_ref()
        .is_some_and(|o| o.name.starts_with('<'))
        || (node.object.is_some()
            && node
                .children
                .iter()
                .any(|c| c.kind == PlanNodeKind::Materialize))
}

fn warning<'a>(cx: &'a Cx<'_>, node: u32, rule: PlanRule) -> Option<&'a crate::PlanWarning> {
    cx.tree
        .warnings
        .iter()
        .find(|w| w.node_id == node && w.rule == rule)
}

fn plain(name: &str) -> ColRef {
    ColRef {
        qualifier: None,
        name: name.to_string(),
        cast: None,
    }
}

fn split_list(text: &str) -> Vec<String> {
    text.split(',')
        .map(|p| p.trim().trim_matches('`').to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

/// Filter conjuncts that apply to a scan: its own `attached_condition` and the `Filter` /
/// `Selection` node right above it.
pub(super) fn filter_conjuncts<'a>(
    cx: &Cx<'a>,
    scan: &PlanNode,
) -> (Vec<Conjunct>, Option<&'a PlanNode>) {
    let own = own_names(scan);
    let parent = cx
        .parent_of(scan.id)
        .filter(|p| p.kind == PlanNodeKind::Filter && p.object.is_none());
    let mut out = Vec::new();
    for n in std::iter::once(scan).chain(parent) {
        for p in n
            .predicates
            .iter()
            .filter(|p| p.role == PredicateRole::Filter)
        {
            out.extend(parse_conjuncts(cx.ctx.dialect, &p.text, &own));
        }
    }
    (out, parent)
}

/// Index conditions of a scan (`ref` lookups, ranges) classified against its own table.
fn index_cond_conjuncts(cx: &Cx<'_>, scan: &PlanNode) -> Vec<Conjunct> {
    let own = own_names(scan);
    scan.predicates
        .iter()
        .filter(|p| p.role == PredicateRole::IndexCond)
        .flat_map(|p| parse_conjuncts(cx.ctx.dialect, &p.text, &own))
        .collect()
}

/// Rows a scan reads and rows that survive its filter.
pub(super) struct Stats {
    pub examined: Option<f64>,
    pub kept: Option<f64>,
    /// The figures are measured (ANALYZE), not estimated.
    pub actual: bool,
}

impl Stats {
    pub fn selectivity(&self) -> Option<f64> {
        match (self.kept, self.examined) {
            (Some(k), Some(e)) if e > 0.0 => Some((k / e).min(1.0)),
            _ => None,
        }
    }
}

pub(super) fn scan_stats(cx: &Cx<'_>, scan: &PlanNode, filter: Option<&PlanNode>) -> Stats {
    match cx.tree.format {
        PlanFormat::MySqlJson => Stats {
            examined: num(scan, "rows_examined_per_scan"),
            kept: scan.est_rows,
            actual: false,
        },
        PlanFormat::MariaDbJson => {
            if let Some(total) = scan.actual_rows_total {
                Stats {
                    examined: Some(total),
                    kept: num(scan, "r_filtered").map(|p| total * p / 100.0),
                    actual: true,
                }
            } else {
                // `filtered` is 100 unless the optimizer has selectivity statistics.
                let examined = scan.est_rows;
                Stats {
                    examined,
                    kept: examined
                        .zip(num(scan, "filtered").filter(|f| *f < 100.0))
                        .map(|(e, f)| e * f / 100.0),
                    actual: false,
                }
            }
        }
        _ => {
            let actual = scan.actual_rows_total.is_some();
            let examined = scan.actual_rows_total.or(scan.est_rows);
            let kept = match filter {
                Some(f) if actual => f.actual_rows_total,
                Some(f) => f.est_rows,
                None => examined,
            };
            Stats {
                examined,
                kept,
                actual,
            }
        }
    }
}

fn pct(p: f64) -> String {
    format!("{}%", fmt_num((p * 1000.0).round() / 10.0))
}

fn caveat(key: &str, verify: bool) -> Caveat {
    Caveat {
        key: key.into(),
        verify,
        params: BTreeMap::new(),
    }
}

fn names_of(c: &super::index::IndexCandidate) -> String {
    c.keys
        .iter()
        .filter_map(|k| k.column.clone())
        .collect::<Vec<_>>()
        .join(",")
}

fn table_sql(ctx: &super::AdvisorContext<'_>, t: &TableRef) -> String {
    let q = |n: &str| quote_ident(ctx.dialect, n);
    match &t.schema {
        Some(s) => format!("{}.{}", q(s), q(&t.name)),
        None => q(&t.name),
    }
}

/// `(major, minor)` of the server version (`8.0.36`, `10.11.6-MariaDB`, `5.5.5-10.4.0-MariaDB`).
fn version_parts(version: &str) -> Option<(u32, u32)> {
    let v = version.strip_prefix("5.5.5-").unwrap_or(version);
    let mut it = v
        .split(|c: char| !c.is_ascii_digit())
        .filter(|p| !p.is_empty())
        .map(|p| p.parse::<u32>().ok());
    Some((it.next()??, it.next().flatten().unwrap_or(0)))
}

fn is_mariadb(cx: &Cx<'_>) -> bool {
    matches!(cx.ctx.dialect, ScriptDialect::MySql { mariadb: true, .. })
}

/// Descending index keys are stored descending from MySQL 8.0 and MariaDB 10.8; before that
/// (and in TiDB) the `DESC` is parsed and ignored.
fn desc_index_ignored(cx: &Cx<'_>) -> bool {
    if cx.tree.format == PlanFormat::TiDbRows {
        return true;
    }
    match version_parts(cx.ctx.server_version) {
        Some((maj, min)) if is_mariadb(cx) => maj < 10 || (maj == 10 && min < 8),
        Some((maj, _)) => maj < 8,
        None => false,
    }
}

// ------------------------------------------------------------------------------------------
// Entry points
// ------------------------------------------------------------------------------------------

/// Rules evaluated for every node of a plan of this family.
pub(crate) fn node_rules(cx: &Cx<'_>, node: &PlanNode, out: &mut Vec<Draft>) {
    if node.loops == Some(0) {
        return;
    }
    let info = stmt_info(cx);
    fix_drafts(cx, &info, out);
    index_for_filter(cx, &info, node, out);
    index_residual_filter(cx, &info, node, out);
    index_for_sort(cx, &info, node, out);
    temp_table_group(cx, &info, node, out);
    join_buffer(cx, &info, node, out);
    index_merge(cx, &info, node, out);
    possible_keys_unused(cx, &info, node, out);
    derived_table(cx, node, out);
    implicit_conversion(cx, &info, node, out);
    mariadb_sort_spill(cx, node, out);
    full_scan_answer(cx, &info, node, out);
    histograms(cx, &info, node, out);
}

/// Rules evaluated once per plan.
pub(crate) fn tree_rules(_cx: &Cx<'_>, _out: &mut Vec<Draft>) {}

/// Correct what the generic rules produced for this family: aliases printed by the plan become
/// real table names, and suggestions on a derived table (no such table) are dropped.
fn fix_drafts(cx: &Cx<'_>, info: &StmtInfo, out: &mut Vec<Draft>) {
    let derived: Vec<String> = cx
        .nodes
        .iter()
        .filter(|n| is_derived(n))
        .filter_map(|n| n.object.as_ref().map(|o| o.name.to_lowercase()))
        .collect();
    out.retain(|d| {
        let on_derived = d.candidate.as_ref().is_some_and(|c| {
            derived.contains(&c.table.name.to_lowercase())
                && !info
                    .aliases
                    .iter()
                    .any(|a| a.alias.eq_ignore_ascii_case(&c.table.name))
        });
        !on_derived
    });
    for d in out.iter_mut() {
        if let Some(t) = &d.table {
            d.table = Some(resolve_table(info, t));
        }
        if let Some(c) = &mut d.candidate {
            c.table = resolve_table(info, &c.table);
        }
    }
}

// ------------------------------------------------------------------------------------------
// indexForFilter
// ------------------------------------------------------------------------------------------

/// A full scan whose filter keeps few rows. Replaces the generic rule for this family: the
/// numbers come from `rows_examined_per_scan` / `filtered`, `r_rows` / `r_filtered` or the
/// `Filter` node above the scan.
fn index_for_filter(cx: &Cx<'_>, info: &StmtInfo, node: &PlanNode, out: &mut Vec<Draft>) {
    if node.kind != PlanNodeKind::FullScan || is_derived(node) {
        return;
    }
    out.retain(|d| !(d.rule == AdviceRule::IndexForFilter && d.node_ids.contains(&node.id)));
    let Some(t) = table_ref(node) else { return };
    let table = resolve_table(info, &t);
    let (conj, filter) = filter_conjuncts(cx, node);
    let (eq, range) = super::common::index_columns(&conj, false);
    if eq.is_empty() && range.is_none() {
        return;
    }
    let st = scan_stats(cx, node, filter);
    let rules = cx.ctx.rules;
    let sel = st.selectivity();
    if st.examined.or(st.kept).unwrap_or(0.0) < rules.filter_min_examined
        || sel.is_some_and(|s| s > rules.filter_max_selectivity)
    {
        return;
    }
    let Some(cand) = candidate(cx, &table, &eq, &[], range.as_ref()) else {
        return;
    };
    let warn = warning(cx, node.id, PlanRule::FullScanLargeTable);
    let mut d = Draft::new(
        AdviceRule::IndexForFilter,
        warn.map_or(Severity::Warning, |w| w.severity),
        Confidence::High,
    )
    .node(node.id)
    .param("columns", columns_text(&cand));
    d.needs_actual = true;
    d.needs_metadata = sel.is_none();
    d.examined = st.examined.or(st.kept);
    d.fingerprint_key = names_of(&cand);
    if let Some(e) = st.examined {
        d.evidence.push(ev(
            "rowsRead",
            Some(node.id),
            e,
            EvidenceUnit::Rows,
            st.actual,
        ));
        d.params.insert("examined".into(), fmt_num(e));
    }
    if let Some(k) = st.kept {
        d.evidence.push(ev(
            "rowsKept",
            Some(node.id),
            k,
            EvidenceUnit::Rows,
            st.actual,
        ));
        d.params.insert("kept".into(), fmt_num(k));
    }
    if let Some(s) = sel {
        d.evidence.push(ev(
            "selectivity",
            Some(node.id),
            s * 100.0,
            EvidenceUnit::Pct,
            st.actual,
        ));
        d.params.insert("selectivity".into(), pct(s));
    }
    if let Some(t) = node.self_time_ms {
        d.evidence
            .push(ev("selfTime", Some(node.id), t, EvidenceUnit::Ms, true));
    }
    if warn.is_some() {
        d.from_warnings.push(PlanRule::FullScanLargeTable);
    }
    d.table = Some(table);
    d.candidate = Some(cand);
    out.push(d);
}

// ------------------------------------------------------------------------------------------
// indexResidualFilter
// ------------------------------------------------------------------------------------------

/// Everything `residual_draft` needs to know about an index access whose filter drops most rows.
pub(super) struct Residual<'a> {
    /// Node that carries the advice.
    pub node: &'a PlanNode,
    /// Node whose object is the table.
    pub scan: &'a PlanNode,
    pub filter: Vec<Conjunct>,
    /// Columns of the index in use, when the plan says.
    pub index_columns: Vec<String>,
    pub examined: f64,
    pub kept: f64,
    pub actual: bool,
}

/// Index with the filter columns added to the one in use.
pub(super) fn residual_draft(cx: &Cx<'_>, info: &StmtInfo, r: &Residual<'_>) -> Option<Draft> {
    let rules = cx.ctx.rules;
    if r.examined < rules.filter_min_examined || r.examined < rules.residual_ratio * r.kept.max(1.0)
    {
        return None;
    }
    let (feq, frange) = super::common::index_columns(&r.filter, false);
    if feq.is_empty() && frange.is_none() {
        return None;
    }
    let table = resolve_table(info, &table_ref(r.scan)?);
    // Columns of the index in use: the plan, else the catalog, else the lookup condition.
    let mut existing: Vec<String> = r.index_columns.clone();
    if existing.is_empty() {
        if let Some(ix) = r.scan.object.as_ref().and_then(|o| o.index.as_deref()) {
            existing = cx
                .ctx
                .table_meta(table.schema.as_deref(), &table.name)
                .and_then(|m| m.indexes.iter().find(|i| i.name.eq_ignore_ascii_case(ix)))
                .map(|i| i.columns.clone())
                .unwrap_or_default();
        }
    }
    if existing.is_empty() {
        let (ieq, _) = super::common::index_columns(&index_cond_conjuncts(cx, r.scan), true);
        existing = ieq.into_iter().map(|c| c.name).collect();
    }
    let known = !existing.is_empty();
    let has = |c: &ColRef| existing.iter().any(|e| e.eq_ignore_ascii_case(&c.name));
    let new_eq: Vec<&ColRef> = feq.iter().filter(|c| !has(c)).collect();
    let new_range = frange.as_ref().filter(|c| !has(c));
    if new_eq.is_empty() && new_range.is_none() {
        return None;
    }
    let mut eq: Vec<ColRef> = existing.iter().map(|c| plain(c)).collect();
    eq.extend(new_eq.into_iter().cloned());
    let cand = candidate(cx, &table, &eq, &[], frange.as_ref())?;
    let mut d = Draft::new(
        AdviceRule::IndexResidualFilter,
        Severity::Warning,
        Confidence::Medium,
    )
    .node(r.node.id)
    .param("columns", columns_text(&cand));
    d.needs_metadata = !known;
    d.needs_actual = !r.actual;
    d.examined = Some(r.examined);
    d.fingerprint_key = names_of(&cand);
    d.evidence.push(ev(
        "rowsRead",
        Some(r.node.id),
        r.examined,
        EvidenceUnit::Rows,
        r.actual,
    ));
    d.evidence.push(ev(
        "rowsKept",
        Some(r.node.id),
        r.kept,
        EvidenceUnit::Rows,
        r.actual,
    ));
    d.params.insert("examined".into(), fmt_num(r.examined));
    d.params.insert("kept".into(), fmt_num(r.kept));
    if let Some(ix) = r.scan.object.as_ref().and_then(|o| o.index.clone()) {
        d.evidence
            .push(ev_text("usedIndex", Some(r.node.id), ix.clone()));
        d.params.insert("usedIndex".into(), ix);
    }
    d.table = Some(table);
    d.candidate = Some(cand);
    Some(d)
}

fn index_residual_filter(cx: &Cx<'_>, info: &StmtInfo, node: &PlanNode, out: &mut Vec<Draft>) {
    if !matches!(node.kind, PlanNodeKind::IndexScan | PlanNodeKind::IndexSeek)
        || node.object.is_none()
        || is_derived(node)
        || node.op == "Index merge"
        || node.op.starts_with("Intersect")
    {
        return;
    }
    let (filter, parent) = filter_conjuncts(cx, node);
    if filter.is_empty() {
        return;
    }
    let st = scan_stats(cx, node, parent);
    let (Some(examined), Some(kept)) = (st.examined, st.kept) else {
        return;
    };
    let index_columns = node
        .extra
        .get("used_key_parts")
        .or_else(|| node.extra.get("index_columns"))
        .map(|v| split_list(v))
        .unwrap_or_default();
    let r = Residual {
        node,
        scan: node,
        filter,
        index_columns,
        examined,
        kept,
        actual: st.actual,
    };
    out.extend(residual_draft(cx, info, &r));
}

// ------------------------------------------------------------------------------------------
// indexForSort
// ------------------------------------------------------------------------------------------

fn chain_ok(n: &PlanNode) -> bool {
    match n.kind {
        PlanNodeKind::Filter | PlanNodeKind::Gather | PlanNodeKind::Limit => true,
        PlanNodeKind::Other => n.object.is_none(),
        k => is_scan_kind(k),
    }
}

/// Scans below `node` when everything in between is a plain pipeline (no join, aggregate...).
fn single_table_scans<'a>(node: &'a PlanNode, out: &mut Vec<&'a PlanNode>) -> bool {
    for c in &node.children {
        if !chain_ok(c) {
            return false;
        }
        if c.object.is_some() && is_scan_kind(c.kind) {
            out.push(c);
        }
        if !single_table_scans(c, out) {
            return false;
        }
    }
    true
}

/// Rows entering a node: the output of its first child (a scan reports what its own condition
/// keeps).
fn input_rows(cx: &Cx<'_>, node: &PlanNode) -> Option<(f64, bool)> {
    let mut cur = node.children.first()?;
    loop {
        if cur.object.is_some() && is_scan_kind(cur.kind) {
            let parent = cx
                .parent_of(cur.id)
                .filter(|p| p.kind == PlanNodeKind::Filter);
            let st = scan_stats(cx, cur, parent);
            let rows = if parent.is_some() && st.kept.is_none() {
                st.examined
            } else {
                st.kept.or(st.examined)
            };
            return rows.map(|r| (r, st.actual));
        }
        if cur.kind == PlanNodeKind::Filter {
            if let Some(r) = cur.actual_rows_total {
                return Some((r, true));
            }
            if let Some(r) = cur.est_rows {
                return Some((r, false));
            }
        }
        cur = cur.children.first()?;
    }
}

fn limit_of(cx: &Cx<'_>, node: &PlanNode) -> Option<f64> {
    if let Some(p) = cx
        .parent_of(node.id)
        .filter(|p| p.kind == PlanNodeKind::Limit)
    {
        if let Some(r) = p.actual_rows_total.or(p.est_rows) {
            return Some(r);
        }
    }
    num(node, "r_limit")
        .or_else(|| num(node, "per_chunk_limit"))
        .or_else(|| {
            let key = node.extra.get("Sort Key")?;
            let rest = key.split("limit input to ").nth(1)?;
            rest.split(|c: char| !c.is_ascii_digit())
                .next()?
                .parse()
                .ok()
        })
}

fn index_for_sort(cx: &Cx<'_>, info: &StmtInfo, node: &PlanNode, out: &mut Vec<Draft>) {
    if node.kind != PlanNodeKind::Sort || node.op == "Ordering (index)" {
        return;
    }
    // MySQL v1 marks a filesort explicitly; an `Ordering` without it reads an index in order.
    if node.op == "Ordering" && node.extra.get("using_filesort").map(String::as_str) != Some("true")
    {
        return;
    }
    let mut scans = Vec::new();
    if !single_table_scans(node, &mut scans) {
        return;
    }
    let mut tables: Vec<TableRef> = Vec::new();
    for s in &scans {
        if let Some(t) = table_ref(s) {
            let t = resolve_table(info, &t);
            if is_derived(s) {
                return;
            }
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
    let own_col = |q: &Option<String>| {
        q.as_ref()
            .is_none_or(|q| own.iter().any(|o| o.eq_ignore_ascii_case(q)))
    };
    let mut sort_cols: Vec<(String, bool)> = Vec::new();
    let keys: Vec<&crate::NodePredicate> = node
        .predicates
        .iter()
        .filter(|p| p.role == PredicateRole::SortKey)
        .collect();
    if keys.is_empty() {
        // JSON v1 does not list the sort columns: take them from the statement.
        let Some(items) = &info.order_by else { return };
        for it in items {
            let qualified = it.qualifier.as_ref().map(|q| {
                info.aliases
                    .iter()
                    .find(|a| a.alias.eq_ignore_ascii_case(q))
            });
            let ok = match qualified {
                None => true,
                Some(Some(a)) => a.table.eq_ignore_ascii_case(&table.name),
                Some(None) => own_col(&it.qualifier),
            };
            if !ok {
                return;
            }
            sort_cols.push((it.column.clone(), it.desc));
        }
    } else {
        for p in keys {
            let Some(k) = parse_sort_key(cx.ctx.dialect, &p.text) else {
                return;
            };
            if k.computed || !own_col(&k.col.qualifier) {
                return;
            }
            sort_cols.push((k.col.name.clone(), k.desc));
        }
    }
    if sort_cols.is_empty() {
        return;
    }
    let Some((rows, rows_actual)) = input_rows(cx, node) else {
        return;
    };
    let spilled = warning(cx, node.id, PlanRule::SpillToDisk).is_some();
    if rows < cx.ctx.rules.filter_min_examined && !spilled {
        return;
    }
    // The generic rule saw the same sort with less information.
    out.retain(|d| !(d.rule == AdviceRule::IndexForSort && d.node_ids.contains(&node.id)));
    // Equality columns: filters and index conditions of the scans.
    let mut eq: Vec<ColRef> = Vec::new();
    for s in &scans {
        let (mut conj, _) = filter_conjuncts(cx, s);
        conj.extend(index_cond_conjuncts(cx, s));
        let (mut e, _) = super::common::index_columns(&conj, false);
        // A `ref` lookup is an equality on the leading columns of the index it uses.
        if s.kind == PlanNodeKind::IndexSeek {
            if let Some(parts) = s.extra.get("used_key_parts") {
                e.splice(0..0, split_list(parts).iter().map(|c| plain(c)));
            }
        }
        for c in e {
            let in_sort = sort_cols
                .iter()
                .any(|(n, _)| n.eq_ignore_ascii_case(&c.name));
            if !in_sort && !eq.iter().any(|x| x.key() == c.key()) {
                eq.push(c);
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
    d.fingerprint_key = names_of(&cand);
    d.examined = Some(rows);
    d.needs_actual = true;
    d.evidence.push(ev(
        "sortRows",
        Some(node.id),
        rows,
        EvidenceUnit::Rows,
        rows_actual,
    ));
    d.params.insert("rows".into(), fmt_num(rows));
    if let Some(l) = limit_of(cx, node) {
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
    if sort_cols.iter().any(|(_, desc)| *desc) && desc_index_ignored(cx) {
        d.caveats.push(caveat("descendingIndexIgnored", false));
    }
    if spilled {
        d.from_warnings.push(PlanRule::SpillToDisk);
        d.caveats.push(caveat("indexForSortSpill", false));
    }
    d.table = Some(table.clone());
    d.candidate = Some(cand);
    out.push(d);
}

// ------------------------------------------------------------------------------------------
// mysqlTempTableGroup
// ------------------------------------------------------------------------------------------

/// The scan that drives a join tree: leftmost leaf.
fn driving_scan(node: &PlanNode) -> Option<&PlanNode> {
    if node.object.is_some() && is_scan_kind(node.kind) {
        return Some(node);
    }
    node.children.first().and_then(driving_scan)
}

fn temp_table_group(cx: &Cx<'_>, info: &StmtInfo, node: &PlanNode, out: &mut Vec<Draft>) {
    let uses_tmp = (node.kind == PlanNodeKind::Aggregate
        && node.extra.get("using_temporary_table").map(String::as_str) == Some("true"))
        || node.op.starts_with("Aggregate using temporary table");
    if !uses_tmp {
        return;
    }
    let Some(items) = &info.group_by else { return };
    let Some(scan) = driving_scan(node).filter(|s| !is_derived(s)) else {
        return;
    };
    let Some(t) = table_ref(scan) else { return };
    let table = resolve_table(info, &t);
    // Every grouping column must belong to the driving table, or no index can avoid the table.
    let own = own_names(scan);
    for it in items {
        if let Some(q) = &it.qualifier {
            let by_alias = info
                .aliases
                .iter()
                .find(|a| a.alias.eq_ignore_ascii_case(q));
            let ok = match by_alias {
                Some(a) => a.table.eq_ignore_ascii_case(&table.name),
                None => own.iter().any(|o| o.eq_ignore_ascii_case(q)),
            };
            if !ok {
                return;
            }
        }
    }
    let rows = node
        .children
        .first()
        .and_then(|c| c.actual_rows_total.or(c.est_rows));
    let Some(rows) = rows.filter(|r| *r >= cx.ctx.rules.filter_min_examined) else {
        return;
    };
    let group_cols: Vec<(String, bool)> = items.iter().map(|i| (i.column.clone(), false)).collect();
    let (conj, _) = filter_conjuncts(cx, scan);
    let (feq, _) = super::common::index_columns(&conj, false);
    let eq: Vec<ColRef> = feq
        .into_iter()
        .filter(|c| {
            !group_cols
                .iter()
                .any(|(n, _)| n.eq_ignore_ascii_case(&c.name))
        })
        .collect();
    let Some(cand) = candidate(cx, &table, &eq, &group_cols, None) else {
        return;
    };
    let mut d = Draft::new(
        AdviceRule::MysqlTempTableGroup,
        Severity::Warning,
        Confidence::Medium,
    )
    .node(node.id)
    .param("columns", columns_text(&cand));
    d.fingerprint_key = names_of(&cand);
    d.examined = Some(rows);
    d.needs_actual = true;
    d.evidence.push(ev(
        "groupRows",
        Some(node.id),
        rows,
        EvidenceUnit::Rows,
        node.children
            .first()
            .and_then(|c| c.actual_rows_total)
            .is_some(),
    ));
    d.evidence.push(ev_text(
        "groupKey",
        Some(node.id),
        group_cols
            .iter()
            .map(|(n, _)| n.clone())
            .collect::<Vec<_>>()
            .join(", "),
    ));
    d.params.insert("rows".into(), fmt_num(rows));
    d.table = Some(table);
    d.candidate = Some(cand);
    out.push(d);
}

// ------------------------------------------------------------------------------------------
// mysqlJoinBuffer
// ------------------------------------------------------------------------------------------

/// The full scan feeding one input of a join (through filters, `Hash` and readers).
fn scan_under(node: &PlanNode) -> Option<&PlanNode> {
    if node.object.is_some() && node.kind == PlanNodeKind::FullScan {
        return Some(node);
    }
    if node.children.len() == 1
        && matches!(
            node.kind,
            PlanNodeKind::Filter | PlanNodeKind::Gather | PlanNodeKind::Other
        )
    {
        return node.children.first().and_then(scan_under);
    }
    None
}

fn join_buffer(cx: &Cx<'_>, info: &StmtInfo, node: &PlanNode, out: &mut Vec<Draft>) {
    let rules = cx.ctx.rules;
    // (scan, join condition texts, algorithm, join node)
    let (scan, join_preds, algorithm): (&PlanNode, Vec<String>, String) =
        if node.kind == PlanNodeKind::FullScan {
            let buffer = node
                .extra
                .get("using_join_buffer")
                .cloned()
                .or_else(|| node.extra.get("join_buffer.join_type").cloned());
            let Some(alg) = buffer else { return };
            let mut preds: Vec<String> = Vec::new();
            for p in node
                .predicates
                .iter()
                .filter(|p| matches!(p.role, PredicateRole::Filter | PredicateRole::JoinCond))
            {
                preds.push(p.text.clone());
            }
            (node, preds, alg)
        } else if node.kind == PlanNodeKind::HashJoin && cx.tree.format != PlanFormat::TiDbRows {
            // The bigger input is the one that benefits from an index nested loop.
            let rows = |n: &PlanNode| n.actual_rows_total.or(n.est_rows).unwrap_or(0.0);
            let Some(s) = node
                .children
                .iter()
                .filter_map(scan_under)
                .max_by(|a, b| rows(a).total_cmp(&rows(b)))
            else {
                return;
            };
            let preds = node
                .predicates
                .iter()
                .filter(|p| p.role == PredicateRole::JoinCond)
                .map(|p| p.text.clone())
                .collect();
            (s, preds, "hash join".to_string())
        } else {
            return;
        };
    if is_derived(scan) {
        return;
    }
    let Some(t) = table_ref(scan) else { return };
    let table = resolve_table(info, &t);
    let own = own_names(scan);
    let mut conj: Vec<Conjunct> = Vec::new();
    for p in &join_preds {
        conj.extend(parse_conjuncts(cx.ctx.dialect, p, &own));
    }
    let (filter, parent) = filter_conjuncts(cx, scan);
    // Equality filters of the table go first in the key.
    let (join_eq, _) = super::common::index_columns(
        &conj
            .iter()
            .filter(|c| c.kind == CondKind::Join)
            .cloned()
            .collect::<Vec<_>>(),
        true,
    );
    if join_eq.is_empty() {
        return;
    }
    let (feq, _) = super::common::index_columns(&filter, false);
    let st = scan_stats(cx, scan, parent);
    let rows = st.examined.or(scan.est_rows).unwrap_or(0.0);
    if rows < rules.filter_min_examined {
        return;
    }
    let mut eq = feq;
    for c in join_eq {
        if !eq.iter().any(|x| x.key() == c.key()) {
            eq.push(c);
        }
    }
    let Some(cand) = candidate(cx, &table, &eq, &[], None) else {
        return;
    };
    let mut d = Draft::new(
        AdviceRule::MysqlJoinBuffer,
        warning(cx, scan.id, PlanRule::FullScanLargeTable)
            .map_or(Severity::Warning, |w| w.severity),
        Confidence::Medium,
    )
    .node(scan.id)
    .param("columns", columns_text(&cand))
    .param("algorithm", algorithm.clone());
    if node.id != scan.id {
        d = d.node(node.id);
    }
    d.fingerprint_key = names_of(&cand);
    d.examined = Some(rows);
    d.needs_actual = true;
    d.evidence.push(ev(
        "rowsRead",
        Some(scan.id),
        rows,
        EvidenceUnit::Rows,
        st.actual,
    ));
    d.evidence
        .push(ev_text("joinAlgorithm", Some(scan.id), algorithm));
    if warning(cx, scan.id, PlanRule::FullScanLargeTable).is_some() {
        d.from_warnings.push(PlanRule::FullScanLargeTable);
    }
    d.table = Some(table);
    d.candidate = Some(cand);
    out.push(d);
}

// ------------------------------------------------------------------------------------------
// mysqlIndexMerge
// ------------------------------------------------------------------------------------------

fn index_merge(cx: &Cx<'_>, info: &StmtInfo, node: &PlanNode, out: &mut Vec<Draft>) {
    let intersect = node.op.starts_with("Intersect rows")
        || (node.op == "Index merge"
            && (node
                .object
                .as_ref()
                .and_then(|o| o.index.as_deref())
                .is_some_and(|i| i.contains("intersect("))
                || node.extra.values().any(|v| v.contains("intersect("))));
    if !intersect {
        return;
    }
    let source = if node.object.is_some() {
        Some(node)
    } else {
        node.children.iter().find(|c| c.object.is_some())
    };
    let Some(source) = source else { return };
    let Some(t) = table_ref(source) else { return };
    let table = resolve_table(info, &t);
    let own = own_names(source);
    let mut conj: Vec<Conjunct> = Vec::new();
    for c in &node.children {
        for p in c
            .predicates
            .iter()
            .filter(|p| p.role == PredicateRole::IndexCond)
        {
            conj.extend(parse_conjuncts(cx.ctx.dialect, &p.text, &own));
        }
    }
    let (f, _) = filter_conjuncts(cx, source);
    conj.extend(f);
    let (eq, range) = super::common::index_columns(&conj, false);
    if eq.len() + usize::from(range.is_some()) < 2 {
        return;
    }
    let Some(cand) = candidate(cx, &table, &eq, &[], range.as_ref()) else {
        return;
    };
    let mut d = Draft::new(
        AdviceRule::MysqlIndexMerge,
        Severity::Warning,
        Confidence::Medium,
    )
    .node(node.id)
    .param("columns", columns_text(&cand));
    d.fingerprint_key = names_of(&cand);
    let used = node
        .object
        .as_ref()
        .and_then(|o| o.index.clone())
        .or_else(|| {
            let ix: Vec<String> = node
                .children
                .iter()
                .filter_map(|c| c.object.as_ref().and_then(|o| o.index.clone()))
                .collect();
            (!ix.is_empty()).then(|| ix.join(", "))
        });
    if let Some(u) = used {
        d.evidence
            .push(ev_text("usedIndex", Some(node.id), u.clone()));
        d.params.insert("usedIndex".into(), u);
    }
    d.table = Some(table);
    d.candidate = Some(cand);
    out.push(d);
}

// ------------------------------------------------------------------------------------------
// mysqlPossibleKeysUnused
// ------------------------------------------------------------------------------------------

fn possible_keys_unused(cx: &Cx<'_>, info: &StmtInfo, node: &PlanNode, out: &mut Vec<Draft>) {
    if node.kind != PlanNodeKind::FullScan || is_derived(node) {
        return;
    }
    let Some(possible) = node.extra.get("possible_keys").filter(|v| !v.is_empty()) else {
        return;
    };
    if node
        .object
        .as_ref()
        .and_then(|o| o.index.as_ref())
        .is_some()
    {
        return;
    }
    let Some(t) = table_ref(node) else { return };
    let table = resolve_table(info, &t);
    let st = scan_stats(cx, node, None);
    let Some(examined) = st.examined.or(node.est_rows) else {
        return;
    };
    let rules = cx.ctx.rules;
    if examined < rules.filter_min_examined {
        return;
    }
    let keys = split_list(possible);
    // Prefer a key named after a filter column (`customer_id` for `customer_id = 5`).
    let (conj, _) = filter_conjuncts(cx, node);
    let (eq, range) = super::common::index_columns(&conj, true);
    let pick = keys
        .iter()
        .find(|k| {
            eq.iter()
                .chain(range.iter())
                .any(|c| c.name.eq_ignore_ascii_case(k))
        })
        .or_else(|| keys.first())
        .cloned()
        .unwrap_or_default();
    let sel = st.selectivity();
    let warn = warning(cx, node.id, PlanRule::FullScanLargeTable);
    let low = sel.is_some_and(|s| s > rules.filter_max_selectivity);
    let mut d = Draft::new(
        AdviceRule::MysqlPossibleKeysUnused,
        warn.map_or(Severity::Warning, |w| w.severity),
        if low {
            Confidence::Low
        } else {
            Confidence::Medium
        },
    )
    .node(node.id)
    .param("keys", keys.join(", "))
    .param("index", pick.clone());
    d.needs_actual = true;
    d.examined = Some(examined);
    d.fingerprint_key = keys.join(",");
    d.evidence
        .push(ev_text("possibleKeys", Some(node.id), keys.join(", ")));
    d.evidence.push(ev(
        "rowsRead",
        Some(node.id),
        examined,
        EvidenceUnit::Rows,
        st.actual,
    ));
    if let Some(s) = sel {
        d.evidence.push(ev(
            "selectivity",
            Some(node.id),
            s * 100.0,
            EvidenceUnit::Pct,
            st.actual,
        ));
        d.params.insert("selectivity".into(), pct(s));
    }
    if low {
        d.caveats.push(caveat("optimizerLikelyRight", false));
    }
    if let Some(a) = statistics_action(cx.ctx, Some(&table)) {
        d.actions.push(a);
    }
    d.actions.push(AdviceAction {
        kind: ActionKind::Hint,
        label_key: "compareForceIndex".into(),
        sql: Some(format!(
            "SELECT ... FROM {} FORCE INDEX ({}) WHERE ...;",
            table_sql(cx.ctx, &table),
            quote_ident(cx.ctx.dialect, &pick)
        )),
        template: true,
        scope: ActionScope::Query,
        lock: None,
        params: BTreeMap::new(),
    });
    if warn.is_some() {
        d.from_warnings.push(PlanRule::FullScanLargeTable);
    }
    d.table = Some(table);
    out.push(d);
}

// ------------------------------------------------------------------------------------------
// Derived tables
// ------------------------------------------------------------------------------------------

/// A big derived table is materialised before the outer query can use any of its filters.
fn derived_table(cx: &Cx<'_>, node: &PlanNode, out: &mut Vec<Draft>) {
    if node.object.is_none()
        || !node
            .children
            .iter()
            .any(|c| c.kind == PlanNodeKind::Materialize)
    {
        return;
    }
    let rows = node
        .actual_rows_total
        .or(node.est_rows)
        .or_else(|| num(node, "rows_examined_per_scan"));
    let Some(rows) = rows.filter(|r| *r >= cx.ctx.rules.filter_min_examined) else {
        return;
    };
    let name = node
        .object
        .as_ref()
        .map(|o| o.name.clone())
        .unwrap_or_default();
    let warn = warning(cx, node.id, PlanRule::FullScanLargeTable);
    let mut d = Draft::new(
        AdviceRule::FullScanReview,
        warn.map_or(Severity::Warning, |w| w.severity),
        Confidence::Low,
    )
    .node(node.id)
    .param("rows", fmt_num(rows))
    .param("derived", name.clone());
    d.fingerprint_key = format!("derived:{}", name.to_lowercase());
    d.examined = Some(rows);
    d.evidence.push(ev(
        "rowsRead",
        Some(node.id),
        rows,
        EvidenceUnit::Rows,
        node.actual_rows_total.is_some(),
    ));
    d.actions
        .push(check_action("reviewDerivedTable", None, ActionScope::Query));
    d.caveats.push(caveat("derivedTableMaterialized", false));
    if warn.is_some() {
        d.from_warnings.push(PlanRule::FullScanLargeTable);
    }
    out.push(d);
}

// ------------------------------------------------------------------------------------------
// implicitConversion
// ------------------------------------------------------------------------------------------

fn is_textual(type_name: &str) -> bool {
    let t = type_name.trim().to_ascii_lowercase();
    [
        "char",
        "varchar",
        "text",
        "tinytext",
        "mediumtext",
        "longtext",
        "enum",
        "set",
    ]
    .iter()
    .any(|p| t.starts_with(p))
}

/// A string column compared with a number: MySQL converts the column, not the constant, so no
/// index on it can be used. Needs the column type, hence the catalog.
fn implicit_conversion(cx: &Cx<'_>, info: &StmtInfo, node: &PlanNode, out: &mut Vec<Draft>) {
    if cx.ctx.tables.is_empty() || node.object.is_none() || !is_scan_kind(node.kind) {
        return;
    }
    let Some(t) = table_ref(node) else { return };
    let table = resolve_table(info, &t);
    let Some(meta) = cx.ctx.table_meta(table.schema.as_deref(), &table.name) else {
        return;
    };
    let (mut conj, _) = filter_conjuncts(cx, node);
    conj.extend(index_cond_conjuncts(cx, node));
    for c in conj {
        if !matches!(c.kind, CondKind::Eq | CondKind::Range) || c.func.is_some() {
            continue;
        }
        let (Some(col), Some(lit)) = (&c.col, &c.literal) else {
            continue;
        };
        if lit.parse::<f64>().is_err() {
            continue;
        }
        let Some(cd) = meta
            .columns
            .iter()
            .find(|cd| cd.name.eq_ignore_ascii_case(&col.name))
            .filter(|cd| is_textual(&cd.type_name))
        else {
            continue;
        };
        let mut d = Draft::new(
            AdviceRule::ImplicitConversion,
            Severity::Warning,
            Confidence::Medium,
        )
        .node(node.id)
        .param("column", cd.name.clone())
        .param("columnType", cd.type_name.clone())
        .param("literal", lit.clone());
        d.fingerprint_key = cd.name.to_lowercase();
        d.evidence.push(ev_text(
            "columnType",
            Some(node.id),
            format!("{}: {}", cd.name, cd.type_name),
        ));
        d.evidence
            .push(ev_text("comparedWith", Some(node.id), lit.clone()));
        d.actions.push(AdviceAction {
            kind: ActionKind::Rewrite,
            label_key: "quoteLiteral".into(),
            sql: Some(format!(
                "{} {} '{}'",
                quote_ident(cx.ctx.dialect, &cd.name),
                if c.op.is_empty() { "=" } else { &c.op },
                lit
            )),
            template: true,
            scope: ActionScope::Query,
            lock: None,
            params: BTreeMap::new(),
        });
        d.table = Some(table.clone());
        out.push(d);
    }
}

// ------------------------------------------------------------------------------------------
// Spill (MariaDB filesort) and fallbacks
// ------------------------------------------------------------------------------------------

fn mariadb_sort_spill(cx: &Cx<'_>, node: &PlanNode, out: &mut Vec<Draft>) {
    if cx.tree.format != PlanFormat::MariaDbJson {
        return;
    }
    let Some(w) = warning(cx, node.id, PlanRule::SpillToDisk) else {
        return;
    };
    let mut d = Draft::new(AdviceRule::SortSpill, w.severity, Confidence::Low).node(node.id);
    d.from_warnings.push(PlanRule::SpillToDisk);
    if let Some(r) = num(node, "r_output_rows") {
        d.evidence
            .push(ev("sortRows", Some(node.id), r, EvidenceUnit::Rows, true));
        d.params.insert("rows".into(), fmt_num(r));
    }
    if let Some(m) = node.extra.get("r_sort_mode") {
        d.evidence
            .push(ev_text("sortMethod", Some(node.id), m.clone()));
    }
    d.actions.push(AdviceAction {
        kind: ActionKind::SessionSetting,
        label_key: "raiseSortBuffer".into(),
        sql: Some("SET SESSION sort_buffer_size = 8388608;".into()),
        template: false,
        scope: ActionScope::Session,
        lock: Some(LockRisk::None),
        params: BTreeMap::new(),
    });
    d.actions.push(check_action(
        "checkSortMergePasses",
        Some("SHOW SESSION STATUS LIKE 'Sort_merge_passes';"),
        ActionScope::Session,
    ));
    d.caveats.push(caveat("sortBufferPerThread", false));
    d.caveats.push(caveat("spillEstimate", false));
    d.fingerprint_key = "sort".into();
    out.push(d);
}

/// A full scan warning that no rule above answered: say it with the real table name.
fn full_scan_answer(cx: &Cx<'_>, info: &StmtInfo, node: &PlanNode, out: &mut Vec<Draft>) {
    let Some(w) = warning(cx, node.id, PlanRule::FullScanLargeTable) else {
        return;
    };
    let answered = out.iter().any(|d| {
        d.from_warnings.contains(&PlanRule::FullScanLargeTable) && d.node_ids.contains(&node.id)
    });
    if answered {
        return;
    }
    let mut d = Draft::new(AdviceRule::FullScanReview, w.severity, Confidence::Low).node(node.id);
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
    d.from_warnings.push(PlanRule::FullScanLargeTable);
    d.table = table_ref(node).map(|t| resolve_table(info, &t));
    if d.table.is_none() {
        d.fingerprint_key = format!("node:{}", node.op);
    }
    out.push(d);
}

// ------------------------------------------------------------------------------------------
// staleStatistics extras
// ------------------------------------------------------------------------------------------

/// A histogram on the filtered column for the generic statistics advice.
fn histograms(cx: &Cx<'_>, info: &StmtInfo, node: &PlanNode, out: &mut [Draft]) {
    let mariadb = is_mariadb(cx);
    let supported = match version_parts(cx.ctx.server_version) {
        Some((maj, _)) if !mariadb => maj >= 8,
        _ => true,
    };
    if cx.tree.format == PlanFormat::TiDbRows || !supported {
        return;
    }
    let Some(scan) = super::common::first_scan(node) else {
        return;
    };
    let (conj, _) = filter_conjuncts(cx, scan);
    let (eq, range) = super::common::index_columns(&conj, false);
    let Some(col) = eq.first().or(range.as_ref()).map(|c| c.name.clone()) else {
        return;
    };
    for d in out.iter_mut() {
        if d.rule != AdviceRule::StaleStatistics
            || d.node_ids.first() != Some(&node.id)
            || d.actions.iter().any(|a| a.label_key == "updateHistogram")
        {
            continue;
        }
        let Some(t) = d.table.clone().map(|t| resolve_table(info, &t)) else {
            continue;
        };
        let name = table_sql(cx.ctx, &t);
        let q = quote_ident(cx.ctx.dialect, &col);
        let sql = if mariadb {
            format!("ANALYZE TABLE {name} PERSISTENT FOR COLUMNS ({q}) INDEXES ();")
        } else {
            format!("ANALYZE TABLE {name} UPDATE HISTOGRAM ON {q} WITH 64 BUCKETS;")
        };
        let mut params = BTreeMap::new();
        params.insert("column".to_string(), col.clone());
        d.actions.push(AdviceAction {
            kind: ActionKind::Statistics,
            label_key: "updateHistogram".into(),
            sql: Some(sql),
            template: false,
            scope: ActionScope::Object,
            lock: Some(LockRisk::None),
            params,
        });
    }
}
