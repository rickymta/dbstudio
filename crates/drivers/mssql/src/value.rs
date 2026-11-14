//! SQL Server cells to [`dbstudio_core::Value`], and column metadata.
//!
//! Nothing is lossy: `decimal`/`numeric`/`money`/`smallmoney` become exact decimal text with the
//! full scale, `bigint` stays a 64-bit integer, temporal types keep all fractional digits
//! (`datetime2(7)`, `time(7)`, `datetimeoffset` with its original offset) and `uniqueidentifier`
//! is its canonical text. Large text/binary cells over the LOB limit become `Ext truncated*`.

use dbstudio_core::{ColumnMeta, ExtValue, Value, ValueKind};
use tiberius::numeric::Numeric;
use tiberius::time::{Date, DateTime, DateTime2, DateTimeOffset, SmallDateTime, Time};
use tiberius::{Column, ColumnData, ColumnType};

/// Default limit of one text/binary cell, in bytes.
pub const DEFAULT_MAX_LOB_BYTES: usize = 8 * 1024 * 1024;

/// Native type name of a result column (the wire metadata carries no length or precision).
pub fn type_name(t: ColumnType) -> &'static str {
    match t {
        ColumnType::Null => "null",
        ColumnType::Bit | ColumnType::Bitn => "bit",
        ColumnType::Int1 => "tinyint",
        ColumnType::Int2 => "smallint",
        ColumnType::Int4 => "int",
        ColumnType::Int8 => "bigint",
        ColumnType::Intn => "int",
        ColumnType::Datetime4 => "smalldatetime",
        ColumnType::Float4 => "real",
        ColumnType::Float8 | ColumnType::Floatn => "float",
        ColumnType::Money => "money",
        ColumnType::Money4 => "smallmoney",
        ColumnType::Datetime | ColumnType::Datetimen => "datetime",
        ColumnType::Guid => "uniqueidentifier",
        ColumnType::Decimaln => "decimal",
        ColumnType::Numericn => "numeric",
        ColumnType::Daten => "date",
        ColumnType::Timen => "time",
        ColumnType::Datetime2 => "datetime2",
        ColumnType::DatetimeOffsetn => "datetimeoffset",
        ColumnType::BigVarBin => "varbinary",
        ColumnType::BigBinary => "binary",
        ColumnType::BigVarChar => "varchar",
        ColumnType::BigChar => "char",
        ColumnType::NVarchar => "nvarchar",
        ColumnType::NChar => "nchar",
        ColumnType::Xml => "xml",
        ColumnType::Udt => "udt",
        ColumnType::Text => "text",
        ColumnType::Image => "image",
        ColumnType::NText => "ntext",
        ColumnType::SSVariant => "sql_variant",
    }
}

/// [`ValueKind`] cells of a column of type `t` are decoded into.
pub fn value_kind(t: ColumnType) -> ValueKind {
    match t {
        ColumnType::Null => ValueKind::Null,
        ColumnType::Bit | ColumnType::Bitn => ValueKind::Bool,
        ColumnType::Int1
        | ColumnType::Int2
        | ColumnType::Int4
        | ColumnType::Int8
        | ColumnType::Intn => ValueKind::Int,
        ColumnType::Float4 | ColumnType::Float8 | ColumnType::Floatn => ValueKind::Float,
        ColumnType::Money | ColumnType::Money4 | ColumnType::Decimaln | ColumnType::Numericn => {
            ValueKind::Decimal
        }
        ColumnType::Datetime4
        | ColumnType::Datetime
        | ColumnType::Datetimen
        | ColumnType::Datetime2 => ValueKind::DateTime,
        ColumnType::Daten => ValueKind::Date,
        ColumnType::Timen => ValueKind::Time,
        ColumnType::DatetimeOffsetn => ValueKind::Timestamp,
        ColumnType::Guid => ValueKind::Uuid,
        ColumnType::BigVarBin | ColumnType::BigBinary | ColumnType::Image | ColumnType::Udt => {
            ValueKind::Bytes
        }
        ColumnType::BigVarChar
        | ColumnType::BigChar
        | ColumnType::NVarchar
        | ColumnType::NChar
        | ColumnType::Xml
        | ColumnType::Text
        | ColumnType::NText => ValueKind::Text,
        // The kind of a variant depends on each value; text is the safe display type.
        ColumnType::SSVariant => ValueKind::Text,
    }
}

