//! Mini parser for the condition texts of plan nodes (design section 5.1).
//!
//! There is no SQL grammar here: a condition is tokenised with the dialect's own lexer, split on
//! top level `AND`, and each conjunct is classified by its comparison operator and the shape of
//! its operands (column, literal, function call, arithmetic). Anything that does not fit is
//! [`CondKind::Other`]. Nothing in this module panics on malformed input.

use dbstudio_core::ScriptDialect;
use dbstudio_sql::lexer::{TokenKind, tokenize};

use crate::json::{JVal, Js};

/// A column mentioned by a condition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ColRef {
    /// Table or alias in front of the column, as written (unquoted).
    pub qualifier: Option<String>,
    /// Column name (unquoted, original case).
    pub name: String,
    /// `::type` cast applied to the column (lower-cased, e.g. `numeric`, `character varying`).
    pub cast: Option<String>,
}

impl ColRef {
    fn plain(name: &str) -> Self {
        ColRef {
            qualifier: None,
            name: name.to_string(),
            cast: None,
        }
    }

    /// Lower-cased column name, the key used for de-duplication and index matching.
    pub fn key(&self) -> String {
        self.name.to_lowercase()
    }
}

/// What a conjunct says about its column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CondKind {
    /// `col = value`, `col IN (...)`, `col = ANY (...)`, `col IS NULL`.
    Eq,
    /// `<`, `<=`, `>`, `>=`, `BETWEEN`, `LIKE 'abc%'`.
    Range,
    /// `col = other_table.col`.
    Join,
    /// Top level `OR`: `cols` lists the columns of the table.
    Or,
    /// A function, arithmetic or a harmful cast around the column, or `LIKE '%...'`.
    NonSargable,
    /// Not usable for an index (`<>`, `IS NOT NULL`, column to column of the same table...).
    Other,
}

/// One conjunct of a condition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Conjunct {
    pub kind: CondKind,
    /// The column of the table this conjunct is about (first one for `Or` / `NonSargable`).
    pub col: Option<ColRef>,
    /// The column on the other side of a `Join`.
    pub other: Option<ColRef>,
    /// Function or operator that makes the conjunct non sargable (`lower`, `+`, `::numeric`).
    pub func: Option<String>,
    /// Every column of the table in an `Or`.
    pub cols: Vec<ColRef>,
    /// The literal compared with, when there is exactly one (`'abc%'`, `5`).
    pub literal: Option<String>,
    /// Operator text (`=`, `>=`, `like`, `in`...), lower-cased.
    pub op: String,
    /// The function (or cast) is applied directly to the bare column: `lower(email)`.
    pub simple: bool,
    /// First argument of the function when it is a literal (`'day'` of `date_trunc`).
    pub func_arg: Option<String>,
}

impl Conjunct {
    fn new(kind: CondKind) -> Self {
        Conjunct {
            kind,
            col: None,
            other: None,
            func: None,
            cols: Vec::new(),
            literal: None,
            op: String::new(),
            simple: false,
            func_arg: None,
        }
    }
}

/// How a cast on a column affects index use.
pub(crate) fn cast_is_harmful(cast: &str, column: &str, column_type: Option<&str>) -> bool {
    let cast = cast.trim().to_lowercase();
    let textual = |t: &str| {
        t.starts_with("text")
            || t.starts_with("varchar")
            || t.starts_with("character")
            || t.starts_with("bpchar")
            || t.starts_with("name")
            || t.starts_with("char")
    };
    let integral = |t: &str| {
        matches!(
            t,
            "int" | "integer" | "int2" | "int4" | "int8" | "smallint" | "bigint"
        )
    };
    if let Some(ty) = column_type {
        let ty = ty.to_lowercase();
        // The same type family on both sides is harmless (`varchar` column cast to text).
        if textual(&cast) && textual(&ty) {
            return false;
        }
        if integral(&cast) && (integral(&ty) || ty.starts_with("int") || ty.starts_with("serial")) {
            return false;
        }
        if cast.starts_with(&ty) || ty.starts_with(&cast) {
            return false;
        }
        return true;
    }
    // No metadata: only the casts that are known to hurt, and `text` over an id-like column.
    if integral(&cast) {
        return false;
    }
    if textual(&cast) {
        let lower = column.to_lowercase();
        return lower == "id" || lower.ends_with("_id");
    }
    true
}

// ------------------------------------------------------------------------------------------
// Tokens
// ------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Word(String),
    QIdent(String),
    Num(String),
    Str(String),
    Op(String),
    LP,
    RP,
    Comma,
    Dot,
    LB,
    RB,
    Other(char),
}

fn is_op_char(c: char) -> bool {
    matches!(
        c,
        '<' | '>'
            | '='
            | '!'
            | '~'
            | '*'
            | '+'
            | '-'
            | '/'
            | '%'
            | '|'
            | '&'
            | '^'
            | '@'
            | '#'
            | '?'
    )
}

fn unquote(d: ScriptDialect, raw: &str) -> String {
    let chars: Vec<char> = raw.chars().collect();
    if chars.len() < 2 {
        return raw.to_string();
    }
    let inner: String = chars[1..chars.len() - 1].iter().collect();
    match d {
        ScriptDialect::MySql { .. } | ScriptDialect::ClickHouse => inner.replace("``", "`"),
        ScriptDialect::MsSql => inner.replace("]]", "]"),
        _ => inner.replace("\"\"", "\""),
    }
}

fn lex(d: ScriptDialect, text: &str) -> Vec<Tok> {
    let chars: Vec<char> = text.chars().collect();
    let slice = |s: u32, e: u32| -> String {
        chars
            .get(s as usize..e as usize)
            .map(|c| c.iter().collect())
            .unwrap_or_default()
    };
    let mut out: Vec<Tok> = Vec::new();
    // End offset of the previous punctuation token, to merge adjacent operator characters.
    let mut last_op_end: Option<u32> = None;
    for t in tokenize(d, text) {
        if t.kind.is_trivia() {
            last_op_end = None;
            continue;
        }
        let s = slice(t.range.start, t.range.end);
        match t.kind {
            TokenKind::Word => {
                out.push(Tok::Word(s));
                last_op_end = None;
            }
            TokenKind::QuotedIdent => {
                out.push(Tok::QIdent(unquote(d, &s)));
                last_op_end = None;
            }
            TokenKind::Number => {
                out.push(Tok::Num(s));
                last_op_end = None;
            }
            TokenKind::String => {
                out.push(Tok::Str(s));
                last_op_end = None;
            }
            TokenKind::Punct => {
                if s == "::" {
                    out.push(Tok::Op(s));
                    last_op_end = None;
                    continue;
                }
                let c = s.chars().next().unwrap_or(' ');
                match c {
                    '(' => out.push(Tok::LP),
                    ')' => out.push(Tok::RP),
                    ',' => out.push(Tok::Comma),
                    '.' => out.push(Tok::Dot),
                    '[' => out.push(Tok::LB),
                    ']' => out.push(Tok::RB),
                    c if is_op_char(c) => {
                        if last_op_end == Some(t.range.start) {
                            if let Some(Tok::Op(prev)) = out.last_mut() {
                                prev.push(c);
                            }
                        } else {
                            out.push(Tok::Op(c.to_string()));
                        }
                        last_op_end = Some(t.range.end);
                        continue;
                    }
                    c => out.push(Tok::Other(c)),
                }
                last_op_end = None;
            }
            _ => {
                last_op_end = None;
            }
        }
    }
    out
}

