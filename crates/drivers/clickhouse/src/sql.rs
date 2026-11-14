//! SQL text builders: filters, `SELECT` / `count()` statements for the data grid and typed
//! literals for data editing. Pure functions, unit tested without a server.

use dbstudio_core::{
    ColumnFilter, Error, FilterOp, ObjectKind, ObjectPath, Result, SortKey, TableCountRequest,
    TableDataRequest, Value,
};

use crate::conn::{quote_ident, quote_literal};
use crate::rowbinary::{fmt_timestamp, parse_datetime, value_text};
use crate::types::CType;

pub(crate) fn quote_table(db: &str, table: &str) -> String {
    format!("{}.{}", quote_ident(db), quote_ident(table))
}

/// Escape `LIKE` wildcards and the escape character (ClickHouse uses a backslash).
fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn single(f: &ColumnFilter) -> Result<&str> {
    f.values.first().map(String::as_str).ok_or_else(|| {
        Error::Invalid(format!(
            "filter on column `{}` requires a value",
            f.column.replace('`', "")
        ))
    })
}

fn like(col: &str, negate: bool, pattern: &str) -> String {
    format!(
        "toString({col}) {}ILIKE {}",
        if negate { "NOT " } else { "" },
        quote_literal(pattern)
    )
}

/// SQL condition of one filter.
pub(crate) fn filter_condition(f: &ColumnFilter) -> Result<String> {
    let col = quote_ident(&f.column);
    let cmp =
        |op: &str| -> Result<String> { Ok(format!("{col} {op} {}", quote_literal(single(f)?))) };
    match f.op {
        FilterOp::Eq => cmp("="),
        FilterOp::Ne => cmp("!="),
        FilterOp::Lt => cmp("<"),
        FilterOp::Le => cmp("<="),
        FilterOp::Gt => cmp(">"),
        FilterOp::Ge => cmp(">="),
        FilterOp::Contains => Ok(like(&col, false, &format!("%{}%", escape_like(single(f)?)))),
        FilterOp::NotContains => Ok(like(&col, true, &format!("%{}%", escape_like(single(f)?)))),
        FilterOp::StartsWith => Ok(like(&col, false, &format!("{}%", escape_like(single(f)?)))),
        FilterOp::EndsWith => Ok(like(&col, false, &format!("%{}", escape_like(single(f)?)))),
        FilterOp::IsNull => Ok(format!("{col} IS NULL")),
        FilterOp::IsNotNull => Ok(format!("{col} IS NOT NULL")),
        FilterOp::In => {
            if f.values.is_empty() {
                Ok("(1 = 0)".to_string())
            } else {
                let list: Vec<String> = f.values.iter().map(|v| quote_literal(v)).collect();
                Ok(format!("{col} IN ({})", list.join(", ")))
            }
        }
    }
}

/// ` WHERE ...` clause (leading space) or an empty string. `raw_where` goes in parentheses with
/// a newline before the closing one so a trailing comment cannot swallow it.
pub(crate) fn where_clause(filters: &[ColumnFilter], raw_where: Option<&str>) -> Result<String> {
    let mut parts = Vec::new();
    for f in filters {
        parts.push(filter_condition(f)?);
    }
    if let Some(raw) = raw_where.map(str::trim).filter(|r| !r.is_empty()) {
        check_raw_where(raw)?;
        parts.push(format!("({raw}\n)"));
    }
    Ok(if parts.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", parts.join(" AND "))
    })
}

/// ` ORDER BY ...` (leading space): the user's keys, then the table's sorting key text.
pub(crate) fn order_clause(sort: &[SortKey], sorting_key: &str) -> String {
    let mut keys: Vec<String> = sort
        .iter()
        .map(|k| {
            format!(
                "{} {}",
                quote_ident(&k.column),
                if k.descending { "DESC" } else { "ASC" }
            )
        })
        .collect();
    let key = sorting_key.trim();
    if !key.is_empty() {
        keys.push(key.to_string());
    }
    if keys.is_empty() {
        String::new()
    } else {
        format!(" ORDER BY {}", keys.join(", "))
    }
}

pub(crate) fn select_sql(
    db: &str,
    table: &str,
    req: &TableDataRequest,
    sorting_key: &str,
) -> Result<String> {
    Ok(format!(
        "SELECT * FROM {}{}{} LIMIT {} OFFSET {}",
        quote_table(db, table),
        where_clause(&req.filters, req.raw_where.as_deref())?,
        order_clause(&req.sort, sorting_key),
        req.limit,
        req.offset
    ))
}

