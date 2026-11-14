//! Dialect specific quoting and literals (used by SQL export, bind and DDL generation).

use dbstudio_core::{ScriptDialect, Value, ValueKind};

/// Quoting rules of one SQL dialect. Dialects without SQL (Mongo, Redis, Opaque) quote like
/// ANSI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dialect(pub ScriptDialect);

/// Dialect of a MySQL / MariaDB session from its `sql_mode` value (`NO_BACKSLASH_ESCAPES`
/// switches off backslash escapes in strings, `ANSI_QUOTES` and the combined mode `ANSI` make
/// `"x"` an identifier instead of a string).
pub fn mysql_dialect_from_sql_mode(sql_mode: &str, mariadb: bool) -> ScriptDialect {
    let has = |name: &str| {
        sql_mode
            .split(',')
            .any(|m| m.trim().eq_ignore_ascii_case(name))
    };
    ScriptDialect::MySql {
        mariadb,
        backslash_escapes: !has("NO_BACKSLASH_ESCAPES"),
        ansi_quotes: has("ANSI_QUOTES") || has("ANSI"),
    }
}

/// Oracle string literals are limited to 4000 bytes; longer text becomes `TO_CLOB(..)||..`.
const ORACLE_CHUNK: usize = 4000;

impl Dialect {
    /// Rules for `d`.
    pub fn new(d: ScriptDialect) -> Self {
        Dialect(d)
    }

    /// Quote an identifier, doubling the closing quote character inside it.
    pub fn quote_ident(&self, name: &str) -> String {
        match self.0 {
            ScriptDialect::MySql { .. } => format!("`{}`", name.replace('`', "``")),
            ScriptDialect::MsSql => format!("[{}]", name.replace(']', "]]")),
            ScriptDialect::ClickHouse => {
                format!("`{}`", name.replace('\\', "\\\\").replace('`', "\\`"))
            }
            _ => format!("\"{}\"", name.replace('"', "\"\"")),
        }
    }

    /// Quoted, escaped string literal.
    pub fn literal_text(&self, s: &str) -> String {
        match self.0 {
            ScriptDialect::MySql {
                backslash_escapes: true,
                ..
            } => format!("'{}'", s.replace('\\', "\\\\").replace('\'', "''")),
            ScriptDialect::ClickHouse => {
                format!("'{}'", s.replace('\\', "\\\\").replace('\'', "''"))
            }
            ScriptDialect::MsSql => format!("N'{}'", s.replace('\'', "''")),
            ScriptDialect::Oracle => oracle_text(s),
            // Safe whatever `standard_conforming_strings` is: with a backslash use the
            // escape string form.
            ScriptDialect::Postgres if s.contains('\\') => {
                format!("E'{}'", s.replace('\\', "\\\\").replace('\'', "''"))
            }
            _ => format!("'{}'", s.replace('\'', "''")),
        }
    }

    /// SQL literal for `v`. Arrays and documents have no SQL literal in the classic SQL dialects
    /// and render as `NULL`; CQL and ClickHouse render them as collections / maps.
    pub fn literal(&self, v: &Value) -> String {
        if matches!(self.0, ScriptDialect::Cql | ScriptDialect::ClickHouse) {
            return self.literal_for_type(v, None);
        }
        match v {
            Value::Null => "NULL".into(),
            Value::Bool(b) => self.bool_literal(*b),
            Value::Int(i) => i.to_string(),
            Value::UInt(u) => u.to_string(),
            Value::Float(f) if f.is_finite() => format!("{f:?}"),
            Value::Float(f) => self.literal_text(&f.to_string()),
            Value::Decimal(s) => {
                if s.parse::<f64>().is_ok_and(f64::is_finite) {
                    s.clone()
                } else {
                    self.literal_text(s)
                }
            }
            Value::Bytes(b) => self.bytes_literal(b),
            Value::Text(s)
            | Value::Date(s)
            | Value::Time(s)
            | Value::DateTime(s)
            | Value::Timestamp(s)
            | Value::Uuid(s)
            | Value::Json(s) => self.literal_text(s),
            Value::Ext(e) => self.literal_text(&e.text),
            Value::Array(_) | Value::Document(_) => "NULL".into(),
        }
    }

