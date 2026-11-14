//! Lazy object tree (`list_children`).
//!
//! Tree shape (folder keys come from [`dbstudio_core::folder`]):
//!
//! ```text
//! root ── Database ── Folder schemas ── Schema ── Folder tables ── Table ── Folder columns ── Column
//!      │                                        │                          ├─ Folder indexes ── Index
//!      │                                        │                          └─ Folder triggers ── Trigger
//!      │                                        ├─ Folder views / materializedViews ── View ── Folder columns
//!      │                                        └─ Folder functions / procedures / sequences
//!      └─ Folder roles ── Role
//! ```

use dbstudio_core::{Error, ObjectKind, ObjectNode, ObjectPath, Result, folder};
use tokio_postgres::Client;

use crate::conn::Shared;
use crate::error::map_error;
use crate::flavor::Flavor;

fn node(
    parent: &ObjectPath,
    kind: ObjectKind,
    name: String,
    has_children: bool,
    detail: Option<String>,
) -> ObjectNode {
    ObjectNode {
        path: parent.child(kind, name.clone()),
        kind,
        name,
        has_children,
        detail,
    }
}

fn folder_for(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::View => folder::VIEWS,
        ObjectKind::MaterializedView => folder::MATERIALIZED_VIEWS,
        ObjectKind::Function => folder::FUNCTIONS,
        ObjectKind::Procedure => folder::PROCEDURES,
        ObjectKind::Sequence => folder::SEQUENCES,
        _ => folder::TABLES,
    }
}

/// Full tree path of a schema.
pub fn schema_path(database: &str, schema: &str) -> ObjectPath {
    ObjectPath::root()
        .child(ObjectKind::Database, database)
        .child(ObjectKind::Folder, folder::SCHEMAS)
        .child(ObjectKind::Schema, schema)
}

/// Full tree path of an object living directly in a schema (table, view, function...).
pub fn object_path(database: &str, schema: &str, kind: ObjectKind, name: &str) -> ObjectPath {
    schema_path(database, schema)
        .child(ObjectKind::Folder, folder_for(kind))
        .child(kind, name)
}

pub async fn list_children(shared: &Shared, parent: &ObjectPath) -> Result<Vec<ObjectNode>> {
    let segs: Vec<(ObjectKind, &str)> = parent
        .segments
        .iter()
        .map(|s| (s.kind, s.name.as_str()))
        .collect();
    use ObjectKind as K;
    match segs.as_slice() {
        [] => root(shared, parent).await,
        [(K::Folder, folder::ROLES)] => roles(shared, parent).await,
        [(K::Database, _)] => Ok(vec![node(
            parent,
            K::Folder,
            folder::SCHEMAS.into(),
            true,
            None,
        )]),
        [(K::Database, db), (K::Folder, folder::SCHEMAS)] => {
            let conn = connect(shared, db).await?;
            schemas(&conn, parent).await
        }
        [
            (K::Database, db),
            (K::Folder, folder::SCHEMAS),
            (K::Schema, schema),
        ] => {
            let conn = connect(shared, db).await?;
            schema_folders(&conn, parent, schema).await
        }
        [
            (K::Database, db),
            (K::Folder, folder::SCHEMAS),
            (K::Schema, schema),
            (K::Folder, key),
        ] => {
            let conn = connect(shared, db).await?;
            schema_objects(&conn, parent, db, schema, key).await
        }
        [
            (K::Database, db),
            (K::Folder, folder::SCHEMAS),
            (K::Schema, schema),
            (K::Folder, _),
            (kind @ (K::Table | K::View | K::MaterializedView), name),
        ] => {
            let conn = connect(shared, db).await?;
            relation_folders(&conn, parent, schema, name, *kind, shared.flavor()).await
        }
        [
            (K::Database, db),
            (K::Folder, folder::SCHEMAS),
            (K::Schema, schema),
            (K::Folder, _),
            (K::Table | K::View | K::MaterializedView, name),
            (K::Folder, key),
        ] => {
            let conn = connect(shared, db).await?;
            relation_items(&conn, parent, schema, name, key, shared.flavor()).await
        }
        // Leaves.
        [
            ..,
            (
                K::Column
                | K::Index
                | K::Trigger
                | K::Role
                | K::Function
                | K::Procedure
                | K::Sequence,
                _,
            ),
        ] => Ok(Vec::new()),
        _ => Err(Error::NotFound("unknown object tree path".to_string())),
    }
}

