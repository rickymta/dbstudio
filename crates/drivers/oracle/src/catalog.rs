//! Data dictionary queries: table details, keys and the pieces shared by browse, editing, the
//! schema model and DDL generation. Everything is read from the `ALL_*` views (what the user may
//! see); `DBA_*` is only tried for sizes and roles.

use std::collections::BTreeMap;

use dbstudio_core::{
    ColumnDetail, Error, ForeignKeyDetail, IndexDetail, ObjectKind, ObjectPath, Result,
    TableDetails, ValueKind,
};
use oracle::Connection;
use oracle::sql_type::ToSql;

use crate::error::map_error;

/// A row of text cells (`None` = NULL).
pub type TextRow = Vec<Option<String>>;

/// Run `sql` and return every row with every column as optional text.
pub fn rows(conn: &Connection, sql: &str, params: &[&dyn ToSql]) -> Result<Vec<TextRow>> {
    let rs = conn.query(sql, params).map_err(map_error)?;
    let mut out = Vec::new();
    for row in rs {
        let row = row.map_err(map_error)?;
        let n = row.column_info().len();
        let mut cells = Vec::with_capacity(n);
        for i in 0..n {
            cells.push(row.get::<_, Option<String>>(i).map_err(map_error)?);
        }
        out.push(cells);
    }
    Ok(out)
}

/// First cell of the first row, if any.
pub fn scalar(conn: &Connection, sql: &str, params: &[&dyn ToSql]) -> Result<Option<String>> {
    Ok(rows(conn, sql, params)?
        .into_iter()
        .next()
        .and_then(|mut r| if r.is_empty() { None } else { r.swap_remove(0) }))
}

/// Text of cell `i` (empty string for NULL).
pub fn cell(row: &TextRow, i: usize) -> String {
    row.get(i).cloned().flatten().unwrap_or_default()
}

/// `value_kind` for a data dictionary type name.
pub fn value_kind_of(data_type: &str, precision: Option<i64>, scale: Option<i64>) -> ValueKind {
    let t = data_type.to_ascii_uppercase();
    if t == "NUMBER" {
        return match (precision, scale) {
            (Some(p), Some(0)) if (1..=18).contains(&p) => ValueKind::Int,
            _ => ValueKind::Decimal,
        };
    }
    if t == "FLOAT" {
        return ValueKind::Decimal;
    }
    if t == "BINARY_FLOAT" || t == "BINARY_DOUBLE" {
        return ValueKind::Float;
    }
    if t == "DATE" {
        return ValueKind::DateTime;
    }
    if t.starts_with("TIMESTAMP") {
        return if t.contains("TIME ZONE") {
            ValueKind::Timestamp
        } else {
            ValueKind::DateTime
        };
    }
    if t.starts_with("INTERVAL") || t == "ROWID" || t == "UROWID" {
        return ValueKind::Ext;
    }
    match t.as_str() {
        "RAW" | "LONG RAW" | "BLOB" | "BFILE" => ValueKind::Bytes,
        "BOOLEAN" => ValueKind::Bool,
        "JSON" => ValueKind::Json,
        _ => ValueKind::Text,
    }
}

/// Type name like `VARCHAR2(20 CHAR)` or `NUMBER(10,2)` from dictionary columns.
#[allow(clippy::too_many_arguments)]
pub fn type_display(
    data_type: &str,
    data_type_owner: Option<&str>,
    data_length: Option<i64>,
    char_length: Option<i64>,
    char_used: Option<&str>,
    precision: Option<i64>,
    scale: Option<i64>,
) -> String {
    let t = data_type.to_ascii_uppercase();
    match t.as_str() {
        "VARCHAR2" => match (char_length, char_used) {
            (Some(n), Some("C")) => format!("VARCHAR2({n} CHAR)"),
            (_, _) => format!("VARCHAR2({})", data_length.unwrap_or(0)),
        },
        "NVARCHAR2" | "NCHAR" => format!("{t}({})", char_length.unwrap_or(0)),
        "CHAR" => match (char_length, char_used) {
            (Some(n), Some("C")) => format!("CHAR({n} CHAR)"),
            _ => format!("CHAR({})", data_length.unwrap_or(0)),
        },
        "RAW" => format!("RAW({})", data_length.unwrap_or(0)),
        "NUMBER" => match (precision, scale) {
            (Some(p), Some(s)) if s != 0 => format!("NUMBER({p},{s})"),
            (Some(p), _) => format!("NUMBER({p})"),
            (None, Some(s)) if s != 0 => format!("NUMBER(*,{s})"),
            _ => "NUMBER".to_string(),
        },
        "FLOAT" => match precision {
            Some(p) if p != 126 => format!("FLOAT({p})"),
            _ => "FLOAT".to_string(),
        },
        _ => match data_type_owner.filter(|o| !o.is_empty()) {
            Some(owner) => format!("\"{owner}\".\"{data_type}\""),
            None => data_type.to_string(),
        },
    }
}