    /// Like [`Dialect::literal`], with the native column type as a hint (`map<text,int>`,
    /// `frozen<address>`, `Array(String)`, ...). The hint only matters for CQL and ClickHouse:
    ///
    /// * CQL: `map<k,v>` -> `{'k': v}`, `set<t>` -> `{a, b}`, `list<t>` -> `[a, b]`,
    ///   `tuple<..>` -> `(a, b)`, a user defined type -> `{field: v}`, `uuid`/`timeuuid` bare,
    ///   `blob` -> `0x..`, `timestamp` -> `'..Z'`.
    /// * ClickHouse: `Array(T)` -> `[a, b]`, `Tuple(..)` -> `(a, b)`, `Map(K, V)` ->
    ///   `map('k', v)`, `DateTime` -> `'YYYY-MM-DD hh:mm:ss[.fff]'`.
    ///
    /// Without a hint (or for another dialect) the value is rendered by its own kind.
    pub fn literal_for_type(&self, v: &Value, native_type: Option<&str>) -> String {
        match self.0 {
            ScriptDialect::Cql => cql_literal(self, v, native_type),
            ScriptDialect::ClickHouse => ch_literal(self, v, native_type),
            _ => self.literal(v),
        }
    }

    /// Column type used when a new table is created from data of this kind.
    pub fn type_for(&self, kind: ValueKind) -> &'static str {
        if let Some(t) = match self.0 {
            ScriptDialect::Cql => Some(cql_type(kind)),
            ScriptDialect::ClickHouse => Some(ch_type(kind)),
            _ => None,
        } {
            return t;
        }
        let (pg, my, ms, ora) = match kind {
            ValueKind::Bool => ("boolean", "tinyint(1)", "bit", "number(1)"),
            ValueKind::Int | ValueKind::UInt => ("bigint", "bigint", "bigint", "number(19)"),
            ValueKind::Float => ("double precision", "double", "float", "binary_double"),
            ValueKind::Decimal => ("numeric", "decimal(38,10)", "decimal(38,10)", "number"),
            ValueKind::Bytes => ("bytea", "longblob", "varbinary(max)", "blob"),
            ValueKind::Date => ("date", "date", "date", "date"),
            ValueKind::Time => ("time", "time", "time", "varchar2(32)"),
            ValueKind::DateTime => ("timestamp", "datetime(6)", "datetime2", "timestamp"),
            ValueKind::Timestamp => (
                "timestamptz",
                "datetime(6)",
                "datetimeoffset",
                "timestamp with time zone",
            ),
            ValueKind::Uuid => ("uuid", "char(36)", "uniqueidentifier", "varchar2(36)"),
            ValueKind::Json => ("jsonb", "json", "nvarchar(max)", "clob"),
            ValueKind::Null
            | ValueKind::Text
            | ValueKind::Array
            | ValueKind::Document
            | ValueKind::Ext => ("text", "longtext", "nvarchar(max)", "clob"),
        };
        match self.0 {
            ScriptDialect::MySql { .. } => my,
            ScriptDialect::MsSql => ms,
            ScriptDialect::Oracle => ora,
            _ => pg,
        }
    }

    fn bool_literal(&self, b: bool) -> String {
        match (self.0, b) {
            (ScriptDialect::MsSql | ScriptDialect::Oracle, true) => "1".into(),
            (ScriptDialect::MsSql | ScriptDialect::Oracle, false) => "0".into(),
            (ScriptDialect::MySql { .. }, true) => "TRUE".into(),
            (ScriptDialect::MySql { .. }, false) => "FALSE".into(),
            (_, true) => "true".into(),
            (_, false) => "false".into(),
        }
    }

    fn bytes_literal(&self, b: &[u8]) -> String {
        let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
        match self.0 {
            ScriptDialect::MySql { .. } => format!("X'{hex}'"),
            ScriptDialect::MsSql => format!("0x{hex}"),
            ScriptDialect::Oracle => format!("HEXTORAW('{hex}')"),
            _ => format!("'\\x{hex}'::bytea"),
        }
    }
}

