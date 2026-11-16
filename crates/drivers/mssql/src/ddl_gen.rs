//! `generate_ddl`: ordered, re-runnable DDL of a database, schema or chosen objects.
//!
//! Order of the returned items (each one batch, no `GO`):
//!
//! 1. `PreData`: drops (when asked), schemas, types, then sequences, tables, functions, views and
//!    procedures sorted by `sys.sql_expression_dependencies` (name order for ties and cycles).
//! 2. `PostData`: indexes, foreign keys, triggers (disabled ones are disabled again) and
//!    table/column comments, so a data load runs without constraints or triggers firing.
//!
//! Limits: system-versioned (temporal) tables are created as plain tables, memory-optimized
//! tables, XML and spatial indexes and CLR objects are not scripted, and `if_not_exists` uses
//! `CREATE OR ALTER` for modules, an existence guard for the other objects and
//! update-or-add for comments.

use std::collections::{BTreeMap, BTreeSet};

use dbstudio_core::{
    DdlItem, DdlLanguage, DdlRequest, DdlSection, Error, ObjectKind, ObjectPath, Result,
};

use crate::browse::database_collation;
use crate::catalog;
use crate::conn::Conn;
use crate::ddl::{self, Names};
use crate::session::MssqlSession;
use crate::sql::{lit, object_path, parse_object, quote, schema_path};
use crate::value::{bool_at, int_at, opt_str_at, str_at};

/// One object of the script with the id used for dependency ordering.
struct Node {
    id: i64,
    kind: ObjectKind,
    schema: String,
    name: String,
    sql: String,
    /// Statement that drops it.
    drop: String,
}

impl Node {
    fn key(&self) -> (u8, String, String) {
        (0, self.schema.clone(), self.name.clone())
    }
}

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

/// Topological order of `nodes` by `edges` (`a -> b` = `a` needs `b`); ties by name.
fn topo(nodes: Vec<Node>, edges: &[(i64, i64)]) -> Vec<Node> {
    let ids: BTreeSet<i64> = nodes.iter().map(|n| n.id).collect();
    let mut needs: BTreeMap<i64, BTreeSet<i64>> = BTreeMap::new();
    for &(a, b) in edges {
        if a != b && ids.contains(&a) && ids.contains(&b) {
            needs.entry(a).or_default().insert(b);
        }
    }
    let mut remaining: Vec<Node> = nodes;
    remaining.sort_by_key(|n| n.key());
    let mut done: BTreeSet<i64> = BTreeSet::new();
    let mut out = Vec::with_capacity(remaining.len());
    while !remaining.is_empty() {
        let pos = remaining
            .iter()
            .position(|n| {
                needs
                    .get(&n.id)
                    .is_none_or(|deps| deps.iter().all(|d| done.contains(d)))
            })
            // A cycle: take the first by name.
            .unwrap_or(0);
        let n = remaining.remove(pos);
        done.insert(n.id);
        out.push(n);
    }
    out
}

struct Selection {
    schema: Option<String>,
    objects: BTreeSet<(String, String)>,
}

impl Selection {
    fn wants(&self, schema: &str, name: &str) -> bool {
        self.objects.is_empty()
            || self
                .objects
                .contains(&(schema.to_string(), name.to_string()))
    }
}

fn schema_filter(col: &str, schema: Option<&str>) -> String {
    match schema {
        Some(s) => format!(" AND {col} = SCHEMA_ID({})", lit(s)),
        None => String::new(),
    }
}

pub async fn generate(sess: &MssqlSession, req: DdlRequest) -> Result<Vec<DdlItem>> {
    let db = sess.database_of(&req.scope);
    let schema = req
        .scope
        .segments
        .iter()
        .find(|s| s.kind == ObjectKind::Schema)
        .map(|s| s.name.clone());
    // A scope naming one object restricts to it.
    let mut objects: BTreeSet<(String, String)> = BTreeSet::new();
    for o in &req.objects {
        let r = parse_object(o)?;
        objects.insert((r.schema, r.name));
    }
    if objects.is_empty() {
        if let Ok(r) = parse_object(&req.scope) {
            objects.insert((r.schema, r.name));
        }
    }
    let sel = Selection { schema, objects };
    let mut conn = sess.shared.acquire_catalog(&db).await?;
    let result = build(&mut conn, &db, &sel, &req).await;
    conn.release();
    result
}

