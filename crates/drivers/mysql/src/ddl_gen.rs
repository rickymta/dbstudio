//! MySQL/MariaDB driver: DDL generation. Owned by work package F.
//!
//! Tables come from `SHOW CREATE TABLE`, which is split into a `PreData` `CREATE TABLE`
//! (columns, primary and unique keys, checks) and `PostData` `ALTER TABLE ... ADD` items for the
//! plain indexes and the foreign keys (indexes first, then foreign keys). Views, routines and
//! triggers come from `SHOW CREATE ...` with the `DEFINER` clause removed so that the script
//! restores on a server that lacks the original account.

use std::collections::{BTreeSet, HashMap, HashSet};

use dbstudio_core::{
    DdlItem, DdlLanguage, DdlRequest, DdlSection, Error, ObjectKind, ObjectPath, Result, folder,
};
use mysql_async::Row;
use mysql_async::prelude::Queryable;

use crate::error::{map_query_error, server_code};
use crate::model::{catalog_rows, rel_path};
use crate::session::{MySqlSession, cell_named};
use crate::sql::quote_ident;

/// Ordered DDL of the objects in a scope.
pub(crate) async fn generate(session: &MySqlSession, req: DdlRequest) -> Result<Vec<DdlItem>> {
    let target = Target::parse(&req)?;
    let mut conn = session.inner.acquire().await?;
    let r = build(&mut conn, &target, &req.options).await;
    let broken = matches!(r, Err(Error::Connection(_) | Error::Timeout));
    session.inner.release(conn, broken).await;
    r
}

// ---------------------------------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Cat {
    Relation,
    Function,
    Procedure,
    Trigger,
}

#[derive(Debug)]
struct Target {
    db: String,
    selection: Option<BTreeSet<(Cat, String)>>,
    /// The scope itself is one object (foreign keys to other tables are kept).
    single: bool,
}

impl Target {
    fn parse(req: &DdlRequest) -> Result<Target> {
        let segs: Vec<_> = req
            .scope
            .segments
            .iter()
            .filter(|s| s.kind != ObjectKind::Folder)
            .collect();
        let (db, single) = match segs.as_slice() {
            [d] if d.kind == ObjectKind::Database => (d.name.clone(), false),
            [d, o] if d.kind == ObjectKind::Database && is_object(o.kind) => (d.name.clone(), true),
            _ => return Err(Error::Unsupported("ddl_generation".into())),
        };
        let mut selection = BTreeSet::new();
        let mut paths: Vec<&ObjectPath> = req.objects.iter().collect();
        if single {
            paths.push(&req.scope);
        }
        for p in paths {
            let segs: Vec<_> = p
                .segments
                .iter()
                .filter(|s| s.kind != ObjectKind::Folder)
                .collect();
            if let [d, o] = segs.as_slice() {
                if d.kind != ObjectKind::Database {
                    continue;
                }
                let cat = match o.kind {
                    ObjectKind::Table | ObjectKind::View => Cat::Relation,
                    ObjectKind::Function => Cat::Function,
                    ObjectKind::Procedure => Cat::Procedure,
                    ObjectKind::Trigger => Cat::Trigger,
                    _ => continue,
                };
                selection.insert((cat, o.name.clone()));
            }
        }
        if (!req.objects.is_empty() || single) && selection.is_empty() {
            return Err(Error::NotFound("no object of the request was found".into()));
        }
        Ok(Target {
            db,
            selection: (!selection.is_empty()).then_some(selection),
            single,
        })
    }

    fn wants(&self, cat: Cat, name: &str) -> bool {
        self.selection
            .as_ref()
            .is_none_or(|s| s.contains(&(cat, name.to_string())))
    }
}

fn is_object(k: ObjectKind) -> bool {
    matches!(
        k,
        ObjectKind::Table
            | ObjectKind::View
            | ObjectKind::Function
            | ObjectKind::Procedure
            | ObjectKind::Trigger
    )
}