fn cql_type(kind: ValueKind) -> &'static str {
    match kind {
        ValueKind::Bool => "boolean",
        ValueKind::Int | ValueKind::UInt => "bigint",
        ValueKind::Float => "double",
        ValueKind::Decimal => "decimal",
        ValueKind::Bytes => "blob",
        ValueKind::Date => "date",
        ValueKind::Time => "time",
        ValueKind::DateTime | ValueKind::Timestamp => "timestamp",
        ValueKind::Uuid => "uuid",
        _ => "text",
    }
}

fn ch_type(kind: ValueKind) -> &'static str {
    match kind {
        ValueKind::Bool => "Bool",
        ValueKind::Int => "Int64",
        ValueKind::UInt => "UInt64",
        ValueKind::Float => "Float64",
        ValueKind::Decimal => "Decimal(38, 10)",
        ValueKind::Date => "Date",
        ValueKind::DateTime => "DateTime64(6)",
        ValueKind::Timestamp => "DateTime64(6, 'UTC')",
        ValueKind::Uuid => "UUID",
        _ => "String",
    }
}

/// Split a type like `map<text, frozen<list<int>>>` or `Map(String, Array(Int32))` into its
/// lower-cased base name and top-level arguments (quotes and nesting respected).
fn type_parts(ty: &str) -> (String, Vec<String>) {
    let ty = ty.trim();
    let open = ty.find(['<', '(']);
    let Some(o) = open else {
        return (ty.to_ascii_lowercase(), Vec::new());
    };
    let base = ty[..o].trim().to_ascii_lowercase();
    let close = if ty.as_bytes()[o] == b'<' { '>' } else { ')' };
    let inner_end = ty.rfind(close).filter(|e| *e > o).unwrap_or(ty.len());
    let inner = &ty[o + 1..inner_end];
    let mut args = Vec::new();
    let (mut depth, mut quote, mut cur) = (0i32, None::<char>, String::new());
    for c in inner.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => {
                quote = None;
                cur.push(c);
            }
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                cur.push(c);
            }
            (None, '<' | '(') => {
                depth += 1;
                cur.push(c);
            }
            (None, '>' | ')') => {
                depth -= 1;
                cur.push(c);
            }
            (None, ',') if depth == 0 => {
                args.push(cur.trim().to_string());
                cur.clear();
            }
            (None, c) => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        args.push(cur.trim().to_string());
    }
    (base, args)
}

/// Strip the wrappers that do not change how a literal is written.
fn unwrap_type(ty: Option<&str>, wrappers: &[&str]) -> Option<(String, Vec<String>)> {
    let mut parts = type_parts(ty?);
    while wrappers.contains(&parts.0.as_str()) && parts.1.len() == 1 {
        parts = type_parts(&parts.1[0].clone());
    }
    Some(parts)
}

fn looks_like_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

fn is_plain_ident(s: &str) -> bool {
    let mut c = s.chars();
    c.next()
        .is_some_and(|f| f.is_ascii_alphabetic() || f == '_')
        && c.all(|x| x.is_ascii_alphanumeric() || x == '_')
}

/// Does an ISO text end with a zone (`Z`, `+hh`, `-hh:mm`)?
fn has_zone(s: &str) -> bool {
    if s.ends_with('Z') || s.ends_with('z') {
        return true;
    }
    let Some(t) = s.find(['T', ' ']) else {
        return false;
    };
    s[t..].rfind(['+', '-']).is_some()
}

