//! Statement splitting.
//!
//! * PostgreSQL: top level `;` (strings, dollar quotes, comments, `BEGIN ATOMIC` bodies respected).
//! * MySQL / MariaDB: `;` or the delimiter set by `DELIMITER xx` lines.
//! * T-SQL: batches separated by `GO [n]` lines ([`split`] returns one range per batch because
//!   variables are batch scoped; [`split_batches`] also gives the repeat count).
//! * Oracle: `;` for SQL, PL/SQL blocks end at a line holding only `/`, SQL*Plus commands
//!   (`SET SERVEROUTPUT`, `PROMPT`, `SPOOL`...) end at the end of the line.
//! * Mongo shell: `;` or a newline outside brackets; Redis: one statement per line.
//! * Opaque: the whole text.
//!
//! Ranges never include the terminator and are trimmed of surrounding whitespace; statements
//! with nothing but whitespace and comments are dropped.

use dbstudio_core::{ScriptDialect, TextRange};

use crate::lexer::{Lexer, TokenKind, to_u32};

/// A statement (or T-SQL batch) found by the scanner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unit {
    /// Range of the statement.
    pub range: TextRange,
    /// `GO n` repeat count (1 for everything else).
    pub repeat: u32,
}

/// Splitter state that must survive between [`StreamSplitter::push`] calls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SplitState {
    /// Active MySQL delimiter.
    delimiter: Vec<char>,
    /// The next character is at the start of a line (only blanks before it on its line).
    line_start: bool,
}

impl Default for SplitState {
    fn default() -> Self {
        SplitState {
            delimiter: vec![';'],
            line_start: true,
        }
    }
}

/// Ranges of the statements of `text` (terminators and `GO` lines excluded).
pub fn split(d: ScriptDialect, text: &str) -> Vec<TextRange> {
    split_units(d, text).into_iter().map(|u| u.range).collect()
}

/// Like [`split`] but keeps the `GO n` repeat count of T-SQL batches.
pub fn split_batches(d: ScriptDialect, text: &str) -> Vec<Unit> {
    split_units(d, text)
}

fn split_units(d: ScriptDialect, text: &str) -> Vec<Unit> {
    let chars: Vec<char> = text.chars().collect();
    let mut st = SplitState::default();
    scan(d, &chars, &mut st, true).0
}

/// Scan `chars`; with `eof == false` only statements closed by an explicit terminator are
/// returned. The second value is the number of characters consumed (everything before it is
/// final and can be dropped by a streaming caller).
pub(crate) fn scan(
    d: ScriptDialect,
    chars: &[char],
    st: &mut SplitState,
    eof: bool,
) -> (Vec<Unit>, usize) {
    let (mut units, consumed) = match d {
        ScriptDialect::Opaque => scan_opaque(chars, eof),
        ScriptDialect::RedisCli => scan_lines(chars, eof),
        ScriptDialect::MongoShell => scan_mongo(chars, eof),
        _ => {
            let r = scan_sql(d, chars, st, eof);
            if r.1 > 0 {
                st.line_start = chars[r.1 - 1] == '\n';
            }
            r
        }
    };
    // Unterminated strings / comments run to the end of the text and may swallow trailing
    // whitespace: ranges are always trimmed.
    for u in &mut units {
        let mut e = u.range.end as usize;
        while e > u.range.start as usize + 1 && chars[e - 1].is_whitespace() {
            e -= 1;
        }
        u.range.end = to_u32(e);
    }
    (units, consumed)
}

fn unit(start: usize, end: usize, repeat: u32) -> Unit {
    Unit {
        range: TextRange {
            start: to_u32(start),
            end: to_u32(end),
        },
        repeat,
    }
}

fn scan_opaque(chars: &[char], eof: bool) -> (Vec<Unit>, usize) {
    if !eof {
        return (Vec::new(), 0);
    }
    let start = chars.iter().position(|c| !c.is_whitespace());
    match start {
        None => (Vec::new(), chars.len()),
        Some(s) => {
            let e = chars.iter().rposition(|c| !c.is_whitespace()).unwrap_or(s) + 1;
            (vec![unit(s, e, 1)], chars.len())
        }
    }
}

/// One statement per non blank line.
fn scan_lines(chars: &[char], eof: bool) -> (Vec<Unit>, usize) {
    let mut out = Vec::new();
    let mut line_start = 0;
    let n = chars.len();
    let mut consumed = 0;
    while line_start < n {
        let nl = chars[line_start..].iter().position(|&c| c == '\n');
        let (line_end, next) = match nl {
            Some(p) => (line_start + p, line_start + p + 1),
            None if eof => (n, n),
            None => break,
        };
        let line = &chars[line_start..line_end];
        if let Some(s) = line.iter().position(|c| !c.is_whitespace()) {
            let e = line.iter().rposition(|c| !c.is_whitespace()).unwrap_or(s) + 1;
            out.push(unit(line_start + s, line_start + e, 1));
        }
        consumed = next;
        line_start = next;
    }
    (out, consumed)
}