fn parse_i64(s: &Option<String>) -> Option<i64> {
    s.as_deref().and_then(|v| v.trim().parse().ok())
}

/// Kind of a table-like object (`TABLE`, `VIEW`, `MATERIALIZED VIEW`), if `schema.name` exists.
pub fn relation_kind(conn: &Connection, schema: &str, name: &str) -> Result<Option<ObjectKind>> {
    let found = rows(
        conn,
        "SELECT object_type FROM all_objects WHERE owner = :1 AND object_name = :2 \
         AND object_type IN ('TABLE', 'VIEW', 'MATERIALIZED VIEW')",
        &[&schema, &name],
    )?;
    let mut kind = None;
    for r in found {
        match cell(&r, 0).as_str() {
            "MATERIALIZED VIEW" => return Ok(Some(ObjectKind::MaterializedView)),
            "VIEW" => kind = Some(ObjectKind::View),
            "TABLE" if kind.is_none() => kind = Some(ObjectKind::Table),
            _ => {}
        }
    }
    Ok(kind)
}

/// Columns of a table or view in position order.
pub fn columns(conn: &Connection, schema: &str, name: &str) -> Result<Vec<ColumnDetail>> {
    let identity: BTreeMap<String, String> = rows(
        conn,
        "SELECT column_name, generation_type FROM all_tab_identity_cols \
         WHERE owner = :1 AND table_name = :2",
        &[&schema, &name],
    )
    // Not available before Oracle 12c.
    .unwrap_or_default()
    .into_iter()
    .map(|r| (cell(&r, 0), cell(&r, 1)))
    .collect();
    let list = rows(
        conn,
        "SELECT c.column_name, c.column_id, c.data_type, c.data_type_owner, c.data_length, \
                c.char_length, c.char_used, c.data_precision, c.data_scale, c.nullable, \
                c.data_default, c.virtual_column, cc.comments \
         FROM all_tab_cols c \
         LEFT JOIN all_col_comments cc ON cc.owner = c.owner AND cc.table_name = c.table_name \
              AND cc.column_name = c.column_name \
         WHERE c.owner = :1 AND c.table_name = :2 AND c.hidden_column = 'NO' \
               AND c.column_id IS NOT NULL \
         ORDER BY c.column_id",
        &[&schema, &name],
    )?;
    let pk: Vec<String> = key_columns(conn, schema, name, "P")?
        .into_iter()
        .next()
        .map(|(_, cols)| cols)
        .unwrap_or_default();
    Ok(list
        .iter()
        .map(|r| {
            let col_name = cell(r, 0);
            let data_type = cell(r, 2);
            let precision = parse_i64(&r[7]);
            let scale = parse_i64(&r[8]);
            let mut extra: Vec<String> = Vec::new();
            if let Some(g) = identity.get(&col_name) {
                extra.push(format!("identity {}", g.to_ascii_lowercase()));
            }
            if cell(r, 11) == "YES" {
                extra.push("virtual".to_string());
            }
            ColumnDetail {
                name: col_name.clone(),
                ordinal: u32::try_from(parse_i64(&r[1]).unwrap_or(0)).unwrap_or(0),
                type_name: type_display(
                    &data_type,
                    r[3].as_deref(),
                    parse_i64(&r[4]),
                    parse_i64(&r[5]),
                    r[6].as_deref(),
                    precision,
                    scale,
                ),
                value_kind: value_kind_of(&data_type, precision, scale),
                nullable: cell(r, 9) != "N",
                default: r[10]
                    .as_deref()
                    .map(|d| d.trim().to_string())
                    .filter(|d| !d.is_empty()),
                comment: r[12].clone().filter(|c| !c.is_empty()),
                primary_key: pk.contains(&col_name),
                extra: (!extra.is_empty()).then(|| extra.join(", ")),
            }
        })
        .collect())
}

