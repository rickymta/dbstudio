//! Object tree, table metadata, search, DDL text and the schema model, all from `system.*`.
//!
//! Path convention: `[Database d, Folder tables, Table t, Folder columns, Column c]`. Folder
//! segments are optional when resolving a path.

use dbstudio_core::{
    ColumnDetail, DdlItem, DdlLanguage, DdlRequest, DdlSection, Error, IndexDetail, ObjectKind,
    ObjectNode, ObjectPath, ObjectSearchRequest, Result, SchemaModel, SchemaModelRequest,
    TableDetails, TableModel, Value, ValueKind, folder,
};

use crate::conn::{Core, cell_bool, cell_opt, cell_text, cell_u64, quote_ident, quote_literal};
use crate::types::kind_of_name;

pub(crate) const SYSTEM_DATABASES: [&str; 3] =
    ["system", "INFORMATION_SCHEMA", "information_schema"];

fn system_list() -> String {
    SYSTEM_DATABASES
        .iter()
        .map(|d| quote_literal(d))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Which tree folder a table-like object lives in, from its engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Class {
    Table,
    View,
    MaterializedView,
    Dictionary,
}

impl Class {
    pub(crate) fn of_engine(engine: &str) -> Class {
        match engine {
            "View" | "LiveView" | "WindowView" => Class::View,
            "MaterializedView" => Class::MaterializedView,
            "Dictionary" => Class::Dictionary,
            _ => Class::Table,
        }
    }

    pub(crate) fn kind(self) -> ObjectKind {
        match self {
            Class::Table => ObjectKind::Table,
            Class::View => ObjectKind::View,
            Class::MaterializedView => ObjectKind::MaterializedView,
            Class::Dictionary => ObjectKind::Dictionary,
        }
    }

    pub(crate) fn folder(self) -> &'static str {
        match self {
            Class::Table => folder::TABLES,
            Class::View => folder::VIEWS,
            Class::MaterializedView => folder::MATERIALIZED_VIEWS,
            Class::Dictionary => folder::DICTIONARIES,
        }
    }

    fn of_folder(name: &str) -> Option<Class> {
        match name {
            folder::TABLES => Some(Class::Table),
            folder::VIEWS => Some(Class::View),
            folder::MATERIALIZED_VIEWS => Some(Class::MaterializedView),
            folder::DICTIONARIES => Some(Class::Dictionary),
            _ => None,
        }
    }

    /// SQL condition on the `engine` column selecting this class.
    fn engine_condition(self) -> &'static str {
        match self {
            Class::Table => {
                "engine NOT IN ('View', 'LiveView', 'WindowView', 'MaterializedView', 'Dictionary')"
            }
            Class::View => "engine IN ('View', 'LiveView', 'WindowView')",
            Class::MaterializedView => "engine = 'MaterializedView'",
            Class::Dictionary => "engine = 'Dictionary'",
        }
    }
}

/// Resolved location inside the tree.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Place {
    Root,
    Database(String),
    DbFolder(String, Class),
    Object(String, String, ObjectKind),
    ObjectFolder(String, String, String),
    Column(String, String, String),
}