/// Mongo shell statements: `;` at depth 0, or a newline at depth 0 unless the statement
/// continues with a `.chain` on the next line (or the line ends with `.`/`,`).
fn scan_mongo(chars: &[char], eof: bool) -> (Vec<Unit>, usize) {
    let lx = Lexer::new(chars, ScriptDialect::MongoShell);
    let n = chars.len();
    let mut out = Vec::new();
    let mut consumed = 0;
    let mut i = 0;
    let mut depth: i32 = 0;
    let mut start: Option<usize> = None;
    let mut last_end = 0;
    let mut last_sig: Option<char> = None;
    while i < n {
        let (kind, end, _) = lx.next(i);
        match kind {
            TokenKind::Whitespace => {
                let has_nl = chars[i..end].contains(&'\n');
                if has_nl && depth <= 0 && start.is_some() && !matches!(last_sig, Some('.' | ',')) {
                    // Look ahead to the next significant token.
                    let mut j = end;
                    let mut next_sig: Option<char> = None;
                    while j < n {
                        let (k, e, _) = lx.next(j);
                        if !k.is_trivia() {
                            next_sig = Some(chars[j]);
                            break;
                        }
                        j = e;
                    }
                    match next_sig {
                        None if !eof => break,
                        Some('.') => {}
                        _ => {
                            if let Some(s) = start.take() {
                                out.push(unit(s, last_end, 1));
                            }
                            consumed = end;
                            last_sig = None;
                            depth = 0;
                        }
                    }
                }
            }
            TokenKind::LineComment | TokenKind::BlockComment => {
                if start.is_none() {
                    start = Some(i);
                }
                last_end = end;
            }
            _ => {
                let c = chars[i];
                if kind == TokenKind::Punct && c == ';' && depth <= 0 {
                    // A `;` always ends the statement; it may follow only comments.
                    let has_code = start.is_some();
                    if let Some(s) = start.take() {
                        if has_code {
                            out.push(unit(s, last_end, 1));
                        }
                    }
                    consumed = end;
                    depth = 0;
                    last_sig = None;
                } else {
                    if start.is_none() {
                        start = Some(i);
                    }
                    if kind == TokenKind::Punct {
                        match c {
                            '(' | '[' | '{' => depth += 1,
                            ')' | ']' | '}' => depth -= 1,
                            _ => {}
                        }
                    }
                    last_end = end;
                    last_sig = Some(chars[end - 1]);
                }
            }
        }
        i = end;
    }
    if eof {
        if let Some(s) = start.take() {
            out.push(unit(s, last_end, 1));
        }
        consumed = n;
    }
    // Comment-only statements are dropped: a statement is only pushed when it has code.
    out.retain(|u| has_code(chars, ScriptDialect::MongoShell, u.range));
    (out, consumed)
}

fn has_code(chars: &[char], d: ScriptDialect, r: TextRange) -> bool {
    let lx = Lexer::new(chars, d);
    let mut i = r.start as usize;
    let end = r.end as usize;
    while i < end {
        let (k, e, _) = lx.next(i);
        if !k.is_trivia() {
            return true;
        }
        i = e;
    }
    false
}

// ---------------------------------------------------------------------------------------------
// SQL dialects
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct Cur {
    start: Option<usize>,
    last_end: usize,
    has_code: bool,
    /// Upper-cased leading words of the statement (at most 10).
    lead: Vec<String>,
    /// PostgreSQL: words of a `CREATE` statement, for the `BEGIN ATOMIC` check.
    words: Vec<String>,
    /// The statement starts with `CREATE` (so `words` is being collected).
    collect: bool,
}

impl Cur {
    fn touch(&mut self, i: usize, end: usize) {
        if self.start.is_none() {
            self.start = Some(i);
        }
        self.last_end = end;
    }

    fn take(&mut self, repeat: u32, out: &mut Vec<Unit>) {
        if self.has_code {
            if let Some(s) = self.start {
                out.push(unit(s, self.last_end, repeat));
            }
        }
        *self = Cur::default();
    }
}

fn is_line_blank_after(chars: &[char], from: usize) -> Option<usize> {
    // Index just after the newline ending the line, if the rest of the line is blank.
    let mut j = from;
    while j < chars.len() && chars[j] != '\n' {
        if !chars[j].is_whitespace() {
            return None;
        }
        j += 1;
    }
    Some(if j < chars.len() { j + 1 } else { j })
}