/// `[-]` followed by `<digits><unit>` groups (`y mo w d h m s ms us µs ns`), or ISO 8601
/// (`P1Y2M3DT4H`, `P0001-02-03T04:05:06`): the forms CQL accepts without quotes.
fn is_duration_literal(s: &str) -> bool {
    let t = s.strip_prefix('-').unwrap_or(s);
    if t.is_empty() {
        return false;
    }
    if let Some(iso) = t.strip_prefix('P') {
        return !iso.is_empty()
            && iso.chars().all(|c| {
                c.is_ascii_digit()
                    || matches!(c, 'Y' | 'M' | 'W' | 'D' | 'T' | 'H' | 'S' | '-' | ':')
            });
    }
    let mut rest = t;
    while !rest.is_empty() {
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        if digits == 0 {
            return false;
        }
        rest = &rest[digits..];
        let unit = ["ms", "mo", "us", "µs", "ns", "y", "w", "d", "h", "m", "s"]
            .iter()
            .find(|u| rest.starts_with(**u));
        match unit {
            Some(u) => rest = &rest[u.len()..],
            None => return false,
        }
    }
    true
}

fn cql_literal(d: &Dialect, v: &Value, ty: Option<&str>) -> String {
    let parts = unwrap_type(ty, &["frozen"]);
    let (base, args) = parts.clone().unwrap_or_default();
    let arg = |i: usize| args.get(i).map(String::as_str);
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => if *b { "true" } else { "false" }.into(),
        Value::Float(f) if f.is_nan() => "NaN".into(),
        Value::Float(f) if f.is_infinite() => {
            if *f > 0.0 { "Infinity" } else { "-Infinity" }.into()
        }
        Value::Bytes(b) => {
            let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
            format!("0x{hex}")
        }
        Value::Uuid(s) | Value::Text(s)
            if looks_like_uuid(s)
                && (base.starts_with("uuid")
                    || base == "timeuuid"
                    || matches!(v, Value::Uuid(_))) =>
        {
            s.clone()
        }
        // A duration is a bare literal (`1h30m`, `14mo3d5ns`, `P1Y2M`); quoted it is rejected.
        Value::Text(s) if base == "duration" && is_duration_literal(s) => s.clone(),
        Value::Ext(e) if e.ty == "duration" && is_duration_literal(&e.text) => e.text.clone(),
        Value::DateTime(s) => {
            let t = if has_zone(s) {
                s.clone()
            } else {
                format!("{s}Z")
            };
            d.literal_text(&t)
        }
        Value::Array(items) => {
            let each = |i: usize| items.get(i).map(|x| cql_literal(d, x, arg(0)));
            let body = |f: &dyn Fn(usize) -> Option<String>| {
                (0..items.len())
                    .filter_map(f)
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            match base.as_str() {
                "set" => format!("{{{}}}", body(&each)),
                "tuple" => format!(
                    "({})",
                    body(&|i| items.get(i).map(|x| cql_literal(d, x, arg(i))))
                ),
                _ => format!("[{}]", body(&each)),
            }
        }
        Value::Document(fields) => {
            let is_map = base == "map" || parts.is_none();
            let body: Vec<String> = fields
                .iter()
                .map(|(k, x)| {
                    if is_map {
                        let key = cql_literal(d, &Value::Text(k.clone()), arg(0));
                        format!("{key}: {}", cql_literal(d, x, arg(1)))
                    } else {
                        let name = if is_plain_ident(k) {
                            k.clone()
                        } else {
                            d.quote_ident(k)
                        };
                        format!("{name}: {}", cql_literal(d, x, None))
                    }
                })
                .collect();
            format!("{{{}}}", body.join(", "))
        }
        Value::Ext(e) => d.literal_text(&e.text),
        other => generic_literal(d, other),
    }
}

/// Double literal for ClickHouse. The `VALUES` parser reads a decimal literal with 16 or 17
/// significant digits a unit in the last place off for some doubles (2.857142857142857 becomes
/// 2.8571428571428568), so those are written as their exact bit pattern; short literals are
/// read exactly.
fn ch_float(f: f64) -> String {
    let exp = format!("{f:e}");
    let (mantissa, e10) = exp.split_once('e').unwrap_or((&exp, "0"));
    let digits = mantissa.chars().filter(char::is_ascii_digit).count();
    let e10: i32 = e10.parse().unwrap_or(0);
    if digits <= 15 && (-22..=22).contains(&e10) {
        format!("{f:?}")
    } else {
        format!("reinterpretAsFloat64(toUInt64({}))", f.to_bits())
    }
}

