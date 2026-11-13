//! Lexing and splitting of Oracle scripts (SQL, PL/SQL and a few SQL*Plus conventions).
//!
//! The splitter understands what makes Oracle scripts different from plain SQL:
//!
//! * a PL/SQL block (`DECLARE`/`BEGIN`, `CREATE [OR REPLACE] PROCEDURE|FUNCTION|PACKAGE [BODY]|
//!   TRIGGER|TYPE BODY`) keeps every inner `;` and ends at its final `END [name];` (balanced over
//!   `BEGIN`, `CASE`, `IF` and `LOOP` and nested subprograms) or at a line holding only `/`,
//! * a line holding only `/` ends any statement (it is never part of the statement text),
//! * `q'[...]'` quoting, `''` escapes, `"quoted identifiers"`, `--` and `/* */` comments hide
//!   their content from the scanner,
//! * `SET <option>`, `PROMPT`, `REM`, ... are SQL*Plus commands that end at the end of the line,
//!   and `EXEC proc(...)` is a one-line call.
//!
//! All positions are character offsets (Unicode scalar values) into the whole script.

/// Kind of lexical token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tok {
    /// Identifier or keyword (unquoted).
    Word,
    /// `"quoted identifier"`.
    Quoted,
    /// String literal, including `q'...'` forms.
    Str,
    /// Number.
    Num,
    /// Any other single character.
    Punct,
    /// `-- ...` or `/* ... */`.
    Comment,
    /// Whitespace.
    Space,
}

/// A token of the script (char offsets, end exclusive).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    pub tok: Tok,
    pub start: usize,
    pub end: usize,
}

fn is_word_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '$' | '#')
}

/// Closing delimiter of a `q'` quote opened with `open`.
fn q_close(open: char) -> char {
    match open {
        '[' => ']',
        '(' => ')',
        '{' => '}',
        '<' => '>',
        c => c,
    }
}

/// Tokenize `chars`.
pub fn tokenize(chars: &[char]) -> Vec<Token> {
    let n = chars.len();
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        let c = chars[i];
        let start = i;
        let tok = if c.is_whitespace() {
            while i < n && chars[i].is_whitespace() {
                i += 1;
            }
            Tok::Space
        } else if c == '-' && i + 1 < n && chars[i + 1] == '-' {
            while i < n && chars[i] != '\n' {
                i += 1;
            }
            Tok::Comment
        } else if c == '/' && i + 1 < n && chars[i + 1] == '*' {
            i += 2;
            while i < n && !(chars[i] == '*' && i + 1 < n && chars[i + 1] == '/') {
                i += 1;
            }
            i = (i + 2).min(n);
            Tok::Comment
        } else if c == '\'' {
            i = skip_string(chars, i);
            Tok::Str
        } else if c == '"' {
            i += 1;
            while i < n && chars[i] != '"' {
                i += 1;
            }
            i = (i + 1).min(n);
            Tok::Quoted
        } else if is_word_start(c) {
            while i < n && is_word_char(chars[i]) {
                i += 1;
            }
            // `q'[...]'` and `nq'[...]'`.
            let word: String = chars[start..i].iter().collect();
            if matches!(word.to_ascii_lowercase().as_str(), "q" | "nq")
                && i + 2 < n
                && chars[i] == '\''
                && !chars[i + 1].is_whitespace()
            {
                let close = q_close(chars[i + 1]);
                let mut j = i + 2;
                let mut found = None;
                while j + 1 < n {
                    if chars[j] == close && chars[j + 1] == '\'' {
                        found = Some(j + 2);
                        break;
                    }
                    j += 1;
                }
                i = found.unwrap_or(n);
                Tok::Str
            } else {
                Tok::Word
            }
        } else if c.is_ascii_digit() {
            while i < n && (chars[i].is_alphanumeric() || chars[i] == '.') {
                // Keep `1..10` as one number: PL/SQL ranges contain no keyword.
                i += 1;
            }
            Tok::Num
        } else {
            i += 1;
            Tok::Punct
        };
        out.push(Token { tok, start, end: i });
    }
    out
}

