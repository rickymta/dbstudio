//! ClickHouse rules (design section 4.6).
//!
//! The plan is `EXPLAIN json = 1, indexes = 1`: no cost, no row counts, only the granules each
//! data skipping stage keeps. The parser stores every stage of a `ReadFromMergeTree` node as an
//! `index <Type> [<name>]` entry in `extra` (`granules 10/247, parts 10/10, keys a, b,
//! condition ...`); this module reads those back.
//!
//! ClickHouse has no B-tree: the "index" the advisor suggests is a data skipping index or a
//! projection. A candidate (`IndexCandidate`) cannot say which type, so these rules write the
//! `ALTER TABLE` actions themselves (see section 12 of the design, R5).

use std::collections::BTreeMap;

use dbstudio_core::ScriptDialect;
use dbstudio_sql::Dialect;
use dbstudio_sql::lexer::{TokenKind, tokenize};

use super::common::{check_action, ev, ev_text, table_ref};
use super::index::quote_ident;
use super::model::{
    ActionKind, ActionScope, AdviceAction, AdviceRule, Caveat, Confidence, EvidenceUnit, LockRisk,
    TableRef,
};
use super::{Cx, Draft};
use crate::json::fmt_num;
use crate::{PlanNode, PlanNodeKind, PlanRule, Severity};

/// Rows per granule the parser assumes (the default `index_granularity`).
const GRANULE_ROWS: f64 = 8192.0;
/// Fewest parts a partition stage must leave untouched before `chPartitionNotPruned` speaks.
const PARTS_MIN: f64 = 100.0;
/// A skip index below this many granules is not worth a remark.
const SKIP_MIN_GRANULES: f64 = 10.0;
/// Share of granules a skip index may keep before it counts as useless.
const SKIP_USELESS_SHARE: f64 = 0.9;
/// `FINAL` matters from this many granules.
const FINAL_MIN_GRANULES: f64 = 1000.0;

/// Rules evaluated for every node of a plan of this family.
pub(crate) fn node_rules(cx: &Cx<'_>, node: &PlanNode, out: &mut Vec<Draft>) {
    if is_merge_tree_read(node) {
        primary_key_unused(cx, node, out);
        partition_not_pruned(cx, node, out);
        skip_index_useless(cx, node, out);
    }
    join_right_large(cx, node, out);
}

/// Rules evaluated once per plan.
pub(crate) fn tree_rules(cx: &Cx<'_>, out: &mut Vec<Draft>) {
    final_modifier(cx, out);
}

// ------------------------------------------------------------------------------------------
// Reading the parser's output
// ------------------------------------------------------------------------------------------

fn is_merge_tree_read(node: &PlanNode) -> bool {
    node.op.starts_with("ReadFromMergeTree") && node.object.is_some()
}

/// One data skipping stage of a read, as the parser printed it.
#[derive(Debug, Clone)]
struct Stage {
    /// `MinMax`, `Partition`, `PrimaryKey`, `Skip`.
    kind: String,
    /// Index name (skip indexes).
    name: Option<String>,
    selected_granules: f64,
    initial_granules: f64,
    selected_parts: f64,
    initial_parts: f64,
    keys: Vec<String>,
    /// `Description` of a skip index (`set GRANULARITY 4`) is not kept by the parser.
    condition: Option<String>,
}

fn pair(text: &str) -> Option<(f64, f64)> {
    let (a, b) = text.trim().split_once('/')?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}

/// Text between `start` and the next of `ends` (or the end).
fn between<'a>(text: &'a str, start: &str, ends: &[&str]) -> Option<&'a str> {
    let from = text.find(start)? + start.len();
    let rest = &text[from..];
    let to = ends
        .iter()
        .filter_map(|e| rest.find(e))
        .min()
        .unwrap_or(rest.len());
    Some(&rest[..to])
}

fn stages(node: &PlanNode) -> Vec<Stage> {
    let mut out = Vec::new();
    for (label, text) in &node.extra {
        let Some(rest) = label.strip_prefix("index ") else {
            continue;
        };
        let (kind, name) = match rest.split_once(' ') {
            Some((k, n)) => (k.to_string(), Some(n.to_string())),
            None => (rest.to_string(), None),
        };
        let Some((sg, ig)) = between(text, "granules ", &[", "]).and_then(pair) else {
            continue;
        };
        let (sp, ip) = between(text, "parts ", &[", "])
            .and_then(pair)
            .unwrap_or((0.0, 0.0));
        let keys = between(text, ", keys ", &[", condition "])
            .map(|k| {
                k.split(", ")
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let condition = between(text, ", condition ", &[]).map(str::to_string);
        out.push(Stage {
            kind,
            name,
            selected_granules: sg,
            initial_granules: ig,
            selected_parts: sp,
            initial_parts: ip,
            keys,
            condition,
        });
    }
    out
}

fn stage_of<'a>(stages: &'a [Stage], kind: &str) -> Option<&'a Stage> {
    stages.iter().find(|s| s.kind == kind)
}

/// The collection a read names; the parser puts the database in `schema`.
fn ch_table(node: &PlanNode) -> Option<TableRef> {
    let mut t = table_ref(node)?;
    t.database = t.schema.clone();
    Some(t)
}

