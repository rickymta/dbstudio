//! SQL text helpers: quoting, literals, object paths, filters, `WHERE` validation.

use dbstudio_core::{
    ColumnFilter, Error, FilterOp, ObjectKind, ObjectPath, Result, ScriptDialect, SortKey, Value,
    folder,
};
use dbstudio_sql::Dialect;

const DIALECT: Dialect = Dialect(ScriptDialect::MsSql);

/// `[name]` with `]` doubled.
pub fn quote(name: &str) -> String {
    DIALECT.quote_ident(name)
}

/// `N'text'` with quotes doubled.
pub fn lit(text: &str) -> String {
    DIALECT.literal_text(text)
}

/// SQL literal of a typed value.
pub fn value_lit(v: &Value) -> String {
    match v {
        Value::Bool(b) => if *b { "1" } else { "0" }.to_string(),
        Value::Decimal(s) if is_plain_number(s) => s.clone(),
        Value::Bytes(b) if b.is_empty() => "0x".to_string(),
        other => DIALECT.literal(other),
    }
}

fn is_plain_number(s: &str) -> bool {
    let t = s.strip_prefix('-').unwrap_or(s);
    !t.is_empty()
        && t.chars().all(|c| c.is_ascii_digit() || c == '.')
        && t.matches('.').count() <= 1
        && !t.starts_with('.')
        && !t.ends_with('.')
}

/// An object addressed by a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjRef {
    /// Database, when the path has one.
    pub db: Option<String>,
    /// Schema (`dbo` when the path has none).
    pub schema: String,
    /// Object name.
    pub name: String,
    /// Kind of the object (`Table`, `View`, ...).
    pub kind: ObjectKind,
    /// Child segment below the object (column, index, trigger).
    pub child: Option<(ObjectKind, String)>,
}

impl ObjRef {
    /// `[schema].[name]`.
    pub fn qualified(&self) -> String {
        format!("{}.{}", quote(&self.schema), quote(&self.name))
    }

    /// `[db].[schema].[name]` or `[schema].[name]` without a database.
    pub fn full(&self) -> String {
        match &self.db {
            Some(db) => format!("{}.{}", quote(db), self.qualified()),
            None => self.qualified(),
        }
    }
}

/// Kinds that live directly in a schema folder.
pub fn is_schema_object(k: ObjectKind) -> bool {
    matches!(
        k,
        ObjectKind::Table
            | ObjectKind::View
            | ObjectKind::Function
            | ObjectKind::Procedure
            | ObjectKind::Sequence
            | ObjectKind::Synonym
            | ObjectKind::Type
    )
}

/// Resolve a path (with or without `Folder` segments) to an object.
pub fn parse_object(path: &ObjectPath) -> Result<ObjRef> {
    let mut db = None;
    let mut schema = None;
    let mut obj: Option<(ObjectKind, String)> = None;
    let mut child = None;
    for seg in &path.segments {
        match seg.kind {
            ObjectKind::Folder => {}
            ObjectKind::Database => db = Some(seg.name.clone()),
            ObjectKind::Schema => schema = Some(seg.name.clone()),
            k if is_schema_object(k) && obj.is_none() => obj = Some((k, seg.name.clone())),
            k @ (ObjectKind::Column | ObjectKind::Index | ObjectKind::Trigger)
                if obj.is_some() && child.is_none() =>
            {
                child = Some((k, seg.name.clone()));
            }
            // A trigger is listed below its table but belongs to the schema; allow it alone too.
            ObjectKind::Trigger if obj.is_none() => obj = Some((ObjectKind::Trigger, seg.name.clone())),
            other => {
                return Err(Error::NotFound(format!(
                    "unexpected `{other:?}` segment in the object path"
                )));
            }
        }
    }
    let (kind, name) =
        obj.ok_or_else(|| Error::NotFound("the path does not name a schema object".into()))?;
    Ok(ObjRef {
        db,
        schema: schema.unwrap_or_else(|| "dbo".to_string()),
        name,
        kind,
        child,
    })
}

/// `[Database d, Folder schemas, Schema s]`.
pub fn schema_path(db: &str, schema: &str) -> ObjectPath {
    ObjectPath::root()
        .child(ObjectKind::Database, db)
        .child(ObjectKind::Folder, folder::SCHEMAS)
        .child(ObjectKind::Schema, schema)
}

/// Folder key and kind of a schema object kind.
pub fn folder_of(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Table => folder::TABLES,
        ObjectKind::View => folder::VIEWS,
        ObjectKind::Function => folder::FUNCTIONS,
        ObjectKind::Procedure => folder::PROCEDURES,
        ObjectKind::Sequence => folder::SEQUENCES,
        ObjectKind::Synonym => folder::SYNONYMS,
        _ => folder::TYPES,
    }
}

/// Full tree path of a schema object.
pub fn object_path(db: &str, schema: &str, kind: ObjectKind, name: &str) -> ObjectPath {
    schema_path(db, schema)
        .child(ObjectKind::Folder, folder_of(kind))
        .child(kind, name)
}

