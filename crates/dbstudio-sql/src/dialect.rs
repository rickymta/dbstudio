//! Dialect specific quoting and literals (used by SQL export, bind and DDL generation).

use dbstudio_core::{ScriptDialect, Value, ValueKind};

/// Quoting rules of one SQL dialect. Dialects without SQL (Mongo, Redis, Opaque) quote like
/// ANSI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dialect(pub ScriptDialect);

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
            ScriptDialect::MsSql => format!("N'{}'", s.replace('\'', "''")),
            ScriptDialect::Oracle => oracle_text(s),
            _ => format!("'{}'", s.replace('\'', "''")),
        }
    }

    /// SQL literal for `v`. Arrays and documents have no SQL literal yet and render as `NULL`.
    pub fn literal(&self, v: &Value) -> String {
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

    /// Column type used when a new table is created from data of this kind.
    pub fn type_for(&self, kind: ValueKind) -> &'static str {
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

    const MY: ScriptDialect = ScriptDialect::MySql {
        mariadb: false,
        backslash_escapes: true,
    };

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
        let my_no_bs = ScriptDialect::MySql {
            mariadb: true,
            backslash_escapes: false,
        };
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
}
