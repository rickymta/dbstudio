//! Partial restore of a logical SQL backup (`formatVersion >= 3`).
//!
//! The script written by the export engine carries comment markers in front of every block:
//! `-- dbstudio:object kind=table schema="public" name="t" phase=pre|data|post` and
//! `-- dbstudio:common` (statements that belong to no object: session settings). A partial
//! restore runs the common blocks and the blocks of the chosen objects (plus the supporting
//! objects they need: schemas, sequences, types), and drops foreign keys that point at a table
//! that is not restored.

use std::collections::BTreeSet;

use dbstudio_core::{Error, ObjectKind, Result};

use crate::{BackupObject, BackupPhase};

/// A marker line of the script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Marker {
    /// `-- dbstudio:common`.
    Common,
    /// `-- dbstudio:object ...`.
    Object {
        kind: ObjectKind,
        schema: Option<String>,
        name: String,
        phase: BackupPhase,
    },
}

const OBJECT_PREFIX: &str = "-- dbstudio:object ";
const COMMON_LINE: &str = "-- dbstudio:common";

/// Whether `line` could be a marker (cheap test before the splitter state is inspected).
pub(crate) fn looks_like_marker(line: &str) -> bool {
    line.starts_with("-- dbstudio:")
}

/// Parse a marker line; `None` when it is not one, is malformed or does not carry `nonce`
/// (the random token of this file, kept in the preamble): a forged line inside data has none.
pub(crate) fn parse_marker(line: &str, nonce: &str) -> Option<Marker> {
    let line = line.trim_end_matches(['\r', '\n']);
    let (is_common, fields) = if let Some(r) = line.strip_prefix(COMMON_LINE) {
        if !r.is_empty() && !r.starts_with(' ') {
            return None;
        }
        (true, r.trim_start())
    } else {
        (false, line.strip_prefix(OBJECT_PREFIX)?)
    };
    let mut rest = fields;
    let (mut kind, mut schema, mut name, mut phase, mut seen_nonce) =
        (None, None, None, None, None);
    while !rest.is_empty() {
        let (key, after) = rest.split_once('=')?;
        let key = key.trim();
        let (value, tail) = if let Some(quoted) = after.strip_prefix('"') {
            // A JSON string: find its end honouring escapes.
            let mut end = None;
            let mut escaped = false;
            for (i, c) in quoted.char_indices() {
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == '"' {
                    end = Some(i);
                    break;
                }
            }
            let end = end?;
            let text: String = serde_json::from_str(&format!("\"{}\"", &quoted[..end])).ok()?;
            (text, quoted[end + 1..].trim_start())
        } else {
            let (word, tail) = after.split_once(' ').unwrap_or((after, ""));
            (word.to_string(), tail.trim_start())
        };
        match key {
            "kind" => kind = Some(value),
            "schema" => schema = Some(value),
            "name" => name = Some(value),
            "phase" => phase = Some(value),
            "nonce" => seen_nonce = Some(value),
            _ => {}
        }
        rest = tail;
    }
    if nonce.is_empty() || seen_nonce.as_deref() != Some(nonce) {
        return None;
    }
    if is_common {
        return Some(Marker::Common);
    }
    let kind: ObjectKind = serde_json::from_value(serde_json::Value::String(kind?)).ok()?;
    let phase = match phase?.as_str() {
        "pre" => BackupPhase::Pre,
        "data" => BackupPhase::Data,
        "post" => BackupPhase::Post,
        _ => return None,
    };
    Some(Marker::Object {
        kind,
        schema,
        name: name?,
        phase,
    })
}

/// Objects chosen for a partial restore.
#[derive(Debug, Clone)]
pub(crate) struct Selection {
    chosen: BTreeSet<(Option<String>, String)>,
    /// Display names of the supporting objects that come along.
    pub support: Vec<String>,
    /// Tables and views that are restored (for the foreign key check).
    tables: Vec<(Option<String>, String)>,
}

fn is_support(kind: ObjectKind) -> bool {
    matches!(
        kind,
        ObjectKind::Schema | ObjectKind::Sequence | ObjectKind::Type | ObjectKind::Database
    )
}

fn display(schema: &Option<String>, name: &str) -> String {
    match schema {
        Some(s) => format!("{s}.{name}"),
        None => name.to_string(),
    }
}

fn is_table_like(kind: ObjectKind) -> bool {
    matches!(
        kind,
        ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView
    )
}

