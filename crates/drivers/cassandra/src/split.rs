//! Statement splitting of CQL scripts (and cqlsh-style commands), working on `char` offsets.
//!
//! * `;` ends a statement unless inside `'...'`, `"..."`, `$$...$$` or a comment (`--`, `//`,
//!   `/* */`).
//! * `BEGIN [UNLOGGED|COUNTER] BATCH ... APPLY BATCH` is one statement.
//! * cqlsh commands (`CONSISTENCY ...`, `TRACING ON`, `USE ks`, `DESCRIBE ...`...) may end at the
//!   end of the line instead of a `;`.

use std::ops::Range;

/// Commands of the cqlsh shell that end at a newline when no `;` follows.
const LINE_COMMANDS: &[&str] = &[
    "consistency",
    "serial",
    "tracing",
    "use",
    "describe",
    "desc",
    "copy",
    "source",
    "paging",
    "expand",
    "capture",
    "clear",
    "cls",
    "login",
    "show",
    "unicode",
];

/// Character ranges of the statements of `text`, trimmed, without the terminating `;`.
pub(crate) fn split(text: &str) -> Vec<Range<usize>> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut out = Vec::new();
    let mut i = 0;
    let mut start: Option<usize> = None;
    let mut first_word = String::new();
    let mut in_batch = false;
    let mut prev_word = String::new();
    // Current word (letters only) used for BEGIN BATCH / APPLY BATCH detection.
    let mut word = String::new();
    let mut real_tokens = 0usize;

    let flush = |start: &mut Option<usize>, end: usize, out: &mut Vec<Range<usize>>| {
        if let Some(s) = start.take() {
            let mut e = end;
            while e > s && chars[e - 1].is_whitespace() {
                e -= 1;
            }
            if e > s {
                out.push(s..e);
            }
        }
    };

    macro_rules! end_word {
        () => {
            if !word.is_empty() {
                let w = word.to_ascii_lowercase();
                real_tokens += 1;
                if real_tokens == 1 {
                    first_word = w.clone();
                }
                if w == "batch" && matches!(prev_word.as_str(), "begin" | "unlogged" | "counter") {
                    in_batch = true;
                }
                if w == "batch" && prev_word == "apply" {
                    in_batch = false;
                }
                prev_word = w;
                word.clear();
            }
        };
    }

    while i < n {
        let c = chars[i];
        // Comments.
        if c == '-' && chars.get(i + 1) == Some(&'-') || c == '/' && chars.get(i + 1) == Some(&'/')
        {
            end_word!();
            while i < n && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && chars.get(i + 1) == Some(&'*') {
            end_word!();
            i += 2;
            while i < n && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                i += 1;
            }
            i = (i + 2).min(n);
            continue;
        }
        if start.is_none() && !c.is_whitespace() && c != ';' {
            start = Some(i);
            first_word.clear();
            prev_word.clear();
            real_tokens = 0;
            in_batch = false;
        }
        match c {
            '\'' | '"' => {
                end_word!();
                real_tokens += 1;
                if real_tokens == 1 {
                    first_word.clear();
                }
                let q = c;
                i += 1;
                while i < n {
                    if chars[i] == q {
                        if chars.get(i + 1) == Some(&q) {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                i += 1;
                continue;
            }
            '$' if chars.get(i + 1) == Some(&'$') => {
                end_word!();
                real_tokens += 1;
                i += 2;
                while i < n && !(chars[i] == '$' && chars.get(i + 1) == Some(&'$')) {
                    i += 1;
                }
                i += 2;
                continue;
            }
            ';' => {
                end_word!();
                if in_batch {
                    i += 1;
                    continue;
                }
                flush(&mut start, i, &mut out);
            }
            '\n' => {
                end_word!();
                if start.is_some() && !in_batch && LINE_COMMANDS.contains(&first_word.as_str()) {
                    flush(&mut start, i, &mut out);
                }
            }
            c if c.is_alphanumeric() || c == '_' => word.push(c),
            _ => end_word!(),
        }
        i += 1;
    }
    flush(&mut start, n, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(text: &str) -> Vec<String> {
        let chars: Vec<char> = text.chars().collect();
        split(text)
            .into_iter()
            .map(|r| chars[r].iter().collect())
            .collect()
    }

    #[test]
    fn splits_on_semicolons() {
        assert_eq!(parts("SELECT 1; SELECT 2;"), vec!["SELECT 1", "SELECT 2"]);
        assert_eq!(parts("  ;; SELECT 1"), vec!["SELECT 1"]);
        assert!(parts("   ").is_empty());
    }

    #[test]
    fn respects_quotes_and_comments() {
        assert_eq!(
            parts("INSERT INTO t (a) VALUES ('x;y'); -- c; d\nSELECT \"a;b\" FROM t;"),
            vec!["INSERT INTO t (a) VALUES ('x;y')", "SELECT \"a;b\" FROM t"]
        );
        assert_eq!(
            parts("/* a; */ SELECT 1; // x;\nSELECT 2"),
            vec!["SELECT 1", "SELECT 2"]
        );
        assert_eq!(
            parts(
                "CREATE FUNCTION f() RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE java AS $$ return 1; $$; SELECT 1"
            ),
            vec![
                "CREATE FUNCTION f() RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE java AS $$ return 1; $$",
                "SELECT 1"
            ]
        );
        assert_eq!(parts("SELECT 'it''s;'"), vec!["SELECT 'it''s;'"]);
    }

    #[test]
    fn batch_is_one_statement() {
        let text = "BEGIN UNLOGGED BATCH INSERT INTO t (a) VALUES (1); INSERT INTO t (a) VALUES (2); APPLY BATCH; SELECT 1";
        let p = parts(text);
        assert_eq!(p.len(), 2);
        assert!(p[0].starts_with("BEGIN") && p[0].ends_with("APPLY BATCH"));
        assert_eq!(
            parts("begin batch update t set a=1 where k=1; apply batch;").len(),
            1
        );
    }

    #[test]
    fn shell_commands_end_at_newline() {
        assert_eq!(
            parts("CONSISTENCY QUORUM\nTRACING ON\nSELECT * FROM t;"),
            vec!["CONSISTENCY QUORUM", "TRACING ON", "SELECT * FROM t"]
        );
        assert_eq!(parts("USE ks\nSELECT 1;"), vec!["USE ks", "SELECT 1"]);
        // A normal statement continues over newlines.
        assert_eq!(parts("SELECT *\nFROM t;"), vec!["SELECT *\nFROM t"]);
    }

    #[test]
    fn offsets_are_char_offsets() {
        let r = split("SELECT 'é'; SELECT 2");
        assert_eq!(r, vec![0..10, 12..20]);
    }
}