/// T-SQL `GO [n]` line starting at `i`. Returns `(index after the line, count, line_complete)`.
fn match_go(chars: &[char], i: usize) -> Option<(usize, u32, bool)> {
    let n = chars.len();
    if i + 1 >= n
        || !matches!(chars[i], 'g' | 'G')
        || !matches!(chars[i + 1], 'o' | 'O')
        || chars
            .get(i + 2)
            .is_some_and(|c| c.is_alphanumeric() || *c == '_')
    {
        return None;
    }
    let mut j = i + 2;
    while j < n && matches!(chars[j], ' ' | '\t') {
        j += 1;
    }
    let mut count: Option<u32> = None;
    let ds = j;
    while j < n && chars[j].is_ascii_digit() {
        j += 1;
    }
    if j > ds {
        let s: String = chars[ds..j].iter().collect();
        count = Some(s.parse().unwrap_or(u32::MAX));
    }
    // blanks and `/* ... */` comments that end on the same line
    loop {
        while j < n && matches!(chars[j], ' ' | '\t') {
            j += 1;
        }
        if j + 1 < n && chars[j] == '/' && chars[j + 1] == '*' {
            let mut k = j + 2;
            while k + 1 < n && !(chars[k] == '*' && chars[k + 1] == '/') && chars[k] != '\n' {
                k += 1;
            }
            if k + 1 < n && chars[k] == '*' && chars[k + 1] == '/' {
                j = k + 2;
                continue;
            }
        }
        break;
    }
    if j + 1 < n && chars[j] == '-' && chars[j + 1] == '-' {
        while j < n && chars[j] != '\n' {
            j += 1;
        }
    }
    if j < n && chars[j] == '\r' {
        j += 1;
    }
    if j < n && chars[j] != '\n' {
        return None;
    }
    let complete = j < n;
    let next = if j < n { j + 1 } else { j };
    Some((next, count.unwrap_or(1), complete))
}

/// Oracle / SQL*Plus line holding only `/`. Returns `(index after the line, complete)`.
fn match_slash(chars: &[char], i: usize) -> Option<(usize, bool)> {
    if chars.get(i) != Some(&'/') {
        return None;
    }
    let next = is_line_blank_after(chars, i + 1)?;
    let complete = next > 0 && (next > i + 1) && chars[next - 1] == '\n';
    Some((next, complete))
}

fn matches_ci(chars: &[char], i: usize, word: &str) -> bool {
    let w: Vec<char> = word.chars().collect();
    if i + w.len() > chars.len() {
        return false;
    }
    chars[i..i + w.len()]
        .iter()
        .zip(&w)
        .all(|(a, b)| a.eq_ignore_ascii_case(b))
}

/// End of the line starting the search at `from`: `(index of the newline or len, next line)`.
fn line_end(chars: &[char], from: usize) -> (usize, usize) {
    let mut j = from;
    while j < chars.len() && chars[j] != '\n' {
        j += 1;
    }
    (j, if j < chars.len() { j + 1 } else { j })
}

fn trim_end_idx(chars: &[char], start: usize, mut end: usize) -> usize {
    while end > start && (chars[end - 1].is_whitespace() || chars[end - 1] == ';') {
        end -= 1;
    }
    end
}

const SQLPLUS_COMMANDS: &[&str] = &[
    "PROMPT", "SPOOL", "REM", "REMARK", "DEFINE", "UNDEFINE", "WHENEVER", "SHOW", "EXIT", "QUIT",
    "COLUMN", "TTITLE", "BTITLE", "ACCEPT", "CLEAR", "TIMING",
];

const SQLPLUS_SET_OPTIONS: &[&str] = &[
    "SERVEROUTPUT",
    "ECHO",
    "FEEDBACK",
    "LINESIZE",
    "LINES",
    "PAGESIZE",
    "PAGES",
    "VERIFY",
    "VER",
    "HEADING",
    "HEA",
    "TIMING",
    "TIME",
    "TRIMSPOOL",
    "TRIMOUT",
    "TERMOUT",
    "DEFINE",
    "SQLBLANKLINES",
    "WRAP",
    "LONG",
    "AUTOCOMMIT",
    "SQLPROMPT",
    "COLSEP",
    "NUMWIDTH",
    "NUMFORMAT",
    "SQLTERMINATOR",
    "SQLT",
    "ARRAYSIZE",
    "TAB",
    "NEWPAGE",
    "UNDERLINE",
    "SQLPLUSCOMPAT",
    "ESCAPE",
    "SQLNUMBER",
    "AUTOTRACE",
    "AUTOPRINT",
    "PAUSE",
    "RECSEP",
    "LONGCHUNKSIZE",
    "SUFFIX",
    "CMDSEP",
];

/// Whether an Oracle statement with these leading words is a PL/SQL block (ends at `/`).
fn oracle_block(lead: &[String]) -> bool {
    let w: Vec<&str> = lead.iter().map(String::as_str).collect();
    match w.first().copied() {
        Some("DECLARE" | "BEGIN") => true,
        Some("CREATE") => {
            let mut i = 1;
            if w.get(i) == Some(&"OR") && w.get(i + 1) == Some(&"REPLACE") {
                i += 2;
            }
            if matches!(w.get(i).copied(), Some("EDITIONABLE" | "NONEDITIONABLE")) {
                i += 1;
            }
            match w.get(i).copied() {
                Some("FUNCTION" | "PROCEDURE" | "PACKAGE" | "TRIGGER") => true,
                Some("TYPE") => {
                    let mut k = i + 1;
                    if w.get(k) == Some(&"BODY") {
                        return true;
                    }
                    // `CREATE TYPE t AS TABLE OF ..` / `AS VARRAY(..)` end with `;`.
                    k += 1; // name
                    !matches!(
                        (w.get(k).copied(), w.get(k + 1).copied()),
                        (Some("AS" | "IS"), Some("TABLE" | "VARRAY" | "VARYING"))
                    )
                }
                _ => false,
            }
        }
        _ => false,
    }
}