// ---------------------------------------------------------------------------------------------
// Text helpers
// ---------------------------------------------------------------------------------------------

/// Parse a `quote`-delimited token (a doubled quote is an escaped quote) at the start of `s`;
/// returns the unquoted text and the rest of `s`.
fn parse_quoted(s: &str, quote: char) -> Option<(String, &str)> {
    let mut chars = s.char_indices();
    match chars.next() {
        Some((_, c)) if c == quote => {}
        _ => return None,
    }
    let mut out = String::new();
    let mut iter = chars.peekable();
    while let Some((i, c)) = iter.next() {
        if c == quote {
            if matches!(iter.peek(), Some((_, n)) if *n == quote) {
                out.push(quote);
                iter.next();
            } else {
                return Some((out, &s[i + c.len_utf8()..]));
            }
        } else {
            out.push(c);
        }
    }
    None
}

/// Remove the `DEFINER=user@host ` clause (first occurrence).
fn strip_definer(sql: &str) -> String {
    let Some(at) = sql.find("DEFINER=") else {
        return sql.to_string();
    };
    let after = &sql[at + "DEFINER=".len()..];
    let take_part = |s: &str| -> Option<usize> {
        for q in ['`', '\''] {
            if let Some((_, rest)) = parse_quoted(s, q) {
                return Some(s.len() - rest.len());
            }
        }
        None
    };
    let mut consumed = 0;
    if after.starts_with("CURRENT_USER") {
        consumed = "CURRENT_USER".len();
        if after[consumed..].starts_with("()") {
            consumed += 2;
        }
    } else {
        // user
        match take_part(after) {
            Some(n) => consumed += n,
            None => {
                consumed += after.find(['@', ' ']).unwrap_or(after.len());
            }
        }
        if after[consumed..].starts_with('@') {
            consumed += 1;
            match take_part(&after[consumed..]) {
                Some(n) => consumed += n,
                None => {
                    consumed += after[consumed..]
                        .find(char::is_whitespace)
                        .unwrap_or(after.len() - consumed);
                }
            }
        }
    }
    let rest = after[consumed..].trim_start_matches(' ');
    format!("{}{}", &sql[..at], rest)
}

/// Qualify the object name that follows `keyword` (`FUNCTION`, `PROCEDURE`, `TRIGGER`, `VIEW`;
/// matched case-insensitively) with `db`. The server returns routines and triggers verbatim as
/// they were created, so the name may be quoted or bare.
fn qualify_head(sql: &str, keyword: &str, name: &str, db: &str) -> String {
    let upper = sql.to_ascii_uppercase();
    let mut from = 0;
    while let Some(rel) = upper[from..].find(keyword) {
        let at = from + rel;
        from = at + keyword.len();
        let before_ok = at == 0 || !upper.as_bytes()[at - 1].is_ascii_alphanumeric();
        let after = &sql[at + keyword.len()..];
        if !before_ok || !after.starts_with(char::is_whitespace) {
            continue;
        }
        let ws = after.len() - after.trim_start().len();
        let ident_start = at + keyword.len() + ws;
        let rest = &sql[ident_start..];
        let (ident, consumed) = if let Some((n, r)) = parse_quoted(rest, '`') {
            (n, rest.len() - r.len())
        } else {
            let n = rest
                .find(|c: char| c.is_whitespace() || c == '(' || c == '.')
                .unwrap_or(rest.len());
            (rest[..n].to_string(), n)
        };
        if rest[consumed..].starts_with('.') {
            // Already qualified.
            return sql.to_string();
        }
        if ident != name {
            continue;
        }
        return format!(
            "{}{}.{}{}",
            &sql[..ident_start],
            quote_ident(db),
            quote_ident(name),
            &sql[ident_start + consumed..]
        );
    }
    sql.to_string()
}

