//! DDL generation for CockroachDB, built on its own `SHOW CREATE ALL ...` statements.
//!
//! `SHOW CREATE ALL TABLES` returns tables, views and sequences in dependency order with the
//! foreign keys (and their validation) after all tables, which maps directly onto the
//! `PreData` / `PostData` split. Schemas, types and routines come from their own statements.

use dbstudio_core::{
    DdlItem, DdlLanguage, DdlOptions, DdlRequest, DdlSection, ObjectKind, ObjectPath, Result,
};
use tokio_postgres::Client;

use crate::browse::{object_path, schema_path};
use crate::ddl_gen::{Cat, Target};
use crate::error::map_error;
use crate::session::PostgresSession;
use crate::sql::{qualified, quote_ident};

pub(crate) async fn generate(
    session: &PostgresSession,
    req: &DdlRequest,
    target: &Target,
) -> Result<Vec<DdlItem>> {
    let database = target
        .database
        .clone()
        .unwrap_or_else(|| session.login_db().to_string());
    let mut conn = session.shared.acquire_catalog(&database).await?;
    let opts = &req.options;
    let result = async {
        let client = &conn.client;
        let mut items = Vec::new();
        if target.selection.is_none() {
            for stmt in show_all(client, "SCHEMAS").await? {
                if let Some(item) = classify(&database, &stmt) {
                    if item.schema == "public" || !wants_schema(target, &item.schema) {
                        continue;
                    }
                    items.push(item.into_item(&database, opts));
                }
            }
            for stmt in show_all(client, "TYPES").await? {
                keep(&mut items, &database, target, opts, &stmt);
            }
        }
        for stmt in show_all(client, "TABLES").await? {
            keep(&mut items, &database, target, opts, &stmt);
        }
        if opts.include_routines && target.selection.is_none() {
            for stmt in show_all(client, "ROUTINES").await? {
                keep(&mut items, &database, target, opts, &stmt);
            }
        }
        Ok(items)
    }
    .await;
    if result.is_ok() {
        conn.mark_reusable();
    }
    result
}

async fn show_all(client: &Client, what: &str) -> Result<Vec<String>> {
    let rows = client
        .query(&format!("SHOW CREATE ALL {what}"), &[])
        .await
        .map_err(map_error)?;
    Ok(rows.iter().map(|r| r.get::<_, String>(0)).collect())
}

fn wants_schema(target: &Target, schema: &str) -> bool {
    target.schema.as_deref().is_none_or(|s| s == schema)
}

