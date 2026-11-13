//! Decoding of MySQL / MariaDB result cells into [`dbstudio_core::Value`].
//!
//! The driver uses the **text protocol**, where the server sends every non-NULL cell as a
//! string. Precision is therefore never lost: `decimal(38,10)` stays exact, `bigint unsigned`
//! keeps all 64 bits, and `datetime(6)` keeps its microseconds. Decoding is driven by the column
//! type, the `UNSIGNED` flag and the character set (63 = binary).
//!
//! Mapping decisions (documented because the contract leaves them open):
//!
//! * `timestamp` is decoded as [`Value::DateTime`] (not `Timestamp`): the server renders it in
//!   the session time zone without any zone marker, so claiming an instant would be wrong.
//! * `time` keeps the server's text (`-838:59:59`, `100:00:00.5`) in [`Value::Time`]; it can be
//!   negative or longer than 24 hours.
//! * Zero dates (`0000-00-00`, `2020-00-10 ...`) cannot be valid ISO dates and are returned as
//!   [`Value::Text`] with the server text.
//! * `bit(n)` becomes [`Value::UInt`] (big-endian bytes, at most 64 bits).
//! * `geometry` (and `vector`) become [`Value::Bytes`] (internal WKB with the 4-byte SRID).
//! * MariaDB's `json` is an alias of `longtext`, so it arrives as [`Value::Text`]; MySQL `json`
//!   becomes [`Value::Json`] (falling back to text when the payload does not parse).
//! * `enum` / `set` become [`Value::Text`]. Text that is not valid UTF-8 is converted lossily.
//! * Anything unknown falls back to text, or bytes when the column is binary.

use dbstudio_core::{Value, ValueKind};
use mysql_async::Column;
use mysql_async::consts::{ColumnFlags, ColumnType};

/// The binary pseudo character set: columns with it hold raw bytes.
const BINARY_CHARSET: u16 = 63;

/// The facts about a result column that drive decoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColInfo {
    /// Wire type.
    pub ty: ColumnType,
    /// `UNSIGNED` flag.
    pub unsigned: bool,
    /// Character set id is 63 (`binary`).
    pub binary: bool,
    /// `ENUM` flag.
    pub is_enum: bool,
    /// `SET` flag.
    pub is_set: bool,
    /// The column may hold NULL (`NOT NULL` flag absent).
    pub nullable: bool,
}

impl ColInfo {
    /// Extract the decoding facts from a protocol column definition.
    pub fn of(col: &Column) -> Self {
        let flags = col.flags();
        ColInfo {
            ty: col.column_type(),
            unsigned: flags.contains(ColumnFlags::UNSIGNED_FLAG),
            binary: col.character_set() == BINARY_CHARSET,
            is_enum: flags.contains(ColumnFlags::ENUM_FLAG),
            is_set: flags.contains(ColumnFlags::SET_FLAG),
            nullable: !flags.contains(ColumnFlags::NOT_NULL_FLAG),
        }
    }

    /// The [`ValueKind`] cells of this column decode into (never `Null`/`Array`/`Document`).
    pub fn value_kind(&self) -> ValueKind {
        use ColumnType::*;
        match self.ty {
            MYSQL_TYPE_TINY | MYSQL_TYPE_SHORT | MYSQL_TYPE_INT24 | MYSQL_TYPE_LONG
            | MYSQL_TYPE_LONGLONG => {
                if self.unsigned {
                    ValueKind::UInt
                } else {
                    ValueKind::Int
                }
            }
            MYSQL_TYPE_YEAR => ValueKind::Int,
            MYSQL_TYPE_BIT => ValueKind::UInt,
            MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => ValueKind::Decimal,
            MYSQL_TYPE_FLOAT | MYSQL_TYPE_DOUBLE => ValueKind::Float,
            MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => ValueKind::Date,
            MYSQL_TYPE_DATETIME
            | MYSQL_TYPE_DATETIME2
            | MYSQL_TYPE_TIMESTAMP
            | MYSQL_TYPE_TIMESTAMP2 => ValueKind::DateTime,
            MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => ValueKind::Time,
            MYSQL_TYPE_JSON => ValueKind::Json,
            MYSQL_TYPE_GEOMETRY | MYSQL_TYPE_VECTOR => ValueKind::Bytes,
            MYSQL_TYPE_ENUM | MYSQL_TYPE_SET => ValueKind::Text,
            MYSQL_TYPE_NULL => ValueKind::Text,
            _ => {
                if self.binary && !self.is_enum && !self.is_set {
                    ValueKind::Bytes
                } else {
                    ValueKind::Text
                }
            }
        }
    }