#[allow(clippy::too_many_lines)]
async fn build(
    conn: &mut Conn,
    db: &str,
    sel: &Selection,
    req: &DdlRequest,
) -> Result<Vec<DdlItem>> {
    let o = &req.options;
    let names = Names {
        qualify: o.qualify_schema,
    };
    let ver = conn.version;
    let collation = database_collation(conn).await?;
    let scope_filter = schema_filter("o.schema_id", sel.schema.as_deref());

    // ---- tables and views' catalog data -------------------------------------------------
    let table_filter = format!("o.type = 'U' AND o.is_ms_shipped = 0{scope_filter}");
    let table_ids = conn
        .rows(&format!(
            "SELECT o.object_id, SCHEMA_NAME(o.schema_id), o.name FROM sys.objects o WHERE {table_filter} ORDER BY 2, 3"
        ))
        .await?;
    let cols = catalog::columns(conn, &table_filter).await?;
    let idx = catalog::indexes(
        conn,
        &format!("o.type IN ('U','V') AND o.is_ms_shipped = 0{scope_filter}"),
    )
    .await?;
    let checks = catalog::checks(conn, &table_filter).await?;
    let fks = catalog::foreign_keys(conn, &table_filter).await?;
    let comments = if o.include_comments {
        catalog::table_comments(conn, &table_filter).await?
    } else {
        BTreeMap::new()
    };

    let mut pre: Vec<DdlItem> = Vec::new();
    let mut post: Vec<DdlItem> = Vec::new();
    let mut drops: Vec<DdlItem> = Vec::new();
    let mut fk_drops: Vec<DdlItem> = Vec::new();
    let mut nodes: Vec<Node> = Vec::new();
    let mut used_schemas: BTreeSet<String> = BTreeSet::new();

    let guard = |exists: String, body: String| -> String {
        if o.if_not_exists {
            format!("IF {exists}\nBEGIN\n{body}\nEND")
        } else {
            body
        }
    };

    // ---- tables ---------------------------------------------------------------------------
    let mut table_nodes: BTreeSet<(String, String)> = BTreeSet::new();
    for r in &table_ids {
        let (id, schema, name) = (int_at(r, 0), str_at(r, 1), str_at(r, 2));
        if !sel.wants(&schema, &name) {
            continue;
        }
        let key = (schema.clone(), name.clone());
        table_nodes.insert(key.clone());
        used_schemas.insert(schema.clone());
        let empty = Vec::new();
        let tcols = cols.get(&key).unwrap_or(&empty);
        let empty_i = Vec::new();
        let tidx = idx.get(&key).unwrap_or(&empty_i);
        let empty_c = Vec::new();
        let tchecks = checks.get(&key).unwrap_or(&empty_c);
        let create = ddl::create_table(names, &schema, &name, tcols, tidx, tchecks, &collation);
        let sql = guard(
            format!(
                "OBJECT_ID({}, N'U') IS NULL",
                lit(&format!("{}.{}", quote(&schema), quote(&name)))
            ),
            create,
        );
        nodes.push(Node {
            id,
            kind: ObjectKind::Table,
            schema: schema.clone(),
            name: name.clone(),
            sql,
            drop: ddl::drop_if_exists(&ver, "TABLE", Some("U"), &names.obj(&schema, &name)),
        });
        let tpath = object_path(db, &schema, ObjectKind::Table, &name);
        if o.include_indexes {
            for i in tidx.iter().filter(|i| !i.primary && !i.unique_constraint) {
                if let Some(s) = ddl::create_index(names, &schema, &name, i) {
                    let sql = guard(
                        format!(
                            "NOT EXISTS (SELECT 1 FROM sys.indexes WHERE name = {} AND object_id = OBJECT_ID({}))",
                            lit(&i.name),
                            lit(&format!("{}.{}", quote(&schema), quote(&name)))
                        ),
                        s,
                    );
                    post.push(item(
                        tpath
                            .child(ObjectKind::Folder, dbstudio_core::folder::INDEXES)
                            .child(ObjectKind::Index, i.name.clone()),
                        ObjectKind::Index,
                        DdlSection::PostData,
                        sql,
                        vec![tpath.clone()],
                    ));
                }
            }
        }
        for k in tchecks.iter().filter(|k| !k.plain()) {
            let sql = guard(
                format!(
                    "NOT EXISTS (SELECT 1 FROM sys.check_constraints WHERE name = {} AND parent_object_id = OBJECT_ID({}))",
                    lit(&k.name),
                    lit(&format!("{}.{}", quote(&schema), quote(&name)))
                ),
                ddl::add_check(names, &schema, &name, k),
            );
            post.push(item(
                tpath.clone(),
                ObjectKind::Table,
                DdlSection::PostData,
                sql,
                vec![],
            ));
        }
        if o.include_foreign_keys {
            for f in fks.get(&key).into_iter().flatten() {
                if !sel.wants(&f.ref_schema, &f.ref_table) {
                    continue;
                }
                let s = ddl::add_foreign_key(names, &schema, &name, f);
                let sql = guard(
                    format!(
                        "NOT EXISTS (SELECT 1 FROM sys.foreign_keys WHERE name = {} AND parent_object_id = OBJECT_ID({}))",
                        lit(&f.name),
                        lit(&format!("{}.{}", quote(&schema), quote(&name)))
                    ),
                    s,
                );
                post.push(item(
                    tpath.clone(),
                    ObjectKind::Table,
                    DdlSection::PostData,
                    sql,
                    vec![object_path(
                        db,
                        &f.ref_schema,
                        ObjectKind::Table,
                        &f.ref_table,
                    )],
                ));
                if o.include_drop {
                    fk_drops.push(item(
                        tpath.clone(),
                        ObjectKind::Table,
                        DdlSection::PreData,
                        ddl::drop_constraint_if_exists(
                            &ver,
                            &names.obj(&schema, &name),
                            &schema,
                            &f.name,
                            "F",
                        ),
                        vec![],
                    ));
                }
            }
        }
        if o.include_comments {
            if let Some(c) = comments.get(&key) {
                post.push(item(
                    tpath.clone(),
                    ObjectKind::Table,
                    DdlSection::PostData,
                    ddl::comment_sql(&schema, &name, None, "TABLE", c, o.if_not_exists),
                    vec![],
                ));
            }
            for c in tcols.iter().filter(|c| c.comment.is_some()) {
                post.push(item(
                    tpath.clone(),
                    ObjectKind::Table,
                    DdlSection::PostData,
                    ddl::comment_sql(
                        &schema,
                        &name,
                        Some(&c.name),
                        "TABLE",
                        c.comment.as_deref().unwrap_or(""),
                        o.if_not_exists,
                    ),
                    vec![],
                ));
            }
        }
        if o.include_triggers {
            let rows = conn
                .rows(&format!(
                    "SELECT OBJECT_DEFINITION(tr.object_id), tr.is_disabled, SCHEMA_NAME(t.schema_id), tr.name \
                     FROM sys.triggers tr JOIN sys.objects t ON t.object_id = tr.object_id \
                     WHERE tr.parent_id = {id} ORDER BY tr.name"
                ))
                .await?;
            for tr in &rows {
                let Some(def) = opt_str_at(tr, 0) else {
                    continue;
                };
                let tname = str_at(tr, 3);
                let tschema = str_at(tr, 2);
                let tp = tpath
                    .child(ObjectKind::Folder, dbstudio_core::folder::TRIGGERS)
                    .child(ObjectKind::Trigger, tname.clone());
                post.push(item(
                    tp.clone(),
                    ObjectKind::Trigger,
                    DdlSection::PostData,
                    if o.if_not_exists {
                        ddl::create_module_idempotent(
                            &ver,
                            &format!("{}.{}", quote(&tschema), quote(&tname)),
                            &def,
                        )
                    } else {
                        def
                    },
                    vec![tpath.clone()],
                ));
                if bool_at(tr, 1) {
                    post.push(item(
                        tp,
                        ObjectKind::Trigger,
                        DdlSection::PostData,
                        format!(
                            "DISABLE TRIGGER {} ON {};",
                            names.obj(&tschema, &tname),
                            names.obj(&schema, &name)
                        ),
                        vec![],
                    ));
                }
            }
        }
    }

    // ---- views, functions, procedures ----------------------------------------------------
    if o.include_views || o.include_routines {
        let mut types: Vec<&str> = Vec::new();
        if o.include_views {
            types.push("'V'");
        }
        if o.include_routines {
            types.extend(["'FN'", "'IF'", "'TF'", "'P'"]);
        }
        let rows = conn
            .rows(&format!(
                "SELECT o.object_id, SCHEMA_NAME(o.schema_id), o.name, o.type, OBJECT_DEFINITION(o.object_id) \
                 FROM sys.objects o WHERE o.type IN ({}) AND o.is_ms_shipped = 0{scope_filter} ORDER BY 2, 3",
                types.join(",")
            ))
            .await?;
        for r in &rows {
            let (id, schema, name) = (int_at(r, 0), str_at(r, 1), str_at(r, 2));
            if !sel.wants(&schema, &name) {
                continue;
            }
            let Some(def) = opt_str_at(r, 4) else {
                continue; // encrypted
            };
            let t = str_at(r, 3);
            let (kind, word) = match t.trim() {
                "V" => (ObjectKind::View, "VIEW"),
                "P" => (ObjectKind::Procedure, "PROCEDURE"),
                _ => (ObjectKind::Function, "FUNCTION"),
            };
            used_schemas.insert(schema.clone());
            nodes.push(Node {
                id,
                kind,
                schema: schema.clone(),
                name: name.clone(),
                sql: if o.if_not_exists {
                    ddl::create_module_idempotent(
                        &ver,
                        &format!("{}.{}", quote(&schema), quote(&name)),
                        &def,
                    )
                } else {
                    def
                },
                drop: ddl::drop_if_exists(
                    &ver,
                    word,
                    match kind {
                        ObjectKind::View => Some("V"),
                        ObjectKind::Procedure => Some("P"),
                        _ => None,
                    },
                    &names.obj(&schema, &name),
                ),
            });
            // Indexes of indexed views.
            if kind == ObjectKind::View && o.include_indexes {
                let key = (schema.clone(), name.clone());
                for i in idx.get(&key).into_iter().flatten() {
                    if let Some(s) = ddl::create_index(names, &schema, &name, i) {
                        let s = guard(
                            format!(
                                "NOT EXISTS (SELECT 1 FROM sys.indexes WHERE name = {} AND object_id = OBJECT_ID({}))",
                                lit(&i.name),
                                lit(&format!("{}.{}", quote(&schema), quote(&name)))
                            ),
                            s,
                        );
                        post.push(item(
                            object_path(db, &schema, ObjectKind::View, &name)
                                .child(ObjectKind::Folder, dbstudio_core::folder::INDEXES)
                                .child(ObjectKind::Index, i.name.clone()),
                            ObjectKind::Index,
                            DdlSection::PostData,
                            s,
                            vec![object_path(db, &schema, ObjectKind::View, &name)],
                        ));
                    }
                }
            }
        }
    }

    // ---- sequences -----------------------------------------------------------------------
    if o.include_sequences && ver.has_sequences() {
        let rows = conn
            .rows(&format!(
                "SELECT o.object_id, SCHEMA_NAME(o.schema_id), o.name, TYPE_NAME(s.user_type_id), \
                 CAST((SELECT CASE WHEN x.n > CAST(s.maximum_value AS DECIMAL(38,0)) THEN CAST(s.maximum_value AS DECIMAL(38,0)) WHEN x.n < CAST(s.minimum_value AS DECIMAL(38,0)) THEN CAST(s.minimum_value AS DECIMAL(38,0)) ELSE x.n END FROM (SELECT CASE WHEN s.last_used_value IS NULL THEN CAST(s.start_value AS DECIMAL(38,0)) ELSE CAST(s.last_used_value AS DECIMAL(38,0)) + CAST(s.increment AS DECIMAL(38,0)) END AS n) x) AS NVARCHAR(60)), CAST(s.increment AS NVARCHAR(60)), \
                 CAST(s.minimum_value AS NVARCHAR(60)), CAST(s.maximum_value AS NVARCHAR(60)), \
                 s.is_cycling, s.is_cached, s.cache_size \
                 FROM sys.sequences s JOIN sys.objects o ON o.object_id = s.object_id \
                 WHERE o.is_ms_shipped = 0{scope_filter} ORDER BY 2, 3"
            ))
            .await?;
        for r in &rows {
            let (id, schema, name) = (int_at(r, 0), str_at(r, 1), str_at(r, 2));
            if !sel.wants(&schema, &name) {
                continue;
            }
            used_schemas.insert(schema.clone());
            let mut s = format!(
                "CREATE SEQUENCE {} AS {} START WITH {} INCREMENT BY {} MINVALUE {} MAXVALUE {} {}",
                names.obj(&schema, &name),
                str_at(r, 3),
                str_at(r, 4),
                str_at(r, 5),
                str_at(r, 6),
                str_at(r, 7),
                if bool_at(r, 8) { "CYCLE" } else { "NO CYCLE" }
            );
            if bool_at(r, 9) {
                match opt_str_at(r, 10) {
                    Some(n) => s.push_str(&format!(" CACHE {n}")),
                    None => s.push_str(" CACHE"),
                }
            } else {
                s.push_str(" NO CACHE");
            }
            s.push(';');
            let sql = guard(
                format!(
                    "OBJECT_ID({}, N'SO') IS NULL",
                    lit(&format!("{}.{}", quote(&schema), quote(&name)))
                ),
                s,
            );
            nodes.push(Node {
                id,
                kind: ObjectKind::Sequence,
                schema: schema.clone(),
                name: name.clone(),
                sql,
                drop: ddl::drop_if_exists(&ver, "SEQUENCE", Some("SO"), &names.obj(&schema, &name)),
            });
        }
    }

    // ---- synonyms ------------------------------------------------------------------------
    {
        let rows = conn
            .rows(&format!(
                "SELECT o.object_id, SCHEMA_NAME(o.schema_id), o.name, sn.base_object_name \
                 FROM sys.synonyms sn JOIN sys.objects o ON o.object_id = sn.object_id \
                 WHERE o.is_ms_shipped = 0{scope_filter} ORDER BY 2, 3"
            ))
            .await?;
        for r in &rows {
            let (id, schema, name) = (int_at(r, 0), str_at(r, 1), str_at(r, 2));
            if !sel.wants(&schema, &name) {
                continue;
            }
            used_schemas.insert(schema.clone());
            let sql = guard(
                format!(
                    "OBJECT_ID({}, N'SN') IS NULL",
                    lit(&format!("{}.{}", quote(&schema), quote(&name)))
                ),
                format!(
                    "CREATE SYNONYM {} FOR {};",
                    names.obj(&schema, &name),
                    str_at(r, 3)
                ),
            );
            nodes.push(Node {
                id,
                kind: ObjectKind::Synonym,
                schema: schema.clone(),
                name: name.clone(),
                sql,
                drop: ddl::drop_if_exists(&ver, "SYNONYM", Some("SN"), &names.obj(&schema, &name)),
            });
        }
    }

    // ---- types ---------------------------------------------------------------------------
    let mut type_items: Vec<DdlItem> = Vec::new();
    {
        let rows = conn
            .rows(&format!(
                "SELECT SCHEMA_NAME(t.schema_id), t.name, t.is_table_type, t.is_assembly_type, bt.name, \
                 t.max_length, t.precision, t.scale, t.is_nullable, \
                 (SELECT tt.type_table_object_id FROM sys.table_types tt WHERE tt.user_type_id = t.user_type_id) \
                 FROM sys.types t LEFT JOIN sys.types bt ON bt.user_type_id = t.system_type_id AND bt.is_user_defined = 0 \
                 WHERE t.is_user_defined = 1{} ORDER BY 1, 2",
                schema_filter("t.schema_id", sel.schema.as_deref())
            ))
            .await?;
        for r in &rows {
            let (schema, name) = (str_at(r, 0), str_at(r, 1));
            if bool_at(r, 3) || !sel.wants(&schema, &name) {
                continue;
            }
            used_schemas.insert(schema.clone());
            let create = if bool_at(r, 2) {
                let tt = int_at(r, 9);
                let tcols = catalog::columns(conn, &format!("o.object_id = {tt}")).await?;
                let list = tcols.into_values().next().unwrap_or_default();
                let lines: Vec<String> = list
                    .iter()
                    .map(|c| format!("    {}", ddl::type_column_definition(c, &collation)))
                    .collect();
                format!(
                    "CREATE TYPE {} AS TABLE (\n{}\n);",
                    names.obj(&schema, &name),
                    lines.join(",\n")
                )
            } else {
                format!(
                    "CREATE TYPE {} FROM {}{};",
                    names.obj(&schema, &name),
                    crate::sql::format_type(
                        &str_at(r, 4),
                        int_at(r, 5),
                        int_at(r, 6),
                        int_at(r, 7)
                    )
                    .to_ascii_uppercase(),
                    if bool_at(r, 8) { " NULL" } else { " NOT NULL" }
                )
            };
            let sql = guard(
                format!(
                    "TYPE_ID({}) IS NULL",
                    lit(&format!("{}.{}", quote(&schema), quote(&name)))
                ),
                create,
            );
            type_items.push(item(
                object_path(db, &schema, ObjectKind::Type, &name),
                ObjectKind::Type,
                DdlSection::PreData,
                sql,
                vec![],
            ));
            drops.push(item(
                object_path(db, &schema, ObjectKind::Type, &name),
                ObjectKind::Type,
                DdlSection::PreData,
                ddl::drop_if_exists(&ver, "TYPE", None, &names.obj(&schema, &name)),
                vec![],
            ));
        }
    }

    // ---- dependency order of the objects -------------------------------------------------
    let edge_rows = conn
        .rows(
            "SELECT referencing_id, referenced_id FROM sys.sql_expression_dependencies \
             WHERE referencing_class = 1 AND referenced_id IS NOT NULL AND referenced_database_name IS NULL",
        )
        .await?;
    let edges: Vec<(i64, i64)> = edge_rows
        .iter()
        .map(|r| (int_at(r, 0), int_at(r, 1)))
        .collect();
    let by_id: BTreeMap<i64, (ObjectKind, String, String)> = nodes
        .iter()
        .map(|n| (n.id, (n.kind, n.schema.clone(), n.name.clone())))
        .collect();
    let ordered = topo(nodes, &edges);
    let mut node_drops: Vec<DdlItem> = Vec::new();
    for n in &ordered {
        let deps: Vec<ObjectPath> = edges
            .iter()
            .filter(|(a, b)| *a == n.id && *b != n.id)
            .filter_map(|(_, b)| by_id.get(b))
            .map(|(k, s, nm)| object_path(db, s, *k, nm))
            .collect();
        let path = object_path(db, &n.schema, n.kind, &n.name);
        pre.push(item(
            path.clone(),
            n.kind,
            DdlSection::PreData,
            n.sql.clone(),
            deps,
        ));
        node_drops.push(item(
            path,
            n.kind,
            DdlSection::PreData,
            n.drop.clone(),
            vec![],
        ));
    }
    node_drops.reverse();

    // ---- schemas -------------------------------------------------------------------------
    let mut schema_items: Vec<DdlItem> = Vec::new();
    for s in used_schemas
        .iter()
        .filter(|s| !s.eq_ignore_ascii_case("dbo"))
    {
        // Schemas are never dropped, so a re-run with drops must tolerate existing ones.
        let create = if o.if_not_exists || o.include_drop {
            format!(
                "IF SCHEMA_ID({}) IS NULL EXEC(N'CREATE SCHEMA {}');",
                lit(s),
                quote(s).replace('\'', "''")
            )
        } else {
            format!("CREATE SCHEMA {};", quote(s))
        };
        schema_items.push(item(
            schema_path(db, s),
            ObjectKind::Schema,
            DdlSection::PreData,
            create,
            vec![],
        ));
    }

    let mut out = Vec::new();
    if o.include_drop {
        out.extend(fk_drops);
        out.extend(node_drops);
        drops.reverse();
        out.extend(drops);
    }
    out.extend(schema_items);
    // Alias types first: table types may use them for their columns.
    let (aliases, table_types): (Vec<_>, Vec<_>) = type_items
        .into_iter()
        .partition(|i| !i.sql.contains("AS TABLE ("));
    out.extend(aliases);
    out.extend(table_types);
    out.extend(pre);
    out.extend(post);
    if out.is_empty() && !req.objects.is_empty() {
        return Err(Error::NotFound(
            "none of the requested objects exist".into(),
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: i64, name: &str) -> Node {
        Node {
            id,
            kind: ObjectKind::View,
            schema: "dbo".into(),
            name: name.into(),
            sql: String::new(),
            drop: String::new(),
        }
    }

    #[test]
    fn topological_order_with_ties_and_cycles() {
        let nodes = vec![node(1, "a_view"), node(2, "b_table"), node(3, "c_func")];
        // a_view needs c_func, c_func needs b_table.
        let order = topo(nodes, &[(1, 3), (3, 2)]);
        let names: Vec<_> = order.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["b_table", "c_func", "a_view"]);
        // A cycle does not hang and keeps every node.
        let order = topo(vec![node(1, "x"), node(2, "y")], &[(1, 2), (2, 1)]);
        assert_eq!(order.len(), 2);
        // Edges to unselected objects are ignored.
        let order = topo(vec![node(1, "x")], &[(1, 99)]);
        assert_eq!(order.len(), 1);
    }

    #[test]
    fn selection_filters() {
        let all = Selection {
            schema: None,
            objects: BTreeSet::new(),
        };
        assert!(all.wants("s", "t"));
        let one = Selection {
            schema: None,
            objects: BTreeSet::from([("s".to_string(), "t".to_string())]),
        };
        assert!(one.wants("s", "t") && !one.wants("s", "u"));
    }
}
