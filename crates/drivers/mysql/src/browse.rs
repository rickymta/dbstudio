//! Object tree browsing, DDL retrieval and table metadata from `information_schema`.
//!
//! Queries only use columns available in MySQL 5.7+, 8.x and MariaDB 10.4+ (functional index
//! expressions, which exist from MySQL 8.0.13, are read opportunistically from `SHOW INDEX`).
//!
//! Path convention: a node's path is its parent's path plus its own segment, e.g. `[Database d,
//! Folder tables, Table t, Folder columns, Column c]`. Paths with or without Folder segments are
//! both accepted when resolving.

use dbstudio_core::{
    ColumnDetail, Error, ForeignKeyDetail, IndexDetail, ObjectKind, ObjectNode, ObjectPath,
    ObjectSearchRequest, Result, TableDetails, ValueKind, folder,
};
use mysql_async::Row;
use mysql_async::prelude::Queryable;

use crate::error::{map_query_error, server_code};
use crate::session::{cell, cell_named};
use crate::sql::{quote_ident, quote_table};
use crate::value::kind_from_data_type;

const SYSTEM_DATABASES: [&str; 4] = ["information_schema", "mysql", "performance_schema", "sys"];

fn is_system_db(name: &str) -> bool {
    SYSTEM_DATABASES
        .iter()
        .any(|s| s.eq_ignore_ascii_case(name))
}

async fn rows<P>(conn: &mut mysql_async::Conn, sql: &str, params: P) -> Result<Vec<Row>>
where
    P: Into<mysql_async::Params> + Send,
{
    conn.exec(sql, params).await.map_err(map_query_error)
}

/// Resolved location inside the tree.
#[derive(Debug, PartialEq, Eq)]
enum Place {
    Root,
    Roles,
    Database(String),
    DbFolder(String, String),
    Table(String, String, ObjectKind),
    TableFolder(String, String, ObjectKind, String),
}

fn resolve(path: &ObjectPath) -> Result<Place> {
    let n = path.segments.len();
    let segs: Vec<_> = path
        .segments
        .iter()
        .enumerate()
        .filter(|(i, s)| s.kind != ObjectKind::Folder || *i + 1 == n)
        .map(|(_, s)| s)
        .collect();
    let bad = || Error::NotFound(format!("unsupported tree path ({} segments)", n));
    match segs.as_slice() {
        [] => Ok(Place::Root),
        [f] if f.kind == ObjectKind::Folder && f.name == folder::ROLES => Ok(Place::Roles),
        [d] if d.kind == ObjectKind::Database => Ok(Place::Database(d.name.clone())),
        [d, f] if d.kind == ObjectKind::Database && f.kind == ObjectKind::Folder => {
            Ok(Place::DbFolder(d.name.clone(), f.name.clone()))
        }
        [d, t]
            if d.kind == ObjectKind::Database
                && matches!(t.kind, ObjectKind::Table | ObjectKind::View) =>
        {
            Ok(Place::Table(d.name.clone(), t.name.clone(), t.kind))
        }
        [d, t, f]
            if d.kind == ObjectKind::Database
                && matches!(t.kind, ObjectKind::Table | ObjectKind::View)
                && f.kind == ObjectKind::Folder =>
        {
            Ok(Place::TableFolder(
                d.name.clone(),
                t.name.clone(),
                t.kind,
                f.name.clone(),
            ))
        }
        _ => Err(bad()),
    }
}

fn node(
    path: ObjectPath,
    kind: ObjectKind,
    name: &str,
    has_children: bool,
    detail: Option<String>,
) -> ObjectNode {
    ObjectNode {
        path,
        kind,
        name: name.to_string(),
        has_children,
        detail,
    }
}

fn count_of(row: &Row, i: usize) -> u64 {
    cell(row, i)
        .and_then(|s| s.split('.').next().and_then(|x| x.parse().ok()))
        .unwrap_or(0)
}

/// Children of `parent`.
pub async fn list_children(
    conn: &mut mysql_async::Conn,
    parent: &ObjectPath,
    is_mariadb: bool,
) -> Result<Vec<ObjectNode>> {
    match resolve(parent)? {
        Place::Root => list_root(conn, parent).await,
        Place::Roles => list_roles(conn, parent).await,
        Place::Database(db) => list_db_folders(conn, parent, &db, is_mariadb).await,
        Place::DbFolder(db, f) => list_db_folder(conn, parent, &db, &f).await,
        Place::Table(db, t, kind) => list_table_folders(conn, parent, &db, &t, kind).await,
        Place::TableFolder(db, t, kind, f) => {
            list_table_folder(conn, parent, &db, &t, kind, &f).await
        }
    }
}

/// Rank of `name` for `query` (both lower-cased): 0 exact, 1 prefix, 2 substring.
fn match_rank(name_lc: &str, query_lc: &str) -> u8 {
    if name_lc == query_lc {
        0
    } else if name_lc.starts_with(query_lc) {
        1
    } else {
        2
    }
}

