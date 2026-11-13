//! Minimal statement splitter for the SQL scripts DBStudio itself writes into a backup.
//!
//! It understands quoted text (`'..'`, `".."`, backticks, PostgreSQL dollar quoting), `--` and
//! `/* */` comments, and MySQL backslash escapes. It is deliberately not a SQL parser: it only
//! has to cut the generated `schema/*.sql` and `data/*.sql` files at their `;` terminators.

/// Split `script` into statements (without the terminating `;`). Leading comments stay with
/// the statement they precede; a trailing comment-only remainder is returned as a last item
/// so that `-- @end` markers are not lost.
pub fn split_statements(script: &str, backslash_escapes: bool) -> Vec<String> {
    let chars: Vec<char> = script.chars().collect();
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\'' | '"' | '`' => {
                cur.push(c);
                i += 1;
                while i < chars.len() {
                    let d = chars[i];
                    cur.push(d);
                    i += 1;
                    if d == '\\' && backslash_escapes && c != '`' && i < chars.len() {
                        cur.push(chars[i]);
                        i += 1;
                    } else if d == c {
                        if chars.get(i) == Some(&c) {
                            cur.push(c);
                            i += 1;
                        } else {
                            break;
                        }
                    }
                }
            }
            '-' if chars.get(i + 1) == Some(&'-') => {
                while i < chars.len() && chars[i] != '\n' {
                    cur.push(chars[i]);
                    i += 1;
                }
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                cur.push_str("/*");
                i += 2;
                while i < chars.len() {
                    if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                        cur.push_str("*/");
                        i += 2;
                        break;
                    }
                    cur.push(chars[i]);
                    i += 1;
                }
            }
            '$' => {
                // `$tag$ ... $tag$` (tag may be empty, must not start with a digit).
                let mut j = i + 1;
                while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_') {
                    j += 1;
                }
                let tag_ok = chars.get(j) == Some(&'$')
                    && !chars.get(i + 1).is_some_and(|c| c.is_ascii_digit());
                if tag_ok {
                    let tag: String = chars[i..=j].iter().collect();
                    cur.push_str(&tag);
                    i = j + 1;
                    while i < chars.len() {
                        if chars[i] == '$' && starts_with(&chars, i, &tag) {
                            cur.push_str(&tag);
                            i += tag.chars().count();
                            break;
                        }
                        cur.push(chars[i]);
                        i += 1;
                    }
                } else {
                    cur.push(c);
                    i += 1;
                }
            }
            ';' => {
                out.push(std::mem::take(&mut cur));
                i += 1;
            }
            _ => {
                cur.push(c);
                i += 1;
            }
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

fn starts_with(chars: &[char], at: usize, pat: &str) -> bool {
    pat.chars()
        .enumerate()
        .all(|(k, p)| chars.get(at + k) == Some(&p))
}

/// Remove leading blank lines and `--` comment lines; returns the markers found
/// (`-- @object NAME` => `Marker::Object`, `-- @end` => `Marker::End`) and the SQL that remains.
pub fn strip_markers(stmt: &str) -> (Vec<Marker>, &str) {
    let mut markers = Vec::new();
    let mut rest = stmt;
    loop {
        let trimmed = rest.trim_start();
        if let Some(line_and_rest) = trimmed.strip_prefix("--") {
            let (line, tail) = line_and_rest
                .split_once('\n')
                .unwrap_or((line_and_rest, ""));
            let line = line.trim();
            if let Some(name) = line.strip_prefix("@object ") {
                markers.push(Marker::Object(name.trim().to_string()));
            } else if line == "@end" {
                markers.push(Marker::End);
            }
            rest = tail;
        } else {
            return (markers, trimmed);
        }
    }
}

/// Structural comment written by the backup writer in front of an object's statements.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Marker {
    /// The following statements belong to this object.
    Object(String),
    /// End of the object's statements.
    End,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(script: &str) -> Vec<String> {
        split_statements(script, true)
            .into_iter()
            .map(|x| x.trim().to_string())
            .collect()
    }

    #[test]
    fn splits_simple_and_quoted() {
        assert_eq!(s("a;b;"), ["a", "b"]);
        assert_eq!(
            s("INSERT INTO t VALUES ('a;b', 'it''s');\nSELECT 1;"),
            ["INSERT INTO t VALUES ('a;b', 'it''s')", "SELECT 1"]
        );
        assert_eq!(s("SELECT \"a;b\", `c;d`;"), ["SELECT \"a;b\", `c;d`"]);
    }

    #[test]
    fn backslash_escapes_only_for_mysql() {
        assert_eq!(s("INSERT 'a\\';b';x;"), ["INSERT 'a\\';b'", "x"]);
        let pg = split_statements("INSERT 'a\\';b';x;", false);
        // Without backslash escapes the literal ends at the second quote.
        assert_eq!(pg.len(), 2, "{pg:?}");
        assert_eq!(pg[0].trim(), "INSERT 'a\\'");
    }

    #[test]
    fn comments_and_dollar_quotes() {
        assert_eq!(s("-- a;b\nSELECT 1;"), ["-- a;b\nSELECT 1"]);
        assert_eq!(
            s("/* ; */ SELECT 1; SELECT 2"),
            ["/* ; */ SELECT 1", "SELECT 2"]
        );
        assert_eq!(
            s("CREATE FUNCTION f() AS $body$ SELECT 1; $body$ LANGUAGE sql; SELECT 2;"),
            [
                "CREATE FUNCTION f() AS $body$ SELECT 1; $body$ LANGUAGE sql",
                "SELECT 2"
            ]
        );
        assert_eq!(s("SELECT $1; SELECT 2;"), ["SELECT $1", "SELECT 2"]);
        assert_eq!(s("SELECT $$a;b$$;"), ["SELECT $$a;b$$"]);
    }

    #[test]
    fn markers_are_parsed_in_order() {
        let (m, rest) =
            strip_markers("\n-- @end\n-- @object public.t\n-- plain\nCREATE TABLE t ()");
        assert_eq!(m, vec![Marker::End, Marker::Object("public.t".into())]);
        assert_eq!(rest, "CREATE TABLE t ()");
        let (m, rest) = strip_markers("-- @end");
        assert_eq!(m, vec![Marker::End]);
        assert_eq!(rest, "");
    }
}
