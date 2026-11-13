//! Lazy object tree (`list_children`) and server-side object search.
//!
//! ```text
//! root ── Schema ── Folder tables ── Table ── Folder columns ── Column
//!      │        │                         ├─ Folder indexes ── Index
//!      │        │                         └─ Folder triggers ── Trigger
//!      │        ├─ Folder views / materializedViews ── View ── Folder columns
//!      │        ├─ Folder sequences / synonyms / functions / procedures / packages / types
//!      │        └─ Folder triggers ── Trigger
//!      └─ Folder roles ── Role      (only when `DBA_ROLES` is readable)
//! ```
//!
//! The first schema is the connected user. Oracle-maintained schemas (`SYS`, `SYSTEM`, ...) are
//! hidden unless the connection option `show_all_schemas` is `true`. Names keep their exact case.

use std::sync::Arc;

use dbstudio_core::{
    Error, ObjectKind, ObjectNode, ObjectPath, ObjectSearchRequest, Result, folder,
};
use oracle::Connection;

use crate::catalog::{self, cell, rows};
use crate::runtime::Shared;
use crate::sql::non_folder_segments;

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

/// Folders of a schema in display order.
const SCHEMA_FOLDERS: [&str; 10] = [
    folder::TABLES,
    folder::VIEWS,
    folder::MATERIALIZED_VIEWS,
    folder::SEQUENCES,
    folder::SYNONYMS,
    folder::FUNCTIONS,
    folder::PROCEDURES,
    folder::PACKAGES,
    folder::TYPES,
    folder::TRIGGERS,
];

/// Children of `parent`.
pub async fn list_children(shared: &Arc<Shared>, parent: &ObjectPath) -> Result<Vec<ObjectNode>> {
    use ObjectKind as K;
    let parent = parent.clone();
    let show_all = shared.target.show_all_schemas;
    shared
        .with_conn(move |conn| {
            let segs: Vec<(ObjectKind, &str)> = parent
                .segments
                .iter()
                .map(|s| (s.kind, s.name.as_str()))
                .collect();
            match segs.as_slice() {
                [] => root(conn, &parent, show_all),
                [(K::Folder, folder::ROLES)] => roles(conn, &parent),
                [(K::Schema, schema)] => Ok(schema_folders(&parent, schema)),
                [(K::Schema, schema), (K::Folder, key)] => {
                    schema_objects(conn, &parent, schema, key)
                }
                [
                    (K::Schema, schema),
                    (K::Folder, _),
                    (kind @ (K::Table | K::View | K::MaterializedView), name),
                ] => Ok(relation_folders(&parent, schema, name, *kind)),
                [
                    (K::Schema, schema),
                    (K::Folder, _),
                    (K::Table | K::View | K::MaterializedView, name),
                    (K::Folder, key),
                ] => relation_items(conn, &parent, schema, name, key),
                // Leaves and unknown shapes.
                _ => match segs.last() {
                    Some((K::Column | K::Index | K::Trigger | K::Role, _))
                    | Some((
                        K::Sequence
                        | K::Synonym
                        | K::Function
                        | K::Procedure
                        | K::Package
                        | K::Type,
                        _,
                    )) => Ok(Vec::new()),
                    _ => Err(Error::NotFound(format!(
                        "no such node in the tree: {}",
                        describe(&parent)
                    ))),
                },
            }
        })
        .await
}

fn describe(path: &ObjectPath) -> String {
    path.segments
        .iter()
        .map(|s| s.name.as_str())
        .collect::<Vec<_>>()
        .join(" / ")
}

