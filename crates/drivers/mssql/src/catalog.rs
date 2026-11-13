//! Catalog readers shared by the object tree, `table_details`, DDL generation and the schema
//! model. Every reader works on the objects selected by a `WHERE` fragment over `sys.objects o`,
//! so one call serves a single table or a whole schema.

use std::collections::BTreeMap;

use dbstudio_core::Result;

use crate::conn::Conn;
use crate::sql::{format_type, lit, quote};
use crate::value::{bool_at, int_at, opt_str_at, str_at};

/// `(schema, name)` of a table or view.
pub type TableKey = (String, String);

/// A column.
#[derive(Debug, Clone)]
pub struct Col {
    pub name: String,
    /// Type name for display (`nvarchar(50)`, `dbo.MyAlias`).
    pub display_type: String,
    /// Type clause for DDL (`NVARCHAR(50)`, `[dbo].[MyAlias]`).
    pub ddl_type: String,
    /// System type behind the column (alias types resolved).
    pub base_type: String,
    pub nullable: bool,
    /// `(seed, increment)`.
    pub identity: Option<(String, String)>,
    /// `(expression, persisted)`.
    pub computed: Option<(String, bool)>,
    /// `(constraint name, definition)`.
    pub default: Option<(String, String)>,
    pub collation: Option<String>,
    pub comment: Option<String>,
    pub sparse: bool,
    /// 1/2 = `GENERATED ALWAYS` row start / end of a temporal table.
    pub generated: i64,
    pub rowversion: bool,
}

/// An index or key constraint.
#[derive(Debug, Clone)]
pub struct Idx {
    pub name: String,
    /// `sys.indexes.type`: 1 clustered, 2 nonclustered, 3 XML, 4 spatial, 5/6 columnstore.
    pub type_id: i64,
    pub type_desc: String,
    pub unique: bool,
    pub primary: bool,
    pub unique_constraint: bool,
    pub filter: Option<String>,
    pub disabled: bool,
    /// Key columns with their descending flag.
    pub keys: Vec<(String, bool)>,
    pub included: Vec<String>,
}

/// A foreign key.
#[derive(Debug, Clone)]
pub struct Fk {
    pub name: String,
    pub cols: Vec<String>,
    pub ref_schema: String,
    pub ref_table: String,
    pub ref_cols: Vec<String>,
    pub on_update: String,
    pub on_delete: String,
    pub disabled: bool,
    pub not_trusted: bool,
}

/// A check constraint.
#[derive(Debug, Clone)]
pub struct Check {
    pub name: String,
    pub definition: String,
}

