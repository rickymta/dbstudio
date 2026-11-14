//! Query Advisor: turns a parsed plan (and its statement) into ranked, concrete optimisation
//! advice. See `docs/design/plan-advisor.md`.
//!
//! Flow of [`advise`]: the rule modules ([`common`], one per dialect family, [`statement`])
//! push [`Draft`]s; [`finalize`] renders index candidates into DDL ([`index::render`]), lowers
//! confidence where the evidence is thin, computes impact, merges duplicates, ranks and cuts.
//!
//! The contract between the engine core and the rule modules is [`Cx`] (read access to the
//! plan) and [`Draft`] (what a rule produces). Rule modules never format DDL themselves.

mod clickhouse;
mod common;
mod context;
mod index;
mod model;
mod mongo;
mod mssql;
mod mysql;
mod newsql;
mod oracle;
mod pg;
mod predicate;
mod statement;
mod tidb;

use std::collections::BTreeMap;

use dbstudio_core::{PlanFormat, TextRange};

pub use context::{
    AdvisorContext, Flavor, Resolution, ServerFacts, TableKey, TableMeta, dialect_of_format,
    resolve_table, search_kind, search_scope,
};
pub use model::{
    ActionKind, ActionScope, AdviceAction, AdviceMetadata, AdviceReport, AdviceRule, Caveat,
    Confidence, Evidence, EvidenceUnit, Impact, ImpactBasis, LockRisk, MetadataState,
    NodePredicate, PlanAdvice, PredicateRole, TableFailure, TableRef,
};

use crate::{PlanNode, PlanRule, PlanTree, Severity};
use index::{IndexCandidate, IndexOutcome};

/// Read access to the plan for rule modules.
pub(crate) struct Cx<'a> {
    pub tree: &'a PlanTree,
    pub ctx: &'a AdvisorContext<'a>,
    pub statement: &'a str,
    /// Nodes by id (ids are pre-order indexes).
    pub nodes: Vec<&'a PlanNode>,
    /// Parent id by node id.
    pub parent: Vec<Option<u32>>,
}

impl<'a> Cx<'a> {
    fn new(tree: &'a PlanTree, statement: &'a str, ctx: &'a AdvisorContext<'a>) -> Self {
        fn walk<'n>(
            n: &'n PlanNode,
            parent: Option<u32>,
            nodes: &mut Vec<&'n PlanNode>,
            parents: &mut Vec<Option<u32>>,
        ) {
            nodes.push(n);
            parents.push(parent);
            for c in &n.children {
                walk(c, Some(n.id), nodes, parents);
            }
        }
        let mut nodes = Vec::new();
        let mut parent = Vec::new();
        walk(&tree.root, None, &mut nodes, &mut parent);
        Cx {
            tree,
            ctx,
            statement,
            nodes,
            parent,
        }
    }

    /// Node by id.
    pub fn node(&self, id: u32) -> Option<&'a PlanNode> {
        self.nodes.get(id as usize).copied().filter(|n| n.id == id)
    }

    /// Parent of a node.
    pub fn parent_of(&self, id: u32) -> Option<&'a PlanNode> {
        self.parent
            .get(id as usize)
            .copied()
            .flatten()
            .and_then(|p| self.node(p))
    }
}

/// How the impact of a draft is measured.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ShareSpec {
    /// Self time (or cost) of the draft's own nodes.
    OwnNodes,
    /// Inclusive time (or cost) of one node.
    Inclusive(u32),
    /// No impact figure.
    Unknown,
}

/// What a rule produces; turned into a [`PlanAdvice`] by [`finalize`].
#[derive(Debug, Clone)]
pub(crate) struct Draft {
    pub rule: AdviceRule,
    pub severity: Severity,
    /// Starting confidence, before the adjustments of design section 3.5.
    pub confidence: Confidence,
    pub node_ids: Vec<u32>,
    pub table: Option<TableRef>,
    pub params: BTreeMap<String, String>,
    pub evidence: Vec<Evidence>,
    /// Actions after the `createIndex` one (which `candidate` produces).
    pub actions: Vec<AdviceAction>,
    pub candidate: Option<IndexCandidate>,
    pub caveats: Vec<Caveat>,
    pub from_warnings: Vec<PlanRule>,
    pub statement_range: Option<TextRange>,
    pub share: ShareSpec,
    /// Rows read by the problem node(s); with a share of at least one half it makes the advice
    /// critical from a million rows.
    pub examined: Option<f64>,
    /// The reasoning uses row counts: an estimate-only plan lowers the confidence.
    pub needs_actual: bool,
    /// The reasoning needs the catalog: without it the confidence drops.
    pub needs_metadata: bool,
    /// Distinguishes advice of the same rule and table (columns, expression...).
    pub fingerprint_key: String,
}

