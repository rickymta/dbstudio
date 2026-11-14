//! DDL generation for CockroachDB, built on its own `SHOW CREATE ALL ...` statements.
//!
//! `SHOW CREATE ALL TABLES` returns tables, views and sequences in dependency order with the
//! foreign keys (and their validation) after all tables, which maps directly onto the
//! `PreData` / `PostData` split. Schemas, types, routines and triggers come from their own
//! statements. Not supported: grants (`include_grants` is refused with `Unsupported`).

use dbstudio_core::{
    DdlItem, DdlLanguage, DdlOptions, DdlRequest, DdlSection, Error, ObjectKind, ObjectPath,
    Result, ScriptDialect, folder,
};
use dbstudio_sql::lexer::{Token, TokenKind, tokenize};
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
    let opts = &req.options;
    if opts.include_grants {
        return Err(Error::Unsupported("ddl_grants on CockroachDB".into()));
    }
    let database = target
        .database
        .clone()
        .unwrap_or_else(|| session.login_db().to_string());
    let triggers = opts.include_triggers && session.shared.flavor().capabilities().triggers;
    let mut conn = session.shared.acquire_catalog(&database).await?;
    let result = async {
        let client = &conn.client;
        let mut parsed: Vec<Parsed> = Vec::new();
        let mut collect = |stmts: Vec<String>| {
            parsed.extend(stmts.iter().filter_map(|s| classify(s)));
        };
        if target.selection.is_none() {
            collect(show_all(client, "SCHEMAS").await?);
        }
        collect(show_all(client, "TYPES").await?);
        collect(show_all(client, "TABLES").await?);
        if opts.include_routines {
            collect(show_all(client, "ROUTINES").await?);
        }
        if triggers {
            collect(show_all(client, "TRIGGERS").await?);
        }
        let mut items = build(&database, target, opts, parsed);
        if opts.include_sequences {
            // `SHOW CREATE ALL` has no current values: the next `nextval` after a restore would
            // repeat numbers already used.
            let values = sequence_values(client, &items).await?;
            items.extend(values);
        }
        Ok(items)
    }
    .await;
    if result.is_ok() {
        conn.mark_reusable();
    }
    result
}

/// `setval` statements (PostData) for the sequences among `items` that have been used.
async fn sequence_values(client: &Client, items: &[DdlItem]) -> Result<Vec<DdlItem>> {
    let mut out = Vec::new();
    for item in items.iter().filter(|i| {
        i.kind == ObjectKind::Sequence
            && i.sql
                .trim_start()
                .to_ascii_uppercase()
                .starts_with("CREATE SEQUENCE")
    }) {
        let mut names = item.path.segments.iter().rev();
        let (Some(name), Some(schema)) = (
            names.next().filter(|s| s.kind == ObjectKind::Sequence),
            names.find(|s| s.kind == ObjectKind::Schema),
        ) else {
            continue;
        };
        let qualified = format!("{}.{}", quote_ident(&schema.name), quote_ident(&name.name));
        let row = client
            .query_opt(
                &format!("SELECT last_value, is_called FROM {qualified}"),
                &[],
            )
            .await
            .map_err(map_error)?;
        let Some(row) = row else { continue };
        let (last, called): (i64, bool) = (row.get(0), row.get(1));
        if !called {
            continue;
        }
        let qualified_literal = qualified.replace('\'', "''");
        out.push(DdlItem {
            path: item.path.clone(),
            kind: ObjectKind::Sequence,
            section: DdlSection::PostData,
            sql: format!("SELECT setval('{qualified_literal}', {last}, true);"),
            language: DdlLanguage::Sql,
            depends_on: vec![],
        });
    }
    Ok(out)
}