    /// MySQL style type name (`bigint unsigned`, `varchar`, `longblob`, ...). Length and
    /// precision are not part of the wire metadata, so this is the bare type name.
    pub fn type_name(&self) -> String {
        use ColumnType::*;
        let base = match self.ty {
            MYSQL_TYPE_TINY => "tinyint",
            MYSQL_TYPE_SHORT => "smallint",
            MYSQL_TYPE_INT24 => "mediumint",
            MYSQL_TYPE_LONG => "int",
            MYSQL_TYPE_LONGLONG => "bigint",
            MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => "decimal",
            MYSQL_TYPE_FLOAT => "float",
            MYSQL_TYPE_DOUBLE => "double",
            MYSQL_TYPE_NULL => "null",
            MYSQL_TYPE_TIMESTAMP | MYSQL_TYPE_TIMESTAMP2 => "timestamp",
            MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => "date",
            MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => "time",
            MYSQL_TYPE_DATETIME | MYSQL_TYPE_DATETIME2 => "datetime",
            MYSQL_TYPE_YEAR => "year",
            MYSQL_TYPE_BIT => "bit",
            MYSQL_TYPE_JSON => "json",
            MYSQL_TYPE_ENUM => "enum",
            MYSQL_TYPE_SET => "set",
            MYSQL_TYPE_GEOMETRY => "geometry",
            MYSQL_TYPE_VECTOR => "vector",
            MYSQL_TYPE_TINY_BLOB => {
                if self.binary {
                    "tinyblob"
                } else {
                    "tinytext"
                }
            }
            MYSQL_TYPE_MEDIUM_BLOB => {
                if self.binary {
                    "mediumblob"
                } else {
                    "mediumtext"
                }
            }
            MYSQL_TYPE_LONG_BLOB => {
                if self.binary {
                    "longblob"
                } else {
                    "longtext"
                }
            }
            MYSQL_TYPE_BLOB => {
                if self.binary {
                    "blob"
                } else {
                    "text"
                }
            }
            MYSQL_TYPE_VARCHAR | MYSQL_TYPE_VAR_STRING => {
                if self.binary {
                    "varbinary"
                } else {
                    "varchar"
                }
            }
            MYSQL_TYPE_STRING => {
                if self.is_enum {
                    "enum"
                } else if self.is_set {
                    "set"
                } else if self.binary {
                    "binary"
                } else {
                    "char"
                }
            }
            _ => "unknown",
        };
        let numeric = matches!(
            self.ty,
            MYSQL_TYPE_TINY
                | MYSQL_TYPE_SHORT
                | MYSQL_TYPE_INT24
                | MYSQL_TYPE_LONG
                | MYSQL_TYPE_LONGLONG
                | MYSQL_TYPE_DECIMAL
                | MYSQL_TYPE_NEWDECIMAL
                | MYSQL_TYPE_FLOAT
                | MYSQL_TYPE_DOUBLE
        );
        if numeric && self.unsigned {
            format!("{base} unsigned")
        } else {
            base.to_string()
        }
    }
}