/// Server-side search of tables, views, functions and procedures by (case-insensitive)
/// substring of the name. See [`ObjectSearchRequest`] for the contract.
pub async fn search_objects(
    conn: &mut mysql_async::Conn,
    req: &ObjectSearchRequest,
) -> Result<Vec<ObjectNode>> {
    let query = req.query.trim();
    if query.is_empty() {
        return Ok(Vec::new());
    }
    let limit = req.limit.clamp(1, 1000);
    let applicable = [
        ObjectKind::Table,
        ObjectKind::View,
        ObjectKind::Function,
        ObjectKind::Procedure,
    ];
    // Empty = the default set; otherwise only the requested kinds that exist in MySQL. A request
    // that names only inapplicable kinds matches nothing.
    let kinds: Vec<ObjectKind> = if req.kinds.is_empty() {
        applicable.to_vec()
    } else {
        req.kinds
            .iter()
            .copied()
            .filter(|k| applicable.contains(k))
            .collect()
    };
    if kinds.is_empty() {
        return Ok(Vec::new());
    }
    let wants = |k: ObjectKind| kinds.contains(&k);
    // None = every user database; Some(db) = that database only.
    let scope_db: Option<String> = match resolve(&req.scope) {
        Ok(Place::Root) => None,
        Ok(Place::Database(d)) | Ok(Place::DbFolder(d, _)) => Some(d),
        Ok(Place::Table(..)) | Ok(Place::TableFolder(..)) => {
            return Err(Error::NotFound(
                "search scope must be the root or a database".into(),
            ));
        }
        Ok(Place::Roles) => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let query_lc = query.to_lowercase();
    let system_list = SYSTEM_DATABASES.map(|d| format!("'{d}'")).join(",");
    let scope_sql = match &scope_db {
        Some(_) => "AND {col} = ?".to_string(),
        None => format!("AND LOWER({{col}}) NOT IN ({system_list})"),
    };

    // (rank, sort name, kind, database, name)
    let mut found: Vec<(u8, String, ObjectKind, String, String)> = Vec::new();
    let mut run = |kind_of: &dyn Fn(&str) -> Option<ObjectKind>, rows: Vec<Row>| {
        for r in rows {
            let (Some(db), Some(name)) = (cell(&r, 0), cell(&r, 1)) else {
                continue;
            };
            let Some(kind) = kind_of(&cell(&r, 2).unwrap_or_default()) else {
                continue;
            };
            let lc = name.to_lowercase();
            found.push((match_rank(&lc, &query_lc), lc, kind, db, name));
        }
    };

    let want_tables = wants(ObjectKind::Table);
    let want_views = wants(ObjectKind::View);
    if want_tables || want_views {
        // Filter by kind in SQL so that LIMIT applies after it.
        let type_cond = match (want_tables, want_views) {
            (true, true) => "TABLE_TYPE <> 'SEQUENCE'",
            (true, false) => "TABLE_TYPE NOT IN ('VIEW','SEQUENCE')",
            _ => "TABLE_TYPE = 'VIEW'",
        };
        let sql = format!(
            "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES \
             WHERE LOCATE(LOWER(?), LOWER(TABLE_NAME)) > 0 {} \
             AND {type_cond} \
             ORDER BY (LOWER(TABLE_NAME) = LOWER(?)) DESC, (LOCATE(LOWER(?), LOWER(TABLE_NAME)) = 1) DESC, \
                      TABLE_NAME, TABLE_SCHEMA LIMIT {limit}",
            scope_sql.replace("{col}", "TABLE_SCHEMA"),
            type_cond = type_cond
        );
        let q = query;
        let rows: Vec<Row> = match &scope_db {
            Some(d) => rows_vec(conn, &sql, (q, d.as_str(), q, q)).await?,
            None => rows_vec(conn, &sql, (q, q, q)).await?,
        };
        run(
            &|ty| match (ty == "VIEW", want_views, want_tables) {
                (true, true, _) => Some(ObjectKind::View),
                (false, _, true) => Some(ObjectKind::Table),
                _ => None,
            },
            rows,
        );
    }
    for (kind, ty, want) in [
        (
            ObjectKind::Function,
            "FUNCTION",
            wants(ObjectKind::Function),
        ),
        (
            ObjectKind::Procedure,
            "PROCEDURE",
            wants(ObjectKind::Procedure),
        ),
    ] {
        if !want {
            continue;
        }
        let sql = format!(
            "SELECT ROUTINE_SCHEMA, ROUTINE_NAME, ROUTINE_TYPE FROM information_schema.ROUTINES \
             WHERE ROUTINE_TYPE = '{ty}' AND LOCATE(LOWER(?), LOWER(ROUTINE_NAME)) > 0 {} \
             ORDER BY (LOWER(ROUTINE_NAME) = LOWER(?)) DESC, (LOCATE(LOWER(?), LOWER(ROUTINE_NAME)) = 1) DESC, \
                      ROUTINE_NAME, ROUTINE_SCHEMA LIMIT {limit}",
            scope_sql.replace("{col}", "ROUTINE_SCHEMA")
        );
        let q = query;
        let rows: Vec<Row> = match &scope_db {
            Some(d) => rows_vec(conn, &sql, (q, d.as_str(), q, q)).await?,
            None => rows_vec(conn, &sql, (q, q, q)).await?,
        };
        run(&|_| Some(kind), rows);
    }

    let path_key = |kind: ObjectKind, db: &str, name: &str| format!("{db}\u{0}{kind:?}\u{0}{name}");
    found.sort_by(|a, b| {
        (a.0, &a.1, &a.4, path_key(a.2, &a.3, &a.4)).cmp(&(
            b.0,
            &b.1,
            &b.4,
            path_key(b.2, &b.3, &b.4),
        ))
    });
    found.truncate(limit as usize);
    Ok(found
        .into_iter()
        .map(|(_, _, kind, db, name)| {
            let folder_key = match kind {
                ObjectKind::View => folder::VIEWS,
                ObjectKind::Function => folder::FUNCTIONS,
                ObjectKind::Procedure => folder::PROCEDURES,
                _ => folder::TABLES,
            };
            let path = ObjectPath::root()
                .child(ObjectKind::Database, db.clone())
                .child(ObjectKind::Folder, folder_key)
                .child(kind, name.clone());
            let has_children = matches!(kind, ObjectKind::Table | ObjectKind::View);
            node(path, kind, &name, has_children, Some(db))
        })
        .collect())
}

async fn rows_vec<P>(conn: &mut mysql_async::Conn, sql: &str, params: P) -> Result<Vec<Row>>
where
    P: Into<mysql_async::Params> + Send,
{
    rows(conn, sql, params).await
}

/// Account name in SQL form: `'user'@'host'`.
fn account_name(user: &str, host: &str) -> String {
    let q = |s: &str| s.replace('\\', "\\\\").replace('\'', "''");
    format!("'{}'@'{}'", q(user), q(host))
}

/// Databases plus, when the account may read `mysql.user`, a `roles` folder.
async fn list_root(conn: &mut mysql_async::Conn, parent: &ObjectPath) -> Result<Vec<ObjectNode>> {
    let mut out = list_databases(conn).await?;
    let count: std::result::Result<Option<Row>, _> =
        conn.query_first("SELECT COUNT(*) FROM mysql.user").await;
    match count {
        Ok(row) => {
            let n = row.map(|r| count_of(&r, 0)).unwrap_or(0);
            out.push(node(
                parent.child(ObjectKind::Folder, folder::ROLES),
                ObjectKind::Folder,
                folder::ROLES,
                n > 0,
                Some(n.to_string()),
            ));
        }
        // No privilege (or no such table): the folder is simply not offered.
        Err(e) if server_code(&e).is_some() => {}
        Err(e) => return Err(map_query_error(e)),
    }
    Ok(out)
}

async fn list_roles(conn: &mut mysql_async::Conn, parent: &ObjectPath) -> Result<Vec<ObjectNode>> {
    let rs: Vec<Row> = conn
        .query("SELECT User, Host FROM mysql.user ORDER BY User, Host")
        .await
        .map_err(map_query_error)?;
    Ok(rs
        .iter()
        .map(|r| {
            let name = account_name(
                &cell(r, 0).unwrap_or_default(),
                &cell(r, 1).unwrap_or_default(),
            );
            node(
                parent.child(ObjectKind::Role, name.clone()),
                ObjectKind::Role,
                &name,
                false,
                None,
            )
        })
        .collect())
}

async fn list_databases(conn: &mut mysql_async::Conn) -> Result<Vec<ObjectNode>> {
    let rs = rows(
        conn,
        "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA ORDER BY SCHEMA_NAME",
        (),
    )
    .await?;
    let mut user = Vec::new();
    let mut system = Vec::new();
    for r in rs {
        let Some(name) = cell(&r, 0) else { continue };
        let path = ObjectPath::root().child(ObjectKind::Database, name.clone());
        if is_system_db(&name) {
            system.push(node(
                path,
                ObjectKind::Database,
                &name,
                true,
                Some("system".into()),
            ));
        } else {
            user.push(node(path, ObjectKind::Database, &name, true, None));
        }
    }
    user.extend(system);
    Ok(user)
}

async fn list_db_folders(
    conn: &mut mysql_async::Conn,
    parent: &ObjectPath,
    db: &str,
    is_mariadb: bool,
) -> Result<Vec<ObjectNode>> {
    let t = rows(
        conn,
        "SELECT COALESCE(SUM(TABLE_TYPE NOT IN ('VIEW','SEQUENCE')),0), \
                COALESCE(SUM(TABLE_TYPE = 'VIEW'),0), COALESCE(SUM(TABLE_TYPE = 'SEQUENCE'),0) \
         FROM information_schema.TABLES WHERE TABLE_SCHEMA = ?",
        (db,),
    )
    .await?;
    let r = rows(
        conn,
        "SELECT COALESCE(SUM(ROUTINE_TYPE = 'FUNCTION'),0), COALESCE(SUM(ROUTINE_TYPE = 'PROCEDURE'),0) \
         FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA = ?",
        (db,),
    )
    .await?;
    let (tables, views, sequences) = t
        .first()
        .map(|x| (count_of(x, 0), count_of(x, 1), count_of(x, 2)))
        .unwrap_or_default();
    let (functions, procedures) = r
        .first()
        .map(|x| (count_of(x, 0), count_of(x, 1)))
        .unwrap_or_default();
    if tables + views + sequences + functions + procedures == 0 {
        let exists = rows(
            conn,
            "SELECT 1 FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = ?",
            (db,),
        )
        .await?;
        if exists.is_empty() {
            return Err(Error::NotFound(format!("database `{db}`")));
        }
    }
    let mut out = Vec::new();
    let mut push = |key: &str, n: u64| {
        if n > 0 {
            out.push(node(
                parent.child(ObjectKind::Folder, key),
                ObjectKind::Folder,
                key,
                true,
                Some(n.to_string()),
            ));
        }
    };
    push(folder::TABLES, tables);
    push(folder::VIEWS, views);
    if is_mariadb {
        push(folder::SEQUENCES, sequences);
    }
    push(folder::FUNCTIONS, functions);
    push(folder::PROCEDURES, procedures);
    Ok(out)
}

async fn list_db_folder(
    conn: &mut mysql_async::Conn,
    parent: &ObjectPath,
    db: &str,
    f: &str,
) -> Result<Vec<ObjectNode>> {
    let base = parent.clone();
    match f {
        folder::TABLES | folder::VIEWS | folder::SEQUENCES => {
            let cond = match f {
                folder::TABLES => "TABLE_TYPE NOT IN ('VIEW','SEQUENCE')",
                folder::VIEWS => "TABLE_TYPE = 'VIEW'",
                _ => "TABLE_TYPE = 'SEQUENCE'",
            };
            let sql = format!(
                "SELECT TABLE_NAME FROM information_schema.TABLES WHERE TABLE_SCHEMA = ? AND {cond} ORDER BY TABLE_NAME"
            );
            let rs = rows(conn, &sql, (db,)).await?;
            let (kind, has_children) = match f {
                folder::TABLES => (ObjectKind::Table, true),
                folder::VIEWS => (ObjectKind::View, true),
                _ => (ObjectKind::Sequence, false),
            };
            Ok(rs
                .iter()
                .filter_map(|r| cell(r, 0))
                .map(|name| {
                    node(
                        base.child(kind, name.clone()),
                        kind,
                        &name,
                        has_children,
                        None,
                    )
                })
                .collect())
        }
        folder::FUNCTIONS | folder::PROCEDURES => {
            let (ty, kind) = if f == folder::FUNCTIONS {
                ("FUNCTION", ObjectKind::Function)
            } else {
                ("PROCEDURE", ObjectKind::Procedure)
            };
            let rs = rows(
                conn,
                "SELECT ROUTINE_NAME, DTD_IDENTIFIER FROM information_schema.ROUTINES \
                 WHERE ROUTINE_SCHEMA = ? AND ROUTINE_TYPE = ? ORDER BY ROUTINE_NAME",
                (db, ty),
            )
            .await?;
            Ok(rs
                .iter()
                .filter_map(|r| {
                    let name = cell(r, 0)?;
                    let detail = if kind == ObjectKind::Function {
                        cell(r, 1)
                            .filter(|d| !d.is_empty())
                            .map(|d| format!("returns {d}"))
                    } else {
                        None
                    };
                    Some(node(
                        base.child(kind, name.clone()),
                        kind,
                        &name,
                        false,
                        detail,
                    ))
                })
                .collect())
        }
        other => Err(Error::NotFound(format!("unknown folder `{other}`"))),
    }
}

/// Verify the table/view exists and report whether it is a view.
async fn table_type(conn: &mut mysql_async::Conn, db: &str, t: &str) -> Result<String> {
    let rs = rows(
        conn,
        "SELECT TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
        (db, t),
    )
    .await?;
    rs.first()
        .and_then(|r| cell(r, 0))
        .ok_or_else(|| Error::NotFound(format!("table `{db}`.`{t}`")))
}

async fn list_table_folders(
    conn: &mut mysql_async::Conn,
    parent: &ObjectPath,
    db: &str,
    t: &str,
    kind: ObjectKind,
) -> Result<Vec<ObjectNode>> {
    let ty = table_type(conn, db, t).await?;
    let mut out = vec![node(
        parent.child(ObjectKind::Folder, folder::COLUMNS),
        ObjectKind::Folder,
        folder::COLUMNS,
        true,
        None,
    )];
    if kind == ObjectKind::View || ty == "VIEW" {
        return Ok(out);
    }
    let c = rows(
        conn,
        "SELECT (SELECT COUNT(*) FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?), \
                (SELECT COUNT(*) FROM information_schema.TRIGGERS WHERE EVENT_OBJECT_SCHEMA = ? AND EVENT_OBJECT_TABLE = ?)",
        (db, t, db, t),
    )
    .await?;
    let (idx, trg) = c
        .first()
        .map(|r| (count_of(r, 0), count_of(r, 1)))
        .unwrap_or_default();
    for (key, n) in [(folder::INDEXES, idx), (folder::TRIGGERS, trg)] {
        out.push(node(
            parent.child(ObjectKind::Folder, key),
            ObjectKind::Folder,
            key,
            n > 0,
            Some(n.to_string()),
        ));
    }
    Ok(out)
}

async fn list_table_folder(
    conn: &mut mysql_async::Conn,
    parent: &ObjectPath,
    db: &str,
    t: &str,
    kind: ObjectKind,
    f: &str,
) -> Result<Vec<ObjectNode>> {
    let base = parent.clone();
    let _ = kind;
    match f {
        folder::COLUMNS => {
            let rs = rows(
                conn,
                "SELECT COLUMN_NAME, COLUMN_TYPE FROM information_schema.COLUMNS \
                 WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION",
                (db, t),
            )
            .await?;
            if rs.is_empty() {
                table_type(conn, db, t).await?;
            }
            Ok(rs
                .iter()
                .filter_map(|r| {
                    let name = cell(r, 0)?;
                    Some(node(
                        base.child(ObjectKind::Column, name.clone()),
                        ObjectKind::Column,
                        &name,
                        false,
                        cell(r, 1),
                    ))
                })
                .collect())
        }
        folder::INDEXES => {
            let idx = fetch_indexes(conn, db, t).await?;
            Ok(idx
                .iter()
                .map(|i| {
                    let detail = format!(
                        "{}({})",
                        if i.primary {
                            "PRIMARY KEY "
                        } else if i.unique {
                            "UNIQUE "
                        } else {
                            ""
                        },
                        i.columns.join(", ")
                    );
                    node(
                        base.child(ObjectKind::Index, i.name.clone()),
                        ObjectKind::Index,
                        &i.name,
                        false,
                        Some(detail),
                    )
                })
                .collect())
        }
        folder::TRIGGERS => {
            let rs = rows(
                conn,
                "SELECT TRIGGER_NAME, ACTION_TIMING, EVENT_MANIPULATION FROM information_schema.TRIGGERS \
                 WHERE EVENT_OBJECT_SCHEMA = ? AND EVENT_OBJECT_TABLE = ? ORDER BY TRIGGER_NAME",
                (db, t),
            )
            .await?;
            Ok(rs
                .iter()
                .filter_map(|r| {
                    let name = cell(r, 0)?;
                    let detail = format!(
                        "{} {}",
                        cell(r, 1).unwrap_or_default(),
                        cell(r, 2).unwrap_or_default()
                    );
                    Some(node(
                        base.child(ObjectKind::Trigger, name.clone()),
                        ObjectKind::Trigger,
                        &name,
                        false,
                        Some(detail.trim().to_string()),
                    ))
                })
                .collect())
        }
        other => Err(Error::NotFound(format!("unknown folder `{other}`"))),
    }
}

/// One index as reported by `SHOW INDEX`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexInfo {
    /// Index name (`PRIMARY` for the primary key).
    pub name: String,
    /// Column / expression list, with prefix lengths and `DESC` markers.
    pub columns: Vec<String>,
    /// The same key parts as plain names (no quoting, prefix length or `DESC`); an expression
    /// key part is `(expr)`. Prefix lengths and sort order live only in `columns` / the DDL.
    pub names: Vec<String>,
    /// Enforces uniqueness.
    pub unique: bool,
    /// Primary key.
    pub primary: bool,
    /// `BTREE`, `HASH`, `FULLTEXT`, `SPATIAL`, `RTREE`.
    pub method: String,
    /// Index comment.
    pub comment: String,
    /// False for MySQL 8 invisible indexes.
    pub visible: bool,
}

