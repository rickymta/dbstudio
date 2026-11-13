//! Splitting a T-SQL script into batches at `GO [count]` lines.
//!
//! `GO` is a client-side separator, not T-SQL: a line holding only `GO` (any case), an optional
//! repeat count and an optional trailing comment, outside strings, quoted identifiers and
//! comments. The scanner is `dbstudio_sql::split::split_batches`.

use dbstudio_core::ScriptDialect;

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

/// Split `text` into batches. Empty (whitespace-only) batches are dropped; the repeat count of a
/// `GO n` line applies to the batch before it. The scanner is `dbstudio_sql::split`.
pub fn split_batches(text: &str) -> Vec<Batch> {
    let chars: Vec<char> = text.chars().collect();
    dbstudio_sql::split::split_batches(ScriptDialect::MsSql, text)
        .into_iter()
        .filter_map(|u| {
            let (start, end) = (
                u.range.start as usize,
                (u.range.end as usize).min(chars.len()),
            );
            if start >= end {
                return None;
            }
            let batch: String = chars[start..end].iter().collect();
            if batch.trim().is_empty() {
                return None;
            }
            Some(Batch {
                text: batch,
                start: u.range.start,
                len: (end - start) as u32,
                repeat: u.repeat.max(1),
            })
        })
        .collect()
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
            vec![("SELECT 1".into(), 1), ("SELECT 2".into(), 1)]
        );
        let b = split_batches(t);
        assert_eq!(b[1].start, 12);
    }

    #[test]
    fn go_with_count_and_comment() {
        assert_eq!(
            texts("INSERT t DEFAULT VALUES\nGO 3 -- three\nSELECT 1"),
            vec![
                ("INSERT t DEFAULT VALUES".into(), 3),
                ("SELECT 1".into(), 1)
            ]
        );
        assert_eq!(texts("A\n  GO   2  \nB").len(), 2);
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
            vec![("SELECT 1".into(), 1), ("SELECT 2".into(), 1)]
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