/// Tables selected by `filter` (`WHERE` fragment over `sys.objects o`; use `o.type IN ('U','V')`).
pub async fn columns(conn: &mut Conn, filter: &str) -> Result<BTreeMap<TableKey, Vec<Col>>> {
    let sql = format!(
        "SELECT SCHEMA_NAME(o.schema_id), o.name, c.column_id, c.name, t.name, \
         SCHEMA_NAME(t.schema_id), t.is_user_defined, bt.name, c.max_length, c.precision, c.scale, \
         c.is_nullable, c.is_identity, c.is_computed, dc.name, dc.definition, cc.definition, \
         cc.is_persisted, CAST(ic.seed_value AS NVARCHAR(60)), CAST(ic.increment_value AS NVARCHAR(60)), \
         CAST(ep.value AS NVARCHAR(MAX)), c.collation_name, c.is_sparse, \
         CAST(COLUMNPROPERTY(c.object_id, c.name, 'GeneratedAlwaysType') AS INT) \
         FROM sys.objects o \
         JOIN sys.columns c ON c.object_id = o.object_id \
         JOIN sys.types t ON t.user_type_id = c.user_type_id \
         LEFT JOIN sys.types bt ON bt.user_type_id = t.system_type_id AND bt.system_type_id = t.system_type_id AND t.is_user_defined = 1 \
         LEFT JOIN sys.default_constraints dc ON dc.object_id = c.default_object_id \
         LEFT JOIN sys.computed_columns cc ON cc.object_id = c.object_id AND cc.column_id = c.column_id \
         LEFT JOIN sys.identity_columns ic ON ic.object_id = c.object_id AND ic.column_id = c.column_id \
         LEFT JOIN sys.extended_properties ep ON ep.class = 1 AND ep.major_id = c.object_id AND ep.minor_id = c.column_id AND ep.name = N'MS_Description' \
         WHERE {filter} \
         ORDER BY 1, 2, c.column_id"
    );
    let mut out: BTreeMap<TableKey, Vec<Col>> = BTreeMap::new();
    for r in conn.rows(&sql).await? {
        let sys_name = str_at(&r, 4);
        let is_udt = bool_at(&r, 6);
        let base = if is_udt {
            opt_str_at(&r, 7).unwrap_or_else(|| sys_name.clone())
        } else {
            sys_name.clone()
        };
        let (max_len, prec, scale) = (int_at(&r, 8), int_at(&r, 9), int_at(&r, 10));
        let (display, ddl) = if is_udt {
            let q = format!("{}.{}", str_at(&r, 5), sys_name);
            (q, format!("{}.{}", quote(&str_at(&r, 5)), quote(&sys_name)))
        } else if sys_name.eq_ignore_ascii_case("timestamp") {
            ("timestamp".to_string(), "ROWVERSION".to_string())
        } else {
            let d = format_type(&sys_name, max_len, prec, scale);
            (d.clone(), d.to_ascii_uppercase())
        };
        let identity = bool_at(&r, 12).then(|| {
            (
                opt_str_at(&r, 18).unwrap_or_else(|| "1".into()),
                opt_str_at(&r, 19).unwrap_or_else(|| "1".into()),
            )
        });
        let computed = if bool_at(&r, 13) {
            opt_str_at(&r, 16).map(|d| (d, bool_at(&r, 17)))
        } else {
            None
        };
        let default = opt_str_at(&r, 15).map(|d| (str_at(&r, 14), d));
        let col = Col {
            name: str_at(&r, 3),
            display_type: display,
            ddl_type: ddl,
            base_type: base,
            nullable: bool_at(&r, 11),
            identity,
            computed,
            default,
            collation: opt_str_at(&r, 21),
            comment: opt_str_at(&r, 20),
            sparse: bool_at(&r, 22),
            generated: int_at(&r, 23),
            rowversion: sys_name.eq_ignore_ascii_case("timestamp"),
        };
        out.entry((str_at(&r, 0), str_at(&r, 1)))
            .or_default()
            .push(col);
    }
    Ok(out)
}

/// Indexes (index_id > 0) of the tables selected by `filter`.
pub async fn indexes(conn: &mut Conn, filter: &str) -> Result<BTreeMap<TableKey, Vec<Idx>>> {
    let sql = format!(
        "SELECT SCHEMA_NAME(o.schema_id), o.name, i.index_id, i.name, i.type, i.type_desc, \
         i.is_unique, i.is_primary_key, i.is_unique_constraint, i.filter_definition, i.is_disabled, \
         c.name, ic.is_descending_key, ic.is_included_column, ic.key_ordinal \
         FROM sys.objects o \
         JOIN sys.indexes i ON i.object_id = o.object_id AND i.index_id > 0 \
         JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
         JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
         WHERE {filter} \
         ORDER BY 1, 2, i.index_id, ic.is_included_column, ic.key_ordinal, ic.index_column_id"
    );
    let mut out: BTreeMap<TableKey, Vec<Idx>> = BTreeMap::new();
    let mut last: Option<(TableKey, i64)> = None;
    for r in conn.rows(&sql).await? {
        let key = (str_at(&r, 0), str_at(&r, 1));
        let id = int_at(&r, 2);
        let list = out.entry(key.clone()).or_default();
        if last.as_ref() != Some(&(key.clone(), id)) {
            list.push(Idx {
                name: str_at(&r, 3),
                type_id: int_at(&r, 4),
                type_desc: str_at(&r, 5),
                unique: bool_at(&r, 6),
                primary: bool_at(&r, 7),
                unique_constraint: bool_at(&r, 8),
                filter: opt_str_at(&r, 9),
                disabled: bool_at(&r, 10),
                keys: Vec::new(),
                included: Vec::new(),
            });
            last = Some((key, id));
        }
        if let Some(idx) = list.last_mut() {
            let name = str_at(&r, 11);
            if bool_at(&r, 13) {
                idx.included.push(name);
            } else {
                idx.keys.push((name, bool_at(&r, 12)));
            }
        }
    }
    Ok(out)
}