/// Constraints of type `kind` (`P`, `U`) as `(constraint name, columns in order)`.
pub fn key_columns(
    conn: &Connection,
    schema: &str,
    name: &str,
    kind: &str,
) -> Result<Vec<(String, Vec<String>)>> {
    let list = rows(
        conn,
        "SELECT c.constraint_name, cc.column_name FROM all_constraints c \
         JOIN all_cons_columns cc ON cc.owner = c.owner AND cc.constraint_name = c.constraint_name \
         WHERE c.owner = :1 AND c.table_name = :2 AND c.constraint_type = :3 \
         ORDER BY c.constraint_name, cc.position",
        &[&schema, &name, &kind],
    )?;
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for r in list {
        let (cn, col) = (cell(&r, 0), cell(&r, 1));
        match out.last_mut() {
            Some((last, cols)) if *last == cn => cols.push(col),
            _ => out.push((cn, vec![col])),
        }
    }
    Ok(out)
}

/// Indexes of a table.
pub fn indexes(conn: &Connection, schema: &str, name: &str) -> Result<Vec<IndexDetail>> {
    let pk_index = scalar(
        conn,
        "SELECT index_name FROM all_constraints WHERE owner = :1 AND table_name = :2 \
         AND constraint_type = 'P'",
        &[&schema, &name],
    )?;
    let list = rows(
        conn,
        "SELECT i.index_name, i.uniqueness, i.index_type, ic.column_name, ic.descend, \
                ie.column_expression \
         FROM all_indexes i \
         JOIN all_ind_columns ic ON ic.index_owner = i.owner AND ic.index_name = i.index_name \
         LEFT JOIN all_ind_expressions ie ON ie.index_owner = i.owner \
              AND ie.index_name = i.index_name AND ie.column_position = ic.column_position \
         WHERE i.table_owner = :1 AND i.table_name = :2 \
         ORDER BY i.index_name, ic.column_position",
        &[&schema, &name],
    )?;
    let mut out: Vec<IndexDetail> = Vec::new();
    let mut seen: Option<String> = None;
    for r in list {
        let index = cell(&r, 0);
        let column = match r[5].as_deref().map(str::trim).filter(|e| !e.is_empty()) {
            Some(expr) => expr.to_string(),
            None => {
                let c = cell(&r, 3);
                if cell(&r, 4) == "DESC" {
                    format!("{c} DESC")
                } else {
                    c
                }
            }
        };
        if seen.as_deref() == Some(index.as_str()) {
            if let Some(last) = out.last_mut() {
                last.columns.push(column);
            }
        } else {
            seen = Some(index.clone());
            out.push(IndexDetail {
                primary: pk_index.as_deref() == Some(index.as_str()),
                name: index,
                columns: vec![column],
                unique: cell(&r, 1) == "UNIQUE",
                method: Some(cell(&r, 2)).filter(|m| !m.is_empty()),
                definition: None,
                partial: false,
            });
        }
    }
    for idx in &mut out {
        let kind = if idx.method.as_deref() == Some("BITMAP") {
            "BITMAP "
        } else if idx.unique {
            "UNIQUE "
        } else {
            ""
        };
        idx.definition = Some(format!(
            "CREATE {kind}INDEX \"{}\" ON \"{schema}\".\"{name}\" ({})",
            idx.name,
            idx.columns.join(", ")
        ));
    }
    Ok(out)
}