fn warning_severity(cx: &Cx<'_>, node: u32, rule: PlanRule) -> Option<Severity> {
    cx.tree
        .warnings
        .iter()
        .find(|w| w.node_id == node && w.rule == rule)
        .map(|w| w.severity)
}

// ------------------------------------------------------------------------------------------
// Conditions: ClickHouse prints them as nested function calls
// ------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Ex {
    Call(String, Vec<Ex>),
    Col(String),
    Lit,
    Other,
}

struct ExParser<'a> {
    s: &'a [u8],
    i: usize,
    depth: u32,
}

impl ExParser<'_> {
    fn skip_ws(&mut self) {
        while self.s.get(self.i).is_some_and(u8::is_ascii_whitespace) {
            self.i += 1;
        }
    }

    fn ident_char(c: u8) -> bool {
        c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'$')
    }

    /// `_String`, `_UInt8`, `_Nullable(String)`: the type a literal is printed with.
    fn skip_type_suffix(&mut self) {
        if self.s.get(self.i) == Some(&b'_') {
            while self.s.get(self.i).is_some_and(|c| Self::ident_char(*c)) {
                self.i += 1;
            }
            if self.s.get(self.i) == Some(&b'(') {
                self.skip_balanced(b'(', b')');
            }
        }
    }

    fn skip_balanced(&mut self, open: u8, close: u8) {
        let mut depth = 0;
        while let Some(&c) = self.s.get(self.i) {
            self.i += 1;
            if c == open {
                depth += 1;
            } else if c == close {
                depth -= 1;
                if depth == 0 {
                    return;
                }
            }
        }
    }

    fn expr(&mut self) -> Ex {
        self.depth += 1;
        let out = if self.depth > 32 {
            Ex::Other
        } else {
            self.expr_inner()
        };
        self.depth -= 1;
        out
    }

    fn expr_inner(&mut self) -> Ex {
        self.skip_ws();
        let Some(&c) = self.s.get(self.i) else {
            return Ex::Other;
        };
        if c == b'\'' {
            self.i += 1;
            while let Some(&c) = self.s.get(self.i) {
                self.i += 1;
                if c == b'\\' {
                    self.i += 1;
                } else if c == b'\'' {
                    break;
                }
            }
            self.skip_type_suffix();
            return Ex::Lit;
        }
        if c.is_ascii_digit()
            || (c == b'-' && self.s.get(self.i + 1).is_some_and(u8::is_ascii_digit))
        {
            self.i += 1;
            while self
                .s
                .get(self.i)
                .is_some_and(|c| c.is_ascii_digit() || matches!(c, b'.' | b'e' | b'E'))
            {
                self.i += 1;
            }
            self.skip_type_suffix();
            return Ex::Lit;
        }
        if c == b'[' {
            self.skip_balanced(b'[', b']');
            self.skip_type_suffix();
            return Ex::Lit;
        }
        if c == b'(' {
            self.i += 1;
            let args = self.args();
            return Ex::Call("tuple".into(), args);
        }
        if Self::ident_char(c) {
            let start = self.i;
            while self.s.get(self.i).is_some_and(|c| Self::ident_char(*c)) {
                self.i += 1;
            }
            let name = String::from_utf8_lossy(&self.s[start..self.i]).into_owned();
            if self.s.get(self.i) == Some(&b'(') {
                self.i += 1;
                let args = self.args();
                return Ex::Call(name, args);
            }
            return Ex::Col(name);
        }
        self.i += 1;
        Ex::Other
    }

    /// Arguments up to the closing parenthesis (already past the opening one).
    fn args(&mut self) -> Vec<Ex> {
        let mut out = Vec::new();
        loop {
            self.skip_ws();
            match self.s.get(self.i) {
                None => return out,
                Some(b')') => {
                    self.i += 1;
                    return out;
                }
                Some(b',') => {
                    self.i += 1;
                }
                Some(_) => {
                    let before = self.i;
                    out.push(self.expr());
                    if self.i == before {
                        self.i += 1;
                    }
                }
            }
        }
    }
}

fn parse_expr(text: &str) -> Ex {
    ExParser {
        s: text.as_bytes(),
        i: 0,
        depth: 0,
    }
    .expr()
}

/// What a condition says about a column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Use {
    Eq,
    Range,
    /// Substring or token search.
    Text,
    /// Token search (`hasToken`).
    Token,
}

/// `__table1.payload` to `payload`.
fn column_name(raw: &str) -> String {
    if raw.starts_with("__table") {
        if let Some((_, rest)) = raw.split_once('.') {
            return rest.to_string();
        }
    }
    raw.to_string()
}