pub(crate) async fn fetch_indexes(
    conn: &mut mysql_async::Conn,
    db: &str,
    table: &str,
) -> Result<Vec<IndexInfo>> {
    let rs: Vec<Row> = conn
        .query(format!("SHOW INDEX FROM {}", quote_table(db, table)))
        .await
        .map_err(|e| match server_code(&e) {
            Some(1146 | 1049) => Error::NotFound(format!("table `{db}`.`{table}`")),
            _ => map_query_error(e),
        })?;
    let mut out: Vec<IndexInfo> = Vec::new();
    for r in &rs {
        let Some(name) = cell_named(r, "Key_name") else {
            continue;
        };
        let expr = cell_named(r, "Expression")
            .filter(|e| !e.is_empty())
            .map(|e| format!("({e})"));
        let (plain, mut col) = match expr {
            Some(e) => (e.clone(), e),
            None => {
                let c = cell_named(r, "Column_name").unwrap_or_default();
                let quoted = quote_ident(&c);
                (c, quoted)
            }
        };
        if let Some(sub) = cell_named(r, "Sub_part").filter(|s| !s.is_empty()) {
            col = format!("{col}({sub})");
        }
        if cell_named(r, "Collation").as_deref() == Some("D") {
            col.push_str(" DESC");
        }
        if out.last().is_none_or(|l| l.name != name) {
            out.push(IndexInfo {
                primary: name == "PRIMARY",
                unique: cell_named(r, "Non_unique").as_deref() == Some("0"),
                method: cell_named(r, "Index_type").unwrap_or_default(),
                comment: cell_named(r, "Index_comment").unwrap_or_default(),
                visible: cell_named(r, "Visible").is_none_or(|v| !v.eq_ignore_ascii_case("NO")),
                columns: Vec::new(),
                names: Vec::new(),
                name,
            });
        }
        if let Some(last) = out.last_mut() {
            last.columns.push(col);
            last.names.push(plain);
        }
    }
    Ok(out)
}