fn ch_literal(d: &Dialect, v: &Value, ty: Option<&str>) -> String {
    let parts = unwrap_type(ty, &["nullable", "lowcardinality"]);
    let (base, args) = parts.unwrap_or_default();
    let arg = |i: usize| args.get(i).map(String::as_str);
    match v {
        Value::Null => "NULL".into(),
        Value::Float(f) if f.is_nan() => "nan".into(),
        Value::Float(f) if f.is_infinite() => if *f > 0.0 { "inf" } else { "-inf" }.into(),
        Value::Float(f) => ch_float(*f),
        Value::Bytes(b) => {
            let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
            format!("unhex('{hex}')")
        }
        Value::DateTime(s) | Value::Timestamp(s) => {
            let t = s.replacen('T', " ", 1);
            let t = t.strip_suffix(['Z', 'z']).unwrap_or(&t);
            d.literal_text(t)
        }
        Value::Array(items) => {
            let tuple = base == "tuple";
            let body: Vec<String> = items
                .iter()
                .map(|x| ch_literal(d, x, if tuple { None } else { arg(0) }))
                .collect();
            if tuple {
                format!("({})", body.join(", "))
            } else {
                format!("[{}]", body.join(", "))
            }
        }
        Value::Document(fields) => {
            let mut out: Vec<String> = Vec::with_capacity(fields.len() * 2);
            for (k, x) in fields {
                out.push(ch_literal(d, &Value::Text(k.clone()), arg(0)));
                out.push(ch_literal(d, x, arg(1)));
            }
            format!("map({})", out.join(", "))
        }
        Value::Ext(e) => d.literal_text(&e.text),
        other => generic_literal(d, other),
    }
}

/// Scalars that are written the same way with or without a type hint.
fn generic_literal(d: &Dialect, v: &Value) -> String {
    match v {
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Float(f) => format!("{f:?}"),
        Value::Decimal(s) => {
            if s.parse::<f64>().is_ok_and(f64::is_finite) {
                s.clone()
            } else {
                d.literal_text(s)
            }
        }
        Value::Text(s)
        | Value::Date(s)
        | Value::Time(s)
        | Value::DateTime(s)
        | Value::Timestamp(s)
        | Value::Uuid(s)
        | Value::Json(s) => d.literal_text(s),
        Value::Bool(b) => if *b { "true" } else { "false" }.into(),
        _ => "NULL".into(),
    }
}

fn oracle_text(s: &str) -> String {
    if s.len() <= ORACLE_CHUNK {
        return format!("'{}'", s.replace('\'', "''"));
    }
    let chars: Vec<char> = s.chars().collect();
    // 1000 chars <= 4000 bytes even at 4 bytes per char.
    let parts: Vec<String> = chars
        .chunks(1000)
        .map(|c| {
            let part: String = c.iter().collect();
            format!("TO_CLOB('{}')", part.replace('\'', "''"))
        })
        .collect();
    parts.join("||")
}

#[cfg(test)]
mod tests {
    use super::*;

    const MY: ScriptDialect = ScriptDialect::mysql(false, true);

    #[test]
    fn quote_ident_per_dialect() {
        assert_eq!(
            Dialect(ScriptDialect::Postgres).quote_ident("a\"b"),
            "\"a\"\"b\""
        );
        assert_eq!(Dialect(MY).quote_ident("a`b"), "`a``b`");
        assert_eq!(Dialect(ScriptDialect::MsSql).quote_ident("a]b"), "[a]]b]");
        assert_eq!(Dialect(ScriptDialect::Oracle).quote_ident("Emp"), "\"Emp\"");
    }

