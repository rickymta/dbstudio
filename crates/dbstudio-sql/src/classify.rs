//! Statement analysis: split, classify and detect destructive statements.
//!
//! The classifier works on the token stream of [`crate::lexer`] (so strings, comments and quoted
//! identifiers never fool it) and on the statement ranges of [`crate::split`]. Statements it
//! cannot recognise are [`StatementClass::Unknown`]; `Procedural` and `Unknown` statements that
//! mention `DROP` / `TRUNCATE` anywhere (string literals and dollar quoted bodies included) are
//! reported as destructive to be safe.
//!
//! T-SQL note: [`crate::split::split`] returns one range per `GO` batch (variables are batch
//! scoped), while the analysis lists the statements *inside* the batches (split at `;`
//! outside `BEGIN ... END`), so the guard can judge each of them.

use dbstudio_core::{
    DestructiveReason, ScriptAnalysis, ScriptDialect, ScriptProblem, StatementClass, StatementInfo,
    TextRange, TxControl,
};

use crate::lexer::{Lexer, TokenKind, slice, to_u32, tokenize_chars};
use crate::split::{SplitState, scan};
use crate::{mongo_shell, redis_cli};

/// Analyse `text`.
///
/// * Blank text has no statements; text with only comments has one [`StatementClass::Empty`]
///   statement.
/// * Lexer problems (unterminated string / comment) make every statement `Unknown`.
pub fn analyze(d: ScriptDialect, text: &str) -> ScriptAnalysis {
    let chars: Vec<char> = text.chars().collect();
    let params = crate::params::find_params(d, text);
    let (statements, problems) = match d {
        ScriptDialect::Postgres
        | ScriptDialect::MySql { .. }
        | ScriptDialect::MsSql
        | ScriptDialect::Oracle
        | ScriptDialect::Cql
        | ScriptDialect::ClickHouse => analyze_sql(d, &chars),
        ScriptDialect::MongoShell => analyze_mongo(text, &chars),
        ScriptDialect::RedisCli => analyze_redis(&chars),
        ScriptDialect::Opaque => analyze_opaque(&chars),
    };
    ScriptAnalysis {
        dialect: d,
        statements,
        params,
        problems,
    }
}

/// Database each statement of `analysis` runs against, following `use <db>` (Mongo shell) and
/// `USE <db>` (MySQL, T-SQL) inside the script. Aligned with `analysis.statements`; `None` =
/// the session's database before the script (the caller's default). The `use` statement itself
/// reports the new database. Other dialects always give `None`.
///
/// `text` must be the text that was analysed.
pub fn statement_databases(
    d: ScriptDialect,
    text: &str,
    analysis: &ScriptAnalysis,
) -> Vec<Option<String>> {
    let n = analysis.statements.len();
    match d {
        ScriptDialect::MongoShell => match mongo_shell::parse(text) {
            Ok(stmts) if stmts.len() == n => {
                let mut cur: Option<String> = None;
                stmts
                    .iter()
                    .map(|s| {
                        if s.kind == mongo_shell::ShellKind::Use {
                            cur = Some(s.method.clone());
                        }
                        // `db.getSiblingDB("x")...` names its own database
                        s.database.clone().or_else(|| cur.clone())
                    })
                    .collect()
            }
            _ => vec![None; n],
        },
        ScriptDialect::MySql { .. }
        | ScriptDialect::MsSql
        | ScriptDialect::Cql
        | ScriptDialect::ClickHouse => {
            let chars: Vec<char> = text.chars().collect();
            let mut cur: Option<String> = None;
            analysis
                .statements
                .iter()
                .map(|s| {
                    let sig = sig_tokens(d, &chars, s.range);
                    if w(&sig, 0) == "USE" {
                        if let Some(t) = sig.get(1) {
                            if matches!(t.kind, TokenKind::Word | TokenKind::QuotedIdent) {
                                cur = Some(unquote(&t.raw));
                            }
                        }
                    }
                    cur.clone()
                })
                .collect()
        }
        _ => vec![None; n],
    }
}

fn range(start: usize, end: usize) -> TextRange {
    TextRange {
        start: to_u32(start),
        end: to_u32(end),
    }
}

fn info(index: usize, range: TextRange, o: Outcome) -> StatementInfo {
    StatementInfo {
        index: to_u32(index),
        range,
        class: o.class,
        verb: o.verb,
        destructive: o.destructive,
        implicit_commit: o.implicit_commit,
        targets: o.targets,
    }
}

#[derive(Debug, Clone)]
struct Outcome {
    class: StatementClass,
    destructive: Option<DestructiveReason>,
    implicit_commit: bool,
    verb: String,
    targets: Vec<String>,
}

impl Outcome {
    fn unknown() -> Self {
        Outcome {
            class: StatementClass::Unknown,
            destructive: None,
            implicit_commit: false,
            verb: String::new(),
            targets: Vec::new(),
        }
    }
}

fn comments_only(chars: &[char]) -> Option<StatementInfo> {
    let s = chars.iter().position(|c| !c.is_whitespace())?;
    let e = chars.iter().rposition(|c| !c.is_whitespace())? + 1;
    Some(info(
        0,
        range(s, e),
        Outcome {
            class: StatementClass::Empty,
            ..Outcome::unknown()
        },
    ))
}

// ---------------------------------------------------------------------------------------------
// Opaque / Redis / Mongo
// ---------------------------------------------------------------------------------------------

fn analyze_opaque(chars: &[char]) -> (Vec<StatementInfo>, Vec<ScriptProblem>) {
    let mut st = SplitState::default();
    let units = scan(ScriptDialect::Opaque, chars, &mut st, true).0;
    let d = ScriptDialect::Opaque;
    let out = units
        .iter()
        .enumerate()
        .map(|(i, u)| {
            let mut o = Outcome::unknown();
            o.destructive = keyword_scan_range(d, chars, u.range, 0);
            info(i, u.range, o)
        })
        .collect();
    (out, Vec::new())
}

fn analyze_redis(chars: &[char]) -> (Vec<StatementInfo>, Vec<ScriptProblem>) {
    let mut st = SplitState::default();
    let units = scan(ScriptDialect::RedisCli, chars, &mut st, true).0;
    let mut out = Vec::new();
    let mut problems = Vec::new();
    for (i, u) in units.iter().enumerate() {
        let line = slice(chars, u.range);
        match redis_cli::tokenize_line(&line) {
            Ok(tokens) => {
                let (class, destructive) = redis_cli::classify(&tokens);
                let spec = redis_cli::find_command(&tokens);
                let verb = match spec {
                    Some(s) => s.name.to_string(),
                    None => tokens
                        .first()
                        .map(|t| String::from_utf8_lossy(t).to_ascii_uppercase())
                        .unwrap_or_default(),
                };
                let targets = match class {
                    StatementClass::Read | StatementClass::Write => tokens
                        .get(1)
                        .map(|t| vec![String::from_utf8_lossy(t).into_owned()])
                        .unwrap_or_default(),
                    _ => Vec::new(),
                };
                out.push(info(
                    i,
                    u.range,
                    Outcome {
                        class,
                        destructive,
                        implicit_commit: false,
                        verb,
                        targets,
                    },
                ));
            }
            Err(e) => {
                problems.push(ScriptProblem {
                    range: u.range,
                    message: e.message(),
                });
                out.push(info(i, u.range, Outcome::unknown()));
            }
        }
    }
    // A problem makes every statement Unknown.
    if !problems.is_empty() {
        for s in &mut out {
            s.class = StatementClass::Unknown;
            s.destructive = None;
        }
    }
    (out, problems)
}

fn analyze_mongo(text: &str, chars: &[char]) -> (Vec<StatementInfo>, Vec<ScriptProblem>) {
    match mongo_shell::parse(text) {
        Ok(stmts) => {
            let out = stmts
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    let (class, destructive) = mongo_shell::classify(s);
                    info(
                        i,
                        s.range,
                        Outcome {
                            class,
                            destructive,
                            implicit_commit: false,
                            verb: mongo_shell::verb(s),
                            targets: s.collection.iter().cloned().collect(),
                        },
                    )
                })
                .collect();
            (out, Vec::new())
        }
        Err(e) => {
            let mut st = SplitState::default();
            let units = scan(ScriptDialect::MongoShell, chars, &mut st, true).0;
            let pos = match &e {
                dbstudio_core::Error::Query {
                    position: Some(p), ..
                } => *p,
                _ => 0,
            };
            let problem_range = units
                .iter()
                .find(|u| u.range.start <= pos && pos <= u.range.end)
                .map(|u| u.range)
                .unwrap_or_else(|| range(pos as usize, (pos as usize + 1).min(chars.len())));
            let out = units
                .iter()
                .enumerate()
                .map(|(i, u)| {
                    let mut o = Outcome::unknown();
                    o.destructive = mongo_keyword_scan(chars, u.range);
                    info(i, u.range, o)
                })
                .collect();
            (
                out,
                vec![ScriptProblem {
                    range: problem_range,
                    message: e.message(),
                }],
            )
        }
    }
}

/// Fallback for Mongo text that does not parse: `drop(` / `dropDatabase(` / `deleteMany(`.
fn mongo_keyword_scan(chars: &[char], r: TextRange) -> Option<DestructiveReason> {
    let lx = Lexer::new(chars, ScriptDialect::MongoShell);
    let mut i = r.start as usize;
    let end = r.end as usize;
    while i < end {
        let (k, e, _) = lx.next(i);
        if k == TokenKind::Word {
            let w: String = chars[i..e].iter().collect();
            let next_is_call = chars.get(e) == Some(&'(');
            match w.as_str() {
                "dropDatabase" if next_is_call => return Some(DestructiveReason::DropDatabase),
                "drop" if next_is_call => return Some(DestructiveReason::DropCollection),
                "deleteMany" | "remove" if next_is_call => {
                    return Some(DestructiveReason::DeleteManyAll);
                }
                "updateMany" if next_is_call => return Some(DestructiveReason::UpdateManyAll),
                _ => {}
            }
        }
        i = e;
    }
    None
}

// ---------------------------------------------------------------------------------------------
// SQL
// ---------------------------------------------------------------------------------------------

/// Significant token of a statement.
#[derive(Debug, Clone)]
struct Sig {
    kind: TokenKind,
    start: usize,
    end: usize,
    /// Upper-cased text of a [`TokenKind::Word`]; raw text of punctuation and numbers; empty
    /// for strings and quoted identifiers.
    up: String,
    /// Raw text of words (original case) and quoted identifiers.
    raw: String,
}

fn sig_tokens(d: ScriptDialect, chars: &[char], r: TextRange) -> Vec<Sig> {
    let lx = Lexer::new(chars, d);
    let mut out = Vec::new();
    let mut i = r.start as usize;
    let end = (r.end as usize).min(chars.len());
    while i < end {
        let (kind, e, _) = lx.next(i);
        let e = e.min(end).max(i + 1);
        if !kind.is_trivia() {
            let text: String = chars[i..e].iter().collect();
            let (up, raw) = match kind {
                TokenKind::Word => (text.to_uppercase(), text),
                TokenKind::QuotedIdent => (unquote(&text).to_uppercase(), text),
                TokenKind::String => (String::new(), String::new()),
                _ => (text, String::new()),
            };
            out.push(Sig {
                kind,
                start: i,
                end: e,
                up,
                raw,
            });
        }
        i = e;
    }
    out
}