struct BrowseConn {
    conn: crate::conn::PooledConn,
}

impl std::ops::Deref for BrowseConn {
    type Target = Client;
    fn deref(&self) -> &Client {
        &self.conn.client
    }
}

impl Drop for BrowseConn {
    fn drop(&mut self) {
        // Catalog queries leave the connection clean.
        self.conn.mark_reusable();
    }
}

async fn connect(shared: &Shared, db: &str) -> Result<BrowseConn> {
    Ok(BrowseConn {
        conn: shared.acquire_catalog(db).await?,
    })
}

async fn root(shared: &Shared, parent: &ObjectPath) -> Result<Vec<ObjectNode>> {
    let conn = connect(shared, &shared.login_db).await?;
    let rows = conn
        .query(
            "SELECT datname::text FROM pg_database \
             WHERE NOT datistemplate AND datallowconn \
               AND has_database_privilege(datname, 'CONNECT') \
             ORDER BY datname",
            &[],
        )
        .await
        .map_err(map_error)?;
    let mut out: Vec<ObjectNode> = rows
        .iter()
        .map(|r| node(parent, ObjectKind::Database, r.get(0), true, None))
        .collect();
    out.push(node(
        parent,
        ObjectKind::Folder,
        folder::ROLES.into(),
        true,
        None,
    ));
    Ok(out)
}

async fn roles(shared: &Shared, parent: &ObjectPath) -> Result<Vec<ObjectNode>> {
    let conn = connect(shared, &shared.login_db).await?;
    let rows = conn
        .query(
            "SELECT rolname::text, rolsuper, rolcanlogin, (left(rolname::text, 3) = 'pg_') \
             FROM pg_roles ORDER BY (left(rolname::text, 3) = 'pg_'), rolname",
            &[],
        )
        .await
        .map_err(map_error)?;
    Ok(rows
        .iter()
        .map(|r| {
            let mut tags: Vec<&str> = Vec::new();
            if r.get::<_, bool>(3) {
                tags.push("system");
            }
            if r.get::<_, bool>(1) {
                tags.push("superuser");
            }
            tags.push(if r.get::<_, bool>(2) {
                "login"
            } else {
                "no login"
            });
            node(
                parent,
                ObjectKind::Role,
                r.get(0),
                false,
                Some(tags.join(", ")),
            )
        })
        .collect())
}

async fn schemas(client: &Client, parent: &ObjectPath) -> Result<Vec<ObjectNode>> {
    let rows = client
        .query(
            "SELECT nspname::text, \
                    (nspname IN ('pg_catalog', 'information_schema', 'crdb_internal', 'pg_extension') \
                     OR (left(nspname::text, 8) = 'pg_toast')) AS is_system \
             FROM pg_namespace \
             WHERE NOT (left(nspname::text, 8) = 'pg_temp_') AND NOT (left(nspname::text, 14) = 'pg_toast_temp_') \
             ORDER BY is_system, nspname",
            &[],
        )
        .await
        .map_err(map_error)?;
    Ok(rows
        .iter()
        .map(|r| {
            let system: bool = r.get(1);
            node(
                parent,
                ObjectKind::Schema,
                r.get(0),
                true,
                system.then(|| "system".to_string()),
            )
        })
        .collect())
}