fn root(conn: &Connection, parent: &ObjectPath, show_all: bool) -> Result<Vec<ObjectNode>> {
    let types = "'TABLE','VIEW','MATERIALIZED VIEW','SEQUENCE','SYNONYM','FUNCTION','PROCEDURE','PACKAGE','TYPE','TRIGGER'";
    let build = |maintained_filter: &str| {
        format!(
            "SELECT u.username, CASE WHEN u.username = SYS_CONTEXT('USERENV','SESSION_USER') \
                                   THEN 1 ELSE 0 END \
             FROM all_users u \
             WHERE {maintained_filter} \
               AND (u.username = SYS_CONTEXT('USERENV','SESSION_USER') OR EXISTS \
                   (SELECT 1 FROM all_objects o WHERE o.owner = u.username \
                    AND o.object_type IN ({types}))) \
             ORDER BY 2 DESC, u.username"
        )
    };
    let filter = if show_all {
        "1 = 1"
    } else {
        "u.oracle_maintained = 'N'"
    };
    let list = match rows(conn, &build(filter), &[]) {
        Ok(l) => l,
        // Before 12.1.0.2 there is no ORACLE_MAINTAINED column.
        Err(_) if !show_all => rows(
            conn,
            &build(
                "u.username NOT IN ('SYS','SYSTEM','OUTLN','DBSNMP','XDB','CTXSYS','MDSYS','ORDSYS','WMSYS','APPQOSSYS','AUDSYS')",
            ),
            &[],
        )?,
        Err(e) => return Err(e),
    };
    let mut out: Vec<ObjectNode> = list
        .iter()
        .map(|r| {
            let current = cell(r, 1) == "1";
            node(
                parent,
                ObjectKind::Schema,
                cell(r, 0),
                true,
                current.then(|| "current user".to_string()),
            )
        })
        .collect();
    // Roles are only listed when the dictionary view is readable.
    if rows(conn, "SELECT role FROM dba_roles WHERE ROWNUM = 1", &[]).is_ok() {
        out.push(node(
            parent,
            ObjectKind::Folder,
            folder::ROLES.to_string(),
            true,
            None,
        ));
    }
    Ok(out)
}

fn roles(conn: &Connection, parent: &ObjectPath) -> Result<Vec<ObjectNode>> {
    let list = rows(conn, "SELECT role FROM dba_roles ORDER BY role", &[])?;
    Ok(list
        .iter()
        .map(|r| node(parent, ObjectKind::Role, cell(r, 0), false, None))
        .collect())
}

fn schema_folders(parent: &ObjectPath, _schema: &str) -> Vec<ObjectNode> {
    SCHEMA_FOLDERS
        .iter()
        .map(|f| node(parent, ObjectKind::Folder, (*f).to_string(), true, None))
        .collect()
}

fn object_type_of(key: &str) -> Option<(ObjectKind, &'static str)> {
    Some(match key {
        folder::FUNCTIONS => (ObjectKind::Function, "FUNCTION"),
        folder::PROCEDURES => (ObjectKind::Procedure, "PROCEDURE"),
        folder::PACKAGES => (ObjectKind::Package, "PACKAGE"),
        folder::TYPES => (ObjectKind::Type, "TYPE"),
        _ => return None,
    })
}