/// Index after the string literal starting at `chars[i] == '\''`.
fn skip_string(chars: &[char], mut i: usize) -> usize {
    let n = chars.len();
    i += 1;
    while i < n {
        if chars[i] == '\'' {
            if i + 1 < n && chars[i + 1] == '\'' {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    n
}

/// What the executor must do with a statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StmtKind {
    /// SQL statement (trailing `;` removed).
    Sql,
    /// PL/SQL block or unit (text keeps its final `;`).
    PlSql,
    /// SQL*Plus `EXEC proc(args)`; the text is the call after the keyword.
    Exec,
    /// SQL*Plus command with no meaning for the server (`SET SERVEROUTPUT ON`, `PROMPT`...).
    Ignored(String),
}

/// One statement of a script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stmt {
    /// Char offset of the first character.
    pub start: usize,
    /// Char offset after the last character (terminator excluded for SQL).
    pub end: usize,
    pub kind: StmtKind,
    /// Text of the statement as sent to the server (for `Exec`, the call without `EXEC`).
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Ends at `;`.
    Plain,
    /// PL/SQL: ends at the `;` closing the outermost `END`.
    Block,
    /// Ends only at a `/` line (Java sources and the like).
    SlashOnly,
    /// SQL*Plus command: ends at the end of the line.
    Line,
}

#[derive(Debug, Clone, Copy)]
struct Frame {
    /// Still in the declaration section (a following `BEGIN` belongs to this frame).
    decl: bool,
}

/// Words that may sit between `CREATE` and the object type.
const CREATE_SKIP: [&str; 9] = [
    "OR",
    "REPLACE",
    "EDITIONABLE",
    "NONEDITIONABLE",
    "AND",
    "RESOLVE",
    "COMPILE",
    "NOFORCE",
    "FORCE",
];

/// SQL*Plus commands that occupy a single line.
const LINE_COMMANDS: [&str; 17] = [
    "PROMPT", "REM", "REMARK", "SPOOL", "WHENEVER", "DEFINE", "UNDEFINE", "COLUMN", "COL",
    "TTITLE", "BTITLE", "CLEAR", "SHOW", "PAUSE", "ACCEPT", "DESC", "DESCRIBE",
];

/// `SET` forms that are SQL statements, not SQL*Plus settings.
const SET_SQL: [&str; 4] = ["TRANSACTION", "CONSTRAINT", "CONSTRAINTS", "ROLE"];

struct Splitter<'a> {
    chars: &'a [char],
    tokens: Vec<Token>,
    out: Vec<Stmt>,
}

#[derive(Default)]
struct Cur {
    /// Token index of the first token of the statement (comments included).
    first: Option<usize>,
    /// Index of the last significant token (not space or comment).
    last_sig: Option<usize>,
    /// Significant words seen before the mode was decided.
    words: Vec<String>,
    mode: Option<Mode>,
    frames: Vec<Frame>,
    paren: i32,
    /// `PROCEDURE`/`FUNCTION` header seen inside a declaration section.
    sub_pending: bool,
    /// Unit header (`CREATE PROCEDURE ...`) waiting for its `IS`/`AS`.
    unit_pending_is: bool,
    compound: bool,
    pending_end: bool,
    prev_word: Option<String>,
    /// Statement text kind once the mode is known.
    plsql: bool,
    exec: bool,
    line_cmd: Option<String>,
}

