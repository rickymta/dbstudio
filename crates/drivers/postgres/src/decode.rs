//! Conversion of PostgreSQL text-format values into [`Value`]s.
//!
//! All results are fetched in the server's *text* format (simple query protocol), with the column
//! types learned from a prepared statement. Text is exact for every type (numeric digits, 64-bit
//! integers, `NaN`), and any type without a dedicated decoder simply stays text, so unknown
//! types (enums, ranges, inet, geometry, composites, extension types...) can never fail.
//!
//! Conventions (the session is configured with `TimeZone = 'UTC'`, `DateStyle = 'ISO, MDY'`,
//! `extra_float_digits = 3` and `bytea_output = 'hex'`):
//!
//! * `timestamptz` -> [`Value::Timestamp`] in ISO-8601 **normalised to UTC** (`...Z`). If the user
//!   changes the session time zone inside a script, the offset the server prints is kept
//!   (`+07:00`) so the value is still unambiguous.
//! * `timestamp` -> [`Value::DateTime`] (`2024-05-31T13:45:00.5`), `date` -> [`Value::Date`],
//!   `time`/`timetz` -> [`Value::Time`] (`timetz` keeps its offset).
//! * `infinity` / `-infinity` and BC or 5-digit-year dates cannot be expressed in ISO-8601 and
//!   are returned as [`Value::Text`].
//! * `json`/`jsonb` -> [`Value::Json`]; numbers beyond what `serde_json` represents exactly
//!   (integers above 2^64, more than 17 significant digits) lose precision in this form.
//! * Whenever a decoder cannot parse what the server sent, the value falls back to
//!   [`Value::Text`] with the original text.

use dbstudio_core::{Value, ValueKind};
use tokio_postgres::types::{Kind, Type};

/// Whether `ty` has a dedicated (non-text) decoder or is one of the text-like built-ins.
fn is_known_scalar(ty: &Type) -> bool {
    matches!(
        *ty,
        Type::INT2
            | Type::INT4
            | Type::INT8
            | Type::OID
            | Type::NUMERIC
            | Type::FLOAT4
            | Type::FLOAT8
            | Type::BOOL
            | Type::TEXT
            | Type::VARCHAR
            | Type::BPCHAR
            | Type::NAME
            | Type::CHAR
            | Type::BYTEA
            | Type::DATE
            | Type::TIME
            | Type::TIMETZ
            | Type::TIMESTAMP
            | Type::TIMESTAMPTZ
            | Type::UUID
            | Type::JSON
            | Type::JSONB
    )
}

/// The [`ValueKind`] cells of a column of type `ty` are decoded into.
pub fn value_kind(ty: &Type) -> ValueKind {
    match *ty {
        Type::INT2 | Type::INT4 | Type::INT8 | Type::OID => ValueKind::Int,
        Type::NUMERIC => ValueKind::Decimal,
        Type::FLOAT4 | Type::FLOAT8 => ValueKind::Float,
        Type::BOOL => ValueKind::Bool,
        Type::BYTEA => ValueKind::Bytes,
        Type::DATE => ValueKind::Date,
        Type::TIME | Type::TIMETZ => ValueKind::Time,
        Type::TIMESTAMP => ValueKind::DateTime,
        Type::TIMESTAMPTZ => ValueKind::Timestamp,
        Type::UUID => ValueKind::Uuid,
        Type::JSON | Type::JSONB => ValueKind::Json,
        _ => match ty.kind() {
            Kind::Array(elem) if is_known_scalar(elem) => ValueKind::Array,
            _ => ValueKind::Text,
        },
    }
}

/// PostgreSQL type name for display (`int4`, `varchar`, `int4[]`, user type names).
pub fn type_label(ty: &Type) -> String {
    match ty.kind() {
        Kind::Array(elem) => format!("{}[]", elem.name()),
        _ => ty.name().to_string(),
    }
}