    #[test]
    fn literal_text_per_dialect() {
        assert_eq!(
            Dialect(ScriptDialect::Postgres).literal_text("it's"),
            "'it''s'"
        );
        assert_eq!(Dialect(MY).literal_text("a\\b'c"), "'a\\\\b''c'");
        let my_no_bs = ScriptDialect::mysql(true, false);
        assert_eq!(Dialect(my_no_bs).literal_text("a\\b"), "'a\\b'");
        assert_eq!(Dialect(ScriptDialect::MsSql).literal_text("x"), "N'x'");
        let long = "a".repeat(4001);
        let lit = Dialect(ScriptDialect::Oracle).literal_text(&long);
        assert!(lit.starts_with("TO_CLOB('") && lit.contains("')||TO_CLOB('"));
    }

    #[test]
    fn literal_values() {
        let pg = Dialect(ScriptDialect::Postgres);
        assert_eq!(pg.literal(&Value::Null), "NULL");
        assert_eq!(pg.literal(&Value::Bool(true)), "true");
        assert_eq!(pg.literal(&Value::Int(-5)), "-5");
        assert_eq!(pg.literal(&Value::Float(1.5)), "1.5");
        assert_eq!(pg.literal(&Value::Decimal("12.50".into())), "12.50");
        assert_eq!(
            pg.literal(&Value::Bytes(vec![0xde, 0xad])),
            "'\\xdead'::bytea"
        );
        assert_eq!(
            pg.literal(&Value::Date("2024-05-31".into())),
            "'2024-05-31'"
        );
        assert_eq!(
            Dialect(ScriptDialect::MsSql).literal(&Value::Bool(false)),
            "0"
        );
        assert_eq!(Dialect(MY).literal(&Value::Bytes(vec![1])), "X'01'");
        assert_eq!(
            Dialect(ScriptDialect::Oracle).literal(&Value::Bytes(vec![1])),
            "HEXTORAW('01')"
        );
        assert_eq!(pg.literal(&Value::Array(vec![])), "NULL");
    }

    #[test]
    fn type_for_per_dialect() {
        assert_eq!(
            Dialect(ScriptDialect::Postgres).type_for(ValueKind::Int),
            "bigint"
        );
        assert_eq!(Dialect(MY).type_for(ValueKind::Text), "longtext");
        assert_eq!(
            Dialect(ScriptDialect::MsSql).type_for(ValueKind::Uuid),
            "uniqueidentifier"
        );
        assert_eq!(
            Dialect(ScriptDialect::Oracle).type_for(ValueKind::Bool),
            "number(1)"
        );
    }

    #[test]
    fn literals_are_single_tokens() {
        // Whatever the content, a text literal lexes as exactly one string token.
        for d in [
            ScriptDialect::Postgres,
            MY,
            ScriptDialect::MsSql,
            ScriptDialect::Oracle,
        ] {
            for s in [
                "",
                "plain",
                "it's",
                "a\\b",
                "'; drop table x; --",
                "\\'",
                "é😀",
                "a\nb",
                "\"q\"",
            ] {
                let lit = Dialect(d).literal_text(s);
                let toks: Vec<_> = crate::lexer::tokenize(d, &lit)
                    .into_iter()
                    .filter(|t| t.kind != crate::lexer::TokenKind::Whitespace)
                    .collect();
                assert_eq!(toks.len(), 1, "{d:?} {s:?} -> {lit}");
                assert_eq!(toks[0].kind, crate::lexer::TokenKind::String);
                assert_eq!(crate::split(d, &lit).len(), 1);
            }
        }
    }

    #[test]
    fn identifiers_are_single_tokens() {
        for d in [
            ScriptDialect::Postgres,
            MY,
            ScriptDialect::MsSql,
            ScriptDialect::Oracle,
        ] {
            for s in ["a", "we ird", "a\"b", "a`b", "a]b", "x;y", "é"] {
                let q = Dialect(d).quote_ident(s);
                let toks: Vec<_> = crate::lexer::tokenize(d, &q);
                assert_eq!(toks.len(), 1, "{d:?} {s:?} -> {q}");
                assert_eq!(toks[0].kind, crate::lexer::TokenKind::QuotedIdent);
            }
        }
    }