pub(crate) fn resolve(path: &ObjectPath) -> Result<Place> {
    let n = path.segments.len();
    let segs: Vec<_> = path
        .segments
        .iter()
        .enumerate()
        .filter(|(i, s)| s.kind != ObjectKind::Folder || *i + 1 == n)
        .map(|(_, s)| s)
        .collect();
    let is_obj = |k: ObjectKind| {
        matches!(
            k,
            ObjectKind::Table
                | ObjectKind::View
                | ObjectKind::MaterializedView
                | ObjectKind::Dictionary
        )
    };
    let bad = || Error::NotFound(format!("unsupported tree path ({n} segments)"));
    match segs.as_slice() {
        [] => Ok(Place::Root),
        [d] if d.kind == ObjectKind::Database => Ok(Place::Database(d.name.clone())),
        [d, f] if d.kind == ObjectKind::Database && f.kind == ObjectKind::Folder => {
            Class::of_folder(&f.name)
                .map(|c| Place::DbFolder(d.name.clone(), c))
                .ok_or_else(|| Error::NotFound(format!("unknown folder `{}`", f.name)))
        }
        [d, t] if d.kind == ObjectKind::Database && is_obj(t.kind) => {
            Ok(Place::Object(d.name.clone(), t.name.clone(), t.kind))
        }
        [d, t, f]
            if d.kind == ObjectKind::Database && is_obj(t.kind) && f.kind == ObjectKind::Folder =>
        {
            Ok(Place::ObjectFolder(
                d.name.clone(),
                t.name.clone(),
                f.name.clone(),
            ))
        }
        [d, t, c]
            if d.kind == ObjectKind::Database && is_obj(t.kind) && c.kind == ObjectKind::Column =>
        {
            Ok(Place::Column(
                d.name.clone(),
                t.name.clone(),
                c.name.clone(),
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

fn user_tables_cond() -> &'static str {
    "NOT startsWith(name, '.inner')"
}

/// Children of `parent`.
pub(crate) async fn list_children(core: &Core, parent: &ObjectPath) -> Result<Vec<ObjectNode>> {
    match resolve(parent)? {
        Place::Root => list_databases(core, parent).await,
        Place::Database(db) => list_db_folders(core, parent, &db).await,
        Place::DbFolder(db, class) => list_db_folder(core, parent, &db, class).await,
        Place::Object(db, t, kind) => list_object_folders(core, parent, &db, &t, kind).await,
        Place::ObjectFolder(db, t, f) => list_object_folder(core, parent, &db, &t, &f).await,
        Place::Column(..) => Ok(Vec::new()),
    }
}

async fn list_databases(core: &Core, parent: &ObjectPath) -> Result<Vec<ObjectNode>> {
    let filter = if core.show_system {
        String::new()
    } else {
        format!("WHERE name NOT IN ({})", system_list())
    };
    let rows = core
        .rows_db(
            None,
            &format!("SELECT name, engine FROM system.databases {filter} ORDER BY name"),
        )
        .await?;
    Ok(rows
        .iter()
        .map(|r| {
            let name = cell_text(r, 0);
            let system = SYSTEM_DATABASES.contains(&name.as_str());
            node(
                parent.child(ObjectKind::Database, name.clone()),
                ObjectKind::Database,
                &name,
                true,
                Some(if system {
                    "system".to_string()
                } else {
                    cell_text(r, 1)
                }),
            )
        })
        .collect())
}

async fn require_database(core: &Core, db: &str) -> Result<()> {
    let n = core
        .rows_db(
            None,
            &format!(
                "SELECT count() FROM system.databases WHERE name = {}",
                quote_literal(db)
            ),
        )
        .await?
        .first()
        .and_then(|r| cell_u64(r, 0))
        .unwrap_or(0);
    if n == 0 {
        Err(Error::NotFound(format!("database `{db}`")))
    } else {
        Ok(())
    }
}

async fn list_db_folders(core: &Core, parent: &ObjectPath, db: &str) -> Result<Vec<ObjectNode>> {
    let rows = core
        .rows_db(
            None,
            &format!(
                "SELECT countIf({t}), countIf({v}), countIf({m}), countIf({d}) \
                 FROM system.tables WHERE database = {} AND {}",
                quote_literal(db),
                user_tables_cond(),
                t = Class::Table.engine_condition(),
                v = Class::View.engine_condition(),
                m = Class::MaterializedView.engine_condition(),
                d = Class::Dictionary.engine_condition(),
            ),
        )
        .await?;
    let counts: Vec<u64> = rows
        .first()
        .map(|r| (0..4).map(|i| cell_u64(r, i).unwrap_or(0)).collect())
        .unwrap_or_else(|| vec![0; 4]);
    if counts.iter().sum::<u64>() == 0 {
        require_database(core, db).await?;
    }
    Ok([
        Class::Table,
        Class::View,
        Class::MaterializedView,
        Class::Dictionary,
    ]
    .into_iter()
    .zip(counts)
    .filter(|(_, n)| *n > 0)
    .map(|(class, n)| {
        node(
            parent.child(ObjectKind::Folder, class.folder()),
            ObjectKind::Folder,
            class.folder(),
            true,
            Some(n.to_string()),
        )
    })
    .collect())
}

async fn list_db_folder(
    core: &Core,
    parent: &ObjectPath,
    db: &str,
    class: Class,
) -> Result<Vec<ObjectNode>> {
    let rows = core
        .rows_db(
            None,
            &format!(
                "SELECT name, engine FROM system.tables WHERE database = {} AND {} AND {} ORDER BY name",
                quote_literal(db),
                user_tables_cond(),
                class.engine_condition()
            ),
        )
        .await?;
    if rows.is_empty() {
        require_database(core, db).await?;
    }
    let kind = class.kind();
    Ok(rows
        .iter()
        .map(|r| {
            let name = cell_text(r, 0);
            node(
                parent.child(kind, name.clone()),
                kind,
                &name,
                class != Class::Dictionary,
                Some(cell_text(r, 1)),
            )
        })
        .collect())
}

async fn object_exists(core: &Core, db: &str, name: &str) -> Result<Option<String>> {
    let rows = core
        .rows_db(
            None,
            &format!(
                "SELECT engine FROM system.tables WHERE database = {} AND name = {}",
                quote_literal(db),
                quote_literal(name)
            ),
        )
        .await?;
    Ok(rows.first().map(|r| cell_text(r, 0)))
}

async fn require_object(core: &Core, db: &str, name: &str) -> Result<String> {
    object_exists(core, db, name)
        .await?
        .ok_or_else(|| Error::NotFound(format!("table `{db}`.`{name}`")))
}

async fn list_object_folders(
    core: &Core,
    parent: &ObjectPath,
    db: &str,
    t: &str,
    kind: ObjectKind,
) -> Result<Vec<ObjectNode>> {
    let engine = require_object(core, db, t).await?;
    let mut out = vec![node(
        parent.child(ObjectKind::Folder, folder::COLUMNS),
        ObjectKind::Folder,
        folder::COLUMNS,
        true,
        None,
    )];
    if kind == ObjectKind::Table && Class::of_engine(&engine) == Class::Table {
        let n = core
            .rows_db(
                None,
                &format!(
                    "SELECT count() FROM system.data_skipping_indices WHERE database = {} AND `table` = {}",
                    quote_literal(db),
                    quote_literal(t)
                ),
            )
            .await?
            .first()
            .and_then(|r| cell_u64(r, 0))
            .unwrap_or(0);
        out.push(node(
            parent.child(ObjectKind::Folder, folder::INDEXES),
            ObjectKind::Folder,
            folder::INDEXES,
            n > 0,
            Some(n.to_string()),
        ));
    }
    Ok(out)
}

async fn list_object_folder(
    core: &Core,
    parent: &ObjectPath,
    db: &str,
    t: &str,
    f: &str,
) -> Result<Vec<ObjectNode>> {
    match f {
        folder::COLUMNS => {
            let rows = core
                .rows_db(
                    None,
                    &format!(
                        "SELECT name, type FROM system.columns WHERE database = {} AND `table` = {} ORDER BY position",
                        quote_literal(db),
                        quote_literal(t)
                    ),
                )
                .await?;
            if rows.is_empty() {
                require_object(core, db, t).await?;
            }
            Ok(rows
                .iter()
                .map(|r| {
                    let name = cell_text(r, 0);
                    node(
                        parent.child(ObjectKind::Column, name.clone()),
                        ObjectKind::Column,
                        &name,
                        false,
                        Some(cell_text(r, 1)),
                    )
                })
                .collect())
        }
        folder::INDEXES => {
            let rows = core
                .rows_db(
                    None,
                    &format!(
                        "SELECT name, type, expr, granularity FROM system.data_skipping_indices \
                         WHERE database = {} AND `table` = {} ORDER BY name",
                        quote_literal(db),
                        quote_literal(t)
                    ),
                )
                .await?;
            Ok(rows
                .iter()
                .map(|r| {
                    let name = cell_text(r, 0);
                    node(
                        parent.child(ObjectKind::Index, name.clone()),
                        ObjectKind::Index,
                        &name,
                        false,
                        Some(format!("{}({})", cell_text(r, 1), cell_text(r, 2))),
                    )
                })
                .collect())
        }
        other => Err(Error::NotFound(format!("unknown folder `{other}`"))),
    }
}

/// Server-side search of tables, views, materialized views, dictionaries (and columns when
/// asked for) by case-insensitive substring.
pub(crate) async fn search_objects(
    core: &Core,
    req: &ObjectSearchRequest,
) -> Result<Vec<ObjectNode>> {
    let query = req.query.trim();
    if query.is_empty() {
        return Ok(Vec::new());
    }
    let limit = req.limit.clamp(1, 1000);
    let object_kinds = [
        ObjectKind::Table,
        ObjectKind::View,
        ObjectKind::MaterializedView,
        ObjectKind::Dictionary,
    ];
    let wants = |k: ObjectKind| {
        if req.kinds.is_empty() {
            object_kinds.contains(&k)
        } else {
            req.kinds.contains(&k)
        }
    };
    let scope_db = match resolve(&req.scope)? {
        Place::Root => None,
        Place::Database(d) | Place::DbFolder(d, _) => Some(d),
        _ => {
            return Err(Error::NotFound(
                "search scope must be the root or a database".into(),
            ));
        }
    };
    let scope_sql = match &scope_db {
        Some(d) => format!("database = {}", quote_literal(d)),
        None if core.show_system => "1".to_string(),
        None => format!("database NOT IN ({})", system_list()),
    };
    let pattern = quote_literal(&format!(
        "%{}%",
        query
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
    ));
    let mut out: Vec<(u8, String, ObjectNode)> = Vec::new();
    let q_lc = query.to_lowercase();
    let rank = |name: &str| {
        let n = name.to_lowercase();
        if n == q_lc {
            0
        } else if n.starts_with(&q_lc) {
            1
        } else {
            2
        }
    };
    let kinds: Vec<ObjectKind> = object_kinds.into_iter().filter(|k| wants(*k)).collect();
    if !kinds.is_empty() {
        let classes: Vec<String> = [
            Class::Table,
            Class::View,
            Class::MaterializedView,
            Class::Dictionary,
        ]
        .into_iter()
        .filter(|c| kinds.contains(&c.kind()))
        .map(|c| format!("({})", c.engine_condition()))
        .collect();
        let rows = core
            .rows_db(
                None,
                &format!(
                    "SELECT database, name, engine FROM system.tables WHERE {scope_sql} AND {} \
                     AND name ILIKE {pattern} AND ({}) ORDER BY name, database LIMIT {limit}",
                    user_tables_cond(),
                    classes.join(" OR ")
                ),
            )
            .await?;
        for r in rows {
            let (db, name) = (cell_text(&r, 0), cell_text(&r, 1));
            let class = Class::of_engine(&cell_text(&r, 2));
            let path = ObjectPath::root()
                .child(ObjectKind::Database, db.clone())
                .child(ObjectKind::Folder, class.folder())
                .child(class.kind(), name.clone());
            out.push((
                rank(&name),
                name.to_lowercase(),
                node(
                    path,
                    class.kind(),
                    &name,
                    class != Class::Dictionary,
                    Some(db),
                ),
            ));
        }
    }
    if wants(ObjectKind::Column) && !req.kinds.is_empty() {
        let rows = core
            .rows_db(
                None,
                &format!(
                    "SELECT c.database, c.`table`, c.name, c.type, t.engine FROM system.columns AS c \
                     INNER JOIN system.tables AS t ON t.database = c.database AND t.name = c.`table` \
                     WHERE c.{scope_sql} AND c.name ILIKE {pattern} AND NOT startsWith(c.`table`, '.inner') \
                     ORDER BY c.name, c.database, c.`table` LIMIT {limit}"
                ),
            )
            .await?;
        for r in rows {
            let (db, table, name) = (cell_text(&r, 0), cell_text(&r, 1), cell_text(&r, 2));
            let class = Class::of_engine(&cell_text(&r, 4));
            let path = ObjectPath::root()
                .child(ObjectKind::Database, db.clone())
                .child(ObjectKind::Folder, class.folder())
                .child(class.kind(), table.clone())
                .child(ObjectKind::Folder, folder::COLUMNS)
                .child(ObjectKind::Column, name.clone());
            out.push((
                rank(&name),
                name.to_lowercase(),
                node(
                    path,
                    ObjectKind::Column,
                    &name,
                    false,
                    Some(format!("{db}.{table}: {}", cell_text(&r, 3))),
                ),
            ));
        }
    }
    out.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    out.truncate(limit as usize);
    Ok(out.into_iter().map(|(_, _, n)| n).collect())
}

/// Facts of one table-like object from `system.tables`.
#[derive(Debug, Clone, Default)]
pub(crate) struct TableFacts {
    pub engine: String,
    pub comment: Option<String>,
    pub total_rows: Option<u64>,
    pub total_bytes: Option<u64>,
    pub sorting_key: String,
    pub primary_key: String,
    pub partition_key: String,
    pub sampling_key: String,
}

pub(crate) async fn table_facts(core: &Core, db: &str, t: &str) -> Result<TableFacts> {
    let rows = core
        .rows_db(
            None,
            &format!(
                "SELECT engine, comment, total_rows, total_bytes, sorting_key, primary_key, \
                 partition_key, sampling_key FROM system.tables WHERE database = {} AND name = {}",
                quote_literal(db),
                quote_literal(t)
            ),
        )
        .await?;
    let r = rows
        .first()
        .ok_or_else(|| Error::NotFound(format!("table `{db}`.`{t}`")))?;
    Ok(TableFacts {
        engine: cell_text(r, 0),
        comment: cell_opt(r, 1).filter(|c| !c.is_empty()),
        total_rows: cell_u64(r, 2),
        total_bytes: cell_u64(r, 3),
        sorting_key: cell_text(r, 4),
        primary_key: cell_text(r, 5),
        partition_key: cell_text(r, 6),
        sampling_key: cell_text(r, 7),
    })
}

/// One row of `system.columns`.
#[derive(Debug, Clone)]
pub(crate) struct ColumnFacts {
    pub name: String,
    pub ty: String,
    pub position: u32,
    pub default_kind: String,
    pub default_expr: String,
    pub comment: Option<String>,
    pub in_partition_key: bool,
    pub in_sorting_key: bool,
    pub codec: String,
}

pub(crate) const COLUMN_QUERY: &str = "SELECT name, type, position, default_kind, default_expression, \
    comment, is_in_partition_key, is_in_sorting_key, compression_codec";

pub(crate) fn column_facts(r: &[Value]) -> ColumnFacts {
    ColumnFacts {
        name: cell_text(r, 0),
        ty: cell_text(r, 1),
        position: cell_u64(r, 2).unwrap_or(0) as u32,
        default_kind: cell_text(r, 3),
        default_expr: cell_text(r, 4),
        comment: cell_opt(r, 5).filter(|c| !c.is_empty()),
        in_partition_key: cell_bool(r, 6),
        in_sorting_key: cell_bool(r, 7),
        codec: cell_text(r, 8),
    }
}

pub(crate) async fn column_list(core: &Core, db: &str, t: &str) -> Result<Vec<ColumnFacts>> {
    let rows = core
        .rows_db(
            None,
            &format!(
                "{COLUMN_QUERY} FROM system.columns WHERE database = {} AND `table` = {} ORDER BY position",
                quote_literal(db),
                quote_literal(t)
            ),
        )
        .await?;
    Ok(rows.iter().map(|r| column_facts(r)).collect())
}

fn detail_of(c: &ColumnFacts) -> ColumnDetail {
    let mut extra: Vec<String> = Vec::new();
    match c.default_kind.as_str() {
        "MATERIALIZED" | "ALIAS" | "EPHEMERAL" => extra.push(if c.default_expr.is_empty() {
            c.default_kind.clone()
        } else {
            format!("{} {}", c.default_kind, c.default_expr)
        }),
        _ => {}
    }
    if c.in_sorting_key {
        extra.push("sorting key".into());
    }
    if c.in_partition_key {
        extra.push("partition key".into());
    }
    if !c.codec.is_empty() {
        extra.push(c.codec.clone());
    }
    ColumnDetail {
        name: c.name.clone(),
        ordinal: c.position,
        type_name: c.ty.clone(),
        value_kind: kind_of_name(&c.ty),
        nullable: c.ty.starts_with("Nullable(") || c.ty.starts_with("LowCardinality(Nullable("),
        default: (c.default_kind == "DEFAULT" && !c.default_expr.is_empty())
            .then(|| c.default_expr.clone()),
        comment: c.comment.clone(),
        primary_key: false,
        extra: (!extra.is_empty()).then(|| extra.join("; ")),
    }
}

/// Details of a table, view, materialized view or dictionary.
pub(crate) async fn table_details(core: &Core, path: &ObjectPath) -> Result<TableDetails> {
    let (db, t, kind) = match resolve(path)? {
        Place::Object(d, t, k) => (d, t, k),
        Place::ObjectFolder(d, t, _) | Place::Column(d, t, _) => {
            let kind = path
                .segments
                .iter()
                .find(|s| s.name == t && s.kind != ObjectKind::Folder)
                .map_or(ObjectKind::Table, |s| s.kind);
            (d, t, kind)
        }
        _ => {
            return Err(Error::Invalid(
                "path does not identify a table or view".into(),
            ));
        }
    };
    let facts = table_facts(core, &db, &t).await?;
    let columns: Vec<ColumnDetail> = column_list(core, &db, &t)
        .await?
        .iter()
        .map(detail_of)
        .collect();
    let mut indexes = Vec::new();
    for (name, expr, method) in [
        ("PARTITION BY", &facts.partition_key, "partition"),
        ("ORDER BY", &facts.sorting_key, "sorting"),
        ("PRIMARY KEY", &facts.primary_key, "primary"),
        ("SAMPLE BY", &facts.sampling_key, "sampling"),
    ] {
        // The primary key defaults to the sorting key: list it only when it is a prefix of it.
        if !expr.is_empty() && !(method == "primary" && *expr == facts.sorting_key) {
            indexes.push(IndexDetail {
                name: name.to_string(),
                columns: expr
                    .split(',')
                    .map(|p| p.trim().trim_matches('`').to_string())
                    .filter(|p| columns.iter().any(|c| &c.name == p))
                    .collect(),
                unique: false,
                primary: false,
                method: Some(method.to_string()),
                definition: Some(expr.clone()),
                partial: false,
            });
        }
    }
    let skipping = core
        .rows_db(
            None,
            &format!(
                "SELECT name, type, expr, granularity FROM system.data_skipping_indices \
                 WHERE database = {} AND `table` = {} ORDER BY name",
                quote_literal(&db),
                quote_literal(&t)
            ),
        )
        .await?;
    for r in &skipping {
        let expr = cell_text(r, 2);
        indexes.push(IndexDetail {
            name: cell_text(r, 0),
            columns: expr
                .split(',')
                .map(|p| p.trim().trim_matches('`').to_string())
                .filter(|p| columns.iter().any(|c| &c.name == p))
                .collect(),
            unique: false,
            primary: false,
            method: Some(cell_text(r, 1)),
            definition: Some(format!("{expr} GRANULARITY {}", cell_text(r, 3))),
            partial: false,
        });
    }
    let class_kind = Class::of_engine(&facts.engine).kind();
    Ok(TableDetails {
        path: path.clone(),
        kind: if kind == ObjectKind::Table {
            class_kind
        } else {
            kind
        },
        comment: facts.comment.clone(),
        columns,
        indexes,
        foreign_keys: Vec::new(),
        estimated_rows: facts.total_rows,
        size_bytes: facts.total_bytes,
    })
}

/// `SHOW CREATE` text of a database, table, view, materialized view or dictionary.
pub(crate) async fn object_ddl(core: &Core, path: &ObjectPath) -> Result<String> {
    let sql = match resolve(path)? {
        Place::Database(db) => format!("SHOW CREATE DATABASE {}", quote_ident(&db)),
        Place::Object(db, t, kind) => {
            let what = if kind == ObjectKind::Dictionary {
                "DICTIONARY"
            } else {
                "TABLE"
            };
            format!(
                "SHOW CREATE {what} {}.{}",
                quote_ident(&db),
                quote_ident(&t)
            )
        }
        _ => return Err(Error::Unsupported("object_ddl".into())),
    };
    let rows = core.rows_db(None, &sql).await?;
    rows.first()
        .and_then(|r| cell_opt(r, 0))
        .ok_or_else(|| Error::NotFound(format!("object for `{sql}`")))
}

fn scope_database(scope: &ObjectPath) -> Result<Option<String>> {
    match resolve(scope)? {
        Place::Root => Ok(None),
        Place::Database(d) | Place::DbFolder(d, _) => Ok(Some(d)),
        _ => Err(Error::Invalid(
            "scope must be the root or a database".into(),
        )),
    }
}

/// Tables, views, materialized views and dictionaries of the scope with their columns.
pub(crate) async fn schema_model(core: &Core, req: SchemaModelRequest) -> Result<SchemaModel> {
    const CAP: usize = 5000;
    let db = scope_database(&req.scope)?;
    let wanted: Vec<(String, String)> = req
        .tables
        .iter()
        .filter_map(|p| crate::sql::table_target(p).ok())
        .collect();
    let scope_sql = match &db {
        Some(d) => format!("database = {}", quote_literal(d)),
        None if core.show_system => "1".to_string(),
        None => format!("database NOT IN ({})", system_list()),
    };
    let only = if wanted.is_empty() {
        String::new()
    } else {
        let list: Vec<String> = wanted
            .iter()
            .map(|(d, t)| format!("({}, {})", quote_literal(d), quote_literal(t)))
            .collect();
        format!(" AND (database, name) IN ({})", list.join(", "))
    };
    let classes = if req.include_views || !wanted.is_empty() {
        "engine != 'Dictionary'"
    } else {
        "engine NOT IN ('View', 'LiveView', 'WindowView', 'MaterializedView', 'Dictionary')"
    };
    let rows = core
        .rows_db(
            None,
            &format!(
                "SELECT database, name, engine, comment, total_rows FROM system.tables \
                 WHERE {scope_sql} AND {classes} AND {}{only} ORDER BY database, name LIMIT {}",
                user_tables_cond(),
                CAP + 1
            ),
        )
        .await?;
    let truncated = rows.len() > CAP;
    let rows = &rows[..rows.len().min(CAP)];
    let mut tables: Vec<TableModel> = rows
        .iter()
        .map(|r| {
            let (d, n) = (cell_text(r, 0), cell_text(r, 1));
            let class = Class::of_engine(&cell_text(r, 2));
            TableModel {
                path: ObjectPath::root()
                    .child(ObjectKind::Database, d.clone())
                    .child(ObjectKind::Folder, class.folder())
                    .child(class.kind(), n.clone()),
                kind: class.kind(),
                schema: Some(d),
                name: n,
                comment: if req.include_comments {
                    cell_opt(r, 3).filter(|c| !c.is_empty())
                } else {
                    None
                },
                columns: Vec::new(),
                primary_key: Vec::new(),
                unique_keys: Vec::new(),
                estimated_rows: cell_u64(r, 4),
            }
        })
        .collect();
    if !tables.is_empty() {
        let crows = core
            .rows_db(
                None,
                &format!(
                    "SELECT database, `table`, name, type, position, default_kind, default_expression, \
                     comment, is_in_partition_key, is_in_sorting_key, compression_codec \
                     FROM system.columns WHERE {scope_sql}{} ORDER BY database, `table`, position",
                    only.replace("(database, name)", "(database, `table`)")
                ),
            )
            .await?;
        let mut idx: std::collections::HashMap<(String, String), usize> = tables
            .iter()
            .enumerate()
            .map(|(i, t)| ((t.schema.clone().unwrap_or_default(), t.name.clone()), i))
            .collect();
        for r in &crows {
            let key = (cell_text(r, 0), cell_text(r, 1));
            if let Some(&i) = idx.get(&key) {
                let facts = ColumnFacts {
                    name: cell_text(r, 2),
                    ty: cell_text(r, 3),
                    position: cell_u64(r, 4).unwrap_or(0) as u32,
                    default_kind: cell_text(r, 5),
                    default_expr: cell_text(r, 6),
                    comment: if req.include_comments {
                        cell_opt(r, 7).filter(|c| !c.is_empty())
                    } else {
                        None
                    },
                    in_partition_key: cell_bool(r, 8),
                    in_sorting_key: cell_bool(r, 9),
                    codec: cell_text(r, 10),
                };
                tables[i].columns.push(detail_of(&facts));
            }
        }
        idx.clear();
    }
    Ok(SchemaModel {
        scope: req.scope,
        tables,
        relations: Vec::new(),
        inferred: false,
        truncated,
    })
}

/// DDL statements of a database (or of the listed objects), in dependency order.
pub(crate) async fn generate_ddl(core: &Core, req: DdlRequest) -> Result<Vec<DdlItem>> {
    let o = &req.options;
    let mut items: Vec<DdlItem> = Vec::new();
    let db_scope = scope_database(&req.scope)?;
    // (database, name, class) of every object to emit.
    let mut targets: Vec<(String, String, Class)> = Vec::new();
    let mut databases: Vec<String> = Vec::new();
    if req.objects.is_empty() {
        let Some(db) = &db_scope else {
            return Err(Error::Invalid(
                "choose a database (scope) or list the objects to generate DDL for".into(),
            ));
        };
        databases.push(db.clone());
        let rows = core
            .rows_db(
                None,
                &format!(
                    "SELECT name, engine, as_select FROM system.tables WHERE database = {} AND {} ORDER BY name",
                    quote_literal(db),
                    user_tables_cond()
                ),
            )
            .await?;
        for r in &rows {
            targets.push((
                db.clone(),
                cell_text(r, 0),
                Class::of_engine(&cell_text(r, 1)),
            ));
        }
    } else {
        for p in &req.objects {
            match resolve(p)? {
                Place::Database(d) => databases.push(d),
                Place::Object(d, t, kind) => {
                    let class = match kind {
                        ObjectKind::View => Class::View,
                        ObjectKind::MaterializedView => Class::MaterializedView,
                        ObjectKind::Dictionary => Class::Dictionary,
                        _ => Class::Table,
                    };
                    targets.push((d, t, class));
                }
                _ => return Err(Error::Invalid("unsupported object for DDL".into())),
            }
        }
    }
    // Without schema qualification the statements become database independent.
    let qualify = |sql: String, db: &str| -> String {
        if o.qualify_schema {
            sql
        } else {
            strip_db_prefix(&sql, db)
        }
    };
    let finish = |mut sql: String, drop: Option<String>| -> String {
        if o.if_not_exists {
            for (from, to) in [
                ("CREATE DATABASE ", "CREATE DATABASE IF NOT EXISTS "),
                ("CREATE TABLE ", "CREATE TABLE IF NOT EXISTS "),
                ("CREATE VIEW ", "CREATE VIEW IF NOT EXISTS "),
                (
                    "CREATE MATERIALIZED VIEW ",
                    "CREATE MATERIALIZED VIEW IF NOT EXISTS ",
                ),
                ("CREATE DICTIONARY ", "CREATE DICTIONARY IF NOT EXISTS "),
            ] {
                if sql.starts_with(from) {
                    sql = sql.replacen(from, to, 1);
                    break;
                }
            }
        }
        let mut out = String::new();
        if let Some(d) = drop.filter(|_| o.include_drop) {
            out.push_str(&d);
            out.push('\n');
        }
        out.push_str(sql.trim_end());
        out.push(';');
        out
    };
    for db in &databases {
        let sql = core
            .rows_db(None, &format!("SHOW CREATE DATABASE {}", quote_ident(db)))
            .await?
            .first()
            .and_then(|r| cell_opt(r, 0))
            .ok_or_else(|| Error::NotFound(format!("database `{db}`")))?;
        items.push(DdlItem {
            path: ObjectPath::root().child(ObjectKind::Database, db.clone()),
            kind: ObjectKind::Database,
            section: DdlSection::PreData,
            sql: finish(
                sql,
                Some(format!("DROP DATABASE IF EXISTS {};", quote_ident(db))),
            ),
            language: DdlLanguage::Sql,
            depends_on: Vec::new(),
        });
    }
    // Plain tables, then dictionaries (their source tables exist), then views, then
    // materialized views (their `TO` tables exist).
    let order = [
        Class::Table,
        Class::Dictionary,
        Class::View,
        Class::MaterializedView,
    ];
    for class in order {
        if matches!(class, Class::View | Class::MaterializedView) && !o.include_views {
            continue;
        }
        let mut group: Vec<&(String, String, Class)> =
            targets.iter().filter(|(_, _, c)| *c == class).collect();
        if class == Class::View {
            group = order_views(core, group).await?;
        }
        for (db, name, _) in group {
            let what = if class == Class::Dictionary {
                "DICTIONARY"
            } else {
                "TABLE"
            };
            let sql = core
                .rows_db(
                    None,
                    &format!(
                        "SHOW CREATE {what} {}.{}",
                        quote_ident(db),
                        quote_ident(name)
                    ),
                )
                .await?
                .first()
                .and_then(|r| cell_opt(r, 0))
                .ok_or_else(|| Error::NotFound(format!("object `{db}`.`{name}`")))?;
            let drop_what = match class {
                Class::Dictionary => "DICTIONARY",
                _ => "TABLE",
            };
            let drop = if o.qualify_schema {
                format!(
                    "DROP {drop_what} IF EXISTS {}.{};",
                    quote_ident(db),
                    quote_ident(name)
                )
            } else {
                format!("DROP {drop_what} IF EXISTS {};", quote_ident(name))
            };
            let path = ObjectPath::root()
                .child(ObjectKind::Database, db.clone())
                .child(ObjectKind::Folder, class.folder())
                .child(class.kind(), name.clone());
            items.push(DdlItem {
                path,
                kind: class.kind(),
                section: if matches!(class, Class::View | Class::MaterializedView) {
                    DdlSection::PostData
                } else {
                    DdlSection::PreData
                },
                sql: finish(qualify(sql, db), Some(drop)),
                language: DdlLanguage::Sql,
                depends_on: if class == Class::Table {
                    Vec::new()
                } else {
                    vec![ObjectPath::root().child(ObjectKind::Database, db.clone())]
                },
            });
        }
    }
    Ok(items)
}

/// Remove the `db.` qualifier from object names (bare, backtick or double quoted) at token
/// boundaries, outside string literals and comments. `mydb.t` and `'/clickhouse/tables/db/x'`
/// are left alone.
pub(crate) fn strip_db_prefix(sql: &str, db: &str) -> String {
    let chars: Vec<char> = sql.chars().collect();
    let is_ident = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    let candidates: Vec<Vec<char>> = [format!("{db}."), format!("`{db}`."), format!("\"{db}\".")]
        .iter()
        .map(|s| s.chars().collect())
        .collect();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\'' => {
                // String literal with backslash escapes and doubled quotes.
                out.push(c);
                i += 1;
                while i < chars.len() {
                    out.push(chars[i]);
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        i += 1;
                        out.push(chars[i]);
                    } else if chars[i] == '\'' {
                        if chars.get(i + 1) == Some(&'\'') {
                            i += 1;
                            out.push('\'');
                        } else {
                            i += 1;
                            break;
                        }
                    }
                    i += 1;
                }
                continue;
            }
            '-' if chars.get(i + 1) == Some(&'-') => {
                while i < chars.len() && chars[i] != '\n' {
                    out.push(chars[i]);
                    i += 1;
                }
                continue;
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                out.push_str("/*");
                i += 2;
                while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                    out.push(chars[i]);
                    i += 1;
                }
                if i < chars.len() {
                    out.push_str("*/");
                    i += 2;
                }
                continue;
            }
            _ => {}
        }
        let boundary = i == 0
            || !(is_ident(chars[i - 1])
                || chars[i - 1] == '.'
                || chars[i - 1] == '`'
                || chars[i - 1] == '"');
        if boundary {
            if let Some(m) = candidates.iter().find(|m| chars[i..].starts_with(m)) {
                i += m.len();
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Order views so a view comes after the views its query mentions.
async fn order_views<'a>(
    core: &Core,
    group: Vec<&'a (String, String, Class)>,
) -> Result<Vec<&'a (String, String, Class)>> {
    if group.len() < 2 {
        return Ok(group);
    }
    let mut selects: Vec<String> = Vec::new();
    for (db, name, _) in &group {
        let rows = core
            .rows_db(
                None,
                &format!(
                    "SELECT as_select FROM system.tables WHERE database = {} AND name = {}",
                    quote_literal(db),
                    quote_literal(name)
                ),
            )
            .await?;
        selects.push(rows.first().map(|r| cell_text(r, 0)).unwrap_or_default());
    }
    let mut placed: Vec<usize> = Vec::new();
    let mut remaining: Vec<usize> = (0..group.len()).collect();
    while !remaining.is_empty() {
        let before = remaining.len();
        let mut next = Vec::new();
        for &i in &remaining {
            let waits = remaining.iter().any(|&j| {
                j != i && {
                    let n = &group[j].1;
                    selects[i].contains(&format!("{}.{}", group[j].0, n))
                        || selects[i].contains(&format!("`{n}`"))
                        || selects[i]
                            .split(|c: char| !c.is_alphanumeric() && c != '_')
                            .any(|w| w == n)
                }
            });
            if waits {
                next.push(i);
            } else {
                placed.push(i);
            }
        }
        remaining = next;
        if remaining.len() == before {
            placed.append(&mut remaining);
        }
    }
    Ok(placed.into_iter().map(|i| group[i]).collect())
}

/// `ValueKind` of a column type name (re-exported for the data editor).
#[allow(dead_code)]
pub(crate) fn value_kind(type_name: &str) -> ValueKind {
    kind_of_name(type_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_resolve_with_and_without_folders() {
        let db = ObjectPath::root().child(ObjectKind::Database, "d");
        assert_eq!(resolve(&ObjectPath::root()).unwrap(), Place::Root);
        assert_eq!(resolve(&db).unwrap(), Place::Database("d".into()));
        assert_eq!(
            resolve(&db.child(ObjectKind::Folder, folder::MATERIALIZED_VIEWS)).unwrap(),
            Place::DbFolder("d".into(), Class::MaterializedView)
        );
        let t = db
            .child(ObjectKind::Folder, folder::TABLES)
            .child(ObjectKind::Table, "t");
        assert_eq!(
            resolve(&t).unwrap(),
            Place::Object("d".into(), "t".into(), ObjectKind::Table)
        );
        assert_eq!(
            resolve(&t.child(ObjectKind::Folder, folder::COLUMNS)).unwrap(),
            Place::ObjectFolder("d".into(), "t".into(), folder::COLUMNS.into())
        );
        assert_eq!(
            resolve(
                &t.child(ObjectKind::Folder, folder::COLUMNS)
                    .child(ObjectKind::Column, "c")
            )
            .unwrap(),
            Place::Column("d".into(), "t".into(), "c".into())
        );
        assert!(resolve(&db.child(ObjectKind::Folder, "nope")).is_err());
    }

    #[test]
    fn db_prefix_is_stripped_only_at_token_boundaries() {
        let sql = "CREATE MATERIALIZED VIEW db.mv TO db.dst AS SELECT x FROM `db`.src JOIN mydb.src \
                   ENGINE = ReplicatedMergeTree('/clickhouse/tables/db/x', '{replica}') COMMENT 'see db.t' -- db.y\n/* db.z */";
        let out = strip_db_prefix(sql, "db");
        assert!(
            out.contains("VIEW mv TO dst AS SELECT x FROM src JOIN mydb.src"),
            "{out}"
        );
        assert!(out.contains("'/clickhouse/tables/db/x'"));
        assert!(out.contains("'see db.t' -- db.y\n/* db.z */"));
        assert_eq!(
            strip_db_prefix("CREATE TABLE \"db\".t (a UInt8)", "db"),
            "CREATE TABLE t (a UInt8)"
        );
        assert_eq!(
            strip_db_prefix("CREATE TABLE xdb.t", "db"),
            "CREATE TABLE xdb.t"
        );
    }

    #[test]
    fn engines_map_to_classes() {
        assert_eq!(Class::of_engine("MergeTree"), Class::Table);
        assert_eq!(Class::of_engine("View"), Class::View);
        assert_eq!(
            Class::of_engine("MaterializedView"),
            Class::MaterializedView
        );
        assert_eq!(Class::of_engine("Dictionary"), Class::Dictionary);
        assert_eq!(Class::of_engine("Distributed"), Class::Table);
    }

    #[test]
    fn column_detail_marks_keys_and_generated() {
        let c = ColumnFacts {
            name: "a".into(),
            ty: "Nullable(String)".into(),
            position: 2,
            default_kind: "MATERIALIZED".into(),
            default_expr: "upper(b)".into(),
            comment: None,
            in_partition_key: false,
            in_sorting_key: true,
            codec: String::new(),
        };
        let d = detail_of(&c);
        assert!(d.nullable);
        assert_eq!(d.default, None);
        assert_eq!(
            d.extra.as_deref(),
            Some("MATERIALIZED upper(b); sorting key")
        );
    }
}
