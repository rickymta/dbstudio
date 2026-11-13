//! Oracle column types -> [`Value`].
//!
//! Numbers are taken as text from OCI so nothing is lost: `NUMBER(p,0)` with `p <= 18` becomes an
//! `Int`, every other `NUMBER` / `FLOAT` a `Decimal` string. Dates and timestamps are formatted by
//! hand (ISO 8601 with a `T`), LOBs are cut at `max_lob_bytes` and then marked as truncated.

use std::io::Read;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use dbstudio_core::{ColumnMeta, Error, ExtValue, Result, Value, ValueKind};
use oracle::sql_type::{Blob, Clob, Lob, Nclob, OracleType, Timestamp};
use oracle::{ColumnInfo, SqlValue};

use crate::error::map_error;

/// How cells of a column are decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Col {
    Int,
    Decimal,
    Float,
    Text,
    Clob,
    NClob,
    Raw,
    Blob,
    Date,
    Timestamp,
    TimestampTz,
    TimestampLtz,
    IntervalDs,
    IntervalYm,
    Rowid,
    Bool,
    /// Anything else: shown as a placeholder text.
    Other,
}

/// Decoding plan of one column.
#[derive(Debug, Clone, Copy)]
pub struct ColPlan {
    pub col: Col,
}

/// Plan and metadata for `info`.
pub fn plan_column(info: &ColumnInfo) -> (ColPlan, ColumnMeta) {
    let ty = info.oracle_type();
    let (col, kind) = match ty {
        OracleType::Number(p, 0) if (1..=18).contains(p) => (Col::Int, ValueKind::Int),
        OracleType::Number(..) | OracleType::Float(_) => (Col::Decimal, ValueKind::Decimal),
        OracleType::Int64 => (Col::Int, ValueKind::Int),
        OracleType::UInt64 => (Col::Decimal, ValueKind::Decimal),
        OracleType::BinaryFloat | OracleType::BinaryDouble => (Col::Float, ValueKind::Float),
        OracleType::Varchar2(_)
        | OracleType::NVarchar2(_)
        | OracleType::Char(_)
        | OracleType::NChar(_)
        | OracleType::Long
        | OracleType::Xml => (Col::Text, ValueKind::Text),
        OracleType::CLOB => (Col::Clob, ValueKind::Text),
        OracleType::NCLOB => (Col::NClob, ValueKind::Text),
        OracleType::Raw(_) | OracleType::LongRaw => (Col::Raw, ValueKind::Bytes),
        OracleType::BLOB => (Col::Blob, ValueKind::Bytes),
        OracleType::Date => (Col::Date, ValueKind::DateTime),
        OracleType::Timestamp(_) => (Col::Timestamp, ValueKind::DateTime),
        OracleType::TimestampTZ(_) => (Col::TimestampTz, ValueKind::Timestamp),
        OracleType::TimestampLTZ(_) => (Col::TimestampLtz, ValueKind::Timestamp),
        OracleType::IntervalDS(..) => (Col::IntervalDs, ValueKind::Ext),
        OracleType::IntervalYM(_) => (Col::IntervalYm, ValueKind::Ext),
        OracleType::Rowid => (Col::Rowid, ValueKind::Ext),
        OracleType::Boolean => (Col::Bool, ValueKind::Bool),
        OracleType::Object(_) => (Col::Text, ValueKind::Text),
        _ => (Col::Other, ValueKind::Text),
    };
    (
        ColPlan { col },
        ColumnMeta {
            name: info.name().to_string(),
            type_name: type_name(ty),
            value_kind: kind,
            nullable: Some(info.nullable()),
        },
    )
}

/// Type name shown to the user (`VARCHAR2(20)`, `NUMBER(10,2)`, `TIMESTAMP(6) WITH TIME ZONE`).
pub fn type_name(ty: &OracleType) -> String {
    ty.to_string()
}

