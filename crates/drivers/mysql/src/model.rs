//! MySQL/MariaDB driver: schema model (ERD, autocomplete). Owned by work package F.
//!
//! The model of a database is read with four `information_schema` queries (tables, columns,
//! primary/unique keys, foreign keys), however many tables it has.

use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use dbstudio_core::{
    ColumnDetail, Error, ModelDetail, ObjectKind, ObjectPath, Relation, Result, SchemaModel,
    SchemaModelRequest, TableModel, folder,
};
use mysql_async::Row;
use mysql_async::prelude::Queryable;

use crate::error::map_query_error;
use crate::session::{MySqlSession, cell};
use crate::value::kind_from_data_type;

/// More tables than this are cut off (`SchemaModel::truncated`).
pub const MAX_TABLES: usize = 2000;

tokio::task_local! {
    static CATALOG_COUNTER: Arc<AtomicU32>;
}

/// Run `f` and count the catalog queries issued by `schema_model` on this task meanwhile. Used
/// by the integration tests.
pub async fn count_catalog_queries<F: Future>(f: F) -> (F::Output, u32) {
    let counter = Arc::new(AtomicU32::new(0));
    let out = CATALOG_COUNTER.scope(counter.clone(), f).await;
    (out, counter.load(Ordering::SeqCst))
}

/// Run one catalog query, counting it when a counter is installed.
pub(crate) async fn catalog_rows(
    conn: &mut mysql_async::Conn,
    sql: &str,
    params: impl Into<mysql_async::Params> + Send,
) -> Result<Vec<Row>> {
    let _ = CATALOG_COUNTER.try_with(|c| c.fetch_add(1, Ordering::SeqCst));
    conn.exec(sql, params).await.map_err(map_query_error)
}

/// The database named by a scope path (`Database` segment, folders ignored).
pub(crate) fn scope_database(path: &ObjectPath) -> Result<String> {
    let segs: Vec<_> = path
        .segments
        .iter()
        .filter(|s| s.kind != ObjectKind::Folder)
        .collect();
    match segs.as_slice() {
        [d] if d.kind == ObjectKind::Database => Ok(d.name.clone()),
        _ => Err(Error::NotFound("scope must be a database".to_string())),
    }
}

/// Name of the table/view a path points to (`Database/[Folder]/Table|View`).
pub(crate) fn table_name_of(path: &ObjectPath) -> Option<String> {
    let segs: Vec<_> = path
        .segments
        .iter()
        .filter(|s| s.kind != ObjectKind::Folder)
        .collect();
    match segs.as_slice() {
        [d, t]
            if d.kind == ObjectKind::Database
                && matches!(t.kind, ObjectKind::Table | ObjectKind::View) =>
        {
            Some(t.name.clone())
        }
        _ => None,
    }
}

/// Tree path of a table or view.
pub(crate) fn rel_path(db: &str, kind: ObjectKind, name: &str) -> ObjectPath {
    let f = if kind == ObjectKind::View {
        folder::VIEWS
    } else {
        folder::TABLES
    };
    ObjectPath::root()
        .child(ObjectKind::Database, db)
        .child(ObjectKind::Folder, f)
        .child(kind, name)
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.filter(|x| !x.is_empty())
}

/// Schema model of a database in four catalog queries.
pub(crate) async fn schema_model(
    session: &MySqlSession,
    req: SchemaModelRequest,
) -> Result<SchemaModel> {
    let db = scope_database(&req.scope)?;
    let mut conn = session.inner.acquire().await?;
    let r = build(&mut conn, &db, session.inner.is_mariadb, &req).await;
    let broken = matches!(r, Err(Error::Connection(_) | Error::Timeout));
    session.inner.release(conn, broken).await;
    r
}

struct RawFk {
    from: String,
    name: String,
    cols: Vec<String>,
    to: String,
    to_cols: Vec<String>,
    upd: Option<String>,
    del: Option<String>,
}