fn is_word(t: &Tok, w: &str) -> bool {
    matches!(t, Tok::Word(s) if s.eq_ignore_ascii_case(w))
}

/// Index of the token matching the opening parenthesis at `open` (`toks[open]` is `LP`).
fn matching(toks: &[Tok], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (i, t) in toks.iter().enumerate().skip(open) {
        match t {
            Tok::LP => depth += 1,
            Tok::RP => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Remove parentheses that wrap the whole slice.
fn strip_parens(mut toks: &[Tok]) -> &[Tok] {
    while toks.len() >= 2 && toks[0] == Tok::LP && matching(toks, 0) == Some(toks.len() - 1) {
        toks = &toks[1..toks.len() - 1];
    }
    toks
}

/// Positions of top level tokens satisfying `f` (parentheses and brackets are skipped over).
fn top_level(toks: &[Tok], f: impl Fn(&Tok) -> bool) -> Vec<usize> {
    let mut depth = 0i32;
    let mut out = Vec::new();
    for (i, t) in toks.iter().enumerate() {
        match t {
            Tok::LP | Tok::LB => depth += 1,
            Tok::RP | Tok::RB => depth = (depth - 1).max(0),
            _ if depth == 0 && f(t) => out.push(i),
            _ => {}
        }
    }
    out
}

/// Split on top level `AND` (a `BETWEEN a AND b` keeps its `AND`).
fn split_and(toks: &[Tok]) -> Vec<&[Tok]> {
    let toks = strip_parens(toks);
    let mut depth = 0i32;
    let mut between = 0usize;
    let mut cuts = Vec::new();
    for (i, t) in toks.iter().enumerate() {
        match t {
            Tok::LP | Tok::LB => depth += 1,
            Tok::RP | Tok::RB => depth = (depth - 1).max(0),
            Tok::Word(w) if depth == 0 => {
                if w.eq_ignore_ascii_case("between") {
                    between += 1;
                } else if w.eq_ignore_ascii_case("and") {
                    if between > 0 {
                        between -= 1;
                    } else {
                        cuts.push(i);
                    }
                }
            }
            _ => {}
        }
    }
    if cuts.is_empty() {
        return vec![toks];
    }
    let mut parts = Vec::new();
    let mut start = 0;
    for c in cuts {
        parts.extend(split_and(&toks[start..c]));
        start = c + 1;
    }
    parts.extend(split_and(&toks[start..]));
    parts
}

// ------------------------------------------------------------------------------------------
// Operands
// ------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Operand {
    Col(ColRef),
    /// Literal or parameter; text kept for `LIKE` pattern checks.
    Lit(String),
    /// Function call over columns.
    Func {
        name: String,
        cols: Vec<ColRef>,
        /// One argument, which is a bare column.
        simple: bool,
        /// First argument when it is a literal.
        first_lit: Option<String>,
    },
    /// Arithmetic / concatenation over columns.
    Expr {
        op: String,
        cols: Vec<ColRef>,
    },
    Opaque,
}

fn cols_of(o: &Operand) -> Vec<ColRef> {
    match o {
        Operand::Col(c) => vec![c.clone()],
        Operand::Func { cols, .. } | Operand::Expr { cols, .. } => cols.clone(),
        _ => Vec::new(),
    }
}

/// Consume `::type` casts at `toks[i..]`; returns the last cast type and the next index.
fn casts(toks: &[Tok], mut i: usize) -> (Option<String>, usize) {
    let mut cast = None;
    while matches!(toks.get(i), Some(Tok::Op(o)) if o == "::") {
        i += 1;
        let mut words: Vec<String> = Vec::new();
        while let Some(Tok::Word(w)) = toks.get(i) {
            words.push(w.to_lowercase());
            i += 1;
        }
        if let Some(Tok::QIdent(q)) = toks.get(i) {
            words.push(q.to_lowercase());
            i += 1;
        }
        // `pg_catalog.text`: keep the last name only.
        while matches!(toks.get(i), Some(Tok::Dot)) {
            i += 1;
            if let Some(Tok::Word(w)) = toks.get(i) {
                words.clear();
                words.push(w.to_lowercase());
                i += 1;
            }
        }
        if matches!(toks.get(i), Some(Tok::LP)) {
            i = matching(toks, i).map_or(toks.len(), |m| m + 1);
        }
        while matches!(toks.get(i), Some(Tok::LB)) && matches!(toks.get(i + 1), Some(Tok::RB)) {
            i += 2;
        }
        if !words.is_empty() {
            cast = Some(words.join(" "));
        }
    }
    (cast, i)
}

fn parse_atom(toks: &[Tok], i: usize) -> (Operand, usize) {
    match toks.get(i) {
        None => (Operand::Opaque, i),
        Some(Tok::LP) => {
            let close = matching(toks, i).unwrap_or(toks.len().saturating_sub(1));
            let inner = &toks[(i + 1).min(close)..close];
            let mut op = parse_expr(inner);
            let (cast, next) = casts(toks, close + 1);
            if let (Some(cast), Operand::Col(c)) = (cast, &mut op) {
                c.cast = Some(cast);
            }
            (op, next)
        }
        Some(Tok::Num(n)) => {
            let (_, next) = casts(toks, i + 1);
            (Operand::Lit(n.clone()), next)
        }
        Some(Tok::Str(s)) => {
            let (_, next) = casts(toks, i + 1);
            (Operand::Lit(s.clone()), next)
        }
        Some(Tok::Op(o)) if (o == "-" || o == "+") && i + 1 < toks.len() => {
            let (inner, next) = parse_atom(toks, i + 1);
            match inner {
                Operand::Lit(t) => (Operand::Lit(format!("{o}{t}")), next),
                other => (other, next),
            }
        }
        Some(Tok::Other(':')) | Some(Tok::Op(_)) => {
            // `:name`, `?`, `@p` style parameters: swallow the following name.
            let mut j = i + 1;
            if let Some(Tok::Word(_) | Tok::Num(_)) = toks.get(j) {
                j += 1;
            }
            (Operand::Lit(String::new()), j)
        }
        Some(Tok::Word(w)) => {
            let lw = w.to_lowercase();
            // Typed literal `DATE '2024-01-01'`, `INTERVAL '1 day'`.
            if let Some(Tok::Str(s)) = toks.get(i + 1) {
                if !matches!(
                    lw.as_str(),
                    "and" | "or" | "not" | "like" | "ilike" | "in" | "is"
                ) {
                    return (Operand::Lit(s.clone()), i + 2);
                }
            }
            match lw.as_str() {
                "null" | "true" | "false" | "default" | "current_date" | "current_timestamp"
                | "current_time" | "localtimestamp" | "sysdate" | "systimestamp" => {
                    let (_, next) = casts(toks, i + 1);
                    return (Operand::Lit(w.clone()), next);
                }
                "array" => {
                    let mut j = i + 1;
                    if matches!(toks.get(j), Some(Tok::LB)) {
                        let mut depth = 0;
                        while let Some(t) = toks.get(j) {
                            match t {
                                Tok::LB => depth += 1,
                                Tok::RB => {
                                    depth -= 1;
                                    if depth == 0 {
                                        j += 1;
                                        break;
                                    }
                                }
                                _ => {}
                            }
                            j += 1;
                        }
                    } else if matches!(toks.get(j), Some(Tok::LP)) {
                        j = matching(toks, j).map_or(toks.len(), |m| m + 1);
                    }
                    let (_, next) = casts(toks, j);
                    return (Operand::Lit("array".into()), next);
                }
                "any" | "all" | "some" if matches!(toks.get(i + 1), Some(Tok::LP)) => {
                    let close = matching(toks, i + 1).unwrap_or(toks.len().saturating_sub(1));
                    return (Operand::Lit("any".into()), close + 1);
                }
                "case" => {
                    // Opaque until the matching END.
                    let mut depth = 0;
                    let mut j = i;
                    while let Some(t) = toks.get(j) {
                        if is_word(t, "case") {
                            depth += 1;
                        } else if is_word(t, "end") {
                            depth -= 1;
                            if depth == 0 {
                                j += 1;
                                break;
                            }
                        }
                        j += 1;
                    }
                    return (Operand::Opaque, j);
                }
                _ => {}
            }
            if matches!(toks.get(i + 1), Some(Tok::LP)) {
                let close = matching(toks, i + 1).unwrap_or(toks.len().saturating_sub(1));
                let inner = &toks[(i + 2).min(close)..close];
                let mut cols = Vec::new();
                let mut simple = false;
                let mut first_lit = None;
                let mut depth_args: Vec<&[Tok]> = Vec::new();
                let mut start = 0;
                for p in top_level(inner, |t| *t == Tok::Comma) {
                    depth_args.push(&inner[start..p]);
                    start = p + 1;
                }
                depth_args.push(&inner[start.min(inner.len())..]);
                if let Some(Tok::Str(t)) = depth_args.first().and_then(|a| a.first()) {
                    first_lit = Some(t.clone());
                }
                let nargs = depth_args.len();
                for a in depth_args {
                    // `CAST(col AS type)`: drop the `AS type` tail.
                    let a = match top_level(a, |t| is_word(t, "as")).first() {
                        Some(p) => &a[..*p],
                        None => a,
                    };
                    let operand = parse_expr(a);
                    if nargs == 1 && matches!(operand, Operand::Col(_)) {
                        simple = true;
                    }
                    cols.extend(cols_of(&operand));
                }
                let (_, next) = casts(toks, close + 1);
                return (
                    Operand::Func {
                        name: lw,
                        cols,
                        simple,
                        first_lit,
                    },
                    next,
                );
            }
            parse_colref(toks, i)
        }
        Some(Tok::QIdent(_)) => parse_colref(toks, i),
        Some(_) => (Operand::Opaque, i + 1),
    }
}

/// `a`, `t.a`, `s.t.a`, `[db].[dbo].[T].[c]` followed by optional casts.
fn parse_colref(toks: &[Tok], mut i: usize) -> (Operand, usize) {
    let mut parts: Vec<String> = Vec::new();
    loop {
        match toks.get(i) {
            Some(Tok::Word(w)) | Some(Tok::QIdent(w)) => {
                parts.push(w.clone());
                i += 1;
            }
            Some(Tok::Op(o)) if o == "*" => {
                parts.push("*".into());
                i += 1;
            }
            _ => break,
        }
        if matches!(toks.get(i), Some(Tok::Dot)) {
            i += 1;
        } else {
            break;
        }
    }
    let Some(name) = parts.pop() else {
        return (Operand::Opaque, i + 1);
    };
    if name == "*" {
        return (Operand::Opaque, i);
    }
    let qualifier = parts.pop();
    let (cast, next) = casts(toks, i);
    (
        Operand::Col(ColRef {
            qualifier,
            name,
            cast,
        }),
        next,
    )
}

const ARITH: &[&str] = &["+", "-", "*", "/", "%", "||", "&", "|", "^"];

/// `operand (arith operand)*`.
fn parse_expr(toks: &[Tok]) -> Operand {
    let toks = strip_parens(toks);
    let (first, mut i) = parse_atom(toks, 0);
    let mut cols = cols_of(&first);
    let mut op_seen: Option<String> = None;
    let mut guard = 0;
    while i < toks.len() && guard < toks.len() + 2 {
        guard += 1;
        match toks.get(i) {
            Some(Tok::Op(o)) if ARITH.contains(&o.as_str()) => {
                op_seen.get_or_insert_with(|| o.clone());
                let (next, j) = parse_atom(toks, i + 1);
                cols.extend(cols_of(&next));
                i = j.max(i + 1);
            }
            _ => break,
        }
    }
    if i < toks.len() && op_seen.is_none() && matches!(first, Operand::Col(_) | Operand::Lit(_)) {
        // Trailing tokens we do not understand (`COLLATE x`, `AT TIME ZONE ..`): not a plain operand.
        return match first {
            Operand::Col(c) => Operand::Func {
                name: "expr".into(),
                cols: vec![c],
                simple: false,
                first_lit: None,
            },
            _ => Operand::Opaque,
        };
    }
    match op_seen {
        Some(op) => Operand::Expr { op, cols },
        None => first,
    }
}

// ------------------------------------------------------------------------------------------
// Conjuncts
// ------------------------------------------------------------------------------------------

fn is_own(c: &ColRef, own: &[String]) -> bool {
    match &c.qualifier {
        None => true,
        Some(q) => own.iter().any(|o| o.eq_ignore_ascii_case(q)),
    }
}

fn is_comparison(t: &Tok) -> Option<String> {
    match t {
        Tok::Op(o) => match o.as_str() {
            "=" | "<>" | "!=" | "<" | "<=" | ">" | ">=" | "~~" | "!~~" | "~~*" | "!~~*" | "~"
            | "!~" | "~*" | "!~*" => Some(o.clone()),
            _ => None,
        },
        Tok::Word(w) => {
            let l = w.to_ascii_lowercase();
            matches!(
                l.as_str(),
                "in" | "like" | "ilike" | "between" | "is" | "not" | "regexp" | "rlike" | "similar"
            )
            .then_some(l)
        }
        _ => None,
    }
}

fn like_kind(pattern: &str) -> CondKind {
    // First character of the literal content (after any prefix letters and the quote).
    let content = pattern.split_once('\'').map_or(pattern, |(_, r)| r);
    match content.chars().next() {
        Some('%') | Some('_') => CondKind::NonSargable,
        Some('\'') | None => CondKind::Other,
        Some(_) => CondKind::Range,
    }
}

/// Classify one conjunct (`own`: names the table of the node goes by: alias and relation name).
fn classify(toks: &[Tok], own: &[String]) -> Conjunct {
    let toks = strip_parens(toks);
    if toks.is_empty() {
        return Conjunct::new(CondKind::Other);
    }
    // OR at the top level.
    if !top_level(toks, |t| is_word(t, "or")).is_empty() {
        let mut c = Conjunct::new(CondKind::Or);
        let mut start = 0;
        let mut parts: Vec<&[Tok]> = Vec::new();
        for p in top_level(toks, |t| is_word(t, "or")) {
            parts.push(&toks[start..p]);
            start = p + 1;
        }
        parts.push(&toks[start..]);
        for part in parts {
            for sub in split_and(part) {
                let k = classify(sub, own);
                if let Some(col) = k.col {
                    if is_own(&col, own) && !c.cols.iter().any(|x| x.key() == col.key()) {
                        c.cols.push(col);
                    }
                }
                for col in k.cols {
                    if !c.cols.iter().any(|x| x.key() == col.key()) {
                        c.cols.push(col);
                    }
                }
            }
        }
        c.col = c.cols.first().cloned();
        c.op = "or".into();
        return c;
    }
    if is_word(&toks[0], "not") || is_word(&toks[0], "exists") {
        return Conjunct::new(CondKind::Other);
    }
    // Find the comparison operator (skipping operands that contain operators inside parens).
    let Some(pos) = top_level(toks, |t| is_comparison(t).is_some())
        .into_iter()
        .find(|p| *p > 0)
    else {
        return Conjunct::new(CondKind::Other);
    };
    let op = is_comparison(&toks[pos]).unwrap_or_default();
    let mut right_start = pos + 1;
    if op == "not" {
        // `col NOT IN (...)`, `col NOT LIKE 'x%'`, `col NOT BETWEEN`.
        let mut c = Conjunct::new(CondKind::Other);
        c.op = "not".into();
        return c;
    }
    let left = parse_expr(&toks[..pos]);
    if op == "is" {
        // `col IS NULL` is an equality; `IS NOT NULL`, `IS DISTINCT FROM` are not.
        let rest = &toks[right_start.min(toks.len())..];
        let mut c = Conjunct::new(CondKind::Other);
        c.op = "is".into();
        if rest.len() == 1 && is_word(&rest[0], "null") {
            if let Operand::Col(col) = &left {
                if is_own(col, own) {
                    c.kind = CondKind::Eq;
                    c.col = Some(col.clone());
                    c.op = "is null".into();
                }
            }
        }
        return c;
    }
    if op == "between" {
        right_start = pos + 1;
    }
    let right_toks = &toks[right_start.min(toks.len())..];
    let right = if op == "in" {
        Operand::Lit("list".into())
    } else if op == "between" {
        Operand::Lit("range".into())
    } else {
        parse_expr(right_toks)
    };
    // `5 < col`: flip so the column is on the left.
    let (left, right, op) = match (&left, &right) {
        (Operand::Lit(_), Operand::Col(_) | Operand::Func { .. } | Operand::Expr { .. }) => {
            let flipped = match op.as_str() {
                "<" => ">".to_string(),
                "<=" => ">=".to_string(),
                ">" => "<".to_string(),
                ">=" => "<=".to_string(),
                o => o.to_string(),
            };
            (right, left, flipped)
        }
        _ => (left, right, op),
    };
    let mut c = Conjunct::new(CondKind::Other);
    c.op = op.clone();
    if let Operand::Lit(t) = &right {
        if !t.is_empty() {
            c.literal = Some(t.clone());
        }
    }
    match &left {
        Operand::Col(col) => {
            if !is_own(col, own) {
                // The column belongs to another table; an own column on the right makes a join.
                if let Operand::Col(r) = &right {
                    if is_own(r, own) && r.qualifier.is_some() && op == "=" {
                        c.kind = CondKind::Join;
                        c.col = Some(r.clone());
                        c.other = Some(col.clone());
                    }
                }
                return c;
            }
            let harmful = col
                .cast
                .as_deref()
                .is_some_and(|cast| cast_is_harmful(cast, &col.name, None));
            if harmful {
                c.kind = CondKind::NonSargable;
                c.func = Some(format!("::{}", col.cast.clone().unwrap_or_default()));
                c.col = Some(col.clone());
                c.simple = true;
                return c;
            }
            c.col = Some(col.clone());
            match &right {
                Operand::Col(r) => {
                    if !is_own(r, own) && matches!(op.as_str(), "=" | "<" | "<=" | ">" | ">=") {
                        c.kind = if op == "=" {
                            CondKind::Join
                        } else {
                            CondKind::Other
                        };
                        if c.kind == CondKind::Join {
                            c.other = Some(r.clone());
                        }
                    } else {
                        // Own column against an own column: not indexable.
                        c.col = None;
                    }
                }
                _ => {
                    c.kind = match op.as_str() {
                        "=" | "in" => CondKind::Eq,
                        "<" | "<=" | ">" | ">=" | "between" => CondKind::Range,
                        "like" | "~~" => match &right {
                            Operand::Lit(t) => like_kind(t),
                            _ => CondKind::Other,
                        },
                        _ => CondKind::Other,
                    };
                    if c.kind == CondKind::NonSargable {
                        c.func = Some("like".into());
                    }
                }
            }
        }
        Operand::Func {
            name,
            cols,
            simple,
            first_lit,
        } => {
            if let Some(col) = cols.iter().find(|c| is_own(c, own)) {
                c.kind = CondKind::NonSargable;
                c.func = Some(name.clone());
                c.col = Some(col.clone());
                c.simple = *simple;
                c.func_arg = first_lit.clone();
            }
        }
        Operand::Expr { op: eop, cols } => {
            if let Some(col) = cols.iter().find(|c| is_own(c, own)) {
                c.kind = CondKind::NonSargable;
                c.func = Some(eop.clone());
                c.col = Some(col.clone());
            }
        }
        _ => {}
    }
    c
}

/// Parse a SQL condition into its top level conjuncts. `own` lists the names the node's table
/// is known by (alias and relation name); columns qualified by anything else belong to other
/// tables of the join.
pub(crate) fn parse_conjuncts(d: ScriptDialect, text: &str, own: &[String]) -> Vec<Conjunct> {
    let toks = lex(d, text);
    split_and(&toks)
        .into_iter()
        .map(|part| classify(part, own))
        .collect()
}

/// One element of a `Sort Key`: `created_at DESC`, `o.id`, `lower((email)::text) DESC NULLS LAST`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SortKey {
    pub col: ColRef,
    pub desc: bool,
    /// The key is an expression, not a bare column.
    pub computed: bool,
}

/// Parse a sort / group key element.
pub(crate) fn parse_sort_key(d: ScriptDialect, text: &str) -> Option<SortKey> {
    let toks = lex(d, text);
    let mut desc = false;
    let mut end = toks.len();
    // Trailing `ASC|DESC [NULLS FIRST|LAST]`.
    while end > 0 {
        match &toks[end - 1] {
            Tok::Word(w) if w.eq_ignore_ascii_case("desc") => {
                desc = true;
                end -= 1;
            }
            Tok::Word(w) if w.eq_ignore_ascii_case("asc") => end -= 1,
            Tok::Word(w) if w.eq_ignore_ascii_case("first") || w.eq_ignore_ascii_case("last") => {
                end -= 1;
                if end > 0 && is_word(&toks[end - 1], "nulls") {
                    end -= 1;
                }
            }
            _ => break,
        }
    }
    match parse_expr(&toks[..end]) {
        Operand::Col(col) => Some(SortKey {
            col,
            desc,
            computed: false,
        }),
        Operand::Func { cols, .. } | Operand::Expr { cols, .. } => {
            cols.first().cloned().map(|col| SortKey {
                col,
                desc,
                computed: true,
            })
        }
        _ => None,
    }
}

// ------------------------------------------------------------------------------------------
// MongoDB filters
// ------------------------------------------------------------------------------------------

/// Parse a Mongo filter document (canonical or relaxed JSON) into conjuncts.
pub(crate) fn parse_mongo_filter(text: &str) -> Vec<Conjunct> {
    let Ok(doc) = crate::json::parse(text) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    mongo_doc(&doc, &mut out, 0);
    out
}

fn mongo_doc(doc: &Js, out: &mut Vec<Conjunct>, depth: u32) {
    if depth > 16 {
        return;
    }
    for (key, value) in doc.members() {
        if key == "$and" {
            for item in value.items() {
                mongo_doc(item, out, depth + 1);
            }
        } else if key.starts_with('$') {
            // `$or`, `$nor`, `$expr`, `$text`...: not usable for a prefix index.
            let mut c = Conjunct::new(CondKind::Other);
            c.op = key.clone();
            out.push(c);
        } else {
            mongo_field(key, value, out);
        }
    }
}

fn mongo_field(field: &str, value: &Js, out: &mut Vec<Conjunct>) {
    let col = ColRef::plain(field);
    let ops: Vec<&(String, Js)> = match &value.val {
        JVal::Obj(m) if m.iter().any(|(k, _)| k.starts_with('$')) && !is_ejson_scalar(value) => {
            m.iter().collect()
        }
        _ => {
            let mut c = Conjunct::new(CondKind::Eq);
            c.col = Some(col);
            c.op = "$eq".into();
            out.push(c);
            return;
        }
    };
    for (op, v) in ops {
        let mut c = Conjunct::new(CondKind::Other);
        c.col = Some(col.clone());
        c.op = op.clone();
        match op.as_str() {
            "$eq" | "$in" => c.kind = CondKind::Eq,
            "$gt" | "$gte" | "$lt" | "$lte" => c.kind = CondKind::Range,
            "$regex" => {
                let pattern = v.as_str().unwrap_or("");
                c.literal = Some(pattern.to_string());
                if pattern.starts_with('^') && !pattern.starts_with("^.*") {
                    c.kind = CondKind::Range;
                } else {
                    c.kind = CondKind::NonSargable;
                    c.func = Some("regex".into());
                }
            }
            "$options" => continue,
            _ => {}
        }
        out.push(c);
    }
}

/// `{"$numberLong": "5"}` and friends are scalars, not operator documents.
fn is_ejson_scalar(v: &Js) -> bool {
    matches!(&v.val, JVal::Obj(m) if m.len() == 1 && matches!(m[0].0.as_str(),
        "$numberInt" | "$numberLong" | "$numberDouble" | "$numberDecimal" | "$oid" | "$date"
        | "$uuid" | "$binary" | "$timestamp"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const PG: ScriptDialect = ScriptDialect::Postgres;
    const MSSQL: ScriptDialect = ScriptDialect::MsSql;
    const ORACLE: ScriptDialect = ScriptDialect::Oracle;
    const MYSQL: ScriptDialect = ScriptDialect::MySql {
        mariadb: false,
        backslash_escapes: true,
        ansi_quotes: false,
    };

    fn own(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn one(d: ScriptDialect, text: &str, names: &[&str]) -> Conjunct {
        let v = parse_conjuncts(d, text, &own(names));
        assert_eq!(v.len(), 1, "{text}: {v:?}");
        v.into_iter().next().unwrap()
    }

    fn col(c: &Conjunct) -> String {
        c.col.as_ref().map(|c| c.name.clone()).unwrap_or_default()
    }

    // ---- PostgreSQL ----------------------------------------------------------------------
    #[test]
    fn pg_equality_with_harmless_text_cast() {
        let c = one(PG, "(status = 'void'::text)", &["orders"]);
        assert_eq!((c.kind, col(&c).as_str()), (CondKind::Eq, "status"));
    }

    #[test]
    fn pg_varchar_column_cast_to_text_is_harmless() {
        let c = one(PG, "((status)::text = 'void'::text)", &["orders"]);
        assert_eq!((c.kind, col(&c).as_str()), (CondKind::Eq, "status"));
    }

    #[test]
    fn pg_text_cast_over_id_column_is_harmful() {
        let c = one(PG, "((customer_id)::text = '5'::text)", &["orders"]);
        assert_eq!(c.kind, CondKind::NonSargable);
        assert_eq!(c.func.as_deref(), Some("::text"));
    }

    #[test]
    fn pg_numeric_cast_is_harmful() {
        let c = one(PG, "((qty)::numeric = 5.5)", &["items"]);
        assert_eq!((c.kind, col(&c).as_str()), (CondKind::NonSargable, "qty"));
    }

    #[test]
    fn pg_integer_cast_is_harmless() {
        let c = one(PG, "((qty)::bigint = 5)", &["items"]);
        assert_eq!(c.kind, CondKind::Eq);
    }

    #[test]
    fn pg_date_cast_is_harmful() {
        let c = one(PG, "((created_at)::date = '2024-05-01'::date)", &["orders"]);
        assert_eq!(c.kind, CondKind::NonSargable);
        assert_eq!(c.func.as_deref(), Some("::date"));
    }

    #[test]
    fn pg_lower_function() {
        let c = one(PG, "(lower((email)::text) = 'a@b.c'::text)", &["users"]);
        assert_eq!((c.kind, col(&c).as_str()), (CondKind::NonSargable, "email"));
        assert_eq!(c.func.as_deref(), Some("lower"));
    }

    #[test]
    fn pg_date_trunc_function() {
        let c = one(
            PG,
            "(date_trunc('day'::text, created_at) = '2024-05-01 00:00:00'::timestamp without time zone)",
            &["orders"],
        );
        assert_eq!(
            (c.kind, col(&c).as_str()),
            (CondKind::NonSargable, "created_at")
        );
        assert_eq!(c.func.as_deref(), Some("date_trunc"));
    }

    #[test]
    fn pg_abs_function() {
        let c = one(PG, "(abs(balance) > 100)", &["accounts"]);
        assert_eq!(
            (c.kind, c.func.as_deref()),
            (CondKind::NonSargable, Some("abs"))
        );
    }

    #[test]
    fn pg_and_split() {
        let v = parse_conjuncts(
            PG,
            "((status = 'a'::text) AND (total > 10) AND (customer_id = 7))",
            &own(&["orders"]),
        );
        let kinds: Vec<_> = v.iter().map(|c| (c.kind, col(c))).collect();
        assert_eq!(
            kinds,
            vec![
                (CondKind::Eq, "status".to_string()),
                (CondKind::Range, "total".to_string()),
                (CondKind::Eq, "customer_id".to_string())
            ]
        );
    }

    #[test]
    fn pg_between_keeps_its_and() {
        let v = parse_conjuncts(
            PG,
            "((created_at >= '2024-01-01'::date) AND (created_at <= '2024-02-01'::date) AND (x BETWEEN 1 AND 5))",
            &own(&["t"]),
        );
        assert_eq!(v.len(), 3);
        assert_eq!(v[2].kind, CondKind::Range);
        assert_eq!(col(&v[2]), "x");
    }

    #[test]
    fn pg_any_array() {
        let c = one(PG, "(status = ANY ('{a,b}'::text[]))", &["orders"]);
        assert_eq!((c.kind, col(&c).as_str()), (CondKind::Eq, "status"));
    }

    #[test]
    fn pg_any_array_constructor() {
        let c = one(PG, "(id = ANY (ARRAY[1, 2, 3]))", &["orders"]);
        assert_eq!(c.kind, CondKind::Eq);
    }

    #[test]
    fn pg_in_list() {
        let c = one(PG, "(status IN ('a', 'b'))", &["orders"]);
        assert_eq!(c.kind, CondKind::Eq);
    }

    #[test]
    fn pg_is_null_is_equality_not_null_is_not() {
        assert_eq!(one(PG, "(deleted_at IS NULL)", &["t"]).kind, CondKind::Eq);
        assert_eq!(
            one(PG, "(deleted_at IS NOT NULL)", &["t"]).kind,
            CondKind::Other
        );
    }

    #[test]
    fn pg_like_prefix_is_range_leading_wildcard_is_not() {
        let c = one(PG, "((name)::text ~~ 'abc%'::text)", &["t"]);
        assert_eq!((c.kind, col(&c).as_str()), (CondKind::Range, "name"));
        let c = one(PG, "((name)::text ~~ '%abc'::text)", &["t"]);
        assert_eq!(c.kind, CondKind::NonSargable);
        let c = one(PG, "(name ~~ '_bc%')", &["t"]);
        assert_eq!(c.kind, CondKind::NonSargable);
    }

    #[test]
    fn pg_not_equal_is_other() {
        assert_eq!(
            one(PG, "(status <> 'x'::text)", &["t"]).kind,
            CondKind::Other
        );
        assert_eq!(one(PG, "(status != 'x')", &["t"]).kind, CondKind::Other);
    }

    #[test]
    fn pg_join_condition() {
        let c = one(PG, "(o.customer_id = c.id)", &["c", "customers"]);
        assert_eq!(c.kind, CondKind::Join);
        assert_eq!(col(&c), "id");
        assert_eq!(c.other.as_ref().unwrap().name, "customer_id");
        let c = one(PG, "(o.customer_id = c.id)", &["o", "orders"]);
        assert_eq!(c.kind, CondKind::Join);
        assert_eq!(col(&c), "customer_id");
    }

    #[test]
    fn pg_index_cond_unqualified_against_outer_alias() {
        let c = one(PG, "(customer_id = o.id)", &["i", "items"]);
        assert_eq!(c.kind, CondKind::Join);
        assert_eq!(col(&c), "customer_id");
    }

    #[test]
    fn pg_column_against_own_column_is_other() {
        let c = one(PG, "(qty > reserved)", &["items"]);
        assert_eq!(c.kind, CondKind::Other);
        assert!(c.col.is_none());
        let c = one(PG, "(i.qty = i.reserved)", &["i"]);
        assert_eq!(c.kind, CondKind::Other);
    }

    #[test]
    fn pg_or_collects_columns() {
        let c = one(PG, "((a = 1) OR (b = 2) OR (a = 3))", &["t"]);
        assert_eq!(c.kind, CondKind::Or);
        let names: Vec<_> = c.cols.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn pg_nested_or_inside_and() {
        let v = parse_conjuncts(PG, "((x = 1) AND ((a = 1) OR (b = 2)))", &own(&["t"]));
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].kind, CondKind::Eq);
        assert_eq!(v[1].kind, CondKind::Or);
    }

    #[test]
    fn pg_and_inside_string_is_not_a_split() {
        let c = one(PG, "(note = 'a AND b'::text)", &["t"]);
        assert_eq!((c.kind, col(&c).as_str()), (CondKind::Eq, "note"));
    }

    #[test]
    fn pg_or_inside_string_is_not_an_or() {
        let c = one(PG, "(note = 'x OR y')", &["t"]);
        assert_eq!(c.kind, CondKind::Eq);
    }

    #[test]
    fn pg_arithmetic_on_column() {
        let c = one(PG, "((qty + 1) = 5)", &["t"]);
        assert_eq!(
            (c.kind, c.func.as_deref()),
            (CondKind::NonSargable, Some("+"))
        );
        let c = one(PG, "((a || b) = 'xy')", &["t"]);
        assert_eq!(c.kind, CondKind::NonSargable);
    }

    #[test]
    fn pg_literal_on_the_left_is_flipped() {
        let c = one(PG, "(5 < qty)", &["t"]);
        assert_eq!((c.kind, c.op.as_str()), (CondKind::Range, ">"));
        assert_eq!(col(&c), "qty");
    }

    #[test]
    fn pg_param_and_subplan_values() {
        assert_eq!(one(PG, "(id = $1)", &["t"]).kind, CondKind::Eq);
        assert_eq!(
            one(PG, "(id = (InitPlan 1).col1)", &["t"]).kind,
            CondKind::Eq
        );
        assert_eq!(one(PG, "(id = (SubPlan 1))", &["t"]).kind, CondKind::Eq);
    }

    #[test]
    fn pg_typed_literal() {
        let c = one(PG, "(created_at >= '2024-01-01'::date)", &["t"]);
        assert_eq!(c.kind, CondKind::Range);
        let c = one(PG, "(created_at > (now() - '7 days'::interval))", &["t"]);
        assert_eq!(c.kind, CondKind::Range);
    }

    #[test]
    fn pg_quoted_identifiers() {
        let c = one(PG, "(\"Order Id\" = 5)", &["t"]);
        assert_eq!(col(&c), "Order Id");
        let c = one(PG, "(\"T\".\"Col\" = 5)", &["T"]);
        assert_eq!(col(&c), "Col");
    }

    #[test]
    fn pg_not_exists_and_not_are_other() {
        assert_eq!(one(PG, "(NOT (SubPlan 1))", &["t"]).kind, CondKind::Other);
        assert_eq!(one(PG, "(NOT (a = 1))", &["t"]).kind, CondKind::Other);
        assert_eq!(one(PG, "(a NOT IN (1, 2))", &["t"]).kind, CondKind::Other);
    }

    #[test]
    fn pg_empty_and_garbage_are_other() {
        assert_eq!(parse_conjuncts(PG, "", &own(&["t"])).len(), 1);
        assert_eq!(one(PG, "", &["t"]).kind, CondKind::Other);
        assert_eq!(one(PG, "(((", &["t"]).kind, CondKind::Other);
        assert_eq!(one(PG, "= =", &["t"]).kind, CondKind::Other);
    }

    #[test]
    fn pg_foreign_column_against_literal_is_not_ours() {
        let c = one(PG, "(c.name = 'x'::text)", &["o"]);
        assert!(c.col.is_none());
    }

    #[test]
    fn pg_comparison_operators_are_merged() {
        for (text, op) in [("(a >= 1)", ">="), ("(a <= 1)", "<="), ("(a <> 1)", "<>")] {
            assert_eq!(one(PG, text, &["t"]).op, op);
        }
    }

    // ---- MSSQL ---------------------------------------------------------------------------
    #[test]
    fn mssql_four_part_name() {
        let c = one(
            MSSQL,
            "[shop].[dbo].[Orders].[CustomerId]=(42)",
            &["Orders"],
        );
        assert_eq!((c.kind, col(&c).as_str()), (CondKind::Eq, "CustomerId"));
    }

    #[test]
    fn mssql_two_part_name_and_other_table() {
        let c = one(MSSQL, "[o].[CustomerId]=[c].[Id]", &["c"]);
        assert_eq!(c.kind, CondKind::Join);
        assert_eq!(col(&c), "Id");
    }

    #[test]
    fn mssql_convert_implicit() {
        let c = one(
            MSSQL,
            "CONVERT_IMPLICIT(nvarchar(50),[shop].[dbo].[Orders].[Code],0)=N'123'",
            &["Orders"],
        );
        assert_eq!(
            (c.kind, c.func.as_deref()),
            (CondKind::NonSargable, Some("convert_implicit"))
        );
        assert_eq!(col(&c), "Code");
    }

    #[test]
    fn mssql_cast_function() {
        let c = one(MSSQL, "CAST([Total] AS int)>(5)", &["Orders"]);
        assert_eq!(
            (c.kind, c.func.as_deref()),
            (CondKind::NonSargable, Some("cast"))
        );
    }

    #[test]
    fn mssql_bracket_with_close_bracket_inside() {
        let c = one(MSSQL, "[a]]b]=1", &["t"]);
        assert_eq!(col(&c), "a]b");
    }

    #[test]
    fn mssql_isnull_function() {
        let c = one(MSSQL, "isnull([Qty],(0))>(1)", &["Orders"]);
        assert_eq!(c.func.as_deref(), Some("isnull"));
    }

    // ---- MySQL ---------------------------------------------------------------------------
    #[test]
    fn mysql_backticks() {
        let c = one(MYSQL, "(`shop`.`orders`.`customer_id` = 42)", &["orders"]);
        assert_eq!((c.kind, col(&c).as_str()), (CondKind::Eq, "customer_id"));
    }

    #[test]
    fn mysql_double_quoted_string_is_a_value() {
        let c = one(MYSQL, "(`t`.`status` = \"x and y\")", &["t"]);
        assert_eq!(c.kind, CondKind::Eq);
    }

    #[test]
    fn mysql_function() {
        let c = one(MYSQL, "(year(`t`.`created_at`) = 2024)", &["t"]);
        assert_eq!(
            (c.kind, c.func.as_deref()),
            (CondKind::NonSargable, Some("year"))
        );
    }

    #[test]
    fn mysql_join_to_other_table() {
        let c = one(
            MYSQL,
            "(`shop`.`o`.`customer_id` = `shop`.`c`.`id`)",
            &["c"],
        );
        assert_eq!(c.kind, CondKind::Join);
    }

    // ---- Oracle --------------------------------------------------------------------------
    #[test]
    fn oracle_quoted_upper() {
        let c = one(ORACLE, "\"O\".\"STATUS\"='X'", &["O", "ORDERS"]);
        assert_eq!((c.kind, col(&c).as_str()), (CondKind::Eq, "STATUS"));
    }

    #[test]
    fn oracle_internal_function() {
        let c = one(ORACLE, "INTERNAL_FUNCTION(\"O\".\"CODE\")=5", &["O"]);
        assert_eq!(c.func.as_deref(), Some("internal_function"));
    }

    #[test]
    fn oracle_to_number() {
        let c = one(ORACLE, "TO_NUMBER(\"CODE\")=123", &["ORDERS"]);
        assert_eq!(c.kind, CondKind::NonSargable);
    }

    // ---- sort keys -----------------------------------------------------------------------
    #[test]
    fn sort_keys() {
        let k = parse_sort_key(PG, "created_at DESC").unwrap();
        assert_eq!(
            (k.col.name.as_str(), k.desc, k.computed),
            ("created_at", true, false)
        );
        let k = parse_sort_key(PG, "o.id").unwrap();
        assert_eq!((k.col.name.as_str(), k.desc), ("id", false));
        let k = parse_sort_key(PG, "orders.total DESC NULLS LAST").unwrap();
        assert_eq!((k.col.name.as_str(), k.desc), ("total", true));
        let k = parse_sort_key(PG, "lower((email)::text)").unwrap();
        assert!(k.computed);
        assert!(parse_sort_key(PG, "").is_none());
    }

    // ---- MongoDB -------------------------------------------------------------------------
    #[test]
    fn mongo_equality_and_operators() {
        let v = parse_mongo_filter(r#"{"status":{"$eq":"late"},"total":{"$gt":5,"$lte":9}}"#);
        let kinds: Vec<_> = v.iter().map(|c| (col(c), c.kind)).collect();
        assert_eq!(
            kinds,
            vec![
                ("status".to_string(), CondKind::Eq),
                ("total".to_string(), CondKind::Range),
                ("total".to_string(), CondKind::Range)
            ]
        );
    }

    #[test]
    fn mongo_scalar_is_equality() {
        let v = parse_mongo_filter(r#"{"status":"late","n":{"$numberLong":"5"}}"#);
        assert!(v.iter().all(|c| c.kind == CondKind::Eq));
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn mongo_in_and_ignored_operators() {
        let v = parse_mongo_filter(
            r#"{"a":{"$in":[1,2]},"b":{"$ne":1},"c":{"$exists":false},"d":{"$nin":[1]}}"#,
        );
        assert_eq!(v[0].kind, CondKind::Eq);
        assert!(v[1..].iter().all(|c| c.kind == CondKind::Other));
    }

    #[test]
    fn mongo_regex_anchored_or_not() {
        let v =
            parse_mongo_filter(r#"{"a":{"$regex":"^abc"},"b":{"$regex":"abc","$options":"i"}}"#);
        assert_eq!(v[0].kind, CondKind::Range);
        assert_eq!(v[1].kind, CondKind::NonSargable);
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn mongo_and_or() {
        let v = parse_mongo_filter(r#"{"$and":[{"a":1},{"b":{"$gt":2}}],"$or":[{"c":1}]}"#);
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].kind, CondKind::Eq);
        assert_eq!(v[1].kind, CondKind::Range);
        assert_eq!(v[2].kind, CondKind::Other);
    }

    #[test]
    fn mongo_garbage_is_empty() {
        assert!(parse_mongo_filter("nope").is_empty());
        assert!(parse_mongo_filter("").is_empty());
        assert!(parse_mongo_filter("[1,2]").is_empty());
    }

    #[test]
    fn mongo_dotted_field() {
        let v = parse_mongo_filter(r#"{"address.city":"Hue"}"#);
        assert_eq!(col(&v[0]), "address.city");
    }

    // ---- cast table ----------------------------------------------------------------------
    #[test]
    fn cast_harm_table_with_metadata() {
        assert!(!cast_is_harmful(
            "text",
            "status",
            Some("character varying(20)")
        ));
        assert!(!cast_is_harmful("bigint", "id", Some("integer")));
        assert!(cast_is_harmful("numeric", "qty", Some("integer")));
        assert!(cast_is_harmful("text", "id", Some("integer")));
        assert!(!cast_is_harmful("date", "d", Some("date")));
    }

    proptest! {
        #[test]
        fn never_panics_on_random_text(s in ".{0,120}") {
            for d in [PG, MSSQL, ORACLE, MYSQL, ScriptDialect::ClickHouse] {
                let _ = parse_conjuncts(d, &s, &own(&["t"]));
                let _ = parse_sort_key(d, &s);
            }
            let _ = parse_mongo_filter(&s);
        }

        #[test]
        fn never_panics_on_sqlish_text(
            s in prop::collection::vec(
                prop::sample::select(vec![
                    "(", ")", "AND", "OR", "NOT", "=", "<", ">=", "'x'", "1", "col", "t.col",
                    "lower(", "::text", "BETWEEN", "IN", "LIKE", "IS", "NULL", ",", ".", "[", "]",
                    "ANY", "ARRAY", "CASE", "END", "\"q\"", "$1", "+", "||", "%", "!", "~~",
                ]),
                0..40,
            )
        ) {
            let text = s.join(" ");
            for d in [PG, MSSQL, ORACLE, MYSQL] {
                let _ = parse_conjuncts(d, &text, &own(&["t"]));
                let _ = parse_sort_key(d, &text);
            }
        }
    }
}