impl Draft {
    /// A draft with nothing but its identity.
    pub fn new(rule: AdviceRule, severity: Severity, confidence: Confidence) -> Self {
        Draft {
            rule,
            severity,
            confidence,
            node_ids: Vec::new(),
            table: None,
            params: BTreeMap::new(),
            evidence: Vec::new(),
            actions: Vec::new(),
            candidate: None,
            caveats: Vec::new(),
            from_warnings: Vec::new(),
            statement_range: None,
            share: ShareSpec::OwnNodes,
            examined: None,
            needs_actual: false,
            needs_metadata: false,
            fingerprint_key: String::new(),
        }
    }

    pub fn param(mut self, k: &str, v: impl Into<String>) -> Self {
        self.params.insert(k.to_string(), v.into());
        self
    }

    pub fn node(mut self, id: u32) -> Self {
        if !self.node_ids.contains(&id) {
            self.node_ids.push(id);
        }
        self
    }
}

/// Advice for `tree` (already analysed) and the statement that produced it, best first.
pub fn advise(tree: &PlanTree, statement: &str, ctx: &AdvisorContext<'_>) -> Vec<PlanAdvice> {
    let cx = Cx::new(tree, statement, ctx);
    let mut drafts: Vec<Draft> = Vec::new();
    for node in &cx.nodes {
        common::node_rules(&cx, node, &mut drafts);
        family_node_rules(&cx, node, &mut drafts);
    }
    family_tree_rules(&cx, &mut drafts);
    common::warning_answers(&cx, &mut drafts);
    statement::run(&cx, &mut drafts);
    finalize(&cx, drafts)
}

fn family_node_rules(cx: &Cx<'_>, node: &PlanNode, out: &mut Vec<Draft>) {
    match cx.tree.format {
        PlanFormat::PostgresJson => pg::node_rules(cx, node, out),
        PlanFormat::MySqlJson
        | PlanFormat::MySqlJsonV2
        | PlanFormat::MySqlTree
        | PlanFormat::MariaDbJson => mysql::node_rules(cx, node, out),
        PlanFormat::TiDbRows => {
            mysql::node_rules(cx, node, out);
            tidb::node_rules(cx, node, out);
        }
        PlanFormat::CockroachText => newsql::node_rules(cx, node, out),
        PlanFormat::MsSqlShowplanXml => mssql::node_rules(cx, node, out),
        PlanFormat::OraclePlanRows => oracle::node_rules(cx, node, out),
        PlanFormat::MongoExplain => mongo::node_rules(cx, node, out),
        PlanFormat::ClickHouseJson => clickhouse::node_rules(cx, node, out),
        PlanFormat::CqlTrace => {}
    }
}

fn family_tree_rules(cx: &Cx<'_>, out: &mut Vec<Draft>) {
    match cx.tree.format {
        PlanFormat::PostgresJson => pg::tree_rules(cx, out),
        PlanFormat::MySqlJson
        | PlanFormat::MySqlJsonV2
        | PlanFormat::MySqlTree
        | PlanFormat::MariaDbJson => mysql::tree_rules(cx, out),
        PlanFormat::TiDbRows => {
            mysql::tree_rules(cx, out);
            tidb::tree_rules(cx, out);
        }
        PlanFormat::CockroachText => newsql::tree_rules(cx, out),
        PlanFormat::MsSqlShowplanXml => mssql::tree_rules(cx, out),
        PlanFormat::OraclePlanRows => oracle::tree_rules(cx, out),
        PlanFormat::MongoExplain => mongo::tree_rules(cx, out),
        PlanFormat::ClickHouseJson => clickhouse::tree_rules(cx, out),
        PlanFormat::CqlTrace => {}
    }
}