/// Wire column to [`ColumnMeta`].
pub fn column_meta(c: &Column) -> ColumnMeta {
    ColumnMeta {
        name: c.name().to_string(),
        type_name: type_name(c.column_type()).to_string(),
        value_kind: value_kind(c.column_type()),
        nullable: None,
    }
}

/// Convert one cell. `lob_limit` caps text and binary cells (bytes).
pub fn decode(data: ColumnData<'static>, lob_limit: usize) -> Value {
    match data {
        ColumnData::U8(v) => v.map_or(Value::Null, |v| Value::Int(i64::from(v))),
        ColumnData::I16(v) => v.map_or(Value::Null, |v| Value::Int(i64::from(v))),
        ColumnData::I32(v) => v.map_or(Value::Null, |v| Value::Int(i64::from(v))),
        ColumnData::I64(v) => v.map_or(Value::Null, Value::Int),
        // `real` keeps its shortest decimal form (0.1 stays 0.1, not 0.10000000149011612).
        ColumnData::F32(v) => v.map_or(Value::Null, |v| {
            Value::Float(v.to_string().parse::<f64>().unwrap_or(f64::from(v)))
        }),
        ColumnData::F64(v) => v.map_or(Value::Null, Value::Float),
        ColumnData::Bit(v) => v.map_or(Value::Null, Value::Bool),
        ColumnData::String(v) => v.map_or(Value::Null, |s| text(s.into_owned(), lob_limit)),
        ColumnData::Guid(v) => v.map_or(Value::Null, |g| Value::Uuid(g.hyphenated().to_string())),
        ColumnData::Binary(v) => v.map_or(Value::Null, |b| bytes(b.into_owned(), lob_limit)),
        ColumnData::Numeric(v) => v.map_or(Value::Null, |n| Value::Decimal(decimal_text(n))),
        ColumnData::Xml(v) => v.map_or(Value::Null, |x| text(x.to_string(), lob_limit)),
        ColumnData::DateTime(v) => v.map_or(Value::Null, |d| Value::DateTime(datetime_text(d))),
        ColumnData::SmallDateTime(v) => {
            v.map_or(Value::Null, |d| Value::DateTime(smalldatetime_text(d)))
        }
        ColumnData::Time(v) => v.map_or(Value::Null, |t| Value::Time(time_text(t))),
        ColumnData::Date(v) => v.map_or(Value::Null, |d| Value::Date(date_text(d))),
        ColumnData::DateTime2(v) => v.map_or(Value::Null, |d| Value::DateTime(datetime2_text(d))),
        ColumnData::DateTimeOffset(v) => {
            v.map_or(Value::Null, |d| Value::Timestamp(datetimeoffset_text(d)))
        }
    }
}

/// Flag a text or binary cell that the PLP cap already cut on the wire as truncated.
pub fn mark_truncated(cell: &mut Value) {
    match std::mem::replace(cell, Value::Null) {
        Value::Text(s) => *cell = Value::Ext(ExtValue::new("truncatedText", &s)),
        Value::Bytes(b) => {
            use base64::Engine as _;
            *cell = Value::Ext(ExtValue::new(
                "truncatedBytes",
                base64::engine::general_purpose::STANDARD.encode(&b),
            ));
        }
        other => *cell = other,
    }
}

fn text(s: String, limit: usize) -> Value {
    if s.len() <= limit {
        return Value::Text(s);
    }
    // Keep a prefix cut on a character boundary.
    let mut end = limit;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    Value::Ext(ExtValue::new("truncatedText", &s[..end]))
}

fn bytes(b: Vec<u8>, limit: usize) -> Value {
    if b.len() <= limit {
        return Value::Bytes(b);
    }
    use base64::Engine as _;
    Value::Ext(ExtValue::new(
        "truncatedBytes",
        base64::engine::general_purpose::STANDARD.encode(&b[..limit]),
    ))
}

/// Exact decimal text with the full scale (`12.3400`, `-0.05`, `7`).
pub fn decimal_text(n: Numeric) -> String {
    let scale = u32::from(n.scale());
    let value = n.value();
    let neg = value < 0;
    let digits = value.unsigned_abs().to_string();
    let body = if scale == 0 {
        digits
    } else {
        let scale = scale as usize;
        let padded = if digits.len() <= scale {
            format!("{}{}", "0".repeat(scale + 1 - digits.len()), digits)
        } else {
            digits
        };
        let (int, frac) = padded.split_at(padded.len() - scale);
        format!("{int}.{frac}")
    };
    if neg { format!("-{body}") } else { body }
}

