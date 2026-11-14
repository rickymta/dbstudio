//! Static analysis of the explained statement (design section 4.8).
//!
//! The statement is tokenised with the dialect's lexer (so strings and comments never fire a
//! rule) and scanned for a few well known anti-patterns. The plan, when it confirms the
//! suspicion (a full scan of the table the column belongs to), raises the confidence.

use std::collections::BTreeMap;

use dbstudio_core::{PlanFormat, ScriptDialect, TextRange};
use dbstudio_sql::lexer::{TokenKind, tokenize};

use super::common::{check_action, ev, ev_text, table_ref};
use super::index::{IndexCandidate, IndexKey, quote_ident};
use super::model::{
    ActionKind, ActionScope, AdviceAction, AdviceRule, Caveat, Confidence, EvidenceUnit, LockRisk,
    TableRef,
};
use super::{Cx, Draft};
use crate::json::fmt_num;
use crate::{PlanNode, PlanNodeKind, PlanRule, Severity};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Clause {
    Select,
    From,
    Where,
    On,
    Other,
}

#[derive(Debug, Clone)]
struct Sig {
    kind: TokenKind,
    text: String,
    lower: String,
    start: u32,
    end: u32,
    depth: u32,
    clause: Clause,
}

impl Sig {
    fn is_word(&self, w: &str) -> bool {
        self.kind == TokenKind::Word && self.lower == w
    }

    fn is_punct(&self, c: char) -> bool {
        self.kind == TokenKind::Punct && self.text.len() == 1 && self.text.starts_with(c)
    }
}

/// Words that are never a column name.
const KEYWORDS: &[&str] = &[
    "select",
    "from",
    "where",
    "and",
    "or",
    "not",
    "in",
    "is",
    "null",
    "like",
    "ilike",
    "between",
    "exists",
    "case",
    "when",
    "then",
    "else",
    "end",
    "as",
    "on",
    "join",
    "inner",
    "left",
    "right",
    "full",
    "outer",
    "cross",
    "group",
    "order",
    "by",
    "having",
    "limit",
    "offset",
    "union",
    "all",
    "distinct",
    "any",
    "some",
    "true",
    "false",
    "interval",
    "date",
    "timestamp",
    "asc",
    "desc",
    "using",
    "with",
    "over",
    "partition",
    "fetch",
    "first",
    "next",
    "rows",
    "row",
    "only",
    "for",
    "update",
    "set",
    "values",
    "into",
    "returning",
    "top",
    "escape",
    "similar",
    "to",
];

fn is_keyword(w: &str) -> bool {
    KEYWORDS.contains(&w)
}

fn lex(cx: &Cx<'_>) -> Vec<Sig> {
    let chars: Vec<char> = cx.statement.chars().collect();
    let mut sigs: Vec<Sig> = Vec::new();
    let mut stack = vec![Clause::Other];
    let mut depth = 0u32;
    for t in tokenize(cx.ctx.dialect, cx.statement) {
        if t.kind.is_trivia() {
            continue;
        }
        let text: String = chars
            .get(t.range.start as usize..t.range.end as usize)
            .map(|c| c.iter().collect())
            .unwrap_or_default();
        let lower = text.to_lowercase();
        let mut sig = Sig {
            kind: t.kind,
            text,
            lower,
            start: t.range.start,
            end: t.range.end,
            depth,
            clause: *stack.last().unwrap_or(&Clause::Other),
        };
        if sig.is_punct('(') {
            let top = *stack.last().unwrap_or(&Clause::Other);
            stack.push(top);
            depth += 1;
        } else if sig.is_punct(')') {
            if stack.len() > 1 {
                stack.pop();
            }
            depth = depth.saturating_sub(1);
            sig.depth = depth;
            sig.clause = *stack.last().unwrap_or(&Clause::Other);
        } else if sig.kind == TokenKind::Word {
            let new = match sig.lower.as_str() {
                "select" => Some(Clause::Select),
                "from" | "join" => Some(Clause::From),
                "where" => Some(Clause::Where),
                "on" => Some(Clause::On),
                "group" | "order" | "having" | "limit" | "union" | "offset" | "fetch" | "set"
                | "values" | "returning" | "window" | "intersect" | "except" | "minus" => {
                    Some(Clause::Other)
                }
                _ => None,
            };
            if let (Some(c), Some(top)) = (new, stack.last_mut()) {
                *top = c;
            }
            sig.clause = *stack.last().unwrap_or(&Clause::Other);
        }
        sigs.push(sig);
    }
    sigs
}