async fn schema_folders(
    client: &Client,
    parent: &ObjectPath,
    schema: &str,
) -> Result<Vec<ObjectNode>> {
    let rel = client
        .query_opt(
            "SELECT count(c.oid) FILTER (WHERE c.relkind IN ('r', 'p', 'f')), \
                    count(c.oid) FILTER (WHERE c.relkind = 'v'), \
                    count(c.oid) FILTER (WHERE c.relkind = 'm'), \
                    count(c.oid) FILTER (WHERE c.relkind = 'S' AND NOT EXISTS ( \
                        SELECT 1 FROM pg_depend d \
                        WHERE d.objid = c.oid AND d.classid = 'pg_class'::regclass \
                          AND d.deptype = 'i')) \
             FROM pg_namespace n LEFT JOIN pg_class c ON c.relnamespace = n.oid \
             WHERE n.nspname = $1 GROUP BY n.oid",
            &[&schema],
        )
        .await
        .map_err(map_error)?
        .ok_or_else(|| Error::NotFound(format!("schema {schema} not found")))?;
    let procs = client
        .query_one(
            "SELECT count(*) FILTER (WHERE p.prokind IN ('f', 'w')), \
                    count(*) FILTER (WHERE p.prokind = 'p') \
             FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
             WHERE n.nspname = $1",
            &[&schema],
        )
        .await
        .map_err(map_error)?;
    let n = |row: &tokio_postgres::Row, i: usize| row.get::<_, i64>(i);
    let mut out = vec![node(
        parent,
        ObjectKind::Folder,
        folder::TABLES.into(),
        n(&rel, 0) > 0,
        None,
    )];
    for (key, count) in [
        (folder::VIEWS, n(&rel, 1)),
        (folder::MATERIALIZED_VIEWS, n(&rel, 2)),
        (folder::FUNCTIONS, n(&procs, 0)),
        (folder::PROCEDURES, n(&procs, 1)),
        (folder::SEQUENCES, n(&rel, 3)),
    ] {
        if count > 0 {
            out.push(node(parent, ObjectKind::Folder, key.into(), true, None));
        }
    }
    Ok(out)
}

async fn schema_objects(
    client: &Client,
    parent: &ObjectPath,
    _db: &str,
    schema: &str,
    key: &str,
) -> Result<Vec<ObjectNode>> {
    match key {
        folder::TABLES => {
            let rows = client
                .query(
                    "SELECT c.relname::text, c.relkind::text, COALESCE(c.relispartition, false) \
                     FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                     WHERE n.nspname = $1 AND c.relkind IN ('r', 'p', 'f') \
                     ORDER BY c.relname",
                    &[&schema],
                )
                .await
                .map_err(map_error)?;
            Ok(rows
                .iter()
                .map(|r| {
                    let relkind: String = r.get(1);
                    let partition: bool = r.get(2);
                    let detail = match relkind.as_str() {
                        "p" => Some("partitioned"),
                        "f" => Some("foreign"),
                        _ if partition => Some("partition"),
                        _ => None,
                    };
                    node(
                        parent,
                        ObjectKind::Table,
                        r.get(0),
                        true,
                        detail.map(String::from),
                    )
                })
                .collect())
        }
        folder::VIEWS | folder::MATERIALIZED_VIEWS => {
            let (relkind, kind) = if key == folder::VIEWS {
                ("v", ObjectKind::View)
            } else {
                ("m", ObjectKind::MaterializedView)
            };
            let rows = client
                .query(
                    "SELECT c.relname::text, COALESCE(c.relispopulated, true) \
                     FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                     WHERE n.nspname = $1 AND c.relkind = $2::text::\"char\" \
                     ORDER BY c.relname",
                    &[&schema, &relkind],
                )
                .await
                .map_err(map_error)?;
            Ok(rows
                .iter()
                .map(|r| {
                    let populated: bool = r.get(1);
                    node(
                        parent,
                        kind,
                        r.get(0),
                        true,
                        (!populated).then(|| "not populated".to_string()),
                    )
                })
                .collect())
        }
        folder::FUNCTIONS | folder::PROCEDURES => {
            let (kinds, kind) = if key == folder::FUNCTIONS {
                ("p.prokind IN ('f', 'w')", ObjectKind::Function)
            } else {
                ("p.prokind = 'p'", ObjectKind::Procedure)
            };
            let sql = format!(
                "SELECT p.proname::text || '(' || pg_get_function_identity_arguments(p.oid) || ')', \
                        pg_get_function_result(p.oid) \
                 FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
                 WHERE n.nspname = $1 AND {kinds} \
                 ORDER BY p.proname, p.oid"
            );
            let rows = client.query(&sql, &[&schema]).await.map_err(map_error)?;
            Ok(rows
                .iter()
                .map(|r| node(parent, kind, r.get(0), false, r.get::<_, Option<String>>(1)))
                .collect())
        }
        folder::SEQUENCES => {
            let rows = client
                .query(
                    "SELECT c.relname::text, format_type(s.seqtypid, NULL) \
                     FROM pg_class c \
                     JOIN pg_namespace n ON n.oid = c.relnamespace \
                     JOIN pg_sequence s ON s.seqrelid = c.oid \
                     WHERE n.nspname = $1 AND c.relkind = 'S' AND NOT EXISTS ( \
                         SELECT 1 FROM pg_depend d \
                         WHERE d.objid = c.oid AND d.classid = 'pg_class'::regclass \
                           AND d.deptype = 'i') \
                     ORDER BY c.relname",
                    &[&schema],
                )
                .await
                .map_err(map_error)?;
            Ok(rows
                .iter()
                .map(|r| node(parent, ObjectKind::Sequence, r.get(0), false, r.get(1)))
                .collect())
        }
        _ => Err(Error::NotFound(format!("unknown folder {key}"))),
    }
}