/// Civil date from days since 1970-01-01 (proleptic Gregorian).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Days from 0001-01-01 to 1970-01-01.
const DAYS_0001_TO_1970: i64 = 719_162;
/// Days from 1900-01-01 to 1970-01-01.
const DAYS_1900_TO_1970: i64 = 25_567;

fn ymd(days_since_1970: i64) -> String {
    let (y, m, d) = civil_from_days(days_since_1970);
    format!("{y:04}-{m:02}-{d:02}")
}

fn date_text(d: Date) -> String {
    ymd(i64::from(d.days()) - DAYS_0001_TO_1970)
}

fn hms(secs: u64) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs / 60) % 60,
        secs % 60
    )
}

/// `increments` of 10^-scale seconds since midnight to `hh:mm:ss[.fffffff]` with exactly
/// `scale` fraction digits.
fn time_parts(increments: u64, scale: u8) -> String {
    let scale = u32::from(scale.min(7));
    let unit = 10u64.pow(scale);
    let secs = increments / unit;
    let mut s = hms(secs);
    if scale > 0 {
        s.push_str(&format!(
            ".{:0width$}",
            increments % unit,
            width = scale as usize
        ));
    }
    s
}

fn time_text(t: Time) -> String {
    time_parts(t.increments(), t.scale())
}

fn datetime2_text(d: DateTime2) -> String {
    format!(
        "{}T{}",
        date_text(d.date()),
        time_parts(d.time().increments(), d.time().scale())
    )
}

fn datetime_text(d: DateTime) -> String {
    // 1/300 second units, shown with the 3 digits SQL Server prints (.000 .003 .007 ...).
    let frag = u64::from(d.seconds_fragments());
    let millis_total = (frag * 10 + 1) / 3;
    let secs = millis_total / 1000;
    format!(
        "{}T{}.{:03}",
        ymd(i64::from(d.days()) - DAYS_1900_TO_1970),
        hms(secs),
        millis_total % 1000
    )
}

fn smalldatetime_text(d: SmallDateTime) -> String {
    let minutes = u64::from(d.seconds_fragments());
    format!(
        "{}T{}",
        ymd(i64::from(d.days()) - DAYS_1900_TO_1970),
        hms(minutes * 60)
    )
}

/// `datetimeoffset` is transported in UTC plus an offset in minutes; show the local time with
/// the original offset.
fn datetimeoffset_text(d: DateTimeOffset) -> String {
    let dt2 = d.datetime2();
    let offset = i64::from(d.offset());
    let time = dt2.time();
    let scale = u32::from(time.scale().min(7));
    let unit = 10u64.pow(scale);
    let day_incr = 86_400 * unit;
    let total = i64::try_from(time.increments()).unwrap_or(0) + offset * 60 * unit as i64;
    let day_shift = total.div_euclid(day_incr as i64);
    let local_incr = total.rem_euclid(day_incr as i64) as u64;
    let days = i64::from(dt2.date().days()) - DAYS_0001_TO_1970 + day_shift;
    let sign = if offset < 0 { '-' } else { '+' };
    let a = offset.abs();
    format!(
        "{}T{}{sign}{:02}:{:02}",
        ymd(days),
        time_parts(local_incr, time.scale()),
        a / 60,
        a % 60
    )
}

/// Text of cell `i` (empty for NULL / out of range / non-text).
pub fn str_at(row: &[Value], i: usize) -> String {
    opt_str_at(row, i).unwrap_or_default()
}

/// Text of cell `i`, `None` for NULL.
pub fn opt_str_at(row: &[Value], i: usize) -> Option<String> {
    match row.get(i)? {
        Value::Null => None,
        Value::Text(s)
        | Value::Decimal(s)
        | Value::Uuid(s)
        | Value::Date(s)
        | Value::Time(s)
        | Value::DateTime(s)
        | Value::Timestamp(s)
        | Value::Json(s) => Some(s.clone()),
        Value::Int(n) => Some(n.to_string()),
        Value::UInt(n) => Some(n.to_string()),
        Value::Bool(b) => Some(if *b { "1" } else { "0" }.to_string()),
        Value::Float(f) => Some(f.to_string()),
        Value::Ext(e) => Some(e.text.clone()),
        _ => None,
    }
}

/// Integer of cell `i` (0 for NULL).
pub fn int_at(row: &[Value], i: usize) -> i64 {
    match row.get(i) {
        Some(Value::Int(n)) => *n,
        Some(Value::UInt(n)) => i64::try_from(*n).unwrap_or(i64::MAX),
        Some(Value::Bool(b)) => i64::from(*b),
        Some(Value::Decimal(s) | Value::Text(s)) => s
            .split('.')
            .next()
            .and_then(|x| x.parse().ok())
            .unwrap_or(0),
        Some(Value::Float(f)) => *f as i64,
        _ => 0,
    }
}