pub(crate) fn count_sql(db: &str, table: &str, req: &TableCountRequest) -> Result<String> {
    Ok(format!(
        "SELECT count() FROM {}{}",
        quote_table(db, table),
        where_clause(&req.filters, req.raw_where.as_deref())?
    ))
}

/// Validate a user-typed `WHERE` body: one expression, balanced, no `;` outside text/comments.
/// Understands `'..'` and `".."` (backslash escapes), backticks, `-- ` / `#` / `#!` line
/// comments and `/* */` comments.
pub(crate) fn check_raw_where(raw: &str) -> Result<()> {
    let fail = |why: &str| Err(Error::query(format!("raw WHERE rejected: {why}")));
    let c: Vec<char> = raw.chars().collect();
    let mut i = 0;
    let mut depth: i64 = 0;
    while i < c.len() {
        match c[i] {
            q @ ('\'' | '"' | '`') => {
                i += 1;
                loop {
                    let Some(&ch) = c.get(i) else {
                        return fail("unterminated quoted text");
                    };
                    if ch == '\\' {
                        i += 2;
                    } else if ch == q {
                        if c.get(i + 1) == Some(&q) {
                            i += 2;
                        } else {
                            i += 1;
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
            }
            '-' if c.get(i + 1) == Some(&'-') => {
                while i < c.len() && c[i] != '\n' {
                    i += 1;
                }
            }
            '#' => {
                while i < c.len() && c[i] != '\n' {
                    i += 1;
                }
            }
            '/' if c.get(i + 1) == Some(&'*') => {
                i += 2;
                loop {
                    match (c.get(i), c.get(i + 1)) {
                        (Some('*'), Some('/')) => {
                            i += 2;
                            break;
                        }
                        (Some(_), _) => i += 1,
                        (None, _) => return fail("unterminated comment"),
                    }
                }
            }
            '(' => {
                depth += 1;
                i += 1;
            }
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return fail("unbalanced closing parenthesis");
                }
                i += 1;
            }
            ';' => return fail("`;` (multiple statements) is not allowed"),
            _ => i += 1,
        }
    }
    if depth != 0 {
        return fail("unbalanced parentheses");
    }
    Ok(())
}

/// `(database, table)` of a table-like path (folder segments are ignored).
pub(crate) fn table_target(path: &ObjectPath) -> Result<(String, String)> {
    let segs: Vec<_> = path
        .segments
        .iter()
        .filter(|s| s.kind != ObjectKind::Folder)
        .collect();
    match segs.as_slice() {
        [db, t]
            if db.kind == ObjectKind::Database
                && matches!(
                    t.kind,
                    ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView
                ) =>
        {
            Ok((db.name.clone(), t.name.clone()))
        }
        _ => Err(Error::Invalid(
            "path does not identify a table or view (expected database/table)".into(),
        )),
    }
}

// ---------------------------------------------------------------------------------------------
// Typed literals
// ---------------------------------------------------------------------------------------------

fn bad(why: impl Into<String>) -> Error {
    Error::Invalid(why.into())
}

fn numeric_text(v: &Value) -> Result<String> {
    let s = match v {
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Bool(b) => u8::from(*b).to_string(),
        Value::Decimal(s) | Value::Text(s) => s.trim().to_string(),
        Value::Float(f) if f.is_finite() => format!("{f}"),
        other => return Err(bad(format!("`{}` is not a number", value_text(other)))),
    };
    let body = s.strip_prefix(['-', '+']).unwrap_or(&s);
    let ok = !body.is_empty()
        && body.chars().filter(|c| *c == '.').count() <= 1
        && body.chars().all(|c| c.is_ascii_digit() || c == '.')
        && body.chars().any(|c| c.is_ascii_digit());
    if ok {
        Ok(s.trim_start_matches('+').to_string())
    } else {
        Err(bad(format!("`{s}` is not a number")))
    }
}

fn utc_text(v: &Value, precision: u8) -> Result<String> {
    let text = value_text(v);
    let (days, tod, nanos) =
        parse_datetime(&text).ok_or_else(|| bad(format!("`{text}` is not a date or timestamp")))?;
    let p = u32::from(precision.min(9));
    let ticks = (days * 86_400 + tod) * 10i64.pow(p) + i64::from(nanos) / 10i64.pow(9 - p);
    Ok(fmt_timestamp(ticks, precision)
        .trim_end_matches('Z')
        .replace('T', " "))
}

fn compound(v: &Value) -> Result<Value> {
    match v {
        Value::Text(s) | Value::Json(s) => {
            let j: serde_json::Value = serde_json::from_str(s)
                .map_err(|e| bad(format!("expected JSON text for a structured value: {e}")))?;
            Ok(json_value(&j))
        }
        other => Ok(other.clone()),
    }
}

fn json_value(j: &serde_json::Value) -> Value {
    match j {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => n
            .as_i64()
            .map(Value::Int)
            .or_else(|| n.as_u64().map(Value::UInt))
            .unwrap_or_else(|| Value::Float(n.as_f64().unwrap_or(f64::NAN))),
        serde_json::Value::String(s) => Value::Text(s.clone()),
        serde_json::Value::Array(a) => Value::Array(a.iter().map(json_value).collect()),
        serde_json::Value::Object(o) => {
            Value::Document(o.iter().map(|(k, v)| (k.clone(), json_value(v))).collect())
        }
    }
}

/// SQL literal of `v` for a column of type `t`.
pub(crate) fn literal(t: &CType, v: &Value) -> Result<String> {
    if v.is_null() {
        return Ok("NULL".into());
    }
    Ok(match t.inner() {
        CType::Int8
        | CType::Int16
        | CType::Int32
        | CType::Int64
        | CType::Int128
        | CType::Int256
        | CType::UInt8
        | CType::UInt16
        | CType::UInt32
        | CType::UInt64
        | CType::UInt128
        | CType::UInt256 => {
            let n = numeric_text(v)?;
            let n = if n.contains('.') {
                let (int, frac) = n.split_once('.').unwrap_or((&n, ""));
                if frac.chars().any(|c| c != '0') {
                    return Err(bad(format!("`{n}` is not a whole number")));
                }
                int.to_string()
            } else {
                n
            };
            // The VALUES parser wraps around instead of failing: check the range here.
            let bounds: Option<(i128, i128)> = match t.inner() {
                CType::Int8 => Some((i8::MIN.into(), i8::MAX.into())),
                CType::Int16 => Some((i16::MIN.into(), i16::MAX.into())),
                CType::Int32 => Some((i32::MIN.into(), i32::MAX.into())),
                CType::Int64 => Some((i64::MIN.into(), i64::MAX.into())),
                CType::UInt8 => Some((0, u8::MAX.into())),
                CType::UInt16 => Some((0, u16::MAX.into())),
                CType::UInt32 => Some((0, u32::MAX.into())),
                CType::UInt64 => Some((0, u64::MAX.into())),
                _ => None,
            };
            if let Some((lo, hi)) = bounds {
                match n.parse::<i128>() {
                    Ok(x) if (lo..=hi).contains(&x) => {}
                    _ => return Err(bad(format!("`{n}` is out of range for this column"))),
                }
            }
            n
        }
        CType::Float32 | CType::Float64 | CType::BFloat16 => match v {
            Value::Float(f) if f.is_nan() => "nan".into(),
            Value::Float(f) if f.is_infinite() => {
                if *f > 0.0 {
                    "inf".into()
                } else {
                    "-inf".into()
                }
            }
            Value::Text(s) if matches!(s.trim(), "nan" | "NaN") => "nan".into(),
            Value::Text(s) if matches!(s.trim(), "inf" | "Infinity") => "inf".into(),
            Value::Text(s) if matches!(s.trim(), "-inf" | "-Infinity") => "-inf".into(),
            other => numeric_text(other)?,
        },
        CType::Bool => match v {
            Value::Bool(b) => b.to_string(),
            Value::Int(i) => (*i != 0).to_string(),
            Value::Text(s) => match s.trim().to_ascii_lowercase().as_str() {
                "true" | "t" | "1" => "true".into(),
                "false" | "f" | "0" => "false".into(),
                _ => return Err(bad(format!("`{s}` is not a boolean"))),
            },
            other => return Err(bad(format!("`{}` is not a boolean", value_text(other)))),
        },
        CType::FixedString(n) => {
            let inner = match v {
                Value::Bytes(b) => {
                    // Trailing padding NULs of a value read back from the table are not needed.
                    let mut b = b.as_slice();
                    while b.last() == Some(&0) {
                        b = &b[..b.len() - 1];
                    }
                    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
                    format!("unhex('{hex}')")
                }
                Value::Array(_) | Value::Document(_) => {
                    return Err(bad(
                        "a structured value cannot be stored in a FixedString column",
                    ));
                }
                other => quote_literal(&value_text(other)),
            };
            format!("toFixedString({inner}, {n})")
        }
        CType::String => match v {
            Value::Bytes(b) => {
                let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
                format!("unhex('{hex}')")
            }
            Value::Array(_) | Value::Document(_) => {
                return Err(bad(
                    "a structured value cannot be stored in a String column",
                ));
            }
            other => quote_literal(&value_text(other)),
        },
        CType::Uuid => format!("toUUID({})", quote_literal(&value_text(v))),
        CType::IPv4 => format!("toIPv4({})", quote_literal(&value_text(v))),
        CType::IPv6 => format!("toIPv6({})", quote_literal(&value_text(v))),
        CType::Date => format!("toDate({})", quote_literal(&date_part(v)?)),
        CType::Date32 => format!("toDate32({})", quote_literal(&date_part(v)?)),
        CType::DateTime(_) => format!("toDateTime({}, 'UTC')", quote_literal(&utc_text(v, 0)?)),
        CType::DateTime64(p, _) => format!(
            "toDateTime64({}, {p}, 'UTC')",
            quote_literal(&utc_text(v, *p)?)
        ),
        CType::Decimal(p, s) => format!(
            "CAST({} AS Decimal({p}, {s}))",
            quote_literal(&numeric_text(v)?)
        ),
        CType::Enum8(_) | CType::Enum16(_) => quote_literal(&value_text(v)),
        CType::Array(inner) => match compound(v)? {
            Value::Array(items) => {
                let parts = items
                    .iter()
                    .map(|i| literal(inner, i))
                    .collect::<Result<Vec<_>>>()?;
                format!("[{}]", parts.join(", "))
            }
            _ => return Err(bad("expected an array")),
        },
        CType::Tuple(items) => {
            let vals: Vec<Value> = match compound(v)? {
                Value::Array(a) => a,
                Value::Document(d) => d.into_iter().map(|(_, v)| v).collect(),
                _ => return Err(bad("expected a tuple")),
            };
            if vals.len() != items.len() {
                return Err(bad("wrong number of tuple elements"));
            }
            let parts = items
                .iter()
                .zip(&vals)
                .map(|((_, t), v)| literal(t, v))
                .collect::<Result<Vec<_>>>()?;
            format!("({})", parts.join(", "))
        }
        CType::Map(kt, vt) => {
            let pairs: Vec<(Value, Value)> = match compound(v)? {
                Value::Document(d) => d.into_iter().map(|(k, v)| (Value::Text(k), v)).collect(),
                Value::Array(a) => a
                    .into_iter()
                    .map(|p| match p {
                        Value::Array(mut kv) if kv.len() == 2 => {
                            let v = kv.pop().unwrap_or(Value::Null);
                            (kv.pop().unwrap_or(Value::Null), v)
                        }
                        _ => (Value::Null, Value::Null),
                    })
                    .collect(),
                _ => return Err(bad("expected a map")),
            };
            let mut parts = Vec::new();
            for (k, val) in &pairs {
                parts.push(literal(kt, k)?);
                parts.push(literal(vt, val)?);
            }
            format!("map({})", parts.join(", "))
        }
        CType::Json => format!("CAST({} AS JSON)", quote_literal(&value_text(v))),
        CType::Variant(_) | CType::Dynamic | CType::Nothing | CType::Unsupported(_) => {
            return Err(Error::Unsupported(
                "editing this column type is not supported; use an ALTER statement".into(),
            ));
        }
        CType::Nullable(_) | CType::LowCardinality(_) => unreachable!("inner() strips wrappers"),
    })
}

fn date_part(v: &Value) -> Result<String> {
    let text = value_text(v);
    let (days, _, _) =
        parse_datetime(&text).ok_or_else(|| bad(format!("`{text}` is not a date")))?;
    Ok(crate::rowbinary::fmt_date(days))
}

/// Literal of a value whose column type is unknown (generic mapping).
pub(crate) fn literal_untyped(v: &Value) -> Result<String> {
    Ok(match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Float(f) if f.is_finite() => format!("{f:?}"),
        Value::Float(f) => quote_literal(&f.to_string()),
        Value::Decimal(s) => quote_literal(s),
        Value::Bytes(b) => {
            let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
            format!("unhex('{hex}')")
        }
        Value::Array(_) | Value::Document(_) => {
            return Err(bad("structured values need a known column type"));
        }
        other => quote_literal(&value_text(other)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn f(col: &str, op: FilterOp, vals: &[&str]) -> ColumnFilter {
        ColumnFilter {
            column: col.into(),
            op,
            values: vals.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn filters_render() {
        assert_eq!(
            filter_condition(&f("a", FilterOp::Eq, &["x'y"])).unwrap(),
            "`a` = 'x\\'y'"
        );
        assert_eq!(
            filter_condition(&f("a", FilterOp::Contains, &["50%_"])).unwrap(),
            "toString(`a`) ILIKE '%50\\\\%\\\\_%'"
        );
        assert_eq!(
            filter_condition(&f("a", FilterOp::In, &[])).unwrap(),
            "(1 = 0)"
        );
        assert!(filter_condition(&f("a", FilterOp::Eq, &[])).is_err());
    }

    #[test]
    fn select_and_count_statements() {
        let req = TableDataRequest {
            query_id: Uuid::nil(),
            table: ObjectPath::root(),
            filters: vec![f("a", FilterOp::IsNull, &[])],
            raw_where: Some("b > 1 -- c".into()),
            sort: vec![SortKey {
                column: "b".into(),
                descending: true,
            }],
            offset: 10,
            limit: 5,
            batch_size: None,
        };
        assert_eq!(
            select_sql("d", "t", &req, "k1, k2").unwrap(),
            "SELECT * FROM `d`.`t` WHERE `a` IS NULL AND (b > 1 -- c\n) ORDER BY `b` DESC, k1, k2 LIMIT 5 OFFSET 10"
        );
        let c = TableCountRequest {
            table: ObjectPath::root(),
            filters: vec![],
            raw_where: None,
            query_id: None,
        };
        assert_eq!(
            count_sql("d", "t", &c).unwrap(),
            "SELECT count() FROM `d`.`t`"
        );
    }

    #[test]
    fn raw_where_is_checked() {
        assert!(check_raw_where("a = 1 AND b IN (1,2)").is_ok());
        assert!(check_raw_where("a = ';'").is_ok());
        assert!(check_raw_where("a = 1; DROP TABLE t").is_err());
        assert!(check_raw_where("(a = 1").is_err());
        assert!(check_raw_where("a = 1)").is_err());
        assert!(check_raw_where("a = 'x").is_err());
        assert!(check_raw_where("a = 1 /* x").is_err());
        assert!(check_raw_where("a = 'it\\'s;'").is_ok());
    }

    #[test]
    fn typed_literals() {
        let t = |s: &str| CType::parse(s).unwrap();
        assert_eq!(literal(&t("UInt8"), &Value::Int(5)).unwrap(), "5");
        assert!(literal(&t("UInt8"), &Value::Int(256)).is_err());
        assert!(literal(&t("Int8"), &Value::Int(-129)).is_err());
        assert!(literal(&t("UInt64"), &Value::Int(-1)).is_err());
        assert!(literal(&t("UInt8"), &Value::Text("abc".into())).is_err());
        assert_eq!(
            literal(&t("Nullable(String)"), &Value::Null).unwrap(),
            "NULL"
        );
        assert_eq!(
            literal(&t("String"), &Value::Bytes(vec![0, 255])).unwrap(),
            "unhex('00ff')"
        );
        assert_eq!(
            literal(
                &t("DateTime64(3)"),
                &Value::Timestamp("2024-01-02T03:04:05.678Z".into())
            )
            .unwrap(),
            "toDateTime64('2024-01-02 03:04:05.678', 3, 'UTC')"
        );
        assert_eq!(
            literal(&t("Date"), &Value::Date("2024-01-02".into())).unwrap(),
            "toDate('2024-01-02')"
        );
        assert_eq!(
            literal(&t("Decimal(10,2)"), &Value::Decimal("1.5".into())).unwrap(),
            "CAST('1.5' AS Decimal(10, 2))"
        );
        assert_eq!(
            literal(&t("Array(String)"), &Value::Text("[\"a\",\"b\"]".into())).unwrap(),
            "['a', 'b']"
        );
        assert_eq!(
            literal(
                &t("Map(String, UInt8)"),
                &Value::Document(vec![("k".into(), Value::Int(1))])
            )
            .unwrap(),
            "map('k', 1)"
        );
        assert_eq!(
            literal(&t("Bool"), &Value::Text("true".into())).unwrap(),
            "true"
        );
        assert!(literal(&t("Dynamic"), &Value::Int(1)).is_err());
    }

    #[test]
    fn table_targets() {
        let p = ObjectPath::root()
            .child(ObjectKind::Database, "d")
            .child(ObjectKind::Folder, "tables")
            .child(ObjectKind::Table, "t");
        assert_eq!(table_target(&p).unwrap(), ("d".into(), "t".into()));
        assert!(table_target(&ObjectPath::root()).is_err());
    }
}
