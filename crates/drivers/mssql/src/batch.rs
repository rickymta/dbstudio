//! Splitting a T-SQL script into batches at `GO [count]` lines.
//!
//! `GO` is a client-side separator, not T-SQL: a line holding only `GO` (any case), an optional
//! repeat count and an optional trailing comment, outside strings, quoted identifiers and
//! comments. `dbstudio_sql::split` returns statement ranges without the repeat count, so the
//! driver keeps its own batch splitter (see the crate docs).

/// One batch of a script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Batch {
    /// Text to send.
    pub text: String,
    /// Character offset of `text` in the script.
    pub start: u32,
    /// Character length of `text`.
    pub len: u32,
    /// Number of times to run it (`GO n`).
    pub repeat: u32,
}

/// Upper bound of a `GO n` count.
const MAX_REPEAT: u32 = 1_000_000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Code,
    Str,
    QuotedIdent,
    BracketIdent,
    LineComment,
    BlockComment(u32),
}

/// Split `text` into batches. Empty (whitespace-only) batches are dropped; the repeat count of a
/// `GO n` line applies to the batch before it.
pub fn split_batches(text: &str) -> Vec<Batch> {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<Batch> = Vec::new();
    let mut state = State::Code;
    let mut batch_start = 0usize;
    let mut i = 0usize;
    let mut at_line_start = true;
    while i < chars.len() {
        if state == State::Code && at_line_start {
            if let Some((end_of_go, count)) = go_line(&chars, i) {
                push_batch(&mut out, &chars, batch_start, i, count);
                i = end_of_go;
                batch_start = i;
                at_line_start = true;
                continue;
            }
        }
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        at_line_start = c == '\n';
        match state {
            State::Code => match (c, next) {
                ('\'', _) => state = State::Str,
                ('"', _) => state = State::QuotedIdent,
                ('[', _) => state = State::BracketIdent,
                ('-', Some('-')) => {
                    state = State::LineComment;
                    i += 1;
                }
                ('/', Some('*')) => {
                    state = State::BlockComment(1);
                    i += 1;
                }
                _ => {}
            },
            State::Str => {
                if c == '\'' {
                    if next == Some('\'') {
                        i += 1;
                    } else {
                        state = State::Code;
                    }
                }
            }
            State::QuotedIdent => {
                if c == '"' {
                    if next == Some('"') {
                        i += 1;
                    } else {
                        state = State::Code;
                    }
                }
            }
            State::BracketIdent => {
                if c == ']' {
                    if next == Some(']') {
                        i += 1;
                    } else {
                        state = State::Code;
                    }
                }
            }
            State::LineComment => {
                if c == '\n' {
                    state = State::Code;
                }
            }
            State::BlockComment(depth) => match (c, next) {
                ('*', Some('/')) => {
                    state = if depth == 1 {
                        State::Code
                    } else {
                        State::BlockComment(depth - 1)
                    };
                    i += 1;
                }
                ('/', Some('*')) => {
                    state = State::BlockComment(depth + 1);
                    i += 1;
                }
                _ => {}
            },
        }
        i += 1;
    }
    push_batch(&mut out, &chars, batch_start, chars.len(), 1);
    out
}

fn push_batch(out: &mut Vec<Batch>, chars: &[char], start: usize, end: usize, repeat: u32) {
    if start >= end {
        return;
    }
    let text: String = chars[start..end].iter().collect();
    if text.trim().is_empty() {
        return;
    }
    out.push(Batch {
        text,
        start: start as u32,
        len: (end - start) as u32,
        repeat,
    });
}

/// If the line starting at `i` is a `GO` line, the index just past its line break and the count.
fn go_line(chars: &[char], i: usize) -> Option<(usize, u32)> {
    let mut j = i;
    while j < chars.len() && matches!(chars[j], ' ' | '\t') {
        j += 1;
    }
    let go: String = chars.get(j..j + 2)?.iter().collect();
    if !go.eq_ignore_ascii_case("go") {
        return None;
    }
    j += 2;
    // The keyword must end here: whitespace, end of text, or a comment start.
    match chars.get(j) {
        None | Some(' ' | '\t' | '\r' | '\n') => {}
        Some('-') if chars.get(j + 1) == Some(&'-') => {}
        Some('/') if chars.get(j + 1) == Some(&'*') => {}
        _ => return None,
    }
    while j < chars.len() && matches!(chars[j], ' ' | '\t') {
        j += 1;
    }
    let mut count: Option<u32> = None;
    let digits_start = j;
    while j < chars.len() && chars[j].is_ascii_digit() {
        j += 1;
    }
    if j > digits_start {
        let s: String = chars[digits_start..j].iter().collect();
        count = Some(s.parse::<u32>().unwrap_or(MAX_REPEAT).min(MAX_REPEAT));
    }
    while j < chars.len() && matches!(chars[j], ' ' | '\t') {
        j += 1;
    }
    // Optional trailing comment.
    if chars.get(j) == Some(&'-') && chars.get(j + 1) == Some(&'-') {
        while j < chars.len() && chars[j] != '\n' {
            j += 1;
        }
    } else if chars.get(j) == Some(&'/') && chars.get(j + 1) == Some(&'*') {
        let mut k = j + 2;
        let mut closed = false;
        while k + 1 < chars.len() {
            if chars[k] == '*' && chars[k + 1] == '/' {
                closed = true;
                k += 2;
                break;
            }
            k += 1;
        }
        if !closed {
            return None;
        }
        j = k;
        while j < chars.len() && matches!(chars[j], ' ' | '\t') {
            j += 1;
        }
    }
    match chars.get(j) {
        None => Some((j, count.unwrap_or(1))),
        Some('\r') if chars.get(j + 1) == Some(&'\n') => Some((j + 2, count.unwrap_or(1))),
        Some('\n' | '\r') => Some((j + 1, count.unwrap_or(1))),
        _ => None,
    }
}