fn analyze_sql(d: ScriptDialect, chars: &[char]) -> (Vec<StatementInfo>, Vec<ScriptProblem>) {
    let (_, problems) = tokenize_chars(d, chars);
    let mut st = SplitState::default();
    let units = scan(d, chars, &mut st, true).0;
    let mut ranges: Vec<TextRange> = Vec::new();
    for u in &units {
        if matches!(d, ScriptDialect::MsSql) {
            ranges.extend(mssql_statements(chars, u.range));
        } else {
            ranges.push(u.range);
        }
    }
    if ranges.is_empty() {
        return (comments_only(chars).into_iter().collect(), problems);
    }
    let mut out = Vec::new();
    for (i, r) in ranges.iter().enumerate() {
        let sig = sig_tokens(d, chars, *r);
        let mut o = classify_sql(d, chars, &sig);
        if !problems.is_empty() {
            // Lexer problems: do not trust the class; keep the keyword safety net.
            o.class = StatementClass::Unknown;
            o.implicit_commit = false;
            o.destructive = keyword_scan_range(d, chars, *r, 0);
        }
        out.push(info(i, *r, o));
    }
    (out, problems)
}

/// Words after which a statement keyword continues the current statement (`ON DELETE`,
/// `FOR UPDATE`, `WHEN MATCHED THEN DELETE`, `GRANT ALTER`...).
const MSSQL_CONTINUE_DML: &[&str] = &[
    "FOR", "AFTER", "INSTEAD", "OF", "THEN", "OR", "AND", ",", "DO", "WHEN", "BEFORE", "GRANT",
    "DENY", "REVOKE", "BULK",
];

/// Does the word `up` at position `k` of the significant tokens `t` start a new T-SQL statement
/// (T-SQL does not require `;`)? `first` is the first word of the statement so far.
fn mssql_starts_statement(
    t: &[(TokenKind, usize, usize, String)],
    k: usize,
    first: &str,
    cte_open: bool,
) -> bool {
    let up = t[k].3.as_str();
    let prev = if k == 0 { "" } else { t[k - 1].3.as_str() };
    let next = t.get(k + 1).map(|x| x.3.as_str()).unwrap_or("");
    let next2 = t.get(k + 2).map(|x| x.3.as_str()).unwrap_or("");
    // Definitions swallow what follows (procedure bodies, ALTER ... DROP ...).
    if matches!(first, "CREATE" | "ALTER") {
        return false;
    }
    // `WITH x AS (...) SELECT|INSERT|UPDATE|DELETE|MERGE`: the main verb belongs to the CTE.
    if cte_open && matches!(up, "SELECT" | "INSERT" | "UPDATE" | "DELETE" | "MERGE") {
        return false;
    }
    match up {
        "SELECT" => !matches!(
            prev,
            "UNION"
                | "ALL"
                | "INTERSECT"
                | "EXCEPT"
                | "AS"
                | "("
                | "FOR"
                | "IN"
                | "EXISTS"
                | "ANY"
                | "SOME"
                | "DISTINCT"
                | "OPTION"
                | "THEN"
                | ","
                | "GRANT"
                | "DENY"
                | "REVOKE"
        ),
        "INSERT" | "UPDATE" | "DELETE" => !MSSQL_CONTINUE_DML.contains(&prev),
        "MERGE" => !matches!(prev, ","),
        "DROP" | "TRUNCATE" | "DECLARE" | "USE" | "DBCC" | "KILL" => true,
        "EXEC" | "EXECUTE" => !matches!(prev, "AS" | "WITH" | ","),
        "CREATE" | "ALTER" => !matches!(prev, "GRANT" | "DENY" | "REVOKE" | "," | "OR" | "ADD"),
        "GRANT" | "REVOKE" | "DENY" | "BACKUP" | "RESTORE" => !matches!(prev, "WITH" | ","),
        "SET" => !matches!(first, "UPDATE" | "MERGE"),
        "WITH" => {
            !matches!(
                next,
                "EXECUTE"
                    | "RECOMPILE"
                    | "ENCRYPTION"
                    | "SCHEMABINDING"
                    | "VIEW_METADATA"
                    | "NATIVE_COMPILATION"
                    | "PASSWORD"
                    | "CHECK"
                    | "GRANT"
                    | "NOCHECK"
            ) && t.get(k + 1).is_some_and(|x| x.0 == TokenKind::Word)
                && matches!(next2, "AS" | "(")
        }
        "IF" | "WHILE" => !matches!(
            prev,
            "ELSE"
                | "TABLE"
                | "VIEW"
                | "INDEX"
                | "PROCEDURE"
                | "PROC"
                | "FUNCTION"
                | "TRIGGER"
                | "SCHEMA"
                | "DATABASE"
                | "TYPE"
                | "SEQUENCE"
                | "SYNONYM"
                | "COLUMN"
                | "CONSTRAINT"
                | "LOGIN"
                | "USER"
                | "ROLE"
                | ","
        ),
        "BEGIN" => !matches!(prev, "ELSE"),
        _ => false,
    }
}

/// T-SQL statements of a batch: split at `;` outside `BEGIN ... END` / `CASE ... END` / parens,
/// and before a statement keyword (T-SQL allows statements without `;`).
fn mssql_statements(chars: &[char], batch: TextRange) -> Vec<TextRange> {
    let d = ScriptDialect::MsSql;
    let lx = Lexer::new(chars, d);
    let end = batch.end as usize;
    // Significant tokens: (kind, start, end, upper text for words and punctuation).
    let mut t: Vec<(TokenKind, usize, usize, String)> = Vec::new();
    // Start of the first comment before the next token (comments belong to the statement
    // that follows them).
    let mut comment_start: Option<usize> = None;
    let mut starts: Vec<usize> = Vec::new();
    let mut i = batch.start as usize;
    while i < end {
        let (kind, e, _) = lx.next(i);
        let e = e.min(end);
        match kind {
            TokenKind::Whitespace => {}
            TokenKind::LineComment | TokenKind::BlockComment => {
                comment_start.get_or_insert(i);
            }
            _ => {
                let text = if matches!(kind, TokenKind::Word | TokenKind::Punct) {
                    chars[i..e].iter().collect::<String>().to_uppercase()
                } else {
                    String::new()
                };
                t.push((kind, i, e, text));
                starts.push(comment_start.take().unwrap_or(i));
            }
        }
        i = e;
    }
    let mut out = Vec::new();
    let mut cur_first: Option<usize> = None; // index in `t` of the first token
    let mut first_word = String::new();
    let mut depth = 0i32;
    let mut paren = 0i32;
    let mut pending_begin = false;
    let mut main_seen = false;
    let flush = |from: usize, to: usize, out: &mut Vec<TextRange>| {
        // statement = tokens t[from..to]
        if from < to {
            out.push(range(starts[from], t[to - 1].2));
        }
    };
    let mut k = 0;
    while k < t.len() {
        let (kind, _, _, ref up) = t[k];
        if kind == TokenKind::Punct && up == ";" && depth <= 0 && paren <= 0 {
            if let Some(f) = cur_first.take() {
                flush(f, k, &mut out);
            }
            depth = 0;
            first_word.clear();
            pending_begin = false;
            main_seen = false;
            k += 1;
            continue;
        }
        if kind == TokenKind::Word {
            if let Some(f) = cur_first {
                if depth <= 0
                    && paren <= 0
                    && !pending_begin
                    && mssql_starts_statement(
                        &t,
                        k,
                        &first_word,
                        first_word == "WITH" && !main_seen,
                    )
                {
                    flush(f, k, &mut out);
                    cur_first = None;
                    first_word.clear();
                    depth = 0;
                    main_seen = false;
                }
            }
        }
        if cur_first.is_none() {
            cur_first = Some(k);
            if kind == TokenKind::Word {
                first_word = up.clone();
            }
        }
        if kind == TokenKind::Punct {
            match up.as_str() {
                "(" => paren += 1,
                ")" => paren = (paren - 1).max(0),
                _ => {}
            }
        } else if kind == TokenKind::Word {
            if pending_begin {
                pending_begin = false;
                if !matches!(
                    up.as_str(),
                    "TRAN" | "TRANSACTION" | "DISTRIBUTED" | "DIALOG" | "CONVERSATION"
                ) {
                    depth += 1;
                }
            }
            if depth <= 0
                && paren <= 0
                && matches!(
                    up.as_str(),
                    "SELECT" | "INSERT" | "UPDATE" | "DELETE" | "MERGE"
                )
            {
                main_seen = true;
            }
            match up.as_str() {
                "BEGIN" => pending_begin = true,
                "CASE" => depth += 1,
                "END" => depth = (depth - 1).max(0),
                _ => {}
            }
        }
        k += 1;
    }
    // A trailing BEGIN opens nothing.
    if let Some(f) = cur_first {
        flush(f, t.len(), &mut out);
    }
    out
}

fn w(sig: &[Sig], i: usize) -> &str {
    match sig.get(i) {
        Some(s) if s.kind == TokenKind::Word => &s.up,
        _ => "",
    }
}

fn p(sig: &[Sig], i: usize) -> &str {
    match sig.get(i) {
        Some(s) if s.kind == TokenKind::Punct => &s.up,
        _ => "",
    }
}

fn is_mysql(d: ScriptDialect) -> bool {
    matches!(d, ScriptDialect::MySql { .. })
}

/// Index of the first word after leading `(`.
fn first_word(sig: &[Sig]) -> Option<usize> {
    let mut i = 0;
    while p(sig, i) == "(" {
        i += 1;
    }
    (!w(sig, i).is_empty()).then_some(i)
}

/// Words that precede a `DELETE` / `UPDATE` that is not a statement.
const NOT_DML_PREV: &[&str] = &[
    "ON", "FOR", "AFTER", "BEFORE", "OR", "INSTEAD", "OF", "DO", "KEY", "NO", "AND", "THEN",
];

const WHERE_STOP: &[&str] = &[
    "RETURNING",
    "ORDER",
    "LIMIT",
    "OFFSET",
    "OPTION",
    "FETCH",
    "OUTPUT",
];

fn trivially_true(cond: &[&Sig]) -> bool {
    // strip outer parentheses
    let mut c = cond;
    while c.len() >= 2 && c[0].up == "(" && c[c.len() - 1].up == ")" {
        c = &c[1..c.len() - 1];
    }
    match c {
        [] => false,
        [t] => {
            (t.kind == TokenKind::Word && t.up == "TRUE")
                || (t.kind == TokenKind::Number && !t.up.chars().all(|x| x == '0' || x == '.'))
        }
        [a, eq, b] => {
            a.kind == TokenKind::Number
                && b.kind == TokenKind::Number
                && eq.up == "="
                && a.up == b.up
        }
        _ => false,
    }
}