impl Splitter<'_> {
    fn word(&self, i: usize) -> Option<String> {
        let t = self.tokens[i];
        (t.tok == Tok::Word).then(|| {
            self.chars[t.start..t.end]
                .iter()
                .collect::<String>()
                .to_ascii_uppercase()
        })
    }

    /// Next significant token after `i` (skipping space and comments).
    fn next_sig(&self, i: usize) -> Option<usize> {
        (i + 1..self.tokens.len())
            .find(|&j| !matches!(self.tokens[j].tok, Tok::Space | Tok::Comment))
    }

    fn punct(&self, i: usize) -> Option<char> {
        let t = self.tokens[i];
        (t.tok == Tok::Punct).then(|| self.chars[t.start])
    }

    /// Is the `/` token at `i` alone on its line?
    fn is_slash_line(&self, i: usize) -> bool {
        let t = self.tokens[i];
        if t.tok != Tok::Punct || self.chars[t.start] != '/' {
            return false;
        }
        let before = self.chars[..t.start]
            .iter()
            .rev()
            .take_while(|c| **c != '\n')
            .all(|c| c.is_whitespace());
        let after = self.chars[t.end..]
            .iter()
            .take_while(|c| **c != '\n')
            .all(|c| c.is_whitespace());
        before && after
    }

    fn finish(&mut self, cur: &mut Cur, end_tok: Option<usize>, include_terminator: bool) {
        let Some(first) = cur.first.take() else {
            *cur = Cur::default();
            return;
        };
        // Statements that hold only comments are dropped.
        let Some(last) = cur.last_sig else {
            *cur = Cur::default();
            return;
        };
        let last_tok = match (end_tok, include_terminator) {
            (Some(e), true) => e.max(last),
            _ => last,
        };
        let start = self.tokens[first].start;
        let end = self.tokens[last_tok].end;
        let kind = if let Some(what) = cur.line_cmd.take() {
            StmtKind::Ignored(what)
        } else if cur.exec {
            StmtKind::Exec
        } else if cur.plsql {
            StmtKind::PlSql
        } else {
            StmtKind::Sql
        };
        let mut text: String = self.chars[start..end].iter().collect();
        if matches!(kind, StmtKind::Exec) {
            // Remove the `EXEC`/`EXECUTE` keyword.
            let t = text.trim_start();
            text = t
                .split_once(char::is_whitespace)
                .map(|(_, r)| r.trim().to_string())
                .unwrap_or_default();
        }
        if !text.trim().is_empty() {
            self.out.push(Stmt {
                start,
                end,
                kind,
                text,
            });
        }
        *cur = Cur::default();
    }

    /// Decide the mode from the words seen so far; returns `true` once decided.
    fn decide(&self, cur: &mut Cur) -> bool {
        let w = &cur.words;
        let Some(w0) = w.first().map(String::as_str) else {
            return false;
        };
        match w0 {
            "BEGIN" | "DECLARE" => {
                cur.mode = Some(Mode::Block);
                cur.plsql = true;
                return true;
            }
            "EXEC" | "EXECUTE" => {
                cur.mode = Some(Mode::Line);
                cur.exec = true;
                return true;
            }
            "SET" => {
                let Some(w1) = w.get(1) else {
                    return false;
                };
                if SET_SQL.contains(&w1.as_str()) {
                    cur.mode = Some(Mode::Plain);
                } else {
                    cur.mode = Some(Mode::Line);
                    cur.line_cmd = Some(format!("SET {w1}"));
                }
                return true;
            }
            "CREATE" => {}
            other => {
                if LINE_COMMANDS.contains(&other) {
                    cur.mode = Some(Mode::Line);
                    cur.line_cmd = Some(other.to_string());
                } else {
                    cur.mode = Some(Mode::Plain);
                }
                return true;
            }
        }
        // CREATE ...: find the object word.
        let Some(idx) = w
            .iter()
            .skip(1)
            .position(|x| !CREATE_SKIP.contains(&x.as_str()))
            .map(|p| p + 1)
        else {
            return false;
        };
        let obj = w[idx].as_str();
        match obj {
            "PROCEDURE" | "FUNCTION" => {
                cur.mode = Some(Mode::Block);
                cur.plsql = true;
                cur.unit_pending_is = true;
                true
            }
            "PACKAGE" => {
                cur.mode = Some(Mode::Block);
                cur.plsql = true;
                cur.unit_pending_is = true;
                true
            }
            "TRIGGER" => {
                cur.mode = Some(Mode::Block);
                cur.plsql = true;
                true
            }
            "TYPE" => match w.get(idx + 1) {
                Some(next) if next == "BODY" => {
                    cur.mode = Some(Mode::Block);
                    cur.plsql = true;
                    cur.unit_pending_is = true;
                    true
                }
                Some(_) => {
                    cur.mode = Some(Mode::Plain);
                    true
                }
                None => false,
            },
            "JAVA" | "LIBRARY" => {
                cur.mode = Some(Mode::SlashOnly);
                cur.plsql = true;
                true
            }
            _ => {
                cur.mode = Some(Mode::Plain);
                true
            }
        }
    }

    /// Update the PL/SQL frame state for one significant token. Returns `true` when the
    /// statement ends with the token `i` (a `;` closing the outermost block).
    fn block_token(&self, cur: &mut Cur, i: usize) -> bool {
        if let Some(p) = self.punct(i) {
            match p {
                '(' => cur.paren += 1,
                ')' => cur.paren -= 1,
                ';' if cur.paren <= 0 => {
                    cur.sub_pending = false;
                    if cur.pending_end {
                        return true;
                    }
                    cur.pending_end = false;
                }
                _ => {}
            }
            cur.prev_word = None;
            return false;
        }
        let Some(word) = self.word(i) else {
            cur.prev_word = None;
            return false;
        };
        let prev = cur.prev_word.take();
        let after_end = prev.as_deref() == Some("END");
        match word.as_str() {
            "DECLARE" => cur.frames.push(Frame { decl: true }),
            "BEGIN" => match cur.frames.last_mut() {
                Some(top) if top.decl => top.decl = false,
                _ => cur.frames.push(Frame { decl: false }),
            },
            "CASE" | "IF" | "LOOP" if !after_end => cur.frames.push(Frame { decl: false }),
            "END" => {
                cur.frames.pop();
                cur.pending_end = cur.frames.is_empty();
            }
            "PROCEDURE" | "FUNCTION" if cur.frames.last().is_some_and(|f| f.decl) => {
                cur.sub_pending = true;
            }
            "COMPOUND"
                if self.next_sig(i).and_then(|j| self.word(j)).as_deref() == Some("TRIGGER") =>
            {
                cur.compound = true;
                cur.frames.push(Frame { decl: true });
            }
            "IS" | "AS" if cur.paren <= 0 => {
                let timing_point = cur.compound
                    && word == "IS"
                    && matches!(prev.as_deref(), Some("STATEMENT" | "ROW"));
                if cur.unit_pending_is || cur.sub_pending || timing_point {
                    let next = self.next_sig(i).and_then(|j| self.word(j));
                    let call_spec = matches!(next.as_deref(), Some("LANGUAGE" | "EXTERNAL"));
                    let self_as_result = word == "AS" && next.as_deref() == Some("RESULT");
                    if call_spec {
                        if cur.frames.is_empty() {
                            // `CREATE PROCEDURE p AS LANGUAGE JAVA ...;` has no body.
                            cur.mode = Some(Mode::Plain);
                        }
                        cur.unit_pending_is = false;
                        cur.sub_pending = false;
                    } else if !self_as_result {
                        cur.frames.push(Frame { decl: true });
                        cur.unit_pending_is = false;
                        cur.sub_pending = false;
                    }
                }
            }
            _ => {}
        }
        cur.prev_word = Some(word);
        false
    }
}

