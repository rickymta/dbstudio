//! `schema_model`: tables, columns and foreign keys of a scope with a handful of dictionary
//! queries (tables, columns, constraints) instead of one set of queries per table.

use std::collections::{BTreeMap, BTreeSet};

use dbstudio_core::{
    ColumnDetail, ModelDetail, ObjectKind, ObjectPath, Relation, Result, SchemaModel,
    SchemaModelRequest, TableModel,
};
use oracle::Connection;

use crate::catalog::{cell, object_path, rows, type_display, value_kind_of};
use crate::sql::non_folder_segments;

/// Schemas covered by a scope: one schema, or every visible non-system one for the root.
pub fn scope_schemas(conn: &Connection, scope: &ObjectPath) -> Result<Vec<String>> {
    use ObjectKind as K;
    match non_folder_segments(scope).as_slice() {
        [(K::Schema, s)] => Ok(vec![(*s).to_string()]),
        [] => {
            let list = rows(
                conn,
                "SELECT u.username FROM all_users u \
                 WHERE u.oracle_maintained = 'N' AND EXISTS \
                   (SELECT 1 FROM all_tables t WHERE t.owner = u.username) \
                 ORDER BY u.username",
                &[],
            )?;
            Ok(list.iter().map(|r| cell(r, 0)).collect())
        }
        _ => Err(dbstudio_core::Error::NotFound(
            "the scope of a schema model must be the root or a schema".into(),
        )),
    }
}

fn table_key(owner: &str, name: &str) -> (String, String) {
    (owner.to_string(), name.to_string())
}

/// Owner, name, kind, comment and estimated rows of a relation.
type TableRow = (String, String, ObjectKind, Option<String>, Option<u64>);

struct Cons {
    kind: String,
    columns: Vec<String>,
    r_owner: Option<String>,
    r_table: Option<String>,
    r_columns: Vec<String>,
    delete_rule: Option<String>,
}