async fn relation_folders(
    client: &Client,
    parent: &ObjectPath,
    schema: &str,
    name: &str,
    kind: ObjectKind,
    flavor: Flavor,
) -> Result<Vec<ObjectNode>> {
    let hidden = crate::details::visible_filter(flavor, "a");
    let row = client
        .query_opt(
            &format!("SELECT (SELECT count(*) FROM pg_attribute a \
                     WHERE a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped{hidden}), \
                    (SELECT count(*) FROM pg_index i WHERE i.indrelid = c.oid), \
                    (SELECT count(*) FROM pg_trigger t WHERE t.tgrelid = c.oid AND NOT t.tgisinternal) \
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relname = $2 AND c.relkind IN ('r', 'p', 'f', 'v', 'm')"),
            &[&schema, &name],
        )
        .await
        .map_err(map_error)?
        .ok_or_else(|| Error::NotFound(format!("{schema}.{name} not found")))?;
    let mut out = vec![node(
        parent,
        ObjectKind::Folder,
        folder::COLUMNS.into(),
        row.get::<_, i64>(0) > 0,
        None,
    )];
    if kind == ObjectKind::Table {
        out.push(node(
            parent,
            ObjectKind::Folder,
            folder::INDEXES.into(),
            row.get::<_, i64>(1) > 0,
            None,
        ));
        // CockroachDB only has triggers from 24.3.
        if flavor.capabilities().triggers {
            out.push(node(
                parent,
                ObjectKind::Folder,
                folder::TRIGGERS.into(),
                row.get::<_, i64>(2) > 0,
                None,
            ));
        }
    }
    Ok(out)
}

fn trigger_detail(tgtype: i32, enabled: &str) -> String {
    let timing = if tgtype & 64 != 0 {
        "INSTEAD OF"
    } else if tgtype & 2 != 0 {
        "BEFORE"
    } else {
        "AFTER"
    };
    let mut events = Vec::new();
    if tgtype & 4 != 0 {
        events.push("INSERT");
    }
    if tgtype & 8 != 0 {
        events.push("DELETE");
    }
    if tgtype & 16 != 0 {
        events.push("UPDATE");
    }
    if tgtype & 32 != 0 {
        events.push("TRUNCATE");
    }
    let level = if tgtype & 1 != 0 { "row" } else { "statement" };
    let mut s = format!("{timing} {}, {level}", events.join(" OR "));
    if enabled == "D" {
        s.push_str(", disabled");
    }
    s
}