/// Decode one text-format cell.
pub fn decode_text(ty: &Type, raw: &str) -> Value {
    if let Kind::Array(elem) = ty.kind() {
        if is_known_scalar(elem) {
            if let Some(v) = decode_array(elem, raw) {
                return v;
            }
        }
        return Value::Text(raw.to_string());
    }
    decode_scalar(ty, raw)
}

fn text(raw: &str) -> Value {
    Value::Text(raw.to_string())
}

fn decode_scalar(ty: &Type, raw: &str) -> Value {
    match *ty {
        Type::INT2 | Type::INT4 | Type::INT8 | Type::OID => raw
            .parse::<i64>()
            .map(Value::Int)
            .unwrap_or_else(|_| text(raw)),
        Type::NUMERIC => Value::Decimal(raw.to_string()),
        Type::FLOAT4 | Type::FLOAT8 => raw
            .parse::<f64>()
            .map(Value::Float)
            .unwrap_or_else(|_| text(raw)),
        Type::BOOL => match raw {
            "t" | "true" => Value::Bool(true),
            "f" | "false" => Value::Bool(false),
            _ => text(raw),
        },
        Type::BYTEA => decode_bytea(raw)
            .map(Value::Bytes)
            .unwrap_or_else(|| text(raw)),
        Type::DATE => {
            if is_iso_date(raw) {
                Value::Date(raw.to_string())
            } else {
                text(raw)
            }
        }
        Type::TIME => {
            if is_time(raw) {
                Value::Time(raw.to_string())
            } else {
                text(raw)
            }
        }
        Type::TIMETZ => match split_offset(raw) {
            Some((time, Some(off))) if is_time(time) => Value::Time(format!("{time}{off}")),
            _ => text(raw),
        },
        Type::TIMESTAMP => match split_datetime(raw) {
            Some((date, time)) if is_iso_date(date) && is_time(time) => {
                Value::DateTime(format!("{date}T{time}"))
            }
            _ => text(raw),
        },
        Type::TIMESTAMPTZ => match split_datetime(raw) {
            Some((date, rest)) if is_iso_date(date) => match split_offset(rest) {
                Some((time, Some(off))) if is_time(time) => {
                    Value::Timestamp(format!("{date}T{time}{off}"))
                }
                _ => text(raw),
            },
            _ => text(raw),
        },
        Type::UUID => Value::Uuid(raw.to_string()),
        Type::JSON | Type::JSONB => serde_json::from_str(raw)
            .map(Value::Json)
            .unwrap_or_else(|_| text(raw)),
        _ => text(raw),
    }
}

fn decode_bytea(raw: &str) -> Option<Vec<u8>> {
    let hex = raw.strip_prefix("\\x")?;
    if hex.len() % 2 != 0 {
        return None;
    }
    let b = hex.as_bytes();
    let nibble = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    b.chunks(2)
        .map(|p| Some(nibble(p[0])? << 4 | nibble(p[1])?))
        .collect()
}

fn is_iso_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
}

fn is_time(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() < 8 || b[2] != b':' || b[5] != b':' {
        return false;
    }
    if !(b[0..2].iter().chain(&b[3..5]).chain(&b[6..8])).all(u8::is_ascii_digit) {
        return false;
    }
    match b.get(8) {
        None => true,
        Some(b'.') => b.len() > 9 && b[9..].iter().all(u8::is_ascii_digit),
        Some(_) => false,
    }
}

fn split_datetime(s: &str) -> Option<(&str, &str)> {
    if s.len() < 19 || !s.is_char_boundary(10) {
        return None;
    }
    let (date, rest) = s.split_at(10);
    rest.strip_prefix(' ').map(|t| (date, t))
}