impl Selection {
    /// Resolve `selectors` (`name` or `schema.name`) against the objects of the backup.
    pub fn resolve(objects: &[BackupObject], selectors: &[String]) -> Result<Selection> {
        let mut chosen = BTreeSet::new();
        let mut tables = Vec::new();
        for sel in selectors {
            let mut found = false;
            for o in objects {
                let qualified = display(&o.schema, &o.name);
                if o.name == *sel || qualified == *sel {
                    found = true;
                    chosen.insert((o.schema.clone(), o.name.clone()));
                    if is_table_like(o.kind) {
                        tables.push((o.schema.clone(), o.name.clone()));
                    }
                }
            }
            if !found {
                return Err(Error::Invalid(format!(
                    "the object {sel:?} is not in this backup"
                )));
            }
        }
        let support = objects
            .iter()
            .filter(|o| is_support(o.kind) && !chosen.contains(&(o.schema.clone(), o.name.clone())))
            .map(|o| display(&o.schema, &o.name))
            .collect();
        Ok(Selection {
            chosen,
            support,
            tables,
        })
    }

    /// Whether the block of this object runs.
    pub fn includes(&self, kind: ObjectKind, schema: &Option<String>, name: &str) -> bool {
        is_support(kind) || self.chosen.contains(&(schema.clone(), name.to_string()))
    }

    /// Whether `(schema, name)` (as written in a statement) is a restored table or view.
    pub fn has_table(&self, schema: Option<&str>, name: &str) -> bool {
        self.tables
            .iter()
            .any(|(s, n)| n == name && (schema.is_none() || s.as_deref() == schema))
    }
}

/// The table a statement points at with `REFERENCES`, when it has such a clause.
pub(crate) fn referenced_table(stmt: &str) -> Option<(Option<String>, String)> {
    let upper = stmt.to_ascii_uppercase();
    let mut from = 0;
    while let Some(pos) = upper[from..].find("REFERENCES") {
        let at = from + pos;
        let before_ok = at == 0
            || upper.as_bytes()[at - 1].is_ascii_whitespace()
            || upper.as_bytes()[at - 1] == b')';
        let after = at + "REFERENCES".len();
        let after_ok = upper
            .as_bytes()
            .get(after)
            .is_some_and(|c| c.is_ascii_whitespace());
        if before_ok && after_ok {
            if let Some(name) = parse_qualified(stmt[after..].trim_start()) {
                return Some(name);
            }
        }
        from = after;
    }
    None
}

/// The table of an `INSERT INTO <table> ...` statement.
pub(crate) fn insert_target(stmt: &str) -> Option<(Option<String>, String)> {
    // Marker and other comment lines may lead the statement.
    let mut t = stmt.trim_start();
    while t.starts_with("--") {
        t = t.split_once('\n').map_or("", |(_, r)| r).trim_start();
    }
    let head = t.get(..11)?;
    if !head.eq_ignore_ascii_case("INSERT INTO") {
        return None;
    }
    let rest = &t[11..];
    if !rest.starts_with(|c: char| c.is_whitespace()) {
        return None;
    }
    parse_qualified(rest.trim_start())
}

/// `a`, `"a"."b"`, `` `a`.`b` `` or `[a].[b]` at the start of `s`.
fn parse_qualified(s: &str) -> Option<(Option<String>, String)> {
    let mut parts: Vec<String> = Vec::new();
    let mut chars = s.char_indices().peekable();
    loop {
        let (_, c) = *chars.peek()?;
        let ident = match c {
            '"' | '`' | '[' => {
                let close = if c == '[' { ']' } else { c };
                chars.next();
                let mut text = String::new();
                loop {
                    let (_, ch) = chars.next()?;
                    if ch == close {
                        // A doubled closing quote is an escaped quote.
                        if close != ']' && chars.peek().is_some_and(|(_, n)| *n == close) {
                            chars.next();
                            text.push(close);
                            continue;
                        }
                        break;
                    }
                    text.push(ch);
                }
                text
            }
            c if c.is_alphanumeric() || c == '_' || c == '$' => {
                let mut text = String::new();
                while let Some((_, ch)) = chars.peek() {
                    if ch.is_alphanumeric() || *ch == '_' || *ch == '$' {
                        text.push(*ch);
                        chars.next();
                    } else {
                        break;
                    }
                }
                text
            }
            _ => return None,
        };
        parts.push(ident);
        if chars.peek().is_some_and(|(_, n)| *n == '.') {
            chars.next();
        } else {
            break;
        }
    }
    let name = parts.pop()?;
    Some((parts.pop(), name))
}