/// Foreign keys of a table.
pub fn foreign_keys(conn: &Connection, schema: &str, name: &str) -> Result<Vec<ForeignKeyDetail>> {
    let list = rows(
        conn,
        "SELECT c.constraint_name, cc.column_name, c.delete_rule, rc.owner, rc.table_name, \
                rcc.column_name \
         FROM all_constraints c \
         JOIN all_cons_columns cc ON cc.owner = c.owner AND cc.constraint_name = c.constraint_name \
         JOIN all_constraints rc ON rc.owner = c.r_owner AND rc.constraint_name = c.r_constraint_name \
         JOIN all_cons_columns rcc ON rcc.owner = rc.owner AND rcc.constraint_name = rc.constraint_name \
              AND rcc.position = cc.position \
         WHERE c.owner = :1 AND c.table_name = :2 AND c.constraint_type = 'R' \
         ORDER BY c.constraint_name, cc.position",
        &[&schema, &name],
    )?;
    let mut out: Vec<ForeignKeyDetail> = Vec::new();
    for r in list {
        let cn = cell(&r, 0);
        match out.last_mut() {
            Some(last) if last.name == cn => {
                last.columns.push(cell(&r, 1));
                last.referenced_columns.push(cell(&r, 5));
            }
            _ => out.push(ForeignKeyDetail {
                name: cn,
                columns: vec![cell(&r, 1)],
                referenced_table: table_path(&cell(&r, 3), &cell(&r, 4)),
                referenced_columns: vec![cell(&r, 5)],
                on_update: None,
                on_delete: Some(cell(&r, 2)).filter(|d| !d.is_empty()),
            }),
        }
    }
    Ok(out)
}

/// Tree path of a table.
pub fn table_path(schema: &str, name: &str) -> ObjectPath {
    ObjectPath::root()
        .child(ObjectKind::Schema, schema)
        .child(ObjectKind::Folder, dbstudio_core::folder::TABLES)
        .child(ObjectKind::Table, name)
}

/// Tree path of a schema object of `kind`.
pub fn object_path(schema: &str, kind: ObjectKind, name: &str) -> ObjectPath {
    ObjectPath::root()
        .child(ObjectKind::Schema, schema)
        .child(ObjectKind::Folder, folder_of(kind))
        .child(kind, name)
}

/// Folder key holding objects of `kind`.
pub fn folder_of(kind: ObjectKind) -> &'static str {
    use dbstudio_core::folder as f;
    match kind {
        ObjectKind::View => f::VIEWS,
        ObjectKind::MaterializedView => f::MATERIALIZED_VIEWS,
        ObjectKind::Sequence => f::SEQUENCES,
        ObjectKind::Synonym => f::SYNONYMS,
        ObjectKind::Function => f::FUNCTIONS,
        ObjectKind::Procedure => f::PROCEDURES,
        ObjectKind::Package => f::PACKAGES,
        ObjectKind::Type => f::TYPES,
        ObjectKind::Trigger => f::TRIGGERS,
        _ => f::TABLES,
    }
}

/// Full description of a table or view.
pub fn table_details(conn: &Connection, schema: &str, name: &str) -> Result<TableDetails> {
    let kind = relation_kind(conn, schema, name)?
        .ok_or_else(|| Error::NotFound(format!("table or view {schema}.{name}")))?;
    let comment = scalar(
        conn,
        "SELECT comments FROM all_tab_comments WHERE owner = :1 AND table_name = :2",
        &[&schema, &name],
    )?
    .filter(|c| !c.is_empty());
    let estimated_rows = scalar(
        conn,
        "SELECT num_rows FROM all_tables WHERE owner = :1 AND table_name = :2",
        &[&schema, &name],
    )?
    .and_then(|v| v.trim().parse::<u64>().ok());
    let size_bytes = if matches!(kind, ObjectKind::Table | ObjectKind::MaterializedView) {
        segment_size(conn, schema, name)
    } else {
        None
    };
    let (columns, indexes, foreign_keys) = if kind == ObjectKind::View {
        (columns(conn, schema, name)?, Vec::new(), Vec::new())
    } else {
        (
            columns(conn, schema, name)?,
            indexes(conn, schema, name)?,
            foreign_keys(conn, schema, name)?,
        )
    };
    Ok(TableDetails {
        path: object_path(schema, kind, name),
        kind,
        comment,
        columns,
        indexes,
        foreign_keys,
        estimated_rows,
        size_bytes,
    })
}