/// Escape `text` for a `LIKE` pattern with `ESCAPE N'\'`.
pub fn like_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '\\' | '%' | '_' | '[') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Case-insensitive collation used for substring matching.
pub const CI_COLLATION: &str = "Latin1_General_100_CI_AS";

/// One filter as a `WHERE` condition.
pub fn filter_sql(f: &ColumnFilter) -> String {
    let col = quote(&f.column);
    let first = f.values.first().map(String::as_str).unwrap_or("");
    let like = |pattern: String| {
        format!(
            "CAST({col} AS NVARCHAR(MAX)) COLLATE {CI_COLLATION} LIKE {} ESCAPE N'\\'",
            lit(&pattern)
        )
    };
    match f.op {
        FilterOp::Eq => format!("{col} = {}", lit(first)),
        FilterOp::Ne => format!("{col} <> {}", lit(first)),
        FilterOp::Lt => format!("{col} < {}", lit(first)),
        FilterOp::Le => format!("{col} <= {}", lit(first)),
        FilterOp::Gt => format!("{col} > {}", lit(first)),
        FilterOp::Ge => format!("{col} >= {}", lit(first)),
        FilterOp::Contains => like(format!("%{}%", like_escape(first))),
        FilterOp::NotContains => format!(
            "{col} IS NOT NULL AND NOT ({})",
            like(format!("%{}%", like_escape(first)))
        ),
        FilterOp::StartsWith => like(format!("{}%", like_escape(first))),
        FilterOp::EndsWith => like(format!("%{}", like_escape(first))),
        FilterOp::IsNull => format!("{col} IS NULL"),
        FilterOp::IsNotNull => format!("{col} IS NOT NULL"),
        FilterOp::In => {
            if f.values.is_empty() {
                "1 = 0".to_string()
            } else {
                let list: Vec<String> = f.values.iter().map(|v| lit(v)).collect();
                format!("{col} IN ({})", list.join(", "))
            }
        }
    }
}

/// `WHERE` clause from structured filters and a raw expression; empty when there is none.
pub fn where_clause(filters: &[ColumnFilter], raw: Option<&str>) -> Result<String> {
    let mut parts: Vec<String> = filters.iter().map(|f| format!("({})", filter_sql(f))).collect();
    if let Some(raw) = raw.map(str::trim).filter(|r| !r.is_empty()) {
        check_expression(raw)?;
        // The line breaks keep a trailing `--` comment from swallowing the closing parenthesis.
        parts.push(format!("(\n{raw}\n)"));
    }
    Ok(if parts.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", parts.join(" AND "))
    })
}

/// `ORDER BY` of a request: the user's sort, then `key` columns, else a constant (T-SQL needs an
/// `ORDER BY` for `OFFSET`).
pub fn order_by(sort: &[SortKey], key: &[String]) -> String {
    let mut parts: Vec<String> = sort
        .iter()
        .map(|s| format!("{}{}", quote(&s.column), if s.descending { " DESC" } else { "" }))
        .collect();
    for k in key {
        if !sort.iter().any(|s| s.column.eq_ignore_ascii_case(k)) {
            parts.push(quote(k));
        }
    }
    if parts.is_empty() {
        " ORDER BY (SELECT NULL)".to_string()
    } else {
        format!(" ORDER BY {}", parts.join(", "))
    }
}

/// Reject text that is not one expression: a top-level `;`, unbalanced parentheses, a comment
/// opening that is not closed, or an unterminated string/identifier.
pub fn check_expression(text: &str) -> Result<()> {
    let bad = |m: &str| Error::Query {
        message: format!("invalid WHERE expression: {m}"),
        position: None,
    };
    let chars: Vec<char> = text.chars().collect();
    let mut depth = 0i32;
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match c {
            '\'' | '"' | '[' => {
                let close = if c == '[' { ']' } else { c };
                let mut j = i + 1;
                loop {
                    match chars.get(j) {
                        None => return Err(bad("unterminated string or identifier")),
                        Some(&x) if x == close => {
                            if chars.get(j + 1) == Some(&close) {
                                j += 2;
                            } else {
                                break;
                            }
                        }
                        _ => j += 1,
                    }
                }
                i = j;
            }
            '-' if next == Some('-') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            '/' if next == Some('*') => {
                let mut level = 1;
                let mut j = i + 2;
                while j < chars.len() && level > 0 {
                    if chars[j] == '*' && chars.get(j + 1) == Some(&'/') {
                        level -= 1;
                        j += 2;
                    } else if chars[j] == '/' && chars.get(j + 1) == Some(&'*') {
                        level += 1;
                        j += 2;
                    } else {
                        j += 1;
                    }
                }
                if level > 0 {
                    return Err(bad("unterminated comment"));
                }
                i = j;
                continue;
            }
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return Err(bad("unbalanced parentheses"));
                }
            }
            ';' => return Err(bad("only one expression is allowed (found `;`)")),
            _ => {}
        }
        i += 1;
    }
    if depth != 0 {
        return Err(bad("unbalanced parentheses"));
    }
    Ok(())
}