/// Boolean of cell `i` (false for NULL).
pub fn bool_at(row: &[Value], i: usize) -> bool {
    match row.get(i) {
        Some(Value::Bool(b)) => *b,
        Some(Value::Int(n)) => *n != 0,
        _ => false,
    }
}

/// [`ValueKind`] of a column of the (base) SQL Server type `name`.
pub fn kind_of_type_name(name: &str) -> ValueKind {
    match name.to_ascii_lowercase().as_str() {
        "bit" => ValueKind::Bool,
        "tinyint" | "smallint" | "int" | "bigint" => ValueKind::Int,
        "decimal" | "numeric" | "money" | "smallmoney" => ValueKind::Decimal,
        "real" | "float" => ValueKind::Float,
        "binary" | "varbinary" | "image" | "timestamp" | "rowversion" | "geography"
        | "geometry" | "hierarchyid" => ValueKind::Bytes,
        "uniqueidentifier" => ValueKind::Uuid,
        "date" => ValueKind::Date,
        "time" => ValueKind::Time,
        "datetime" | "datetime2" | "smalldatetime" => ValueKind::DateTime,
        "datetimeoffset" => ValueKind::Timestamp,
        "json" => ValueKind::Json,
        _ => ValueKind::Text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_text_keeps_scale_and_sign() {
        let d = |v: i128, s: u8| decimal_text(Numeric::new_with_scale(v, s));
        assert_eq!(d(123_400, 4), "12.3400");
        assert_eq!(d(-5, 2), "-0.05");
        assert_eq!(d(7, 0), "7");
        assert_eq!(d(0, 3), "0.000");
        assert_eq!(d(-100, 2), "-1.00");
        assert_eq!(
            d(99_999_999_999_999_999_999_999_999_999_999_999_999, 38),
            "0.99999999999999999999999999999999999999"
        );
    }

    #[test]
    fn dates_and_times() {
        // 2024-05-31 is day 739_036 since 0001-01-01.
        assert_eq!(date_text(Date::new(739_036)), "2024-05-31");
        assert_eq!(date_text(Date::new(0)), "0001-01-01");
        assert_eq!(time_text(Time::new(123_456_789, 7)), "00:00:12.3456789");
        assert_eq!(time_text(Time::new(45, 0)), "00:00:45");
        assert_eq!(time_text(Time::new(5, 3)), "00:00:00.005");
        let dt = DateTime::new(0, 300 * 3600 + 1);
        assert_eq!(datetime_text(dt), "1900-01-01T01:00:00.003");
        assert_eq!(
            smalldatetime_text(SmallDateTime::new(1, 61)),
            "1900-01-02T01:01:00"
        );
    }

    #[test]
    fn datetimeoffset_keeps_offset() {
        // 13:45:00 UTC on 2024-05-31 shown at +07:00 is 20:45.
        let dt2 = DateTime2::new(Date::new(739_036), Time::new(13 * 3600 + 45 * 60, 0));
        assert_eq!(
            datetimeoffset_text(DateTimeOffset::new(dt2, 420)),
            "2024-05-31T20:45:00+07:00"
        );
        // Crossing midnight backwards.
        let dt2 = DateTime2::new(Date::new(739_036), Time::new(60, 0));
        assert_eq!(
            datetimeoffset_text(DateTimeOffset::new(dt2, -90)),
            "2024-05-30T22:31:00-01:30"
        );
    }

    #[test]
    fn lob_limit_truncates_on_char_boundary() {
        match text("héllo".to_string(), 2) {
            Value::Ext(e) => {
                assert_eq!(e.ty, "truncatedText");
                assert_eq!(e.text, "h");
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(bytes(vec![1, 2, 3], 2), Value::Ext(e) if e.ty == "truncatedBytes"));
        assert_eq!(bytes(vec![1], 2), Value::Bytes(vec![1]));
    }

    #[test]
    fn real_keeps_short_form() {
        assert_eq!(decode(ColumnData::F32(Some(0.1)), 100), Value::Float(0.1));
        assert_eq!(
            decode(ColumnData::I64(Some(i64::MIN)), 100),
            Value::Int(i64::MIN)
        );
        assert_eq!(decode(ColumnData::Bit(None), 100), Value::Null);
    }
}