/// Whether a statement left in the splitter is only blanks and `--` comment lines, that is the
/// splitter sits at a statement boundary.
pub(crate) fn only_comments(text: &str) -> bool {
    text.lines().all(|l| {
        let t = l.trim();
        t.is_empty() || t.starts_with("--")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(kind: ObjectKind, schema: Option<&str>, name: &str) -> BackupObject {
        BackupObject {
            kind,
            schema: schema.map(str::to_string),
            name: name.into(),
            phases: vec![BackupPhase::Pre],
            rows: None,
        }
    }

    #[test]
    fn markers_parse_with_escapes() {
        let n = "0123456789abcdef";
        assert_eq!(
            parse_marker("-- dbstudio:common nonce=0123456789abcdef\n", n),
            Some(Marker::Common)
        );
        // No nonce, or a wrong one: plain text.
        assert_eq!(parse_marker("-- dbstudio:common\n", n), None);
        assert_eq!(
            parse_marker("-- dbstudio:common nonce=ffffffffffffffff", n),
            None
        );
        assert_eq!(
            parse_marker(
                r#"-- dbstudio:object kind=table name="t" phase=data nonce=ffffffffffffffff"#,
                n
            ),
            None
        );
        assert_eq!(
            parse_marker(r#"-- dbstudio:object kind=table name="t" phase=data"#, n),
            None
        );
        assert_eq!(
            parse_marker(
                r#"-- dbstudio:object kind=table schema="public" name="we\"ird t" phase=data nonce=0123456789abcdef"#,
                n
            ),
            Some(Marker::Object {
                kind: ObjectKind::Table,
                schema: Some("public".into()),
                name: "we\"ird t".into(),
                phase: BackupPhase::Data,
            })
        );
        assert_eq!(
            parse_marker(
                r#"-- dbstudio:object kind=view name="v" phase=pre nonce=0123456789abcdef"#,
                n
            ),
            Some(Marker::Object {
                kind: ObjectKind::View,
                schema: None,
                name: "v".into(),
                phase: BackupPhase::Pre,
            })
        );
        assert_eq!(parse_marker("-- just a comment", n), None);
        assert_eq!(
            parse_marker(
                "-- dbstudio:object kind=table name=\"t\" phase=x nonce=0123456789abcdef",
                n
            ),
            None
        );
    }

    #[test]
    fn selection_resolves_names_and_keeps_support_objects() {
        let objects = vec![
            obj(ObjectKind::Schema, None, "app"),
            obj(ObjectKind::Table, Some("app"), "a"),
            obj(ObjectKind::Table, Some("app"), "b"),
            obj(ObjectKind::Sequence, Some("app"), "a_id_seq"),
        ];
        let sel = Selection::resolve(&objects, &["app.a".into()]).unwrap();
        assert!(sel.includes(ObjectKind::Table, &Some("app".into()), "a"));
        assert!(!sel.includes(ObjectKind::Table, &Some("app".into()), "b"));
        assert!(sel.includes(ObjectKind::Sequence, &Some("app".into()), "other"));
        assert_eq!(sel.support, ["app", "app.a_id_seq"]);
        assert!(sel.has_table(Some("app"), "a") && !sel.has_table(None, "b"));
        assert!(Selection::resolve(&objects, &["nope".into()]).is_err());
        // A bare name matches too.
        assert!(Selection::resolve(&objects, &["b".into()]).is_ok());
    }

    #[test]
    fn references_are_found_in_every_quoting_style() {
        let r = |s| referenced_table(s);
        assert_eq!(
            r(
                "ALTER TABLE \"app\".\"b\" ADD CONSTRAINT fk FOREIGN KEY (a_id) REFERENCES \"app\".\"a\" (id);"
            ),
            Some((Some("app".into()), "a".into()))
        );
        assert_eq!(
            r("ALTER TABLE `b` ADD CONSTRAINT `fk` FOREIGN KEY (`a_id`) REFERENCES `a` (`id`);"),
            Some((None, "a".into()))
        );
        assert_eq!(
            r("ALTER TABLE [dbo].[b] ADD FOREIGN KEY ([x]) REFERENCES [dbo].[a] ([id]);"),
            Some((Some("dbo".into()), "a".into()))
        );
        assert_eq!(r("CREATE INDEX i ON t (a);"), None);
        assert_eq!(r("COMMENT ON COLUMN t.references IS 'x'"), None);
    }

    #[test]
    fn insert_targets() {
        assert_eq!(
            insert_target("INSERT INTO [dbo].[child] ([id]) VALUES (1)"),
            Some((Some("dbo".into()), "child".into()))
        );
        assert_eq!(
            insert_target("  insert into t (a) values (1)"),
            Some((None, "t".into()))
        );
        assert_eq!(
            insert_target(
                "-- dbstudio:object kind=table name=\"t\" phase=data\nINSERT INTO t (a) VALUES (1)"
            ),
            Some((None, "t".into()))
        );
        assert_eq!(insert_target("INSERTINTO t"), None);
        assert_eq!(insert_target("CREATE TABLE t (a int)"), None);
    }

    #[test]
    fn leftover_text_is_a_boundary_only_when_blank() {
        assert!(only_comments("  \n-- note\n"));
        assert!(!only_comments("INSERT INTO t VALUES ('a\n"));
    }
}