/// Split `13:45:00.5+07` into the time and a normalised offset (`Z` or `+07:00`).
fn split_offset(s: &str) -> Option<(&str, Option<String>)> {
    let idx = s.rfind(['+', '-']);
    let Some(idx) = idx.filter(|i| *i >= 8) else {
        return Some((s, None));
    };
    let (time, off) = s.split_at(idx);
    let sign = &off[..1];
    let mut parts = off[1..].split(':');
    let hh = parts.next()?;
    let mm = parts.next().unwrap_or("00");
    let ss = parts.next().unwrap_or("00");
    if parts.next().is_some()
        || ![hh, mm, ss]
            .iter()
            .all(|p| p.len() == 2 && p.bytes().all(|c| c.is_ascii_digit()))
    {
        return None;
    }
    if hh == "00" && mm == "00" && ss == "00" {
        return Some((time, Some("Z".to_string())));
    }
    let seconds = if ss == "00" {
        String::new()
    } else {
        format!(":{ss}")
    };
    Some((time, Some(format!("{sign}{hh}:{mm}{seconds}"))))
}

/// Parse a PostgreSQL array literal (`{1,2,NULL}`, nested for multi-dimensional arrays).
fn decode_array(elem: &Type, raw: &str) -> Option<Value> {
    // Skip an explicit dimension decoration such as `[0:2]={...}`.
    let body = if raw.starts_with('[') {
        &raw[raw.find('=')? + 1..]
    } else {
        raw
    };
    let b = body.as_bytes();
    let mut pos = 0;
    let v = parse_array(elem, b, &mut pos)?;
    (pos == b.len()).then_some(v)
}

