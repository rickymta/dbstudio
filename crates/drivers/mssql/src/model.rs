//! `schema_model`: tables, columns, keys and relations of a database or schema in four catalog
//! queries (columns, indexes, foreign keys, table statistics and comments).

use std::collections::{BTreeMap, BTreeSet};

use dbstudio_core::{
    ColumnDetail, ModelDetail, ObjectKind, ObjectPath, Relation, Result, SchemaModel,
    SchemaModelRequest, TableModel,
};

use crate::browse::column_detail;
use crate::catalog::{self, Col, Fk, Idx, TableKey};
use crate::conn::Conn;
use crate::session::MssqlSession;
use crate::sql::{lit, object_path, parse_object};
use crate::value::{int_at, opt_str_at, str_at};

/// Most tables a model holds.
const MAX_TABLES: usize = 2000;

/// `WHERE` fragment over `sys.objects o` for the tables/views selected by `req`.
fn filter_of(req: &SchemaModelRequest, schema: Option<&str>) -> Result<String> {
    let mut f = catalog::scope(schema, req.include_views);
    if !req.tables.is_empty() {
        let mut parts = Vec::new();
        for t in &req.tables {
            let o = parse_object(t)?;
            parts.push(format!(
                "(o.schema_id = SCHEMA_ID({}) AND o.name = {})",
                lit(&o.schema),
                lit(&o.name)
            ));
        }
        f.push_str(&format!(" AND ({})", parts.join(" OR ")));
    }
    Ok(f)
}

/// Row estimates, kinds and table comments in one query.
async fn stats(
    conn: &mut Conn,
    filter: &str,
) -> Result<BTreeMap<TableKey, (ObjectKind, Option<u64>, Option<String>)>> {
    let rows = conn
        .rows(&format!(
            "SELECT SCHEMA_NAME(o.schema_id), o.name, o.type, \
             (SELECT SUM(p.rows) FROM sys.partitions p WHERE p.object_id = o.object_id AND p.index_id IN (0, 1)), \
             CAST((SELECT TOP 1 ep.value FROM sys.extended_properties ep WHERE ep.class = 1 AND ep.major_id = o.object_id \
                   AND ep.minor_id = 0 AND ep.name = N'MS_Description') AS NVARCHAR(MAX)) \
             FROM sys.objects o WHERE {filter} ORDER BY 1, 2"
        ))
        .await?;
    Ok(rows
        .iter()
        .map(|r| {
            let kind = if str_at(r, 2).trim() == "V" {
                ObjectKind::View
            } else {
                ObjectKind::Table
            };
            let est = match r.get(3) {
                Some(dbstudio_core::Value::Null) | None => None,
                _ => Some(int_at(r, 3).max(0) as u64),
            };
            ((str_at(r, 0), str_at(r, 1)), (kind, est, opt_str_at(r, 4)))
        })
        .collect())
}

fn unique_keys(idx: &[Idx]) -> Vec<Vec<String>> {
    idx.iter()
        .filter(|i| i.unique && !i.primary && i.filter.is_none() && matches!(i.type_id, 1 | 2))
        .map(|i| i.keys.iter().map(|(c, _)| c.clone()).collect())
        .collect()
}

fn relation(db: &str, from: &TableKey, fk: &Fk, cols: &[Col], idx: &[Idx]) -> Relation {
    let nullable = |c: &str| cols.iter().find(|x| x.name == c).is_none_or(|x| x.nullable);
    let set: BTreeSet<&str> = fk.cols.iter().map(String::as_str).collect();
    let same = |keys: &[(String, bool)]| {
        keys.len() == set.len() && keys.iter().all(|(c, _)| set.contains(c.as_str()))
    };
    let one_to_one = idx
        .iter()
        .any(|i| (i.primary || i.unique) && i.filter.is_none() && same(&i.keys));
    Relation {
        name: fk.name.clone(),
        from_table: object_path(db, &from.0, ObjectKind::Table, &from.1),
        from_columns: fk.cols.clone(),
        to_table: object_path(db, &fk.ref_schema, ObjectKind::Table, &fk.ref_table),
        to_columns: fk.ref_cols.clone(),
        on_update: Some(fk.on_update.clone()),
        on_delete: Some(fk.on_delete.clone()),
        from_optional: fk.cols.iter().any(|c| nullable(c)),
        one_to_one,
        inferred: false,
    }
}