fn collect_uses(ex: &Ex, out: &mut Vec<(String, Use)>) {
    let Ex::Call(f, args) = ex else { return };
    if f == "and" {
        for a in args {
            collect_uses(a, out);
        }
        return;
    }
    let Some(Ex::Col(c)) = args.first() else {
        return;
    };
    let use_ = match f.as_str() {
        "equals" | "in" | "has" | "globalIn" => Use::Eq,
        "less" | "greater" | "lessOrEquals" | "greaterOrEquals" => Use::Range,
        "like" | "ilike" | "match" | "position" | "positionCaseInsensitive" | "notEmpty" => {
            if f == "notEmpty" {
                return;
            }
            Use::Text
        }
        "hasToken" | "hasTokenCaseInsensitive" => Use::Token,
        _ => return,
    };
    let name = column_name(c);
    if !out.iter().any(|(n, u)| *n == name && *u == use_) {
        out.push((name, use_));
    }
}

/// Filter expressions that apply to a read: its own `PREWHERE` and the `WHERE` filters stacked
/// directly above it.
fn filter_exprs(cx: &Cx<'_>, read: &PlanNode) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(c) = &read.condition {
        for part in c.split("; ") {
            if let Some(e) = part.strip_prefix("PREWHERE ") {
                out.push(e.to_string());
            }
        }
    }
    let mut cur = cx.parent_of(read.id);
    while let Some(p) = cur {
        match p.kind {
            PlanNodeKind::Filter => {
                if let Some(c) = &p.condition {
                    out.push(c.clone());
                }
            }
            PlanNodeKind::Other => {}
            _ => break,
        }
        cur = cx.parent_of(p.id);
    }
    out
}