/// Split `text` into statements.
pub fn split(text: &str) -> Vec<Stmt> {
    let chars: Vec<char> = text.chars().collect();
    let tokens = tokenize(&chars);
    let mut sp = Splitter {
        chars: &chars,
        tokens,
        out: Vec::new(),
    };
    let mut cur = Cur::default();
    let mut i = 0;
    while i < sp.tokens.len() {
        let t = sp.tokens[i];
        match t.tok {
            Tok::Space => {
                // A SQL*Plus command ends at the end of its line.
                if cur.mode == Some(Mode::Line)
                    && cur.paren <= 0
                    && cur.last_sig.is_some()
                    && sp.chars[t.start..t.end].contains(&'\n')
                {
                    sp.finish(&mut cur, None, false);
                }
                i += 1;
                continue;
            }
            Tok::Comment => {
                if cur.first.is_none() {
                    cur.first = Some(i);
                }
                i += 1;
                continue;
            }
            _ => {}
        }
        // A `/` alone on its line ends the statement (and is dropped).
        if sp.is_slash_line(i) {
            sp.finish(&mut cur, None, false);
            i += 1;
            continue;
        }
        if cur.first.is_none() {
            cur.first = Some(i);
        }
        cur.last_sig = Some(i);
        if cur.mode.is_none() {
            let w = sp.word(i).unwrap_or_else(|| "?".to_string());
            cur.words.push(w);
            if !sp.decide(&mut cur) && cur.words.len() >= 8 {
                cur.mode = Some(Mode::Plain);
            }
            // The deciding word itself may already open a block (`BEGIN`, `DECLARE`).
            if cur.mode == Some(Mode::Block) {
                let w0 = cur.words[0].as_str();
                if (w0 == "BEGIN" || w0 == "DECLARE") && cur.words.len() == 1 {
                    sp.block_token(&mut cur, i);
                }
            }
            // A statement made of a single `;`-less decision word still needs terminator logic.
            if cur.mode == Some(Mode::Plain) && sp.punct(i) == Some(';') {
                cur.last_sig = None;
                sp.finish(&mut cur, None, false);
            }
            i += 1;
            continue;
        }
        match cur.mode {
            Some(Mode::Plain) | Some(Mode::Line) => {
                if sp.punct(i) == Some(';') {
                    // The terminator itself is not part of the statement.
                    cur.last_sig = i.checked_sub(1).and_then(|j| {
                        (0..=j)
                            .rev()
                            .find(|&k| !matches!(sp.tokens[k].tok, Tok::Space | Tok::Comment))
                            .filter(|&k| Some(k) >= cur.first)
                    });
                    sp.finish(&mut cur, None, false);
                } else if cur.mode == Some(Mode::Line) {
                    match sp.punct(i) {
                        Some('(') => cur.paren += 1,
                        Some(')') => cur.paren -= 1,
                        _ => {}
                    }
                }
            }
            Some(Mode::Block) => {
                if sp.block_token(&mut cur, i) {
                    sp.finish(&mut cur, Some(i), true);
                }
            }
            Some(Mode::SlashOnly) | None => {}
        }
        i += 1;
    }
    sp.finish(&mut cur, None, false);
    sp.out
}