/// PostgreSQL: `CREATE FUNCTION/PROCEDURE ... BEGIN ATOMIC` body that has not reached its
/// closing `END` yet.
fn open_atomic_body(w: &[String]) -> bool {
    if w.first().map(String::as_str) != Some("CREATE") {
        return false;
    }
    if !w
        .iter()
        .take(6)
        .any(|x| matches!(x.as_str(), "FUNCTION" | "PROCEDURE"))
    {
        return false;
    }
    let Some(atomic) = w
        .windows(2)
        .position(|p| p[0] == "BEGIN" && p[1] == "ATOMIC")
    else {
        return false;
    };
    let mut depth = 0i32;
    for x in &w[atomic..] {
        match x.as_str() {
            "BEGIN" | "CASE" => depth += 1,
            "END" => depth -= 1,
            _ => {}
        }
    }
    depth > 0
}

fn scan_sql(
    d: ScriptDialect,
    chars: &[char],
    st: &mut SplitState,
    eof: bool,
) -> (Vec<Unit>, usize) {
    let lx = Lexer::new(chars, d);
    let n = chars.len();
    let is_mysql = matches!(d, ScriptDialect::MySql { .. });
    let is_mssql = matches!(d, ScriptDialect::MsSql);
    let is_oracle = matches!(d, ScriptDialect::Oracle);
    let is_pg = matches!(d, ScriptDialect::Postgres);
    let is_cql = matches!(d, ScriptDialect::Cql);
    // `;` ends a statement (PostgreSQL, CQL, ClickHouse).
    let semi = is_pg || is_cql || matches!(d, ScriptDialect::ClickHouse);
    // CQL `BEGIN [UNLOGGED|COUNTER] BATCH ... APPLY BATCH` is one statement.
    let mut in_batch = false;
    let mut prev_word = String::new();
    let mut out = Vec::new();
    let mut consumed = 0usize;
    let mut cur = Cur::default();
    let mut i = 0usize;
    let mut line_start = st.line_start;

    while i < n {
        if line_start {
            if is_mssql {
                if let Some((next, count, complete)) = match_go(chars, i) {
                    if !complete && !eof {
                        break;
                    }
                    cur.take(count.max(1), &mut out);
                    i = next;
                    consumed = i;
                    continue;
                }
            }
            if is_oracle {
                if let Some((next, complete)) = match_slash(chars, i) {
                    if !complete && !eof {
                        break;
                    }
                    cur.take(1, &mut out);
                    i = next;
                    consumed = i;
                    continue;
                }
            }
            if is_mysql
                && cur.start.is_none()
                && matches_ci(chars, i, "delimiter")
                && chars.get(i + 9).is_some_and(|c| *c == ' ' || *c == '\t')
            {
                let (e, next) = line_end(chars, i + 9);
                if e >= n && !eof {
                    break;
                }
                let arg: String = chars[i + 9..e].iter().collect();
                let arg = arg.split_whitespace().next().unwrap_or("");
                if !arg.is_empty() {
                    st.delimiter = arg.chars().collect();
                }
                i = next;
                consumed = i;
                continue;
            }
        }

        // MySQL custom (or default) delimiter at a token start.
        if is_mysql && chars[i..].starts_with(&st.delimiter) {
            cur.take(1, &mut out);
            i += st.delimiter.len();
            consumed = i;
            line_start = false;
            continue;
        }

        let (kind, mut end, _) = lx.next(i);
        if is_mysql && matches!(kind, TokenKind::Word | TokenKind::Number) && st.delimiter.len() > 1
        {
            // `END$$` : the delimiter wins over the identifier characters.
            let dl = st.delimiter.len();
            let mut k = i + 1;
            while k + dl <= end {
                if chars[k..k + dl] == st.delimiter[..] {
                    end = k;
                    break;
                }
                k += 1;
            }
        }
        match kind {
            TokenKind::Whitespace => {
                if chars[i..end].contains(&'\n') {
                    line_start = true;
                }
                // cqlsh commands (`CONSISTENCY ALL`, `USE ks`, `DESCRIBE TABLES`...) end at the end
                // of the line, no `;` needed.
                if is_cql
                    && cur.has_code
                    && !in_batch
                    && cql_line_command(&cur.lead)
                    && chars[i..end].iter().any(|c| *c == '\n' || *c == '\r')
                {
                    cur.take(1, &mut out);
                    prev_word.clear();
                    consumed = end;
                }
            }
            TokenKind::LineComment | TokenKind::BlockComment => {
                cur.touch(i, end);
                // A comment keeps "line start" only when it ends the line (line comment).
            }
            _ => {
                line_start = false;
                if semi
                    && kind == TokenKind::Punct
                    && chars[i] == ';'
                    && !(is_pg && open_atomic_body(&cur.words))
                    && !in_batch
                {
                    cur.take(1, &mut out);
                    prev_word.clear();
                    i = end;
                    consumed = i;
                    continue;
                }
                if is_oracle
                    && kind == TokenKind::Punct
                    && chars[i] == ';'
                    && !oracle_block(&cur.lead)
                {
                    // `;` ends SQL but not PL/SQL blocks.
                    cur.take(1, &mut out);
                    i = end;
                    consumed = i;
                    continue;
                }
                if is_pg {
                    // Words of a CREATE statement, for the `BEGIN ATOMIC` check. Literals and
                    // quoted identifiers count as one opaque word, like in the legacy splitter.
                    let word: Option<String> = match kind {
                        TokenKind::Word => {
                            Some(chars[i..end].iter().collect::<String>().to_uppercase())
                        }
                        TokenKind::String | TokenKind::QuotedIdent | TokenKind::Number => {
                            Some("_".to_string())
                        }
                        _ => None,
                    };
                    if !cur.has_code {
                        cur.collect = word.as_deref() == Some("CREATE");
                    }
                    if cur.collect {
                        cur.words.extend(word);
                    }
                }
                cur.has_code = true;
                cur.touch(i, end);
                if kind == TokenKind::Word {
                    let w: String = chars[i..end].iter().collect::<String>().to_uppercase();
                    if is_cql && w == "BATCH" {
                        if cur.lead.first().map(String::as_str) == Some("BEGIN")
                            && matches!(prev_word.as_str(), "BEGIN" | "UNLOGGED" | "COUNTER")
                        {
                            in_batch = true;
                        } else if prev_word == "APPLY" {
                            in_batch = false;
                        }
                    }
                    prev_word.clone_from(&w);
                    if cur.lead.len() < 10 {
                        cur.lead.push(w);
                    }
                    if is_oracle {
                        match oracle_command_check(chars, &mut cur, end, eof, &mut out) {
                            Check::Wait => break,
                            Check::Consumed(next) => {
                                i = next;
                                consumed = i;
                                line_start = true;
                                continue;
                            }
                            Check::No => {}
                        }
                    }
                }
            }
        }
        i = end;
    }
    if eof {
        cur.take(1, &mut out);
        consumed = n;
    }
    (out, consumed)
}