/// Normalise the text of a NUMBER: OCI omits the leading zero of fractions (`.5`).
pub fn normalize_number(s: &str) -> String {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("-.") {
        format!("-0.{rest}")
    } else if let Some(rest) = s.strip_prefix('.') {
        format!("0.{rest}")
    } else if let Some(rest) = s.strip_prefix("+.") {
        format!("0.{rest}")
    } else {
        s.to_string()
    }
}

/// ISO 8601 text of a timestamp. `with_offset` appends the time zone offset (`Z` when the value
/// carries none, e.g. `TIMESTAMP WITH LOCAL TIME ZONE` read in a UTC session).
pub fn format_timestamp(ts: &Timestamp, with_offset: bool) -> String {
    let mut s = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        ts.year(),
        ts.month(),
        ts.day(),
        ts.hour(),
        ts.minute(),
        ts.second()
    );
    let prec = ts.precision().min(9) as usize;
    if prec > 0 {
        let frac = format!("{:09}", ts.nanosecond());
        s.push('.');
        s.push_str(&frac[..prec]);
    }
    if with_offset {
        if ts.with_tz() {
            let sign = if ts.tz_hour_offset() < 0 || ts.tz_minute_offset() < 0 {
                '-'
            } else {
                '+'
            };
            s.push_str(&format!(
                "{sign}{:02}:{:02}",
                ts.tz_hour_offset().abs(),
                ts.tz_minute_offset().abs()
            ));
        } else {
            s.push('Z');
        }
    }
    s
}

fn trunc_bytes(mut b: Vec<u8>, max: usize) -> Value {
    if b.len() <= max {
        return Value::Bytes(b);
    }
    b.truncate(max);
    Value::Ext(ExtValue::new("truncatedBytes", STANDARD.encode(b)))
}

fn read_text_lob<L: Read + Lob>(mut lob: L, max: usize) -> Result<Value> {
    let chars = lob.size().map_err(map_error)?;
    let mut buf = Vec::new();
    // `max` bytes always hold at least `max / 4` characters; read one byte more than `max` to
    // see that the value is longer.
    lob.by_ref()
        .take(max as u64 + 1)
        .read_to_end(&mut buf)
        .map_err(Error::from)?;
    let truncated = buf.len() > max || chars as usize > max;
    let mut text = match String::from_utf8(buf) {
        Ok(t) => t,
        Err(e) => {
            // A multi-byte character cut in half at the end.
            let valid = e.utf8_error().valid_up_to();
            let mut bytes = e.into_bytes();
            bytes.truncate(valid);
            String::from_utf8(bytes).unwrap_or_default()
        }
    };
    if truncated {
        let mut cut = text.len().min(max);
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        return Ok(Value::Ext(ExtValue::new("truncatedText", text)));
    }
    Ok(Value::Text(text))
}