async fn show_all(client: &Client, what: &str) -> Result<Vec<String>> {
    let rows = client
        .query(&format!("SHOW CREATE ALL {what}"), &[])
        .await
        .map_err(map_error)?;
    Ok(rows.iter().map(|r| r.get::<_, String>(0)).collect())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum What {
    Schema,
    Type,
    Table,
    View,
    MaterializedView,
    Sequence,
    Routine,
    Trigger,
    Comment,
    ForeignKey,
    Other,
}

#[derive(Debug)]
struct Parsed {
    what: What,
    schema: String,
    name: String,
    sql: String,
    /// Table a trigger / foreign key / comment belongs to; table referenced by a foreign key.
    references: Option<(String, String)>,
}

fn wants_routine(target: &Target, schema: &str, name: &str) -> bool {
    match &target.selection {
        None => true,
        Some(sel) => sel.iter().any(|(c, s, n)| {
            *c == Cat::Routine && s == schema && n.split('(').next() == Some(name)
        }),
    }
}

/// Filter by scope and selection, apply the options and order the items.
fn build(database: &str, target: &Target, opts: &DdlOptions, parsed: Vec<Parsed>) -> Vec<DdlItem> {
    let in_scope = |p: &Parsed| target.schema.as_deref().is_none_or(|s| s == p.schema);
    let rel_wanted = |s: &str, n: &str| target.wants(Cat::Relation, s, n);
    // Tables, views and sequences first: types are kept when something kept uses them.
    let mut kept: Vec<Parsed> = Vec::new();
    let mut types: Vec<Parsed> = Vec::new();
    let mut others: Vec<Parsed> = Vec::new();
    for p in parsed {
        if p.what != What::Schema && !in_scope(&p) {
            continue;
        }
        match p.what {
            What::Schema => {
                if p.name != "public" && target.schema.as_deref().is_none_or(|s| s == p.name) {
                    kept.push(p);
                }
            }
            What::Type => types.push(p),
            What::Table | What::View | What::MaterializedView => {
                if rel_wanted(&p.schema, &p.name) && (p.what == What::Table || opts.include_views) {
                    kept.push(p);
                }
            }
            What::Sequence => {
                if target.wants(Cat::Sequence, &p.schema, &p.name) && opts.include_sequences {
                    kept.push(p);
                }
            }
            What::Routine => {
                if opts.include_routines && wants_routine(target, &p.schema, &p.name) {
                    others.push(p);
                }
            }
            What::Trigger | What::Comment | What::ForeignKey | What::Other => {
                let table_ok = rel_wanted(&p.schema, &p.name);
                let allowed = match p.what {
                    What::Trigger => opts.include_triggers,
                    What::Comment => opts.include_comments,
                    What::ForeignKey => {
                        opts.include_foreign_keys
                            && p.references.as_ref().is_none_or(|(s, n)| rel_wanted(s, n))
                    }
                    _ => true,
                };
                if table_ok && allowed {
                    others.push(p);
                }
            }
        }
    }
    // Types: all of them without a selection; else the selected ones and those used by the
    // kept relations and routines.
    let used_text: String = kept
        .iter()
        .filter(|p| p.what != What::Schema)
        .chain(others.iter().filter(|p| p.what == What::Routine))
        .map(|p| p.sql.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let types: Vec<Parsed> = types
        .into_iter()
        .filter(|t| {
            target.selection.is_none()
                || target.wants(Cat::Type, &t.schema, &t.name)
                || used_text.contains(&format!("{}.{}", t.schema, t.name))
        })
        .collect();

    let mut items: Vec<DdlItem> = Vec::new();
    let (schemas, rels): (Vec<_>, Vec<_>) = kept.into_iter().partition(|p| p.what == What::Schema);
    for p in schemas {
        items.push(to_item(database, opts, p, true));
    }
    if opts.include_drop {
        // Drops come first, dependants before what they depend on: views, tables and
        // sequences (in reverse creation order), then the types.
        for p in rels.iter().rev() {
            if let Some(sql) = drop_sql(opts, p) {
                items.push(raw_item(database, p, sql));
            }
        }
        for p in types.iter().rev() {
            if let Some(sql) = drop_sql(opts, p) {
                items.push(raw_item(database, p, sql));
            }
        }
    }
    for p in types.into_iter().chain(rels) {
        items.push(to_item(database, opts, p, false));
    }
    // Routines may use tables and types: after them. Foreign keys, comments and triggers
    // (PostData) come last, in the order of the server.
    let (routines, post): (Vec<_>, Vec<_>) =
        others.into_iter().partition(|p| p.what == What::Routine);
    for p in routines.into_iter().chain(post) {
        items.push(to_item(database, opts, p, false));
    }
    items
}

fn kind_of(what: What) -> ObjectKind {
    match what {
        What::Schema => ObjectKind::Schema,
        What::Type => ObjectKind::Type,
        What::View => ObjectKind::View,
        What::MaterializedView => ObjectKind::MaterializedView,
        What::Sequence => ObjectKind::Sequence,
        What::Routine => ObjectKind::Function,
        What::Trigger => ObjectKind::Trigger,
        _ => ObjectKind::Table,
    }
}

fn path_of(database: &str, p: &Parsed) -> ObjectPath {
    match p.what {
        What::Schema => schema_path(database, &p.name),
        What::Trigger => object_path(database, &p.schema, ObjectKind::Table, &p.name)
            .child(ObjectKind::Folder, folder::TRIGGERS)
            .child(
                ObjectKind::Trigger,
                p.references
                    .as_ref()
                    .map(|r| r.1.clone())
                    .unwrap_or_default(),
            ),
        _ => object_path(database, &p.schema, kind_of(p.what), &p.name),
    }
}

fn section_of(what: What) -> DdlSection {
    match what {
        What::Schema
        | What::Type
        | What::Table
        | What::View
        | What::MaterializedView
        | What::Sequence
        | What::Routine => DdlSection::PreData,
        _ => DdlSection::PostData,
    }
}

fn raw_item(database: &str, p: &Parsed, sql: String) -> DdlItem {
    DdlItem {
        path: path_of(database, p),
        kind: kind_of(p.what),
        section: DdlSection::PreData,
        sql,
        language: DdlLanguage::Sql,
        depends_on: Vec::new(),
    }
}

/// `DROP ...` of an object (CockroachDB has no `DROP TYPE ... CASCADE`, nor a drop for the
/// unnamed-signature routines: those are replaced instead).
fn drop_sql(opts: &DdlOptions, p: &Parsed) -> Option<String> {
    let name = if opts.qualify_schema {
        qualified(&p.schema, &p.name)
    } else {
        quote_ident(&p.name)
    };
    Some(match p.what {
        What::Table => format!("DROP TABLE IF EXISTS {name} CASCADE;"),
        What::View => format!("DROP VIEW IF EXISTS {name} CASCADE;"),
        What::MaterializedView => format!("DROP MATERIALIZED VIEW IF EXISTS {name} CASCADE;"),
        What::Sequence => format!("DROP SEQUENCE IF EXISTS {name} CASCADE;"),
        What::Type => format!("DROP TYPE IF EXISTS {name};"),
        _ => return None,
    })
}

fn to_item(database: &str, opts: &DdlOptions, p: Parsed, schema_item: bool) -> DdlItem {
    // Views and routines name tables as `database.schema.name`; the DDL must replay into
    // another database too (cross-database references are off by default).
    let mut sql = strip_qualifier(&p.sql, database, 3);
    if !opts.qualify_schema && !schema_item && !p.schema.is_empty() {
        sql = strip_qualifier(&sql, &p.schema, 2);
    }
    if !opts.include_indexes {
        sql = drop_inline_indexes(&sql);
    }
    if p.what == What::MaterializedView {
        sql = drop_hidden_rowid_column(&sql);
    }
    if opts.if_not_exists || (schema_item && opts.include_drop) {
        for kw in [
            "CREATE TABLE ",
            "CREATE VIEW ",
            "CREATE MATERIALIZED VIEW ",
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
        // Routines have no drop here: replace them.
        if let Some(rest) = sql.strip_prefix("CREATE FUNCTION ") {
            sql = format!("CREATE OR REPLACE FUNCTION {rest}");
        } else if let Some(rest) = sql.strip_prefix("CREATE PROCEDURE ") {
            sql = format!("CREATE OR REPLACE PROCEDURE {rest}");
        }
    }
    DdlItem {
        path: path_of(database, &p),
        kind: kind_of(p.what),
        section: section_of(p.what),
        sql,
        language: DdlLanguage::Sql,
        depends_on: Vec::new(),
    }
}

/// `SHOW CREATE` of a materialized view lists the hidden `rowid` among its columns, which the
/// statement cannot be replayed with ("specifies 2 column names, but data source has 1").
fn drop_hidden_rowid_column(sql: &str) -> String {
    let lines: Vec<&str> = sql.lines().collect();
    let Some(i) = lines
        .iter()
        .position(|l| matches!(l.trim(), "rowid" | "rowid,"))
    else {
        return sql.to_string();
    };
    let mut owned: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    owned.remove(i);
    if i > 0 && lines[i].trim() == "rowid" {
        // It was the last column: the previous one loses its comma.
        owned[i - 1] = owned[i - 1].trim_end().trim_end_matches(',').to_string();
    }
    owned.join("\n")
}

/// Remove the non-unique inline `INDEX ...` lines of a `CREATE TABLE`.
fn drop_inline_indexes(sql: &str) -> String {
    if !sql.starts_with("CREATE TABLE") {
        return sql.to_string();
    }
    let mut lines: Vec<&str> = sql
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            !(t.starts_with("INDEX ") || t.starts_with("INVERTED INDEX "))
        })
        .collect();
    // The last element before the closing `)` must not end with a comma.
    if let Some(close) = lines.iter().rposition(|l| l.trim_start().starts_with(')')) {
        if close > 0 {
            let prev = lines[close - 1]
                .trim_end()
                .trim_end_matches(',')
                .to_string();
            let mut owned: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
            owned[close - 1] = prev;
            return owned.join("\n");
        }
    }
    lines.retain(|_| true);
    lines.join("\n")
}

// ---------------------------------------------------------------------------------------------
// Names
// ---------------------------------------------------------------------------------------------

/// Text of an identifier token (unquoted), `None` for other tokens.
fn ident_text(chars: &[char], t: &Token) -> Option<String> {
    let raw: String = chars[t.range.start as usize..t.range.end as usize]
        .iter()
        .collect();
    match t.kind {
        TokenKind::Word => Some(raw),
        TokenKind::QuotedIdent => {
            let inner = raw.strip_prefix('"')?.strip_suffix('"')?;
            Some(inner.replace("\"\"", "\""))
        }
        _ => None,
    }
}

/// Remove a leading qualifier `first.` from names that have at least `parts` dotted parts
/// (`database.schema.name` with 3, `schema.name` with 2), outside strings and comments. The
/// text comes from the lexer of the Postgres dialect, so quoted identifiers such as `"it's"`
/// and strings such as `E'\''` are never misread.
fn strip_qualifier(sql: &str, first: &str, parts: usize) -> String {
    let chars: Vec<char> = sql.chars().collect();
    let tokens: Vec<Token> = tokenize(ScriptDialect::Postgres, sql);
    let sig: Vec<usize> = (0..tokens.len())
        .filter(|&i| !tokens[i].kind.is_trivia())
        .collect();
    let is_dot = |t: &Token| t.kind == TokenKind::Punct && chars[t.range.start as usize] == '.';
    let mut skip: Vec<(usize, usize)> = Vec::new();
    let mut k = 0;
    while k < sig.len() {
        let t = &tokens[sig[k]];
        let chain_start = k == 0 || !is_dot(&tokens[sig[k - 1]]);
        if chain_start && ident_text(&chars, t).as_deref() == Some(first) {
            // Count the identifiers of the dotted chain starting here.
            let mut n = 1;
            let mut j = k;
            while j + 2 < sig.len()
                && is_dot(&tokens[sig[j + 1]])
                && ident_text(&chars, &tokens[sig[j + 2]]).is_some()
            {
                n += 1;
                j += 2;
            }
            if n >= parts {
                let dot = &tokens[sig[k + 1]];
                skip.push((t.range.start as usize, dot.range.end as usize));
            }
            k = j + 1;
            continue;
        }
        k += 1;
    }
    let mut out = String::with_capacity(sql.len());
    for (i, c) in chars.iter().enumerate() {
        if !skip.iter().any(|(a, b)| i >= *a && i < *b) {
            out.push(*c);
        }
    }
    out
}

/// Identifiers of the dotted name that starts `text` (quoted ones unquoted).
fn qualified_name(text: &str) -> Vec<String> {
    let text = text.trim_start();
    let chars: Vec<char> = text.chars().collect();
    let tokens = tokenize(ScriptDialect::Postgres, text);
    let mut out = Vec::new();
    let mut expect_ident = true;
    for t in tokens.iter().filter(|t| !t.kind.is_trivia()) {
        if expect_ident {
            match ident_text(&chars, t) {
                Some(id) => out.push(id),
                None => break,
            }
            expect_ident = false;
        } else if t.kind == TokenKind::Punct && chars[t.range.start as usize] == '.' {
            expect_ident = true;
        } else {
            break;
        }
    }
    out
}

fn split_schema(mut names: Vec<String>) -> (String, String) {
    match names.len() {
        0 => (String::new(), String::new()),
        1 => ("public".to_string(), names.remove(0)),
        n => (names[n - 2].clone(), names[n - 1].clone()),
    }
}

/// Classify one statement of a `SHOW CREATE ALL` result. Comment-only rows give `None`.
fn classify(stmt: &str) -> Option<Parsed> {
    let body: String = stmt
        .trim()
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
        .take(60)
        .collect::<String>()
        .to_ascii_uppercase();
    let strip = |prefix: &str| upper.starts_with(prefix).then(|| &body[prefix.len()..]);
    let mut references = None;
    let (what, rest) = if let Some(r) = strip("CREATE TABLE ") {
        (What::Table, r)
    } else if let Some(r) = strip("CREATE MATERIALIZED VIEW ") {
        (What::MaterializedView, r)
    } else if let Some(r) = strip("CREATE VIEW ") {
        (What::View, r)
    } else if let Some(r) = strip("CREATE SEQUENCE ") {
        (What::Sequence, r)
    } else if let Some(r) = strip("CREATE TYPE ") {
        (What::Type, r)
    } else if let Some(r) = strip("CREATE SCHEMA ") {
        (What::Schema, r)
    } else if let Some(r) = strip("CREATE FUNCTION ").or_else(|| strip("CREATE PROCEDURE ")) {
        (What::Routine, r)
    } else if let Some(r) = strip("CREATE TRIGGER ") {
        (What::Trigger, r)
    } else if let Some(r) = strip("ALTER TABLE ") {
        let up = body.to_ascii_uppercase();
        let fk = up.contains("FOREIGN KEY") || up.contains("VALIDATE CONSTRAINT");
        if let Some(i) = up.find(" REFERENCES ") {
            references = Some(split_schema(qualified_name(
                &body[i + " REFERENCES ".len()..],
            )));
        }
        (if fk { What::ForeignKey } else { What::Other }, r)
    } else if let Some(r) = strip("COMMENT ON TABLE ").or_else(|| strip("COMMENT ON INDEX ")) {
        (What::Comment, r)
    } else if let Some(r) = strip("COMMENT ON COLUMN ") {
        (What::Comment, r)
    } else {
        (What::Other, body)
    };
    let mut names = qualified_name(rest);
    if upper.starts_with("COMMENT ON COLUMN") && names.len() > 1 {
        names.pop();
    }
    let (schema, name) = match what {
        What::Schema => {
            let n = names.last().cloned().unwrap_or_default();
            (n.clone(), n)
        }
        What::Trigger => {
            // `CREATE TRIGGER name ... ON db.schema.table ...`
            let trg = names.first().cloned().unwrap_or_default();
            let up = body.to_ascii_uppercase();
            let (s, t) = up
                .find(" ON ")
                .map(|i| split_schema(qualified_name(&body[i + 4..])))
                .unwrap_or_default();
            references = Some((s.clone(), trg));
            (s, t)
        }
        _ => split_schema(names),
    };
    let mut sql = body.to_string();
    if !sql.ends_with(';') {
        sql.push(';');
    }
    Some(Parsed {
        what,
        schema,
        name,
        sql,
        references,
    })
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
            strip_qualifier(
                "CREATE VIEW sv.v AS SELECT a FROM dbstudio.sv.t JOIN dbstudio.sv.u",
                "dbstudio",
                3
            ),
            "CREATE VIEW sv.v AS SELECT a FROM sv.t JOIN sv.u"
        );
        assert_eq!(
            strip_qualifier(
                "SELECT \"my db\".s.t, x.dbstudio.y, 'dbstudio.a.b'",
                "my db",
                3
            ),
            "SELECT s.t, x.dbstudio.y, 'dbstudio.a.b'"
        );
        // A two-part name whose first part equals the database stays.
        assert_eq!(
            strip_qualifier("SELECT * FROM dbstudio.t", "dbstudio", 3),
            "SELECT * FROM dbstudio.t"
        );
    }

    #[test]
    fn lexer_handles_quotes_and_escapes() {
        // A quote inside a quoted identifier and an escaped quote in an E-string.
        let sql = "SELECT \"it's\" FROM d.s.t WHERE x = E'\\'' AND y = 'd.s.t'";
        assert_eq!(
            strip_qualifier(sql, "d", 3),
            "SELECT \"it's\" FROM s.t WHERE x = E'\\'' AND y = 'd.s.t'"
        );
        // After the odd quote the tokenizer is still in sync.
        let sql = "COMMENT ON TABLE d.s.t IS 'it''s'; SELECT * FROM d.s.u";
        assert_eq!(
            strip_qualifier(sql, "d", 3),
            "COMMENT ON TABLE s.t IS 'it''s'; SELECT * FROM s.u"
        );
    }

    #[test]
    fn schema_qualifier_is_removed_without_touching_columns() {
        assert_eq!(
            strip_qualifier(
                "CREATE TABLE sv.t (sv text, FOREIGN KEY (a) REFERENCES sv.p(id))",
                "sv",
                2
            ),
            "CREATE TABLE t (sv text, FOREIGN KEY (a) REFERENCES p(id))"
        );
    }

    #[test]
    fn classifies_statements() {
        let p = classify("CREATE TABLE sv.child (\n id INT8\n);").unwrap();
        assert_eq!(
            (p.what, p.schema.as_str(), p.name.as_str()),
            (What::Table, "sv", "child")
        );
        let p = classify(
            "ALTER TABLE sv.child ADD CONSTRAINT x FOREIGN KEY (pid) REFERENCES sv.parent(id);",
        )
        .unwrap();
        assert_eq!((p.what, p.name.as_str()), (What::ForeignKey, "child"));
        assert_eq!(p.references, Some(("sv".into(), "parent".into())));
        let p = classify("COMMENT ON COLUMN sv.parent.name IS 'nm';").unwrap();
        assert_eq!(
            (p.what, p.schema.as_str(), p.name.as_str()),
            (What::Comment, "sv", "parent")
        );
        let p = classify("CREATE SCHEMA sv;").unwrap();
        assert_eq!((p.what, p.schema.as_str()), (What::Schema, "sv"));
        let p = classify("CREATE MATERIALIZED VIEW sv.mv (x) AS SELECT 1;").unwrap();
        assert_eq!(p.what, What::MaterializedView);
        let p = classify(
            "CREATE TRIGGER trg BEFORE INSERT ON d.sv.tg FOR EACH ROW EXECUTE FUNCTION d.sv.tf();",
        )
        .unwrap();
        assert_eq!(
            (p.what, p.schema.as_str(), p.name.as_str()),
            (What::Trigger, "sv", "tg")
        );
        assert!(classify("-- Validate foreign key constraints.").is_none());
        assert_eq!(
            classify("CREATE TABLE tt (a INT8);").unwrap().schema,
            "public"
        );
    }

    #[test]
    fn materialized_view_loses_hidden_rowid() {
        assert_eq!(
            drop_hidden_rowid_column(
                "CREATE MATERIALIZED VIEW s.mv (\n\tid,\n\trowid\n) AS SELECT id FROM s.t;"
            ),
            "CREATE MATERIALIZED VIEW s.mv (\n\tid\n) AS SELECT id FROM s.t;"
        );
    }

    #[test]
    fn inline_indexes_are_dropped_with_comma_fix() {
        let sql = "CREATE TABLE s.t (\n\tid INT8 NOT NULL,\n\tCONSTRAINT p PRIMARY KEY (id ASC),\n\tINDEX i (v ASC)\n);";
        assert_eq!(
            drop_inline_indexes(sql),
            "CREATE TABLE s.t (\n\tid INT8 NOT NULL,\n\tCONSTRAINT p PRIMARY KEY (id ASC)\n);"
        );
    }

    fn parsed(sql: &str) -> Parsed {
        classify(sql).unwrap()
    }

    fn target_none() -> Target {
        // Parsed through the public request path in the IT; here an empty selection is enough.
        let req = DdlRequest {
            scope: ObjectPath::root().child(ObjectKind::Database, "d"),
            objects: vec![],
            options: DdlOptions::default(),
        };
        Target::parse(&req).unwrap()
    }

    #[test]
    fn drops_come_first_types_last_and_materialized_views_have_their_own() {
        let opts = DdlOptions {
            include_drop: true,
            ..DdlOptions::default()
        };
        let items = build(
            "d",
            &target_none(),
            &opts,
            vec![
                parsed("CREATE TYPE s.mood AS ENUM ('a');"),
                parsed("CREATE TABLE s.t (m s.mood);"),
                parsed("CREATE MATERIALIZED VIEW s.mv (x) AS SELECT 1;"),
            ],
        );
        let sqls: Vec<&str> = items.iter().map(|i| i.sql.as_str()).collect();
        assert_eq!(
            sqls,
            vec![
                "DROP MATERIALIZED VIEW IF EXISTS \"s\".\"mv\" CASCADE;",
                "DROP TABLE IF EXISTS \"s\".\"t\" CASCADE;",
                "DROP TYPE IF EXISTS \"s\".\"mood\";",
                "CREATE TYPE s.mood AS ENUM ('a');",
                "CREATE TABLE s.t (m s.mood);",
                "CREATE MATERIALIZED VIEW s.mv (x) AS SELECT 1;",
            ]
        );
        assert!(
            !sqls
                .iter()
                .any(|s| s.contains("TYPE") && s.contains("CASCADE"))
        );
    }

    #[test]
    fn options_drop_foreign_keys_comments_and_unqualify() {
        let opts = DdlOptions {
            include_foreign_keys: false,
            include_comments: false,
            qualify_schema: false,
            ..DdlOptions::default()
        };
        let items = build(
            "d",
            &target_none(),
            &opts,
            vec![
                parsed("CREATE TABLE s.t (id INT8);"),
                parsed("COMMENT ON TABLE s.t IS 'x';"),
                parsed("ALTER TABLE s.t ADD CONSTRAINT f FOREIGN KEY (id) REFERENCES s.u(id);"),
            ],
        );
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].sql, "CREATE TABLE t (id INT8);");
    }
}