/// `DELETE` / `UPDATE` (at any depth) without a real `WHERE` in `sig[from..]`.
fn dml_without_where(sig: &[Sig], from: usize) -> Option<DestructiveReason> {
    for k in from..sig.len() {
        let s = &sig[k];
        if s.kind != TokenKind::Word || !matches!(s.up.as_str(), "DELETE" | "UPDATE") {
            continue;
        }
        if k > from && NOT_DML_PREV.contains(&w(sig, k - 1)) {
            continue;
        }
        // Look for WHERE at the same depth until the level closes.
        let mut dd = 0i32;
        let mut where_at: Option<usize> = None;
        let mut j = k + 1;
        while j < sig.len() {
            let t = &sig[j];
            if t.kind == TokenKind::Punct {
                match t.up.as_str() {
                    "(" => dd += 1,
                    ")" => {
                        dd -= 1;
                        if dd < 0 {
                            break;
                        }
                    }
                    _ => {}
                }
            } else if dd == 0 && t.kind == TokenKind::Word && t.up == "WHERE" {
                where_at = Some(j);
                break;
            }
            j += 1;
        }
        let reason = if s.up == "DELETE" {
            DestructiveReason::DeleteWithoutWhere
        } else {
            DestructiveReason::UpdateWithoutWhere
        };
        match where_at {
            None => return Some(reason),
            Some(wi) => {
                let mut cond: Vec<&Sig> = Vec::new();
                let mut dd = 0i32;
                for t in &sig[wi + 1..] {
                    if t.kind == TokenKind::Punct {
                        match t.up.as_str() {
                            "(" => dd += 1,
                            ")" => {
                                dd -= 1;
                                if dd < 0 {
                                    break;
                                }
                            }
                            _ => {}
                        }
                    } else if dd == 0
                        && t.kind == TokenKind::Word
                        && WHERE_STOP.contains(&t.up.as_str())
                    {
                        break;
                    }
                    cond.push(t);
                }
                if trivially_true(&cond) {
                    return Some(reason);
                }
            }
        }
    }
    None
}

fn string_inner(text: &str) -> String {
    let c: Vec<char> = text.chars().collect();
    if c.first() == Some(&'$') {
        // $tag$ ... $tag$
        if let Some(close) = c[1..].iter().position(|x| *x == '$') {
            let dl = close + 2;
            if c.len() >= dl * 2 {
                return c[dl..c.len() - dl].iter().collect();
            }
            if c.len() > dl {
                return c[dl..].iter().collect();
            }
        }
        return String::new();
    }
    let first = c.iter().position(|x| *x == '\'' || *x == '"');
    match first {
        Some(f) if c.len() > f + 1 => {
            let end = if matches!(c.last(), Some('\'' | '"')) && c.len() > f + 1 {
                c.len() - 1
            } else {
                c.len()
            };
            c[f + 1..end.max(f + 1)].iter().collect()
        }
        _ => String::new(),
    }
}

/// `DROP` / `TRUNCATE` anywhere in `r`, strings included (depth limits the recursion).
fn keyword_scan_range(
    d: ScriptDialect,
    chars: &[char],
    r: TextRange,
    depth: u8,
) -> Option<DestructiveReason> {
    let sig = sig_tokens(d, chars, r);
    keyword_scan(d, chars, &sig, depth)
}

fn keyword_scan(
    d: ScriptDialect,
    chars: &[char],
    sig: &[Sig],
    depth: u8,
) -> Option<DestructiveReason> {
    for s in sig {
        match s.kind {
            TokenKind::Word => match s.up.as_str() {
                "DROP" => return Some(DestructiveReason::Drop),
                "TRUNCATE" => return Some(DestructiveReason::Truncate),
                _ => {}
            },
            TokenKind::String if depth < 2 => {
                let text: String = chars[s.start..s.end].iter().collect();
                let inner: Vec<char> = string_inner(&text).chars().collect();
                if inner.is_empty() {
                    continue;
                }
                let r = range(0, inner.len());
                if let Some(x) = keyword_scan_range(d, &inner, r, depth + 1) {
                    return Some(x);
                }
            }
            _ => {}
        }
    }
    // Data-destroying statements inside blocks / dynamic SQL.
    if let Some(r) = dml_without_where(sig, 0) {
        return Some(r);
    }
    if sig
        .iter()
        .any(|s| s.kind == TokenKind::Word && s.up == "MERGE")
        && sig
            .windows(2)
            .any(|p| p[0].up == "THEN" && p[1].up == "DELETE")
    {
        return Some(DestructiveReason::DeleteWithoutWhere);
    }
    if let Some(a) = sig
        .iter()
        .position(|s| s.kind == TokenKind::Word && s.up == "ALTER")
    {
        if sig[a..]
            .iter()
            .any(|s| s.kind == TokenKind::Word && s.up == "DROP")
        {
            return Some(DestructiveReason::AlterDrop);
        }
    }
    None
}

fn after_modifiers(sig: &[Sig], mut i: usize) -> usize {
    while matches!(
        w(sig, i),
        "OR" | "REPLACE"
            | "TEMP"
            | "TEMPORARY"
            | "UNIQUE"
            | "GLOBAL"
            | "LOCAL"
            | "UNLOGGED"
            | "EDITIONABLE"
            | "NONEDITIONABLE"
            | "CLUSTERED"
            | "NONCLUSTERED"
            | "ONLINE"
            | "FORCE"
            | "NOFORCE"
            | "PUBLIC"
            | "SHARED"
            | "AND"
            | "COMPILE"
            | "RECURSIVE"
    ) {
        i += 1;
    }
    i
}

fn verb_of(sig: &[Sig]) -> String {
    let Some(f) = first_word(sig) else {
        return String::new();
    };
    let first = w(sig, f);
    match first {
        "CREATE" | "ALTER" | "DROP" => {
            let i = after_modifiers(sig, f + 1);
            let obj = w(sig, i);
            if obj.is_empty() {
                return first.to_string();
            }
            if matches!(
                obj,
                "MATERIALIZED" | "FOREIGN" | "EVENT" | "FULLTEXT" | "SPATIAL"
            ) {
                let next = w(sig, i + 1);
                if !next.is_empty() {
                    return format!("{first} {obj} {next}");
                }
            }
            format!("{first} {obj}")
        }
        "TRUNCATE" if w(sig, f + 1) == "TABLE" => "TRUNCATE TABLE".to_string(),
        "START" if w(sig, f + 1) == "TRANSACTION" => "START TRANSACTION".to_string(),
        "BULK" if w(sig, f + 1) == "INSERT" => "BULK INSERT".to_string(),
        "LOAD" if w(sig, f + 1) == "DATA" => "LOAD DATA".to_string(),
        _ => first.to_string(),
    }
}

fn unquote(raw: &str) -> String {
    let c: Vec<char> = raw.chars().collect();
    if c.len() >= 2 {
        let (a, b) = (c[0], c[c.len() - 1]);
        if (a == '"' && b == '"') || (a == '`' && b == '`') || (a == '[' && b == ']') {
            return c[1..c.len() - 1].iter().collect();
        }
    }
    raw.to_string()
}

const NOT_A_NAME: &[&str] = &[
    "SELECT",
    "VALUES",
    "LATERAL",
    "ONLY",
    "IF",
    "NOT",
    "EXISTS",
    "TEMP",
    "TEMPORARY",
    "UNLOGGED",
    "MATERIALIZED",
    "DUAL",
    "UNNEST",
    "WITH",
    "TABLE",
    "VIEW",
    "INDEX",
    "SEQUENCE",
    "SCHEMA",
    "DATABASE",
    "FUNCTION",
    "PROCEDURE",
    "TRIGGER",
    "TYPE",
    "OR",
    "REPLACE",
    "UNIQUE",
    "GLOBAL",
    "FROM",
    "SET",
    "WHERE",
    "USING",
    "ON",
    "OUTPUT",
    "TOP",
    "ALL",
    "DISTINCT",
];