fn schema_objects(
    conn: &Connection,
    parent: &ObjectPath,
    schema: &str,
    key: &str,
) -> Result<Vec<ObjectNode>> {
    match key {
        folder::TABLES => {
            let list = rows(
                conn,
                "SELECT t.table_name, t.num_rows, t.temporary, t.partitioned FROM all_tables t \
                 WHERE t.owner = :1 AND t.secondary = 'N' AND t.nested = 'NO' \
                   AND (t.iot_type IS NULL OR t.iot_type = 'IOT') \
                   AND t.table_name NOT LIKE 'BIN$%' \
                   AND NOT EXISTS (SELECT 1 FROM all_mviews m \
                                   WHERE m.owner = t.owner AND m.mview_name = t.table_name) \
                 ORDER BY t.table_name",
                &[&schema],
            )?;
            Ok(list
                .iter()
                .map(|r| {
                    let mut notes: Vec<String> = Vec::new();
                    if let Some(n) = r[1].as_deref().filter(|n| !n.is_empty()) {
                        notes.push(format!("~{n} rows"));
                    }
                    if cell(r, 2) == "Y" {
                        notes.push("temporary".to_string());
                    }
                    if cell(r, 3) == "YES" {
                        notes.push("partitioned".to_string());
                    }
                    node(
                        parent,
                        ObjectKind::Table,
                        cell(r, 0),
                        true,
                        (!notes.is_empty()).then(|| notes.join(", ")),
                    )
                })
                .collect())
        }
        folder::VIEWS => {
            let list = rows(
                conn,
                "SELECT view_name FROM all_views WHERE owner = :1 ORDER BY view_name",
                &[&schema],
            )?;
            Ok(list
                .iter()
                .map(|r| node(parent, ObjectKind::View, cell(r, 0), true, None))
                .collect())
        }
        folder::MATERIALIZED_VIEWS => {
            let list = rows(
                conn,
                "SELECT mview_name FROM all_mviews WHERE owner = :1 ORDER BY mview_name",
                &[&schema],
            )?;
            Ok(list
                .iter()
                .map(|r| node(parent, ObjectKind::MaterializedView, cell(r, 0), true, None))
                .collect())
        }
        folder::SEQUENCES => {
            let list = rows(
                conn,
                "SELECT sequence_name FROM all_sequences WHERE owner = :1 \
                 AND sequence_name NOT LIKE 'ISEQ$$\\_%' ESCAPE '\\' ORDER BY sequence_name",
                &[&schema],
            )?;
            Ok(list
                .iter()
                .map(|r| node(parent, ObjectKind::Sequence, cell(r, 0), false, None))
                .collect())
        }
        folder::SYNONYMS => {
            let list = rows(
                conn,
                "SELECT synonym_name, table_owner, table_name, db_link FROM all_synonyms \
                 WHERE owner = :1 ORDER BY synonym_name",
                &[&schema],
            )?;
            Ok(list
                .iter()
                .map(|r| {
                    let mut target = match (r[1].as_deref(), cell(r, 2)) {
                        (Some(o), n) if !o.is_empty() => format!("{o}.{n}"),
                        (_, n) => n,
                    };
                    if let Some(link) = r[3].as_deref().filter(|l| !l.is_empty()) {
                        target.push('@');
                        target.push_str(link);
                    }
                    node(
                        parent,
                        ObjectKind::Synonym,
                        cell(r, 0),
                        false,
                        Some(format!("-> {target}")),
                    )
                })
                .collect())
        }
        folder::TRIGGERS => {
            let list = rows(
                conn,
                "SELECT trigger_name, trigger_type, triggering_event, status FROM all_triggers \
                 WHERE owner = :1 ORDER BY trigger_name",
                &[&schema],
            )?;
            Ok(list
                .iter()
                .map(|r| {
                    node(
                        parent,
                        ObjectKind::Trigger,
                        cell(r, 0),
                        false,
                        Some(trigger_detail(&cell(r, 1), &cell(r, 2), &cell(r, 3))),
                    )
                })
                .collect())
        }
        other => {
            let (kind, ty) = object_type_of(other)
                .ok_or_else(|| Error::NotFound(format!("unknown folder \"{other}\"")))?;
            let list = rows(
                conn,
                "SELECT object_name, status FROM all_objects \
                 WHERE owner = :1 AND object_type = :2 AND object_name NOT LIKE 'BIN$%' \
                   AND generated = 'N' ORDER BY object_name",
                &[&schema, &ty],
            )?;
            Ok(list
                .iter()
                .map(|r| {
                    node(
                        parent,
                        kind,
                        cell(r, 0),
                        false,
                        (cell(r, 1) != "VALID").then(|| "INVALID".to_string()),
                    )
                })
                .collect())
        }
    }
}

/// `BEFORE EACH ROW INSERT OR UPDATE, disabled`.
fn trigger_detail(kind: &str, event: &str, status: &str) -> String {
    let mut s = format!("{} {}", kind.trim(), event.trim());
    if status == "DISABLED" {
        s.push_str(", disabled");
    }
    s
}

fn relation_folders(
    parent: &ObjectPath,
    _schema: &str,
    _name: &str,
    kind: ObjectKind,
) -> Vec<ObjectNode> {
    let mut keys = vec![folder::COLUMNS];
    if kind != ObjectKind::View {
        keys.push(folder::INDEXES);
    }
    if kind != ObjectKind::MaterializedView {
        keys.push(folder::TRIGGERS);
    }
    keys.into_iter()
        .map(|k| node(parent, ObjectKind::Folder, k.to_string(), true, None))
        .collect()
}

fn relation_items(
    conn: &Connection,
    parent: &ObjectPath,
    schema: &str,
    name: &str,
    key: &str,
) -> Result<Vec<ObjectNode>> {
    match key {
        folder::COLUMNS => Ok(catalog::columns(conn, schema, name)?
            .into_iter()
            .map(|c| {
                let detail = if c.primary_key {
                    format!("{} (PK)", c.type_name)
                } else {
                    c.type_name.clone()
                };
                node(parent, ObjectKind::Column, c.name, false, Some(detail))
            })
            .collect()),
        folder::INDEXES => Ok(catalog::indexes(conn, schema, name)?
            .into_iter()
            .map(|i| {
                let detail = format!(
                    "{}({})",
                    if i.primary {
                        "primary key "
                    } else if i.unique {
                        "unique "
                    } else {
                        ""
                    },
                    i.columns.join(", ")
                );
                node(parent, ObjectKind::Index, i.name, false, Some(detail))
            })
            .collect()),
        folder::TRIGGERS => {
            let list = rows(
                conn,
                "SELECT trigger_name, trigger_type, triggering_event, status FROM all_triggers \
                 WHERE table_owner = :1 AND table_name = :2 ORDER BY trigger_name",
                &[&schema, &name],
            )?;
            Ok(list
                .iter()
                .map(|r| {
                    node(
                        parent,
                        ObjectKind::Trigger,
                        cell(r, 0),
                        false,
                        Some(trigger_detail(&cell(r, 1), &cell(r, 2), &cell(r, 3))),
                    )
                })
                .collect())
        }
        other => Err(Error::NotFound(format!("unknown folder \"{other}\""))),
    }
}