/// A skip index is already declared on a column when its condition names it.
fn covered_by_skip_index(stages: &[Stage], column: &str) -> bool {
    stages
        .iter()
        .filter(|s| s.kind == "Skip")
        .filter_map(|s| s.condition.as_deref())
        .any(|c| c.contains(column))
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

fn action(
    kind: ActionKind,
    label: &str,
    sql: Option<String>,
    scope: ActionScope,
    lock: LockRisk,
) -> AdviceAction {
    AdviceAction {
        kind,
        label_key: label.into(),
        sql,
        template: false,
        scope,
        lock: Some(lock),
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

fn table_sql(t: &TableRef) -> String {
    let q = |n: &str| quote_ident(ScriptDialect::ClickHouse, n);
    match &t.schema {
        Some(db) => format!("{}.{}", q(db), q(&t.name)),
        None => q(&t.name),
    }
}

/// `SELECT <column> FROM system.tables WHERE database = ... AND name = ...`.
fn system_tables_query(t: &TableRef, column: &str) -> String {
    let lit = |s: &str| Dialect::new(ScriptDialect::ClickHouse).literal_text(s);
    format!(
        "SELECT {column} FROM system.tables WHERE database = {} AND name = {};",
        lit(t.schema.as_deref().unwrap_or("currentDatabase()")),
        lit(&t.name)
    )
}

/// The `ORDER BY` key of a table when the catalog was read.
fn primary_key(cx: &Cx<'_>, t: &TableRef) -> Option<Vec<String>> {
    cx.ctx
        .table_meta(t.schema.as_deref(), &t.name)?
        .indexes
        .iter()
        .find(|i| i.primary)
        .map(|i| i.columns.clone())
        .filter(|c| !c.is_empty())
}

// ------------------------------------------------------------------------------------------
// chPrimaryKeyUnused
// ------------------------------------------------------------------------------------------

fn primary_key_unused(cx: &Cx<'_>, node: &PlanNode, out: &mut Vec<Draft>) {
    let st = stages(node);
    let Some(pk) = stage_of(&st, "PrimaryKey") else {
        return;
    };
    let rules = cx.ctx.rules;
    // The key kept (nearly) every granule it was offered, and it is a lot of data.
    if pk.initial_granules <= 0.0
        || pk.selected_granules < pk.initial_granules
        || pk.initial_granules * GRANULE_ROWS < rules.large_table_rows
    {
        return;
    }
    let Some(table) = ch_table(node) else { return };
    let exprs = filter_exprs(cx, node);
    if exprs.is_empty() {
        return;
    }
    let mut uses: Vec<(String, Use)> = Vec::new();
    for e in &exprs {
        collect_uses(&parse_expr(e), &mut uses);
    }
    let warn = warning_severity(cx, node.id, PlanRule::FullScanLargeTable);
    let pk_cols = primary_key(cx, &table);
    // Columns the filter uses and the key does not already lead with.
    let key_lead: Vec<String> = pk_cols.iter().flatten().map(|c| c.to_lowercase()).collect();
    let pick = |wanted: &[Use]| {
        uses.iter()
            .filter(|(n, u)| {
                wanted.contains(u)
                    && !covered_by_skip_index(&st, n)
                    && !key_lead.first().is_some_and(|k| *k == n.to_lowercase())
            })
            .map(|(n, u)| (n.clone(), *u))
            .next()
    };
    let target = pick(&[Use::Eq]).or_else(|| pick(&[Use::Token, Use::Text, Use::Range]));
    let rows = pk.selected_granules * GRANULE_ROWS;
    let mut d = Draft::new(
        AdviceRule::ChPrimaryKeyUnused,
        warn.unwrap_or(Severity::Warning),
        Confidence::Medium,
    )
    .node(node.id)
    .param("granules", fmt_num(pk.selected_granules))
    .param("rows", fmt_num(rows));
    // Without the table's `ORDER BY` key the rewrite is only a pointer: lower the confidence.
    d.needs_metadata = pk_cols.is_none();
    d.examined = Some(rows);
    d.fingerprint_key = target
        .as_ref()
        .map(|(c, _)| c.to_lowercase())
        .unwrap_or_default();
    d.evidence.push(ev(
        "granulesSelected",
        Some(node.id),
        pk.selected_granules,
        EvidenceUnit::Count,
        false,
    ));
    d.evidence.push(ev(
        "granulesTotal",
        Some(node.id),
        pk.initial_granules,
        EvidenceUnit::Count,
        false,
    ));
    d.evidence.push(ev(
        "rowsRead",
        Some(node.id),
        rows,
        EvidenceUnit::Rows,
        false,
    ));
    d.evidence
        .push(ev_text("filter", Some(node.id), exprs.join(" AND ")));
    if let Some(key) = &pk_cols {
        d.params.insert("primaryKey".into(), key.join(", "));
    }
    if warn.is_some() {
        d.from_warnings.push(PlanRule::FullScanLargeTable);
    }
    // (1) Filter on the ORDER BY key.
    let mut find_key = action(
        ActionKind::Check,
        "findSortingKey",
        Some(system_tables_query(&table, "sorting_key")),
        ActionScope::Object,
        LockRisk::None,
    );
    find_key.lock = None;
    if let Some(key) = &pk_cols {
        let mut rewrite = action(
            ActionKind::Rewrite,
            "filterOnPrimaryKey",
            None,
            ActionScope::Query,
            LockRisk::None,
        );
        rewrite.lock = None;
        rewrite.params.insert("primaryKey".into(), key.join(", "));
        d.actions.push(rewrite);
    } else {
        d.actions.push(find_key);
    }
    // (2) A skip index and (3) a projection on the column the filter uses.
    if let Some((col, use_)) = target {
        let t = table_sql(&table);
        let q = quote_ident(ScriptDialect::ClickHouse, &col);
        let name = quote_ident(
            ScriptDialect::ClickHouse,
            &format!("ix_{}_{}", sanitize(&table.name), sanitize(&col)),
        );
        let ty = match use_ {
            Use::Eq => "bloom_filter(0.01)",
            Use::Range => "minmax",
            Use::Text => "ngrambf_v1(3, 10240, 3, 0)",
            Use::Token => "tokenbf_v1(10240, 3, 0)",
        };
        let mut add = action(
            ActionKind::CreateIndex,
            "addSkipIndex",
            Some(format!(
                "ALTER TABLE {t} ADD INDEX {name} {q} TYPE {ty} GRANULARITY 4;"
            )),
            ActionScope::Object,
            LockRisk::None,
        );
        add.params.insert("column".into(), col.clone());
        d.actions.push(add);
        d.actions.push(action(
            ActionKind::Maintenance,
            "materializeSkipIndex",
            Some(format!("ALTER TABLE {t} MATERIALIZE INDEX {name};")),
            ActionScope::Object,
            LockRisk::HeavyMutation,
        ));
        let proj = quote_ident(ScriptDialect::ClickHouse, &format!("p_{}", sanitize(&col)));
        let mut add_proj = action(
            ActionKind::Maintenance,
            "addProjection",
            Some(format!(
                "ALTER TABLE {t} ADD PROJECTION {proj} (SELECT * ORDER BY {q});"
            )),
            ActionScope::Object,
            LockRisk::None,
        );
        add_proj.params.insert("column".into(), col.clone());
        d.actions.push(add_proj);
        d.actions.push(action(
            ActionKind::Maintenance,
            "materializeProjection",
            Some(format!("ALTER TABLE {t} MATERIALIZE PROJECTION {proj};")),
            ActionScope::Object,
            LockRisk::HeavyMutation,
        ));
        d.caveats.push(caveat("skipIndexTypeGuess", true));
        d.caveats.push(caveat("projectionStorage", false));
        d.params.insert("columns".into(), col);
    }
    d.table = Some(table);
    out.push(d);
}

// ------------------------------------------------------------------------------------------
// chPartitionNotPruned
// ------------------------------------------------------------------------------------------

fn partition_not_pruned(cx: &Cx<'_>, node: &PlanNode, out: &mut Vec<Draft>) {
    let st = stages(node);
    let Some(part) = stage_of(&st, "Partition") else {
        return;
    };
    if part.initial_parts < PARTS_MIN || part.selected_parts < part.initial_parts {
        return;
    }
    let Some(table) = ch_table(node) else { return };
    // Nothing to prune when the query has no filter at all.
    if filter_exprs(cx, node).is_empty() {
        return;
    }
    let mut d = Draft::new(
        AdviceRule::ChPartitionNotPruned,
        Severity::Warning,
        Confidence::Medium,
    )
    .node(node.id)
    .param("parts", fmt_num(part.initial_parts));
    d.fingerprint_key = String::new();
    d.evidence.push(ev(
        "partsSelected",
        Some(node.id),
        part.selected_parts,
        EvidenceUnit::Count,
        false,
    ));
    d.evidence.push(ev(
        "partsTotal",
        Some(node.id),
        part.initial_parts,
        EvidenceUnit::Count,
        false,
    ));
    let key = part
        .keys
        .first()
        .cloned()
        .or_else(|| stage_of(&st, "MinMax").and_then(|m| m.keys.first().cloned()));
    let mut rewrite = action(
        ActionKind::Rewrite,
        "filterOnPartitionKey",
        None,
        ActionScope::Query,
        LockRisk::None,
    );
    rewrite.lock = None;
    if let Some(k) = key {
        rewrite.params.insert("partitionKey".into(), k.clone());
        d.params.insert("partitionKey".into(), k);
    }
    d.actions.push(rewrite);
    let mut find = action(
        ActionKind::Check,
        "findPartitionKey",
        Some(system_tables_query(&table, "partition_key")),
        ActionScope::Object,
        LockRisk::None,
    );
    find.lock = None;
    d.actions.push(find);
    d.table = Some(table);
    out.push(d);
}

// ------------------------------------------------------------------------------------------
// chSkipIndexUseless
// ------------------------------------------------------------------------------------------

fn skip_index_useless(_cx: &Cx<'_>, node: &PlanNode, out: &mut Vec<Draft>) {
    let Some(table) = ch_table(node) else { return };
    for s in stages(node).iter().filter(|s| s.kind == "Skip") {
        let Some(name) = &s.name else { continue };
        if s.initial_granules < SKIP_MIN_GRANULES
            || s.selected_granules < SKIP_USELESS_SHARE * s.initial_granules
        {
            continue;
        }
        let share = s.selected_granules / s.initial_granules;
        let mut d = Draft::new(
            AdviceRule::ChSkipIndexUseless,
            Severity::Info,
            Confidence::Low,
        )
        .node(node.id)
        .param("index", name.clone())
        .param(
            "share",
            format!("{}%", fmt_num((share * 1000.0).round() / 10.0)),
        );
        d.fingerprint_key = name.to_lowercase();
        d.evidence.push(ev(
            "granulesSelected",
            Some(node.id),
            s.selected_granules,
            EvidenceUnit::Count,
            false,
        ));
        d.evidence.push(ev(
            "granulesTotal",
            Some(node.id),
            s.initial_granules,
            EvidenceUnit::Count,
            false,
        ));
        d.evidence.push(ev(
            "selectivity",
            Some(node.id),
            share * 100.0,
            EvidenceUnit::Pct,
            false,
        ));
        let lit = |v: &str| Dialect::new(ScriptDialect::ClickHouse).literal_text(v);
        let sql = format!(
            "SELECT name, type_full, granularity FROM system.data_skipping_indices WHERE database = {} AND `table` = {};",
            lit(table.schema.as_deref().unwrap_or("currentDatabase()")),
            lit(&table.name)
        );
        let mut review = check_action("reviewSkipIndex", Some(&sql), ActionScope::Object);
        review.params.insert("index".into(), name.clone());
        d.actions.push(review);
        d.table = Some(table.clone());
        out.push(d);
    }
}

// ------------------------------------------------------------------------------------------
// chFinal
// ------------------------------------------------------------------------------------------

/// Words in front of a top level `FINAL` keyword of the statement (the table or its alias).
fn final_targets(statement: &str) -> Option<Vec<String>> {
    let toks: Vec<(TokenKind, String)> = {
        let chars: Vec<char> = statement.chars().collect();
        tokenize(ScriptDialect::ClickHouse, statement)
            .into_iter()
            .filter(|t| !t.kind.is_trivia())
            .map(|t| {
                (
                    t.kind,
                    chars
                        .get(t.range.start as usize..t.range.end as usize)
                        .map(|c| c.iter().collect())
                        .unwrap_or_default(),
                )
            })
            .collect()
    };
    let mut out = Vec::new();
    let mut found = false;
    for (i, (kind, text)) in toks.iter().enumerate() {
        if *kind != TokenKind::Word || !text.eq_ignore_ascii_case("final") || i == 0 {
            continue;
        }
        let prev = &toks[i - 1];
        let keyword = prev.0 == TokenKind::Word
            && matches!(
                prev.1.to_ascii_lowercase().as_str(),
                "select"
                    | "distinct"
                    | "by"
                    | "and"
                    | "or"
                    | "where"
                    | "on"
                    | "as"
                    | "from"
                    | "join"
                    | "not"
                    | "when"
                    | "then"
                    | "else"
                    | "case"
                    | "prewhere"
                    | "having"
            );
        if keyword || !matches!(prev.0, TokenKind::Word | TokenKind::QuotedIdent) {
            continue;
        }
        found = true;
        out.push(prev.1.trim_matches('`').trim_matches('"').to_string());
        // `FROM t AS x FINAL`: the table is two words back.
        if i >= 3 && toks[i - 2].1.eq_ignore_ascii_case("as") {
            out.push(
                toks[i - 3]
                    .1
                    .trim_matches('`')
                    .trim_matches('"')
                    .to_string(),
            );
        }
    }
    found.then_some(out)
}

fn final_modifier(cx: &Cx<'_>, out: &mut Vec<Draft>) {
    let Some(targets) = final_targets(cx.statement) else {
        return;
    };
    let reads: Vec<&PlanNode> = cx
        .nodes
        .iter()
        .copied()
        .filter(|n| is_merge_tree_read(n))
        .collect();
    let named: Vec<&PlanNode> = reads
        .iter()
        .copied()
        .filter(|n| {
            n.object
                .as_ref()
                .is_some_and(|o| targets.iter().any(|t| t.eq_ignore_ascii_case(&o.name)))
        })
        .collect();
    let pool = if named.is_empty() { reads } else { named };
    for node in pool {
        let granules = stage_of(&stages(node), "PrimaryKey")
            .map(|s| s.selected_granules)
            .or_else(|| {
                node.extra
                    .get("granules_selected")
                    .and_then(|g| g.parse().ok())
            })
            .unwrap_or(0.0);
        if granules < FINAL_MIN_GRANULES {
            continue;
        }
        let Some(table) = ch_table(node) else {
            continue;
        };
        let mut d =
            Draft::new(AdviceRule::ChFinal, Severity::Warning, Confidence::Low).node(node.id);
        d.fingerprint_key = String::new();
        d.evidence.push(ev(
            "granulesSelected",
            Some(node.id),
            granules,
            EvidenceUnit::Count,
            false,
        ));
        d.params.insert("granules".into(), fmt_num(granules));
        d.actions.push(action(
            ActionKind::SessionSetting,
            "finalPerPartition",
            Some("SET do_not_merge_across_partitions_select_final = 1;".into()),
            ActionScope::Session,
            LockRisk::None,
        ));
        d.caveats.push(caveat("finalPartitionSemantics", true));
        d.table = Some(table);
        out.push(d);
    }
}

// ------------------------------------------------------------------------------------------
// chJoinRightLarge
// ------------------------------------------------------------------------------------------

/// Granules the reads below `node` keep, and the tables they read.
fn side_granules(node: &PlanNode, tables: &mut Vec<String>) -> Option<f64> {
    let mut total: Option<f64> = None;
    if is_merge_tree_read(node) {
        if let Some(g) = node
            .extra
            .get("granules_selected")
            .and_then(|g| g.parse::<f64>().ok())
        {
            total = Some(g);
            if let Some(o) = &node.object {
                tables.push(o.name.clone());
            }
        }
    }
    for c in &node.children {
        if let Some(g) = side_granules(c, tables) {
            total = Some(total.unwrap_or(0.0) + g);
        }
    }
    total
}

fn join_right_large(cx: &Cx<'_>, node: &PlanNode, out: &mut Vec<Draft>) {
    if node.op != "Join" {
        return;
    }
    let [left, right] = node.children.as_slice() else {
        return;
    };
    let (mut lt, mut rt) = (Vec::new(), Vec::new());
    let (Some(lg), Some(rg)) = (side_granules(left, &mut lt), side_granules(right, &mut rt)) else {
        return;
    };
    let min_granules = (cx.ctx.rules.large_table_rows / GRANULE_ROWS).ceil();
    if rg <= lg || rg < min_granules {
        return;
    }
    let mut d = Draft::new(
        AdviceRule::ChJoinRightLarge,
        Severity::Warning,
        Confidence::Low,
    )
    .node(node.id)
    .param("left", lt.join(", "))
    .param("right", rt.join(", "));
    d.fingerprint_key = format!("{}|{}", lt.join(","), rt.join(","));
    d.evidence.push(ev(
        "granulesLeft",
        Some(left.id),
        lg,
        EvidenceUnit::Count,
        false,
    ));
    d.evidence.push(ev(
        "granulesRight",
        Some(right.id),
        rg,
        EvidenceUnit::Count,
        false,
    ));
    let mut swap = action(
        ActionKind::Rewrite,
        "swapJoinSides",
        None,
        ActionScope::Query,
        LockRisk::None,
    );
    swap.lock = None;
    d.actions.push(swap);
    d.actions.push(action(
        ActionKind::SessionSetting,
        "graceHashJoin",
        Some("SET join_algorithm = 'grace_hash';".into()),
        ActionScope::Session,
        LockRisk::None,
    ));
    d.caveats.push(caveat("joinSizesFromGranules", true));
    out.push(d);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uses(text: &str) -> Vec<(String, Use)> {
        let mut out = Vec::new();
        collect_uses(&parse_expr(text), &mut out);
        out
    }

    #[test]
    fn expressions_as_clickhouse_prints_them() {
        assert_eq!(
            uses("equals(__table1.payload, 'zzz'_String)"),
            vec![("payload".to_string(), Use::Eq)]
        );
        assert_eq!(
            uses(
                "and(equals(__table1.kind, 'a'_String), greaterOrEquals(__table1.ts, '2024-01-05'_String))"
            ),
            vec![
                ("kind".to_string(), Use::Eq),
                ("ts".to_string(), Use::Range)
            ]
        );
        // Functions of a column, runtime filters and junk give nothing.
        assert!(uses("equals(toDate(__table1.ts), '2024-01-05'_String)").is_empty());
        assert!(uses("and(__applyFilter(_runtime_filter_1_0, __table1.user_id))").is_empty());
        assert!(uses("").is_empty());
        assert!(uses("((((").is_empty());
        assert_eq!(
            uses("hasToken(__table1.msg, 'err'_String)"),
            vec![("msg".to_string(), Use::Token)]
        );
        assert_eq!(
            uses("in(__table1.id, __set_1_2)"),
            vec![("id".to_string(), Use::Eq)]
        );
        // A quote inside a literal.
        assert_eq!(
            uses("equals(__table1.a, 'it\\'s'_String)"),
            vec![("a".to_string(), Use::Eq)]
        );
    }

    #[test]
    fn stage_text_round_trips() {
        let mut n = PlanNode::new("ReadFromMergeTree", PlanNodeKind::FullScan);
        n.extra.insert(
            "index PrimaryKey".into(),
            "granules 10/247, parts 3/10, keys user_id, ts, condition and((ts in [1, +Inf)), (user_id in [42, 42]))".into(),
        );
        n.extra.insert(
            "index Skip idx_kind".into(),
            "granules 247/247, parts 10/10, condition (kind in ['a', 'a'])".into(),
        );
        let st = stages(&n);
        let pk = stage_of(&st, "PrimaryKey").unwrap();
        assert_eq!(
            (
                pk.selected_granules,
                pk.initial_granules,
                pk.selected_parts,
                pk.initial_parts
            ),
            (10.0, 247.0, 3.0, 10.0)
        );
        assert_eq!(pk.keys, vec!["user_id".to_string(), "ts".to_string()]);
        let skip = stage_of(&st, "Skip").unwrap();
        assert_eq!(skip.name.as_deref(), Some("idx_kind"));
        assert!(covered_by_skip_index(&st, "kind"));
        assert!(!covered_by_skip_index(&st, "payload"));
    }

    use crate::advice::model::PlanAdvice;
    use crate::{PlanRules, parse_and_analyze};
    use dbstudio_core::{PlanFormat, RawPlan};

    fn advise(plan: &str, statement: &str) -> Vec<PlanAdvice> {
        let raw = RawPlan {
            format: PlanFormat::ClickHouseJson,
            text: plan.into(),
            statement: statement.into(),
            analyzed: false,
            rolled_back: false,
            server_version: "26.3".into(),
            captured_at: "2026-11-14T00:00:00Z".into(),
            extra: BTreeMap::new(),
        };
        parse_and_analyze(&raw, &PlanRules::default())
            .expect("plan parses")
            .advice
    }

    /// A MergeTree read whose stages keep `sel` of `init` granules and `psel` of `pinit` parts.
    fn read(table: &str, init: u32, sel: u32, pinit: u32, psel: u32) -> String {
        format!(
            r#"{{"Node Type":"ReadFromMergeTree","Description":"{table}","Indexes":[
            {{"Type":"Partition","Condition":"true","Initial Parts":{pinit},"Selected Parts":{psel},
              "Initial Granules":{init},"Selected Granules":{init}}},
            {{"Type":"PrimaryKey","Condition":"true","Initial Parts":{psel},"Selected Parts":{psel},
              "Initial Granules":{init},"Selected Granules":{sel}}}]}}"#
        )
    }

    fn filtered(read: &str, filter: &str) -> String {
        format!(
            r#"[{{"Plan":{{"Node Type":"Filter","Filter Column":"{filter}","Plans":[{read}]}}}}]"#
        )
    }

    fn rules_of(a: &[PlanAdvice]) -> Vec<AdviceRule> {
        a.iter().map(|x| x.rule).collect()
    }

    #[test]
    fn known_primary_key_gives_a_rewrite_with_medium_confidence() {
        let plan = filtered(
            &read("db.t", 5000, 5000, 3, 3),
            "equals(__table1.a, 5_UInt8)",
        );
        let raw = RawPlan {
            format: PlanFormat::ClickHouseJson,
            text: plan,
            statement: String::new(),
            analyzed: false,
            rolled_back: false,
            server_version: "26.3".into(),
            captured_at: "2026-11-14T00:00:00Z".into(),
            extra: BTreeMap::new(),
        };
        let rules = PlanRules::default();
        let mut ctx = crate::advice::AdvisorContext::basic(&raw, &rules);
        ctx.tables.insert(
            crate::advice::TableKey::new(Some("db"), "t"),
            crate::advice::TableMeta {
                path: dbstudio_core::ObjectPath::root(),
                estimated_rows: None,
                columns: vec![],
                indexes: vec![dbstudio_core::IndexDetail {
                    name: "PRIMARY".into(),
                    columns: vec!["user_id".into(), "ts".into()],
                    unique: false,
                    primary: true,
                    method: None,
                    definition: None,
                    partial: false,
                }],
            },
        );
        ctx.metadata = crate::advice::MetadataState::Checked;
        let mut tree = crate::parse(&raw).unwrap();
        crate::analyze(&mut tree, &rules);
        let a = crate::advice::advise(&tree, "", &ctx);
        let p = a
            .iter()
            .find(|x| x.rule == AdviceRule::ChPrimaryKeyUnused)
            .unwrap();
        assert_eq!(p.confidence, Confidence::Medium);
        assert_eq!(p.params["primaryKey"], "user_id, ts");
        assert!(
            p.actions
                .iter()
                .any(|x| x.label_key == "filterOnPrimaryKey")
        );
        assert!(p.caveats.iter().all(|c| c.key != "metadataUnavailable"));
    }

    #[test]
    fn small_reads_and_unfiltered_reads_are_not_reported() {
        let small = filtered(&read("db.t", 10, 10, 3, 3), "equals(__table1.a, 5_UInt8)");
        assert!(!rules_of(&advise(&small, "")).contains(&AdviceRule::ChPrimaryKeyUnused));
        // No filter at all: reading everything is what the query asked for.
        let plain = format!("[{{\"Plan\":{}}}]", read("db.t", 5000, 5000, 3, 3));
        assert!(!rules_of(&advise(&plain, "")).contains(&AdviceRule::ChPrimaryKeyUnused));
        // A filter the key narrows is fine.
        let narrowed = filtered(&read("db.t", 5000, 40, 3, 3), "equals(__table1.a, 5_UInt8)");
        assert!(!rules_of(&advise(&narrowed, "")).contains(&AdviceRule::ChPrimaryKeyUnused));
    }

    #[test]
    fn partitions_not_pruned() {
        let plan = filtered(
            &read("db.t", 5000, 300, 300, 300),
            "equals(__table1.a, 5_UInt8)",
        );
        let a = advise(&plan, "");
        let p = a
            .iter()
            .find(|x| x.rule == AdviceRule::ChPartitionNotPruned)
            .unwrap();
        assert_eq!(p.params["parts"], "300");
        // Fewer than 100 parts: not worth a remark.
        let few = filtered(
            &read("db.t", 5000, 300, 30, 30),
            "equals(__table1.a, 5_UInt8)",
        );
        assert!(!rules_of(&advise(&few, "")).contains(&AdviceRule::ChPartitionNotPruned));
    }

    #[test]
    fn join_with_a_bigger_right_side() {
        let join = |left: u32, right: u32| {
            format!(
                r#"[{{"Plan":{{"Node Type":"Join","Algorithm":"HashJoin","Clauses":"[(a) = (b)]",
                "Plans":[{},{}]}}}}]"#,
                read("db.l", left, left, 2, 2),
                read("db.r", right, right, 2, 2)
            )
        };
        let a = advise(&join(10, 900), "");
        let j = a
            .iter()
            .find(|x| x.rule == AdviceRule::ChJoinRightLarge)
            .unwrap();
        assert_eq!(
            (j.params["left"].as_str(), j.params["right"].as_str()),
            ("l", "r")
        );
        assert!(
            j.actions
                .iter()
                .any(|x| x.sql.as_deref() == Some("SET join_algorithm = 'grace_hash';"))
        );
        assert!(!rules_of(&advise(&join(900, 10), "")).contains(&AdviceRule::ChJoinRightLarge));
        // Both small.
        assert!(!rules_of(&advise(&join(1, 4), "")).contains(&AdviceRule::ChJoinRightLarge));
    }

    #[test]
    fn final_on_a_big_read() {
        let plan = format!("[{{\"Plan\":{}}}]", read("db.rep", 5000, 5000, 3, 3));
        let a = advise(&plan, "SELECT count() FROM db.rep FINAL");
        let f = a.iter().find(|x| x.rule == AdviceRule::ChFinal).unwrap();
        assert!(
            f.actions[0]
                .sql
                .as_deref()
                .unwrap()
                .contains("do_not_merge_across_partitions_select_final")
        );
        // No FINAL in the statement, or a small read: nothing.
        assert!(
            !rules_of(&advise(&plan, "SELECT count() FROM db.rep")).contains(&AdviceRule::ChFinal)
        );
        let small = format!("[{{\"Plan\":{}}}]", read("db.rep", 50, 50, 3, 3));
        assert!(
            !rules_of(&advise(&small, "SELECT count() FROM db.rep FINAL"))
                .contains(&AdviceRule::ChFinal)
        );
    }

    proptest::proptest! {
        #[test]
        fn expression_parser_never_panics(s in "[ -~]{0,160}") {
            let mut out = Vec::new();
            collect_uses(&parse_expr(&s), &mut out);
            let _ = final_targets(&s);
        }

        #[test]
        fn stage_parser_never_panics(label in "index [A-Za-z ]{0,12}", text in "[ -~]{0,120}") {
            let mut n = PlanNode::new("ReadFromMergeTree", PlanNodeKind::FullScan);
            n.extra.insert(label, text);
            let _ = stages(&n);
        }
    }

    #[test]
    fn final_keyword_detection() {
        assert_eq!(
            final_targets("SELECT count() FROM rep FINAL WHERE v = 'z'"),
            Some(vec!["rep".to_string()])
        );
        assert_eq!(
            final_targets("SELECT 1 FROM db.t AS x FINAL"),
            Some(vec!["x".to_string(), "t".to_string()])
        );
        assert_eq!(final_targets("SELECT 'FINAL' FROM t -- FINAL"), None);
        assert_eq!(final_targets("SELECT final FROM t"), None);
    }
}