pub async fn schema_model(sess: &MssqlSession, req: SchemaModelRequest) -> Result<SchemaModel> {
    let db = sess.database_of(&req.scope);
    let schema = req
        .scope
        .segments
        .iter()
        .find(|s| s.kind == ObjectKind::Schema)
        .map(|s| s.name.clone());
    let filter = filter_of(&req, schema.as_deref())?;
    let mut conn = sess.shared.acquire_catalog(&db).await?;
    let loaded = load(&mut conn, &filter).await;
    conn.release();
    let (stats, cols, idx, fks) = loaded?;

    let mut keys: Vec<&TableKey> = stats.keys().collect();
    keys.sort();
    let truncated = keys.len() > MAX_TABLES;
    keys.truncate(MAX_TABLES);
    let included: BTreeSet<&TableKey> = keys.iter().copied().collect();

    let empty_cols: Vec<Col> = Vec::new();
    let empty_idx: Vec<Idx> = Vec::new();
    let mut tables = Vec::with_capacity(keys.len());
    let mut relations = Vec::new();
    for key in keys {
        let (kind, est, comment) = &stats[key];
        let tcols = cols.get(key).unwrap_or(&empty_cols);
        let tidx = idx.get(key).unwrap_or(&empty_idx);
        let tfks = fks.get(key);
        let pk: Vec<String> = tidx
            .iter()
            .find(|i| i.primary)
            .map(|i| i.keys.iter().map(|(c, _)| c.clone()).collect())
            .unwrap_or_default();
        let uniques = unique_keys(tidx);
        let mut important: BTreeSet<&str> = pk.iter().map(String::as_str).collect();
        for u in &uniques {
            important.extend(u.iter().map(String::as_str));
        }
        for f in tfks.into_iter().flatten() {
            important.extend(f.cols.iter().map(String::as_str));
        }
        let columns: Vec<ColumnDetail> = tcols
            .iter()
            .enumerate()
            .filter(|(_, c)| req.detail == ModelDetail::Full || important.contains(c.name.as_str()))
            .map(|(n, c)| {
                let mut d = column_detail(n, c, pk.contains(&c.name));
                if !req.include_comments {
                    d.comment = None;
                }
                d
            })
            .collect();
        for f in tfks.into_iter().flatten() {
            if included.contains(&(f.ref_schema.clone(), f.ref_table.clone())) {
                relations.push(relation(&db, key, f, tcols, tidx));
            }
        }
        tables.push(TableModel {
            path: object_path(&db, &key.0, *kind, &key.1),
            kind: *kind,
            schema: Some(key.0.clone()),
            name: key.1.clone(),
            comment: if req.include_comments { comment.clone() } else { None },
            columns,
            primary_key: pk,
            unique_keys: uniques,
            estimated_rows: *est,
        });
    }
    let scope: ObjectPath = req.scope.clone();
    Ok(SchemaModel {
        scope,
        tables,
        relations,
        inferred: false,
        truncated,
    })
}

type Loaded = (
    BTreeMap<TableKey, (ObjectKind, Option<u64>, Option<String>)>,
    BTreeMap<TableKey, Vec<Col>>,
    BTreeMap<TableKey, Vec<Idx>>,
    BTreeMap<TableKey, Vec<Fk>>,
);

async fn load(conn: &mut Conn, filter: &str) -> Result<Loaded> {
    let stats = stats(conn, filter).await?;
    let cols = catalog::columns(conn, filter).await?;
    let idx = catalog::indexes(conn, filter).await?;
    let fks = catalog::foreign_keys(conn, filter).await?;
    Ok((stats, cols, idx, fks))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, nullable: bool) -> Col {
        Col {
            id: 1,
            name: name.into(),
            display_type: "int".into(),
            ddl_type: "INT".into(),
            base_type: "int".into(),
            nullable,
            identity: None,
            computed: None,
            default: None,
            collation: None,
            comment: None,
            sparse: false,
            generated: 0,
            rowversion: false,
        }
    }

    fn idx(name: &str, primary: bool, unique: bool, keys: &[&str]) -> Idx {
        Idx {
            name: name.into(),
            type_id: 2,
            type_desc: "NONCLUSTERED".into(),
            unique,
            primary,
            unique_constraint: false,
            filter: None,
            disabled: false,
            keys: keys.iter().map(|k| (k.to_string(), false)).collect(),
            included: vec![],
        }
    }

    #[test]
    fn relation_flags() {
        let fk = Fk {
            name: "fk".into(),
            cols: vec!["a".into()],
            ref_schema: "dbo".into(),
            ref_table: "r".into(),
            ref_cols: vec!["id".into()],
            on_update: "NO ACTION".into(),
            on_delete: "NO ACTION".into(),
            disabled: false,
            not_trusted: false,
        };
        let from = ("dbo".to_string(), "t".to_string());
        let cols = vec![col("a", true)];
        let r = relation("d", &from, &fk, &cols, &[]);
        assert!(r.from_optional && !r.one_to_one);
        let cols = vec![col("a", false)];
        let r = relation("d", &from, &fk, &cols, &[idx("pk", true, true, &["a"])]);
        assert!(!r.from_optional && r.one_to_one);
        assert_eq!(r.to_table, object_path("d", "dbo", ObjectKind::Table, "r"));
    }

    #[test]
    fn unique_key_selection() {
        let mut filtered = idx("f", false, true, &["x"]);
        filtered.filter = Some("([x] IS NOT NULL)".into());
        let all = vec![
            idx("pk", true, true, &["id"]),
            idx("u", false, true, &["code"]),
            idx("plain", false, false, &["z"]),
            filtered,
        ];
        assert_eq!(unique_keys(&all), vec![vec!["code".to_string()]]);
    }
}