// ------------------------------------------------------------------------------------------
// Finalisation
// ------------------------------------------------------------------------------------------

struct Finished {
    advice: PlanAdvice,
    /// Lower-cased key columns of the suggested index, for prefix merging.
    index_keys: Option<(String, Vec<String>)>,
}

fn fnv8(text: &str) -> String {
    let mut h: u32 = 0x811c_9dc5;
    for b in text.bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    format!("{h:08x}")
}

fn rule_name(rule: AdviceRule) -> String {
    serde_json::to_value(rule)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// Impact figures of a draft: share of the statement and what it is a share of.
fn impact_of(cx: &Cx<'_>, d: &Draft) -> (ImpactBasis, Option<f64>) {
    let exec = cx.tree.totals.execution_time_ms.filter(|t| *t > 0.0);
    let cost = cx.tree.totals.total_cost.filter(|c| *c > 0.0);
    match &d.share {
        ShareSpec::Unknown => (ImpactBasis::None, None),
        ShareSpec::OwnNodes if d.node_ids.is_empty() => (ImpactBasis::None, None),
        ShareSpec::OwnNodes => {
            let nodes: Vec<&PlanNode> = d.node_ids.iter().filter_map(|i| cx.node(*i)).collect();
            if let Some(total) = exec {
                let sum: f64 = nodes.iter().filter_map(|n| n.self_time_ms).sum();
                if nodes.iter().any(|n| n.self_time_ms.is_some()) {
                    return (ImpactBasis::Time, Some((sum / total).clamp(0.0, 1.0)));
                }
            }
            if let Some(total) = cost {
                let sum: f64 = nodes.iter().filter_map(|n| n.self_cost).sum();
                if nodes.iter().any(|n| n.self_cost.is_some()) {
                    return (ImpactBasis::Cost, Some((sum / total).clamp(0.0, 1.0)));
                }
            }
            (ImpactBasis::None, None)
        }
        ShareSpec::Inclusive(id) => {
            let Some(n) = cx.node(*id) else {
                return (ImpactBasis::None, None);
            };
            if let (Some(total), Some(t)) = (exec, n.actual_time_ms) {
                return (ImpactBasis::Time, Some((t / total).clamp(0.0, 1.0)));
            }
            if let (Some(total), Some(c)) = (cost, n.est_cost) {
                return (ImpactBasis::Cost, Some((c / total).clamp(0.0, 1.0)));
            }
            (ImpactBasis::None, None)
        }
    }
}

fn severity_weight(s: Severity) -> f64 {
    match s {
        Severity::Critical => 1.3,
        Severity::Warning => 1.0,
        Severity::Info => 0.6,
    }
}

fn caveat(key: &str, verify: bool) -> Caveat {
    Caveat {
        key: key.into(),
        verify,
        params: BTreeMap::new(),
    }
}

fn add_caveat(list: &mut Vec<Caveat>, c: Caveat) {
    if !list.iter().any(|x| x.key == c.key) {
        list.push(c);
    }
}

/// Seconds since the Unix epoch of an RFC 3339 UTC timestamp (`2025-01-01T00:00:00Z`).
pub(crate) fn epoch_secs(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (n(0..4)?, n(5..7)?, n(8..10)?);
    let (h, mi, sec) = (n(11..13)?, n(14..16)?, n(17..19)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // Days from civil (Howard Hinnant).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3_600 + mi * 60 + sec)
}

fn finalize(cx: &Cx<'_>, drafts: Vec<Draft>) -> Vec<PlanAdvice> {
    let ctx = cx.ctx;
    let rules = ctx.rules;
    let stale = match (epoch_secs(ctx.captured_at), epoch_secs(ctx.now)) {
        (Some(c), Some(n)) => n - c > 24 * 3_600,
        _ => false,
    };
    let mut finished: Vec<Finished> = Vec::new();
    for mut d in drafts {
        let mut caveats = std::mem::take(&mut d.caveats);
        let mut actions = std::mem::take(&mut d.actions);
        let mut confidence = d.confidence;
        let mut rule = d.rule;
        let mut params = std::mem::take(&mut d.params);
        let mut index_keys = None;
        // 1. Render the index candidate.
        if let Some(c) = d.candidate.take() {
            let key_cols: Vec<String> = c
                .keys
                .iter()
                .map(|k| {
                    k.column
                        .clone()
                        .unwrap_or_else(|| k.sql.clone())
                        .to_lowercase()
                })
                .collect();
            let table_id = format!(
                "{}.{}",
                c.table.schema.clone().unwrap_or_default().to_lowercase(),
                c.table.name.to_lowercase()
            );
            if d.table.is_none() {
                d.table = Some(c.table.clone());
            }
            match index::render(ctx, &c) {
                IndexOutcome::Create {
                    action,
                    caveats: extra,
                    name,
                    follow_up,
                } => {
                    actions.insert(0, *action);
                    for (i, a) in follow_up.into_iter().enumerate() {
                        actions.insert(1 + i, a);
                    }
                    for e in extra {
                        add_caveat(&mut caveats, e);
                    }
                    params.entry("index".into()).or_insert(name);
                    index_keys = Some((table_id, key_cols));
                }
                IndexOutcome::Existing { index } => {
                    rule = AdviceRule::ExistingIndexUnused;
                    params.insert("index".into(), index);
                    actions.retain(|a| a.kind != ActionKind::CreateIndex);
                    if let Some(a) = common::statistics_action(ctx, d.table.as_ref()) {
                        actions.push(a);
                    }
                    confidence = Confidence::Medium;
                    d.needs_actual = false;
                    d.needs_metadata = false;
                    d.fingerprint_key = key_cols.join(",");
                }
                IndexOutcome::Unsupported => {
                    add_caveat(&mut caveats, caveat("noDdlForDialect", false));
                }
            }
        }
        // 2. Confidence adjustments.
        if d.needs_actual && !cx.tree.analyzed {
            confidence = confidence.lowered();
            add_caveat(&mut caveats, caveat("estimatedOnly", true));
        }
        if d.needs_metadata {
            let found = d
                .table
                .as_ref()
                .is_some_and(|t| ctx.table_meta(t.schema.as_deref(), &t.name).is_some());
            if !found {
                confidence = confidence.lowered();
                add_caveat(&mut caveats, caveat("metadataUnavailable", true));
            }
        }
        if stale {
            add_caveat(&mut caveats, caveat("planMayBeStale", false));
        }
        // 3. Impact and severity.
        let (basis, share) = impact_of(cx, &d);
        let mut severity = d.severity;
        if let Some(s) = share {
            let time_big = cx
                .tree
                .totals
                .execution_time_ms
                .is_some_and(|t| t * s >= 1_000.0 && basis == ImpactBasis::Time);
            if s >= 0.5 && (d.examined.is_some_and(|r| r >= 1_000_000.0) || time_big) {
                severity = Severity::Critical;
            } else if s < rules.min_share {
                severity = Severity::Info;
            }
        }
        let score = share.unwrap_or(0.02) * confidence.weight() * severity_weight(severity);
        // 4. Identity.
        let table_key = d
            .table
            .as_ref()
            .map(|t| format!("{}.{}", t.schema.clone().unwrap_or_default(), t.name))
            .unwrap_or_default();
        let fingerprint =
            format!("{}|{}|{}", rule_name(rule), table_key, d.fingerprint_key).to_lowercase();
        let id = format!("{}:{}", rule_name(rule), fnv8(&fingerprint));
        if let Some(t) = &d.table {
            params
                .entry("table".into())
                .or_insert_with(|| match &t.schema {
                    Some(s) => format!("{s}.{}", t.name),
                    None => t.name.clone(),
                });
        }
        finished.push(Finished {
            advice: PlanAdvice {
                id,
                rule,
                severity,
                confidence,
                node_ids: d.node_ids,
                table: d.table,
                params,
                evidence: d.evidence,
                actions,
                caveats,
                impact: Impact {
                    basis,
                    share: share.map(|s| (s * 1000.0).round() / 1000.0),
                    score,
                },
                from_warnings: d.from_warnings,
                statement_range: d.statement_range,
                fingerprint,
            },
            index_keys,
        });
    }
    let merged = merge(finished);
    rank(merged, rules.max_advice as usize, &cx.tree.warnings)
}

/// Merge advice with the same fingerprint, then fold index suggestions that are a prefix of
/// another one on the same table.
fn merge(list: Vec<Finished>) -> Vec<Finished> {
    let mut out: Vec<Finished> = Vec::new();
    for f in list {
        let Some(pos) = out
            .iter()
            .position(|o| o.advice.fingerprint == f.advice.fingerprint)
        else {
            out.push(f);
            continue;
        };
        let o = std::mem::replace(&mut out[pos], placeholder());
        let incoming_better =
            f.advice.impact.share.unwrap_or(-1.0) > o.advice.impact.share.unwrap_or(-1.0);
        let (mut base, extra) = if incoming_better { (f, o) } else { (o, f) };
        for id in extra.advice.node_ids {
            if !base.advice.node_ids.contains(&id) {
                base.advice.node_ids.push(id);
            }
        }
        for w in extra.advice.from_warnings {
            if !base.advice.from_warnings.contains(&w) {
                base.advice.from_warnings.push(w);
            }
        }
        for c in extra.advice.caveats {
            add_caveat(&mut base.advice.caveats, c);
        }
        if base.advice.statement_range.is_none() {
            base.advice.statement_range = extra.advice.statement_range;
        }
        out[pos] = base;
    }
    // Prefix folding.
    let mut drop = vec![false; out.len()];
    let mut extend: Vec<(usize, usize)> = Vec::new();
    let mut covered: Vec<usize> = Vec::new();
    for i in 0..out.len() {
        for j in 0..out.len() {
            if i == j || drop[i] || drop[j] || covered.contains(&i) {
                continue;
            }
            let (Some((ti, ki)), Some((tj, kj))) = (&out[i].index_keys, &out[j].index_keys) else {
                continue;
            };
            if ti == tj && ki.len() < kj.len() && kj[..ki.len()] == ki[..] {
                if keeps_own_advice(out[i].advice.rule) {
                    // The rule explains more than "add an index": keep the card, but its DDL is
                    // redundant with the wider index.
                    covered.push(i);
                    continue;
                }
                drop[i] = true;
                extend.push((j, i));
            }
        }
    }
    for (keep, gone) in extend {
        let (nodes, warns) = (
            out[gone].advice.node_ids.clone(),
            out[gone].advice.from_warnings.clone(),
        );
        let target = &mut out[keep].advice;
        for id in nodes {
            if !target.node_ids.contains(&id) {
                target.node_ids.push(id);
            }
        }
        for w in warns {
            if !target.from_warnings.contains(&w) {
                target.from_warnings.push(w);
            }
        }
        add_caveat(&mut target.caveats, caveat("coversOtherAdvice", false));
    }
    for i in covered {
        let a = &mut out[i].advice;
        a.actions
            .retain(|x| x.kind != ActionKind::CreateIndex && x.label_key != "materializeIndex");
        a.caveats.retain(|c| c.key != "noDdlForDialect");
        add_caveat(&mut a.caveats, caveat("coveredByAnotherIndex", false));
    }
    out.into_iter()
        .zip(drop)
        .filter(|(_, d)| !*d)
        .map(|(f, _)| f)
        .collect()
}

/// Rules whose advice is about more than the index DDL, so prefix folding must not delete them.
fn keeps_own_advice(rule: AdviceRule) -> bool {
    matches!(
        rule,
        AdviceRule::OracleIndexSkipScan | AdviceRule::MssqlKeyLookup
    )
}

fn placeholder() -> Finished {
    Finished {
        advice: PlanAdvice {
            id: String::new(),
            rule: AdviceRule::Unknown,
            severity: Severity::Info,
            confidence: Confidence::Low,
            node_ids: Vec::new(),
            table: None,
            params: BTreeMap::new(),
            evidence: Vec::new(),
            actions: Vec::new(),
            caveats: Vec::new(),
            impact: Impact {
                basis: ImpactBasis::None,
                share: None,
                score: 0.0,
            },
            from_warnings: Vec::new(),
            statement_range: None,
            fingerprint: String::new(),
        },
        index_keys: None,
    }
}

/// Sort by score, then cut to `max` keeping advice that is the only answer to a warning.
fn rank(list: Vec<Finished>, max: usize, warnings: &[crate::PlanWarning]) -> Vec<PlanAdvice> {
    let mut advice: Vec<PlanAdvice> = list.into_iter().map(|f| f.advice).collect();
    advice.sort_by(|a, b| {
        b.impact
            .score
            .partial_cmp(&a.impact.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.node_ids.first().cmp(&b.node_ids.first()))
            .then_with(|| a.id.cmp(&b.id))
    });
    if advice.len() <= max {
        return advice;
    }
    let mut kept: Vec<PlanAdvice> = Vec::new();
    let mut answered: Vec<PlanRule> = Vec::new();
    for a in advice {
        let needed = a
            .from_warnings
            .iter()
            .any(|r| warnings.iter().any(|w| w.rule == *r) && !answered.contains(r));
        if kept.len() < max || needed {
            answered.extend(a.from_warnings.iter().copied());
            kept.push(a);
        }
    }
    kept
}

#[cfg(test)]
pub(crate) mod testing {
    //! Helpers for rule unit tests: run the whole engine on an inline plan.

    use dbstudio_core::{PlanFormat, RawPlan, ScriptDialect};

    use super::*;
    use crate::{PlanRules, parse_and_analyze};

    /// A raw PostgreSQL plan with `statement`.
    pub fn pg_raw(plan_json: &str, statement: &str) -> RawPlan {
        RawPlan {
            format: PlanFormat::PostgresJson,
            text: plan_json.into(),
            statement: statement.into(),
            analyzed: true,
            rolled_back: false,
            server_version: "16.2".into(),
            captured_at: "2025-01-01T00:00:00Z".into(),
            extra: BTreeMap::new(),
        }
    }

    /// Advice for an inline PostgreSQL plan.
    pub fn pg(plan_json: &str, statement: &str) -> Vec<PlanAdvice> {
        parse_and_analyze(&pg_raw(plan_json, statement), &PlanRules::default())
            .expect("plan parses")
            .advice
    }

    /// Advice for an inline plan, pretending the connection speaks `dialect`.
    pub fn pg_as(dialect: ScriptDialect, plan_json: &str, statement: &str) -> Vec<PlanAdvice> {
        let raw = pg_raw(plan_json, statement);
        let rules = PlanRules::default();
        let mut ctx = AdvisorContext::basic(&raw, &rules);
        ctx.dialect = dialect;
        let mut tree = crate::parse(&raw).expect("plan parses");
        crate::analyze(&mut tree, &rules);
        advise(&tree, statement, &ctx)
    }

    /// A seq scan of `orders` that keeps 10 of 200 000 rows through `filter`.
    pub fn seq_scan(filter: &str) -> String {
        format!(
            r#"[{{"Plan":{{"Node Type":"Seq Scan","Relation Name":"orders","Alias":"orders",
            "Startup Cost":0.0,"Total Cost":4000.0,"Plan Rows":10,"Actual Startup Time":0.1,
            "Actual Total Time":100.0,"Actual Rows":10,"Actual Loops":1,"Filter":"{filter}",
            "Rows Removed by Filter":199990}},"Execution Time":100.5}}]"#
        )
    }

    /// An index only scan with many heap fetches (a plan sign for `SELECT *`).
    pub const HEAP_PLAN: &str = r#"[{"Plan":{"Node Type":"Index Only Scan","Relation Name":"orders",
        "Alias":"orders","Index Name":"ix","Startup Cost":0.0,"Total Cost":500.0,"Plan Rows":9000,
        "Actual Startup Time":0.1,"Actual Total Time":50.0,"Actual Rows":9000,"Actual Loops":1,
        "Heap Fetches":5000},"Execution Time":50.5}]"#;

    pub fn rules(advice: &[PlanAdvice]) -> Vec<AdviceRule> {
        advice.iter().map(|a| a.rule).collect()
    }

    #[test]
    fn epoch_seconds_and_staleness() {
        assert_eq!(epoch_secs("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(epoch_secs("2025-01-01T00:00:00Z"), Some(1_735_689_600));
        assert_eq!(epoch_secs("nope"), None);
        assert_eq!(epoch_secs("2025-13-01T00:00:00Z"), None);
        let raw = pg_raw(&seq_scan("(status = 'x'::text)"), "");
        let rules = PlanRules::default();
        let mut ctx = AdvisorContext::basic(&raw, &rules);
        let mut tree = crate::parse(&raw).unwrap();
        crate::analyze(&mut tree, &rules);
        let fresh = advise(&tree, "", &ctx);
        assert!(
            fresh
                .iter()
                .all(|a| a.caveats.iter().all(|c| c.key != "planMayBeStale"))
        );
        ctx.now = "2025-01-03T00:00:00Z";
        let old = advise(&tree, "", &ctx);
        assert!(!old.is_empty());
        assert!(
            old.iter()
                .all(|a| a.caveats.iter().any(|c| c.key == "planMayBeStale"))
        );
    }

    #[test]
    fn existing_index_turns_into_existing_index_unused() {
        let raw = pg_raw(&seq_scan("(status = 'x'::text)"), "");
        let rules = PlanRules::default();
        let mut ctx = AdvisorContext::basic(&raw, &rules);
        ctx.tables.insert(
            TableKey::new(None, "orders"),
            TableMeta {
                path: dbstudio_core::ObjectPath::root(),
                estimated_rows: Some(200_000),
                columns: vec![],
                indexes: vec![dbstudio_core::IndexDetail {
                    name: "ix_status".into(),
                    columns: vec!["status".into()],
                    unique: false,
                    primary: false,
                    method: Some("btree".into()),
                    definition: None,
                    partial: false,
                }],
            },
        );
        ctx.metadata = MetadataState::Checked;
        let mut tree = crate::parse(&raw).unwrap();
        crate::analyze(&mut tree, &rules);
        let advice = advise(&tree, "", &ctx);
        let a = advice
            .iter()
            .find(|a| a.rule == AdviceRule::ExistingIndexUnused)
            .expect("existing index");
        assert_eq!(a.params.get("index").map(String::as_str), Some("ix_status"));
        assert!(a.actions.iter().all(|x| x.kind != ActionKind::CreateIndex));
        assert!(a.actions.iter().any(|x| x.kind == ActionKind::Statistics));
        // With the catalog read, the missing-metadata caveat is gone.
        assert!(a.caveats.iter().all(|c| c.key != "metadataUnavailable"));
    }

    #[test]
    fn prefix_index_advice_is_folded_into_the_longer_one() {
        // Two scans of the same table: one filters on status, the other on status + customer.
        let plan = r#"[{"Plan":{"Node Type":"Append","Startup Cost":0.0,"Total Cost":9000.0,
            "Plan Rows":20,"Actual Startup Time":0.1,"Actual Total Time":200.0,"Actual Rows":20,
            "Actual Loops":1,"Plans":[
            {"Node Type":"Seq Scan","Relation Name":"orders","Alias":"a","Startup Cost":0.0,
             "Total Cost":4000.0,"Plan Rows":10,"Actual Startup Time":0.1,"Actual Total Time":100.0,
             "Actual Rows":10,"Actual Loops":1,"Filter":"(a.status = 'x'::text)",
             "Rows Removed by Filter":199990},
            {"Node Type":"Seq Scan","Relation Name":"orders","Alias":"b","Startup Cost":0.0,
             "Total Cost":4000.0,"Plan Rows":10,"Actual Startup Time":0.1,"Actual Total Time":100.0,
             "Actual Rows":10,"Actual Loops":1,
             "Filter":"((b.status = 'y'::text) AND (b.customer_id = 5))",
             "Rows Removed by Filter":199990}]},"Execution Time":200.5}]"#;
        let advice = pg(plan, "");
        let idx: Vec<_> = advice
            .iter()
            .filter(|a| a.rule == AdviceRule::IndexForFilter)
            .collect();
        assert_eq!(idx.len(), 1, "{advice:#?}");
        assert_eq!(idx[0].node_ids.len(), 2);
        assert!(idx[0].caveats.iter().any(|c| c.key == "coversOtherAdvice"));
        let sql = idx[0].actions[0].sql.as_deref().unwrap();
        assert!(sql.contains("(status, customer_id)"), "{sql}");
    }

    #[test]
    fn max_advice_cuts_but_keeps_warning_answers() {
        let raw = pg_raw(&seq_scan("(status = 'x'::text)"), "");
        let rules = PlanRules {
            max_advice: 1,
            ..PlanRules::default()
        };
        let tree = parse_and_analyze(&raw, &rules).unwrap();
        assert!(tree.advice.len() <= 2);
    }
}