/// Decode one non-NULL text-protocol cell. Never fails and never panics.
pub fn decode_cell(info: &ColInfo, bytes: Vec<u8>) -> Value {
    use ColumnType::*;
    match info.ty {
        MYSQL_TYPE_TINY | MYSQL_TYPE_SHORT | MYSQL_TYPE_INT24 | MYSQL_TYPE_LONG
        | MYSQL_TYPE_LONGLONG | MYSQL_TYPE_YEAR => decode_int(info, bytes),
        MYSQL_TYPE_BIT => {
            if bytes.len() <= 8 {
                Value::UInt(bytes.iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b)))
            } else {
                Value::Bytes(bytes)
            }
        }
        MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => text_or(bytes, Value::Decimal),
        MYSQL_TYPE_FLOAT | MYSQL_TYPE_DOUBLE => match std::str::from_utf8(&bytes)
            .ok()
            .and_then(|s| s.trim().parse::<f64>().ok())
        {
            Some(f) => Value::Float(f),
            None => lossy_text(bytes),
        },
        MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => {
            let s = lossy(bytes);
            if is_valid_date(&s) {
                Value::Date(s)
            } else {
                Value::Text(s)
            }
        }
        MYSQL_TYPE_DATETIME
        | MYSQL_TYPE_DATETIME2
        | MYSQL_TYPE_TIMESTAMP
        | MYSQL_TYPE_TIMESTAMP2 => {
            let s = lossy(bytes);
            if s.len() >= 10 && is_valid_date(&s[..10]) {
                Value::DateTime(s.replacen(' ', "T", 1))
            } else {
                Value::Text(s)
            }
        }
        MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => text_or(bytes, Value::Time),
        MYSQL_TYPE_JSON => {
            // Keep the server's original JSON text (numbers keep their exact digits).
            if serde_json::from_slice::<serde::de::IgnoredAny>(&bytes).is_ok() {
                Value::Json(lossy(bytes))
            } else {
                lossy_text(bytes)
            }
        }
        MYSQL_TYPE_GEOMETRY | MYSQL_TYPE_VECTOR => Value::Bytes(bytes),
        MYSQL_TYPE_ENUM | MYSQL_TYPE_SET | MYSQL_TYPE_NULL => lossy_text(bytes),
        _ => {
            if info.binary && !info.is_enum && !info.is_set {
                Value::Bytes(bytes)
            } else {
                lossy_text(bytes)
            }
        }
    }
}

fn decode_int(info: &ColInfo, bytes: Vec<u8>) -> Value {
    let parsed = std::str::from_utf8(&bytes)
        .ok()
        .map(str::trim)
        .and_then(|s| {
            // YEAR is flagged unsigned by the server but is a plain small integer.
            if info.unsigned && info.ty != ColumnType::MYSQL_TYPE_YEAR {
                s.parse::<u64>().ok().map(Value::UInt)
            } else {
                s.parse::<i64>().ok().map(Value::Int)
            }
        });
    parsed.unwrap_or_else(|| lossy_text(bytes))
}

fn lossy(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

fn lossy_text(bytes: Vec<u8>) -> Value {
    Value::Text(lossy(bytes))
}

fn text_or(bytes: Vec<u8>, make: fn(String) -> Value) -> Value {
    make(lossy(bytes))
}

/// `YYYY-MM-DD` with a non-zero month and day.
fn is_valid_date(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return false;
    }
    let num = |r: std::ops::Range<usize>| -> Option<u32> { s.get(r)?.parse().ok() };
    match (num(0..4), num(5..7), num(8..10)) {
        (Some(y), Some(m), Some(d)) => y > 0 && (1..=12).contains(&m) && (1..=31).contains(&d),
        _ => false,
    }
}

