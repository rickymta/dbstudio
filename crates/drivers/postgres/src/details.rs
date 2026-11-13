//! Table metadata for the structure view (`table_details`) and path resolution helpers.

use dbstudio_core::{
    ColumnDetail, Error, ForeignKeyDetail, IndexDetail, ObjectKind, ObjectPath, Result,
    TableDetails, ValueKind,
};
use tokio_postgres::Client;
use tokio_postgres::types::Type;

use crate::browse::object_path;
use crate::error::map_error;
use crate::sql::non_folder_segments;

/// A table-like object located by a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relation {
    pub database: String,
    pub schema: String,
    pub name: String,
    pub kind: ObjectKind,
}

/// Extract database, schema and name of a table, view or materialized view from `path`
/// (folder segments are ignored).
pub fn resolve_relation(path: &ObjectPath) -> Result<Relation> {
    let segs = non_folder_segments(path);
    match segs.as_slice() {
        [
            (ObjectKind::Database, db),
            (ObjectKind::Schema, schema),
            (kind @ (ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView), name),
        ] => Ok(Relation {
            database: (*db).to_string(),
            schema: (*schema).to_string(),
            name: (*name).to_string(),
            kind: *kind,
        }),
        _ => Err(Error::NotFound(
            "path does not point to a table, view or materialized view".to_string(),
        )),
    }
}

fn fk_action(code: &str) -> Option<String> {
    Some(
        match code {
            "a" => "NO ACTION",
            "r" => "RESTRICT",
            "c" => "CASCADE",
            "n" => "SET NULL",
            "d" => "SET DEFAULT",
            _ => return None,
        }
        .to_string(),
    )
}

fn kind_of_relkind(relkind: &str) -> ObjectKind {
    match relkind {
        "v" => ObjectKind::View,
        "m" => ObjectKind::MaterializedView,
        _ => ObjectKind::Table,
    }
}

/// Columns of the primary key, else of the smallest unique index whose columns are all
/// `NOT NULL`; empty for views and tables without such a key.
pub async fn key_columns(client: &Client, schema: &str, name: &str) -> Result<Vec<String>> {
    let rows = client
        .query(
            "WITH idx AS ( \
                SELECT i.indrelid, i.indkey::int2[] AS keys \
                FROM pg_index i \
                JOIN pg_class c ON c.oid = i.indrelid \
                JOIN pg_namespace n ON n.oid = c.relnamespace \
                JOIN pg_class ic ON ic.oid = i.indexrelid \
                WHERE n.nspname = $1 AND c.relname = $2 AND i.indisvalid \
                  AND i.indpred IS NULL AND i.indexprs IS NULL \
                  AND (i.indisprimary OR (i.indisunique AND NOT EXISTS ( \
                        SELECT 1 FROM unnest(i.indkey::int2[]) u(k) \
                        JOIN pg_attribute a2 ON a2.attrelid = i.indrelid AND a2.attnum = u.k \
                        WHERE NOT a2.attnotnull))) \
                ORDER BY i.indisprimary DESC, i.indnatts, ic.relname LIMIT 1) \
             SELECT a.attname::text \
             FROM idx CROSS JOIN LATERAL unnest(idx.keys) WITH ORDINALITY k(attnum, ord) \
             JOIN pg_attribute a ON a.attrelid = idx.indrelid AND a.attnum = k.attnum \
             ORDER BY k.ord",
            &[&schema, &name],
        )
        .await
        .map_err(map_error)?;
    Ok(rows.iter().map(|r| r.get(0)).collect())
}