fn targets_of(sig: &[Sig]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < sig.len() {
        let kw = w(sig, i);
        let is_kw = matches!(
            kw,
            "FROM"
                | "JOIN"
                | "INTO"
                | "UPDATE"
                | "TABLE"
                | "VIEW"
                | "TRUNCATE"
                | "DESCRIBE"
                | "DESC"
        ) || (kw == "ON" && i >= 2 && matches!(w(sig, i - 2), "INDEX" | "TRIGGER"));
        if !is_kw {
            i += 1;
            continue;
        }
        // `FOR UPDATE`, `ON UPDATE`, `DO UPDATE`, `KEY UPDATE` are not targets.
        if kw == "UPDATE" && i > 0 && NOT_DML_PREV.contains(&w(sig, i - 1)) {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < sig.len() && NOT_A_NAME.contains(&w(sig, j)) && w(sig, j) != "DUAL" {
            j += 1;
        }
        let mut parts: Vec<String> = Vec::new();
        loop {
            match sig.get(j) {
                Some(s) if s.kind == TokenKind::Word && !NOT_A_NAME.contains(&s.up.as_str()) => {
                    parts.push(s.raw.clone());
                }
                Some(s) if s.kind == TokenKind::QuotedIdent => parts.push(unquote(&s.raw)),
                _ => break,
            }
            j += 1;
            if p(sig, j) == "." {
                j += 1;
            } else {
                break;
            }
        }
        // `FROM f(x)` is a function call, not a table.
        let is_call = p(sig, j) == "(" && matches!(kw, "FROM" | "JOIN");
        if !parts.is_empty() && !is_call {
            let name = parts.join(".");
            if !out.contains(&name) {
                out.push(name);
            }
        }
        i = j.max(i + 1);
    }
    out
}

fn depth0_word_pos(sig: &[Sig], from: usize, names: &[&str]) -> Option<usize> {
    let mut depth = 0i32;
    for (k, s) in sig.iter().enumerate().skip(from) {
        match s.kind {
            TokenKind::Punct if s.up == "(" => depth += 1,
            TokenKind::Punct if s.up == ")" => depth -= 1,
            TokenKind::Word if depth == 0 && names.contains(&s.up.as_str()) => return Some(k),
            _ => {}
        }
    }
    None
}

fn is_write_like(c: StatementClass) -> bool {
    !matches!(
        c,
        StatementClass::Read
            | StatementClass::Empty
            | StatementClass::Session
            | StatementClass::Transaction { .. }
            | StatementClass::Explain { writes: false, .. }
    )
}

fn explain(
    d: ScriptDialect,
    chars: &[char],
    sig: &[Sig],
    f: usize,
) -> (StatementClass, Option<DestructiveReason>) {
    use StatementClass as C;
    let mut i = f + 1;
    let mut analyze = false;
    match d {
        ScriptDialect::Postgres => {
            if p(sig, i) == "(" {
                let mut depth = 0;
                while i < sig.len() {
                    match p(sig, i) {
                        "(" => depth += 1,
                        ")" => {
                            depth -= 1;
                            if depth == 0 {
                                i += 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                    if matches!(w(sig, i), "ANALYZE" | "ANALYSE")
                        && !matches!(w(sig, i + 1), "FALSE" | "OFF")
                        && sig.get(i + 1).is_none_or(|n| n.up != "0")
                    {
                        analyze = true;
                    }
                    i += 1;
                }
            } else {
                while matches!(w(sig, i), "ANALYZE" | "ANALYSE" | "VERBOSE") {
                    if w(sig, i) != "VERBOSE" {
                        analyze = true;
                    }
                    i += 1;
                }
            }
        }
        // ClickHouse `EXPLAIN [AST|SYNTAX|PLAN|PIPELINE|ESTIMATE] <query>` never runs the query.
        ScriptDialect::ClickHouse | ScriptDialect::Cql => {
            return (
                C::Explain {
                    analyze: false,
                    writes: false,
                },
                None,
            );
        }
        ScriptDialect::Oracle => {
            // EXPLAIN PLAN [SET STATEMENT_ID = 'x'] FOR <stmt>
            return (
                C::Explain {
                    analyze: false,
                    writes: false,
                },
                None,
            );
        }
        _ => {
            if w(sig, i) == "ANALYZE" {
                analyze = true;
                i += 1;
            }
            // FORMAT = JSON | TREE | TRADITIONAL
            if w(sig, i) == "FORMAT" && p(sig, i + 1) == "=" {
                i += 3;
            }
        }
    }
    let inner = if i < sig.len() {
        classify_sig(d, chars, &sig[i..])
    } else {
        (C::Unknown, None)
    };
    // `EXPLAIN tbl` (MySQL) is DESCRIBE.
    if is_mysql(d) && !analyze && !matches!(inner.0, C::Read | C::Write | C::Ddl) {
        return (
            C::Explain {
                analyze: false,
                writes: false,
            },
            None,
        );
    }
    let writes = analyze && is_write_like(inner.0);
    (
        C::Explain { analyze, writes },
        if analyze { inner.1 } else { None },
    )
}

/// Functions that change state although they are called from a `SELECT`.
const VOLATILE_FUNCTIONS: &[&str] = &[
    "NEXTVAL",
    "SETVAL",
    "SET_CONFIG",
    "PG_TERMINATE_BACKEND",
    "PG_CANCEL_BACKEND",
    "PG_RELOAD_CONF",
    "PG_ROTATE_LOGFILE",
    "PG_SWITCH_WAL",
    "PG_CREATE_RESTORE_POINT",
    "LO_IMPORT",
    "LO_EXPORT",
    "LO_UNLINK",
    "LO_CREAT",
    "LO_CREATE",
    "LO_FROM_BYTEA",
    "LO_PUT",
    "DBLINK_EXEC",
    "DBLINK",
    "DBLINK_SEND_QUERY",
    "GET_LOCK",
    "DBLINK_CONNECT",
    "PG_REPLICATION_SLOT_ADVANCE",
    "PG_DROP_REPLICATION_SLOT",
    "PG_CREATE_LOGICAL_REPLICATION_SLOT",
    "PG_CREATE_PHYSICAL_REPLICATION_SLOT",
];

/// The class of a `SELECT` (or the `SELECT` of a `WITH`): anything that writes, locks or
/// changes session state is not a plain read.
fn select_class(d: ScriptDialect, sig: &[Sig], f: usize) -> StatementClass {
    use StatementClass as C;
    if matches!(d, ScriptDialect::Cql) {
        return C::Read;
    }
    if matches!(d, ScriptDialect::ClickHouse) {
        // `INTO OUTFILE` writes a file on the client; everything else is a read.
        return if depth0_word_pos(sig, f, &["INTO"]).is_some() {
            C::Write
        } else {
            C::Read
        };
    }
    if let Some(k) = depth0_word_pos(sig, f, &["INTO"]) {
        match d {
            ScriptDialect::Postgres | ScriptDialect::MsSql => return C::Ddl,
            // INTO OUTFILE / DUMPFILE write files, INTO @var changes session state.
            ScriptDialect::MySql { .. } => {
                let _ = k;
                return C::Write;
            }
            _ => {}
        }
    }
    for (k, s) in sig.iter().enumerate().skip(f) {
        if !matches!(s.kind, TokenKind::Word | TokenKind::QuotedIdent) {
            continue;
        }
        // A quoted identifier is only a function when it is called.
        let callable = s.kind == TokenKind::Word || p(sig, k + 1) == "(";
        if callable
            && (VOLATILE_FUNCTIONS.contains(&s.up.as_str())
                || s.up.starts_with("PG_ADVISORY_")
                || s.up.starts_with("PG_TRY_ADVISORY_"))
        {
            return C::Write;
        }
        if s.kind != TokenKind::Word {
            continue;
        }
        // Row locks: FOR UPDATE / FOR SHARE / FOR NO KEY UPDATE / FOR KEY SHARE,
        // MySQL LOCK IN SHARE MODE.
        if s.up == "FOR"
            && (matches!(w(sig, k + 1), "UPDATE" | "SHARE")
                || (matches!(w(sig, k + 1), "NO" | "KEY")
                    && matches!(w(sig, k + 2), "KEY" | "UPDATE" | "SHARE")))
        {
            return C::Write;
        }
        if s.up == "LOCK" && w(sig, k + 1) == "IN" && w(sig, k + 2) == "SHARE" {
            return C::Write;
        }
    }
    C::Read
}

/// `READ WRITE` among the words (transaction characteristics that lift read-only).
fn has_read_write(sig: &[Sig]) -> bool {
    sig.windows(2)
        .any(|p| p[0].up == "READ" && p[1].up == "WRITE" && p[0].kind == TokenKind::Word)
}

/// Does this SET / RESET change read-only mode, string lexing or the role?
fn set_lifts(sig: &[Sig]) -> bool {
    const NAMES: &[&str] = &[
        "TRANSACTION_READ_ONLY",
        "TX_READ_ONLY",
        "SQL_MODE",
        "SUPER_READ_ONLY",
        "READONLY",
        "STANDARD_CONFORMING_STRINGS",
        "BACKSLASH_QUOTE",
        "ESCAPE_STRING_WARNING",
    ];
    let named = |s: &Sig| matches!(s.kind, TokenKind::Word | TokenKind::QuotedIdent);
    has_read_write(sig)
        || sig.iter().any(|s| {
            named(s)
                && (NAMES.iter().any(|k| s.up.contains(k))
                    || s.up == "READ_ONLY"
                    || s.up.ends_with(".READ_ONLY"))
        })
        || sig
            .iter()
            .any(|s| matches!(s.kind, TokenKind::Word | TokenKind::QuotedIdent) && s.up == "ROLE")
        || sig
            .windows(2)
            .any(|p| p[0].up == "SESSION" && p[1].up == "AUTHORIZATION")
}

fn classify_sig(
    d: ScriptDialect,
    chars: &[char],
    sig: &[Sig],
) -> (StatementClass, Option<DestructiveReason>) {
    use StatementClass as C;
    let Some(f) = first_word(sig) else {
        // e.g. `[sp_executesql] N'drop table t'`: unknown, but never skip the safety net.
        return (C::Unknown, keyword_scan(d, chars, sig, 0));
    };
    let first = w(sig, f);
    let second = w(sig, f + 1);
    let tx = |op| (C::Transaction { op }, None);
    let procedural = |sig: &[Sig]| (C::Procedural, keyword_scan(d, chars, sig, 0));
    let mssql = matches!(d, ScriptDialect::MsSql);
    let oracle = matches!(d, ScriptDialect::Oracle);
    let pg = matches!(d, ScriptDialect::Postgres);
    if matches!(d, ScriptDialect::Cql) {
        if let Some(r) = classify_cql(d, chars, sig, f) {
            return r;
        }
    }
    if matches!(d, ScriptDialect::ClickHouse) {
        if let Some(r) = classify_clickhouse(sig, f) {
            return r;
        }
    }
    match first {
        // TiDB non-transactional DML: `BATCH ON id LIMIT 1000 DELETE FROM t WHERE ...`.
        "BATCH" if is_mysql(d) && matches!(second, "ON" | "LIMIT") => {
            (C::Write, dml_without_where(sig, f + 1))
        }
        // TiDB `ADMIN SHOW DDL JOBS`, `ADMIN CANCEL DDL JOBS`; CockroachDB / TiDB
        // `CANCEL QUERY|JOB|SESSION|IMPORT JOB`, CockroachDB `PAUSE|RESUME JOB`.
        "ADMIN" if is_mysql(d) => (C::Admin, None),
        "CANCEL" if pg || is_mysql(d) => (C::Admin, None),
        "PAUSE" | "RESUME" if pg && matches!(second, "JOB" | "JOBS" | "SCHEDULE" | "SCHEDULES") => {
            (C::Admin, None)
        }
        // CockroachDB `SHOW RANGES` / `SHOW RANGE FROM ...` expose cluster internals.
        "SHOW" if pg && matches!(second, "RANGES" | "RANGE") => (C::Admin, None),
        "SET" if pg && second == "CLUSTER" => (C::Admin, None),
        "SET" if is_mysql(d) && second == "CONFIG" => (C::Admin, None),
        "SELECT" => (select_class(d, sig, f), None),
        "VALUES" => (select_class(d, sig, f), None),
        "TABLE" if pg => (C::Read, None),
        "WITH" => {
            let main = depth0_word_pos(
                sig,
                f + 1,
                &["SELECT", "INSERT", "UPDATE", "DELETE", "MERGE", "VALUES"],
            );
            let dml = dml_without_where(sig, f);
            match main.map(|k| w(sig, k)) {
                Some("INSERT" | "UPDATE" | "DELETE" | "MERGE") => (C::Write, dml),
                Some("SELECT") => {
                    // PostgreSQL data-modifying CTE: INSERT/UPDATE/DELETE inside parentheses.
                    let inner_dml = sig.iter().enumerate().any(|(k, s)| {
                        s.kind == TokenKind::Word
                            && matches!(s.up.as_str(), "INSERT" | "UPDATE" | "DELETE")
                            && k > 0
                            && !NOT_DML_PREV.contains(&w(sig, k - 1))
                            && !(s.up == "INSERT" && matches!(w(sig, k - 1), "INTO"))
                            && p(sig, k - 1) == "("
                    });
                    if inner_dml {
                        (C::Write, dml)
                    } else {
                        (select_class(d, sig, main.unwrap_or(f)), None)
                    }
                }
                _ => (C::Read, None),
            }
        }
        // MySQL: DESCRIBE/DESC is EXPLAIN; `DESCRIBE ANALYZE <stmt>` executes the statement.
        "DESCRIBE" | "DESC"
            if is_mysql(d)
                && matches!(
                    second,
                    "ANALYZE"
                        | "SELECT"
                        | "INSERT"
                        | "UPDATE"
                        | "DELETE"
                        | "REPLACE"
                        | "WITH"
                        | "FORMAT"
                ) =>
        {
            explain(d, chars, sig, f)
        }
        "SHOW" | "DESCRIBE" | "DESC" | "HELP" | "FETCH" | "MOVE" => (C::Read, None),
        "EXPLAIN" => explain(d, chars, sig, f),
        "ANALYZE"
            if is_mysql(d)
                && matches!(
                    second,
                    "SELECT" | "INSERT" | "UPDATE" | "DELETE" | "WITH" | "FORMAT"
                ) =>
        {
            // MariaDB `ANALYZE <stmt>` executes the statement like EXPLAIN ANALYZE.
            let inner = classify_sig(d, chars, &sig[f + 1..]);
            (
                C::Explain {
                    analyze: true,
                    writes: is_write_like(inner.0),
                },
                inner.1,
            )
        }
        "MERGE" => {
            // WHEN [NOT] MATCHED ... THEN DELETE removes rows.
            let del = sig
                .windows(2)
                .any(|p| p[0].up == "THEN" && p[1].up == "DELETE" && p[1].kind == TokenKind::Word);
            (
                C::Write,
                del.then_some(DestructiveReason::DeleteWithoutWhere),
            )
        }
        "INSERT" | "UPSERT" => (C::Write, None),
        "REPLACE" if is_mysql(d) => (C::Write, None),
        "LOAD" if is_mysql(d) && second == "DATA" => (C::Write, None),
        "BULK" if mssql => (C::Write, None),
        "UPDATE" if mssql && second == "STATISTICS" => (C::Admin, None),
        "UPDATE" | "DELETE" => (C::Write, dml_without_where(sig, f)),
        "COPY" if pg => {
            if sig.iter().any(|s| s.up == "PROGRAM") {
                return (C::Admin, None);
            }
            match depth0_word_pos(sig, f + 1, &["FROM", "TO"]).map(|k| w(sig, k)) {
                Some("FROM") => (C::Write, None),
                Some("TO") => (C::Read, None),
                _ => (C::Admin, None),
            }
        }
        "CALL" | "EXEC" | "EXECUTE" | "DO" => procedural(sig),
        "DECLARE" => {
            if oracle {
                procedural(sig)
            } else {
                (C::Session, None)
            }
        }
        "BEGIN" => {
            if oracle {
                procedural(sig)
            } else if mssql {
                if matches!(second, "TRAN" | "TRANSACTION" | "DISTRIBUTED") {
                    tx(TxControl::Begin)
                } else {
                    procedural(sig)
                }
            } else if has_read_write(sig) {
                (C::Admin, None)
            } else {
                tx(TxControl::Begin)
            }
        }
        "START" if second == "TRANSACTION" && has_read_write(sig) => (C::Admin, None),
        "START" if second == "TRANSACTION" => tx(TxControl::Begin),
        "COMMIT" => tx(TxControl::Commit),
        "END" if pg => tx(TxControl::Commit),
        "ABORT" if pg => tx(TxControl::Rollback),
        "PREPARE" if pg && second == "TRANSACTION" => tx(TxControl::Commit),
        "ROLLBACK" => {
            if second == "TO" {
                tx(TxControl::RollbackToSavepoint)
            } else {
                tx(TxControl::Rollback)
            }
        }
        "SAVEPOINT" => tx(TxControl::Savepoint),
        "SAVE" if mssql && matches!(second, "TRAN" | "TRANSACTION") => tx(TxControl::Savepoint),
        "RELEASE" => tx(TxControl::Release),
        "SET" => {
            // MariaDB: SET STATEMENT var=value FOR <stmt> is judged by the statement.
            if is_mysql(d) && second == "STATEMENT" {
                if let Some(k) = depth0_word_pos(sig, f + 2, &["FOR"]) {
                    if set_lifts(&sig[f..k]) {
                        return (C::Admin, None);
                    }
                    return classify_sig(d, chars, &sig[k + 1..]);
                }
            }
            // Settings that lift read-only mode, change how strings are lexed or switch the
            // role are not plain session statements.
            if set_lifts(sig) {
                return (C::Admin, None);
            }
            if is_mysql(d) {
                let global = matches!(second, "GLOBAL" | "PERSIST" | "PERSIST_ONLY" | "PASSWORD")
                    || sig.get(f + 1).is_some_and(|s| s.up.starts_with("@@GLOBAL"));
                if global {
                    return (C::Admin, None);
                }
            }
            (C::Session, None)
        }
        "RESET" => {
            if is_mysql(d) || second == "ALL" || set_lifts(sig) {
                (C::Admin, None)
            } else {
                (C::Session, None)
            }
        }
        "DISCARD" if second == "ALL" => (C::Admin, None),
        "LOCK" => {
            // LOCK TABLE(S) ... WRITE / EXCLUSIVE blocks other sessions.
            if sig.iter().any(|s| {
                s.kind == TokenKind::Word && matches!(s.up.as_str(), "WRITE" | "EXCLUSIVE")
            }) {
                (C::Admin, None)
            } else {
                (C::Session, None)
            }
        }
        "PREPARE" => {
            // PostgreSQL: PREPARE n AS <stmt>; MySQL: PREPARE n FROM '<stmt>'.
            let inner = if pg {
                depth0_word_pos(sig, f + 1, &["AS"]).map(|k| classify_sig(d, chars, &sig[k + 1..]))
            } else {
                depth0_word_pos(sig, f + 1, &["FROM"]).and_then(|k| match sig.get(k + 1) {
                    Some(t) if t.kind == TokenKind::String => {
                        let text: String = chars[t.start..t.end].iter().collect();
                        let inner: Vec<char> = string_inner(&text).chars().collect();
                        let r = range(0, inner.len());
                        let isig = sig_tokens(d, &inner, r);
                        Some(classify_sig(d, &inner, &isig))
                    }
                    _ => None,
                })
            };
            match inner {
                Some((c, dr)) => {
                    let class = if c == C::Read {
                        C::Session
                    } else {
                        C::Procedural
                    };
                    (class, dr.or_else(|| keyword_scan(d, chars, sig, 0)))
                }
                None => procedural(sig),
            }
        }
        "USE" | "UNLOCK" | "LISTEN" | "UNLISTEN" | "NOTIFY" | "DISCARD" | "CLOSE"
        | "DEALLOCATE" | "OPEN" | "WORK" => (C::Session, None),
        "CREATE" | "ALTER" | "DROP" | "TRUNCATE" | "RENAME" | "COMMENT" | "FLASHBACK"
        | "REFRESH" => classify_ddl(d, sig, f),
        "GRANT" | "REVOKE" | "DENY" | "KILL" | "SHUTDOWN" | "FLUSH" | "DBCC" | "BACKUP"
        | "RESTORE" | "CHECKPOINT" | "RECONFIGURE" | "VACUUM" | "REINDEX" | "CLUSTER"
        | "OPTIMIZE" | "CHECK" | "REPAIR" | "AUDIT" | "NOAUDIT" | "PURGE" | "ANALYZE"
        | "INSTALL" | "UNINSTALL" | "CHANGE" | "ASSOCIATE" | "DISASSOCIATE" | "SECURITY"
        | "IMPORT" => (C::Admin, None),
        "PRINT" | "RAISERROR" | "THROW" | "WAITFOR" | "IF" | "WHILE" | "ELSE" | "RETURN"
        | "GOTO" | "TRY" | "BREAK" | "CONTINUE" | "SIGNAL" | "RESIGNAL"
            if mssql || is_mysql(d) =>
        {
            procedural(sig)
        }
        _ => {
            // Unknown verb: keyword safety net.
            (C::Unknown, keyword_scan(d, chars, sig, 0))
        }
    }
}

/// CQL statements that the generic SQL rules do not classify right.
fn classify_cql(
    d: ScriptDialect,
    chars: &[char],
    sig: &[Sig],
    f: usize,
) -> Option<(StatementClass, Option<DestructiveReason>)> {
    use StatementClass as C;
    let second = w(sig, f + 1);
    match w(sig, f) {
        // BEGIN [UNLOGGED | COUNTER] BATCH ... APPLY BATCH writes; any other BEGIN is not CQL.
        "BEGIN" => {
            let batch = (f + 1..f + 3).any(|k| w(sig, k) == "BATCH");
            Some(if batch {
                (C::Write, None)
            } else {
                (C::Procedural, keyword_scan(d, chars, sig, 0))
            })
        }
        "LIST" => Some((C::Read, None)),
        "CONSISTENCY" | "TRACING" | "PAGING" => Some((C::Session, None)),
        "SERIAL" if second == "CONSISTENCY" => Some((C::Session, None)),
        _ => None,
    }
}

/// ClickHouse statements that the generic SQL rules do not classify right.
fn classify_clickhouse(
    sig: &[Sig],
    f: usize,
) -> Option<(StatementClass, Option<DestructiveReason>)> {
    use StatementClass as C;
    match w(sig, f) {
        "EXISTS" | "WATCH" => Some((C::Read, None)),
        "DETACH" => Some((C::Ddl, Some(DestructiveReason::Drop))),
        "ATTACH" | "OPTIMIZE" | "EXCHANGE" | "UNDROP" => Some((C::Ddl, None)),
        "SYSTEM" => Some((C::Admin, None)),
        "ALTER" if w(sig, after_modifiers(sig, f + 1)) == "TABLE" => {
            // Mutations `ALTER TABLE t UPDATE ... WHERE` / `DELETE WHERE` change data. A `DELETE`
            // after `TTL` is only the TTL action.
            let ttl = sig
                .iter()
                .position(|s| s.kind == TokenKind::Word && s.up == "TTL");
            let m = depth0_word_pos(sig, f + 1, &["UPDATE", "DELETE"])
                .filter(|k| ttl.is_none_or(|t| *k < t));
            let drops = sig.iter().enumerate().skip(f + 1).any(|(k, s)| {
                s.kind == TokenKind::Word
                    && matches!(s.up.as_str(), "DROP" | "DETACH")
                    && !matches!(w(sig, k + 1), "DEFAULT" | "MATERIALIZED" | "ALIAS")
            });
            if drops {
                return Some((
                    if m.is_some() { C::Write } else { C::Ddl },
                    Some(DestructiveReason::AlterDrop),
                ));
            }
            m.map(|k| (C::Write, dml_without_where(sig, k)))
        }
        _ => None,
    }
}

fn classify_ddl(
    d: ScriptDialect,
    sig: &[Sig],
    f: usize,
) -> (StatementClass, Option<DestructiveReason>) {
    use StatementClass as C;
    let first = w(sig, f);
    let obj_i = after_modifiers(sig, f + 1);
    let obj = w(sig, obj_i);
    match first {
        "ALTER" if obj == "SESSION" => return (C::Session, None),
        "ALTER" if obj == "SYSTEM" => return (C::Admin, None),
        "REFRESH" => return (C::Write, None),
        "COMMENT" => return (C::Ddl, None),
        _ => {}
    }
    let ch_admin = matches!(d, ScriptDialect::ClickHouse)
        && (matches!(obj, "QUOTA" | "POLICY" | "SETTINGS")
            || (obj == "ROW" && w(sig, obj_i + 1) == "POLICY"));
    let admin_obj = (matches!(obj, "USER" | "ROLE" | "LOGIN" | "PROFILE" | "TABLESPACE")
        || ch_admin)
        && !matches!(first, "TRUNCATE" | "RENAME");
    let class = if admin_obj { C::Admin } else { C::Ddl };
    let destructive = match first {
        "DROP" => Some(DestructiveReason::Drop),
        "TRUNCATE" => Some(DestructiveReason::Truncate),
        "ALTER" => {
            let drop_at = sig
                .iter()
                .enumerate()
                .skip(f + 1)
                .find(|(k, s)| {
                    s.kind == TokenKind::Word
                        && s.up == "DROP"
                        && !matches!(w(sig, k + 1), "NOT" | "DEFAULT" | "IDENTITY" | "EXPRESSION")
                })
                .map(|(k, _)| k);
            if drop_at.is_some() {
                Some(DestructiveReason::AlterDrop)
            } else if sig
                .iter()
                .skip(f + 1)
                .any(|s| s.kind == TokenKind::Word && s.up == "TRUNCATE")
            {
                // ALTER TABLE t TRUNCATE PARTITION p
                Some(DestructiveReason::Truncate)
            } else {
                None
            }
        }
        _ => None,
    };
    (class, destructive)
}

fn implicit_commit(d: ScriptDialect, sig: &[Sig], class: StatementClass) -> bool {
    let Some(f) = first_word(sig) else {
        return false;
    };
    let first = w(sig, f);
    let second = w(sig, f + 1);
    match d {
        ScriptDialect::MySql { .. } => {
            // Temporary tables do not commit.
            let obj = w(sig, after_modifiers(sig, f + 1));
            let temp =
                matches!(first, "CREATE" | "DROP") && (second == "TEMPORARY" || obj == "TEMPORARY");
            if temp {
                return false;
            }
            matches!(class, StatementClass::Ddl)
                || matches!(
                    first,
                    "GRANT"
                        | "REVOKE"
                        | "ANALYZE"
                        | "OPTIMIZE"
                        | "REPAIR"
                        | "LOCK"
                        | "UNLOCK"
                        | "RENAME"
                )
                || (first == "CREATE" || first == "ALTER" || first == "DROP")
                    && class == StatementClass::Admin
                || first == "START"
                || first == "BEGIN"
        }
        ScriptDialect::Oracle => {
            matches!(class, StatementClass::Ddl)
                || matches!(first, "GRANT" | "REVOKE" | "AUDIT" | "NOAUDIT" | "ANALYZE")
                || (matches!(first, "CREATE" | "ALTER" | "DROP") && class == StatementClass::Admin)
        }
        _ => false,
    }
}

fn classify_sql(d: ScriptDialect, chars: &[char], sig: &[Sig]) -> Outcome {
    if sig.is_empty() {
        return Outcome {
            class: StatementClass::Empty,
            ..Outcome::unknown()
        };
    }
    // SQL*Plus line commands (Oracle).
    if matches!(d, ScriptDialect::Oracle) {
        let first = w(sig, 0);
        let sqlplus = matches!(
            first,
            "PROMPT"
                | "SPOOL"
                | "REM"
                | "REMARK"
                | "DEFINE"
                | "UNDEFINE"
                | "WHENEVER"
                | "SHOW"
                | "EXIT"
                | "QUIT"
                | "COLUMN"
                | "TTITLE"
                | "BTITLE"
                | "ACCEPT"
                | "CLEAR"
                | "TIMING"
        ) || (first == "SET" && is_sqlplus_set(w(sig, 1)));
        if sqlplus {
            return Outcome {
                class: StatementClass::Session,
                verb: first.to_string(),
                ..Outcome::unknown()
            };
        }
    }
    let (class, destructive) = classify_sig(d, chars, sig);
    let implicit = implicit_commit(d, sig, class);
    Outcome {
        class,
        destructive,
        implicit_commit: implicit,
        verb: verb_of(sig),
        targets: targets_of(sig),
    }
}

fn is_sqlplus_set(opt: &str) -> bool {
    matches!(
        opt,
        "SERVEROUTPUT"
            | "ECHO"
            | "FEEDBACK"
            | "LINESIZE"
            | "LINES"
            | "PAGESIZE"
            | "PAGES"
            | "VERIFY"
            | "VER"
            | "HEADING"
            | "HEA"
            | "TIMING"
            | "TIME"
            | "TRIMSPOOL"
            | "TRIMOUT"
            | "TERMOUT"
            | "DEFINE"
            | "SQLBLANKLINES"
            | "WRAP"
            | "LONG"
            | "AUTOCOMMIT"
            | "SQLPROMPT"
            | "COLSEP"
            | "NUMWIDTH"
            | "NUMFORMAT"
            | "SQLTERMINATOR"
            | "SQLT"
            | "ARRAYSIZE"
            | "TAB"
            | "NEWPAGE"
            | "UNDERLINE"
            | "SQLPLUSCOMPAT"
            | "ESCAPE"
            | "SQLNUMBER"
            | "AUTOTRACE"
            | "AUTOPRINT"
            | "PAUSE"
            | "RECSEP"
            | "LONGCHUNKSIZE"
            | "SUFFIX"
            | "CMDSEP"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use StatementClass as C;

    const PG: ScriptDialect = ScriptDialect::Postgres;
    const MY: ScriptDialect = ScriptDialect::mysql(false, true);
    const MS: ScriptDialect = ScriptDialect::MsSql;
    const ORA: ScriptDialect = ScriptDialect::Oracle;

    fn one(d: ScriptDialect, text: &str) -> StatementInfo {
        let a = analyze(d, text);
        assert_eq!(a.statements.len(), 1, "{text:?} -> {:?}", a.statements);
        a.statements.into_iter().next().unwrap()
    }

    fn cls(d: ScriptDialect, text: &str) -> (StatementClass, Option<DestructiveReason>) {
        let s = one(d, text);
        (s.class, s.destructive)
    }

    #[test]
    fn blank_and_comment_only() {
        assert!(analyze(PG, "").statements.is_empty());
        assert!(analyze(MS, " \n\t").statements.is_empty());
        let a = analyze(PG, "  -- hello\n");
        assert_eq!(a.statements.len(), 1);
        assert_eq!(a.statements[0].class, C::Empty);
        assert_eq!(a.statements[0].range, TextRange { start: 2, end: 10 });
        assert_eq!(analyze(PG, "/* x */").statements[0].class, C::Empty);
    }

    #[test]
    fn multiple_statements_have_ranges_and_indexes() {
        let a = analyze(PG, "select 1; delete from t; -- c\n drop table x");
        let v: Vec<_> = a
            .statements
            .iter()
            .map(|s| (s.index, s.range, s.class))
            .collect();
        assert_eq!(v.len(), 3);
        assert_eq!(v[0], (0, TextRange { start: 0, end: 8 }, C::Read));
        assert_eq!(v[1].0, 1);
        assert_eq!(v[2].0, 2);
        assert_eq!(
            a.statements[1].destructive,
            Some(DestructiveReason::DeleteWithoutWhere)
        );
        assert_eq!(a.statements[2].destructive, Some(DestructiveReason::Drop));
    }

    // ----- common SQL classification --------------------------------------------------------

    #[test]
    fn reads() {
        for d in [PG, MY, MS, ORA] {
            assert_eq!(cls(d, "select 1"), (C::Read, None), "{d:?}");
            assert_eq!(cls(d, "SELECT * FROM t WHERE a = 1"), (C::Read, None));
            assert_eq!(cls(d, "(select 1) union (select 2)"), (C::Read, None));
            assert_eq!(cls(d, "-- c\n select 1"), (C::Read, None));
            assert_eq!(
                cls(d, "with a as (select 1) select * from a"),
                (C::Read, None)
            );
            assert_eq!(cls(d, "select * from t for update"), (C::Write, None));
        }
        assert_eq!(cls(PG, "values (1), (2)"), (C::Read, None));
        assert_eq!(cls(PG, "table t"), (C::Read, None));
        assert_eq!(cls(PG, "show search_path"), (C::Read, None));
        assert_eq!(cls(MY, "show tables"), (C::Read, None));
        assert_eq!(cls(MY, "describe t"), (C::Read, None));
        assert_eq!(cls(MY, "desc t"), (C::Read, None));
        assert_eq!(cls(PG, "fetch 10 from c"), (C::Read, None));
    }

    #[test]
    fn writes() {
        for d in [PG, MY, MS, ORA] {
            assert_eq!(cls(d, "insert into t values (1)"), (C::Write, None));
            assert_eq!(cls(d, "update t set a = 1 where id = 2"), (C::Write, None));
            assert_eq!(cls(d, "delete from t where id = 2"), (C::Write, None));
            assert_eq!(
                cls(
                    d,
                    "merge into t using s on (t.id = s.id) when matched then update set a = 1"
                ),
                (C::Write, None)
            );
        }
        assert_eq!(cls(MY, "replace into t values (1)"), (C::Write, None));
        assert_eq!(
            cls(MY, "load data infile 'x' into table t"),
            (C::Write, None)
        );
        assert_eq!(cls(MS, "bulk insert t from 'x'"), (C::Write, None));
        assert_eq!(cls(PG, "copy t from stdin"), (C::Write, None));
        assert_eq!(cls(PG, "copy t to stdout"), (C::Read, None));
        assert_eq!(cls(PG, "copy (select 1) to stdout"), (C::Read, None));
        assert_eq!(cls(PG, "copy t to program 'x'"), (C::Admin, None));
        assert_eq!(cls(PG, "select * into newt from t"), (C::Ddl, None));
        assert_eq!(cls(MS, "select * into #t from t"), (C::Ddl, None));
        assert_eq!(
            cls(MY, "select * from t into outfile '/tmp/x'"),
            (C::Write, None)
        );
        assert_eq!(cls(MY, "select a into @v from t"), (C::Write, None));
        assert_eq!(cls(MS, "update statistics t"), (C::Admin, None));
    }

    #[test]
    fn cte_with_dml() {
        assert_eq!(
            cls(
                PG,
                "with d as (delete from t where a = 1 returning *) select * from d"
            ),
            (C::Write, None)
        );
        assert_eq!(
            cls(PG, "with d as (delete from t returning *) select * from d"),
            (C::Write, Some(DestructiveReason::DeleteWithoutWhere))
        );
        assert_eq!(
            cls(PG, "with a as (select 1) insert into t select * from a"),
            (C::Write, None)
        );
        assert_eq!(
            cls(PG, "with a as (select 1) update t set x = 1"),
            (C::Write, Some(DestructiveReason::UpdateWithoutWhere))
        );
        assert_eq!(
            cls(PG, "with a as (select 'delete') select * from a"),
            (C::Read, None)
        );
        assert_eq!(
            cls(PG, "with a as (select 1) select * from a for update"),
            (C::Write, None)
        );
    }

    #[test]
    fn destructive_dml_without_where() {
        for d in [PG, MY, MS, ORA] {
            assert_eq!(
                cls(d, "delete from t").1,
                Some(DestructiveReason::DeleteWithoutWhere),
                "{d:?}"
            );
            assert_eq!(
                cls(d, "update t set a = 1").1,
                Some(DestructiveReason::UpdateWithoutWhere)
            );
            assert_eq!(
                cls(d, "delete from t where 1=1").1,
                Some(DestructiveReason::DeleteWithoutWhere)
            );
            assert_eq!(
                cls(d, "delete from t where 2 = 2").1,
                Some(DestructiveReason::DeleteWithoutWhere)
            );
            assert_eq!(
                cls(d, "delete from t where (1=1)").1,
                Some(DestructiveReason::DeleteWithoutWhere)
            );
            assert_eq!(
                cls(d, "update t set a=1 where 1=1").1,
                Some(DestructiveReason::UpdateWithoutWhere)
            );
            assert_eq!(cls(d, "delete from t where 1=2").1, None);
            assert_eq!(cls(d, "delete from t where 1=1 and a = 2").1, None);
            assert_eq!(
                cls(d, "delete from t where a = (select max(a) from s)").1,
                None
            );
        }
        assert_eq!(
            cls(PG, "delete from t where true").1,
            Some(DestructiveReason::DeleteWithoutWhere)
        );
        assert_eq!(
            cls(PG, "delete from t where TRUE returning *").1,
            Some(DestructiveReason::DeleteWithoutWhere)
        );
        assert_eq!(
            cls(MY, "delete from t where 1").1,
            Some(DestructiveReason::DeleteWithoutWhere)
        );
        assert_eq!(
            cls(MY, "delete from t limit 10").1,
            Some(DestructiveReason::DeleteWithoutWhere)
        );
        assert_eq!(
            cls(MY, "update t set a = 1 order by id limit 1").1,
            Some(DestructiveReason::UpdateWithoutWhere)
        );
        assert_eq!(
            cls(MS, "delete top (5) from t").1,
            Some(DestructiveReason::DeleteWithoutWhere)
        );
        assert_eq!(
            cls(MS, "update t set a = 1 from t join s on s.id = t.id").1,
            Some(DestructiveReason::UpdateWithoutWhere)
        );
        assert_eq!(
            cls(
                MS,
                "update t set a = 1 from t join s on s.id = t.id where s.x = 1"
            )
            .1,
            None
        );
        assert_eq!(
            cls(MY, "insert into t values (1) on duplicate key update a = 2").1,
            None
        );
        assert_eq!(
            cls(
                PG,
                "insert into t values (1) on conflict (id) do update set a = 2"
            )
            .1,
            None
        );
        assert_eq!(
            cls(
                PG,
                "update t set a = 1 where id in (select id from s where x = 1)"
            )
            .1,
            None
        );
    }

    #[test]
    fn ddl_and_destructive() {
        for d in [PG, MY, MS, ORA] {
            assert_eq!(
                cls(d, "drop table t"),
                (C::Ddl, Some(DestructiveReason::Drop)),
                "{d:?}"
            );
            assert_eq!(
                cls(d, "drop table if exists t"),
                (C::Ddl, Some(DestructiveReason::Drop))
            );
            assert_eq!(
                cls(d, "drop index i"),
                (C::Ddl, Some(DestructiveReason::Drop))
            );
            assert_eq!(
                cls(d, "truncate table t"),
                (C::Ddl, Some(DestructiveReason::Truncate))
            );
            assert_eq!(
                cls(d, "truncate t"),
                (C::Ddl, Some(DestructiveReason::Truncate))
            );
            assert_eq!(cls(d, "create table t (a int)"), (C::Ddl, None));
            assert_eq!(cls(d, "alter table t add column b int"), (C::Ddl, None));
            assert_eq!(
                cls(d, "alter table t drop column b"),
                (C::Ddl, Some(DestructiveReason::AlterDrop))
            );
            assert_eq!(
                cls(d, "alter table t drop constraint c"),
                (C::Ddl, Some(DestructiveReason::AlterDrop))
            );
            assert_eq!(cls(d, "create index i on t (a)"), (C::Ddl, None));
            assert_eq!(
                cls(d, "create or replace view v as select 1"),
                (C::Ddl, None)
            );
        }
        assert_eq!(
            cls(MY, "drop database x"),
            (C::Ddl, Some(DestructiveReason::Drop))
        );
        assert_eq!(
            cls(PG, "drop schema s cascade"),
            (C::Ddl, Some(DestructiveReason::Drop))
        );
        assert_eq!(
            cls(PG, "alter table t alter column a drop not null"),
            (C::Ddl, None)
        );
        assert_eq!(
            cls(PG, "alter table t alter column a drop default"),
            (C::Ddl, None)
        );
        assert_eq!(cls(MY, "rename table a to b"), (C::Ddl, None));
        assert_eq!(cls(PG, "comment on table t is 'x'"), (C::Ddl, None));
        assert_eq!(
            cls(
                PG,
                "create function f() returns int as $$ select 1 $$ language sql"
            ),
            (C::Ddl, None)
        );
        assert_eq!(
            cls(
                PG,
                "create function f() returns void as $$ begin drop table x; end $$ language plpgsql"
            ),
            (C::Ddl, None)
        );
    }

    #[test]
    fn admin_statements() {
        assert_eq!(cls(PG, "grant select on t to u"), (C::Admin, None));
        assert_eq!(cls(PG, "revoke all on t from u"), (C::Admin, None));
        assert_eq!(cls(PG, "vacuum t"), (C::Admin, None));
        assert_eq!(cls(PG, "analyze t"), (C::Admin, None));
        assert_eq!(cls(PG, "reindex table t"), (C::Admin, None));
        assert_eq!(
            cls(PG, "alter system set work_mem = '1MB'"),
            (C::Admin, None)
        );
        assert_eq!(cls(PG, "create role r"), (C::Admin, None));
        assert_eq!(cls(PG, "create user u password 'x'"), (C::Admin, None));
        assert_eq!(
            cls(PG, "drop role r"),
            (C::Admin, Some(DestructiveReason::Drop))
        );
        assert_eq!(cls(MY, "set global max_connections = 10"), (C::Admin, None));
        assert_eq!(
            cls(MY, "set persist max_connections = 10"),
            (C::Admin, None)
        );
        assert_eq!(cls(MY, "set @@global.x = 1"), (C::Admin, None));
        assert_eq!(cls(MY, "set password = 'x'"), (C::Admin, None));
        assert_eq!(cls(MY, "kill 10"), (C::Admin, None));
        assert_eq!(cls(MY, "flush privileges"), (C::Admin, None));
        assert_eq!(cls(MY, "optimize table t"), (C::Admin, None));
        assert_eq!(cls(MS, "dbcc checkdb"), (C::Admin, None));
        assert_eq!(cls(MS, "backup database d to disk = 'x'"), (C::Admin, None));
        assert_eq!(cls(MS, "deny select on t to u"), (C::Admin, None));
        assert_eq!(cls(ORA, "grant select on t to u"), (C::Admin, None));
        assert_eq!(
            cls(ORA, "alter system kill session '1,2'"),
            (C::Admin, None)
        );
    }

    #[test]
    fn session_and_transactions() {
        assert_eq!(cls(PG, "set search_path = a"), (C::Session, None));
        assert_eq!(cls(PG, "set local x = 1"), (C::Session, None));
        assert_eq!(cls(PG, "reset work_mem"), (C::Session, None));
        assert_eq!(cls(PG, "reset all"), (C::Admin, None));
        assert_eq!(cls(PG, "discard all"), (C::Admin, None));
        assert_eq!(cls(PG, "discard plans"), (C::Session, None));
        assert_eq!(cls(MY, "use db"), (C::Session, None));
        assert_eq!(cls(MY, "set autocommit = 0"), (C::Session, None));
        assert_eq!(cls(MY, "set names utf8mb4"), (C::Session, None));
        assert_eq!(cls(MS, "use master"), (C::Session, None));
        assert_eq!(cls(MS, "set nocount on"), (C::Session, None));
        assert_eq!(cls(MS, "declare @a int"), (C::Session, None));
        assert_eq!(
            cls(ORA, "alter session set nls_date_format = 'x'"),
            (C::Session, None)
        );
        assert_eq!(cls(ORA, "set transaction read only"), (C::Session, None));
        let t = |d, s: &str, op| assert_eq!(cls(d, s), (C::Transaction { op }, None), "{s}");
        t(PG, "begin", TxControl::Begin);
        t(PG, "begin isolation level serializable", TxControl::Begin);
        t(PG, "start transaction", TxControl::Begin);
        t(PG, "commit", TxControl::Commit);
        t(PG, "end", TxControl::Commit);
        t(PG, "abort", TxControl::Rollback);
        t(PG, "rollback", TxControl::Rollback);
        t(PG, "savepoint a", TxControl::Savepoint);
        t(PG, "release savepoint a", TxControl::Release);
        t(
            PG,
            "rollback to savepoint a",
            TxControl::RollbackToSavepoint,
        );
        t(PG, "rollback to a", TxControl::RollbackToSavepoint);
        t(PG, "commit prepared 'x'", TxControl::Commit);
        t(MY, "begin", TxControl::Begin);
        t(MY, "start transaction", TxControl::Begin);
        t(MY, "commit", TxControl::Commit);
        t(MY, "rollback", TxControl::Rollback);
        t(MS, "begin tran", TxControl::Begin);
        t(MS, "begin transaction t1", TxControl::Begin);
        t(MS, "commit tran", TxControl::Commit);
        t(MS, "rollback transaction", TxControl::Rollback);
        t(MS, "save tran sp1", TxControl::Savepoint);
        t(ORA, "commit", TxControl::Commit);
        t(
            ORA,
            "rollback to savepoint s",
            TxControl::RollbackToSavepoint,
        );
        t(ORA, "savepoint s", TxControl::Savepoint);
    }

    #[test]
    fn procedural_statements() {
        assert_eq!(cls(PG, "call p()"), (C::Procedural, None));
        assert_eq!(cls(PG, "do $$ begin null; end $$"), (C::Procedural, None));
        assert_eq!(cls(PG, "execute stmt(1)"), (C::Procedural, None));
        assert_eq!(cls(MY, "call p()"), (C::Procedural, None));
        assert_eq!(cls(MS, "exec sp_who"), (C::Procedural, None));
        assert_eq!(cls(MS, "execute dbo.p @a = 1"), (C::Procedural, None));
        assert_eq!(cls(MS, "print 'x'"), (C::Procedural, None));
        // T-SQL needs no `;`: the condition and the statement it guards are listed separately.
        let a = analyze(MS, "if 1 = 1 select 1");
        assert_eq!(a.statements[0].class, C::Procedural);
        assert_eq!(a.statements[1].class, C::Read);
        let a = analyze(
            MS,
            "begin try select 1 end try begin catch select 2 end catch",
        );
        assert!(a.statements.iter().all(|s| s.class == C::Procedural));
        assert_eq!(cls(ORA, "begin null; end;"), (C::Procedural, None));
        assert_eq!(
            cls(ORA, "declare a number; begin a := 1; end;"),
            (C::Procedural, None)
        );
        assert_eq!(cls(ORA, "call p()"), (C::Procedural, None));
        assert_eq!(cls(ORA, "exec p"), (C::Procedural, None));
    }

    #[test]
    fn procedural_keyword_safety_net() {
        assert_eq!(
            cls(PG, "do $$ begin drop table x; end $$").1,
            Some(DestructiveReason::Drop)
        );
        assert_eq!(
            cls(PG, "do $$ begin truncate x; end $$").1,
            Some(DestructiveReason::Truncate)
        );
        assert_eq!(
            cls(PG, "do $$ begin execute 'DROP TABLE x'; end $$").1,
            Some(DestructiveReason::Drop)
        );
        assert_eq!(
            cls(MS, "exec('DROP TABLE x')").1,
            Some(DestructiveReason::Drop)
        );
        assert_eq!(
            cls(MS, "exec sp_executesql N'truncate table x'").1,
            Some(DestructiveReason::Truncate)
        );
        let a = analyze(MS, "if 1=1 drop table x");
        assert_eq!(
            a.statements.last().unwrap().destructive,
            Some(DestructiveReason::Drop)
        );
        assert_eq!(
            cls(ORA, "begin execute immediate 'drop table x'; end;").1,
            Some(DestructiveReason::Drop)
        );
        assert_eq!(cls(MY, "call p('x')").1, None);
        assert_eq!(cls(PG, "do $$ begin null; end $$").1, None);
        // Unknown statements get the same net.
        assert_eq!(
            cls(PG, "frobnicate drop table x").1,
            Some(DestructiveReason::Drop)
        );
        assert_eq!(cls(PG, "frobnicate x"), (C::Unknown, None));
    }

    #[test]
    fn explain_classification() {
        let e = |analyze, writes| C::Explain { analyze, writes };
        assert_eq!(cls(PG, "explain select 1"), (e(false, false), None));
        assert_eq!(cls(PG, "explain delete from t"), (e(false, false), None));
        assert_eq!(cls(PG, "explain analyze select 1"), (e(true, false), None));
        assert_eq!(
            cls(PG, "explain analyze delete from t"),
            (e(true, true), Some(DestructiveReason::DeleteWithoutWhere))
        );
        assert_eq!(
            cls(
                PG,
                "explain (analyze, buffers) update t set a = 1 where b = 2"
            ),
            (e(true, true), None)
        );
        assert_eq!(
            cls(PG, "explain (analyze false) update t set a = 1"),
            (e(false, false), None)
        );
        assert_eq!(
            cls(PG, "explain (analyze off, verbose) select 1"),
            (e(false, false), None)
        );
        assert_eq!(
            cls(PG, "explain analyze verbose select 1"),
            (e(true, false), None)
        );
        assert_eq!(
            cls(PG, "explain (analyze) insert into t values (1)"),
            (e(true, true), None)
        );
        assert_eq!(cls(MY, "explain select 1"), (e(false, false), None));
        assert_eq!(cls(MY, "explain analyze select 1"), (e(true, false), None));
        assert_eq!(
            cls(MY, "explain format=json select 1"),
            (e(false, false), None)
        );
        assert_eq!(cls(MY, "explain t"), (e(false, false), None));
        assert_eq!(
            cls(MY, "explain analyze delete from t where a = 1"),
            (e(true, true), None)
        );
        assert_eq!(
            cls(ORA, "explain plan for select 1 from dual"),
            (e(false, false), None)
        );
        assert_eq!(
            cls(ORA, "explain plan set statement_id = 'x' for delete from t"),
            (e(false, false), None)
        );
    }

    #[test]
    fn implicit_commit_flags() {
        let ic = |d, s: &str| one(d, s).implicit_commit;
        assert!(ic(MY, "create table t (a int)"));
        assert!(ic(MY, "alter table t add b int"));
        assert!(ic(MY, "drop table t"));
        assert!(ic(MY, "truncate t"));
        assert!(ic(MY, "rename table a to b"));
        assert!(ic(MY, "grant select on t to u"));
        assert!(ic(MY, "create user u"));
        assert!(!ic(MY, "create temporary table t (a int)"));
        assert!(!ic(MY, "drop temporary table t"));
        assert!(!ic(MY, "select 1"));
        assert!(!ic(MY, "insert into t values (1)"));
        assert!(ic(ORA, "create table t (a int)"));
        assert!(ic(ORA, "drop table t"));
        assert!(ic(ORA, "grant select on t to u"));
        assert!(!ic(ORA, "insert into t values (1)"));
        assert!(!ic(PG, "create table t (a int)"));
        assert!(!ic(MS, "drop table t"));
    }

    #[test]
    fn verbs_and_targets() {
        assert_eq!(one(PG, "select 1").verb, "SELECT");
        assert_eq!(one(PG, "alter table t add b int").verb, "ALTER TABLE");
        assert_eq!(
            one(PG, "create or replace view v as select 1").verb,
            "CREATE VIEW"
        );
        assert_eq!(
            one(PG, "create unique index i on t(a)").verb,
            "CREATE INDEX"
        );
        assert_eq!(
            one(PG, "create materialized view m as select 1").verb,
            "CREATE MATERIALIZED VIEW"
        );
        assert_eq!(one(PG, "drop table if exists t").verb, "DROP TABLE");
        assert_eq!(one(PG, "truncate t").verb, "TRUNCATE");
        assert_eq!(one(PG, "truncate table t").verb, "TRUNCATE TABLE");
        assert_eq!(one(MY, "start transaction").verb, "START TRANSACTION");
        assert_eq!(one(PG, "(select 1)").verb, "SELECT");
        let t = |d, s: &str| one(d, s).targets;
        assert_eq!(
            t(
                PG,
                "select * from public.users u join orders o on o.uid = u.id"
            ),
            ["public.users", "orders"]
        );
        assert_eq!(t(PG, "insert into \"My Table\" values (1)"), ["My Table"]);
        assert_eq!(t(PG, "update only t set a = 1"), ["t"]);
        assert_eq!(t(PG, "delete from s.t where a = 1"), ["s.t"]);
        assert_eq!(t(PG, "drop table if exists t"), ["t"]);
        assert_eq!(t(MY, "select * from `db`.`t`"), ["db.t"]);
        assert_eq!(t(MS, "select * from [dbo].[t]"), ["dbo.t"]);
        assert_eq!(
            t(PG, "select * from generate_series(1, 3)"),
            Vec::<String>::new()
        );
        assert_eq!(t(PG, "select * from (select 1) x"), Vec::<String>::new());
        assert_eq!(t(PG, "select * from t for update"), ["t"]);
        assert_eq!(t(PG, "select 1"), Vec::<String>::new());
        assert_eq!(t(PG, "select * from t, u"), ["t"]);
        assert_eq!(t(PG, "select * from t join t on 1=1"), ["t"]);
    }

    // ----- dialect specific lexing influence ------------------------------------------------

    #[test]
    fn strings_and_comments_do_not_fool_the_classifier() {
        assert_eq!(
            cls(PG, "select 'drop table x; delete from t'"),
            (C::Read, None)
        );
        assert_eq!(cls(PG, "select 1 -- drop table x"), (C::Read, None));
        assert_eq!(cls(PG, "/* drop table x */ select 1"), (C::Read, None));
        assert_eq!(cls(PG, "select \"delete\" from t").1, None);
        assert_eq!(
            cls(PG, "update t set a = 'where' ").1,
            Some(DestructiveReason::UpdateWithoutWhere)
        );
        assert_eq!(
            cls(PG, "update t set a = 1 -- where x\n").1,
            Some(DestructiveReason::UpdateWithoutWhere)
        );
        assert_eq!(
            cls(PG, "update t set \"where\" = 1").1,
            Some(DestructiveReason::UpdateWithoutWhere)
        );
    }

    #[test]
    fn lexer_problems_make_everything_unknown() {
        let a = analyze(PG, "select 1; drop table x; select 'oops");
        assert_eq!(a.problems.len(), 1);
        assert!(a.statements.iter().all(|s| s.class == C::Unknown));
        assert_eq!(a.statements[1].destructive, Some(DestructiveReason::Drop));
        let a = analyze(MY, "select 1 /* never closed");
        assert_eq!(a.problems.len(), 1);
        assert_eq!(a.statements[0].class, C::Unknown);
        assert!(analyze(PG, "select 1").problems.is_empty());
    }

    #[test]
    fn mssql_statements_inside_batches() {
        let a = analyze(MS, "declare @a int; select @a; delete from t\nGO\nselect 2");
        let v: Vec<_> = a.statements.iter().map(|s| s.class).collect();
        assert_eq!(v, [C::Session, C::Read, C::Write, C::Read]);
        assert_eq!(
            a.statements[2].destructive,
            Some(DestructiveReason::DeleteWithoutWhere)
        );
        assert_eq!(a.statements[3].index, 3);
    }

    #[test]
    fn mssql_blocks_stay_whole() {
        let a = analyze(MS, "if 1 = 1 begin delete from t; select 1; end; select 2");
        assert_eq!(a.statements.len(), 3);
        assert_eq!(a.statements[0].class, C::Procedural);
        // the BEGIN..END block is one statement, judged as a whole
        assert_eq!(a.statements[1].class, C::Procedural);
        assert_eq!(a.statements[2].class, C::Read);
        let a = analyze(
            MS,
            "begin tran; update t set a = 1 where b = 2; commit tran",
        );
        let v: Vec<_> = a.statements.iter().map(|s| s.class).collect();
        assert_eq!(
            v,
            [
                C::Transaction {
                    op: TxControl::Begin
                },
                C::Write,
                C::Transaction {
                    op: TxControl::Commit
                }
            ]
        );
        let a = analyze(MS, "select case when 1=1 then 1 else 2 end; select 2");
        assert_eq!(a.statements.len(), 2);
    }

    #[test]
    fn oracle_statements() {
        let a = analyze(
            ORA,
            "SET SERVEROUTPUT ON\nBEGIN null; END;\n/\nselect 1 from dual;\nPROMPT done",
        );
        let v: Vec<_> = a.statements.iter().map(|s| s.class).collect();
        assert_eq!(v, [C::Session, C::Procedural, C::Read, C::Session]);
        assert_eq!(
            cls(ORA, "create or replace procedure p is begin null; end;\n/").0,
            C::Ddl
        );
        assert_eq!(cls(ORA, "select * from t for update").0, C::Write);
        assert_eq!(
            cls(
                ORA,
                "insert all into a values (1) into b values (2) select 1 from dual"
            )
            .0,
            C::Write
        );
    }

    #[test]
    fn mysql_delimiter_scripts() {
        let a = analyze(
            MY,
            "DELIMITER $$\nCREATE PROCEDURE p() BEGIN DELETE FROM t; END$$\nDELIMITER ;\nCALL p();",
        );
        assert_eq!(a.statements.len(), 2);
        assert_eq!(a.statements[0].class, C::Ddl);
        assert_eq!(a.statements[0].destructive, None);
        assert_eq!(a.statements[1].class, C::Procedural);
    }

    #[test]
    fn params_are_reported() {
        let a = analyze(PG, "select :a, $1");
        assert_eq!(a.params.len(), 2);
        assert_eq!(a.dialect, PG);
    }

    #[test]
    fn unicode_ranges() {
        let a = analyze(PG, "select 'é😀'; delete from t");
        assert_eq!(a.statements[1].range.start, 13);
    }

    // ----- Opaque / Redis / Mongo -----------------------------------------------------------

    #[test]
    fn opaque() {
        let a = analyze(ScriptDialect::Opaque, "anything; goes");
        assert_eq!(a.statements.len(), 1);
        assert_eq!(a.statements[0].class, C::Unknown);
        assert_eq!(a.statements[0].destructive, None);
        assert_eq!(
            analyze(ScriptDialect::Opaque, "x; DROP y").statements[0].destructive,
            Some(DestructiveReason::Drop)
        );
        assert!(analyze(ScriptDialect::Opaque, "  ").statements.is_empty());
    }

    #[test]
    fn redis_scripts() {
        let a = analyze(
            ScriptDialect::RedisCli,
            "GET a\nSET a 1\nFLUSHDB\nconfig set maxmemory 1\nFOO x",
        );
        let v: Vec<_> = a
            .statements
            .iter()
            .map(|s| (s.class, s.destructive))
            .collect();
        assert_eq!(
            v,
            [
                (C::Read, None),
                (C::Write, None),
                (C::Write, Some(DestructiveReason::FlushDb)),
                (C::Admin, None),
                (C::Unknown, None)
            ]
        );
        assert_eq!(a.statements[0].verb, "GET");
        assert_eq!(a.statements[0].targets, ["a"]);
        assert_eq!(a.statements[3].verb, "CONFIG SET");
        assert_eq!(a.statements[4].verb, "FOO");
        let a = analyze(ScriptDialect::RedisCli, "GET a\nSET a \"oops");
        assert_eq!(a.problems.len(), 1);
        assert!(a.statements.iter().all(|s| s.class == C::Unknown));
    }

    #[test]
    fn mongo_scripts() {
        let a = analyze(
            ScriptDialect::MongoShell,
            "use shop\ndb.users.find({})\ndb.users.deleteMany({})\ndb.users.drop()",
        );
        let v: Vec<_> = a
            .statements
            .iter()
            .map(|s| (s.class, s.destructive))
            .collect();
        assert_eq!(
            v,
            [
                (C::Session, None),
                (C::Read, None),
                (C::Write, Some(DestructiveReason::DeleteManyAll)),
                (C::Ddl, Some(DestructiveReason::DropCollection)),
            ]
        );
        assert_eq!(a.statements[1].verb, "db.users.find");
        assert_eq!(a.statements[1].targets, ["users"]);
    }

    #[test]
    fn mongo_parse_error_is_a_problem() {
        let a = analyze(ScriptDialect::MongoShell, "db.a.find({\ndb.b.drop()");
        assert_eq!(a.problems.len(), 1);
        assert!(a.statements.iter().all(|s| s.class == C::Unknown));
        assert!(
            a.statements
                .iter()
                .any(|s| s.destructive == Some(DestructiveReason::DropCollection))
        );
    }
}
