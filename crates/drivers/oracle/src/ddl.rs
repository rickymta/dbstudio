//! DDL text through `DBMS_METADATA` (`object_ddl`) and ordered DDL of a scope (`generate_ddl`).
//!
//! Transform parameters: `SQLTERMINATOR`, no `SEGMENT_ATTRIBUTES` / `STORAGE`, constraints inline
//! in `CREATE TABLE` (`CONSTRAINTS_AS_ALTER` off) except foreign keys (`REF_CONSTRAINTS` off) which
//! are emitted separately in the post-data section together with indexes, comments and triggers.
//! Pre-data objects are ordered with `ALL_DEPENDENCIES`.

use std::collections::{HashMap, HashSet};

use dbstudio_core::{
    DdlItem, DdlLanguage, DdlOptions, DdlRequest, DdlSection, Error, ObjectKind, ObjectPath, Result,
};
use oracle::Connection;

use crate::catalog::{self, cell, object_path, rows};
use crate::error::map_error;
use crate::sql::schema_object_of;

/// `ORA-31603`: object not found; `ORA-31608`: no dependent object of that type.
const NOT_FOUND_CODES: [i32; 2] = [31603, 31608];

fn ora_code(e: &Error) -> Option<i32> {
    // `map_error` keeps the "ORA-nnnnn:" prefix in the message.
    let m = e.message();
    let idx = m.find("ORA-")?;
    m[idx + 4..idx + 9].parse().ok()
}

/// Set the session transform parameters used by every call below.
pub fn set_transforms(conn: &Connection) -> Result<()> {
    conn.execute(
        "BEGIN \
           DBMS_METADATA.SET_TRANSFORM_PARAM(DBMS_METADATA.SESSION_TRANSFORM, 'SQLTERMINATOR', TRUE); \
           DBMS_METADATA.SET_TRANSFORM_PARAM(DBMS_METADATA.SESSION_TRANSFORM, 'SEGMENT_ATTRIBUTES', FALSE); \
           DBMS_METADATA.SET_TRANSFORM_PARAM(DBMS_METADATA.SESSION_TRANSFORM, 'STORAGE', FALSE); \
           DBMS_METADATA.SET_TRANSFORM_PARAM(DBMS_METADATA.SESSION_TRANSFORM, 'CONSTRAINTS_AS_ALTER', FALSE); \
           DBMS_METADATA.SET_TRANSFORM_PARAM(DBMS_METADATA.SESSION_TRANSFORM, 'REF_CONSTRAINTS', FALSE); \
           DBMS_METADATA.SET_TRANSFORM_PARAM(DBMS_METADATA.SESSION_TRANSFORM, 'PRETTY', TRUE); \
         END;",
        &[],
    )
    .map_err(map_error)?;
    Ok(())
}

fn clean(s: String) -> String {
    s.trim().to_string()
}