pub async fn table_details(
    client: &Client,
    path: &ObjectPath,
    database: &str,
) -> Result<TableDetails> {
    let rel = resolve_relation(path)?;
    let header = client
        .query_opt(
            "SELECT c.oid, c.relkind::text, obj_description(c.oid, 'pg_class'), c.reltuples::int8, \
                    CASE WHEN c.relkind IN ('r', 'p', 'm') THEN pg_total_relation_size(c.oid) END \
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relname = $2 AND c.relkind IN ('r', 'p', 'f', 'v', 'm')",
            &[&rel.schema, &rel.name],
        )
        .await
        .map_err(map_error)?
        .ok_or_else(|| Error::NotFound(format!("{}.{} not found", rel.schema, rel.name)))?;
    let oid: u32 = header.get(0);
    let relkind: String = header.get(1);
    let comment: Option<String> = header.get(2);
    let reltuples: i64 = header.get(3);
    let size: Option<i64> = header.get(4);

    let column_rows = client
        .query(
            "SELECT a.attname::text, a.attnum::int4, format_type(a.atttypid, a.atttypmod), \
                    a.attnotnull, pg_get_expr(d.adbin, d.adrelid), \
                    col_description(a.attrelid, a.attnum), a.attidentity::text, \
                    a.attgenerated::text, \
                    EXISTS (SELECT 1 FROM pg_index i WHERE i.indrelid = a.attrelid \
                            AND i.indisprimary AND a.attnum = ANY (i.indkey)), \
                    CASE WHEN t.typtype = 'd' THEN t.typbasetype ELSE t.oid END \
             FROM pg_attribute a \
             JOIN pg_type t ON t.oid = a.atttypid \
             LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
             WHERE a.attrelid = $1 AND a.attnum > 0 AND NOT a.attisdropped \
             ORDER BY a.attnum",
            &[&oid],
        )
        .await
        .map_err(map_error)?;
    let columns = column_rows
        .iter()
        .map(|r| {
            let identity: String = r.get(6);
            let generated: String = r.get(7);
            let extra = match (identity.as_str(), generated.as_str()) {
                ("a", _) => Some("identity always".to_string()),
                ("d", _) => Some("identity by default".to_string()),
                (_, "s") => Some("generated stored".to_string()),
                _ => None,
            };
            let type_oid: u32 = r.get(9);
            ColumnDetail {
                name: r.get(0),
                ordinal: r.get::<_, i32>(1).max(0) as u32,
                type_name: r.get(2),
                value_kind: Type::from_oid(type_oid)
                    .map(|t| crate::decode::value_kind(&t))
                    .unwrap_or(ValueKind::Text),
                nullable: !r.get::<_, bool>(3),
                default: r.get(4),
                comment: r.get(5),
                primary_key: r.get(8),
                extra,
            }
        })
        .collect();

    let index_rows = client
        .query(
            "SELECT ic.relname::text, i.indisunique, i.indisprimary, am.amname::text, \
                    pg_get_indexdef(i.indexrelid), \
                    ARRAY(SELECT CASE WHEN i.indkey[k - 1] = 0 \
                                      THEN '(' || pg_get_indexdef(i.indexrelid, k, true) || ')' \
                                      ELSE (SELECT a.attname::text FROM pg_attribute a \
                                            WHERE a.attrelid = i.indrelid \
                                              AND a.attnum = i.indkey[k - 1]) END \
                          FROM generate_series(1, i.indnkeyatts::int4) k ORDER BY k)::text[], \
                    i.indpred IS NOT NULL \
             FROM pg_index i \
             JOIN pg_class ic ON ic.oid = i.indexrelid \
             JOIN pg_am am ON am.oid = ic.relam \
             WHERE i.indrelid = $1 \
             ORDER BY i.indisprimary DESC, ic.relname",
            &[&oid],
        )
        .await
        .map_err(map_error)?;
    let indexes = index_rows
        .iter()
        .map(|r| IndexDetail {
            name: r.get(0),
            unique: r.get(1),
            primary: r.get(2),
            method: Some(r.get(3)),
            definition: Some(r.get(4)),
            columns: r.get(5),
            partial: r.get(6),
        })
        .collect();

    let fk_rows = client
        .query(
            "SELECT c.conname::text, \
                    ARRAY(SELECT a.attname::text \
                          FROM unnest(c.conkey) WITH ORDINALITY k(attnum, ord) \
                          JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = k.attnum \
                          ORDER BY k.ord)::text[], \
                    rn.nspname::text, rc.relname::text, \
                    ARRAY(SELECT a.attname::text \
                          FROM unnest(c.confkey) WITH ORDINALITY k(attnum, ord) \
                          JOIN pg_attribute a ON a.attrelid = c.confrelid AND a.attnum = k.attnum \
                          ORDER BY k.ord)::text[], \
                    c.confupdtype::text, c.confdeltype::text, rc.relkind::text \
             FROM pg_constraint c \
             JOIN pg_class rc ON rc.oid = c.confrelid \
             JOIN pg_namespace rn ON rn.oid = rc.relnamespace \
             WHERE c.conrelid = $1 AND c.contype = 'f' \
             ORDER BY c.conname",
            &[&oid],
        )
        .await
        .map_err(map_error)?;
    let foreign_keys = fk_rows
        .iter()
        .map(|r| {
            let ref_schema: String = r.get(2);
            let ref_table: String = r.get(3);
            let ref_kind = kind_of_relkind(&r.get::<_, String>(7));
            ForeignKeyDetail {
                name: r.get(0),
                columns: r.get(1),
                referenced_table: object_path(database, &ref_schema, ref_kind, &ref_table),
                referenced_columns: r.get(4),
                on_update: fk_action(&r.get::<_, String>(5)),
                on_delete: fk_action(&r.get::<_, String>(6)),
            }
        })
        .collect();

    Ok(TableDetails {
        path: path.clone(),
        kind: kind_of_relkind(&relkind),
        comment,
        columns,
        indexes,
        foreign_keys,
        estimated_rows: (reltuples >= 0).then_some(reltuples as u64),
        size_bytes: size.map(|s| s.max(0) as u64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_paths_with_and_without_folders() {
        let full = ObjectPath::root()
            .child(ObjectKind::Database, "d")
            .child(ObjectKind::Folder, "schemas")
            .child(ObjectKind::Schema, "s")
            .child(ObjectKind::Folder, "tables")
            .child(ObjectKind::Table, "My \"T\"");
        let rel = resolve_relation(&full).unwrap();
        assert_eq!(
            rel,
            Relation {
                database: "d".into(),
                schema: "s".into(),
                name: "My \"T\"".into(),
                kind: ObjectKind::Table
            }
        );
        let short = ObjectPath::root()
            .child(ObjectKind::Database, "d")
            .child(ObjectKind::Schema, "s")
            .child(ObjectKind::View, "v");
        assert_eq!(resolve_relation(&short).unwrap().kind, ObjectKind::View);
        let bad = ObjectPath::root().child(ObjectKind::Table, "t");
        assert!(matches!(resolve_relation(&bad), Err(Error::NotFound(_))));
        let col = full
            .child(ObjectKind::Folder, "columns")
            .child(ObjectKind::Column, "c");
        assert!(resolve_relation(&col).is_err());
    }

    #[test]
    fn fk_actions() {
        assert_eq!(fk_action("c").as_deref(), Some("CASCADE"));
        assert_eq!(fk_action("a").as_deref(), Some("NO ACTION"));
        assert_eq!(fk_action("x"), None);
    }
}