fn keep(items: &mut Vec<DdlItem>, database: &str, target: &Target, opts: &DdlOptions, stmt: &str) {
    let Some(parsed) = classify(database, stmt) else {
        return;
    };
    if !wants_schema(target, &parsed.schema) {
        return;
    }
    if let Some(cat) = parsed.cat {
        if !target.wants(cat, &parsed.schema, &parsed.name) {
            return;
        }
    }
    let allowed = match parsed.what {
        What::Sequence => opts.include_sequences,
        What::View => opts.include_views,
        What::Comment => opts.include_comments,
        What::ForeignKey => opts.include_foreign_keys,
        What::Routine => opts.include_routines,
        _ => true,
    };
    if allowed {
        items.push(parsed.into_item(database, opts));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum What {
    Schema,
    Type,
    Table,
    View,
    Sequence,
    Routine,
    Comment,
    ForeignKey,
    Other,
}

struct Parsed {
    what: What,
    cat: Option<Cat>,
    schema: String,
    name: String,
    sql: String,
}

impl Parsed {
    fn into_item(self, database: &str, opts: &DdlOptions) -> DdlItem {
        let (kind, section) = match self.what {
            What::Schema => (ObjectKind::Schema, DdlSection::PreData),
            What::Type => (ObjectKind::Type, DdlSection::PreData),
            What::Table => (ObjectKind::Table, DdlSection::PreData),
            What::View => (ObjectKind::View, DdlSection::PreData),
            What::Sequence => (ObjectKind::Sequence, DdlSection::PreData),
            What::Routine => (ObjectKind::Function, DdlSection::PreData),
            What::Comment | What::ForeignKey | What::Other => {
                (ObjectKind::Table, DdlSection::PostData)
            }
        };
        let path: ObjectPath = if self.what == What::Schema {
            schema_path(database, &self.name)
        } else {
            object_path(database, &self.schema, kind, &self.name)
        };
        // Views and routines name tables as `database.schema.name`; the DDL must replay into
        // another database too (cross-database references are off by default).
        let mut sql = strip_database(&self.sql, database);
        let drop = match self.what {
            What::Table => Some(format!(
                "DROP TABLE IF EXISTS {} CASCADE;\n",
                qualified(&self.schema, &self.name)
            )),
            What::View => Some(format!(
                "DROP VIEW IF EXISTS {} CASCADE;\n",
                qualified(&self.schema, &self.name)
            )),
            What::Sequence => Some(format!(
                "DROP SEQUENCE IF EXISTS {} CASCADE;\n",
                qualified(&self.schema, &self.name)
            )),
            What::Type => Some(format!(
                "DROP TYPE IF EXISTS {} CASCADE;\n",
                qualified(&self.schema, &self.name)
            )),
            _ => None,
        };
        if opts.if_not_exists {
            for kw in [
                "CREATE TABLE ",
                "CREATE VIEW ",
                "CREATE SEQUENCE ",
                "CREATE SCHEMA ",
            ] {
                if let Some(rest) = sql.strip_prefix(kw) {
                    sql = format!("{kw}IF NOT EXISTS {rest}");
                    break;
                }
            }
        }
        if opts.include_drop {
            if let Some(d) = drop {
                sql = format!("{d}{sql}");
            }
        }
        DdlItem {
            path,
            kind,
            section,
            sql,
            language: DdlLanguage::Sql,
            depends_on: Vec::new(),
        }
    }
}

/// Remove the `database.` qualifier of three-part names (outside string literals), plain or
/// quoted.
fn strip_database(sql: &str, database: &str) -> String {
    let quoted = format!("\"{}\"", database.replace('"', "\"\""));
    let is_ident = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    let mut out = String::with_capacity(sql.len());
    let mut in_str = false;
    let mut prev: Option<char> = None;
    let mut i = 0;
    while i < sql.len() {
        let ch = sql[i..].chars().next().expect("in range");
        if ch == '\'' {
            in_str = !in_str;
        }
        if !in_str && prev.is_none_or(|p| !(is_ident(p) || p == '.' || p == '"')) {
            if let Some(skip) = qualifier_len(&sql[i..], &quoted, database) {
                i += skip;
                prev = Some('.');
                continue;
            }
        }
        out.push(ch);
        i += ch.len_utf8();
        prev = Some(ch);
    }
    out
}

/// Length of a leading `database.` when it starts a three-part name `database.schema.name`.
fn qualifier_len(rest: &str, quoted: &str, database: &str) -> Option<usize> {
    for pat in [quoted, database] {
        let Some(after) = rest.strip_prefix(pat).and_then(|r| r.strip_prefix('.')) else {
            continue;
        };
        let end = if after.starts_with('"') {
            after[1..].find('"').map(|p| p + 2)
        } else {
            after.find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
        };
        if end.is_some_and(|e| after[e..].starts_with('.')) {
            return Some(pat.len() + 1);
        }
    }
    None
}

/// Classify one statement of a `SHOW CREATE ALL` result. Comment-only rows give `None`.
fn classify(_database: &str, stmt: &str) -> Option<Parsed> {
    let text = stmt.trim();
    // Rows that are only SQL comments (the "validate foreign keys" hint) carry no statement.
    let body: String = text
        .lines()
        .filter(|l| !l.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n");
    let body = body.trim();
    if body.is_empty() {
        return None;
    }
    let upper: String = body
        .chars()
        .take(48)
        .collect::<String>()
        .to_ascii_uppercase();
    let (what, cat, rest) = if let Some(r) = strip_ci(body, &upper, "CREATE TABLE ") {
        (What::Table, Some(Cat::Relation), r)
    } else if let Some(r) = strip_ci(body, &upper, "CREATE MATERIALIZED VIEW ") {
        (What::View, Some(Cat::Relation), r)
    } else if let Some(r) = strip_ci(body, &upper, "CREATE VIEW ") {
        (What::View, Some(Cat::Relation), r)
    } else if let Some(r) = strip_ci(body, &upper, "CREATE SEQUENCE ") {
        (What::Sequence, Some(Cat::Sequence), r)
    } else if let Some(r) = strip_ci(body, &upper, "CREATE TYPE ") {
        (What::Type, Some(Cat::Type), r)
    } else if let Some(r) = strip_ci(body, &upper, "CREATE SCHEMA ") {
        (What::Schema, None, r)
    } else if let Some(r) = strip_ci(body, &upper, "CREATE FUNCTION ")
        .or_else(|| strip_ci(body, &upper, "CREATE PROCEDURE "))
    {
        (What::Routine, Some(Cat::Routine), r)
    } else if let Some(r) = strip_ci(body, &upper, "ALTER TABLE ") {
        let fk = body.to_ascii_uppercase().contains("FOREIGN KEY")
            || body.to_ascii_uppercase().contains("VALIDATE CONSTRAINT");
        (
            if fk { What::ForeignKey } else { What::Other },
            Some(Cat::Relation),
            r,
        )
    } else if let Some(r) = strip_ci(body, &upper, "COMMENT ON TABLE ") {
        (What::Comment, Some(Cat::Relation), r)
    } else if let Some(r) = strip_ci(body, &upper, "COMMENT ON COLUMN ") {
        (What::Comment, Some(Cat::Relation), r)
    } else if let Some(r) = strip_ci(body, &upper, "COMMENT ON INDEX ") {
        (What::Comment, Some(Cat::Relation), r)
    } else {
        (What::Other, None, body)
    };
    let mut names = qualified_name(rest);
    // `db.schema.name` / `schema.name` / `name`; for a column comment the column is last.
    if body.to_ascii_uppercase().starts_with("COMMENT ON COLUMN") && names.len() > 1 {
        names.pop();
    }
    let (schema, name) = match what {
        What::Schema => (String::new(), names.last().cloned().unwrap_or_default()),
        _ => match names.as_slice() {
            [] => (String::new(), String::new()),
            [n] => ("public".to_string(), n.clone()),
            [.., s, n] => (s.clone(), n.clone()),
        },
    };
    let schema = if what == What::Schema {
        name.clone()
    } else {
        schema
    };
    let mut sql = body.to_string();
    if !sql.ends_with(';') {
        sql.push(';');
    }
    let _ = quote_ident;
    Some(Parsed {
        what,
        cat,
        schema,
        name,
        sql,
    })
}

fn strip_ci<'a>(body: &'a str, upper: &str, prefix: &str) -> Option<&'a str> {
    upper.starts_with(prefix).then(|| &body[prefix.len()..])
}

/// Dot separated identifiers at the start of `text` (quoted ones keep their content).
fn qualified_name(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = text.trim_start().chars().peekable();
    // `IF NOT EXISTS` is not emitted by SHOW CREATE.
    loop {
        let mut part = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            while let Some(c) = chars.next() {
                if c == '"' {
                    if chars.peek() == Some(&'"') {
                        chars.next();
                        part.push('"');
                    } else {
                        break;
                    }
                } else {
                    part.push(c);
                }
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c == '.' || c.is_whitespace() || c == '(' || c == ';' {
                    break;
                }
                part.push(c);
                chars.next();
            }
        }
        if part.is_empty() {
            break;
        }
        out.push(part);
        if chars.peek() == Some(&'.') {
            chars.next();
        } else {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_names() {
        assert_eq!(qualified_name("sv.parent (\n id"), vec!["sv", "parent"]);
        assert_eq!(
            qualified_name("\"my db\".\"a\"\"b\".t ADD"),
            vec!["my db", "a\"b", "t"]
        );
        assert_eq!(qualified_name("t;"), vec!["t"]);
    }

    #[test]
    fn strips_database_qualifier() {
        assert_eq!(
            strip_database(
                "CREATE VIEW sv.v AS SELECT a FROM dbstudio.sv.t JOIN dbstudio.sv.u",
                "dbstudio"
            ),
            "CREATE VIEW sv.v AS SELECT a FROM sv.t JOIN sv.u"
        );
        assert_eq!(
            strip_database(
                "SELECT \"my db\".s.t, x.dbstudio.y, 'dbstudio.a.b'",
                "my db"
            ),
            "SELECT s.t, x.dbstudio.y, 'dbstudio.a.b'"
        );
        // A two-part name whose first part equals the database stays.
        assert_eq!(
            strip_database("SELECT * FROM dbstudio.t", "dbstudio"),
            "SELECT * FROM dbstudio.t"
        );
    }

    #[test]
    fn classifies_statements() {
        let p = classify("d", "CREATE TABLE sv.child (\n id INT8\n);").unwrap();
        assert_eq!(
            (p.what, p.schema.as_str(), p.name.as_str()),
            (What::Table, "sv", "child")
        );
        let p = classify(
            "d",
            "ALTER TABLE sv.child ADD CONSTRAINT x FOREIGN KEY (pid) REFERENCES sv.parent(id);",
        )
        .unwrap();
        assert_eq!((p.what, p.name.as_str()), (What::ForeignKey, "child"));
        let p = classify("d", "COMMENT ON COLUMN sv.parent.name IS 'nm';").unwrap();
        assert_eq!(
            (p.what, p.schema.as_str(), p.name.as_str()),
            (What::Comment, "sv", "parent")
        );
        let p = classify("d", "CREATE SCHEMA sv;").unwrap();
        assert_eq!((p.what, p.schema.as_str()), (What::Schema, "sv"));
        assert!(classify("d", "-- Validate foreign key constraints.").is_none());
        let p = classify("d", "CREATE TABLE tt (a INT8);").unwrap();
        assert_eq!(p.schema, "public");
    }
}