/// Rebuild a creation statement for an index. Column names are already quoted in `columns`.
pub fn index_ddl(db: &str, table: &str, i: &IndexInfo) -> String {
    use crate::sql::quote_literal;
    let cols = i.columns.join(", ");
    let tbl = quote_table(db, table);
    if i.primary {
        return format!("ALTER TABLE {tbl} ADD PRIMARY KEY ({cols});");
    }
    let kind = match i.method.to_ascii_uppercase().as_str() {
        "FULLTEXT" => "FULLTEXT ",
        "SPATIAL" => "SPATIAL ",
        _ if i.unique => "UNIQUE ",
        _ => "",
    };
    let using = match i.method.to_ascii_uppercase().as_str() {
        "HASH" => " USING HASH",
        _ => "",
    };
    let comment = if i.comment.is_empty() {
        String::new()
    } else {
        format!(" COMMENT {}", quote_literal(&i.comment, false))
    };
    let invisible = if i.visible { "" } else { " INVISIBLE" };
    format!(
        "CREATE {kind}INDEX {} ON {tbl} ({cols}){using}{comment}{invisible};",
        quote_ident(&i.name)
    )
}

fn not_found_code(e: &mysql_async::Error) -> bool {
    matches!(
        server_code(e),
        Some(1146 | 1305 | 1360 | 1049 | 1347 | 1109)
    )
}