/// Check that `text` is a fragment of exactly one statement: no top-level `;` or `/` line, no
/// unterminated string, quoted identifier or block comment (which would swallow what the driver
/// appends), and balanced parentheses. Used for user text spliced into generated SQL.
pub fn validate_fragment(text: &str) -> std::result::Result<(), String> {
    let chars: Vec<char> = text.chars().collect();
    let mut depth = 0i32;
    for t in tokenize(&chars) {
        let first = chars[t.start];
        let last = chars[t.end - 1];
        match t.tok {
            Tok::Punct if first == ';' => {
                return Err("a statement separator (;) is not allowed here".into());
            }
            Tok::Punct if first == '(' => depth += 1,
            Tok::Punct if first == ')' => {
                depth -= 1;
                if depth < 0 {
                    return Err("unbalanced parenthesis".into());
                }
            }
            Tok::Str if t.end - t.start < 2 || last != '\'' => {
                return Err("unterminated string literal".into());
            }
            Tok::Quoted if t.end - t.start < 2 || last != '"' => {
                return Err("unterminated quoted identifier".into());
            }
            Tok::Comment if first == '/' && !text_ends_comment(&chars[t.start..t.end]) => {
                return Err("unterminated comment".into());
            }
            _ => {}
        }
    }
    if depth != 0 {
        return Err("unbalanced parenthesis".into());
    }
    if split(text).len() > 1 {
        return Err("only one statement is allowed here".into());
    }
    Ok(())
}

fn text_ends_comment(c: &[char]) -> bool {
    c.len() >= 4 && c[c.len() - 2] == '*' && c[c.len() - 1] == '/'
}

/// Uppercase significant words at the start of `text` (at most `n`), skipping comments.
pub fn leading_words(text: &str, n: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let tokens = tokenize(&chars);
    let mut out = Vec::new();
    for t in tokens {
        match t.tok {
            Tok::Space | Tok::Comment => {}
            Tok::Word => {
                out.push(
                    chars[t.start..t.end]
                        .iter()
                        .collect::<String>()
                        .to_ascii_uppercase(),
                );
                if out.len() == n {
                    break;
                }
            }
            _ => {
                out.push(chars[t.start..t.end].iter().collect());
                if out.len() == n {
                    break;
                }
            }
        }
    }
    out
}