/// Map a `DATA_TYPE` / `COLUMN_TYPE` pair from `information_schema.COLUMNS` to a [`ValueKind`].
pub fn kind_from_data_type(data_type: &str, column_type: &str) -> ValueKind {
    let unsigned = column_type.to_ascii_lowercase().contains("unsigned");
    match data_type.to_ascii_lowercase().as_str() {
        "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" => {
            if unsigned {
                ValueKind::UInt
            } else {
                ValueKind::Int
            }
        }
        "year" => ValueKind::Int,
        "bit" => ValueKind::UInt,
        "decimal" | "numeric" | "dec" | "fixed" => ValueKind::Decimal,
        "float" | "double" | "real" => ValueKind::Float,
        "date" => ValueKind::Date,
        "datetime" | "timestamp" => ValueKind::DateTime,
        "time" => ValueKind::Time,
        "json" => ValueKind::Json,
        "binary" | "varbinary" | "tinyblob" | "blob" | "mediumblob" | "longblob" | "geometry"
        | "point" | "linestring" | "polygon" | "multipoint" | "multilinestring"
        | "multipolygon" | "geometrycollection" | "vector" => ValueKind::Bytes,
        _ => ValueKind::Text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(ty: ColumnType) -> ColInfo {
        ColInfo {
            ty,
            unsigned: false,
            binary: false,
            is_enum: false,
            is_set: false,
            nullable: true,
        }
    }
    fn dec(i: ColInfo, s: &str) -> Value {
        decode_cell(&i, s.as_bytes().to_vec())
    }

    #[test]
    fn integers() {
        use ColumnType::*;
        for ty in [
            MYSQL_TYPE_TINY,
            MYSQL_TYPE_SHORT,
            MYSQL_TYPE_INT24,
            MYSQL_TYPE_LONG,
            MYSQL_TYPE_LONGLONG,
        ] {
            assert_eq!(dec(info(ty), "-5"), Value::Int(-5));
            assert_eq!(info(ty).value_kind(), ValueKind::Int);
        }
        let mut u = info(MYSQL_TYPE_LONGLONG);
        u.unsigned = true;
        assert_eq!(dec(u, "18446744073709551615"), Value::UInt(u64::MAX));
        assert_eq!(u.value_kind(), ValueKind::UInt);
        assert_eq!(u.type_name(), "bigint unsigned");
        assert_eq!(
            dec(info(MYSQL_TYPE_LONGLONG), "-9223372036854775808"),
            Value::Int(i64::MIN)
        );
        assert_eq!(dec(info(MYSQL_TYPE_YEAR), "2024"), Value::Int(2024));
        // Garbage never panics.
        assert_eq!(dec(info(MYSQL_TYPE_LONG), "abc"), Value::Text("abc".into()));
    }

    #[test]
    fn decimals_and_floats() {
        use ColumnType::*;
        let s = "-12345678901234567890123456.1234567890";
        assert_eq!(
            dec(info(MYSQL_TYPE_NEWDECIMAL), s),
            Value::Decimal(s.into())
        );
        assert_eq!(dec(info(MYSQL_TYPE_DOUBLE), "3.5e10"), Value::Float(3.5e10));
        assert_eq!(dec(info(MYSQL_TYPE_FLOAT), "0.1"), Value::Float(0.1));
        assert_eq!(
            dec(info(MYSQL_TYPE_FLOAT), "nonsense"),
            Value::Text("nonsense".into())
        );
    }

    #[test]
    fn temporal() {
        use ColumnType::*;
        assert_eq!(
            dec(info(MYSQL_TYPE_DATE), "2024-02-29"),
            Value::Date("2024-02-29".into())
        );
        assert_eq!(
            dec(info(MYSQL_TYPE_DATE), "0000-00-00"),
            Value::Text("0000-00-00".into())
        );
        assert_eq!(
            dec(info(MYSQL_TYPE_DATE), "2024-00-10"),
            Value::Text("2024-00-10".into())
        );
        assert_eq!(
            dec(info(MYSQL_TYPE_DATETIME), "2024-02-29 13:45:01.123456"),
            Value::DateTime("2024-02-29T13:45:01.123456".into())
        );
        assert_eq!(
            dec(info(MYSQL_TYPE_DATETIME), "0000-00-00 00:00:00"),
            Value::Text("0000-00-00 00:00:00".into())
        );
        assert_eq!(
            dec(info(MYSQL_TYPE_TIMESTAMP), "2024-02-29 13:45:01"),
            Value::DateTime("2024-02-29T13:45:01".into())
        );
        assert_eq!(info(MYSQL_TYPE_TIMESTAMP).value_kind(), ValueKind::DateTime);
        assert_eq!(
            dec(info(MYSQL_TYPE_TIME), "-838:59:59"),
            Value::Time("-838:59:59".into())
        );
        assert_eq!(
            dec(info(MYSQL_TYPE_TIME), "100:00:00.5"),
            Value::Time("100:00:00.5".into())
        );
    }

    #[test]
    fn strings_bytes_and_misc() {
        use ColumnType::*;
        assert_eq!(
            dec(info(MYSQL_TYPE_VAR_STRING), "héllo"),
            Value::Text("héllo".into())
        );
        let mut bin = info(MYSQL_TYPE_BLOB);
        bin.binary = true;
        assert_eq!(
            decode_cell(&bin, vec![0, 255, 1]),
            Value::Bytes(vec![0, 255, 1])
        );
        assert_eq!(bin.type_name(), "blob");
        assert_eq!(info(MYSQL_TYPE_BLOB).type_name(), "text");
        // Invalid UTF-8 in a text column does not panic.
        assert_eq!(
            decode_cell(&info(MYSQL_TYPE_VAR_STRING), vec![b'a', 0xff, b'b']),
            Value::Text("a\u{fffd}b".into())
        );
        assert_eq!(
            decode_cell(&info(MYSQL_TYPE_BIT), vec![0x01, 0x00]),
            Value::UInt(256)
        );
        assert_eq!(
            dec(info(MYSQL_TYPE_JSON), r#"{"a":[1,2]}"#),
            Value::Json(r#"{"a":[1,2]}"#.to_string())
        );
        assert_eq!(
            dec(info(MYSQL_TYPE_JSON), "not json"),
            Value::Text("not json".into())
        );
        assert_eq!(
            decode_cell(&info(MYSQL_TYPE_GEOMETRY), vec![1, 2]),
            Value::Bytes(vec![1, 2])
        );
        let mut en = info(MYSQL_TYPE_STRING);
        en.is_enum = true;
        assert_eq!(dec(en, "red"), Value::Text("red".into()));
        assert_eq!(en.type_name(), "enum");
        // Unknown type falls back to text / bytes.
        assert_eq!(dec(info(MYSQL_TYPE_UNKNOWN), "x"), Value::Text("x".into()));
        let mut ub = info(MYSQL_TYPE_UNKNOWN);
        ub.binary = true;
        assert_eq!(decode_cell(&ub, vec![9]), Value::Bytes(vec![9]));
    }

    #[test]
    fn data_type_kinds() {
        assert_eq!(
            kind_from_data_type("bigint", "bigint(20) unsigned"),
            ValueKind::UInt
        );
        assert_eq!(kind_from_data_type("int", "int(11)"), ValueKind::Int);
        assert_eq!(
            kind_from_data_type("decimal", "decimal(38,10)"),
            ValueKind::Decimal
        );
        assert_eq!(
            kind_from_data_type("varchar", "varchar(10)"),
            ValueKind::Text
        );
        assert_eq!(
            kind_from_data_type("longblob", "longblob"),
            ValueKind::Bytes
        );
        assert_eq!(
            kind_from_data_type("timestamp", "timestamp"),
            ValueKind::DateTime
        );
        assert_eq!(kind_from_data_type("enum", "enum('a')"), ValueKind::Text);
        assert_eq!(kind_from_data_type("point", "point"), ValueKind::Bytes);
    }
}