// -------------------------------------------------------------------------------------------
// Search
// -------------------------------------------------------------------------------------------

const DEFAULT_SEARCH_KINDS: [ObjectKind; 10] = [
    ObjectKind::Table,
    ObjectKind::View,
    ObjectKind::MaterializedView,
    ObjectKind::Sequence,
    ObjectKind::Synonym,
    ObjectKind::Function,
    ObjectKind::Procedure,
    ObjectKind::Package,
    ObjectKind::Type,
    ObjectKind::Trigger,
];

fn dictionary_type(kind: ObjectKind) -> Option<&'static str> {
    Some(match kind {
        ObjectKind::Table => "TABLE",
        ObjectKind::View => "VIEW",
        ObjectKind::MaterializedView => "MATERIALIZED VIEW",
        ObjectKind::Sequence => "SEQUENCE",
        ObjectKind::Synonym => "SYNONYM",
        ObjectKind::Function => "FUNCTION",
        ObjectKind::Procedure => "PROCEDURE",
        ObjectKind::Package => "PACKAGE",
        ObjectKind::Type => "TYPE",
        ObjectKind::Trigger => "TRIGGER",
        _ => return None,
    })
}

fn kind_of_dictionary(ty: &str) -> ObjectKind {
    match ty {
        "VIEW" => ObjectKind::View,
        "MATERIALIZED VIEW" => ObjectKind::MaterializedView,
        "SEQUENCE" => ObjectKind::Sequence,
        "SYNONYM" => ObjectKind::Synonym,
        "FUNCTION" => ObjectKind::Function,
        "PROCEDURE" => ObjectKind::Procedure,
        "PACKAGE" => ObjectKind::Package,
        "TYPE" => ObjectKind::Type,
        "TRIGGER" => ObjectKind::Trigger,
        _ => ObjectKind::Table,
    }
}

/// Search scope: the whole database (visible schemas) or one schema.
fn search_scope(req: &ObjectSearchRequest) -> Result<Option<String>> {
    use ObjectKind as K;
    match non_folder_segments(&req.scope).as_slice() {
        [] => Ok(None),
        [(K::Schema, s)] => Ok(Some((*s).to_string())),
        _ => Err(Error::NotFound(
            "search scope must be the root or a schema".to_string(),
        )),
    }
}

