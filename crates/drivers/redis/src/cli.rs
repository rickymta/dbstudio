//! redis-cli style line handling: tokenizer, argument quoting and script splitting.

/// Split one command line into arguments like `redis-cli` (shared tokenizer of
/// `dbstudio-sql`; the error is the human readable message).
pub fn tokenize_line(line: &str) -> Result<Vec<Vec<u8>>, String> {
    dbstudio_sql::redis_cli::tokenize_line(line).map_err(|e| e.message())
}

/// Render one argument so that [`tokenize_line`] reads it back unchanged.
pub fn quote_arg(arg: &[u8]) -> String {
    let plain = !arg.is_empty()
        && arg
            .iter()
            .all(|b| b.is_ascii_graphic() && !matches!(b, b'"' | b'\'' | b'\\'));
    if plain {
        return String::from_utf8_lossy(arg).into_owned();
    }
    let mut s = String::from("\"");
    match std::str::from_utf8(arg) {
        Ok(text) => {
            for c in text.chars() {
                match c {
                    '"' => s.push_str("\\\""),
                    '\\' => s.push_str("\\\\"),
                    '\n' => s.push_str("\\n"),
                    '\r' => s.push_str("\\r"),
                    '\t' => s.push_str("\\t"),
                    c if c.is_control() => s.push_str(&format!("\\x{:02x}", c as u32)),
                    c => s.push(c),
                }
            }
        }
        Err(_) => {
            for b in arg {
                match b {
                    b'"' => s.push_str("\\\""),
                    b'\\' => s.push_str("\\\\"),
                    0x20..=0x7e => s.push(*b as char),
                    b => s.push_str(&format!("\\x{b:02x}")),
                }
            }
        }
    }
    s.push('"');
    s
}

/// Render a whole command (`SET "a b" 1`).
pub fn render_command(args: &[Vec<u8>]) -> String {
    args.iter()
        .map(|a| quote_arg(a))
        .collect::<Vec<_>>()
        .join(" ")
}

/// A non-empty command line of a script and its position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line<'a> {
    /// Text of the line without surrounding whitespace.
    pub text: &'a str,
    /// Character offset of the first character of `text` in the whole script.
    pub start: u32,
    /// Character offset after the last character.
    pub end: u32,
}

/// Lines of a console script: one command per line; blank lines and `#` comments are skipped.
pub fn script_lines(text: &str) -> Vec<Line<'_>> {
    let mut out = Vec::new();
    let mut offset: u32 = 0;
    for raw in text.split('\n') {
        let raw_chars = raw.chars().count() as u32;
        let trimmed = raw.trim();
        if !trimmed.is_empty() && !trimmed.starts_with('#') {
            let lead = raw.chars().take_while(|c| c.is_whitespace()).count() as u32;
            let start = offset + lead;
            out.push(Line {
                text: trimmed,
                start,
                end: start + trimmed.chars().count() as u32,
            });
        }
        offset += raw_chars + 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(line: &str) -> Vec<String> {
        tokenize_line(line)
            .unwrap()
            .into_iter()
            .map(|a| String::from_utf8_lossy(&a).into_owned())
            .collect()
    }

    #[test]
    fn tokenizes_like_redis_cli() {
        assert_eq!(t("GET  key"), ["GET", "key"]);
        assert_eq!(t(r#"SET "a b" "x\ny""#), ["SET", "a b", "x\ny"]);
        assert_eq!(t("SET 'it\\'s' v"), ["SET", "it's", "v"]);
        assert_eq!(
            tokenize_line(r#"SET k "\xff\x00""#).unwrap()[2],
            vec![0xff, 0]
        );
        assert!(tokenize_line(r#"SET "oops"#).is_err());
        assert!(tokenize_line(r#"SET "a"b"#).is_err());
        assert!(tokenize_line("   ").unwrap().is_empty());
    }

    #[test]
    fn quote_round_trips() {
        for arg in [
            b"plain".to_vec(),
            b"with space".to_vec(),
            b"quote\"back\\slash".to_vec(),
            "tiếng việt".as_bytes().to_vec(),
            vec![0xff, 0x00, b'a'],
            b"".to_vec(),
            b"line\nbreak".to_vec(),
        ] {
            let rendered = render_command(&[b"X".to_vec(), arg.clone()]);
            let back = tokenize_line(&rendered).unwrap();
            assert_eq!(back[1], arg, "rendered: {rendered}");
        }
    }

    #[test]
    fn script_lines_report_char_offsets() {
        let lines = script_lines("# c\n  GET é\n\nSET a 1");
        assert_eq!(lines.len(), 2);
        assert_eq!(
            (lines[0].text, lines[0].start, lines[0].end),
            ("GET é", 6, 11)
        );
        assert_eq!((lines[1].text, lines[1].start), ("SET a 1", 13));
    }
}