    #[test]
    fn more_value_literals() {
        let pg = Dialect(ScriptDialect::Postgres);
        assert_eq!(pg.literal(&Value::UInt(7)), "7");
        assert_eq!(pg.literal(&Value::Float(f64::NAN)), "'NaN'");
        assert_eq!(pg.literal(&Value::Float(f64::INFINITY)), "'inf'");
        assert_eq!(pg.literal(&Value::Decimal("NaN".into())), "'NaN'");
        assert_eq!(pg.literal(&Value::Uuid("u'x".into())), "'u''x'");
        assert_eq!(Dialect(MY).literal(&Value::Bool(true)), "TRUE");
        assert_eq!(
            Dialect(ScriptDialect::MsSql).literal(&Value::Bytes(vec![1, 255])),
            "0x01ff"
        );
        assert_eq!(
            Dialect(ScriptDialect::Oracle).literal(&Value::Bool(true)),
            "1"
        );
    }

    #[test]
    fn postgres_literal_with_backslash_is_an_escape_string() {
        let pg = Dialect(ScriptDialect::Postgres);
        assert_eq!(pg.literal_text("a\\b"), "E'a\\\\b'");
        assert_eq!(
            pg.literal_text("\\'; drop table x; --"),
            "E'\\\\''; drop table x; --'"
        );
        assert_eq!(pg.literal_text("plain"), "'plain'");
        // other dialects are unchanged
        assert_eq!(
            Dialect(ScriptDialect::MsSql).literal_text("a\\b"),
            "N'a\\b'"
        );
    }

    #[test]
    fn sql_mode_to_dialect() {
        let d = |m, mariadb| mysql_dialect_from_sql_mode(m, mariadb);
        assert_eq!(d("", false), MY);
        assert_eq!(d("STRICT_TRANS_TABLES,ONLY_FULL_GROUP_BY", false), MY);
        assert_eq!(
            d("ANSI,NO_BACKSLASH_ESCAPES", true),
            ScriptDialect::MySql {
                mariadb: true,
                backslash_escapes: false,
                ansi_quotes: true
            }
        );
        assert_eq!(
            d("only_full_group_by, ansi_quotes", false),
            ScriptDialect::MySql {
                mariadb: false,
                backslash_escapes: true,
                ansi_quotes: true
            }
        );
        assert_eq!(
            d("no_backslash_escapes", false),
            ScriptDialect::mysql(false, false)
        );
    }

    #[test]
    fn clickhouse_doubles_with_long_digits_are_written_exactly() {
        let ch = Dialect::new(ScriptDialect::ClickHouse);
        // Short literals are read exactly by the VALUES parser.
        assert_eq!(ch.literal_for_type(&Value::Float(1.5), None), "1.5");
        assert_eq!(ch.literal_for_type(&Value::Float(0.1), None), "0.1");
        // 16-17 digit literals are not: they travel as their bit pattern.
        let v = 20.0_f64 / 7.0;
        assert_eq!(
            ch.literal_for_type(&Value::Float(v), Some("Float64")),
            format!("reinterpretAsFloat64(toUInt64({}))", v.to_bits())
        );
        assert_eq!(ch.literal_for_type(&Value::Float(f64::NAN), None), "nan");
    }

    #[test]
    fn cql_durations_are_bare_literals() {
        use dbstudio_core::ExtValue;
        let cql = Dialect::new(ScriptDialect::Cql);
        let ext = Value::Ext(ExtValue::new("duration", "14mo3d14400000000000ns"));
        assert_eq!(
            cql.literal_for_type(&ext, Some("duration")),
            "14mo3d14400000000000ns"
        );
        assert_eq!(
            cql.literal_for_type(&Value::Text("1h30m".into()), Some("duration")),
            "1h30m"
        );
        assert_eq!(
            cql.literal_for_type(&Value::Text("P1Y2M3DT4H".into()), Some("duration")),
            "P1Y2M3DT4H"
        );
        // Anything that is not a duration stays a quoted string (no injection through a cell).
        assert_eq!(
            cql.literal_for_type(&Value::Text("1h'; DROP".into()), Some("duration")),
            "'1h''; DROP'"
        );
    }
}