/// Parse a MySQL string literal (`'...'`, backslash and doubled-quote escapes) at the start of
/// `s`; returns the byte length of the literal.
fn literal_len(s: &str) -> Option<usize> {
    let mut chars = s.char_indices().peekable();
    match chars.next() {
        Some((_, '\'')) => {}
        _ => return None,
    }
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            '\'' => {
                if matches!(chars.peek(), Some((_, '\''))) {
                    chars.next();
                } else {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// Remove a trailing `COMMENT '...'` (columns, keys) or `COMMENT='...'` (table options) clause.
fn strip_comment(line: &str) -> String {
    for marker in [" COMMENT '", " COMMENT='"] {
        let mut from = 0;
        while let Some(rel) = line[from..].find(marker) {
            let m = from + rel;
            let lit_start = m + marker.len() - 1;
            if let Some(len) = literal_len(&line[lit_start..]) {
                let rest = &line[lit_start + len..];
                let trimmed = rest.trim();
                if trimmed.is_empty() || trimmed.starts_with("/*!") {
                    return format!("{}{}", &line[..m], rest);
                }
            }
            from = m + 1;
        }
    }
    line.to_string()
}

#[derive(Debug, Default, PartialEq, Eq)]
struct TableParts {
    /// Column, primary/unique key and check lines, trimmed and without trailing comma.
    body: Vec<String>,
    /// `(name, clause)` of plain indexes.
    keys: Vec<(String, String)>,
    /// `(name, clause)` of foreign keys.
    fks: Vec<(String, String)>,
    /// From the closing parenthesis on (engine options, partitioning).
    tail: String,
}

/// The backtick-quoted identifier following `prefix` at the start of `line`.
fn ident_after(line: &str, prefix: &str) -> Option<String> {
    line.strip_prefix(prefix)
        .and_then(|r| parse_quoted(r, '`'))
        .map(|(n, _)| n)
}

/// Split the output of `SHOW CREATE TABLE`.
fn parse_table(ddl: &str) -> Option<TableParts> {
    let lines: Vec<&str> = ddl.split('\n').collect();
    let end = lines
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, l)| l.starts_with(')'))
        .map(|(i, _)| i)?;
    let mut parts = TableParts {
        tail: lines[end..].join("\n"),
        ..Default::default()
    };
    for raw in &lines[1..end] {
        let line = raw.trim().trim_end_matches(',').to_string();
        let fk_name = ident_after(&line, "CONSTRAINT ").filter(|_| line.contains(" FOREIGN KEY ("));
        let key_name = ident_after(&line, "KEY ")
            .or_else(|| ident_after(&line, "FULLTEXT KEY "))
            .or_else(|| ident_after(&line, "SPATIAL KEY "));
        if let Some(name) = fk_name {
            parts.fks.push((name, line));
        } else if let Some(name) = key_name {
            parts.keys.push((name, line));
        } else {
            parts.body.push(line);
        }
    }
    Some(parts)
}

/// Table referenced by a `... REFERENCES [`db`.]`t` (...)` clause: `(database, table)`.
fn referenced_table(clause: &str) -> Option<(Option<String>, String)> {
    let at = clause.find(" REFERENCES ")?;
    let rest = &clause[at + " REFERENCES ".len()..];
    let (first, rest) = parse_quoted(rest, '`')?;
    match rest.strip_prefix('.') {
        Some(r) => parse_quoted(r, '`').map(|(t, _)| (Some(first), t)),
        None => Some((None, first)),
    }
}

fn with_semicolon(sql: &str) -> String {
    let t = sql.trim_end();
    if t.ends_with(';') {
        t.to_string()
    } else {
        format!("{t};")
    }
}

// ---------------------------------------------------------------------------------------------
// Assembly
// ---------------------------------------------------------------------------------------------

fn item(
    path: ObjectPath,
    kind: ObjectKind,
    section: DdlSection,
    sql: String,
    deps: Vec<ObjectPath>,
) -> DdlItem {
    DdlItem {
        path,
        kind,
        section,
        sql,
        language: DdlLanguage::Sql,
        depends_on: deps,
    }
}

async fn show_create(
    conn: &mut mysql_async::Conn,
    what: &str,
    db: &str,
    name: &str,
    column: &str,
) -> Result<String> {
    let sql = format!(
        "SHOW CREATE {what} {}.{}",
        quote_ident(db),
        quote_ident(name)
    );
    let rows: Vec<Row> = conn.query(sql).await.map_err(|e| match server_code(&e) {
        Some(1146 | 1305 | 1360 | 1049 | 1347 | 1109) => {
            Error::NotFound(format!("{} `{db}`.`{name}`", what.to_lowercase()))
        }
        _ => map_query_error(e),
    })?;
    rows.first()
        .and_then(|r| cell_named(r, column))
        .ok_or_else(|| {
            Error::query(format!(
                "insufficient privileges to view the definition of {} `{name}`",
                what.to_lowercase()
            ))
        })
}

async fn build(
    conn: &mut mysql_async::Conn,
    target: &Target,
    opts: &dbstudio_core::DdlOptions,
) -> Result<Vec<DdlItem>> {
    let db = target.db.as_str();
    let qualify = opts.qualify_schema;
    let ine = if opts.if_not_exists {
        "IF NOT EXISTS "
    } else {
        ""
    };
    let q = |name: &str| {
        if qualify {
            format!("{}.{}", quote_ident(db), quote_ident(name))
        } else {
            quote_ident(name)
        }
    };
    let mut pre: Vec<DdlItem> = Vec::new();
    let mut post: Vec<DdlItem> = Vec::new();
    let db_path = ObjectPath::root().child(ObjectKind::Database, db);

    // Relations.
    let rows = catalog_rows(
        conn,
        "SELECT TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES \
         WHERE TABLE_SCHEMA = ? AND TABLE_TYPE IN ('BASE TABLE', 'VIEW') ORDER BY TABLE_NAME",
        (db,),
    )
    .await?;
    if rows.is_empty() {
        // Make sure the database exists (an unknown database is an error, an empty one is not).
        let exists = catalog_rows(
            conn,
            "SELECT 1 FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = ?",
            (db,),
        )
        .await?;
        if exists.is_empty() {
            return Err(Error::NotFound(format!("database `{db}`")));
        }
    }
    let mut tables: Vec<String> = Vec::new();
    let mut views: Vec<String> = Vec::new();
    for r in &rows {
        let name = crate::session::cell(r, 0).unwrap_or_default();
        let is_view = crate::session::cell(r, 1).as_deref() == Some("VIEW");
        if !target.wants(Cat::Relation, &name) {
            continue;
        }
        if is_view {
            if opts.include_views {
                views.push(name);
            }
        } else {
            tables.push(name);
        }
    }

    if opts.include_drop {
        pre.push(item(
            db_path.clone(),
            ObjectKind::Database,
            DdlSection::PreData,
            "SET FOREIGN_KEY_CHECKS = 0;".into(),
            vec![],
        ));
    }
    if qualify {
        let create = show_create_database(conn, db).await?;
        pre.push(item(
            db_path.clone(),
            ObjectKind::Database,
            DdlSection::PreData,
            create,
            vec![],
        ));
    }

    // Tables.
    let table_set: HashSet<&String> = tables.iter().collect();
    for name in &tables {
        let raw = show_create(conn, "TABLE", db, name, "Create Table").await?;
        let path = rel_path(db, ObjectKind::Table, name);
        let mut parts = parse_table(&raw).ok_or_else(|| {
            Error::Other(format!("unexpected SHOW CREATE TABLE output for `{name}`"))
        })?;
        if !opts.include_comments {
            for l in parts.body.iter_mut() {
                *l = strip_comment(l);
            }
            for (_, l) in parts.keys.iter_mut() {
                *l = strip_comment(l);
            }
            parts.tail = parts
                .tail
                .split('\n')
                .map(strip_comment)
                .collect::<Vec<_>>()
                .join("\n");
        }
        if opts.include_drop {
            pre.push(item(
                path.clone(),
                ObjectKind::Table,
                DdlSection::PreData,
                format!("DROP TABLE IF EXISTS {};", q(name)),
                vec![],
            ));
        }
        let body = parts
            .body
            .iter()
            .map(|l| format!("  {l}"))
            .collect::<Vec<_>>()
            .join(",\n");
        pre.push(item(
            path.clone(),
            ObjectKind::Table,
            DdlSection::PreData,
            format!("CREATE TABLE {ine}{} (\n{body}\n{};", q(name), parts.tail),
            vec![],
        ));
        if opts.include_indexes {
            for (key, clause) in &parts.keys {
                post.push(item(
                    path.child(ObjectKind::Folder, folder::INDEXES)
                        .child(ObjectKind::Index, key.as_str()),
                    ObjectKind::Index,
                    DdlSection::PostData,
                    format!("ALTER TABLE {} ADD {clause};", q(name)),
                    vec![path.clone()],
                ));
            }
        }
        if opts.include_foreign_keys {
            for (_, clause) in &parts.fks {
                let refd = referenced_table(clause);
                // A foreign key to a table that is not part of the selection would break the
                // restore (unless the request is the DDL of that single table).
                if let Some((rdb, rt)) = &refd {
                    let same_db = rdb.as_deref().is_none_or(|d| d.eq_ignore_ascii_case(db));
                    if same_db && !target.single && !table_set.contains(rt) {
                        continue;
                    }
                }
                let mut deps = vec![path.clone()];
                if let Some((rdb, rt)) = refd {
                    deps.push(rel_path(
                        rdb.as_deref().unwrap_or(db),
                        ObjectKind::Table,
                        &rt,
                    ));
                }
                post.push(item(
                    path.clone(),
                    ObjectKind::Table,
                    DdlSection::PostData,
                    format!("ALTER TABLE {} ADD {clause};", q(name)),
                    deps,
                ));
            }
        }
    }
    // Indexes come before foreign keys (a foreign key reuses a matching index).
    post.sort_by_key(|i| i.kind != ObjectKind::Index);

    // Views in dependency order (a view mentions the views it reads from).
    let mut view_sql: HashMap<String, String> = HashMap::new();
    for name in &views {
        let raw = show_create(conn, "VIEW", db, name, "Create View").await?;
        view_sql.insert(name.clone(), raw);
    }
    let mut ordered: Vec<&String> = Vec::new();
    let mut remaining: Vec<&String> = views.iter().collect();
    let view_set: HashSet<&String> = views.iter().collect();
    let deps_of = |name: &String| -> Vec<&String> {
        let body = &view_sql[name];
        views
            .iter()
            .filter(|o| *o != name && body.contains(&quote_ident(o)))
            .filter(|o| view_set.contains(*o))
            .collect()
    };
    while !remaining.is_empty() {
        let before = remaining.len();
        let mut next = Vec::new();
        for v in remaining {
            if deps_of(v).iter().all(|d| ordered.contains(d)) {
                ordered.push(v);
            } else {
                next.push(v);
            }
        }
        remaining = next;
        if remaining.len() == before {
            ordered.append(&mut remaining);
        }
    }
    for name in ordered {
        let path = rel_path(db, ObjectKind::View, name);
        let mut sql = strip_definer(&view_sql[name]);
        if sql.starts_with("CREATE ") && !sql.starts_with("CREATE OR REPLACE ") {
            sql = format!("CREATE OR REPLACE {}", &sql["CREATE ".len()..]);
        }
        if qualify {
            sql = qualify_head(&sql, "VIEW", name, db);
        } else {
            sql = sql.replace(&format!("{}.", quote_ident(db)), "");
        }
        let deps: Vec<ObjectPath> = deps_of(name)
            .into_iter()
            .map(|d| rel_path(db, ObjectKind::View, d))
            .collect();
        if opts.include_drop {
            pre.push(item(
                path.clone(),
                ObjectKind::View,
                DdlSection::PreData,
                format!("DROP VIEW IF EXISTS {};", q(name)),
                vec![],
            ));
        }
        pre.push(item(
            path,
            ObjectKind::View,
            DdlSection::PreData,
            with_semicolon(&sql),
            deps,
        ));
    }

    // Routines.
    if opts.include_routines {
        let rows = catalog_rows(
            conn,
            "SELECT ROUTINE_NAME, ROUTINE_TYPE FROM information_schema.ROUTINES \
             WHERE ROUTINE_SCHEMA = ? ORDER BY ROUTINE_TYPE, ROUTINE_NAME",
            (db,),
        )
        .await?;
        for r in &rows {
            let name = crate::session::cell(r, 0).unwrap_or_default();
            let is_proc = crate::session::cell(r, 1).as_deref() == Some("PROCEDURE");
            let (what, column, cat, kind, f) = if is_proc {
                (
                    "PROCEDURE",
                    "Create Procedure",
                    Cat::Procedure,
                    ObjectKind::Procedure,
                    folder::PROCEDURES,
                )
            } else {
                (
                    "FUNCTION",
                    "Create Function",
                    Cat::Function,
                    ObjectKind::Function,
                    folder::FUNCTIONS,
                )
            };
            if !target.wants(cat, &name) {
                continue;
            }
            let raw = show_create(conn, what, db, &name, column).await?;
            let mut sql = strip_definer(&raw);
            if qualify {
                sql = qualify_head(&sql, what, &name, db);
            }
            let path = db_path
                .child(ObjectKind::Folder, f)
                .child(kind, name.as_str());
            if opts.include_drop {
                pre.push(item(
                    path.clone(),
                    kind,
                    DdlSection::PreData,
                    format!("DROP {what} IF EXISTS {};", q(&name)),
                    vec![],
                ));
            }
            pre.push(item(
                path,
                kind,
                DdlSection::PreData,
                with_semicolon(&sql),
                vec![],
            ));
        }
    }

    // Triggers.
    if opts.include_triggers {
        let rows = catalog_rows(
            conn,
            "SELECT TRIGGER_NAME, EVENT_OBJECT_TABLE FROM information_schema.TRIGGERS \
             WHERE TRIGGER_SCHEMA = ? \
             ORDER BY EVENT_OBJECT_TABLE, EVENT_MANIPULATION, ACTION_TIMING, ACTION_ORDER, \
                      TRIGGER_NAME",
            (db,),
        )
        .await?;
        for r in &rows {
            let name = crate::session::cell(r, 0).unwrap_or_default();
            let table = crate::session::cell(r, 1).unwrap_or_default();
            let wanted = match &target.selection {
                None => true,
                Some(_) => {
                    target.wants(Cat::Trigger, &name)
                        || (target.wants(Cat::Relation, &table) && table_set.contains(&table))
                }
            };
            if !wanted {
                continue;
            }
            let raw = show_create(conn, "TRIGGER", db, &name, "SQL Original Statement").await?;
            let mut sql = strip_definer(&raw);
            if qualify {
                sql = qualify_head(&sql, "TRIGGER", &name, db);
            }
            let table_path = rel_path(db, ObjectKind::Table, &table);
            let path = table_path
                .child(ObjectKind::Folder, folder::TRIGGERS)
                .child(ObjectKind::Trigger, name.as_str());
            if opts.include_drop {
                pre.push(item(
                    path.clone(),
                    ObjectKind::Trigger,
                    DdlSection::PreData,
                    format!("DROP TRIGGER IF EXISTS {};", q(&name)),
                    vec![],
                ));
            }
            post.push(item(
                path,
                ObjectKind::Trigger,
                DdlSection::PostData,
                with_semicolon(&sql),
                vec![table_path],
            ));
        }
    }

    if opts.include_drop {
        post.push(item(
            db_path,
            ObjectKind::Database,
            DdlSection::PostData,
            "SET FOREIGN_KEY_CHECKS = 1;".into(),
            vec![],
        ));
    }
    // Drops of triggers were queued with the PreData items above; keep PreData first.
    let mut items = pre;
    items.extend(post);
    Ok(items)
}

async fn show_create_database(conn: &mut mysql_async::Conn, db: &str) -> Result<String> {
    let rows: Vec<Row> = conn
        .query(format!("SHOW CREATE DATABASE {}", quote_ident(db)))
        .await
        .map_err(|e| match server_code(&e) {
            Some(1049) => Error::NotFound(format!("database `{db}`")),
            _ => map_query_error(e),
        })?;
    let text = rows
        .first()
        .and_then(|r| cell_named(r, "Create Database"))
        .unwrap_or_else(|| format!("CREATE DATABASE {}", quote_ident(db)));
    let text = match text.strip_prefix("CREATE DATABASE ") {
        Some(rest) if !rest.starts_with("IF NOT EXISTS") => {
            format!("CREATE DATABASE IF NOT EXISTS {rest}")
        }
        _ => text,
    };
    Ok(with_semicolon(&text))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHOW: &str = "CREATE TABLE `film` (\n  `film_id` int NOT NULL AUTO_INCREMENT COMMENT 'it''s the id',\n  `title` varchar(255) NOT NULL,\n  `language_id` int NOT NULL,\n  PRIMARY KEY (`film_id`),\n  UNIQUE KEY `uq_title` (`title`),\n  KEY `idx_lang` (`language_id`) COMMENT 'lang',\n  FULLTEXT KEY `ft_title` (`title`),\n  CONSTRAINT `fk_lang` FOREIGN KEY (`language_id`) REFERENCES `language` (`language_id`) ON DELETE CASCADE,\n  CONSTRAINT `chk` CHECK ((`film_id` > 0))\n) ENGINE=InnoDB AUTO_INCREMENT=3 DEFAULT CHARSET=utf8mb4 COMMENT='films'";

    #[test]
    fn splits_show_create_table() {
        let p = parse_table(SHOW).unwrap();
        assert_eq!(p.body.len(), 6, "{:?}", p.body);
        assert!(p.body[3].starts_with("PRIMARY KEY"));
        assert!(p.body[4].starts_with("UNIQUE KEY"));
        assert!(p.body[5].starts_with("CONSTRAINT `chk` CHECK"));
        assert_eq!(p.keys.len(), 2);
        assert_eq!(p.keys[0].0, "idx_lang");
        assert_eq!(p.keys[1].0, "ft_title");
        assert_eq!(p.fks.len(), 1);
        assert_eq!(p.fks[0].0, "fk_lang");
        assert!(p.tail.starts_with(") ENGINE=InnoDB AUTO_INCREMENT=3"));
        assert_eq!(
            referenced_table(&p.fks[0].1),
            Some((None, "language".to_string()))
        );
        assert_eq!(
            referenced_table("CONSTRAINT `x` FOREIGN KEY (`a`) REFERENCES `other`.`t` (`id`)"),
            Some((Some("other".into()), "t".into()))
        );
    }

    #[test]
    fn strips_comments() {
        assert_eq!(
            strip_comment("`film_id` int NOT NULL AUTO_INCREMENT COMMENT 'it''s the id'"),
            "`film_id` int NOT NULL AUTO_INCREMENT"
        );
        assert_eq!(
            strip_comment("KEY `i` (`a`) COMMENT 'x' /*!80000 INVISIBLE */"),
            "KEY `i` (`a`) /*!80000 INVISIBLE */"
        );
        assert_eq!(
            strip_comment(") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COMMENT='films'"),
            ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"
        );
        assert_eq!(
            strip_comment("`c` varchar(5) DEFAULT 'a COMMENT ''b''' COMMENT 'real'"),
            "`c` varchar(5) DEFAULT 'a COMMENT ''b'''"
        );
        assert_eq!(strip_comment("`c` int"), "`c` int");
    }

    #[test]
    fn strips_definers() {
        assert_eq!(
            strip_definer(
                "CREATE ALGORITHM=UNDEFINED DEFINER=`root`@`%` SQL SECURITY DEFINER VIEW `v` AS select 1"
            ),
            "CREATE ALGORITHM=UNDEFINED SQL SECURITY DEFINER VIEW `v` AS select 1"
        );
        assert_eq!(
            strip_definer("CREATE DEFINER=`a``b`@`h` FUNCTION `f`() RETURNS int RETURN 1"),
            "CREATE FUNCTION `f`() RETURNS int RETURN 1"
        );
        assert_eq!(
            strip_definer(
                "CREATE DEFINER=CURRENT_USER TRIGGER `t` BEFORE INSERT ON `x` FOR EACH ROW SET @a=1"
            ),
            "CREATE TRIGGER `t` BEFORE INSERT ON `x` FOR EACH ROW SET @a=1"
        );
        assert_eq!(strip_definer("CREATE TABLE t"), "CREATE TABLE t");
    }

    #[test]
    fn qualifies_heads() {
        assert_eq!(
            qualify_head("CREATE FUNCTION `f`() RETURNS int", "FUNCTION", "f", "d"),
            "CREATE FUNCTION `d`.`f`() RETURNS int"
        );
        assert_eq!(
            qualify_head(
                "CREATE trigger ins_film BEFORE INSERT ON film FOR EACH ROW SET a=1",
                "TRIGGER",
                "ins_film",
                "d"
            ),
            "CREATE trigger `d`.`ins_film` BEFORE INSERT ON film FOR EACH ROW SET a=1"
        );
        assert_eq!(
            qualify_head(
                "CREATE FUNCTION f(a int) RETURNS int RETURN 1",
                "FUNCTION",
                "f",
                "d"
            ),
            "CREATE FUNCTION `d`.`f`(a int) RETURNS int RETURN 1"
        );
        assert_eq!(
            qualify_head(
                "CREATE FUNCTION d.f() RETURNS int RETURN 1",
                "FUNCTION",
                "f",
                "d"
            ),
            "CREATE FUNCTION d.f() RETURNS int RETURN 1"
        );
        assert_eq!(
            qualify_head("CREATE VIEW `v` AS select 1", "VIEW", "v", "d"),
            "CREATE VIEW `d`.`v` AS select 1"
        );
        assert_eq!(with_semicolon("a;"), "a;");
        assert_eq!(with_semicolon("a\n"), "a;");
    }

    #[test]
    fn target_parsing() {
        let db = ObjectPath::root().child(ObjectKind::Database, "d");
        let req = DdlRequest {
            scope: db.clone(),
            objects: vec![],
            options: Default::default(),
        };
        let t = Target::parse(&req).unwrap();
        assert!(t.selection.is_none() && !t.single);
        let req = DdlRequest {
            scope: rel_path("d", ObjectKind::Table, "t"),
            objects: vec![],
            options: Default::default(),
        };
        let t = Target::parse(&req).unwrap();
        assert!(t.single && t.wants(Cat::Relation, "t") && !t.wants(Cat::Relation, "u"));
        let req = DdlRequest {
            scope: ObjectPath::root(),
            objects: vec![],
            options: Default::default(),
        };
        assert!(Target::parse(&req).is_err());
    }
}