/// Format a type for display: `nvarchar(50)`, `decimal(10,2)`, `datetime2(3)`.
pub fn format_type(name: &str, max_length: i64, precision: i64, scale: i64) -> String {
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "nvarchar" | "nchar" => {
            if max_length < 0 {
                format!("{lower}(max)")
            } else {
                format!("{lower}({})", max_length / 2)
            }
        }
        "varchar" | "char" | "varbinary" | "binary" => {
            if max_length < 0 {
                format!("{lower}(max)")
            } else {
                format!("{lower}({max_length})")
            }
        }
        "decimal" | "numeric" => format!("{lower}({precision},{scale})"),
        "datetime2" | "time" | "datetimeoffset" => format!("{lower}({scale})"),
        "float" if precision != 53 && precision > 0 => format!("float({precision})"),
        _ => lower,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_core::PathSegment;

    #[test]
    fn quoting() {
        assert_eq!(quote("a]b"), "[a]]b]");
        assert_eq!(lit("it's"), "N'it''s'");
        assert_eq!(value_lit(&Value::Bool(true)), "1");
        assert_eq!(value_lit(&Value::Decimal("12.50".into())), "12.50");
        assert_eq!(value_lit(&Value::Decimal("1e5".into())), "N'1e5'");
        assert_eq!(value_lit(&Value::Bytes(vec![])), "0x");
        assert_eq!(value_lit(&Value::Bytes(vec![0xde, 0xad])), "0xdead");
    }

    #[test]
    fn path_with_and_without_folders() {
        let full = object_path("app", "sales", ObjectKind::Table, "orders");
        let r = parse_object(&full).unwrap();
        assert_eq!(r.db.as_deref(), Some("app"));
        assert_eq!((r.schema.as_str(), r.name.as_str()), ("sales", "orders"));
        assert_eq!(r.full(), "[app].[sales].[orders]");
        let bare = ObjectPath {
            segments: vec![
                PathSegment {
                    kind: ObjectKind::Database,
                    name: "app".into(),
                },
                PathSegment {
                    kind: ObjectKind::Table,
                    name: "t".into(),
                },
            ],
        };
        assert_eq!(parse_object(&bare).unwrap().qualified(), "[dbo].[t]");
        let col = full.child(ObjectKind::Folder, "columns").child(ObjectKind::Column, "id");
        assert_eq!(parse_object(&col).unwrap().child, Some((ObjectKind::Column, "id".into())));
        assert!(parse_object(&ObjectPath::root()).is_err());
    }

    #[test]
    fn filters() {
        let f = |op, vals: &[&str]| ColumnFilter {
            column: "na]me".into(),
            op,
            values: vals.iter().map(|s| s.to_string()).collect(),
        };
        assert_eq!(filter_sql(&f(FilterOp::Eq, &["a'b"])), "[na]]me] = N'a''b'");
        assert!(filter_sql(&f(FilterOp::Contains, &["50%_[x"])).contains("N'%50\\%\\_\\[x%'"));
        assert_eq!(filter_sql(&f(FilterOp::In, &[])), "1 = 0");
        assert_eq!(filter_sql(&f(FilterOp::In, &["1", "2"])), "[na]]me] IN (N'1', N'2')");
        assert!(filter_sql(&f(FilterOp::NotContains, &["x"])).starts_with("[na]]me] IS NOT NULL AND NOT"));
        assert_eq!(filter_sql(&f(FilterOp::IsNull, &[])), "[na]]me] IS NULL");
    }

    #[test]
    fn where_expression_checks() {
        assert!(check_expression("a = 1 AND (b = 2 OR c LIKE '%;%')").is_ok());
        assert!(check_expression("a = ']' -- trailing").is_ok());
        assert!(check_expression("a = 1; DROP TABLE t").is_err());
        assert!(check_expression("a = 1) OR (1=1").is_err());
        assert!(check_expression("(a = 1").is_err());
        assert!(check_expression("a = 'x").is_err());
        assert!(check_expression("a /* x").is_err());
        let w = where_clause(&[], Some("a = 1 -- c")).unwrap();
        assert_eq!(w, " WHERE (\na = 1 -- c\n)");
        assert_eq!(where_clause(&[], None).unwrap(), "");
    }

    #[test]
    fn ordering_adds_key() {
        let s = vec![SortKey {
            column: "name".into(),
            descending: true,
        }];
        assert_eq!(
            order_by(&s, &["id".into(), "NAME".into()]),
            " ORDER BY [name] DESC, [id]"
        );
        assert_eq!(order_by(&[], &[]), " ORDER BY (SELECT NULL)");
    }

    #[test]
    fn type_formatting() {
        assert_eq!(format_type("nvarchar", 100, 0, 0), "nvarchar(50)");
        assert_eq!(format_type("nvarchar", -1, 0, 0), "nvarchar(max)");
        assert_eq!(format_type("varchar", 20, 0, 0), "varchar(20)");
        assert_eq!(format_type("decimal", 9, 10, 2), "decimal(10,2)");
        assert_eq!(format_type("datetime2", 8, 27, 7), "datetime2(7)");
        assert_eq!(format_type("int", 4, 10, 0), "int");
        assert_eq!(format_type("float", 8, 53, 0), "float");
        assert_eq!(format_type("float", 4, 24, 0), "float(24)");
    }
}