fn parse_array(elem: &Type, b: &[u8], pos: &mut usize) -> Option<Value> {
    if b.get(*pos) != Some(&b'{') {
        return None;
    }
    *pos += 1;
    let mut items = Vec::new();
    if b.get(*pos) == Some(&b'}') {
        *pos += 1;
        return Some(Value::Array(items));
    }
    loop {
        match b.get(*pos)? {
            b'{' => items.push(parse_array(elem, b, pos)?),
            b'"' => {
                *pos += 1;
                let mut buf = Vec::new();
                loop {
                    match *b.get(*pos)? {
                        b'\\' => {
                            buf.push(*b.get(*pos + 1)?);
                            *pos += 2;
                        }
                        b'"' => {
                            *pos += 1;
                            break;
                        }
                        c => {
                            buf.push(c);
                            *pos += 1;
                        }
                    }
                }
                let s = String::from_utf8(buf).ok()?;
                items.push(decode_scalar(elem, &s));
            }
            _ => {
                let start = *pos;
                while !matches!(b.get(*pos)?, b',' | b'}') {
                    *pos += 1;
                }
                let tok = std::str::from_utf8(&b[start..*pos]).ok()?;
                if tok.eq_ignore_ascii_case("NULL") {
                    items.push(Value::Null);
                } else {
                    items.push(decode_scalar(elem, tok));
                }
            }
        }
        match b.get(*pos)? {
            b',' => *pos += 1,
            b'}' => {
                *pos += 1;
                return Some(Value::Array(items));
            }
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn integers_keep_all_digits() {
        assert_eq!(
            decode_text(&Type::INT8, "9007199254740993"),
            Value::Int(9_007_199_254_740_993)
        );
        assert_eq!(
            decode_text(&Type::INT8, "-9223372036854775808"),
            Value::Int(i64::MIN)
        );
        assert_eq!(decode_text(&Type::INT2, "-5"), Value::Int(-5));
        assert_eq!(
            decode_text(&Type::OID, "4294967295"),
            Value::Int(4_294_967_295)
        );
    }

    #[test]
    fn numeric_is_exact_text() {
        let big = "9999999999999999999999999999.9999999999".to_string();
        assert_eq!(
            decode_text(&Type::NUMERIC, &big),
            Value::Decimal(big.clone())
        );
        assert_eq!(
            decode_text(&Type::NUMERIC, "NaN"),
            Value::Decimal("NaN".into())
        );
        assert_eq!(
            decode_text(&Type::NUMERIC, "-0.0000000001"),
            Value::Decimal("-0.0000000001".into())
        );
    }

    #[test]
    fn floats() {
        assert_eq!(decode_text(&Type::FLOAT8, "1.5"), Value::Float(1.5));
        assert!(matches!(decode_text(&Type::FLOAT8, "NaN"), Value::Float(f) if f.is_nan()));
        assert_eq!(
            decode_text(&Type::FLOAT8, "Infinity"),
            Value::Float(f64::INFINITY)
        );
        assert_eq!(
            decode_text(&Type::FLOAT4, "-Infinity"),
            Value::Float(f64::NEG_INFINITY)
        );
        assert_eq!(decode_text(&Type::FLOAT4, "0.1"), Value::Float(0.1));
    }

    #[test]
    fn bool_text_bytes() {
        assert_eq!(decode_text(&Type::BOOL, "t"), Value::Bool(true));
        assert_eq!(decode_text(&Type::BOOL, "f"), Value::Bool(false));
        assert_eq!(
            decode_text(&Type::TEXT, "héllo"),
            Value::Text("héllo".into())
        );
        assert_eq!(
            decode_text(&Type::BYTEA, "\\x00ff10"),
            Value::Bytes(vec![0, 255, 16])
        );
        assert_eq!(decode_text(&Type::BYTEA, "\\x"), Value::Bytes(vec![]));
        assert_eq!(
            decode_text(&Type::BYTEA, "bogus"),
            Value::Text("bogus".into())
        );
    }

    #[test]
    fn temporal_values() {
        assert_eq!(
            decode_text(&Type::DATE, "2024-05-31"),
            Value::Date("2024-05-31".into())
        );
        assert_eq!(
            decode_text(&Type::DATE, "infinity"),
            Value::Text("infinity".into())
        );
        assert_eq!(
            decode_text(&Type::DATE, "0044-03-15 BC"),
            Value::Text("0044-03-15 BC".into())
        );
        assert_eq!(
            decode_text(&Type::TIME, "13:45:00.5"),
            Value::Time("13:45:00.5".into())
        );
        assert_eq!(
            decode_text(&Type::TIMETZ, "13:45:00+07"),
            Value::Time("13:45:00+07:00".into())
        );
        assert_eq!(
            decode_text(&Type::TIMETZ, "13:45:00+00"),
            Value::Time("13:45:00Z".into())
        );
        assert_eq!(
            decode_text(&Type::TIMESTAMP, "2024-05-31 13:45:00.123456"),
            Value::DateTime("2024-05-31T13:45:00.123456".into())
        );
        assert_eq!(
            decode_text(&Type::TIMESTAMP, "-infinity"),
            Value::Text("-infinity".into())
        );
        assert_eq!(
            decode_text(&Type::TIMESTAMPTZ, "2024-05-31 13:45:00+00"),
            Value::Timestamp("2024-05-31T13:45:00Z".into())
        );
        assert_eq!(
            decode_text(&Type::TIMESTAMPTZ, "2024-05-31 13:45:00.25-08"),
            Value::Timestamp("2024-05-31T13:45:00.25-08:00".into())
        );
        assert_eq!(
            decode_text(&Type::TIMESTAMPTZ, "2024-05-31 13:45:00+05:30"),
            Value::Timestamp("2024-05-31T13:45:00+05:30".into())
        );
        assert_eq!(
            decode_text(&Type::TIMESTAMPTZ, "infinity"),
            Value::Text("infinity".into())
        );
        assert_eq!(
            decode_text(&Type::TIMESTAMPTZ, "2024-05-31 13:45:00+00 BC"),
            Value::Text("2024-05-31 13:45:00+00 BC".into())
        );
    }

    #[test]
    fn json_and_uuid() {
        assert_eq!(
            decode_text(&Type::JSONB, "{\"a\": [1, null]}"),
            Value::Json(json!({"a": [1, null]}))
        );
        assert_eq!(
            decode_text(&Type::JSON, "not json"),
            Value::Text("not json".into())
        );
        assert_eq!(
            decode_text(&Type::UUID, "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11"),
            Value::Uuid("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11".into())
        );
    }

    #[test]
    fn arrays() {
        assert_eq!(
            decode_text(&Type::INT4_ARRAY, "{1,2,NULL}"),
            Value::Array(vec![Value::Int(1), Value::Int(2), Value::Null])
        );
        assert_eq!(decode_text(&Type::INT4_ARRAY, "{}"), Value::Array(vec![]));
        assert_eq!(
            decode_text(&Type::TEXT_ARRAY, r#"{a,"b c","d\"e","NULL",NULL,"",z}"#),
            Value::Array(vec![
                Value::Text("a".into()),
                Value::Text("b c".into()),
                Value::Text("d\"e".into()),
                Value::Text("NULL".into()),
                Value::Null,
                Value::Text(String::new()),
                Value::Text("z".into()),
            ])
        );
        assert_eq!(
            decode_text(&Type::INT4_ARRAY, "{{1,2},{3,4}}"),
            Value::Array(vec![
                Value::Array(vec![Value::Int(1), Value::Int(2)]),
                Value::Array(vec![Value::Int(3), Value::Int(4)])
            ])
        );
        assert_eq!(
            decode_text(&Type::INT4_ARRAY, "[0:1]={5,6}"),
            Value::Array(vec![Value::Int(5), Value::Int(6)])
        );
        assert_eq!(
            decode_text(&Type::NUMERIC_ARRAY, "{1.50,NaN}"),
            Value::Array(vec![
                Value::Decimal("1.50".into()),
                Value::Decimal("NaN".into())
            ])
        );
        assert_eq!(
            decode_text(&Type::BYTEA_ARRAY, r#"{"\\x0102"}"#),
            Value::Array(vec![Value::Bytes(vec![1, 2])])
        );
        assert_eq!(
            decode_text(&Type::TIMESTAMPTZ_ARRAY, r#"{"2024-01-01 00:00:00+00"}"#),
            Value::Array(vec![Value::Timestamp("2024-01-01T00:00:00Z".into())])
        );
        assert_eq!(
            decode_text(&Type::JSONB_ARRAY, r#"{"{\"a\": 1}"}"#),
            Value::Array(vec![Value::Json(json!({"a": 1}))])
        );
        // Broken array text never errors.
        assert_eq!(
            decode_text(&Type::INT4_ARRAY, "{1,2"),
            Value::Text("{1,2".into())
        );
    }

    #[test]
    fn unknown_types_fall_back_to_text() {
        assert_eq!(
            decode_text(&Type::INET, "10.0.0.1/8"),
            Value::Text("10.0.0.1/8".into())
        );
        assert_eq!(
            decode_text(&Type::INTERVAL, "1 day"),
            Value::Text("1 day".into())
        );
        assert_eq!(
            decode_text(&Type::INT4_RANGE, "[1,5)"),
            Value::Text("[1,5)".into())
        );
        assert_eq!(
            decode_text(&Type::MONEY, "$1.00"),
            Value::Text("$1.00".into())
        );
        // Arrays of unsupported element types stay whole text.
        assert_eq!(
            decode_text(&Type::INET_ARRAY, "{10.0.0.1}"),
            Value::Text("{10.0.0.1}".into())
        );
    }

    #[test]
    fn kinds_and_labels() {
        assert_eq!(value_kind(&Type::INT8), ValueKind::Int);
        assert_eq!(value_kind(&Type::NUMERIC), ValueKind::Decimal);
        assert_eq!(value_kind(&Type::TIMETZ), ValueKind::Time);
        assert_eq!(value_kind(&Type::TIMESTAMPTZ), ValueKind::Timestamp);
        assert_eq!(value_kind(&Type::INT4_ARRAY), ValueKind::Array);
        assert_eq!(value_kind(&Type::INET_ARRAY), ValueKind::Text);
        assert_eq!(value_kind(&Type::INET), ValueKind::Text);
        assert_eq!(type_label(&Type::INT4), "int4");
        assert_eq!(type_label(&Type::INT4_ARRAY), "int4[]");
        assert_eq!(type_label(&Type::TIMESTAMPTZ), "timestamptz");
    }
}