async fn relation_items(
    client: &Client,
    parent: &ObjectPath,
    schema: &str,
    name: &str,
    key: &str,
    flavor: Flavor,
) -> Result<Vec<ObjectNode>> {
    match key {
        folder::COLUMNS => {
            let hidden = crate::details::visible_filter(flavor, "a");
            let rows = client
                .query(
                    &format!("SELECT a.attname::text, format_type(a.atttypid, a.atttypmod) \
                     FROM pg_attribute a \
                     JOIN pg_class c ON c.oid = a.attrelid \
                     JOIN pg_namespace n ON n.oid = c.relnamespace \
                     WHERE n.nspname = $1 AND c.relname = $2 AND a.attnum > 0 AND NOT a.attisdropped{hidden} \
                     ORDER BY a.attnum"),
                    &[&schema, &name],
                )
                .await
                .map_err(map_error)?;
            Ok(rows
                .iter()
                .map(|r| node(parent, ObjectKind::Column, r.get(0), false, r.get(1)))
                .collect())
        }
        folder::INDEXES => {
            let rows = client
                .query(
                    "SELECT ic.relname::text, i.indisunique, i.indisprimary, \
                            substring(pg_get_indexdef(i.indexrelid) from ' USING (.*)$') \
                     FROM pg_index i \
                     JOIN pg_class c ON c.oid = i.indrelid \
                     JOIN pg_namespace n ON n.oid = c.relnamespace \
                     JOIN pg_class ic ON ic.oid = i.indexrelid \
                     WHERE n.nspname = $1 AND c.relname = $2 \
                     ORDER BY i.indisprimary DESC, ic.relname",
                    &[&schema, &name],
                )
                .await
                .map_err(map_error)?;
            Ok(rows
                .iter()
                .map(|r| {
                    let mut parts: Vec<String> = Vec::new();
                    if r.get::<_, bool>(2) {
                        parts.push("PRIMARY KEY".into());
                    } else if r.get::<_, bool>(1) {
                        parts.push("UNIQUE".into());
                    }
                    if let Some(using) = r.get::<_, Option<String>>(3) {
                        parts.push(using);
                    }
                    node(
                        parent,
                        ObjectKind::Index,
                        r.get(0),
                        false,
                        Some(parts.join(", ")),
                    )
                })
                .collect())
        }
        folder::TRIGGERS => {
            let rows = client
                .query(
                    "SELECT t.tgname::text, t.tgtype::int4, t.tgenabled::text \
                     FROM pg_trigger t \
                     JOIN pg_class c ON c.oid = t.tgrelid \
                     JOIN pg_namespace n ON n.oid = c.relnamespace \
                     WHERE n.nspname = $1 AND c.relname = $2 AND NOT t.tgisinternal \
                     ORDER BY t.tgname",
                    &[&schema, &name],
                )
                .await
                .map_err(map_error)?;
            Ok(rows
                .iter()
                .map(|r| {
                    node(
                        parent,
                        ObjectKind::Trigger,
                        r.get(0),
                        false,
                        Some(trigger_detail(r.get(1), &r.get::<_, String>(2))),
                    )
                })
                .collect())
        }
        _ => Err(Error::NotFound(format!("unknown folder {key}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_paths_include_folders() {
        let p = object_path("d", "s", ObjectKind::Table, "t");
        let kinds: Vec<_> = p
            .segments
            .iter()
            .map(|s| (s.kind, s.name.as_str()))
            .collect();
        assert_eq!(
            kinds,
            vec![
                (ObjectKind::Database, "d"),
                (ObjectKind::Folder, "schemas"),
                (ObjectKind::Schema, "s"),
                (ObjectKind::Folder, "tables"),
                (ObjectKind::Table, "t"),
            ]
        );
        let m = object_path("d", "s", ObjectKind::MaterializedView, "m");
        assert_eq!(m.segments[3].name, "materializedViews");
    }

    #[test]
    fn trigger_descriptions() {
        // BEFORE (2) + row (1) + INSERT (4) + UPDATE (16)
        assert_eq!(
            trigger_detail(2 | 1 | 4 | 16, "O"),
            "BEFORE INSERT OR UPDATE, row"
        );
        assert_eq!(trigger_detail(1 | 8, "D"), "AFTER DELETE, row, disabled");
        assert_eq!(trigger_detail(64 | 1 | 4, "O"), "INSTEAD OF INSERT, row");
    }
}