/// Build the model of `schemas`.
pub fn build(
    conn: &Connection,
    req: &SchemaModelRequest,
    schemas: &[String],
) -> Result<SchemaModel> {
    let wanted: BTreeSet<(String, String)> = req
        .tables
        .iter()
        .filter_map(|p| crate::sql::relation_of(p).ok())
        .collect();
    let mut tables: Vec<TableRow> = Vec::new();
    let mut columns: BTreeMap<(String, String), Vec<ColumnDetail>> = BTreeMap::new();
    let mut constraints: BTreeMap<(String, String), Vec<(String, Cons)>> = BTreeMap::new();

    for owner in schemas {
        let owner_ref = owner.as_str();
        // Tables (and views on request) with their comments.
        let list = rows(
            conn,
            "SELECT t.table_name, 'TABLE', t.num_rows, c.comments FROM all_tables t \
             LEFT JOIN all_tab_comments c ON c.owner = t.owner AND c.table_name = t.table_name \
             WHERE t.owner = :1 AND t.secondary = 'N' AND t.nested = 'NO' \
               AND (t.iot_type IS NULL OR t.iot_type = 'IOT') AND t.table_name NOT LIKE 'BIN$%' \
               AND NOT EXISTS (SELECT 1 FROM all_mviews m WHERE m.owner = t.owner \
                               AND m.mview_name = t.table_name) \
             ORDER BY t.table_name",
            &[&owner_ref],
        )?;
        for r in &list {
            tables.push((
                owner.clone(),
                cell(r, 0),
                ObjectKind::Table,
                r[3].clone().filter(|c| !c.is_empty()),
                r[2].as_deref().and_then(|n| n.trim().parse().ok()),
            ));
        }
        if req.include_views {
            let views = rows(
                conn,
                "SELECT v.view_name, c.comments FROM all_views v \
                 LEFT JOIN all_tab_comments c ON c.owner = v.owner AND c.table_name = v.view_name \
                 WHERE v.owner = :1 ORDER BY v.view_name",
                &[&owner_ref],
            )?;
            for r in &views {
                tables.push((
                    owner.clone(),
                    cell(r, 0),
                    ObjectKind::View,
                    r[1].clone().filter(|c| !c.is_empty()),
                    None,
                ));
            }
        }
        // Columns of every relation of the owner at once.
        let cols = rows(
            conn,
            "SELECT c.table_name, c.column_name, c.column_id, c.data_type, c.data_type_owner, \
                    c.data_length, c.char_length, c.char_used, c.data_precision, c.data_scale, \
                    c.nullable, c.data_default, cc.comments \
             FROM all_tab_cols c \
             LEFT JOIN all_col_comments cc ON cc.owner = c.owner AND cc.table_name = c.table_name \
                  AND cc.column_name = c.column_name \
             WHERE c.owner = :1 AND c.hidden_column = 'NO' AND c.column_id IS NOT NULL \
             ORDER BY c.table_name, c.column_id",
            &[&owner_ref],
        )?;
        for r in &cols {
            let num = |i: usize| r[i].as_deref().and_then(|v| v.trim().parse::<i64>().ok());
            let data_type = cell(r, 3);
            columns
                .entry(table_key(owner, &cell(r, 0)))
                .or_default()
                .push(ColumnDetail {
                    name: cell(r, 1),
                    ordinal: u32::try_from(num(2).unwrap_or(0)).unwrap_or(0),
                    type_name: type_display(
                        &data_type,
                        r[4].as_deref(),
                        num(5),
                        num(6),
                        r[7].as_deref(),
                        num(8),
                        num(9),
                    ),
                    value_kind: value_kind_of(&data_type, num(8), num(9)),
                    nullable: cell(r, 10) != "N",
                    default: r[11]
                        .as_deref()
                        .map(|d| d.trim().to_string())
                        .filter(|d| !d.is_empty()),
                    comment: if req.include_comments {
                        r[12].clone().filter(|c| !c.is_empty())
                    } else {
                        None
                    },
                    primary_key: false,
                    extra: None,
                });
        }
        // Primary, unique and foreign key constraints with their columns.
        let cons = rows(
            conn,
            "SELECT c.table_name, c.constraint_name, c.constraint_type, cc.column_name, \
                    c.delete_rule, rc.owner, rc.table_name, rcc.column_name \
             FROM all_constraints c \
             JOIN all_cons_columns cc ON cc.owner = c.owner AND cc.constraint_name = c.constraint_name \
             LEFT JOIN all_constraints rc ON rc.owner = c.r_owner \
                  AND rc.constraint_name = c.r_constraint_name \
             LEFT JOIN all_cons_columns rcc ON rcc.owner = rc.owner \
                  AND rcc.constraint_name = rc.constraint_name AND rcc.position = cc.position \
             WHERE c.owner = :1 AND c.constraint_type IN ('P', 'U', 'R') \
             ORDER BY c.table_name, c.constraint_name, cc.position",
            &[&owner_ref],
        )?;
        for r in &cons {
            let entry = constraints
                .entry(table_key(owner, &cell(r, 0)))
                .or_default();
            let cname = cell(r, 1);
            match entry.iter_mut().find(|(n, _)| *n == cname) {
                Some((_, c)) => {
                    c.columns.push(cell(r, 3));
                    if let Some(rc) = r[7].clone() {
                        c.r_columns.push(rc);
                    }
                }
                None => entry.push((
                    cname,
                    Cons {
                        kind: cell(r, 2),
                        columns: vec![cell(r, 3)],
                        r_owner: r[5].clone(),
                        r_table: r[6].clone(),
                        r_columns: r[7].clone().into_iter().collect(),
                        delete_rule: r[4].clone().filter(|d| !d.is_empty()),
                    },
                )),
            }
        }
    }

    let included: BTreeSet<(String, String)> = tables
        .iter()
        .filter(|(o, n, ..)| wanted.is_empty() || wanted.contains(&(o.clone(), n.clone())))
        .map(|(o, n, ..)| (o.clone(), n.clone()))
        .collect();
    let mut models = Vec::new();
    let mut relations = Vec::new();
    for (owner, name, kind, comment, estimated_rows) in &tables {
        let key = (owner.clone(), name.clone());
        if !included.contains(&key) {
            continue;
        }
        let cons = constraints.get(&key).map(Vec::as_slice).unwrap_or(&[]);
        let primary_key: Vec<String> = cons
            .iter()
            .find(|(_, c)| c.kind == "P")
            .map(|(_, c)| c.columns.clone())
            .unwrap_or_default();
        let unique_keys: Vec<Vec<String>> = cons
            .iter()
            .filter(|(_, c)| c.kind == "U")
            .map(|(_, c)| c.columns.clone())
            .collect();
        let mut cols = columns.get(&key).cloned().unwrap_or_default();
        for c in &mut cols {
            c.primary_key = primary_key.contains(&c.name);
        }
        if req.detail == ModelDetail::Keys {
            let mut keep: BTreeSet<&str> = BTreeSet::new();
            keep.extend(primary_key.iter().map(String::as_str));
            for (_, c) in cons {
                keep.extend(c.columns.iter().map(String::as_str));
            }
            cols.retain(|c| keep.contains(c.name.as_str()));
        }
        let from_path = object_path(owner, *kind, name);
        for (cname, c) in cons.iter().filter(|(_, c)| c.kind == "R") {
            let (Some(r_owner), Some(r_table)) = (&c.r_owner, &c.r_table) else {
                continue;
            };
            // Both ends must be in the model (an ERD cannot draw a line to a missing table).
            if !included.contains(&(r_owner.clone(), r_table.clone())) {
                continue;
            }
            let all_cols = columns.get(&key).map(Vec::as_slice).unwrap_or(&[]);
            let from_optional = c.columns.iter().any(|col| {
                all_cols
                    .iter()
                    .find(|d| &d.name == col)
                    .is_none_or(|d| d.nullable)
            });
            let set = |v: &[String]| -> BTreeSet<String> { v.iter().cloned().collect() };
            let fk_set = set(&c.columns);
            let one_to_one = (!primary_key.is_empty() && set(&primary_key) == fk_set)
                || unique_keys.iter().any(|u| set(u) == fk_set);
            relations.push(Relation {
                name: cname.clone(),
                from_table: from_path.clone(),
                from_columns: c.columns.clone(),
                to_table: object_path(r_owner, ObjectKind::Table, r_table),
                to_columns: c.r_columns.clone(),
                on_update: None,
                on_delete: c.delete_rule.clone(),
                from_optional,
                one_to_one,
                inferred: false,
            });
        }
        models.push(TableModel {
            path: from_path,
            kind: *kind,
            schema: Some(owner.clone()),
            name: name.clone(),
            comment: if req.include_comments {
                comment.clone()
            } else {
                None
            },
            columns: cols,
            primary_key,
            unique_keys,
            estimated_rows: *estimated_rows,
        });
    }
    Ok(SchemaModel {
        scope: req.scope.clone(),
        tables: models,
        relations,
        inferred: false,
        truncated: false,
    })
}