/// First words of a cqlsh shell command, which ends at the end of its line.
const CQL_LINE_COMMANDS: &[&str] = &[
    "CONSISTENCY",
    "SERIAL",
    "TRACING",
    "PAGING",
    "USE",
    "DESCRIBE",
    "DESC",
    "COPY",
    "SOURCE",
    "EXPAND",
    "CAPTURE",
    "CLEAR",
    "CLS",
    "LOGIN",
    "SHOW",
    "UNICODE",
];

fn cql_line_command(lead: &[String]) -> bool {
    lead.first()
        .is_some_and(|w| CQL_LINE_COMMANDS.contains(&w.as_str()))
}

enum Check {
    No,
    Wait,
    Consumed(usize),
}

/// Oracle: after a word was pushed to `cur.lead`, detect a SQL*Plus line command. When found
/// the statement is emitted covering the whole line and `cur` is reset.
fn oracle_command_check(
    chars: &[char],
    cur: &mut Cur,
    word_end: usize,
    eof: bool,
    out: &mut Vec<Unit>,
) -> Check {
    let is_cmd = match cur.lead.as_slice() {
        [w] => SQLPLUS_COMMANDS.contains(&w.as_str()),
        [s, o] => s == "SET" && SQLPLUS_SET_OPTIONS.contains(&o.as_str()),
        _ => false,
    };
    if !is_cmd {
        return Check::No;
    }
    let (e, next) = line_end(chars, word_end);
    if e >= chars.len() && !eof {
        return Check::Wait;
    }
    let start = cur.start.unwrap_or(word_end);
    let end = trim_end_idx(chars, start, e);
    out.push(unit(start, end.max(word_end), 1));
    *cur = Cur::default();
    Check::Consumed(next)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PG: ScriptDialect = ScriptDialect::Postgres;
    const MY: ScriptDialect = ScriptDialect::mysql(false, true);
    const MS: ScriptDialect = ScriptDialect::MsSql;
    const ORA: ScriptDialect = ScriptDialect::Oracle;

    fn texts(d: ScriptDialect, text: &str) -> Vec<String> {
        let chars: Vec<char> = text.chars().collect();
        split(d, text)
            .into_iter()
            .map(|r| chars[r.start as usize..r.end as usize].iter().collect())
            .collect()
    }

    fn stream(d: ScriptDialect, text: &str, chunk: usize) -> Vec<String> {
        let mut s = StreamSplitter::new(d);
        let mut out = Vec::new();
        let chars: Vec<char> = text.chars().collect();
        for c in chars.chunks(chunk) {
            let part: String = c.iter().collect();
            out.extend(s.push(&part));
        }
        out.extend(s.finish());
        out.into_iter().map(|t| t.trim().to_string()).collect()
    }

    // ----- PostgreSQL corpus (same cases as the legacy driver splitter) ------------------

    #[test]
    fn pg_basic_corpus() {
        let t = texts(
            PG,
            "select 1; select ';' ; -- c;\n select \"a;b\"; /* x; */ ;  ",
        );
        assert_eq!(t, ["select 1", "select ';'", "-- c;\n select \"a;b\""]);
    }

    #[test]
    fn pg_dollar_quotes() {
        let t = texts(
            PG,
            "do $$ begin perform 1; end $$; select $a$;$a$; select 2",
        );
        assert_eq!(t.len(), 3);
        assert_eq!(t[2], "select 2");
        assert_eq!(t[0], "do $$ begin perform 1; end $$");
    }

    #[test]
    fn pg_escape_strings() {
        assert_eq!(texts(PG, "select E'it\\'s;'; select 'it''s;'").len(), 2);
    }

    #[test]
    fn pg_offsets_are_characters() {
        let r = split(PG, "select 1;\nselect 'é'; select 3");
        assert_eq!(r[1].start, 10);
        assert_eq!(
            r[2].start as usize,
            "select 1;\nselect 'é'; ".chars().count()
        );
    }

    #[test]
    fn pg_only_comments_is_empty() {
        assert!(split(PG, "  -- nothing\n /* at all */ ; ").is_empty());
        assert!(split(PG, "").is_empty());
        assert!(split(PG, " \n").is_empty());
        assert_eq!(texts(PG, "select $1").len(), 1);
    }

    #[test]
    fn pg_begin_atomic() {
        let t = texts(
            PG,
            "CREATE FUNCTION f(a int) RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT CASE WHEN a > 1 THEN 1 ELSE 2 END; SELECT a; END; SELECT 2; \
             CREATE PROCEDURE p() BEGIN ATOMIC SELECT 1; END;",
        );
        assert_eq!(t.len(), 3, "{t:?}");
        assert!(t[0].ends_with("END"));
        assert_eq!(t[1], "SELECT 2");
    }

    #[test]
    fn pg_nested_comments() {
        assert_eq!(
            texts(PG, "select 1 /* ; /* nested ; */ ; */ ; select 2").len(),
            2
        );
    }

    #[test]
    fn pg_last_statement_without_terminator() {
        assert_eq!(texts(PG, "select 1;select 2"), ["select 1", "select 2"]);
        assert_eq!(texts(PG, "select 1;;;select 2;"), ["select 1", "select 2"]);
    }

    #[test]
    fn pg_unterminated_string_swallows_rest() {
        assert_eq!(texts(PG, "select 'a; select 2").len(), 1);
    }

    #[test]
    fn range_is_trimmed() {
        let r = split(PG, "  select 1  ;  ");
        assert_eq!(r, vec![TextRange { start: 2, end: 10 }]);
    }

    // ----- MySQL --------------------------------------------------------------------------

    #[test]
    fn mysql_semicolons_and_backslashes() {
        assert_eq!(texts(MY, "select 'a\\';b'; select 2").len(), 2);
        assert_eq!(texts(MY, "select 1; # c;\n select `a;b`").len(), 2);
    }

    #[test]
    fn mysql_delimiter_command() {
        let sql = "DELIMITER $$\nCREATE PROCEDURE p()\nBEGIN\n  select 1;\n  select 2;\nEND$$\nDELIMITER ;\nselect 3;";
        let t = texts(MY, sql);
        assert_eq!(t.len(), 2, "{t:?}");
        assert!(t[0].starts_with("CREATE PROCEDURE"));
        assert!(t[0].ends_with("END"));
        assert_eq!(t[1], "select 3");
    }

    #[test]
    fn mysql_delimiter_double_slash_and_case() {
        let sql = "delimiter //\ncreate trigger t before insert on a for each row begin set @x=1; end//\ndelimiter ;\nselect 1";
        let t = texts(MY, sql);
        assert_eq!(t.len(), 2, "{t:?}");
        assert!(t[0].ends_with("end"));
    }

    #[test]
    fn mysql_delimiter_inside_string_is_ignored() {
        let t = texts(MY, "DELIMITER $$\nselect '$$' $$\nselect 2$$");
        assert_eq!(t, ["select '$$'", "select 2"]);
    }

    // ----- T-SQL --------------------------------------------------------------------------

    #[test]
    fn mssql_go_batches() {
        let t = texts(MS, "select 1\nGO\nselect 2\ngo\n  select 3");
        assert_eq!(t, ["select 1", "select 2", "select 3"]);
    }

    #[test]
    fn mssql_go_with_count_and_comment() {
        let u = split_batches(MS, "insert into t values (1)\nGO 5 -- five\nselect 1\nGO");
        assert_eq!(u.len(), 2);
        assert_eq!(u[0].repeat, 5);
        assert_eq!(u[1].repeat, 1);
    }

    #[test]
    fn mssql_go_may_be_followed_by_comments() {
        assert_eq!(
            texts(MS, "select 1\nGO /* c */\ndrop table t"),
            ["select 1", "drop table t"]
        );
        assert_eq!(
            texts(MS, "select 1\ngo 2 /* a */ /* b */ -- c\nselect 2").len(),
            2
        );
        let u = split_batches(MS, "select 1\nGO 3 /* c */\nselect 2");
        assert_eq!(u[0].repeat, 3);
        // a comment that spans lines is not part of the GO line
        assert_eq!(texts(MS, "select 1\nGO /* a\nb */\nselect 2").len(), 1);
    }

    #[test]
    fn mssql_go_must_be_alone() {
        assert_eq!(texts(MS, "select 1 go\nselect 2").len(), 1);
        assert_eq!(texts(MS, "goto x\nselect 2").len(), 1);
        assert_eq!(texts(MS, "select '\nGO\n'").len(), 1);
        assert_eq!(texts(MS, "/* a\nGO\n*/ select 1").len(), 1);
    }

    #[test]
    fn mssql_semicolons_do_not_split_batches() {
        let t = texts(MS, "declare @a int; select @a;\nGO");
        assert_eq!(t, ["declare @a int; select @a;"]);
    }

    #[test]
    fn mssql_empty_batches_dropped() {
        assert!(split(MS, "GO\nGO\n-- c\nGO").is_empty());
    }

    // ----- Oracle -------------------------------------------------------------------------

    #[test]
    fn oracle_sql_with_semicolons() {
        assert_eq!(
            texts(ORA, "select 1 from dual; select 2 from dual;").len(),
            2
        );
    }

    #[test]
    fn oracle_plsql_block_ends_at_slash() {
        let sql = "BEGIN\n  x := 1;\n  y := 2;\nEND;\n/\nselect 1 from dual;";
        let t = texts(ORA, sql);
        assert_eq!(t.len(), 2, "{t:?}");
        assert!(t[0].starts_with("BEGIN") && t[0].ends_with("END;"));
    }

    #[test]
    fn oracle_create_procedure() {
        let sql = "CREATE OR REPLACE PROCEDURE p IS\nBEGIN\n  null;\nEND;\n/\nCREATE TABLE t (a int);\nCREATE OR REPLACE TRIGGER tr BEFORE INSERT ON t BEGIN null; END;\n/";
        let t = texts(ORA, sql);
        assert_eq!(t.len(), 3, "{t:?}");
        assert!(t[1].starts_with("CREATE TABLE"));
    }

    #[test]
    fn oracle_package_body_and_type() {
        let sql = "CREATE PACKAGE BODY pk AS\n procedure a is begin null; end;\nEND pk;\n/\nCREATE TYPE t AS TABLE OF NUMBER;\nselect 1 from dual;";
        let t = texts(ORA, sql);
        assert_eq!(t.len(), 3, "{t:?}");
        assert_eq!(t[1], "CREATE TYPE t AS TABLE OF NUMBER");
    }

    #[test]
    fn oracle_declare_block_without_trailing_slash() {
        let t = texts(ORA, "DECLARE a number; BEGIN a := 1; END;");
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn oracle_sqlplus_commands_are_line_statements() {
        let t = texts(
            ORA,
            "SET SERVEROUTPUT ON\nPROMPT it's done\nSPOOL out.txt;\nselect 1 from dual;",
        );
        assert_eq!(
            t,
            [
                "SET SERVEROUTPUT ON",
                "PROMPT it's done",
                "SPOOL out.txt",
                "select 1 from dual"
            ]
        );
    }

    #[test]
    fn oracle_set_transaction_is_sql() {
        let t = texts(ORA, "SET TRANSACTION READ ONLY; select 1 from dual");
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn oracle_slash_ends_plain_sql_too() {
        let t = texts(ORA, "select 1 from dual\n/\nselect 2 from dual\n/");
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn oracle_q_quote_hides_semicolon() {
        let t = texts(ORA, "select q'[a;b]' from dual; select 2 from dual");
        assert_eq!(t.len(), 2);
    }

    // ----- other dialects -----------------------------------------------------------------

    #[test]
    fn opaque_is_one_statement() {
        let t = texts(ScriptDialect::Opaque, "  a; b  ");
        assert_eq!(t, ["a; b"]);
        assert!(split(ScriptDialect::Opaque, "  ").is_empty());
    }

    #[test]
    fn redis_lines() {
        let t = texts(ScriptDialect::RedisCli, "SET a 1\n\n  GET a  \r\nDEL a");
        assert_eq!(t, ["SET a 1", "GET a", "DEL a"]);
    }

    #[test]
    fn mongo_statements() {
        let t = texts(
            ScriptDialect::MongoShell,
            "use shop\ndb.users.find({a: 1})\n  .sort({a: 1}); db.x.drop()\n// c\nshow dbs",
        );
        assert_eq!(
            t,
            [
                "use shop",
                "db.users.find({a: 1})\n  .sort({a: 1})",
                "db.x.drop()",
                "show dbs"
            ]
        );
    }

    #[test]
    fn mongo_multiline_object() {
        let t = texts(
            ScriptDialect::MongoShell,
            "db.a.insertOne({\n a: 1,\n b: [1,\n2]\n})\ndb.b.find()",
        );
        assert_eq!(t.len(), 2);
    }

    // ----- streaming ----------------------------------------------------------------------

    #[test]
    fn stream_matches_split_for_any_chunk_size() {
        let cases: Vec<(ScriptDialect, &str)> = vec![
            (PG, "select 1; do $$ a; b $$; select 'x;y'; select 3"),
            (
                MY,
                "DELIMITER $$\ncreate procedure p() begin select 1; end$$\nDELIMITER ;\nselect 2;",
            ),
            (MS, "select 1\nGO\nselect 2\nGO 2\nselect 3"),
            (
                ORA,
                "BEGIN\n null;\nEND;\n/\nselect 1 from dual;\nSET SERVEROUTPUT ON\n",
            ),
            (ScriptDialect::RedisCli, "SET a 1\nGET a\nDEL a"),
            (
                ScriptDialect::MongoShell,
                "use a\ndb.b.find()\n .limit(1)\ndb.c.drop()",
            ),
        ];
        for (d, text) in cases {
            let chars: Vec<char> = text.chars().collect();
            let mut want: Vec<String> = Vec::new();
            for u in split_batches(d, text) {
                let s: String = chars[u.range.start as usize..u.range.end as usize]
                    .iter()
                    .collect();
                for _ in 0..u.repeat {
                    want.push(s.clone());
                }
            }
            for chunk in [1, 2, 3, 5, 7, 64] {
                assert_eq!(stream(d, text, chunk), want, "{d:?} chunk {chunk}");
            }
        }
    }

    #[test]
    fn stream_emits_only_complete_statements() {
        let mut s = StreamSplitter::new(PG);
        assert!(s.push("select 1").is_empty());
        assert_eq!(s.push("; select 2;"), vec!["select 1", "select 2"]);
        assert!(s.push(" sel").is_empty());
        assert_eq!(s.finish(), vec!["sel".to_string()]);
    }

    #[test]
    fn stream_opaque_returns_on_finish() {
        let mut s = StreamSplitter::new(ScriptDialect::Opaque);
        assert!(s.push("a;").is_empty());
        assert!(s.push(" b").is_empty());
        assert_eq!(s.finish(), vec!["a; b".to_string()]);
        assert!(StreamSplitter::new(ORA).finish().is_empty());
    }
}

/// Incremental splitter for big files (SQL import): feed chunks, receive complete statements.
///
/// Statements come back trimmed of leading whitespace like [`split`] ranges; T-SQL batches with
/// `GO n` are repeated `n` times (capped at 10 000).
#[derive(Debug, Clone)]
pub struct StreamSplitter {
    dialect: ScriptDialect,
    buffer: Vec<char>,
    state: SplitState,
}

impl StreamSplitter {
    /// New splitter for `d`.
    pub fn new(d: ScriptDialect) -> Self {
        StreamSplitter {
            dialect: d,
            buffer: Vec::new(),
            state: SplitState::default(),
        }
    }

    /// Dialect in use.
    pub fn dialect(&self) -> ScriptDialect {
        self.dialect
    }

    fn collect(&self, units: &[Unit]) -> Vec<String> {
        let mut out = Vec::new();
        for u in units {
            let s: String = self.buffer[u.range.start as usize..u.range.end as usize]
                .iter()
                .collect();
            for _ in 0..u.repeat.clamp(1, 10_000) {
                out.push(s.clone());
            }
        }
        out
    }

    /// Feed a chunk; returns the statements completed by it.
    pub fn push(&mut self, chunk: &str) -> Vec<String> {
        self.buffer.extend(chunk.chars());
        let mut st = self.state.clone();
        let (units, consumed) = scan(self.dialect, &self.buffer, &mut st, false);
        let out = self.collect(&units);
        self.state = st;
        self.buffer.drain(..consumed);
        out
    }

    /// End of input; returns the remaining statement, if any.
    pub fn finish(mut self) -> Vec<String> {
        let mut st = std::mem::take(&mut self.state);
        let (units, _) = scan(self.dialect, &self.buffer, &mut st, true);
        self.collect(&units)
    }
}