/// `search_objects`: case-insensitive substring match on the object name.
pub async fn search(shared: &Arc<Shared>, req: ObjectSearchRequest) -> Result<Vec<ObjectNode>> {
    if req.query.trim().is_empty() {
        return Ok(Vec::new());
    }
    let schema = search_scope(&req)?;
    let kinds: Vec<ObjectKind> = if req.kinds.is_empty() {
        DEFAULT_SEARCH_KINDS.to_vec()
    } else {
        req.kinds.clone()
    };
    let types: Vec<&str> = kinds.iter().filter_map(|k| dictionary_type(*k)).collect();
    if types.is_empty() {
        return Ok(Vec::new());
    }
    let limit = req.limit.clamp(1, 1000);
    let query = req.query.trim().to_string();
    let show_all = shared.target.show_all_schemas;
    let type_list = types
        .iter()
        .map(|t| format!("'{t}'"))
        .collect::<Vec<_>>()
        .join(",");
    // Without a schema scope the Oracle-maintained schemas are skipped (like the tree does).
    let owner_filter = if schema.is_some() {
        "o.owner = :schema"
    } else if show_all {
        ":schema IS NULL"
    } else {
        "(:schema IS NULL AND (o.owner = SYS_CONTEXT('USERENV','SESSION_USER') OR o.owner IN \
          (SELECT u.username FROM all_users u WHERE u.oracle_maintained = 'N')))"
    };
    let sql = format!(
        "SELECT o.owner, o.object_name, o.object_type FROM all_objects o \
         WHERE o.object_type IN ({type_list}) AND {owner_filter} \
           AND o.object_name NOT LIKE 'BIN$%' AND o.generated = 'N' \
           AND INSTR(UPPER(o.object_name), UPPER(:q)) > 0 \
           AND NOT (o.object_type = 'TABLE' AND EXISTS \
                    (SELECT 1 FROM all_mviews m WHERE m.owner = o.owner AND m.mview_name = o.object_name)) \
         ORDER BY CASE WHEN UPPER(o.object_name) = UPPER(:q) THEN 0 \
                       WHEN INSTR(UPPER(o.object_name), UPPER(:q)) = 1 THEN 1 ELSE 2 END, \
                  o.object_name, o.owner \
         FETCH FIRST {limit} ROWS ONLY"
    );
    let scoped = schema.is_some();
    let schema_bind = schema.clone();
    let list = shared
        .with_conn(move |conn| {
            let rs = conn
                .query_named(
                    &sql,
                    &[
                        ("schema", &schema_bind as &dyn oracle::sql_type::ToSql),
                        ("q", &query),
                    ],
                )
                .map_err(crate::error::map_error)?;
            let mut out = Vec::new();
            for row in rs {
                let row = row.map_err(crate::error::map_error)?;
                let owner: String = row.get(0).map_err(crate::error::map_error)?;
                let name: String = row.get(1).map_err(crate::error::map_error)?;
                let ty: String = row.get(2).map_err(crate::error::map_error)?;
                out.push((owner, name, ty));
            }
            Ok(out)
        })
        .await?;
    let _ = scoped;
    Ok(list
        .into_iter()
        .map(|(owner, name, ty)| {
            let kind = kind_of_dictionary(&ty);
            ObjectNode {
                path: catalog::object_path(&owner, kind, &name),
                kind,
                has_children: matches!(
                    kind,
                    ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView
                ),
                detail: Some(owner),
                name,
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_folder_order_is_stable() {
        let parent = ObjectPath::root().child(ObjectKind::Schema, "HR");
        let nodes = schema_folders(&parent, "HR");
        let names: Vec<&str> = nodes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "tables",
                "views",
                "materializedViews",
                "sequences",
                "synonyms",
                "functions",
                "procedures",
                "packages",
                "types",
                "triggers"
            ]
        );
        assert!(
            nodes
                .iter()
                .all(|n| n.kind == ObjectKind::Folder && n.has_children)
        );
        assert_eq!(nodes[0].path.segments.len(), 2);
    }

    #[test]
    fn relation_folders_depend_on_the_kind() {
        let parent = ObjectPath::root().child(ObjectKind::Schema, "HR");
        let names = |k| -> Vec<String> {
            relation_folders(&parent, "HR", "T", k)
                .into_iter()
                .map(|n| n.name)
                .collect()
        };
        assert_eq!(names(ObjectKind::Table), ["columns", "indexes", "triggers"]);
        assert_eq!(names(ObjectKind::View), ["columns", "triggers"]);
        assert_eq!(names(ObjectKind::MaterializedView), ["columns", "indexes"]);
    }

    #[test]
    fn trigger_details() {
        assert_eq!(
            trigger_detail("BEFORE EACH ROW", "INSERT OR UPDATE", "ENABLED"),
            "BEFORE EACH ROW INSERT OR UPDATE"
        );
        assert!(trigger_detail("AFTER STATEMENT", "DELETE", "DISABLED").ends_with(", disabled"));
    }

    #[test]
    fn search_scope_parsing() {
        let mk = |p: ObjectPath| ObjectSearchRequest {
            query: "x".into(),
            scope: p,
            kinds: vec![],
            limit: 5,
        };
        assert_eq!(search_scope(&mk(ObjectPath::root())).unwrap(), None);
        let s = ObjectPath::root().child(ObjectKind::Schema, "HR");
        assert_eq!(search_scope(&mk(s.clone())).unwrap().as_deref(), Some("HR"));
        let bad = s
            .child(ObjectKind::Folder, "tables")
            .child(ObjectKind::Table, "T");
        assert!(search_scope(&mk(bad)).is_err());
    }

    #[test]
    fn kind_mapping_roundtrips() {
        for k in DEFAULT_SEARCH_KINDS {
            let ty = dictionary_type(k).unwrap();
            assert_eq!(kind_of_dictionary(ty), k);
        }
        assert!(dictionary_type(ObjectKind::Column).is_none());
    }
}