/// `GET_DDL` of one object; `None` when it does not exist (or is not visible).
pub fn get_ddl(conn: &Connection, ty: &str, name: &str, owner: &str) -> Result<Option<String>> {
    match catalog::scalar(
        conn,
        "SELECT DBMS_METADATA.GET_DDL(:1, :2, :3) FROM dual",
        &[&ty, &name, &owner],
    ) {
        Ok(v) => Ok(v.map(clean).filter(|s| !s.is_empty())),
        Err(e) if ora_code(&e).is_some_and(|c| NOT_FOUND_CODES.contains(&c)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// `GET_DEPENDENT_DDL` (`INDEX`, `COMMENT`, `REF_CONSTRAINT`, `OBJECT_GRANT`) of a base object.
pub fn get_dependent_ddl(
    conn: &Connection,
    ty: &str,
    base: &str,
    owner: &str,
) -> Result<Option<String>> {
    match catalog::scalar(
        conn,
        "SELECT DBMS_METADATA.GET_DEPENDENT_DDL(:1, :2, :3) FROM dual",
        &[&ty, &base, &owner],
    ) {
        Ok(v) => Ok(v.map(clean).filter(|s| !s.is_empty())),
        Err(e) if ora_code(&e).is_some_and(|c| NOT_FOUND_CODES.contains(&c)) => Ok(None),
        Err(e) => Err(e),
    }
}

fn permission_hint(e: Error) -> Error {
    match ora_code(&e) {
        Some(31600..=31699) | Some(942) | Some(1031) => Error::Permission(format!(
            "{} (reading another schema's DDL needs the SELECT_CATALOG_ROLE role)",
            e.message()
        )),
        _ => e,
    }
}

/// Metadata types that make up the DDL of an object kind.
fn metadata_types(kind: ObjectKind) -> Result<&'static [&'static str]> {
    Ok(match kind {
        ObjectKind::Table => &["TABLE"],
        ObjectKind::View => &["VIEW"],
        ObjectKind::MaterializedView => &["MATERIALIZED_VIEW"],
        ObjectKind::Sequence => &["SEQUENCE"],
        ObjectKind::Synonym => &["SYNONYM"],
        ObjectKind::Function => &["FUNCTION"],
        ObjectKind::Procedure => &["PROCEDURE"],
        ObjectKind::Package => &["PACKAGE_SPEC", "PACKAGE_BODY"],
        ObjectKind::Type => &["TYPE_SPEC", "TYPE_BODY"],
        ObjectKind::Trigger => &["TRIGGER"],
        ObjectKind::Index => &["INDEX"],
        _ => return Err(Error::Unsupported("object_ddl".into())),
    })
}

/// DDL that recreates the object. For a table this includes its indexes, comments and foreign
/// keys.
pub fn object_ddl(conn: &Connection, schema: &str, kind: ObjectKind, name: &str) -> Result<String> {
    let types = metadata_types(kind)?;
    set_transforms(conn).map_err(permission_hint)?;
    let mut parts: Vec<String> = Vec::new();
    for ty in types {
        match get_ddl(conn, ty, name, schema).map_err(permission_hint)? {
            Some(ddl) => parts.push(ddl),
            // A package without body, a type without body.
            None if types.len() > 1 && !parts.is_empty() => {}
            None => {}
        }
    }
    if parts.is_empty() {
        return Err(Error::NotFound(format!("{schema}.{name}")));
    }
    if matches!(kind, ObjectKind::Table | ObjectKind::View) {
        for (ty, base) in standalone_post_data(kind) {
            if ty == "INDEX" {
                for idx in standalone_indexes(conn, schema, name)? {
                    if let Some(d) =
                        get_ddl(conn, "INDEX", &idx, schema).map_err(permission_hint)?
                    {
                        parts.push(d);
                    }
                }
                continue;
            }
            if ty == "REF_CONSTRAINT" && kind != ObjectKind::Table {
                continue;
            }
            if let Some(d) = get_dependent_ddl(conn, ty, base.unwrap_or(name), schema)
                .map_err(permission_hint)?
            {
                parts.push(d);
            }
        }
    }
    Ok(parts.join("\n\n"))
}

fn standalone_post_data(kind: ObjectKind) -> Vec<(&'static str, Option<&'static str>)> {
    match kind {
        ObjectKind::Table => vec![("INDEX", None), ("COMMENT", None), ("REF_CONSTRAINT", None)],
        _ => vec![("COMMENT", None)],
    }
}

/// Indexes of a table that are not created implicitly by a primary key / unique constraint.
fn standalone_indexes(conn: &Connection, schema: &str, table: &str) -> Result<Vec<String>> {
    Ok(rows(
        conn,
        "SELECT i.index_name FROM all_indexes i \
         WHERE i.table_owner = :1 AND i.table_name = :2 AND i.index_name NOT LIKE 'BIN$%' \
           AND NOT EXISTS (SELECT 1 FROM all_constraints c \
                           WHERE c.owner = i.owner AND c.index_name = i.index_name \
                             AND c.constraint_type IN ('P', 'U')) \
           AND i.index_type NOT IN ('LOB', 'IOT - TOP') \
         ORDER BY i.index_name",
        &[&schema, &table],
    )?
    .iter()
    .map(|r| cell(r, 0))
    .collect())
}

/// Objects of a schema in creation order rank.
fn rank(kind: ObjectKind) -> u8 {
    match kind {
        ObjectKind::Type => 0,
        ObjectKind::Sequence => 1,
        ObjectKind::Table => 2,
        ObjectKind::View | ObjectKind::MaterializedView => 3,
        ObjectKind::Synonym => 4,
        ObjectKind::Function => 5,
        ObjectKind::Procedure => 6,
        ObjectKind::Package => 7,
        _ => 8,
    }
}

fn dictionary_kind(ty: &str) -> Option<ObjectKind> {
    Some(match ty {
        "TABLE" => ObjectKind::Table,
        "VIEW" => ObjectKind::View,
        "MATERIALIZED VIEW" => ObjectKind::MaterializedView,
        "SEQUENCE" => ObjectKind::Sequence,
        "SYNONYM" => ObjectKind::Synonym,
        "FUNCTION" => ObjectKind::Function,
        "PROCEDURE" => ObjectKind::Procedure,
        "PACKAGE" | "PACKAGE BODY" => ObjectKind::Package,
        "TYPE" | "TYPE BODY" => ObjectKind::Type,
        "TRIGGER" => ObjectKind::Trigger,
        _ => return None,
    })
}

/// Which kinds are enabled by the options.
fn wanted(kind: ObjectKind, o: &DdlOptions) -> bool {
    match kind {
        ObjectKind::Table | ObjectKind::Synonym => true,
        ObjectKind::View | ObjectKind::MaterializedView => o.include_views,
        ObjectKind::Sequence => o.include_sequences,
        ObjectKind::Function | ObjectKind::Procedure | ObjectKind::Package | ObjectKind::Type => {
            o.include_routines
        }
        ObjectKind::Trigger => o.include_triggers,
        _ => false,
    }
}

/// Objects of `schema` (name, kind), without system generated ones.
fn list_objects(conn: &Connection, schema: &str) -> Result<Vec<(String, ObjectKind)>> {
    let list = rows(
        conn,
        "SELECT o.object_name, o.object_type FROM all_objects o \
         WHERE o.owner = :1 AND o.generated = 'N' AND o.object_name NOT LIKE 'BIN$%' \
           AND o.object_type IN ('TABLE','VIEW','MATERIALIZED VIEW','SEQUENCE','SYNONYM', \
                                 'FUNCTION','PROCEDURE','PACKAGE','TYPE','TRIGGER') \
           AND NOT (o.object_type = 'TABLE' AND (EXISTS \
                    (SELECT 1 FROM all_mviews m WHERE m.owner = o.owner AND m.mview_name = o.object_name) \
                 OR EXISTS (SELECT 1 FROM all_tables t WHERE t.owner = o.owner \
                    AND t.table_name = o.object_name \
                    AND (t.nested = 'YES' OR t.secondary = 'Y' \
                         OR t.iot_type IN ('IOT_OVERFLOW', 'IOT_MAPPING')))))",
        &[&schema],
    )?;
    Ok(list
        .iter()
        .filter_map(|r| dictionary_kind(&cell(r, 1)).map(|k| (cell(r, 0), k)))
        .collect())
}

/// An object (name, kind) and the object it depends on, inside one schema.
type Dependency = ((String, ObjectKind), (String, ObjectKind));

/// Dependencies between the objects of one schema.
fn dependencies(conn: &Connection, schema: &str) -> Result<Vec<Dependency>> {
    let list = rows(
        conn,
        "SELECT name, type, referenced_name, referenced_type FROM all_dependencies \
         WHERE owner = :1 AND referenced_owner = :1 AND name <> referenced_name \
           AND referenced_type IN ('TABLE','VIEW','MATERIALIZED VIEW','SEQUENCE','SYNONYM', \
                                   'FUNCTION','PROCEDURE','PACKAGE','TYPE')",
        &[&schema],
    )?;
    Ok(list
        .iter()
        .filter_map(|r| {
            let a = dictionary_kind(&cell(r, 1))?;
            let b = dictionary_kind(&cell(r, 3))?;
            Some(((cell(r, 0), a), (cell(r, 2), b)))
        })
        .collect())
}

/// Order `items` so that dependencies come first; ties by category rank then name. Cycles (two
/// packages calling each other) are broken at the second visit.
pub fn order_by_dependencies(
    items: &[(String, ObjectKind)],
    deps: &[Dependency],
) -> Vec<(String, ObjectKind)> {
    let mut sorted = items.to_vec();
    sorted.sort_by(|a, b| (rank(a.1), &a.0).cmp(&(rank(b.1), &b.0)));
    let present: HashSet<&(String, ObjectKind)> = items.iter().collect();
    let mut graph: HashMap<&(String, ObjectKind), Vec<&(String, ObjectKind)>> = HashMap::new();
    for (from, to) in deps {
        if present.contains(from) && present.contains(to) {
            graph.entry(from).or_default().push(to);
        }
    }
    let mut out = Vec::with_capacity(items.len());
    let mut done: HashSet<(String, ObjectKind)> = HashSet::new();
    let mut visiting: HashSet<(String, ObjectKind)> = HashSet::new();

    fn visit(
        node: &(String, ObjectKind),
        graph: &HashMap<&(String, ObjectKind), Vec<&(String, ObjectKind)>>,
        done: &mut HashSet<(String, ObjectKind)>,
        visiting: &mut HashSet<(String, ObjectKind)>,
        out: &mut Vec<(String, ObjectKind)>,
    ) {
        if done.contains(node) || visiting.contains(node) {
            return;
        }
        visiting.insert(node.clone());
        if let Some(children) = graph.get(node) {
            let mut children = children.clone();
            children.sort_by(|a, b| (rank(a.1), &a.0).cmp(&(rank(b.1), &b.0)));
            for c in children {
                visit(c, graph, done, visiting, out);
            }
        }
        visiting.remove(node);
        done.insert(node.clone());
        out.push(node.clone());
    }
    for node in &sorted {
        visit(node, &graph, &mut done, &mut visiting, &mut out);
    }
    out
}

fn drop_statement(schema: &str, kind: ObjectKind, name: &str) -> Option<String> {
    let q = format!("\"{schema}\".\"{name}\"");
    Some(match kind {
        ObjectKind::Table => format!("DROP TABLE {q} CASCADE CONSTRAINTS PURGE;"),
        ObjectKind::View => format!("DROP VIEW {q};"),
        ObjectKind::MaterializedView => format!("DROP MATERIALIZED VIEW {q};"),
        ObjectKind::Sequence => format!("DROP SEQUENCE {q};"),
        ObjectKind::Synonym => format!("DROP SYNONYM {q};"),
        ObjectKind::Function => format!("DROP FUNCTION {q};"),
        ObjectKind::Procedure => format!("DROP PROCEDURE {q};"),
        ObjectKind::Package => format!("DROP PACKAGE {q};"),
        ObjectKind::Type => format!("DROP TYPE {q} FORCE;"),
        ObjectKind::Trigger => format!("DROP TRIGGER {q};"),
        _ => return None,
    })
}

/// Remove the `"OWNER".` qualification.
fn unqualify(sql: &str, owner: &str) -> String {
    sql.replace(&format!("\"{owner}\"."), "")
}

/// DDL of the objects selected by `req` in `schema`, pre-data first (dependency ordered), then
/// the post-data section.
pub fn generate(conn: &Connection, req: &DdlRequest, schema: &str) -> Result<Vec<DdlItem>> {
    let o = &req.options;
    set_transforms(conn).map_err(permission_hint)?;
    let mut selected: Vec<(String, ObjectKind)> = Vec::new();
    if req.objects.is_empty() {
        selected = list_objects(conn, schema)?
            .into_iter()
            .filter(|(_, k)| wanted(*k, o))
            .collect();
    } else {
        for path in &req.objects {
            let (s, kind, name) = schema_object_of(path)?;
            if s != schema {
                return Err(Error::query("all objects must belong to the scope schema"));
            }
            selected.push((name, kind));
        }
    }
    let deps = dependencies(conn, schema)?;
    // Triggers are post-data; the rest is ordered by dependencies.
    let (triggers, pre): (Vec<_>, Vec<_>) = selected
        .iter()
        .cloned()
        .partition(|(_, k)| *k == ObjectKind::Trigger);
    let ordered = order_by_dependencies(&pre, &deps);
    let in_scope: HashSet<(String, ObjectKind)> = selected.iter().cloned().collect();
    let mut items: Vec<DdlItem> = Vec::new();
    let mut post: Vec<DdlItem> = Vec::new();

    let finish = |sql: String| -> String {
        if o.qualify_schema {
            sql
        } else {
            unqualify(&sql, schema)
        }
    };
    for (name, kind) in &ordered {
        let types = metadata_types(*kind)?;
        let mut texts = Vec::new();
        for ty in types {
            if let Some(d) = get_ddl(conn, ty, name, schema).map_err(permission_hint)? {
                texts.push(d);
            }
        }
        if texts.is_empty() {
            continue;
        }
        let mut sql = texts.join("\n\n");
        if o.include_drop
            && let Some(d) = drop_statement(schema, *kind, name)
        {
            sql = format!("{d}\n\n{sql}");
        }
        let depends_on: Vec<ObjectPath> = deps
            .iter()
            .filter(|(from, to)| from.0 == *name && from.1 == *kind && in_scope.contains(to))
            .map(|(_, to)| object_path(schema, to.1, &to.0))
            .collect();
        let path = object_path(schema, *kind, name);
        items.push(DdlItem {
            path: path.clone(),
            kind: *kind,
            section: DdlSection::PreData,
            sql: finish(sql),
            language: DdlLanguage::Sql,
            depends_on,
        });
        if matches!(
            *kind,
            ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView
        ) {
            post_data_of(conn, schema, name, *kind, o, &in_scope, &finish, &mut post)?;
        }
    }
    for (name, kind) in &triggers {
        if let Some(d) = get_ddl(conn, "TRIGGER", name, schema).map_err(permission_hint)? {
            post.push(DdlItem {
                path: object_path(schema, *kind, name),
                kind: *kind,
                section: DdlSection::PostData,
                sql: finish(d),
                language: DdlLanguage::Sql,
                depends_on: Vec::new(),
            });
        }
    }
    items.extend(post);
    Ok(items)
}

/// Indexes, foreign keys, comments and grants of one relation.
#[allow(clippy::too_many_arguments)]
fn post_data_of(
    conn: &Connection,
    schema: &str,
    name: &str,
    kind: ObjectKind,
    o: &DdlOptions,
    in_scope: &HashSet<(String, ObjectKind)>,
    finish: &dyn Fn(String) -> String,
    out: &mut Vec<DdlItem>,
) -> Result<()> {
    let table_path = object_path(schema, kind, name);
    let mut push = |kind: ObjectKind, path: ObjectPath, sql: String, deps: Vec<ObjectPath>| {
        out.push(DdlItem {
            path,
            kind,
            section: DdlSection::PostData,
            sql: finish(sql),
            language: DdlLanguage::Sql,
            depends_on: deps,
        });
    };
    if o.include_indexes && kind != ObjectKind::View {
        for idx in standalone_indexes(conn, schema, name)? {
            if let Some(d) = get_ddl(conn, "INDEX", &idx, schema).map_err(permission_hint)? {
                push(
                    ObjectKind::Index,
                    table_path
                        .child(ObjectKind::Folder, "indexes")
                        .child(ObjectKind::Index, idx),
                    d,
                    vec![table_path.clone()],
                );
            }
        }
    }
    if o.include_foreign_keys && kind == ObjectKind::Table {
        let fks = rows(
            conn,
            "SELECT c.constraint_name, rc.owner, rc.table_name FROM all_constraints c \
             JOIN all_constraints rc ON rc.owner = c.r_owner AND rc.constraint_name = c.r_constraint_name \
             WHERE c.owner = :1 AND c.table_name = :2 AND c.constraint_type = 'R' \
             ORDER BY c.constraint_name",
            &[&schema, &name],
        )?;
        for fk in fks {
            let cname = cell(&fk, 0);
            let Some(d) =
                get_ddl(conn, "REF_CONSTRAINT", &cname, schema).map_err(permission_hint)?
            else {
                continue;
            };
            let mut deps = vec![table_path.clone()];
            let target = (cell(&fk, 2), ObjectKind::Table);
            if cell(&fk, 1) == schema && in_scope.contains(&target) {
                deps.push(object_path(schema, ObjectKind::Table, &target.0));
            }
            push(
                ObjectKind::Table,
                table_path
                    .child(ObjectKind::Folder, "constraints")
                    .child(ObjectKind::Table, cname),
                d,
                deps,
            );
        }
    }
    if o.include_comments
        && let Some(d) =
            get_dependent_ddl(conn, "COMMENT", name, schema).map_err(permission_hint)?
    {
        push(
            kind,
            table_path
                .child(ObjectKind::Folder, "comments")
                .child(kind, name),
            d,
            vec![table_path.clone()],
        );
    }
    if o.include_grants
        && let Some(d) =
            get_dependent_ddl(conn, "OBJECT_GRANT", name, schema).map_err(permission_hint)?
    {
        push(
            kind,
            table_path
                .child(ObjectKind::Folder, "grants")
                .child(kind, name),
            d,
            vec![table_path.clone()],
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(name: &str, kind: ObjectKind) -> (String, ObjectKind) {
        (name.to_string(), kind)
    }

    #[test]
    fn ordering_puts_dependencies_first() {
        let items = vec![
            n("V1", ObjectKind::View),
            n("T_CHILD", ObjectKind::Table),
            n("T_PARENT", ObjectKind::Table),
            n("SEQ", ObjectKind::Sequence),
            n("PK", ObjectKind::Package),
            n("F1", ObjectKind::Function),
            n("TY", ObjectKind::Type),
        ];
        let deps = vec![
            (n("V1", ObjectKind::View), n("T_CHILD", ObjectKind::Table)),
            (n("F1", ObjectKind::Function), n("V1", ObjectKind::View)),
            (n("PK", ObjectKind::Package), n("F1", ObjectKind::Function)),
            (n("T_CHILD", ObjectKind::Table), n("TY", ObjectKind::Type)),
        ];
        let out = order_by_dependencies(&items, &deps);
        let pos = |name: &str| out.iter().position(|(x, _)| x == name).unwrap();
        assert_eq!(out.len(), items.len());
        assert!(pos("TY") < pos("T_CHILD"));
        assert!(pos("T_CHILD") < pos("V1"));
        assert!(pos("V1") < pos("F1"));
        assert!(pos("F1") < pos("PK"));
        // Rank decides the rest: types, sequences, tables first.
        assert_eq!(out[0].0, "TY");
        assert!(pos("SEQ") < pos("T_CHILD"));
    }

    #[test]
    fn cycles_do_not_loop() {
        let items = vec![n("A", ObjectKind::Package), n("B", ObjectKind::Package)];
        let deps = vec![
            (n("A", ObjectKind::Package), n("B", ObjectKind::Package)),
            (n("B", ObjectKind::Package), n("A", ObjectKind::Package)),
        ];
        let out = order_by_dependencies(&items, &deps);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn drop_statements_and_unqualify() {
        assert_eq!(
            drop_statement("HR", ObjectKind::Table, "T").unwrap(),
            "DROP TABLE \"HR\".\"T\" CASCADE CONSTRAINTS PURGE;"
        );
        assert!(drop_statement("HR", ObjectKind::Column, "C").is_none());
        assert_eq!(
            unqualify("CREATE TABLE \"HR\".\"T\" (a number)", "HR"),
            "CREATE TABLE \"T\" (a number)"
        );
    }

    #[test]
    fn kind_filters_follow_options() {
        let o = DdlOptions {
            include_views: false,
            include_triggers: false,
            ..DdlOptions::default()
        };
        assert!(wanted(ObjectKind::Table, &o));
        assert!(!wanted(ObjectKind::View, &o));
        assert!(!wanted(ObjectKind::Trigger, &o));
        assert!(wanted(ObjectKind::Package, &o));
    }

    #[test]
    fn ora_codes_are_read_from_messages() {
        let e = Error::Query {
            message: "ORA-31603: object \"X\" of type TABLE not found in schema \"HR\"".into(),
            position: None,
        };
        assert_eq!(ora_code(&e), Some(31603));
        assert_eq!(ora_code(&Error::query("plain")), None);
    }
}