/// Index of the `)` matching the `(` at `open`.
fn close_of(sigs: &[Sig], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    for (i, s) in sigs.iter().enumerate().skip(open) {
        if s.is_punct('(') {
            depth += 1;
        } else if s.is_punct(')') {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
    }
    None
}

/// Length in tokens of a comparison operator at `i` (`=`, `<=`, `<>`, `LIKE`, `IN`...).
fn comparison_at(sigs: &[Sig], i: usize) -> bool {
    let Some(s) = sigs.get(i) else { return false };
    match s.kind {
        TokenKind::Punct => {
            s.is_punct('=')
                || s.is_punct('<')
                || s.is_punct('>')
                || (s.is_punct('!') && sigs.get(i + 1).is_some_and(|n| n.is_punct('=')))
        }
        TokenKind::Word => matches!(s.lower.as_str(), "like" | "ilike" | "in" | "between" | "is"),
        _ => false,
    }
}

fn range(a: &Sig, b: &Sig) -> TextRange {
    TextRange {
        start: a.start,
        end: b.end,
    }
}

fn text_of(cx: &Cx<'_>, r: TextRange) -> String {
    cx.statement
        .chars()
        .skip(r.start as usize)
        .take((r.end - r.start) as usize)
        .collect()
}

/// A scan the plan did as a full scan of the table `qualifier` names (or any, when unqualified).
fn confirm_scan<'a>(cx: &'a Cx<'_>, qualifier: Option<&str>) -> Option<&'a PlanNode> {
    cx.nodes.iter().copied().find(|n| {
        n.kind == PlanNodeKind::FullScan
            && n.loops != Some(0)
            && table_ref(n).is_some()
            && match qualifier {
                None => true,
                Some(q) => n.object.as_ref().is_some_and(|o| {
                    o.name.eq_ignore_ascii_case(q)
                        || o.alias
                            .as_deref()
                            .is_some_and(|a| a.eq_ignore_ascii_case(q))
                }),
            }
    })
}

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

fn is_sql(d: ScriptDialect) -> bool {
    matches!(
        d,
        ScriptDialect::Postgres
            | ScriptDialect::MySql { .. }
            | ScriptDialect::MsSql
            | ScriptDialect::Oracle
            | ScriptDialect::ClickHouse
    )
}

pub(crate) fn run(cx: &Cx<'_>, drafts: &mut Vec<Draft>) {
    if cx.statement.trim().is_empty()
        || !is_sql(cx.ctx.dialect)
        || cx.tree.format == PlanFormat::MongoExplain
    {
        return;
    }
    let sigs = lex(cx);
    non_sargable(cx, &sigs, drafts);
    leading_wildcard(cx, &sigs, drafts);
    or_across_columns(cx, &sigs, drafts);
    select_star(cx, &sigs, drafts);
    large_offset(cx, &sigs, drafts);
    not_in_subquery(cx, &sigs, drafts);
}

// ------------------------------------------------------------------------------------------

/// Column name and qualifier of the identifier ending at `i` (`t.col` or `col`).
fn colref_ending_at(sigs: &[Sig], i: usize) -> Option<(Option<String>, String)> {
    let s = sigs.get(i)?;
    if !matches!(s.kind, TokenKind::Word | TokenKind::QuotedIdent)
        || (s.kind == TokenKind::Word && is_keyword(&s.lower))
    {
        return None;
    }
    let name = unquote(&s.text);
    let qualifier = if i >= 2 && sigs[i - 1].is_punct('.') {
        sigs.get(i - 2)
            .filter(|q| matches!(q.kind, TokenKind::Word | TokenKind::QuotedIdent))
            .map(|q| unquote(&q.text))
    } else {
        None
    };
    Some((qualifier, name))
}

fn unquote(s: &str) -> String {
    let c: Vec<char> = s.chars().collect();
    if c.len() >= 2 && matches!(c[0], '"' | '`' | '[') {
        c[1..c.len() - 1].iter().collect()
    } else {
        s.to_string()
    }
}

const INDEXABLE_FUNCS: &[&str] = &[
    "lower", "upper", "abs", "trim", "btrim", "ltrim", "rtrim", "length",
];

fn non_sargable(cx: &Cx<'_>, sigs: &[Sig], drafts: &mut Vec<Draft>) {
    let mut seen: Vec<String> = Vec::new();
    for i in 0..sigs.len() {
        let s = &sigs[i];
        if !matches!(s.clause, Clause::Where | Clause::On) {
            continue;
        }
        // (a) function call over a column followed by a comparison.
        let mut hit: Option<(usize, usize, String, Option<String>, String)> = None; // (start, end idx, func, qualifier, column)
        if s.kind == TokenKind::Word
            && !is_keyword(&s.lower)
            && sigs.get(i + 1).is_some_and(|n| n.is_punct('('))
        {
            if let Some(close) = close_of(sigs, i + 1) {
                let after_cmp = comparison_at(sigs, close + 1);
                let before_cmp = i > 0 && comparison_at(sigs, i - 1) && !sigs[i - 1].is_word("in");
                let args = &sigs[i + 2..close];
                // First bare column among the arguments, ignoring `AS type` tails.
                let mut col: Option<(Option<String>, String)> = None;
                let mut j = 0;
                while j < args.len() {
                    let a = &args[j];
                    if a.is_word("as") {
                        break;
                    }
                    let next_is_call = args.get(j + 1).is_some_and(|n| n.is_punct('('));
                    let next_is_dot = args.get(j + 1).is_some_and(|n| n.is_punct('.'));
                    if matches!(a.kind, TokenKind::Word | TokenKind::QuotedIdent)
                        && !next_is_call
                        && !next_is_dot
                        && !(a.kind == TokenKind::Word && is_keyword(&a.lower))
                        && col.is_none()
                    {
                        col = colref_ending_at(args, j);
                    }
                    j += 1;
                }
                if after_cmp && !before_cmp {
                    if let Some((q, c)) = col {
                        hit = Some((i, close, s.lower.clone(), q, c));
                    }
                }
            }
        }
        // (b) arithmetic: `col + 1 = ...`.
        if hit.is_none()
            && matches!(s.kind, TokenKind::Word | TokenKind::QuotedIdent)
            && !(s.kind == TokenKind::Word && is_keyword(&s.lower))
            && !sigs
                .get(i + 1)
                .is_some_and(|n| n.is_punct('(') || n.is_punct('.'))
        {
            if let (Some(op), Some(num), true) =
                (sigs.get(i + 1), sigs.get(i + 2), comparison_at(sigs, i + 3))
            {
                if op.kind == TokenKind::Punct
                    && matches!(op.text.as_str(), "+" | "-" | "*" | "/" | "%")
                    && num.kind == TokenKind::Number
                {
                    if let Some((q, c)) = colref_ending_at(sigs, i) {
                        hit = Some((i, i + 2, op.text.clone(), q, c));
                    }
                }
            }
        }
        let Some((start, end, func, qualifier, column)) = hit else {
            continue;
        };
        let r = range(&sigs[start], &sigs[end]);
        let expr_text = text_of(cx, r);
        let key = format!("{}|{}", func, column.to_lowercase());
        if seen.contains(&key) {
            continue;
        }
        seen.push(key.clone());
        // The plan already explained it: only point at the text.
        if let Some(existing) = drafts.iter_mut().find(|d| {
            d.rule == AdviceRule::PgFunctionOnColumn
                && d.params
                    .get("column")
                    .is_some_and(|c| c.eq_ignore_ascii_case(&column))
        }) {
            existing.statement_range.get_or_insert(r);
            continue;
        }
        let confirm = confirm_scan(cx, qualifier.as_deref());
        let confirmed = confirm.is_some();
        let mut d = Draft::new(
            AdviceRule::NonSargablePredicate,
            if confirmed {
                Severity::Warning
            } else {
                Severity::Info
            },
            if confirmed {
                Confidence::High
            } else {
                Confidence::Low
            },
        )
        .param("function", func.clone())
        .param("column", column.clone())
        .param("expression", expr_text.clone());
        d.statement_range = Some(r);
        d.fingerprint_key = key;
        d.evidence.push(ev_text("expression", None, expr_text));
        if let Some(n) = confirm {
            d = d.node(n.id);
            d.table = table_ref(n);
            d.evidence
                .push(ev_text("planConfirms", Some(n.id), n.op.clone()));
        }
        d.actions.push(action(
            ActionKind::Rewrite,
            "moveFunctionToConstant",
            None,
            false,
            ActionScope::Query,
            None,
        ));
        if cx.ctx.dialect == ScriptDialect::Postgres && INDEXABLE_FUNCS.contains(&func.as_str()) {
            let q = |n: &str| quote_ident(ScriptDialect::Postgres, n);
            let expr = format!("({func}({}))", q(&column));
            let name_part = format!("{func}_{column}");
            if let Some(t) = d.table.clone() {
                d.candidate = Some(IndexCandidate {
                    table: t,
                    keys: vec![IndexKey::expression(expr, name_part)],
                    eq_count: 0,
                    include: Vec::new(),
                });
            } else {
                let name = format!(
                    "ix_{}",
                    name_part
                        .to_lowercase()
                        .replace(|c: char| !c.is_ascii_alphanumeric(), "_")
                );
                let mut a = action(
                    ActionKind::CreateIndex,
                    "createIndex",
                    Some(format!(
                        "CREATE INDEX CONCURRENTLY IF NOT EXISTS {name} ON /* table */ ({expr});"
                    )),
                    true,
                    ActionScope::Object,
                    Some(LockRisk::ShortLock),
                );
                a.params.insert("index".into(), name);
                d.actions.push(a);
            }
        }
        drafts.push(d);
    }
}

fn leading_wildcard(cx: &Cx<'_>, sigs: &[Sig], drafts: &mut Vec<Draft>) {
    let mut seen: Vec<String> = Vec::new();
    for i in 0..sigs.len() {
        let s = &sigs[i];
        if !(s.is_word("like") || s.is_word("ilike"))
            || !matches!(s.clause, Clause::Where | Clause::On)
        {
            continue;
        }
        let Some(lit) = sigs.get(i + 1).filter(|n| n.kind == TokenKind::String) else {
            continue;
        };
        let content = lit
            .text
            .split_once('\'')
            .map_or(lit.text.as_str(), |(_, r)| r);
        if !matches!(content.chars().next(), Some('%') | Some('_')) {
            continue;
        }
        // Column: the identifier before LIKE (or before NOT LIKE).
        let mut k = i;
        if k > 0 && sigs[k - 1].is_word("not") {
            k -= 1;
        }
        let Some((qualifier, column)) = k.checked_sub(1).and_then(|p| colref_ending_at(sigs, p))
        else {
            continue;
        };
        let key = format!("{}|{}", column.to_lowercase(), content);
        if seen.contains(&key) {
            continue;
        }
        seen.push(key.clone());
        let confirm = confirm_scan(cx, qualifier.as_deref());
        let confirmed = confirm.is_some();
        let r = range(s, lit);
        let mut d = Draft::new(
            AdviceRule::LeadingWildcardLike,
            if confirmed {
                Severity::Warning
            } else {
                Severity::Info
            },
            if confirmed {
                Confidence::Medium
            } else {
                Confidence::Low
            },
        )
        .param("column", column.clone())
        .param("pattern", lit.text.clone());
        d.statement_range = Some(r);
        d.fingerprint_key = key;
        d.evidence.push(ev_text("pattern", None, lit.text.clone()));
        if let Some(n) = confirm {
            d = d.node(n.id);
            d.table = table_ref(n);
        }
        match cx.ctx.dialect {
            ScriptDialect::Postgres => {
                let q = |n: &str| quote_ident(ScriptDialect::Postgres, n);
                let table_sql = d.table.as_ref().map_or_else(
                    || "/* table */".to_string(),
                    |t: &TableRef| match &t.schema {
                        Some(s) => format!("{}.{}", q(s), q(&t.name)),
                        None => q(&t.name),
                    },
                );
                let name = format!(
                    "ix_{}_{}_trgm",
                    d.table
                        .as_ref()
                        .map_or("t".to_string(), |t| t.name.to_lowercase()),
                    column.to_lowercase()
                )
                .replace(|c: char| !c.is_ascii_alphanumeric() && c != '_', "_");
                let mut a = action(
                    ActionKind::CreateIndex,
                    "createTrigramIndex",
                    Some(format!(
                        "CREATE INDEX CONCURRENTLY IF NOT EXISTS {name} ON {table_sql} USING gin ({} gin_trgm_ops);",
                        q(&column)
                    )),
                    d.table.is_none(),
                    ActionScope::Object,
                    Some(LockRisk::ShortLock),
                );
                a.params.insert("index".into(), name);
                d.actions.push(a);
                let mut c = caveat("requiresExtension", true);
                c.params.insert("extension".into(), "pg_trgm".into());
                d.caveats.push(c);
            }
            ScriptDialect::MySql { .. } => {
                d.actions.push(action(
                    ActionKind::Rewrite,
                    "useFulltext",
                    None,
                    false,
                    ActionScope::Query,
                    None,
                ));
                d.caveats.push(caveat("fulltextSemantics", false));
            }
            _ => d
                .actions
                .push(check_action("noSuitableIndex", None, ActionScope::Query)),
        }
        drafts.push(d);
    }
}

fn or_across_columns(cx: &Cx<'_>, sigs: &[Sig], drafts: &mut Vec<Draft>) {
    let Some(w) = sigs.iter().position(|s| s.is_word("where")) else {
        return;
    };
    let depth = sigs[w].depth;
    // Tokens of the clause: until a terminator at the same depth, or the closing parenthesis.
    let mut end = sigs.len();
    for (j, s) in sigs.iter().enumerate().skip(w + 1) {
        if s.depth < depth
            || (s.depth == depth
                && s.kind == TokenKind::Word
                && matches!(
                    s.lower.as_str(),
                    "group"
                        | "order"
                        | "having"
                        | "limit"
                        | "union"
                        | "offset"
                        | "fetch"
                        | "window"
                        | "intersect"
                        | "except"
                        | "returning"
                ))
        {
            end = j;
            break;
        }
    }
    let clause = &sigs[w + 1..end];
    let cuts: Vec<usize> = clause
        .iter()
        .enumerate()
        .filter(|(_, s)| s.depth == depth && s.is_word("or"))
        .map(|(i, _)| i)
        .collect();
    if cuts.is_empty() {
        return;
    }
    let mut parts: Vec<&[Sig]> = Vec::new();
    let mut start = 0;
    for c in &cuts {
        parts.push(&clause[start..*c]);
        start = c + 1;
    }
    parts.push(&clause[start..]);
    let mut columns: Vec<String> = Vec::new();
    for p in &parts {
        let col = p.iter().enumerate().find_map(|(j, s)| {
            let is_col = matches!(s.kind, TokenKind::Word | TokenKind::QuotedIdent)
                && !(s.kind == TokenKind::Word && is_keyword(&s.lower))
                && !p
                    .get(j + 1)
                    .is_some_and(|n| n.is_punct('(') || n.is_punct('.'));
            is_col.then(|| unquote(&s.text).to_lowercase())
        });
        match col {
            Some(c) if !columns.contains(&c) => columns.push(c),
            Some(_) => {}
            None => return,
        }
    }
    if columns.len() < 2 {
        return;
    }
    // A plan that uses an index for the whole OR (BitmapOr) needs no rewrite.
    if cx.nodes.iter().any(|n| n.op == "BitmapOr") {
        return;
    }
    let confirm = cx.nodes.iter().copied().find(|n| {
        n.kind == PlanNodeKind::FullScan
            && n.predicates
                .iter()
                .any(|p| p.text.to_lowercase().contains(" or "))
    });
    if confirm.is_none() && !cx.nodes.iter().any(|n| n.kind == PlanNodeKind::FullScan) {
        return;
    }
    let (Some(first), Some(last)) = (clause.first(), clause.last()) else {
        return;
    };
    let r = range(first, last);
    let texts: Vec<String> = parts
        .iter()
        .filter_map(|p| Some(text_of(cx, range(p.first()?, p.last()?)).trim().to_string()))
        .collect();
    let mut d = Draft::new(
        AdviceRule::OrAcrossColumns,
        if confirm.is_some() {
            Severity::Warning
        } else {
            Severity::Info
        },
        if confirm.is_some() {
            Confidence::Medium
        } else {
            Confidence::Low
        },
    )
    .param("columns", columns.join(", "));
    d.statement_range = Some(r);
    d.fingerprint_key = columns.join(",");
    if let Some(n) = confirm {
        d = d.node(n.id);
        d.table = table_ref(n);
    }
    if texts.len() <= 4 {
        let mut branches = Vec::new();
        for (i, t) in texts.iter().enumerate() {
            let mut cond = t.clone();
            for earlier in &texts[..i] {
                cond.push_str(&format!(" AND NOT ({earlier})"));
            }
            branches.push(format!(
                "SELECT /* columns */ FROM /* table */ WHERE {cond}"
            ));
        }
        d.actions.push(action(
            ActionKind::Rewrite,
            "rewriteUnionAll",
            Some(branches.join("\nUNION ALL\n")),
            true,
            ActionScope::Query,
            None,
        ));
    }
    d.actions
        .push(check_action("indexEachOrColumn", None, ActionScope::Object));
    d.caveats.push(caveat("unionAllSemantics", false));
    drafts.push(d);
}

fn select_star(cx: &Cx<'_>, sigs: &[Sig], drafts: &mut Vec<Draft>) {
    let Some(sel) = sigs
        .iter()
        .position(|s| s.is_word("select") && s.depth == 0)
    else {
        return;
    };
    let mut j = sel + 1;
    while sigs
        .get(j)
        .is_some_and(|s| s.is_word("distinct") || s.is_word("all"))
    {
        j += 1;
    }
    let star = match sigs.get(j) {
        Some(s) if s.is_punct('*') => Some(s),
        Some(s)
            if matches!(s.kind, TokenKind::Word | TokenKind::QuotedIdent)
                && sigs.get(j + 1).is_some_and(|n| n.is_punct('.'))
                && sigs.get(j + 2).is_some_and(|n| n.is_punct('*')) =>
        {
            sigs.get(j + 2)
        }
        _ => None,
    };
    let Some(star) = star else { return };
    // Only with a sign in the plan that wider rows cost something.
    let mut nodes: Vec<&PlanNode> = Vec::new();
    let mut sign = "";
    for n in &cx.nodes {
        if n.kind == PlanNodeKind::KeyLookup {
            nodes.push(n);
            sign = "keyLookup";
        } else if n.kind == PlanNodeKind::IndexOnlyScan
            && n.extra
                .get("Heap Fetches")
                .and_then(|v| v.parse::<f64>().ok())
                .is_some_and(|f| f >= 1000.0)
        {
            nodes.push(n);
            sign = "heapFetches";
        }
    }
    if nodes.is_empty() {
        for w in cx
            .tree
            .warnings
            .iter()
            .filter(|w| w.rule == PlanRule::SpillToDisk)
        {
            if let Some(n) = cx.node(w.node_id) {
                nodes.push(n);
                sign = "spill";
            }
        }
    }
    if nodes.is_empty() {
        return;
    }
    let mut d = Draft::new(AdviceRule::SelectStar, Severity::Info, Confidence::Medium);
    for n in &nodes {
        d = d.node(n.id);
    }
    d.statement_range = Some(TextRange {
        start: star.start,
        end: star.end,
    });
    d.evidence
        .push(ev_text("planSign", nodes.first().map(|n| n.id), sign));
    d.params.insert("sign".into(), sign.into());
    d.actions.push(action(
        ActionKind::Rewrite,
        "listColumns",
        None,
        false,
        ActionScope::Query,
        None,
    ));
    d.fingerprint_key = "star".into();
    drafts.push(d);
}

fn large_offset(cx: &Cx<'_>, sigs: &[Sig], drafts: &mut Vec<Draft>) {
    let limit_value = |from: usize| -> Option<String> {
        sigs.iter()
            .enumerate()
            .find(|(i, s)| *i >= from.saturating_sub(0) && s.is_word("limit"))
            .and_then(|(i, _)| sigs.get(i + 1))
            .filter(|n| n.kind == TokenKind::Number)
            .map(|n| n.text.clone())
    };
    let mut hit: Option<(usize, usize, f64)> = None;
    for (i, s) in sigs.iter().enumerate() {
        if s.is_word("offset") {
            if let Some(n) = sigs.get(i + 1).filter(|n| n.kind == TokenKind::Number) {
                if let Ok(v) = n.text.parse::<f64>() {
                    hit = Some((i, i + 1, v));
                    break;
                }
            }
        }
        if matches!(
            cx.ctx.dialect,
            ScriptDialect::MySql { .. } | ScriptDialect::ClickHouse
        ) && s.is_word("limit")
        {
            if let (Some(a), Some(c), Some(_)) = (sigs.get(i + 1), sigs.get(i + 2), sigs.get(i + 3))
            {
                if a.kind == TokenKind::Number && c.is_punct(',') {
                    if let Ok(v) = a.text.parse::<f64>() {
                        hit = Some((i, i + 1, v));
                        break;
                    }
                }
            }
        }
    }
    let Some((a, b, value)) = hit else { return };
    if value < cx.ctx.rules.large_offset {
        return;
    }
    let mut d = Draft::new(AdviceRule::LargeOffset, Severity::Warning, Confidence::High);
    d.statement_range = Some(range(&sigs[a], &sigs[b]));
    d.params.insert("offset".into(), fmt_num(value));
    d.evidence
        .push(ev("offset", None, value, EvidenceUnit::Rows, false));
    d.fingerprint_key = "offset".into();
    // Keyset pagination from the ORDER BY of the statement.
    let page = limit_value(0).unwrap_or_else(|| "50".into());
    let mut order: Vec<(String, bool)> = Vec::new();
    if let Some(o) = sigs
        .iter()
        .position(|s| s.is_word("order") && s.depth == sigs[a].depth)
    {
        if sigs.get(o + 1).is_some_and(|s| s.is_word("by")) {
            let mut j = o + 2;
            let mut cur = String::new();
            let mut desc = false;
            while let Some(s) = sigs.get(j) {
                if s.depth < sigs[a].depth
                    || s.is_word("limit")
                    || s.is_word("offset")
                    || s.is_word("fetch")
                    || s.is_punct(';')
                {
                    break;
                }
                if s.is_punct(',') {
                    order.push((std::mem::take(&mut cur), desc));
                    desc = false;
                } else if s.is_word("desc") {
                    desc = true;
                } else if !s.is_word("asc") {
                    if !cur.is_empty() && !s.is_punct('.') && !cur.ends_with('.') {
                        cur.push(' ');
                    }
                    cur.push_str(&s.text);
                }
                j += 1;
            }
            if !cur.is_empty() {
                order.push((cur, desc));
            }
        }
    }
    let uniform = order.iter().all(|(_, d)| *d == order[0].1);
    let sql = if order.is_empty() {
        Some(format!(
            "WHERE (/* sort columns */) > (:last_values)\nORDER BY /* sort columns */\nLIMIT {page}"
        ))
    } else if uniform
        && matches!(
            cx.ctx.dialect,
            ScriptDialect::Postgres | ScriptDialect::MySql { .. } | ScriptDialect::ClickHouse
        )
    {
        let cols: Vec<&str> = order.iter().map(|(c, _)| c.as_str()).collect();
        let params: Vec<String> = cols
            .iter()
            .map(|c| {
                format!(
                    ":last_{}",
                    c.rsplit('.')
                        .next()
                        .unwrap_or(c)
                        .trim_matches(|ch: char| !ch.is_alphanumeric() && ch != '_')
                )
            })
            .collect();
        let cmp = if order[0].1 { "<" } else { ">" };
        Some(format!(
            "WHERE ({}) {cmp} ({})\nORDER BY {}{}\nLIMIT {page}",
            cols.join(", "),
            params.join(", "),
            cols.join(", "),
            if order[0].1 { " DESC" } else { "" }
        ))
    } else {
        None
    };
    if let Some(sql) = sql {
        d.actions.push(action(
            ActionKind::Rewrite,
            "keysetPagination",
            Some(sql),
            true,
            ActionScope::Query,
            None,
        ));
    } else {
        d.actions.push(action(
            ActionKind::Rewrite,
            "keysetPagination",
            None,
            false,
            ActionScope::Query,
            None,
        ));
    }
    d.caveats.push(caveat("keysetNeedsUniqueOrder", false));
    drafts.push(d);
}

fn not_in_subquery(cx: &Cx<'_>, sigs: &[Sig], drafts: &mut Vec<Draft>) {
    for i in 0..sigs.len() {
        let s = &sigs[i];
        if !(s.is_word("not")
            && sigs.get(i + 1).is_some_and(|n| n.is_word("in"))
            && sigs.get(i + 2).is_some_and(|n| n.is_punct('('))
            && sigs.get(i + 3).is_some_and(|n| n.is_word("select")))
        {
            continue;
        }
        let Some(close) = close_of(sigs, i + 2) else {
            continue;
        };
        // Outer operand: the identifier before NOT.
        let Some((oq, ocol)) = i.checked_sub(1).and_then(|p| colref_ending_at(sigs, p)) else {
            continue;
        };
        let outer = match &oq {
            Some(q) => format!("{q}.{ocol}"),
            None => ocol.clone(),
        };
        // Subquery select item: a single column before FROM.
        let Some(from) = (i + 4..close)
            .find(|j| sigs[*j].is_word("from") && sigs[*j].depth == sigs[i + 3].depth)
        else {
            continue;
        };
        let Some((sq, scol)) = colref_ending_at(sigs, from - 1) else {
            continue;
        };
        if from - 1 != i + 4 && !(from - 1 == i + 6 && sigs[i + 5].is_punct('.')) {
            continue; // an expression or several columns
        }
        let sub_col = match &sq {
            Some(q) => format!("{q}.{scol}"),
            None => scol.clone(),
        };
        let where_at = (from + 1..close)
            .find(|j| sigs[*j].is_word("where") && sigs[*j].depth == sigs[from].depth);
        let from_end = where_at.unwrap_or(close);
        let from_text = text_of(
            cx,
            TextRange {
                start: sigs[from + 1].start,
                end: sigs[from_end - 1].end,
            },
        );
        let where_text = where_at.and_then(|w| {
            (w + 1 < close).then(|| {
                text_of(
                    cx,
                    TextRange {
                        start: sigs[w + 1].start,
                        end: sigs[close - 1].end,
                    },
                )
            })
        });
        // Nullability from the catalog, when known.
        let mut nullable: Option<bool> = None;
        for m in cx.ctx.tables.values() {
            for c in &m.columns {
                if c.name.eq_ignore_ascii_case(&scol) {
                    nullable = Some(nullable.unwrap_or(false) || c.nullable);
                }
            }
        }
        if nullable == Some(false) {
            continue;
        }
        let mut sql = format!("NOT EXISTS (SELECT 1 FROM {from_text} WHERE {sub_col} = {outer}");
        if let Some(w) = where_text {
            sql.push_str(&format!(" AND ({w})"));
        }
        sql.push(')');
        let r = range(s, &sigs[close]);
        // The plan already found the per-row subplan: enrich that advice with the text.
        if let Some(existing) = drafts
            .iter_mut()
            .find(|d| d.rule == AdviceRule::PgNotInSubplan)
        {
            existing.statement_range.get_or_insert(r);
            if let Some(a) = existing
                .actions
                .iter_mut()
                .find(|a| a.label_key == "rewriteNotExists")
            {
                a.sql = Some(sql);
            }
            continue;
        }
        let mut d = Draft::new(
            AdviceRule::NotInSubquery,
            Severity::Info,
            if nullable == Some(true) {
                Confidence::Medium
            } else {
                Confidence::Low
            },
        )
        .param("column", ocol);
        d.statement_range = Some(r);
        d.needs_metadata = false;
        d.fingerprint_key = format!("{}|{}", outer.to_lowercase(), sub_col.to_lowercase());
        d.actions.push(action(
            ActionKind::Rewrite,
            "rewriteNotExists",
            Some(sql),
            true,
            ActionScope::Query,
            None,
        ));
        d.caveats.push(caveat("semanticsDiffer", true));
        drafts.push(d);
    }
}