/// `true` when `text` (one statement) contains the two consecutive words `a b`.
pub fn contains_words(text: &str, a: &str, b: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    let words: Vec<String> = tokenize(&chars)
        .iter()
        .filter(|t| t.tok == Tok::Word)
        .map(|t| {
            chars[t.start..t.end]
                .iter()
                .collect::<String>()
                .to_ascii_uppercase()
        })
        .collect();
    words.windows(2).any(|w| w[0] == a && w[1] == b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(s: &str) -> Vec<String> {
        split(s).into_iter().map(|st| st.text).collect()
    }

    #[test]
    fn plain_statements_split_at_semicolons() {
        assert_eq!(
            texts("select 1 from dual; select 2 from dual;\n"),
            vec!["select 1 from dual", "select 2 from dual"]
        );
        assert_eq!(texts("select 1 from dual"), vec!["select 1 from dual"]);
        assert!(texts("  \n -- only a comment\n /* x */ ").is_empty());
        assert!(texts(";;;").is_empty());
    }

    #[test]
    fn semicolons_in_literals_and_comments_do_not_split() {
        assert_eq!(
            texts("select 'a;b', \"x;y\", q'[c;d]' from dual -- e;f\n; select 2 from dual"),
            vec![
                "select 'a;b', \"x;y\", q'[c;d]' from dual",
                "select 2 from dual"
            ]
        );
        assert_eq!(
            texts("select 1 /* ; */ from dual;"),
            vec!["select 1 /* ; */ from dual"]
        );
        assert_eq!(
            texts("select q'{it's;}' , 'it''s;' from dual;"),
            vec!["select q'{it's;}' , 'it''s;' from dual"]
        );
    }

    #[test]
    fn anonymous_block_keeps_inner_semicolons() {
        let s =
            "BEGIN\n  INSERT INTO t VALUES (1);\n  UPDATE t SET a = 2;\nEND;\nselect 1 from dual;";
        let st = split(s);
        assert_eq!(st.len(), 2);
        assert_eq!(st[0].kind, StmtKind::PlSql);
        assert!(st[0].text.starts_with("BEGIN") && st[0].text.ends_with("END;"));
        assert_eq!(st[1].text, "select 1 from dual");
    }

    #[test]
    fn block_with_nested_control_flow_and_subprograms() {
        let s = "DECLARE\n  x NUMBER := 0;\n  CURSOR c IS SELECT 1 FROM dual;\n  PROCEDURE p(a NUMBER) IS\n  BEGIN\n    IF a > 1 THEN NULL; ELSE NULL; END IF;\n  END p;\n  FUNCTION f RETURN NUMBER IS BEGIN RETURN 1; END;\nBEGIN\n  FOR i IN 1..3 LOOP\n    x := x + CASE WHEN i = 1 THEN 1 ELSE 2 END;\n    CASE x WHEN 1 THEN NULL; ELSE NULL; END CASE;\n  END LOOP;\n  p(1);\nEXCEPTION WHEN OTHERS THEN NULL;\nEND;\nselect 2 from dual;";
        let st = split(s);
        assert_eq!(st.len(), 2, "{st:#?}");
        assert!(
            st[0]
                .text
                .ends_with("EXCEPTION WHEN OTHERS THEN NULL;\nEND;")
        );
        assert_eq!(st[1].text, "select 2 from dual");
    }

    #[test]
    fn slash_line_ends_a_block_and_is_dropped() {
        let s = "BEGIN\n  NULL;\nEND;\n/\nselect 1 from dual;\n/\nselect 2 from dual";
        assert_eq!(
            texts(s),
            vec![
                "BEGIN\n  NULL;\nEND;",
                "select 1 from dual",
                "select 2 from dual"
            ]
        );
        // Division on a line with other content is not a terminator.
        assert_eq!(
            texts("select 4 / 2 from dual;"),
            vec!["select 4 / 2 from dual"]
        );
    }

    #[test]
    fn create_procedure_function_trigger() {
        let proc = "CREATE OR REPLACE PROCEDURE hello(p_name IN VARCHAR2) AS\nBEGIN\n  DBMS_OUTPUT.PUT_LINE('Hello ' || p_name);\n  NULL;\nEND hello;\n/\nselect 1 from dual;";
        let st = split(proc);
        assert_eq!(st.len(), 2, "{st:#?}");
        assert_eq!(st[0].kind, StmtKind::PlSql);
        assert!(st[0].text.ends_with("END hello;"));

        // Without the `/`, the unit still ends at its final END.
        let no_slash = "CREATE FUNCTION f RETURN NUMBER IS\n  x NUMBER;\nBEGIN\n  x := 1;\n  RETURN x;\nEND;\nselect 3 from dual;";
        let st = split(no_slash);
        assert_eq!(st.len(), 2, "{st:#?}");
        assert!(st[0].text.starts_with("CREATE FUNCTION"));

        let trg = "CREATE OR REPLACE TRIGGER trg BEFORE INSERT ON t FOR EACH ROW\nBEGIN\n  :new.a := 1;\n  IF :new.b IS NULL THEN :new.b := 2; END IF;\nEND;\n/";
        let st = split(trg);
        assert_eq!(st.len(), 1, "{st:#?}");
        assert_eq!(st[0].kind, StmtKind::PlSql);

        let trg2 = "CREATE TRIGGER t2 AFTER UPDATE ON t DECLARE x NUMBER; BEGIN x := 1; END;\nselect 1 from dual;";
        assert_eq!(split(trg2).len(), 2);
    }

    #[test]
    fn packages_and_type_bodies() {
        let spec = "CREATE OR REPLACE PACKAGE pk AS\n  PROCEDURE a(x NUMBER);\n  FUNCTION b RETURN NUMBER;\nEND pk;\n/\nCREATE OR REPLACE PACKAGE BODY pk AS\n  PROCEDURE a(x NUMBER) IS BEGIN NULL; END a;\n  FUNCTION b RETURN NUMBER IS BEGIN RETURN 1; END b;\nBEGIN\n  NULL;\nEND pk;\n/\nselect 1 from dual";
        let st = split(spec);
        assert_eq!(st.len(), 3, "{st:#?}");
        assert!(
            st[0].text.ends_with("END pk;") && st[0].text.contains("FUNCTION b RETURN NUMBER;")
        );
        assert!(st[1].text.starts_with("CREATE OR REPLACE PACKAGE BODY"));
        assert_eq!(st[2].text, "select 1 from dual");

        // Without slashes the package units still separate.
        let nos = "CREATE PACKAGE p2 AS PROCEDURE x; END p2;\nCREATE PACKAGE BODY p2 AS PROCEDURE x IS BEGIN NULL; END; END p2;\nselect 1 from dual;";
        assert_eq!(split(nos).len(), 3, "{:#?}", split(nos));

        let ty = "CREATE TYPE pt AS OBJECT (a NUMBER, MEMBER FUNCTION f RETURN NUMBER);\n/\nCREATE TYPE BODY pt AS\n  MEMBER FUNCTION f RETURN NUMBER IS BEGIN RETURN a; END;\nEND;\n/";
        let st = split(ty);
        assert_eq!(st.len(), 2, "{st:#?}");
        assert_eq!(st[0].kind, StmtKind::Sql);
        assert_eq!(st[1].kind, StmtKind::PlSql);
    }

    #[test]
    fn compound_triggers() {
        let s = "CREATE OR REPLACE TRIGGER ct FOR INSERT ON t COMPOUND TRIGGER\n  v NUMBER;\n  BEFORE STATEMENT IS BEGIN v := 0; END BEFORE STATEMENT;\n  AFTER EACH ROW IS BEGIN v := v + 1; END AFTER EACH ROW;\nEND ct;\nselect 1 from dual;";
        let st = split(s);
        assert_eq!(st.len(), 2, "{st:#?}");
        assert!(st[0].text.ends_with("END ct;"));
    }

    #[test]
    fn call_specs_have_no_body() {
        let s = "CREATE PROCEDURE j AS LANGUAGE JAVA NAME 'a.b()';\nselect 1 from dual;";
        assert_eq!(split(s).len(), 2, "{:#?}", split(s));
    }

    #[test]
    fn sqlplus_commands_and_exec() {
        let s = "SET SERVEROUTPUT ON\nSET DEFINE OFF;\nPROMPT hello world\nselect 1 from dual;\nEXEC dbms_output.put_line('x')\nEXECUTE p(1,\n 2);\nSET TRANSACTION READ ONLY;";
        let st = split(s);
        assert_eq!(st.len(), 7, "{st:#?}");
        assert_eq!(st[0].kind, StmtKind::Ignored("SET SERVEROUTPUT".into()));
        assert_eq!(st[1].kind, StmtKind::Ignored("SET DEFINE".into()));
        assert_eq!(st[2].kind, StmtKind::Ignored("PROMPT".into()));
        assert_eq!(st[3].text, "select 1 from dual");
        assert_eq!(st[4].kind, StmtKind::Exec);
        assert_eq!(st[4].text, "dbms_output.put_line('x')");
        assert_eq!(st[5].kind, StmtKind::Exec);
        assert_eq!(st[5].text, "p(1,\n 2)");
        // `SET TRANSACTION` is SQL, kept as the last statement.
        assert_eq!(st[6].kind, StmtKind::Sql);
    }

    #[test]
    fn set_transaction_is_sql() {
        let st = split("SET TRANSACTION READ ONLY;\nselect 1 from dual");
        assert_eq!(st.len(), 2);
        assert_eq!(st[0].kind, StmtKind::Sql);
        assert_eq!(st[0].text, "SET TRANSACTION READ ONLY");
    }

    #[test]
    fn offsets_are_char_offsets() {
        let s = "select 'é€' from dual;\n  select 2 from dual;";
        let st = split(s);
        assert_eq!(st[0].start, 0);
        assert_eq!(st[0].end, 21);
        let chars: Vec<char> = s.chars().collect();
        assert_eq!(st[1].start, 25);
        let slice: String = chars[st[1].start..st[1].end].iter().collect();
        assert_eq!(slice, "select 2 from dual");
        // Emoji (outside the BMP) is one char.
        let s = "select '😀' from dual; select 3 from dual";
        let expected = s.chars().position(|c| c == ';').unwrap() + 2;
        assert_eq!(split(s)[1].start, expected);
    }

    #[test]
    fn comments_belong_to_the_statement_that_follows() {
        let st = split("-- first\nselect 1 from dual; /* c */ select 2 from dual");
        assert_eq!(st.len(), 2);
        assert!(st[0].text.starts_with("-- first"));
        assert!(st[1].text.starts_with("/* c */"));
    }

    #[test]
    fn begin_end_inside_strings_and_comments_are_ignored() {
        let s = "BEGIN\n  -- end; begin\n  dbms_output.put_line('END; BEGIN');\n  NULL;\nEND;\nselect 1 from dual;";
        let st = split(s);
        assert_eq!(st.len(), 2, "{st:#?}");
    }

    #[test]
    fn end_if_loop_case_labels() {
        let s = "BEGIN\n  <<outer>>\n  FOR i IN 1..2 LOOP\n    IF i = 1 THEN\n      CONTINUE outer;\n    END IF;\n  END LOOP outer;\n  NULL;\nEND;\nselect 1 from dual;";
        assert_eq!(split(s).len(), 2, "{:#?}", split(s));
    }

    #[test]
    fn fragments_must_be_one_balanced_statement() {
        for ok in [
            "a = 1",
            "a = ';' -- x",
            "f(a, (b)) > 1 /* c */",
            "MOD(ID, 10) = 0 -- comment",
        ] {
            assert!(validate_fragment(ok).is_ok(), "{ok}");
        }
        for bad in [
            "1 = 1; DROP TABLE t",
            "a = 1) OR (1 = 1",
            "(a = 1",
            "a = 'x",
            "a = \"x",
            "a = 1 /* open",
            "a = 1\n/\nDROP TABLE t",
        ] {
            assert!(validate_fragment(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn leading_words_skip_comments() {
        assert_eq!(
            leading_words("  /* c */ -- d\n  select * from t", 2),
            vec!["SELECT", "*"]
        );
    }

    #[test]
    fn contains_words_checks_whole_words() {
        assert!(contains_words(
            "select * from t for update",
            "FOR",
            "UPDATE"
        ));
        assert!(!contains_words(
            "select 1 -- for update\n from t",
            "FOR",
            "UPDATE"
        ));
    }
}
