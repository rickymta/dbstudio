//! SQL text helpers: identifier and literal quoting, filter / order / paging generation and the
//! path helpers shared by the browse and data code.

use dbstudio_core::{ColumnFilter, Error, FilterOp, ObjectKind, ObjectPath, Result, SortKey};

/// Quote an identifier. Oracle identifiers cannot contain `"`, so such a name is rejected.
pub fn quote_ident(name: &str) -> Result<String> {
    if name.is_empty() {
        return Err(Error::query("an identifier must not be empty"));
    }
    if name.contains('"') || name.contains('\0') {
        return Err(Error::query(format!(
            "invalid identifier \"{}\"",
            name.replace(['"', '\0'], "?")
        )));
    }
    Ok(format!("\"{name}\""))
}

/// `"SCHEMA"."NAME"`.
pub fn qualified(schema: &str, name: &str) -> Result<String> {
    Ok(format!("{}.{}", quote_ident(schema)?, quote_ident(name)?))
}

/// SQL string literal (quotes doubled). Oracle limits SQL literals to 4000 bytes: longer text is
/// split into `TO_CLOB('..') || TO_CLOB('..')`.
pub fn quote_literal(value: &str) -> Result<String> {
    if value.contains('\0') {
        return Err(Error::query("a value must not contain a NUL character"));
    }
    const CHUNK: usize = 1500;
    if value.len() <= 3900 {
        return Ok(format!("'{}'", value.replace('\'', "''")));
    }
    let mut parts = Vec::new();
    let mut cur = String::new();
    for c in value.chars() {
        if cur.len() + c.len_utf8() > CHUNK {
            parts.push(std::mem::take(&mut cur));
        }
        cur.push(c);
    }
    if !cur.is_empty() {
        parts.push(cur);
    }
    Ok(parts
        .iter()
        .map(|p| format!("TO_CLOB('{}')", p.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(" || "))
}

/// Escape the `LIKE` wildcards and the escape character `\`.
pub fn escape_like(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn first_value(f: &ColumnFilter) -> Result<&str> {
    f.values.first().map(String::as_str).ok_or_else(|| {
        Error::query(format!(
            "filter on column \"{}\" needs a value",
            f.column.replace('"', "'")
        ))
    })
}

/// SQL condition of one filter. An empty string equals NULL in Oracle, so `= ''` becomes
/// `IS NULL`.
pub fn filter_condition(f: &ColumnFilter) -> Result<String> {
    let col = quote_ident(&f.column)?;
    let cmp = |sym: &str| -> Result<String> {
        let v = first_value(f)?;
        if v.is_empty() && matches!(sym, "=" | "<>") {
            return Ok(format!(
                "{col} IS {}NULL",
                if sym == "<>" { "NOT " } else { "" }
            ));
        }
        Ok(format!("{col} {sym} {}", quote_literal(v)?))
    };
    let like = |negated: bool, prefix: &str, suffix: &str| -> Result<String> {
        let pattern = format!("{prefix}{}{suffix}", escape_like(first_value(f)?));
        Ok(format!(
            "UPPER({col}) {}LIKE UPPER({}) ESCAPE '\\'",
            if negated { "NOT " } else { "" },
            quote_literal(&pattern)?
        ))
    };
    match f.op {
        FilterOp::Eq => cmp("="),
        FilterOp::Ne => cmp("<>"),
        FilterOp::Lt => cmp("<"),
        FilterOp::Le => cmp("<="),
        FilterOp::Gt => cmp(">"),
        FilterOp::Ge => cmp(">="),
        FilterOp::Contains => like(false, "%", "%"),
        FilterOp::NotContains => like(true, "%", "%"),
        FilterOp::StartsWith => like(false, "", "%"),
        FilterOp::EndsWith => like(false, "%", ""),
        FilterOp::IsNull => Ok(format!("{col} IS NULL")),
        FilterOp::IsNotNull => Ok(format!("{col} IS NOT NULL")),
        FilterOp::In => {
            if f.values.is_empty() {
                return Ok("1 = 0".to_string());
            }
            // Oracle allows at most 1000 expressions in an IN list.
            let chunks: Vec<String> = f
                .values
                .chunks(1000)
                .map(|chunk| {
                    let list: Result<Vec<String>> =
                        chunk.iter().map(|v| quote_literal(v)).collect();
                    list.map(|l| format!("{col} IN ({})", l.join(", ")))
                })
                .collect::<Result<_>>()?;
            if chunks.len() == 1 {
                Ok(chunks.into_iter().next().unwrap_or_default())
            } else {
                Ok(format!("({})", chunks.join(" OR ")))
            }
        }
    }
}

/// `WHERE ...` clause (leading newline) combining filters and the raw clause; empty when there is
/// nothing to filter on.
pub fn where_clause(filters: &[ColumnFilter], raw_where: Option<&str>) -> Result<String> {
    let mut parts: Vec<String> = Vec::new();
    for f in filters {
        parts.push(filter_condition(f)?);
    }
    if let Some(raw) = raw_where.map(str::trim).filter(|r| !r.is_empty()) {
        if raw.contains('\0') {
            return Err(Error::query(
                "the WHERE clause must not contain a NUL character",
            ));
        }
        // Newlines keep a trailing `-- comment` of the raw text from swallowing the parenthesis.
        parts.push(format!("(\n{raw}\n)"));
    }
    if parts.is_empty() {
        Ok(String::new())
    } else {
        Ok(format!("\nWHERE {}", parts.join("\n  AND ")))
    }
}

/// One element of a tiebreak: a quoted column or the `ROWID` pseudo column.
fn tiebreak_expr(name: &str) -> Result<String> {
    if name == ROWID {
        Ok(ROWID.to_string())
    } else {
        quote_ident(name)
    }
}

/// Name used for the `ROWID` pseudo column as a key and as an extra result column.
pub const ROWID: &str = "ROWID";

/// `ORDER BY ...` clause (leading newline) or an empty string. `tiebreak` columns (a key of the
/// table) follow the user's sort keys so that `OFFSET` paging is deterministic.
pub fn order_clause(sort: &[SortKey], tiebreak: &[String]) -> Result<String> {
    let mut parts = Vec::new();
    for s in sort {
        parts.push(format!(
            "{}{}",
            quote_ident(&s.column)?,
            if s.descending { " DESC" } else { " ASC" }
        ));
    }
    for t in tiebreak {
        if !sort.iter().any(|s| &s.column == t) {
            parts.push(format!("{} ASC", tiebreak_expr(t)?));
        }
    }
    if parts.is_empty() {
        Ok(String::new())
    } else {
        Ok(format!("\nORDER BY {}", parts.join(", ")))
    }
}

/// The `SELECT` that implements a table page request. `select_list` is `*` or an explicit list.
#[allow(clippy::too_many_arguments)]
pub fn select_sql(
    select_list: &str,
    table: &str,
    filters: &[ColumnFilter],
    raw_where: Option<&str>,
    sort: &[SortKey],
    tiebreak: &[String],
    offset: u64,
    limit: u64,
) -> Result<String> {
    Ok(format!(
        "SELECT {select_list} FROM {table}{}{}\nOFFSET {offset} ROWS FETCH NEXT {limit} ROWS ONLY",
        where_clause(filters, raw_where)?,
        order_clause(sort, tiebreak)?
    ))
}

/// The `SELECT COUNT(*)` of a count request.
pub fn count_sql(table: &str, filters: &[ColumnFilter], raw_where: Option<&str>) -> Result<String> {
    Ok(format!(
        "SELECT COUNT(*) FROM {table}{}",
        where_clause(filters, raw_where)?
    ))
}

/// Non-folder segments of a path.
pub fn non_folder_segments(path: &ObjectPath) -> Vec<(ObjectKind, &str)> {
    path.segments
        .iter()
        .filter(|s| s.kind != ObjectKind::Folder)
        .map(|s| (s.kind, s.name.as_str()))
        .collect()
}

/// `(schema, name)` of a table, view or materialized view path (folders optional).
pub fn relation_of(path: &ObjectPath) -> Result<(String, String)> {
    use ObjectKind as K;
    match non_folder_segments(path).as_slice() {
        [
            (K::Schema, s),
            (K::Table | K::View | K::MaterializedView, n),
        ] => Ok(((*s).to_string(), (*n).to_string())),
        _ => Err(Error::NotFound(
            "expected a path to a table or view (schema, table)".into(),
        )),
    }
}

/// `(schema, kind, name)` of an object living directly in a schema.
pub fn schema_object_of(path: &ObjectPath) -> Result<(String, ObjectKind, String)> {
    use ObjectKind as K;
    match non_folder_segments(path).as_slice() {
        [(K::Schema, s), (kind, n)]
            if !matches!(
                kind,
                K::Schema
                    | K::Column
                    | K::Database
                    | K::Role
                    | K::Key
                    | K::KeyPrefix
                    | K::Collection
            ) =>
        {
            Ok(((*s).to_string(), *kind, (*n).to_string()))
        }
        _ => Err(Error::NotFound("expected a path to a schema object".into())),
    }
}

/// Lowercase hexadecimal text of `bytes`.
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 15) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(column: &str, op: FilterOp, values: &[&str]) -> ColumnFilter {
        ColumnFilter {
            column: column.into(),
            op,
            values: values.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn identifiers_keep_case_and_reject_quotes() {
        assert_eq!(quote_ident("Emp").unwrap(), "\"Emp\"");
        assert_eq!(qualified("HR", "T").unwrap(), "\"HR\".\"T\"");
        assert!(quote_ident("a\"b").is_err());
        assert!(quote_ident("").is_err());
    }

    #[test]
    fn literals_double_quotes_and_split_long_text() {
        assert_eq!(quote_literal("it's").unwrap(), "'it''s'");
        assert!(quote_literal("a\0b").is_err());
        let long = "x".repeat(5000);
        let lit = quote_literal(&long).unwrap();
        assert!(lit.starts_with("TO_CLOB('") && lit.contains("') || TO_CLOB('"));
        assert_eq!(lit.matches('x').count(), 5000);
    }

    #[test]
    fn filters_follow_oracle_semantics() {
        assert_eq!(
            filter_condition(&f("A", FilterOp::Eq, &["x"])).unwrap(),
            "\"A\" = 'x'"
        );
        // '' is NULL in Oracle.
        assert_eq!(
            filter_condition(&f("A", FilterOp::Eq, &[""])).unwrap(),
            "\"A\" IS NULL"
        );
        assert_eq!(
            filter_condition(&f("A", FilterOp::Ne, &[""])).unwrap(),
            "\"A\" IS NOT NULL"
        );
        assert_eq!(
            filter_condition(&f("A", FilterOp::Contains, &["50%_"])).unwrap(),
            "UPPER(\"A\") LIKE UPPER('%50\\%\\_%') ESCAPE '\\'"
        );
        assert_eq!(
            filter_condition(&f("A", FilterOp::NotContains, &["x"])).unwrap(),
            "UPPER(\"A\") NOT LIKE UPPER('%x%') ESCAPE '\\'"
        );
        assert_eq!(
            filter_condition(&f("A", FilterOp::StartsWith, &["x"])).unwrap(),
            "UPPER(\"A\") LIKE UPPER('x%') ESCAPE '\\'"
        );
        assert_eq!(
            filter_condition(&f("A", FilterOp::In, &["1", "2"])).unwrap(),
            "\"A\" IN ('1', '2')"
        );
        assert_eq!(
            filter_condition(&f("A", FilterOp::In, &[])).unwrap(),
            "1 = 0"
        );
        assert!(filter_condition(&f("A", FilterOp::Eq, &[])).is_err());
        assert_eq!(
            filter_condition(&f("A", FilterOp::IsNotNull, &[])).unwrap(),
            "\"A\" IS NOT NULL"
        );
    }

    #[test]
    fn long_in_lists_are_chunked() {
        let values: Vec<String> = (0..2500).map(|i| i.to_string()).collect();
        let c = filter_condition(&ColumnFilter {
            column: "A".into(),
            op: FilterOp::In,
            values,
        })
        .unwrap();
        assert_eq!(c.matches(" IN (").count(), 3);
        assert!(c.starts_with('(') && c.contains(") OR \"A\" IN ("));
    }

    #[test]
    fn where_clause_wraps_raw_text_in_lines() {
        let w = where_clause(&[f("A", FilterOp::IsNull, &[])], Some("b > 1 -- c")).unwrap();
        assert_eq!(w, "\nWHERE \"A\" IS NULL\n  AND (\nb > 1 -- c\n)");
        assert_eq!(where_clause(&[], Some("  ")).unwrap(), "");
    }

    #[test]
    fn order_adds_a_deterministic_tiebreak() {
        let sort = vec![SortKey {
            column: "B".into(),
            descending: true,
        }];
        assert_eq!(
            order_clause(&sort, &["ID".into(), "B".into()]).unwrap(),
            "\nORDER BY \"B\" DESC, \"ID\" ASC"
        );
        assert_eq!(
            order_clause(&[], &[ROWID.to_string()]).unwrap(),
            "\nORDER BY ROWID ASC"
        );
        assert_eq!(order_clause(&[], &[]).unwrap(), "");
    }

    #[test]
    fn paging_uses_offset_fetch() {
        let sql = select_sql(
            "*",
            "\"S\".\"T\"",
            &[],
            None,
            &[],
            &["ID".to_string()],
            20,
            10,
        )
        .unwrap();
        assert_eq!(
            sql,
            "SELECT * FROM \"S\".\"T\"\nORDER BY \"ID\" ASC\nOFFSET 20 ROWS FETCH NEXT 10 ROWS ONLY"
        );
        assert_eq!(
            count_sql("\"S\".\"T\"", &[f("A", FilterOp::IsNull, &[])], None).unwrap(),
            "SELECT COUNT(*) FROM \"S\".\"T\"\nWHERE \"A\" IS NULL"
        );
    }

    #[test]
    fn path_helpers_accept_missing_folders() {
        let p = ObjectPath::root()
            .child(ObjectKind::Schema, "HR")
            .child(ObjectKind::Folder, "tables")
            .child(ObjectKind::Table, "EMP");
        assert_eq!(relation_of(&p).unwrap(), ("HR".into(), "EMP".into()));
        let q = ObjectPath::root()
            .child(ObjectKind::Schema, "HR")
            .child(ObjectKind::Table, "EMP");
        assert_eq!(relation_of(&q).unwrap(), ("HR".into(), "EMP".into()));
        assert!(relation_of(&ObjectPath::root()).is_err());
        let f = ObjectPath::root()
            .child(ObjectKind::Schema, "HR")
            .child(ObjectKind::Folder, "packages")
            .child(ObjectKind::Package, "PK");
        assert_eq!(
            schema_object_of(&f).unwrap(),
            ("HR".into(), ObjectKind::Package, "PK".into())
        );
        assert_eq!(hex(&[0, 15, 255]), "000FFF");
    }
}