async fn show_create(
    conn: &mut mysql_async::Conn,
    what: &str,
    qualified: String,
    column: &str,
) -> Result<String> {
    let rs: Vec<Row> = conn
        .query(format!("SHOW CREATE {what} {qualified}"))
        .await
        .map_err(|e| {
            if not_found_code(&e) {
                Error::NotFound(format!("{} {qualified}", what.to_lowercase()))
            } else {
                map_query_error(e)
            }
        })?;
    let row = rs
        .first()
        .ok_or_else(|| Error::NotFound(format!("{} {qualified}", what.to_lowercase())))?;
    // Routines whose definition the user may not see come back with a NULL definition column.
    cell_named(row, column).ok_or_else(|| {
        Error::query(format!(
            "insufficient privileges to view the definition of {} {qualified}",
            what.to_lowercase()
        ))
    })
}

/// DDL of the object at `path`.
pub async fn object_ddl(conn: &mut mysql_async::Conn, path: &ObjectPath) -> Result<String> {
    let segs: Vec<_> = path
        .segments
        .iter()
        .filter(|s| s.kind != ObjectKind::Folder)
        .collect();
    let last = segs
        .last()
        .ok_or_else(|| Error::Unsupported("object_ddl".into()))?;
    let db = segs
        .first()
        .filter(|s| s.kind == ObjectKind::Database)
        .map(|s| s.name.clone());
    let need_db = || {
        db.clone()
            .ok_or_else(|| Error::NotFound("database missing from path".into()))
    };
    match last.kind {
        ObjectKind::Table | ObjectKind::View if segs.len() == 2 => {
            let db = need_db()?;
            let (what, col) = if last.kind == ObjectKind::Table {
                ("TABLE", "Create Table")
            } else {
                ("VIEW", "Create View")
            };
            show_create(conn, what, quote_table(&db, &last.name), col).await
        }
        ObjectKind::Function if segs.len() == 2 => {
            show_create(
                conn,
                "FUNCTION",
                quote_table(&need_db()?, &last.name),
                "Create Function",
            )
            .await
        }
        ObjectKind::Procedure if segs.len() == 2 => {
            show_create(
                conn,
                "PROCEDURE",
                quote_table(&need_db()?, &last.name),
                "Create Procedure",
            )
            .await
        }
        ObjectKind::Trigger => {
            show_create(
                conn,
                "TRIGGER",
                quote_table(&need_db()?, &last.name),
                "SQL Original Statement",
            )
            .await
        }
        ObjectKind::Index if segs.len() == 3 => {
            let db = need_db()?;
            let table = &segs[1].name;
            let idx = fetch_indexes(conn, &db, table).await?;
            idx.iter()
                .find(|i| i.name == last.name)
                .map(|i| index_ddl(&db, table, i))
                .ok_or_else(|| {
                    Error::NotFound(format!("index `{}` on `{db}`.`{table}`", last.name))
                })
        }
        _ => Err(Error::Unsupported("object_ddl".into())),
    }
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.filter(|x| !x.is_empty())
}