/// Bytes used by a table and its indexes (needs access to `DBA_SEGMENTS`, otherwise `None`).
fn segment_size(conn: &Connection, schema: &str, name: &str) -> Option<u64> {
    let sql = "SELECT SUM(bytes) FROM dba_segments WHERE owner = :1 AND \
               (segment_name = :2 OR segment_name IN \
                (SELECT index_name FROM all_indexes WHERE table_owner = :1 AND table_name = :2))";
    scalar(conn, sql, &[&schema, &name])
        .ok()
        .flatten()
        .and_then(|v| v.trim().parse::<u64>().ok())
}

/// What decides whether and how the rows of a table can be edited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyInfo {
    pub kind: ObjectKind,
    pub primary_key: Vec<String>,
    /// Unique constraints or unique indexes whose columns are all `NOT NULL`.
    pub unique_keys: Vec<Vec<String>>,
    /// Columns that cannot be written (virtual and `GENERATED ALWAYS` identity columns).
    pub read_only_columns: Vec<String>,
    /// All columns in order (name, nullable).
    pub columns: Vec<(String, bool)>,
    /// Index-organized tables have no usable ROWID for updates in some releases; kept for info.
    pub index_organized: bool,
}

/// Keys and read-only columns of `schema.name`.
pub fn key_info(conn: &Connection, schema: &str, name: &str) -> Result<KeyInfo> {
    let kind = relation_kind(conn, schema, name)?
        .ok_or_else(|| Error::NotFound(format!("table or view {schema}.{name}")))?;
    let primary_key = key_columns(conn, schema, name, "P")?
        .into_iter()
        .next()
        .map(|(_, c)| c)
        .unwrap_or_default();
    let cols = rows(
        conn,
        "SELECT column_name, nullable, virtual_column FROM all_tab_cols \
         WHERE owner = :1 AND table_name = :2 AND hidden_column = 'NO' AND column_id IS NOT NULL \
         ORDER BY column_id",
        &[&schema, &name],
    )?;
    let nullable: BTreeMap<String, bool> = cols
        .iter()
        .map(|r| (cell(r, 0), cell(r, 1) != "N"))
        .collect();
    let mut read_only_columns: Vec<String> = cols
        .iter()
        .filter(|r| cell(r, 2) == "YES")
        .map(|r| cell(r, 0))
        .collect();
    for r in rows(
        conn,
        "SELECT column_name FROM all_tab_identity_cols WHERE owner = :1 AND table_name = :2 \
         AND generation_type = 'ALWAYS'",
        &[&schema, &name],
    )
    .unwrap_or_default()
    {
        let c = cell(&r, 0);
        if !read_only_columns.contains(&c) {
            read_only_columns.push(c);
        }
    }
    let mut unique_keys: Vec<Vec<String>> = Vec::new();
    let mut candidates: Vec<(String, Vec<String>)> = key_columns(conn, schema, name, "U")?;
    // Unique indexes (their columns in order); one backing a constraint is already listed.
    let mut by_index: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for r in rows(
        conn,
        "SELECT i.index_name, ic.column_name FROM all_indexes i \
         JOIN all_ind_columns ic ON ic.index_owner = i.owner AND ic.index_name = i.index_name \
         WHERE i.table_owner = :1 AND i.table_name = :2 AND i.uniqueness = 'UNIQUE' \
         ORDER BY i.index_name, ic.column_position",
        &[&schema, &name],
    )? {
        by_index.entry(cell(&r, 0)).or_default().push(cell(&r, 1));
    }
    for (idx, cols) in by_index {
        if !candidates.iter().any(|(_, c)| *c == cols) {
            candidates.push((idx, cols));
        }
    }
    for (_, cols) in candidates {
        let all_not_null = cols.iter().all(|c| nullable.get(c).is_some_and(|n| !*n));
        if all_not_null && !unique_keys.contains(&cols) && cols != primary_key {
            unique_keys.push(cols);
        }
    }
    let index_organized = scalar(
        conn,
        "SELECT iot_type FROM all_tables WHERE owner = :1 AND table_name = :2",
        &[&schema, &name],
    )?
    .is_some_and(|t| t == "IOT");
    Ok(KeyInfo {
        kind,
        primary_key,
        unique_keys,
        read_only_columns,
        columns: cols
            .iter()
            .map(|r| (cell(r, 0), cell(r, 1) != "N"))
            .collect(),
        index_organized,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_kinds_of_dictionary_types() {
        assert_eq!(value_kind_of("NUMBER", Some(10), Some(0)), ValueKind::Int);
        assert_eq!(
            value_kind_of("NUMBER", Some(19), Some(0)),
            ValueKind::Decimal
        );
        assert_eq!(value_kind_of("NUMBER", None, None), ValueKind::Decimal);
        assert_eq!(
            value_kind_of("NUMBER", Some(10), Some(2)),
            ValueKind::Decimal
        );
        assert_eq!(value_kind_of("BINARY_DOUBLE", None, None), ValueKind::Float);
        assert_eq!(value_kind_of("DATE", None, None), ValueKind::DateTime);
        assert_eq!(
            value_kind_of("TIMESTAMP(6)", None, None),
            ValueKind::DateTime
        );
        assert_eq!(
            value_kind_of("TIMESTAMP(6) WITH TIME ZONE", None, None),
            ValueKind::Timestamp
        );
        assert_eq!(
            value_kind_of("TIMESTAMP(6) WITH LOCAL TIME ZONE", None, None),
            ValueKind::Timestamp
        );
        assert_eq!(
            value_kind_of("INTERVAL DAY(2) TO SECOND(6)", None, None),
            ValueKind::Ext
        );
        assert_eq!(value_kind_of("RAW", None, None), ValueKind::Bytes);
        assert_eq!(value_kind_of("BLOB", None, None), ValueKind::Bytes);
        assert_eq!(value_kind_of("CLOB", None, None), ValueKind::Text);
        assert_eq!(value_kind_of("NVARCHAR2", None, None), ValueKind::Text);
        assert_eq!(value_kind_of("JSON", None, None), ValueKind::Json);
        assert_eq!(value_kind_of("ROWID", None, None), ValueKind::Ext);
    }

    #[test]
    fn type_display_matches_oracle_style() {
        let t = |ty: &str, len, clen, cu, p, s| type_display(ty, None, len, clen, cu, p, s);
        assert_eq!(
            t("VARCHAR2", Some(80), Some(20), Some("C"), None, None),
            "VARCHAR2(20 CHAR)"
        );
        assert_eq!(
            t("VARCHAR2", Some(20), Some(20), Some("B"), None, None),
            "VARCHAR2(20)"
        );
        assert_eq!(
            t("NVARCHAR2", Some(40), Some(20), None, None, None),
            "NVARCHAR2(20)"
        );
        assert_eq!(
            t("NUMBER", Some(22), None, None, Some(10), Some(2)),
            "NUMBER(10,2)"
        );
        assert_eq!(
            t("NUMBER", Some(22), None, None, Some(10), Some(0)),
            "NUMBER(10)"
        );
        assert_eq!(t("NUMBER", Some(22), None, None, None, None), "NUMBER");
        assert_eq!(t("FLOAT", Some(22), None, None, Some(126), None), "FLOAT");
        assert_eq!(t("RAW", Some(16), None, None, None, None), "RAW(16)");
        assert_eq!(t("DATE", Some(7), None, None, None, None), "DATE");
        assert_eq!(
            type_display("MY_TYPE", Some("HR"), None, None, None, None, None),
            "\"HR\".\"MY_TYPE\""
        );
    }

    #[test]
    fn folder_mapping() {
        assert_eq!(folder_of(ObjectKind::Package), "packages");
        assert_eq!(folder_of(ObjectKind::Table), "tables");
        let p = object_path("HR", ObjectKind::View, "V");
        assert_eq!(p.segments.len(), 3);
        assert_eq!(p.segments[1].name, "views");
    }
}