/// Character offset of the start of the 1-based `line` of `batch_text`.
pub fn line_start_offset(batch_text: &str, line: u32) -> u32 {
    if line <= 1 {
        return 0;
    }
    let mut seen = 1u32;
    for (offset, c) in batch_text.chars().enumerate() {
        if c == '\n' {
            seen += 1;
            if seen == line {
                return offset as u32 + 1;
            }
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(s: &str) -> Vec<(String, u32)> {
        split_batches(s)
            .into_iter()
            .map(|b| (b.text, b.repeat))
            .collect()
    }

    #[test]
    fn plain_text_is_one_batch() {
        let b = split_batches("SELECT 1;\nSELECT 2");
        assert_eq!(b.len(), 1);
        assert_eq!((b[0].start, b[0].len, b[0].repeat), (0, 18, 1));
        assert!(split_batches("  \n").is_empty());
    }

    #[test]
    fn go_splits_batches() {
        let t = "SELECT 1\nGO\nSELECT 2\ngo\n";
        assert_eq!(
            texts(t),
            vec![("SELECT 1\n".into(), 1), ("SELECT 2\n".into(), 1)]
        );
        let b = split_batches(t);
        assert_eq!(b[1].start, 12);
    }

    #[test]
    fn go_with_count_and_comment() {
        assert_eq!(
            texts("INSERT t DEFAULT VALUES\nGO 3 -- three\nSELECT 1"),
            vec![
                ("INSERT t DEFAULT VALUES\n".into(), 3),
                ("SELECT 1".into(), 1)
            ]
        );
        assert_eq!(texts("A\n  GO   2  \nB").len(), 2);
        assert_eq!(texts("A\nGO 2 /* c */\nB")[0].1, 2);
    }

    #[test]
    fn go_inside_strings_and_comments_is_text() {
        assert_eq!(texts("SELECT '\nGO\n'").len(), 1);
        assert_eq!(texts("/*\nGO\n*/ SELECT 1").len(), 1);
        assert_eq!(texts("-- x\nSELECT [a\nGO\nb]").len(), 1);
        assert_eq!(texts("SELECT 1 -- GO\nGO\nSELECT 2").len(), 2);
        assert_eq!(texts("SELECT \"a\nGO\nb\"").len(), 1);
    }

    #[test]
    fn go_must_be_alone_on_its_line() {
        assert_eq!(texts("GO TO 5\nSELECT 1").len(), 1);
        assert_eq!(texts("GOTO label").len(), 1);
        assert_eq!(texts("SELECT 1 GO").len(), 1);
        assert_eq!(texts("SELECT go FROM t").len(), 1);
    }

    #[test]
    fn crlf_and_trailing_go() {
        assert_eq!(
            texts("SELECT 1\r\nGO\r\nSELECT 2\r\nGO"),
            vec![("SELECT 1\r\n".into(), 1), ("SELECT 2\r\n".into(), 1)]
        );
    }

    #[test]
    fn offsets_count_characters() {
        let b = split_batches("SELECT 'é😀'\nGO\nSELECT 2");
        assert_eq!(b[0].start, 0);
        assert_eq!(b[1].start, "SELECT 'é😀'\nGO\n".chars().count() as u32);
    }

    #[test]
    fn line_offsets() {
        assert_eq!(line_start_offset("a\nbc\nd", 1), 0);
        assert_eq!(line_start_offset("a\nbc\nd", 2), 2);
        assert_eq!(line_start_offset("a\nbc\nd", 3), 5);
        assert_eq!(line_start_offset("a", 9), 0);
    }
}