/// Decode one cell.
pub fn decode(sv: &SqlValue, plan: ColPlan, max_lob: u64) -> Result<Value> {
    if sv.is_null().map_err(map_error)? {
        return Ok(Value::Null);
    }
    let max = usize::try_from(max_lob).unwrap_or(usize::MAX);
    let get_string = || sv.get::<String>().map_err(map_error);
    Ok(match plan.col {
        Col::Int => {
            let s = get_string()?;
            match s.trim().parse::<i64>() {
                Ok(n) => Value::Int(n),
                Err(_) => Value::Decimal(normalize_number(&s)),
            }
        }
        Col::Decimal => Value::Decimal(normalize_number(&get_string()?)),
        Col::Float => Value::Float(sv.get::<f64>().map_err(map_error)?),
        Col::Text => Value::Text(get_string()?),
        Col::Clob => read_text_lob(sv.get::<Clob>().map_err(map_error)?, max)?,
        Col::NClob => read_text_lob(sv.get::<Nclob>().map_err(map_error)?, max)?,
        Col::Raw => trunc_bytes(sv.get::<Vec<u8>>().map_err(map_error)?, max),
        Col::Blob => {
            let mut blob: Blob = sv.get().map_err(map_error)?;
            let size = blob.size().map_err(map_error)?;
            let mut buf = Vec::new();
            blob.by_ref()
                .take(max as u64)
                .read_to_end(&mut buf)
                .map_err(Error::from)?;
            if size as usize > max {
                Value::Ext(ExtValue::new("truncatedBytes", STANDARD.encode(buf)))
            } else {
                Value::Bytes(buf)
            }
        }
        Col::Date | Col::Timestamp => Value::DateTime(format_timestamp(
            &sv.get::<Timestamp>().map_err(map_error)?,
            false,
        )),
        Col::TimestampTz | Col::TimestampLtz => Value::Timestamp(format_timestamp(
            &sv.get::<Timestamp>().map_err(map_error)?,
            true,
        )),
        Col::IntervalDs | Col::IntervalYm => Value::Ext(ExtValue::new("interval", get_string()?)),
        Col::Rowid => Value::Ext(ExtValue::new("rowid", get_string()?)),
        Col::Bool => Value::Bool(sv.get::<bool>().map_err(map_error)?),
        Col::Other => match get_string() {
            Ok(s) => Value::Text(s),
            Err(_) => Value::Ext(ExtValue::new("unsupported", "<unsupported type>")),
        },
    })
}

/// Hint added to the error of a query that returns a column type the OCI binding cannot read.
pub fn unsupported_type_hint(message: &str) -> Option<&'static str> {
    let m = message.to_ascii_lowercase();
    if m.contains("unsupported oracle type") || m.contains("unsupported") && m.contains("type") {
        Some(
            "This column type cannot be read directly: wrap it in the query, e.g. \
             JSON_SERIALIZE(col RETURNING CLOB) for JSON or TO_CHAR(col) for VECTOR.",
        )
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_get_a_leading_zero() {
        assert_eq!(normalize_number(".5"), "0.5");
        assert_eq!(normalize_number("-.25"), "-0.25");
        assert_eq!(normalize_number("12.5"), "12.5");
        assert_eq!(
            normalize_number("123456789012345678901234567890"),
            "123456789012345678901234567890"
        );
    }

    #[test]
    fn timestamps_are_formatted_with_the_declared_precision() {
        let ts = Timestamp::new(2024, 5, 31, 13, 45, 0, 123_456_000)
            .unwrap()
            .and_prec(6)
            .unwrap();
        assert_eq!(format_timestamp(&ts, false), "2024-05-31T13:45:00.123456");
        assert_eq!(format_timestamp(&ts, true), "2024-05-31T13:45:00.123456Z");
        let tz = Timestamp::new(2024, 5, 31, 13, 45, 0, 0)
            .unwrap()
            .and_tz_hm_offset(-8, -30)
            .unwrap()
            .and_prec(0)
            .unwrap();
        assert_eq!(format_timestamp(&tz, true), "2024-05-31T13:45:00-08:30");
        let plus = Timestamp::new(2024, 1, 1, 0, 0, 0, 0)
            .unwrap()
            .and_tz_offset(7 * 3600)
            .unwrap()
            .and_prec(0)
            .unwrap();
        assert_eq!(format_timestamp(&plus, true), "2024-01-01T00:00:00+07:00");
    }

    #[test]
    fn lob_values_are_cut_on_char_boundaries() {
        assert_eq!(trunc_bytes(vec![1, 2, 3], 3), Value::Bytes(vec![1, 2, 3]));
        assert_eq!(
            trunc_bytes(vec![1, 2, 3], 2),
            Value::Ext(ExtValue::new("truncatedBytes", STANDARD.encode([1, 2])))
        );
    }

    #[test]
    fn hint_for_unsupported_types() {
        assert!(unsupported_type_hint("unsupported Oracle type JSON").is_some());
        assert!(unsupported_type_hint("ORA-00942: table or view does not exist").is_none());
    }
}