/// Foreign keys whose parent table is selected by `filter`.
pub async fn foreign_keys(conn: &mut Conn, filter: &str) -> Result<BTreeMap<TableKey, Vec<Fk>>> {
    let sql = format!(
        "SELECT SCHEMA_NAME(o.schema_id), o.name, fk.name, fk.delete_referential_action_desc, \
         fk.update_referential_action_desc, SCHEMA_NAME(rt.schema_id), rt.name, pc.name, rc.name, \
         fk.is_disabled, fk.is_not_trusted \
         FROM sys.objects o \
         JOIN sys.foreign_keys fk ON fk.parent_object_id = o.object_id \
         JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id \
         JOIN sys.objects rt ON rt.object_id = fk.referenced_object_id \
         JOIN sys.columns pc ON pc.object_id = fkc.parent_object_id AND pc.column_id = fkc.parent_column_id \
         JOIN sys.columns rc ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id \
         WHERE {filter} \
         ORDER BY 1, 2, fk.name, fkc.constraint_column_id"
    );
    let mut out: BTreeMap<TableKey, Vec<Fk>> = BTreeMap::new();
    for r in conn.rows(&sql).await? {
        let key = (str_at(&r, 0), str_at(&r, 1));
        let name = str_at(&r, 2);
        let list = out.entry(key).or_default();
        if list.last().is_none_or(|f| f.name != name) {
            list.push(Fk {
                name,
                cols: Vec::new(),
                ref_schema: str_at(&r, 5),
                ref_table: str_at(&r, 6),
                ref_cols: Vec::new(),
                on_delete: str_at(&r, 3).replace('_', " "),
                on_update: str_at(&r, 4).replace('_', " "),
                disabled: bool_at(&r, 9),
                not_trusted: bool_at(&r, 10),
            });
        }
        if let Some(f) = list.last_mut() {
            f.cols.push(str_at(&r, 7));
            f.ref_cols.push(str_at(&r, 8));
        }
    }
    Ok(out)
}

/// Check constraints of the tables selected by `filter`.
pub async fn checks(conn: &mut Conn, filter: &str) -> Result<BTreeMap<TableKey, Vec<Check>>> {
    let sql = format!(
        "SELECT SCHEMA_NAME(o.schema_id), o.name, k.name, k.definition \
         FROM sys.objects o JOIN sys.check_constraints k ON k.parent_object_id = o.object_id \
         WHERE {filter} ORDER BY 1, 2, k.name"
    );
    let mut out: BTreeMap<TableKey, Vec<Check>> = BTreeMap::new();
    for r in conn.rows(&sql).await? {
        out.entry((str_at(&r, 0), str_at(&r, 1)))
            .or_default()
            .push(Check {
                name: str_at(&r, 2),
                definition: str_at(&r, 3),
            });
    }
    Ok(out)
}

/// Table-level `MS_Description` comments.
pub async fn table_comments(conn: &mut Conn, filter: &str) -> Result<BTreeMap<TableKey, String>> {
    let sql = format!(
        "SELECT SCHEMA_NAME(o.schema_id), o.name, CAST(ep.value AS NVARCHAR(MAX)) \
         FROM sys.objects o JOIN sys.extended_properties ep \
         ON ep.class = 1 AND ep.major_id = o.object_id AND ep.minor_id = 0 AND ep.name = N'MS_Description' \
         WHERE {filter}"
    );
    let mut out = BTreeMap::new();
    for r in conn.rows(&sql).await? {
        if let Some(c) = opt_str_at(&r, 2) {
            out.insert((str_at(&r, 0), str_at(&r, 1)), c);
        }
    }
    Ok(out)
}

/// Filter for one object by name.
pub fn one(schema: &str, name: &str) -> String {
    format!(
        "o.object_id = OBJECT_ID({})",
        lit(&format!("{}.{}", quote(schema), quote(name)))
    )
}

/// Filter for the tables (and optionally views) of a schema, or of every schema.
pub fn scope(schema: Option<&str>, views: bool) -> String {
    let types = if views { "'U','V'" } else { "'U'" };
    let mut f = format!("o.type IN ({types}) AND o.is_ms_shipped = 0");
    if let Some(s) = schema {
        f.push_str(&format!(" AND o.schema_id = SCHEMA_ID({})", lit(s)));
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters() {
        assert_eq!(
            one("dbo", "o'x"),
            "o.object_id = OBJECT_ID(N'[dbo].[o''x]')"
        );
        assert!(scope(Some("s"), false).contains("SCHEMA_ID(N's')"));
        assert!(scope(None, true).contains("'U','V'"));
    }
}