async fn build(
    conn: &mut mysql_async::Conn,
    db: &str,
    is_mariadb: bool,
    req: &SchemaModelRequest,
) -> Result<SchemaModel> {
    let wanted: Option<BTreeSet<String>> = if req.tables.is_empty() {
        None
    } else {
        Some(req.tables.iter().filter_map(table_name_of).collect())
    };
    let empty = |truncated| SchemaModel {
        scope: req.scope.clone(),
        tables: Vec::new(),
        relations: Vec::new(),
        inferred: false,
        truncated,
    };
    if wanted.as_ref().is_some_and(|w| w.is_empty()) {
        return Ok(empty(false));
    }

    // 1. tables and views
    let table_types = if req.include_views {
        "('BASE TABLE', 'VIEW')"
    } else {
        "('BASE TABLE')"
    };
    let rows = catalog_rows(
        conn,
        &format!(
            "SELECT TABLE_NAME, TABLE_TYPE, TABLE_COMMENT, TABLE_ROWS \
             FROM information_schema.TABLES \
             WHERE TABLE_SCHEMA = ? AND TABLE_TYPE IN {table_types} \
             ORDER BY TABLE_NAME"
        ),
        (db,),
    )
    .await?;
    let mut tables: Vec<TableModel> = Vec::new();
    let mut by_name: HashMap<String, usize> = HashMap::new();
    let mut truncated = false;
    for r in &rows {
        let name = cell(r, 0).unwrap_or_default();
        if wanted.as_ref().is_some_and(|w| !w.contains(&name)) {
            continue;
        }
        if tables.len() >= MAX_TABLES {
            truncated = true;
            break;
        }
        let is_view = cell(r, 1).as_deref() == Some("VIEW");
        let kind = if is_view {
            ObjectKind::View
        } else {
            ObjectKind::Table
        };
        by_name.insert(name.clone(), tables.len());
        tables.push(TableModel {
            path: rel_path(db, kind, &name),
            kind,
            schema: None,
            name,
            comment: if is_view || !req.include_comments {
                None
            } else {
                non_empty(cell(r, 2))
            },
            columns: Vec::new(),
            primary_key: Vec::new(),
            unique_keys: Vec::new(),
            estimated_rows: if is_view {
                None
            } else {
                cell(r, 3).and_then(|s| s.parse().ok())
            },
        });
    }
    if tables.is_empty() {
        return Ok(empty(truncated));
    }

    // 2. columns
    let rows = catalog_rows(
        conn,
        "SELECT TABLE_NAME, COLUMN_NAME, ORDINAL_POSITION, COLUMN_TYPE, DATA_TYPE, IS_NULLABLE, \
                COLUMN_DEFAULT, COLUMN_COMMENT, COLUMN_KEY, EXTRA, GENERATION_EXPRESSION \
         FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = ? \
         ORDER BY TABLE_NAME, ORDINAL_POSITION",
        (db,),
    )
    .await?;
    for r in &rows {
        let Some(&i) = cell(r, 0).and_then(|n| by_name.get(&n)) else {
            continue;
        };
        let nullable = cell(r, 5).as_deref() == Some("YES");
        let column_type = cell(r, 3).unwrap_or_default();
        let data_type = cell(r, 4).unwrap_or_default();
        let mut default = cell(r, 6);
        if is_mariadb && nullable && default.as_deref() == Some("NULL") {
            default = None;
        }
        let mut extra = cell(r, 9).unwrap_or_default();
        if let Some(g) = non_empty(cell(r, 10)) {
            extra = format!("{extra} AS ({g})").trim().to_string();
        }
        tables[i].columns.push(ColumnDetail {
            name: cell(r, 1).unwrap_or_default(),
            ordinal: cell(r, 2).and_then(|s| s.parse().ok()).unwrap_or(0),
            value_kind: kind_from_data_type(&data_type, &column_type),
            type_name: column_type,
            nullable,
            default,
            comment: if req.include_comments {
                non_empty(cell(r, 7))
            } else {
                None
            },
            primary_key: cell(r, 8).as_deref() == Some("PRI"),
            extra: non_empty(Some(extra)),
        });
    }

    // 3. primary and unique keys (key order, which `COLUMN_KEY` does not give)
    let rows = catalog_rows(
        conn,
        "SELECT TABLE_NAME, INDEX_NAME, NON_UNIQUE, SEQ_IN_INDEX, COLUMN_NAME \
         FROM information_schema.STATISTICS \
         WHERE TABLE_SCHEMA = ? AND (INDEX_NAME = 'PRIMARY' OR NON_UNIQUE = 0) \
         ORDER BY TABLE_NAME, INDEX_NAME, SEQ_IN_INDEX",
        (db,),
    )
    .await?;
    // (table, index) -> columns; `None` marks an index with an expression part.
    let mut keys: Vec<(usize, String, Option<Vec<String>>)> = Vec::new();
    for r in &rows {
        let Some(&i) = cell(r, 0).and_then(|n| by_name.get(&n)) else {
            continue;
        };
        let index = cell(r, 1).unwrap_or_default();
        if keys
            .last()
            .is_none_or(|(ti, ix, _)| *ti != i || *ix != index)
        {
            keys.push((i, index, Some(Vec::new())));
        }
        if let Some((_, _, cols)) = keys.last_mut() {
            match (cell(r, 4), cols.as_mut()) {
                (Some(c), Some(v)) => v.push(c),
                _ => *cols = None,
            }
        }
    }
    for (i, index, cols) in keys {
        let Some(cols) = cols.filter(|c| !c.is_empty()) else {
            continue;
        };
        if index == "PRIMARY" {
            tables[i].primary_key = cols;
        } else if !tables[i].unique_keys.contains(&cols) {
            tables[i].unique_keys.push(cols);
        }
    }

    // 4. foreign keys
    let rows = catalog_rows(
        conn,
        "SELECT kcu.TABLE_NAME, kcu.CONSTRAINT_NAME, kcu.COLUMN_NAME, kcu.REFERENCED_TABLE_SCHEMA, \
                kcu.REFERENCED_TABLE_NAME, kcu.REFERENCED_COLUMN_NAME, rc.UPDATE_RULE, rc.DELETE_RULE \
         FROM information_schema.KEY_COLUMN_USAGE kcu \
         JOIN information_schema.REFERENTIAL_CONSTRAINTS rc \
           ON rc.CONSTRAINT_SCHEMA = kcu.CONSTRAINT_SCHEMA AND rc.CONSTRAINT_NAME = kcu.CONSTRAINT_NAME \
          AND rc.TABLE_NAME = kcu.TABLE_NAME \
         WHERE kcu.TABLE_SCHEMA = ? AND kcu.REFERENCED_TABLE_NAME IS NOT NULL \
         ORDER BY kcu.TABLE_NAME, kcu.CONSTRAINT_NAME, kcu.ORDINAL_POSITION",
        (db,),
    )
    .await?;
    let mut fks: Vec<RawFk> = Vec::new();
    for r in &rows {
        let from = cell(r, 0).unwrap_or_default();
        let name = cell(r, 1).unwrap_or_default();
        let ref_db = cell(r, 3).unwrap_or_default();
        if !by_name.contains_key(&from) || !ref_db.eq_ignore_ascii_case(db) {
            continue;
        }
        if fks.last().is_none_or(|f| f.from != from || f.name != name) {
            fks.push(RawFk {
                from,
                name,
                cols: Vec::new(),
                to: cell(r, 4).unwrap_or_default(),
                to_cols: Vec::new(),
                upd: cell(r, 6),
                del: cell(r, 7),
            });
        }
        if let Some(f) = fks.last_mut() {
            f.cols.push(cell(r, 2).unwrap_or_default());
            f.to_cols.push(cell(r, 5).unwrap_or_default());
        }
    }

    let mut relations = Vec::new();
    for fk in fks {
        let (Some(&fi), Some(&ti)) = (by_name.get(&fk.from), by_name.get(&fk.to)) else {
            continue;
        };
        let from = &tables[fi];
        let nullable = |name: &String| {
            from.columns
                .iter()
                .find(|c| &c.name == name)
                .is_none_or(|c| c.nullable)
        };
        let cols: BTreeSet<&String> = fk.cols.iter().collect();
        let same = |other: &Vec<String>| {
            !other.is_empty() && other.iter().collect::<BTreeSet<_>>() == cols
        };
        relations.push(Relation {
            name: fk.name,
            from_table: from.path.clone(),
            from_columns: fk.cols.clone(),
            to_table: tables[ti].path.clone(),
            to_columns: fk.to_cols,
            on_update: fk.upd,
            on_delete: fk.del,
            from_optional: fk.cols.iter().any(nullable),
            one_to_one: same(&from.primary_key) || from.unique_keys.iter().any(same),
            inferred: false,
        });
    }

    if req.detail == ModelDetail::Keys {
        let mut fk_cols: Vec<BTreeSet<String>> = vec![BTreeSet::new(); tables.len()];
        for r in &relations {
            if let Some(&i) = table_name_of(&r.from_table).and_then(|n| by_name.get(&n)) {
                fk_cols[i].extend(r.from_columns.iter().cloned());
            }
        }
        for (i, t) in tables.iter_mut().enumerate() {
            let pk = t.primary_key.clone();
            let uniq: BTreeSet<String> = t.unique_keys.iter().flatten().cloned().collect();
            t.columns.retain(|c| {
                pk.contains(&c.name) || uniq.contains(&c.name) || fk_cols[i].contains(&c.name)
            });
        }
    }

    Ok(SchemaModel {
        scope: req.scope.clone(),
        tables,
        relations,
        inferred: false,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_and_table_paths() {
        let db = ObjectPath::root().child(ObjectKind::Database, "d");
        assert_eq!(scope_database(&db).unwrap(), "d");
        assert_eq!(
            scope_database(&db.child(ObjectKind::Folder, "tables")).unwrap(),
            "d"
        );
        assert!(scope_database(&ObjectPath::root()).is_err());
        let t = rel_path("d", ObjectKind::Table, "t");
        assert_eq!(table_name_of(&t).as_deref(), Some("t"));
        assert_eq!(table_name_of(&db), None);
        let v = rel_path("d", ObjectKind::View, "v");
        assert_eq!(v.segments[1].name, "views");
    }
}
