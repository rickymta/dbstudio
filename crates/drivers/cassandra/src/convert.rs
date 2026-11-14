//! Conversion between CQL values and the generic [`Value`], and CQL literal rendering.

use dbstudio_core::{Error, ExtValue, Result, Value};
use scylla::value::{CqlDecimal, CqlDuration, CqlValue, CqlVarint};

use crate::ident::{quote_ident, quote_text};
use crate::types::CqlType;

// ---------------------------------------------------------------------------------------------
// Dates
// ---------------------------------------------------------------------------------------------

/// Civil date from days since 1970-01-01 (proleptic Gregorian, Howard Hinnant's algorithm).
pub(crate) fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
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

fn format_year(y: i64) -> String {
    if (0..=9999).contains(&y) {
        format!("{y:04}")
    } else if y > 0 {
        format!("+{y}")
    } else {
        format!("-{:04}", -y)
    }
}

/// `YYYY-MM-DD` of `days` since the unix epoch.
pub(crate) fn format_date(days: i64) -> String {
    let (y, m, d) = civil_from_days(days);
    format!("{}-{m:02}-{d:02}", format_year(y))
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ` (UTC) of a millisecond timestamp.
pub(crate) fn format_timestamp(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let rem = ms.rem_euclid(86_400_000);
    let (h, mi, s, milli) = (
        rem / 3_600_000,
        rem / 60_000 % 60,
        rem / 1000 % 60,
        rem % 1000,
    );
    format!("{}T{h:02}:{mi:02}:{s:02}.{milli:03}Z", format_date(days))
}

/// `HH:MM:SS[.fraction]` of nanoseconds since midnight.
pub(crate) fn format_time(ns: i64) -> String {
    let secs = ns.div_euclid(1_000_000_000);
    let frac = ns.rem_euclid(1_000_000_000);
    let base = format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60);
    if frac == 0 {
        base
    } else {
        let f = format!("{frac:09}");
        format!("{base}.{}", f.trim_end_matches('0'))
    }
}

/// CQL duration text (`1mo2d3ns`), zero components omitted.
pub(crate) fn format_duration(d: &CqlDuration) -> String {
    let mut out = String::new();
    if d.months == 0 && d.days == 0 && d.nanoseconds == 0 {
        return "0ns".into();
    }
    if d.months != 0 {
        out.push_str(&format!("{}mo", d.months));
    }
    if d.days != 0 {
        out.push_str(&format!("{}d", d.days));
    }
    if d.nanoseconds != 0 {
        out.push_str(&format!("{}ns", d.nanoseconds));
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Big numbers
// ---------------------------------------------------------------------------------------------

/// Decimal text of a two's complement big-endian integer.
pub(crate) fn varint_to_string(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "0".into();
    }
    let negative = bytes[0] & 0x80 != 0;
    // Magnitude as big-endian bytes.
    let mut mag: Vec<u8> = bytes.to_vec();
    if negative {
        // two's complement negate
        let mut carry = true;
        for b in mag.iter_mut().rev() {
            *b = !*b;
            if carry {
                let (v, c) = b.overflowing_add(1);
                *b = v;
                carry = c;
            }
        }
        if carry {
            mag.insert(0, 1);
        }
    }
    // Base 1e9 limbs by repeated division.
    let mut limbs: Vec<u32> = mag.iter().map(|b| u32::from(*b)).collect(); // base 256 digits
    let mut out: Vec<u32> = Vec::new();
    while !limbs.is_empty() {
        let mut rem: u64 = 0;
        let mut next: Vec<u32> = Vec::with_capacity(limbs.len());
        for d in &limbs {
            let cur = rem * 256 + u64::from(*d);
            let q = (cur / 1_000_000_000) as u32;
            rem = cur % 1_000_000_000;
            if !next.is_empty() || q != 0 {
                next.push(q);
            }
        }
        out.push(rem as u32);
        limbs = next;
    }
    let mut s = String::new();
    for (i, limb) in out.iter().rev().enumerate() {
        if i == 0 {
            s.push_str(&limb.to_string());
        } else {
            s.push_str(&format!("{limb:09}"));
        }
    }
    if s.is_empty() {
        s.push('0');
    }
    if negative && s != "0" {
        s.insert(0, '-');
    }
    s
}

/// Decimal text of `unscaled * 10^-scale`.
pub(crate) fn decimal_to_string(unscaled: &str, scale: i32) -> String {
    let (neg, digits) = match unscaled.strip_prefix('-') {
        Some(d) => (true, d),
        None => (false, unscaled),
    };
    let body = if scale <= 0 {
        if digits == "0" {
            "0".to_string()
        } else {
            format!("{digits}{}", "0".repeat(scale.unsigned_abs() as usize))
        }
    } else {
        let scale = scale as usize;
        if digits.len() > scale {
            let (int, frac) = digits.split_at(digits.len() - scale);
            format!("{int}.{frac}")
        } else {
            format!("0.{}{digits}", "0".repeat(scale - digits.len()))
        }
    };
    if neg { format!("-{body}") } else { body }
}

fn varint_text(v: &CqlVarint) -> String {
    varint_to_string(v.as_signed_bytes_be_slice())
}

fn decimal_text(d: &CqlDecimal) -> String {
    let (bytes, scale) = d.as_signed_be_bytes_slice_and_exponent();
    decimal_to_string(&varint_to_string(bytes), scale)
}

// ---------------------------------------------------------------------------------------------
// CqlValue -> Value
// ---------------------------------------------------------------------------------------------

/// Convert a cell. `ty` decides how maps are shown (text keys: document, else pairs).
pub(crate) fn to_value(v: &CqlValue, ty: &CqlType) -> Value {
    let ty = ty.unfrozen();
    match v {
        CqlValue::Ascii(s) | CqlValue::Text(s) => Value::Text(s.clone()),
        CqlValue::Boolean(b) => Value::Bool(*b),
        CqlValue::Blob(b) => Value::Bytes(b.clone()),
        CqlValue::Counter(c) => Value::Int(c.0),
        CqlValue::Decimal(d) => Value::Decimal(decimal_text(d)),
        CqlValue::Date(d) => Value::Date(format_date(i64::from(d.0) - (1i64 << 31))),
        CqlValue::Double(f) => Value::Float(*f),
        CqlValue::Duration(d) => Value::Ext(ExtValue::new("duration", format_duration(d))),
        CqlValue::Empty => Value::Ext(ExtValue::new("empty", "")),
        CqlValue::Float(f) => Value::Float(f.to_string().parse().unwrap_or(f64::from(*f))),
        CqlValue::Int(i) => Value::Int(i64::from(*i)),
        CqlValue::BigInt(i) => Value::Int(*i),
        CqlValue::SmallInt(i) => Value::Int(i64::from(*i)),
        CqlValue::TinyInt(i) => Value::Int(i64::from(*i)),
        CqlValue::Timestamp(t) => Value::Timestamp(format_timestamp(t.0)),
        CqlValue::Inet(ip) => Value::Text(ip.to_string()),
        CqlValue::Time(t) => Value::Time(format_time(t.0)),
        CqlValue::Timeuuid(u) => Value::Uuid(u.to_string()),
        CqlValue::Uuid(u) => Value::Uuid(u.to_string()),
        CqlValue::Varint(n) => Value::Decimal(varint_text(n)),
        CqlValue::List(items) | CqlValue::Set(items) | CqlValue::Vector(items) => {
            let elem = match ty {
                CqlType::List(t) | CqlType::Set(t) | CqlType::Vector(t, _) => (**t).clone(),
                _ => CqlType::Custom(String::new()),
            };
            Value::Array(items.iter().map(|i| to_value(i, &elem)).collect())
        }
        CqlValue::Map(pairs) => {
            let (kt, vt) = match ty {
                CqlType::Map(k, v) => ((**k).clone(), (**v).clone()),
                _ => (
                    CqlType::Custom(String::new()),
                    CqlType::Custom(String::new()),
                ),
            };
            let text_keys = kt.is_text()
                || (matches!(kt, CqlType::Custom(_))
                    && pairs
                        .iter()
                        .all(|(k, _)| matches!(k, CqlValue::Text(_) | CqlValue::Ascii(_))));
            if text_keys {
                Value::Document(
                    pairs
                        .iter()
                        .map(|(k, val)| {
                            let key = match k {
                                CqlValue::Text(s) | CqlValue::Ascii(s) => s.clone(),
                                other => format!("{other:?}"),
                            };
                            (key, to_value(val, &vt))
                        })
                        .collect(),
                )
            } else {
                Value::Array(
                    pairs
                        .iter()
                        .map(|(k, val)| Value::Array(vec![to_value(k, &kt), to_value(val, &vt)]))
                        .collect(),
                )
            }
        }
        CqlValue::UserDefinedType { fields, .. } => {
            let defs: &[(String, CqlType)] = match ty {
                CqlType::Udt { fields, .. } => fields,
                _ => &[],
            };
            Value::Document(
                fields
                    .iter()
                    .map(|(name, val)| {
                        let fty = defs
                            .iter()
                            .find(|(n, _)| n == name)
                            .map(|(_, t)| t.clone())
                            .unwrap_or(CqlType::Custom(String::new()));
                        (
                            name.clone(),
                            val.as_ref().map_or(Value::Null, |x| to_value(x, &fty)),
                        )
                    })
                    .collect(),
            )
        }
        CqlValue::Tuple(items) => {
            let tys: Vec<CqlType> = match ty {
                CqlType::Tuple(ts) => ts.clone(),
                _ => Vec::new(),
            };
            Value::Array(
                items
                    .iter()
                    .enumerate()
                    .map(|(i, val)| {
                        let t = tys
                            .get(i)
                            .cloned()
                            .unwrap_or(CqlType::Custom(String::new()));
                        val.as_ref().map_or(Value::Null, |x| to_value(x, &t))
                    })
                    .collect(),
            )
        }
        _ => Value::Text(format!("{v:?}")),
    }
}

// ---------------------------------------------------------------------------------------------
// Value -> CQL literal
// ---------------------------------------------------------------------------------------------

fn invalid(ty: &CqlType, what: &str) -> Error {
    Error::Invalid(format!("cannot use {what} as a CQL {ty} value"))
}

fn is_integer_text(s: &str) -> bool {
    let d = s.strip_prefix(['-', '+']).unwrap_or(s);
    !d.is_empty() && d.chars().all(|c| c.is_ascii_digit())
}

fn is_decimal_text(s: &str) -> bool {
    let d = s.strip_prefix(['-', '+']).unwrap_or(s);
    let (mant, exp) = match d.find(['e', 'E']) {
        Some(i) => (&d[..i], Some(&d[i + 1..])),
        None => (d, None),
    };
    let mant_ok = {
        let mut parts = mant.splitn(2, '.');
        let a = parts.next().unwrap_or("");
        let b = parts.next();
        (!a.is_empty() || b.is_some_and(|b| !b.is_empty()))
            && a.chars().all(|c| c.is_ascii_digit())
            && b.is_none_or(|b| b.chars().all(|c| c.is_ascii_digit()))
    };
    mant_ok && exp.is_none_or(is_integer_text)
}

fn is_uuid_text(s: &str) -> bool {
    s.len() == 36
        && s.chars().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

fn is_hex_blob(s: &str) -> bool {
    s.len() >= 2
        && (s.starts_with("0x") || s.starts_with("0X"))
        && s[2..].len() % 2 == 0
        && s[2..].chars().all(|c| c.is_ascii_hexdigit())
}

fn is_duration_text(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | ':' | '.' | 'µ'))
}

/// A balanced literal typed by a user for a collection/UDT cell (quotes and brackets close).
fn is_balanced_literal(s: &str) -> bool {
    let mut depth = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => loop {
                match chars.next() {
                    Some('\'') => {
                        if chars.peek() == Some(&'\'') {
                            chars.next();
                        } else {
                            break;
                        }
                    }
                    Some(_) => {}
                    None => return false,
                }
            },
            '[' | '{' | '(' => depth.push(c),
            ']' => {
                if depth.pop() != Some('[') {
                    return false;
                }
            }
            '}' => {
                if depth.pop() != Some('{') {
                    return false;
                }
            }
            ')' => {
                if depth.pop() != Some('(') {
                    return false;
                }
            }
            ';' => return false,
            _ => {}
        }
    }
    depth.is_empty()
}

fn float_literal(f: f64) -> String {
    if f.is_nan() {
        "NaN".into()
    } else if f.is_infinite() {
        if f > 0.0 { "Infinity" } else { "-Infinity" }.into()
    } else {
        format!("{f:?}")
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(2 + bytes.len() * 2);
    s.push_str("0x");
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Render `v` as a CQL literal for a column of type `ty`.
pub(crate) fn literal(v: &Value, ty: &CqlType) -> Result<String> {
    let ty = ty.unfrozen();
    if let Value::Null = v {
        return Ok("null".into());
    }
    if let Value::Ext(e) = v {
        if e.ty == "empty" {
            return match ty.native_name() {
                Some(n) => Ok(format!("blobAs{n}(0x)")),
                None => Err(invalid(ty, "an empty value")),
            };
        }
    }
    match ty {
        CqlType::Ascii | CqlType::Text | CqlType::Inet | CqlType::Date | CqlType::Time => Ok(
            quote_text(&scalar_text(v).ok_or_else(|| invalid(ty, "this value"))?),
        ),
        CqlType::Timestamp => match v {
            Value::Int(i) => Ok(i.to_string()),
            _ => Ok(quote_text(
                &scalar_text(v).ok_or_else(|| invalid(ty, "this value"))?,
            )),
        },
        CqlType::Boolean => match v {
            Value::Bool(b) => Ok(b.to_string()),
            other => match scalar_text(other).as_deref().map(str::to_ascii_lowercase) {
                Some(s) if s == "true" || s == "false" => Ok(s),
                _ => Err(invalid(ty, "this value")),
            },
        },
        CqlType::BigInt
        | CqlType::Int
        | CqlType::SmallInt
        | CqlType::TinyInt
        | CqlType::Counter => match v {
            Value::Int(i) => Ok(i.to_string()),
            Value::UInt(u) => Ok(u.to_string()),
            Value::Decimal(s) | Value::Text(s) if is_integer_text(s.trim()) => {
                Ok(s.trim().to_string())
            }
            _ => Err(invalid(ty, "this value")),
        },
        CqlType::Varint => match v {
            Value::Int(i) => Ok(i.to_string()),
            Value::UInt(u) => Ok(u.to_string()),
            Value::Decimal(s) | Value::Text(s) if is_integer_text(s.trim()) => {
                Ok(s.trim().to_string())
            }
            _ => Err(invalid(ty, "this value")),
        },
        CqlType::Decimal => match v {
            Value::Int(i) => Ok(i.to_string()),
            Value::UInt(u) => Ok(u.to_string()),
            Value::Float(f) if f.is_finite() => Ok(format!("{f}")),
            Value::Decimal(s) | Value::Text(s) if is_decimal_text(s.trim()) => {
                Ok(s.trim().to_string())
            }
            _ => Err(invalid(ty, "this value")),
        },
        CqlType::Double | CqlType::Float => match v {
            Value::Float(f) => Ok(float_literal(*f)),
            Value::Int(i) => Ok(i.to_string()),
            Value::Decimal(s) | Value::Text(s) => {
                let t = s.trim();
                if is_decimal_text(t) || matches!(t, "NaN" | "Infinity" | "-Infinity") {
                    Ok(t.to_string())
                } else {
                    Err(invalid(ty, "this text"))
                }
            }
            _ => Err(invalid(ty, "this value")),
        },
        CqlType::Blob => match v {
            Value::Bytes(b) => Ok(hex(b)),
            Value::Text(s) if is_hex_blob(s.trim()) => Ok(s.trim().to_string()),
            _ => Err(invalid(ty, "this value (expected bytes or 0x... text)")),
        },
        CqlType::Uuid | CqlType::Timeuuid => match v {
            Value::Uuid(s) | Value::Text(s) if is_uuid_text(s.trim()) => Ok(s.trim().to_string()),
            _ => Err(invalid(ty, "this value (expected a UUID)")),
        },
        CqlType::Duration => match v {
            Value::Ext(e) if is_duration_text(&e.text) => Ok(e.text.clone()),
            Value::Text(s) if is_duration_text(s.trim()) => Ok(s.trim().to_string()),
            _ => Err(invalid(ty, "this value (expected e.g. 1h30m)")),
        },
        CqlType::List(t) | CqlType::Vector(t, _) => match v {
            Value::Array(items) => Ok(format!("[{}]", join_literals(items, t)?)),
            other => typed_text_literal(other, ty),
        },
        CqlType::Set(t) => match v {
            Value::Array(items) => Ok(format!("{{{}}}", join_literals(items, t)?)),
            other => typed_text_literal(other, ty),
        },
        CqlType::Tuple(ts) => match v {
            Value::Array(items) => {
                if items.len() != ts.len() {
                    return Err(Error::Invalid(format!(
                        "tuple needs {} elements, got {}",
                        ts.len(),
                        items.len()
                    )));
                }
                let parts = items
                    .iter()
                    .zip(ts)
                    .map(|(i, t)| literal(i, t))
                    .collect::<Result<Vec<_>>>()?;
                Ok(format!("({})", parts.join(", ")))
            }
            other => typed_text_literal(other, ty),
        },
        CqlType::Map(kt, vt) => match v {
            Value::Document(fields) => {
                let parts = fields
                    .iter()
                    .map(|(k, val)| {
                        Ok(format!(
                            "{}: {}",
                            literal(&Value::Text(k.clone()), kt)?,
                            literal(val, vt)?
                        ))
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(format!("{{{}}}", parts.join(", ")))
            }
            Value::Array(pairs) => {
                let parts = pairs
                    .iter()
                    .map(|p| match p {
                        Value::Array(kv) if kv.len() == 2 => Ok(format!(
                            "{}: {}",
                            literal(&kv[0], kt)?,
                            literal(&kv[1], vt)?
                        )),
                        _ => Err(Error::Invalid(
                            "map entries must be [key, value] pairs".into(),
                        )),
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(format!("{{{}}}", parts.join(", ")))
            }
            other => typed_text_literal(other, ty),
        },
        CqlType::Udt { fields, .. } => match v {
            Value::Document(vals) => {
                let parts = vals
                    .iter()
                    .map(|(name, val)| {
                        let fty = fields
                            .iter()
                            .find(|(n, _)| n == name)
                            .map(|(_, t)| t.clone());
                        let lit = match fty {
                            Some(t) => literal(val, &t)?,
                            None => untyped_literal(val)?,
                        };
                        Ok(format!("{}: {lit}", quote_ident(name)))
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(format!("{{{}}}", parts.join(", ")))
            }
            other => typed_text_literal(other, ty),
        },
        CqlType::Frozen(_) => unreachable!("unfrozen above"),
        CqlType::Custom(_) => match v {
            Value::Bytes(b) => Ok(hex(b)),
            Value::Text(s) if is_hex_blob(s.trim()) => Ok(s.trim().to_string()),
            _ => Err(invalid(ty, "this value")),
        },
    }
}

fn join_literals(items: &[Value], t: &CqlType) -> Result<String> {
    Ok(items
        .iter()
        .map(|i| literal(i, t))
        .collect::<Result<Vec<_>>>()?
        .join(", "))
}

/// A user-typed CQL literal for a composite type (`[1, 2]`, `{'a': 1}`), accepted when balanced.
fn typed_text_literal(v: &Value, ty: &CqlType) -> Result<String> {
    match v {
        Value::Text(s) | Value::Json(s) => {
            let t = s.trim();
            if t.starts_with(['[', '{', '(']) && is_balanced_literal(t) {
                Ok(t.to_string())
            } else {
                Err(invalid(
                    ty,
                    "this text (expected a CQL literal such as [1, 2])",
                ))
            }
        }
        _ => Err(invalid(ty, "this value")),
    }
}

/// Literal of a value whose column type is unknown (UDT field not in the definition).
fn untyped_literal(v: &Value) -> Result<String> {
    Ok(match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Float(f) => float_literal(*f),
        Value::Decimal(s) => s.clone(),
        Value::Bytes(b) => hex(b),
        Value::Uuid(s) => s.clone(),
        Value::Text(s)
        | Value::Date(s)
        | Value::Time(s)
        | Value::DateTime(s)
        | Value::Timestamp(s)
        | Value::Json(s) => quote_text(s),
        Value::Ext(e) => e.text.clone(),
        Value::Array(items) => format!(
            "[{}]",
            items
                .iter()
                .map(untyped_literal)
                .collect::<Result<Vec<_>>>()?
                .join(", ")
        ),
        Value::Document(fields) => format!(
            "{{{}}}",
            fields
                .iter()
                .map(|(k, v)| Ok(format!("{}: {}", quote_ident(k), untyped_literal(v)?)))
                .collect::<Result<Vec<_>>>()?
                .join(", ")
        ),
    })
}

fn scalar_text(v: &Value) -> Option<String> {
    Some(match v {
        Value::Text(s)
        | Value::Date(s)
        | Value::Time(s)
        | Value::DateTime(s)
        | Value::Timestamp(s)
        | Value::Uuid(s)
        | Value::Decimal(s)
        | Value::Json(s) => s.clone(),
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Float(f) => format!("{f}"),
        Value::Bool(b) => b.to_string(),
        Value::Ext(e) => e.text.clone(),
        _ => return None,
    })
}

/// Literal of text typed by a user (filters, text edits) for a column of type `ty`.
pub(crate) fn literal_from_text(s: &str, ty: &CqlType) -> Result<String> {
    let t = ty.unfrozen();
    match t {
        CqlType::Ascii | CqlType::Text | CqlType::Inet | CqlType::Date | CqlType::Time => {
            Ok(quote_text(s))
        }
        CqlType::Timestamp => {
            let tr = s.trim();
            if is_integer_text(tr) {
                Ok(tr.to_string())
            } else {
                Ok(quote_text(tr))
            }
        }
        _ => literal(&Value::Text(s.to_string()), t),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scylla::value::{CqlDate, CqlTime, CqlTimestamp};

    #[test]
    fn dates_and_times() {
        assert_eq!(format_date(0), "1970-01-01");
        assert_eq!(format_date(19_874), "2024-05-31");
        assert_eq!(format_date(-1), "1969-12-31");
        assert_eq!(format_timestamp(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            format_timestamp(1_717_162_245_123),
            "2024-05-31T13:30:45.123Z"
        );
        assert_eq!(format_timestamp(-1), "1969-12-31T23:59:59.999Z");
        assert_eq!(format_time(49_500_000_000_000), "13:45:00");
        assert_eq!(format_time(1), "00:00:00.000000001");
        let epoch_date = CqlValue::Date(CqlDate(1 << 31));
        assert_eq!(
            to_value(&epoch_date, &CqlType::Date),
            Value::Date("1970-01-01".into())
        );
        assert_eq!(
            to_value(&CqlValue::Time(CqlTime(3_600_000_000_000)), &CqlType::Time),
            Value::Time("01:00:00".into())
        );
        assert_eq!(
            to_value(
                &CqlValue::Timestamp(CqlTimestamp(1000)),
                &CqlType::Timestamp
            ),
            Value::Timestamp("1970-01-01T00:00:01.000Z".into())
        );
    }

    #[test]
    fn big_numbers() {
        assert_eq!(varint_to_string(&[]), "0");
        assert_eq!(varint_to_string(&[0x00]), "0");
        assert_eq!(varint_to_string(&[0x7f]), "127");
        assert_eq!(varint_to_string(&[0xff]), "-1");
        assert_eq!(varint_to_string(&[0x80]), "-128");
        assert_eq!(varint_to_string(&[0x00, 0xff]), "255");
        // 2^64 = 18446744073709551616
        assert_eq!(
            varint_to_string(&[0x01, 0, 0, 0, 0, 0, 0, 0, 0]),
            "18446744073709551616"
        );
        assert_eq!(
            varint_to_string(&[0xff, 0, 0, 0, 0, 0, 0, 0, 0]),
            "-18446744073709551616"
        );
        assert_eq!(decimal_to_string("12345", 2), "123.45");
        assert_eq!(decimal_to_string("-5", 3), "-0.005");
        assert_eq!(decimal_to_string("12", -2), "1200");
        assert_eq!(decimal_to_string("0", 2), "0.00");
    }

    #[test]
    fn duration_text() {
        let d = CqlDuration {
            months: 1,
            days: 2,
            nanoseconds: 3,
        };
        assert_eq!(format_duration(&d), "1mo2d3ns");
        assert_eq!(
            to_value(&CqlValue::Duration(d), &CqlType::Duration),
            Value::Ext(ExtValue::new("duration", "1mo2d3ns"))
        );
    }

    #[test]
    fn maps_by_key_type() {
        let ty = CqlType::parse("map<text, int>");
        let v = CqlValue::Map(vec![(CqlValue::Text("a".into()), CqlValue::Int(1))]);
        assert_eq!(
            to_value(&v, &ty),
            Value::Document(vec![("a".into(), Value::Int(1))])
        );
        let ty = CqlType::parse("map<int, text>");
        let v = CqlValue::Map(vec![(CqlValue::Int(1), CqlValue::Text("x".into()))]);
        assert_eq!(
            to_value(&v, &ty),
            Value::Array(vec![Value::Array(vec![
                Value::Int(1),
                Value::Text("x".into())
            ])])
        );
    }

    #[test]
    fn scalar_literals() {
        let l = |v: Value, t: &str| literal(&v, &CqlType::parse(t)).unwrap();
        assert_eq!(l(Value::Text("it's".into()), "text"), "'it''s'");
        assert_eq!(l(Value::Int(5), "int"), "5");
        assert_eq!(l(Value::Null, "int"), "null");
        assert_eq!(l(Value::Bool(true), "boolean"), "true");
        assert_eq!(l(Value::Bytes(vec![1, 0xab]), "blob"), "0x01ab");
        assert_eq!(l(Value::Float(1.5), "double"), "1.5");
        assert_eq!(l(Value::Decimal("1.50".into()), "decimal"), "1.50");
        assert_eq!(
            l(
                Value::Uuid("123e4567-e89b-12d3-a456-426614174000".into()),
                "uuid"
            ),
            "123e4567-e89b-12d3-a456-426614174000"
        );
        assert_eq!(
            l(
                Value::Timestamp("2024-05-31T13:30:45.123Z".into()),
                "timestamp"
            ),
            "'2024-05-31T13:30:45.123Z'"
        );
        assert_eq!(
            l(Value::Ext(ExtValue::new("empty", "")), "int"),
            "blobAsint(0x)"
        );
        assert!(literal(&Value::Text("abc".into()), &CqlType::Int).is_err());
        assert!(literal(&Value::Text("1; DROP".into()), &CqlType::Decimal).is_err());
    }

    #[test]
    fn collection_literals() {
        let l = |v: Value, t: &str| literal(&v, &CqlType::parse(t)).unwrap();
        assert_eq!(
            l(
                Value::Array(vec![Value::Int(1), Value::Int(2)]),
                "list<int>"
            ),
            "[1, 2]"
        );
        assert_eq!(
            l(
                Value::Array(vec![Value::Text("a".into())]),
                "frozen<set<text>>"
            ),
            "{'a'}"
        );
        assert_eq!(
            l(
                Value::Document(vec![("k".into(), Value::Int(1))]),
                "map<text, int>"
            ),
            "{'k': 1}"
        );
        assert_eq!(
            l(
                Value::Array(vec![Value::Array(vec![
                    Value::Int(1),
                    Value::Text("x".into())
                ])]),
                "map<int, text>"
            ),
            "{1: 'x'}"
        );
        assert_eq!(
            l(
                Value::Array(vec![Value::Int(1), Value::Text("t".into())]),
                "tuple<int, text>"
            ),
            "(1, 't')"
        );
        assert_eq!(
            l(
                Value::Array(vec![Value::Float(1.0), Value::Float(2.5)]),
                "vector<float, 2>"
            ),
            "[1.0, 2.5]"
        );
        assert_eq!(l(Value::Text("[1, 2]".into()), "list<int>"), "[1, 2]");
        assert!(
            literal(
                &Value::Text("[1]; drop".into()),
                &CqlType::parse("list<int>")
            )
            .is_err()
        );
        assert!(literal(&Value::Text("[1".into()), &CqlType::parse("list<int>")).is_err());
    }

    #[test]
    fn udt_literal_uses_field_types() {
        let ty = CqlType::Udt {
            name: "address".into(),
            fields: vec![("zip".into(), CqlType::Int), ("City".into(), CqlType::Text)],
        };
        let v = Value::Document(vec![
            ("zip".into(), Value::Int(1)),
            ("City".into(), Value::Text("x".into())),
        ]);
        assert_eq!(literal(&v, &ty).unwrap(), "{zip: 1, \"City\": 'x'}");
    }

    #[test]
    fn text_literals_by_type() {
        assert_eq!(literal_from_text("a'b", &CqlType::Text).unwrap(), "'a''b'");
        assert_eq!(literal_from_text("12", &CqlType::Int).unwrap(), "12");
        assert!(literal_from_text("x", &CqlType::Int).is_err());
        assert_eq!(
            literal_from_text("1700000000000", &CqlType::Timestamp).unwrap(),
            "1700000000000"
        );
        assert_eq!(
            literal_from_text("2024-01-01", &CqlType::Timestamp).unwrap(),
            "'2024-01-01'"
        );
    }
}