fn pairs(row: &Row, a: usize) -> Option<String> {
    cell(row, a)
}

/// Details of a table or view.
pub async fn table_details(
    conn: &mut mysql_async::Conn,
    path: &ObjectPath,
    is_mariadb: bool,
) -> Result<TableDetails> {
    let (db, table) = crate::sql::table_target(path)?;
    // MySQL 8 caches statistics (default 24h); ask for fresh numbers. Unknown variable on 5.7
    // and MariaDB: ignore.
    let _ = conn
        .query_drop("SET SESSION information_schema_stats_expiry = 0")
        .await;

    let t = rows(
        conn,
        "SELECT TABLE_TYPE, TABLE_COMMENT, TABLE_ROWS, DATA_LENGTH, INDEX_LENGTH \
         FROM information_schema.TABLES WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
        (&db, &table),
    )
    .await?;
    let t = t
        .first()
        .ok_or_else(|| Error::NotFound(format!("table `{db}`.`{table}`")))?;
    let is_view = pairs(t, 0).as_deref() == Some("VIEW");
    let kind = if is_view {
        ObjectKind::View
    } else {
        ObjectKind::Table
    };
    let comment = if is_view { None } else { non_empty(cell(t, 1)) };
    let estimated_rows = if is_view {
        None
    } else {
        cell(t, 2).and_then(|s| s.parse().ok())
    };
    let size_bytes = if is_view {
        None
    } else {
        let d: Option<u64> = cell(t, 3).and_then(|s| s.parse().ok());
        let i: Option<u64> = cell(t, 4).and_then(|s| s.parse().ok());
        match (d, i) {
            (None, None) => None,
            (d, i) => Some(d.unwrap_or(0) + i.unwrap_or(0)),
        }
    };

    let cols = rows(
        conn,
        "SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_TYPE, DATA_TYPE, IS_NULLABLE, COLUMN_DEFAULT, \
                COLUMN_COMMENT, COLUMN_KEY, EXTRA, GENERATION_EXPRESSION \
         FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION",
        (&db, &table),
    )
    .await?;
    let columns = cols
        .iter()
        .map(|r| {
            let nullable = cell(r, 4).as_deref() == Some("YES");
            let column_type = cell(r, 2).unwrap_or_default();
            let data_type = cell(r, 3).unwrap_or_default();
            let mut default = cell(r, 5);
            if is_mariadb && nullable && default.as_deref() == Some("NULL") {
                default = None;
            }
            let mut extra = cell(r, 8).unwrap_or_default();
            if let Some(g) = non_empty(cell(r, 9)) {
                extra = format!("{extra} AS ({g})").trim().to_string();
            }
            ColumnDetail {
                name: cell(r, 0).unwrap_or_default(),
                ordinal: cell(r, 1).and_then(|s| s.parse().ok()).unwrap_or(0),
                value_kind: kind_from_data_type(&data_type, &column_type),
                type_name: column_type,
                nullable,
                default,
                comment: non_empty(cell(r, 6)),
                primary_key: cell(r, 7).as_deref() == Some("PRI"),
                extra: non_empty(Some(extra)),
            }
        })
        .collect::<Vec<_>>();
    let _ = ValueKind::Null;

    let indexes = if is_view {
        Vec::new()
    } else {
        fetch_indexes(conn, &db, &table)
            .await?
            .into_iter()
            .map(|i| IndexDetail {
                definition: Some(index_ddl(&db, &table, &i).trim_end_matches(';').to_string()),
                columns: i.names.clone(),
                unique: i.unique,
                primary: i.primary,
                partial: false,
                method: non_empty(Some(i.method.clone())),
                name: i.name,
            })
            .collect()
    };

    let fk_rows = rows(
        conn,
        "SELECT kcu.CONSTRAINT_NAME, kcu.COLUMN_NAME, kcu.REFERENCED_TABLE_SCHEMA, \
                kcu.REFERENCED_TABLE_NAME, kcu.REFERENCED_COLUMN_NAME, rc.UPDATE_RULE, rc.DELETE_RULE \
         FROM information_schema.KEY_COLUMN_USAGE kcu \
         JOIN information_schema.REFERENTIAL_CONSTRAINTS rc \
           ON rc.CONSTRAINT_SCHEMA = kcu.CONSTRAINT_SCHEMA AND rc.CONSTRAINT_NAME = kcu.CONSTRAINT_NAME \
          AND rc.TABLE_NAME = kcu.TABLE_NAME \
         WHERE kcu.TABLE_SCHEMA = ? AND kcu.TABLE_NAME = ? AND kcu.REFERENCED_TABLE_NAME IS NOT NULL \
         ORDER BY kcu.CONSTRAINT_NAME, kcu.ORDINAL_POSITION",
        (&db, &table),
    )
    .await?;
    let mut foreign_keys: Vec<ForeignKeyDetail> = Vec::new();
    for r in &fk_rows {
        let name = cell(r, 0).unwrap_or_default();
        if foreign_keys.last().is_none_or(|f| f.name != name) {
            let ref_db = cell(r, 2).unwrap_or_else(|| db.clone());
            let ref_table = cell(r, 3).unwrap_or_default();
            foreign_keys.push(ForeignKeyDetail {
                name,
                columns: Vec::new(),
                referenced_table: ObjectPath::root()
                    .child(ObjectKind::Database, ref_db)
                    .child(ObjectKind::Folder, folder::TABLES)
                    .child(ObjectKind::Table, ref_table),
                referenced_columns: Vec::new(),
                on_update: cell(r, 5),
                on_delete: cell(r, 6),
            });
        }
        if let Some(f) = foreign_keys.last_mut() {
            f.columns.push(cell(r, 1).unwrap_or_default());
            f.referenced_columns.push(cell(r, 4).unwrap_or_default());
        }
    }

    Ok(TableDetails {
        path: path.clone(),
        kind,
        comment,
        columns,
        indexes,
        foreign_keys,
        estimated_rows,
        size_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_names_are_quoted() {
        assert_eq!(account_name("root", "%"), "'root'@'%'");
        assert_eq!(account_name("o'neil", "h"), "'o''neil'@'h'");
        assert_eq!(account_name("", "localhost"), "''@'localhost'");
    }

    #[test]
    fn resolve_paths() {
        let db = ObjectPath::root().child(ObjectKind::Database, "d");
        assert_eq!(resolve(&ObjectPath::root()).unwrap(), Place::Root);
        assert_eq!(
            resolve(&ObjectPath::root().child(ObjectKind::Folder, "roles")).unwrap(),
            Place::Roles
        );
        assert_eq!(resolve(&db).unwrap(), Place::Database("d".into()));
        assert_eq!(
            resolve(&db.child(ObjectKind::Folder, "tables")).unwrap(),
            Place::DbFolder("d".into(), "tables".into())
        );
        let t = db.child(ObjectKind::Table, "t");
        assert_eq!(
            resolve(&t).unwrap(),
            Place::Table("d".into(), "t".into(), ObjectKind::Table)
        );
        // Table reached through its folder.
        let t2 = db
            .child(ObjectKind::Folder, "tables")
            .child(ObjectKind::Table, "t");
        assert_eq!(
            resolve(&t2).unwrap(),
            Place::Table("d".into(), "t".into(), ObjectKind::Table)
        );
        assert_eq!(
            resolve(&t.child(ObjectKind::Folder, "columns")).unwrap(),
            Place::TableFolder("d".into(), "t".into(), ObjectKind::Table, "columns".into())
        );
        assert!(resolve(&t.child(ObjectKind::Column, "c")).is_err());
    }

    fn idx(name: &str, unique: bool, primary: bool, method: &str) -> IndexInfo {
        IndexInfo {
            name: name.into(),
            columns: vec!["`a`".into(), "`b c`(10)".into()],
            names: vec!["a".into(), "b c".into()],
            unique,
            primary,
            method: method.into(),
            comment: String::new(),
            visible: true,
        }
    }

    #[test]
    fn index_statements() {
        assert_eq!(
            index_ddl("d", "t", &idx("PRIMARY", true, true, "BTREE")),
            "ALTER TABLE `d`.`t` ADD PRIMARY KEY (`a`, `b c`(10));"
        );
        assert_eq!(
            index_ddl("d", "t", &idx("u", true, false, "BTREE")),
            "CREATE UNIQUE INDEX `u` ON `d`.`t` (`a`, `b c`(10));"
        );
        assert_eq!(
            index_ddl("d", "t", &idx("h", false, false, "HASH")),
            "CREATE INDEX `h` ON `d`.`t` (`a`, `b c`(10)) USING HASH;"
        );
        assert_eq!(
            index_ddl("d", "t", &idx("f", false, false, "FULLTEXT")),
            "CREATE FULLTEXT INDEX `f` ON `d`.`t` (`a`, `b c`(10));"
        );
        let mut i = idx("c", false, false, "BTREE");
        i.comment = "it's".into();
        i.visible = false;
        assert_eq!(
            index_ddl("d", "t", &i),
            "CREATE INDEX `c` ON `d`.`t` (`a`, `b c`(10)) COMMENT 'it''s' INVISIBLE;"
        );
    }
}
