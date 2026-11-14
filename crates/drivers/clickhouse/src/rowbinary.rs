//! `RowBinaryWithNamesAndTypes` decoder and `RowBinary` encoder.
//!
//! The decoder works on a byte slice and reports [`DecErr::NeedMore`] when the slice ends in the
//! middle of a value, so a streaming caller can append the next network chunk and retry the row.
//! Value mapping follows the design table (`docs/design/backlog-gd9.md` §3.2.2).

use dbstudio_core::{Error, ExtValue, Result, Value};

use crate::types::CType;

/// Decoding failure.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum DecErr {
    /// The slice ended inside a value.
    NeedMore,
    /// The bytes are not valid RowBinary for the declared types.
    Bad(String),
    /// A type that cannot be decoded (the caller may fall back to another format).
    Unsupported(String),
}

type R<T> = std::result::Result<T, DecErr>;

/// Byte cursor over a slice.
pub(crate) struct Cur<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Cur<'a> {
    pub(crate) fn new(b: &'a [u8]) -> Self {
        Cur { b, p: 0 }
    }

    pub(crate) fn pos(&self) -> usize {
        self.p
    }

    #[cfg(test)]
    pub(crate) fn at_end(&self) -> bool {
        self.p >= self.b.len()
    }

    fn take(&mut self, n: usize) -> R<&'a [u8]> {
        let end = self.p.checked_add(n).ok_or(DecErr::NeedMore)?;
        if end > self.b.len() {
            return Err(DecErr::NeedMore);
        }
        let s = &self.b[self.p..end];
        self.p = end;
        Ok(s)
    }

    fn u8(&mut self) -> R<u8> {
        Ok(self.take(1)?[0])
    }

    fn arr<const N: usize>(&mut self) -> R<[u8; N]> {
        let s = self.take(N)?;
        let mut a = [0u8; N];
        a.copy_from_slice(s);
        Ok(a)
    }

    fn varuint(&mut self) -> R<u64> {
        let mut v = 0u64;
        for shift in (0..70).step_by(7) {
            let b = self.u8()?;
            if shift < 64 {
                v |= u64::from(b & 0x7f) << shift;
            }
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(DecErr::Bad("varint too long".into()))
    }

    fn len(&mut self) -> R<usize> {
        usize::try_from(self.varuint()?).map_err(|_| DecErr::Bad("length overflow".into()))
    }

    fn string_bytes(&mut self) -> R<&'a [u8]> {
        let n = self.len()?;
        self.take(n)
    }

    fn string(&mut self) -> R<String> {
        Ok(String::from_utf8_lossy(self.string_bytes()?).into_owned())
    }
}

/// Column names and types of the response header. `None` while the header is incomplete.
pub(crate) fn decode_header(buf: &[u8]) -> R<(Vec<(String, String)>, usize)> {
    let mut c = Cur::new(buf);
    let n = c.len()?;
    if n > 100_000 {
        return Err(DecErr::Bad("implausible column count".into()));
    }
    let mut names = Vec::with_capacity(n.min(1024));
    for _ in 0..n {
        names.push(c.string()?);
    }
    let mut cols = Vec::with_capacity(n.min(1024));
    for name in names {
        cols.push((name, c.string()?));
    }
    Ok((cols, c.pos()))
}

/// Decode one row (all `types`) from the cursor.
pub(crate) fn decode_row(c: &mut Cur<'_>, types: &[CType]) -> R<Vec<Value>> {
    let mut row = Vec::with_capacity(types.len());
    for t in types {
        row.push(decode_value(c, t)?);
    }
    Ok(row)
}

const DAYS_PER_400Y: i64 = 146_097;

/// Civil date of a day count since 1970-01-01 (proleptic Gregorian).
pub(crate) fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(DAYS_PER_400Y);
    let doe = z.rem_euclid(DAYS_PER_400Y);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Day count since 1970-01-01 of a civil date.
pub(crate) fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = i64::from(if m > 2 { m - 3 } else { m + 9 });
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * DAYS_PER_400Y + doe - 719_468
}

pub(crate) fn fmt_date(days: i64) -> String {
    let (y, m, d) = civil_from_days(days);
    if y < 0 {
        format!("-{:04}-{m:02}-{d:02}", -y)
    } else {
        format!("{y:04}-{m:02}-{d:02}")
    }
}

/// `YYYY-MM-DDTHH:MM:SS[.fff]Z` for `ticks` units of `10^-precision` seconds since the epoch.
pub(crate) fn fmt_timestamp(ticks: i64, precision: u8) -> String {
    let scale = 10i64.pow(u32::from(precision.min(18)));
    let secs = ticks.div_euclid(scale);
    let frac = ticks.rem_euclid(scale);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (h, mi, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    let date = fmt_date(days);
    if precision == 0 {
        format!("{date}T{h:02}:{mi:02}:{s:02}Z")
    } else {
        format!(
            "{date}T{h:02}:{mi:02}:{s:02}.{frac:0width$}Z",
            width = usize::from(precision)
        )
    }
}

/// Decimal text of a sign-magnitude number with `scale` fractional digits.
pub(crate) fn fmt_decimal(neg: bool, digits: &str, scale: u8) -> String {
    let scale = usize::from(scale);
    let mut d = digits.trim_start_matches('0').to_string();
    if d.is_empty() {
        d.push('0');
    }
    let body = if scale == 0 {
        d
    } else {
        if d.len() <= scale {
            d = format!("{}{d}", "0".repeat(scale - d.len() + 1));
        }
        let split = d.len() - scale;
        format!("{}.{}", &d[..split], &d[split..])
    };
    if neg && body.chars().any(|c| c.is_ascii_digit() && c != '0') {
        format!("-{body}")
    } else {
        body
    }
}

/// Magnitude of a 256-bit integer as four little-endian 64-bit limbs.
fn limbs(bytes: [u8; 32]) -> [u64; 4] {
    let mut l = [0u64; 4];
    for (i, chunk) in bytes.as_chunks::<8>().0.iter().enumerate() {
        l[i] = u64::from_le_bytes(*chunk);
    }
    l
}

fn decimal_digits_of_limbs(mut l: [u64; 4]) -> String {
    if l == [0; 4] {
        return "0".into();
    }
    let mut parts: Vec<u64> = Vec::new();
    const BASE: u128 = 10_000_000_000_000_000_000; // 10^19
    while l != [0; 4] {
        let mut rem: u128 = 0;
        for limb in l.iter_mut().rev() {
            let cur = (rem << 64) | u128::from(*limb);
            *limb = (cur / BASE) as u64;
            rem = cur % BASE;
        }
        parts.push(rem as u64);
    }
    let mut out = parts.pop().map(|p| p.to_string()).unwrap_or_default();
    for p in parts.iter().rev() {
        out.push_str(&format!("{p:019}"));
    }
    out
}

/// (negative, magnitude digits) of a 256-bit integer.
fn i256_parts(bytes: [u8; 32], signed: bool) -> (bool, String) {
    let mut l = limbs(bytes);
    let neg = signed && (bytes[31] & 0x80) != 0;
    if neg {
        // Two's complement negation.
        let mut carry = true;
        for limb in &mut l {
            let (v, c) = (!*limb).overflowing_add(u64::from(carry));
            *limb = v;
            carry = c;
        }
    }
    (neg, decimal_digits_of_limbs(l))
}

fn f32_value(f: f32) -> Value {
    if f.is_finite() {
        // Shortest decimal text of the f32 so 0.1f32 shows as 0.1, not 0.10000000149011612.
        Value::Float(f.to_string().parse::<f64>().unwrap_or(f64::from(f)))
    } else {
        Value::Float(f64::from(f))
    }
}

fn text_or_bytes(b: &[u8]) -> Value {
    match std::str::from_utf8(b) {
        Ok(s) => Value::Text(s.to_string()),
        Err(_) => Value::Bytes(b.to_vec()),
    }
}

/// UUID text of the 16 RowBinary bytes (two little-endian 64-bit halves).
pub(crate) fn fmt_uuid(raw: [u8; 16]) -> String {
    let mut b = [0u8; 16];
    for i in 0..8 {
        b[i] = raw[7 - i];
        b[8 + i] = raw[15 - i];
    }
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0],
        b[1],
        b[2],
        b[3],
        b[4],
        b[5],
        b[6],
        b[7],
        b[8],
        b[9],
        b[10],
        b[11],
        b[12],
        b[13],
        b[14],
        b[15]
    )
}

fn decode_value(c: &mut Cur<'_>, t: &CType) -> R<Value> {
    Ok(match t {
        CType::Int8 => Value::Int(i64::from(c.u8()? as i8)),
        CType::Int16 => Value::Int(i64::from(i16::from_le_bytes(c.arr()?))),
        CType::Int32 => Value::Int(i64::from(i32::from_le_bytes(c.arr()?))),
        CType::Int64 => Value::Int(i64::from_le_bytes(c.arr()?)),
        CType::UInt8 => Value::Int(i64::from(c.u8()?)),
        CType::UInt16 => Value::Int(i64::from(u16::from_le_bytes(c.arr()?))),
        CType::UInt32 => Value::Int(i64::from(u32::from_le_bytes(c.arr()?))),
        CType::UInt64 => Value::UInt(u64::from_le_bytes(c.arr()?)),
        CType::Int128 => Value::Decimal(i128::from_le_bytes(c.arr()?).to_string()),
        CType::UInt128 => Value::Decimal(u128::from_le_bytes(c.arr()?).to_string()),
        CType::Int256 => {
            let (neg, d) = i256_parts(c.arr()?, true);
            Value::Decimal(if neg { format!("-{d}") } else { d })
        }
        CType::UInt256 => Value::Decimal(i256_parts(c.arr()?, false).1),
        CType::Float32 => f32_value(f32::from_le_bytes(c.arr()?)),
        CType::Float64 => Value::Float(f64::from_le_bytes(c.arr()?)),
        CType::BFloat16 => {
            let hi = u16::from_le_bytes(c.arr()?);
            f32_value(f32::from_bits(u32::from(hi) << 16))
        }
        CType::Bool => Value::Bool(c.u8()? != 0),
        CType::String => text_or_bytes(c.string_bytes()?),
        CType::FixedString(n) => {
            let b = c.take(*n)?;
            // Padding NULs are part of the value: show them as bytes instead of hiding them.
            if b.contains(&0) {
                Value::Bytes(b.to_vec())
            } else {
                text_or_bytes(b)
            }
        }
        CType::Uuid => Value::Uuid(fmt_uuid(c.arr()?)),
        CType::Date => Value::Date(fmt_date(i64::from(u16::from_le_bytes(c.arr()?)))),
        CType::Date32 => Value::Date(fmt_date(i64::from(i32::from_le_bytes(c.arr()?)))),
        CType::DateTime(_) => {
            Value::Timestamp(fmt_timestamp(i64::from(u32::from_le_bytes(c.arr()?)), 0))
        }
        CType::DateTime64(p, _) => {
            Value::Timestamp(fmt_timestamp(i64::from_le_bytes(c.arr()?), *p))
        }
        CType::Decimal(p, s) => {
            let (neg, digits) = match *p {
                0..=9 => {
                    let v = i32::from_le_bytes(c.arr()?);
                    (v < 0, v.unsigned_abs().to_string())
                }
                10..=18 => {
                    let v = i64::from_le_bytes(c.arr()?);
                    (v < 0, v.unsigned_abs().to_string())
                }
                19..=38 => {
                    let v = i128::from_le_bytes(c.arr()?);
                    (v < 0, v.unsigned_abs().to_string())
                }
                _ => i256_parts(c.arr()?, true),
            };
            Value::Decimal(fmt_decimal(neg, &digits, *s))
        }
        CType::Enum8(items) => {
            let v = i16::from(c.u8()? as i8);
            enum_label(items, v)
        }
        CType::Enum16(items) => enum_label(items, i16::from_le_bytes(c.arr()?)),
        CType::IPv4 => {
            Value::Text(std::net::Ipv4Addr::from(u32::from_le_bytes(c.arr()?)).to_string())
        }
        CType::IPv6 => Value::Text(std::net::Ipv6Addr::from(c.arr::<16>()?).to_string()),
        CType::Nullable(inner) => {
            if c.u8()? != 0 {
                Value::Null
            } else {
                decode_value(c, inner)?
            }
        }
        CType::LowCardinality(inner) => decode_value(c, inner)?,
        CType::Array(inner) => {
            let n = c.len()?;
            let mut out = Vec::with_capacity(n.min(1024));
            for _ in 0..n {
                out.push(decode_value(c, inner)?);
            }
            Value::Array(out)
        }
        CType::Tuple(items) => {
            let mut vals = Vec::with_capacity(items.len());
            for (_, t) in items {
                vals.push(decode_value(c, t)?);
            }
            if !items.is_empty() && items.iter().all(|(n, _)| n.is_some()) {
                Value::Document(
                    items
                        .iter()
                        .zip(vals)
                        .map(|((n, _), v)| (n.clone().unwrap_or_default(), v))
                        .collect(),
                )
            } else {
                Value::Array(vals)
            }
        }
        CType::Map(k, v) => {
            let n = c.len()?;
            let string_keys = matches!(k.inner(), CType::String | CType::FixedString(_));
            let mut doc = Vec::new();
            let mut pairs = Vec::new();
            for _ in 0..n {
                let key = decode_value(c, k)?;
                let val = decode_value(c, v)?;
                if string_keys {
                    doc.push((value_text(&key), val));
                } else {
                    pairs.push(Value::Array(vec![key, val]));
                }
            }
            if string_keys {
                Value::Document(doc)
            } else {
                Value::Array(pairs)
            }
        }
        CType::Json => Value::Json(String::from_utf8_lossy(c.string_bytes()?).into_owned()),
        CType::Variant(items) => {
            let d = c.u8()?;
            if d == 0xff {
                Value::Null
            } else {
                let t = items.get(usize::from(d)).ok_or_else(|| {
                    DecErr::Bad(format!("variant discriminator {d} out of range"))
                })?;
                decode_value(c, t)?
            }
        }
        CType::Dynamic => {
            let t = read_binary_type(c)?;
            if t == CType::Nothing {
                Value::Null
            } else {
                decode_value(c, &t)?
            }
        }
        CType::Nothing => Value::Null,
        CType::Unsupported(name) => return Err(DecErr::Unsupported(name.clone())),
    })
}

fn enum_label(items: &[(String, i16)], v: i16) -> Value {
    match items.iter().find(|(_, x)| *x == v) {
        Some((label, _)) => Value::Text(label.clone()),
        None => Value::Text(v.to_string()),
    }
}

/// Text of a scalar value (map keys).
pub(crate) fn value_text(v: &Value) -> String {
    match v {
        Value::Text(s)
        | Value::Decimal(s)
        | Value::Date(s)
        | Value::Time(s)
        | Value::DateTime(s)
        | Value::Timestamp(s)
        | Value::Uuid(s)
        | Value::Json(s) => s.clone(),
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => "NULL".into(),
        Value::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        Value::Ext(e) => e.text.clone(),
        Value::Array(_) | Value::Document(_) => format!("{v:?}"),
    }
}

/// Binary type encoding of `Dynamic` values (`BinaryTypeIndex`).
fn read_binary_type(c: &mut Cur<'_>) -> R<CType> {
    read_binary_type_at(c, 0)
}

const MAX_TYPE_DEPTH: u32 = 64;

fn read_binary_type_at(c: &mut Cur<'_>, depth: u32) -> R<CType> {
    if depth > MAX_TYPE_DEPTH {
        return Err(DecErr::Bad("Dynamic type nested too deeply".into()));
    }
    let d = depth + 1;
    let tag = c.u8()?;
    Ok(match tag {
        0 => CType::Nothing,
        1 => CType::UInt8,
        2 => CType::UInt16,
        3 => CType::UInt32,
        4 => CType::UInt64,
        5 => CType::UInt128,
        6 => CType::UInt256,
        7 => CType::Int8,
        8 => CType::Int16,
        9 => CType::Int32,
        10 => CType::Int64,
        11 => CType::Int128,
        12 => CType::Int256,
        13 => CType::Float32,
        14 => CType::Float64,
        15 => CType::Date,
        16 => CType::Date32,
        17 => CType::DateTime(None),
        18 => CType::DateTime(Some(c.string()?)),
        19 => CType::DateTime64(c.u8()?, None),
        20 => {
            let p = c.u8()?;
            CType::DateTime64(p, Some(c.string()?))
        }
        21 => CType::String,
        22 => CType::FixedString(c.len()?),
        23 | 24 => {
            let n = c.len()?;
            let mut items = Vec::new();
            for _ in 0..n {
                let name = c.string()?;
                let v = if tag == 23 {
                    i16::from(c.u8()? as i8)
                } else {
                    i16::from_le_bytes(c.arr()?)
                };
                items.push((name, v));
            }
            if tag == 23 {
                CType::Enum8(items)
            } else {
                CType::Enum16(items)
            }
        }
        25..=28 => {
            let p = c.u8()?;
            let s = c.u8()?;
            CType::Decimal(p, s)
        }
        29 => CType::Uuid,
        30 => CType::Array(Box::new(read_binary_type_at(c, d)?)),
        31 => {
            let n = c.len()?;
            let mut items = Vec::new();
            for _ in 0..n {
                items.push((None, read_binary_type_at(c, d)?));
            }
            CType::Tuple(items)
        }
        32 => {
            let n = c.len()?;
            let mut items = Vec::new();
            for _ in 0..n {
                let name = c.string()?;
                items.push((Some(name), read_binary_type_at(c, d)?));
            }
            CType::Tuple(items)
        }
        35 => CType::Nullable(Box::new(read_binary_type_at(c, d)?)),
        38 => CType::LowCardinality(Box::new(read_binary_type_at(c, d)?)),
        39 => {
            let k = read_binary_type_at(c, d)?;
            let v = read_binary_type_at(c, d)?;
            CType::Map(Box::new(k), Box::new(v))
        }
        40 => CType::IPv4,
        41 => CType::IPv6,
        42 => {
            let n = c.len()?;
            let mut items = Vec::new();
            for _ in 0..n {
                items.push(read_binary_type_at(c, d)?);
            }
            CType::Variant(items)
        }
        45 => CType::Bool,
        49 => CType::BFloat16,
        48 => {
            // JSON(max_dynamic_paths) in the binary encoding carries parameters; keep it simple.
            return Err(DecErr::Unsupported("JSON inside Dynamic".into()));
        }
        other => return Err(DecErr::Unsupported(format!("Dynamic subtype #{other}"))),
    })
}

// ---------------------------------------------------------------------------------------------
// Encoder (RowBinary / RowBinaryWithDefaults)
// ---------------------------------------------------------------------------------------------

fn put_varuint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

fn bad(t: &CType, v: &Value, why: &str) -> Error {
    Error::Invalid(format!(
        "cannot store {} in a {} column ({why})",
        value_summary(v),
        type_label(t)
    ))
}

fn value_summary(v: &Value) -> String {
    let text = value_text(v);
    let short: String = text.chars().take(40).collect();
    format!("`{short}`")
}

fn type_label(t: &CType) -> String {
    format!("{t:?}")
        .split(['(', ' '])
        .next()
        .unwrap_or("?")
        .to_string()
}

/// Integer text of a value (accepts numbers, decimal-looking text, booleans).
fn int_text(t: &CType, v: &Value) -> Result<String> {
    Ok(match v {
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Bool(b) => u8::from(*b).to_string(),
        Value::Decimal(s) | Value::Text(s) => {
            let s = s.trim();
            let (int_part, frac) = s.split_once('.').unwrap_or((s, ""));
            if !frac.chars().all(|c| c == '0') {
                return Err(bad(t, v, "not a whole number"));
            }
            int_part.to_string()
        }
        Value::Float(f) if f.fract() == 0.0 && f.abs() < 9.0e15 => format!("{}", *f as i64),
        _ => return Err(bad(t, v, "not a number")),
    })
}

fn parse_int<T: std::str::FromStr>(t: &CType, v: &Value) -> Result<T> {
    let s = int_text(t, v)?;
    s.trim_start_matches('+')
        .parse::<T>()
        .map_err(|_| bad(t, v, "out of range or not a number"))
}

/// Little-endian two's complement bytes of a decimal integer text, `N` bytes wide.
fn int_text_to_le<const N: usize>(
    t: &CType,
    v: &Value,
    text: &str,
    signed: bool,
) -> Result<[u8; N]> {
    let text = text.trim().trim_start_matches('+');
    let (neg, digits) = match text.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, text),
    };
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return Err(bad(t, v, "not an integer"));
    }
    if neg && !signed {
        return Err(bad(t, v, "negative value"));
    }
    // Multiply-accumulate into little-endian bytes.
    let mut acc = [0u8; N];
    for ch in digits.bytes() {
        let mut carry = u32::from(ch - b'0');
        for byte in acc.iter_mut() {
            let x = u32::from(*byte) * 10 + carry;
            *byte = (x & 0xff) as u8;
            carry = x >> 8;
        }
        if carry != 0 {
            return Err(bad(t, v, "out of range"));
        }
    }
    let top_bit = acc[N - 1] & 0x80 != 0;
    if signed {
        if !neg && top_bit {
            return Err(bad(t, v, "out of range"));
        }
        if neg {
            // Negate (two's complement).
            let mut carry = true;
            for byte in acc.iter_mut() {
                let (x, c) = (!*byte).overflowing_add(u8::from(carry));
                *byte = x;
                carry = c;
            }
            // `-2^(8N-1)` negates to itself; anything else must now have the top bit set.
            if acc[N - 1] & 0x80 == 0 && acc.iter().any(|b| *b != 0) {
                return Err(bad(t, v, "out of range"));
            }
        }
    }
    Ok(acc)
}

fn float_of(t: &CType, v: &Value) -> Result<f64> {
    match v {
        Value::Float(f) => Ok(*f),
        Value::Int(i) => Ok(*i as f64),
        Value::UInt(u) => Ok(*u as f64),
        Value::Decimal(s) | Value::Text(s) => match s.trim() {
            "NaN" | "nan" => Ok(f64::NAN),
            "Infinity" | "inf" => Ok(f64::INFINITY),
            "-Infinity" | "-inf" => Ok(f64::NEG_INFINITY),
            other => other.parse().map_err(|_| bad(t, v, "not a number")),
        },
        _ => Err(bad(t, v, "not a number")),
    }
}

fn scalar_text(v: &Value) -> Option<String> {
    match v {
        Value::Text(s)
        | Value::Decimal(s)
        | Value::Date(s)
        | Value::Time(s)
        | Value::DateTime(s)
        | Value::Timestamp(s)
        | Value::Uuid(s)
        | Value::Json(s) => Some(s.clone()),
        Value::Int(_) | Value::UInt(_) | Value::Float(_) | Value::Bool(_) => Some(value_text(v)),
        Value::Ext(ExtValue { text, .. }) => Some(text.clone()),
        _ => None,
    }
}

/// Parse `YYYY-MM-DD` (optionally followed by a time part) into `(days, seconds_of_day, nanos)`.
pub(crate) fn parse_datetime(s: &str) -> Option<(i64, i64, u32)> {
    let s = s.trim();
    let (date, rest) = match s.find(['T', ' ']) {
        Some(i) => (&s[..i], s[i + 1..].trim()),
        None => (s, ""),
    };
    let (neg, date_body) = match date.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, date),
    };
    let mut it = date_body.split('-');
    let y: i64 = it.next()?.parse().ok()?;
    let m: u32 = it.next()?.parse().ok()?;
    let d: u32 = it.next()?.parse().ok()?;
    if it.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let days = days_from_civil(if neg { -y } else { y }, m, d);
    if rest.is_empty() {
        return Some((days, 0, 0));
    }
    // Strip a UTC designator / offset (`Z`, `+00:00`); other offsets are applied.
    let (time, offset_secs) = split_offset(rest)?;
    let (hms, frac) = match time.split_once('.') {
        Some((a, b)) => (a, b),
        None => (time, ""),
    };
    let mut parts = hms.split(':');
    let h: i64 = parts.next()?.parse().ok()?;
    let mi: i64 = parts.next().map_or(Some(0), |x| x.parse().ok())?;
    let sec: i64 = parts.next().map_or(Some(0), |x| x.parse().ok())?;
    if parts.next().is_some() || h > 24 || mi > 59 || sec > 60 {
        return None;
    }
    let mut nanos = 0u32;
    if !frac.is_empty() {
        if !frac.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let padded: String = frac.chars().chain(std::iter::repeat('0')).take(9).collect();
        nanos = padded.parse().ok()?;
    }
    let tod = h * 3600 + mi * 60 + sec - offset_secs;
    Some((days + tod.div_euclid(86_400), tod.rem_euclid(86_400), nanos))
}

fn split_offset(t: &str) -> Option<(&str, i64)> {
    if let Some(r) = t.strip_suffix('Z').or_else(|| t.strip_suffix('z')) {
        return Some((r.trim(), 0));
    }
    if let Some(i) = t.rfind(['+', '-']) {
        // A sign after the time digits: `+02:00` / `-0500`.
        if i > 0 && t[..i].contains(':') {
            let off = &t[i + 1..];
            let digits: String = off.chars().filter(char::is_ascii_digit).collect();
            if digits.len() == 4 || digits.len() == 2 {
                let hh: i64 = digits[..2].parse().ok()?;
                let mm: i64 = if digits.len() == 4 {
                    digits[2..].parse().ok()?
                } else {
                    0
                };
                let secs = hh * 3600 + mm * 60;
                return Some((
                    t[..i].trim(),
                    if t.as_bytes()[i] == b'-' { -secs } else { secs },
                ));
            }
        }
    }
    Some((t, 0))
}

fn decimal_to_scaled(t: &CType, v: &Value, scale: u8) -> Result<String> {
    let text = match v {
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Float(f) => format!("{f}"),
        other => scalar_text(other).ok_or_else(|| bad(t, v, "not a number"))?,
    };
    let text = text.trim();
    let (neg, body) = match text.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, text.trim_start_matches('+')),
    };
    let (int_part, frac) = body.split_once('.').unwrap_or((body, ""));
    if int_part.is_empty() && frac.is_empty()
        || !int_part.chars().all(|c| c.is_ascii_digit())
        || !frac.chars().all(|c| c.is_ascii_digit())
    {
        return Err(bad(t, v, "not a decimal number"));
    }
    let scale = usize::from(scale);
    if frac.len() > scale && frac[scale..].chars().any(|c| c != '0') {
        return Err(bad(t, v, "too many fractional digits"));
    }
    let mut frac: String = frac.chars().take(scale).collect();
    while frac.len() < scale {
        frac.push('0');
    }
    let digits = format!("{int_part}{frac}");
    let digits = digits.trim_start_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
    Ok(if neg && digits != "0" {
        format!("-{digits}")
    } else {
        digits.to_string()
    })
}

fn json_to_value(j: &serde_json::Value) -> Value {
    match j {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int(i)
            } else if let Some(u) = n.as_u64() {
                Value::UInt(u)
            } else {
                Value::Float(n.as_f64().unwrap_or(f64::NAN))
            }
        }
        serde_json::Value::String(s) => Value::Text(s.clone()),
        serde_json::Value::Array(a) => Value::Array(a.iter().map(json_to_value).collect()),
        serde_json::Value::Object(o) => Value::Document(
            o.iter()
                .map(|(k, v)| (k.clone(), json_to_value(v)))
                .collect(),
        ),
    }
}

/// Text typed by a user for a compound column is JSON; scalars pass through unchanged.
fn compound_input(t: &CType, v: &Value) -> Result<Value> {
    match v {
        Value::Text(s) | Value::Json(s) => {
            let j: serde_json::Value = serde_json::from_str(s)
                .map_err(|e| bad(t, v, &format!("expected JSON text: {e}")))?;
            Ok(json_to_value(&j))
        }
        other => Ok(other.clone()),
    }
}

/// Encode `v` as a `t` value (RowBinary).
pub(crate) fn encode_value(out: &mut Vec<u8>, t: &CType, v: &Value) -> Result<()> {
    match t {
        CType::Nullable(inner) => {
            if v.is_null() {
                out.push(1);
                if **inner == CType::Nothing {
                    return Ok(());
                }
                return Ok(());
            }
            out.push(0);
            return encode_value(out, inner, v);
        }
        CType::LowCardinality(inner) => return encode_value(out, inner, v),
        _ => {}
    }
    if v.is_null() {
        return Err(bad(t, v, "NULL in a non-nullable column"));
    }
    match t {
        CType::Int8 => out.push(parse_int::<i8>(t, v)? as u8),
        CType::Int16 => out.extend(parse_int::<i16>(t, v)?.to_le_bytes()),
        CType::Int32 => out.extend(parse_int::<i32>(t, v)?.to_le_bytes()),
        CType::Int64 => out.extend(parse_int::<i64>(t, v)?.to_le_bytes()),
        CType::UInt8 => out.push(parse_int::<u8>(t, v)?),
        CType::UInt16 => out.extend(parse_int::<u16>(t, v)?.to_le_bytes()),
        CType::UInt32 => out.extend(parse_int::<u32>(t, v)?.to_le_bytes()),
        CType::UInt64 => out.extend(parse_int::<u64>(t, v)?.to_le_bytes()),
        CType::Int128 => out.extend(int_text_to_le::<16>(t, v, &int_text(t, v)?, true)?),
        CType::UInt128 => out.extend(int_text_to_le::<16>(t, v, &int_text(t, v)?, false)?),
        CType::Int256 => out.extend(int_text_to_le::<32>(t, v, &int_text(t, v)?, true)?),
        CType::UInt256 => out.extend(int_text_to_le::<32>(t, v, &int_text(t, v)?, false)?),
        CType::Float32 => out.extend((float_of(t, v)? as f32).to_le_bytes()),
        CType::Float64 => out.extend(float_of(t, v)?.to_le_bytes()),
        CType::BFloat16 => {
            let bits = (float_of(t, v)? as f32).to_bits();
            out.extend(((bits >> 16) as u16).to_le_bytes());
        }
        CType::Bool => out.push(match v {
            Value::Bool(b) => u8::from(*b),
            Value::Int(i) => u8::from(*i != 0),
            Value::UInt(u) => u8::from(*u != 0),
            Value::Text(s) => match s.trim().to_ascii_lowercase().as_str() {
                "true" | "t" | "1" | "yes" => 1,
                "false" | "f" | "0" | "no" => 0,
                _ => return Err(bad(t, v, "not a boolean")),
            },
            _ => return Err(bad(t, v, "not a boolean")),
        }),
        CType::String => match v {
            Value::Bytes(b) => {
                put_varuint(out, b.len() as u64);
                out.extend(b);
            }
            Value::Array(_) | Value::Document(_) => {
                return Err(bad(t, v, "a structured value"));
            }
            other => {
                let s = scalar_text(other).ok_or_else(|| bad(t, v, "not text"))?;
                put_varuint(out, s.len() as u64);
                out.extend(s.as_bytes());
            }
        },
        CType::FixedString(n) => {
            let bytes: Vec<u8> = match v {
                Value::Bytes(b) => b.clone(),
                other => scalar_text(other)
                    .ok_or_else(|| bad(t, v, "not text"))?
                    .into_bytes(),
            };
            if bytes.len() > *n {
                return Err(bad(t, v, &format!("longer than {n} bytes")));
            }
            out.extend(&bytes);
            out.extend(std::iter::repeat_n(0u8, n - bytes.len()));
        }
        CType::Uuid => {
            let s = scalar_text(v).ok_or_else(|| bad(t, v, "not a UUID"))?;
            let hex: String = s.chars().filter(|c| *c != '-').collect();
            if hex.len() != 32 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(bad(t, v, "not a UUID"));
            }
            let mut b = [0u8; 16];
            for (i, byte) in b.iter_mut().enumerate() {
                *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16)
                    .map_err(|_| bad(t, v, "not a UUID"))?;
            }
            for i in 0..8 {
                out.push(b[7 - i]);
            }
            for i in 0..8 {
                out.push(b[15 - i]);
            }
        }
        CType::Date | CType::Date32 => {
            let s = scalar_text(v).ok_or_else(|| bad(t, v, "not a date"))?;
            let (days, _, _) = parse_datetime(&s).ok_or_else(|| bad(t, v, "not a date"))?;
            if *t == CType::Date {
                let d = u16::try_from(days).map_err(|_| bad(t, v, "date out of range"))?;
                out.extend(d.to_le_bytes());
            } else {
                let d = i32::try_from(days).map_err(|_| bad(t, v, "date out of range"))?;
                out.extend(d.to_le_bytes());
            }
        }
        CType::DateTime(_) => {
            let s = scalar_text(v).ok_or_else(|| bad(t, v, "not a timestamp"))?;
            let (days, tod, _) = parse_datetime(&s).ok_or_else(|| bad(t, v, "not a timestamp"))?;
            let secs = u32::try_from(days * 86_400 + tod)
                .map_err(|_| bad(t, v, "timestamp out of range"))?;
            out.extend(secs.to_le_bytes());
        }
        CType::DateTime64(p, _) => {
            let s = scalar_text(v).ok_or_else(|| bad(t, v, "not a timestamp"))?;
            let (days, tod, nanos) =
                parse_datetime(&s).ok_or_else(|| bad(t, v, "not a timestamp"))?;
            let p = u32::from(*p).min(9);
            let ticks = (days * 86_400 + tod)
                .checked_mul(10i64.pow(p))
                .and_then(|x| x.checked_add(i64::from(nanos) / 10i64.pow(9 - p)))
                .ok_or_else(|| bad(t, v, "timestamp out of range"))?;
            out.extend(ticks.to_le_bytes());
        }
        CType::Decimal(p, s) => {
            let scaled = decimal_to_scaled(t, v, *s)?;
            match *p {
                0..=9 => out.extend(
                    scaled
                        .parse::<i32>()
                        .map_err(|_| bad(t, v, "out of range"))?
                        .to_le_bytes(),
                ),
                10..=18 => out.extend(
                    scaled
                        .parse::<i64>()
                        .map_err(|_| bad(t, v, "out of range"))?
                        .to_le_bytes(),
                ),
                19..=38 => out.extend(
                    scaled
                        .parse::<i128>()
                        .map_err(|_| bad(t, v, "out of range"))?
                        .to_le_bytes(),
                ),
                _ => out.extend(int_text_to_le::<32>(t, v, &scaled, true)?),
            }
        }
        CType::Enum8(items) | CType::Enum16(items) => {
            let n = match v {
                Value::Int(i) => i16::try_from(*i).map_err(|_| bad(t, v, "out of range"))?,
                other => {
                    let s = scalar_text(other).ok_or_else(|| bad(t, v, "not an enum label"))?;
                    match items.iter().find(|(l, _)| *l == s) {
                        Some((_, n)) => *n,
                        None => s
                            .trim()
                            .parse::<i16>()
                            .ok()
                            .filter(|n| items.iter().any(|(_, x)| x == n))
                            .ok_or_else(|| bad(t, v, "unknown enum label"))?,
                    }
                }
            };
            if matches!(t, CType::Enum8(_)) {
                out.push(i8::try_from(n).map_err(|_| bad(t, v, "out of range"))? as u8);
            } else {
                out.extend(n.to_le_bytes());
            }
        }
        CType::IPv4 => {
            let s = scalar_text(v).ok_or_else(|| bad(t, v, "not an IPv4 address"))?;
            let ip: std::net::Ipv4Addr = s
                .trim()
                .parse()
                .map_err(|_| bad(t, v, "not an IPv4 address"))?;
            out.extend(u32::from(ip).to_le_bytes());
        }
        CType::IPv6 => {
            let s = scalar_text(v).ok_or_else(|| bad(t, v, "not an IPv6 address"))?;
            let ip: std::net::Ipv6Addr = s
                .trim()
                .parse()
                .map_err(|_| bad(t, v, "not an IPv6 address"))?;
            out.extend(ip.octets());
        }
        CType::Array(inner) => {
            let items = match compound_input(t, v)? {
                Value::Array(a) => a,
                _ => return Err(bad(t, v, "not an array")),
            };
            put_varuint(out, items.len() as u64);
            for item in &items {
                encode_value(out, inner, item)?;
            }
        }
        CType::Tuple(items) => {
            let vals: Vec<Value> = match compound_input(t, v)? {
                Value::Array(a) => a,
                Value::Document(d) => {
                    let mut vs = Vec::new();
                    for (i, (name, _)) in items.iter().enumerate() {
                        let found = match name {
                            Some(n) => d.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone()),
                            None => d.get(i).map(|(_, v)| v.clone()),
                        };
                        vs.push(found.ok_or_else(|| bad(t, v, "missing tuple element"))?);
                    }
                    vs
                }
                _ => return Err(bad(t, v, "not a tuple")),
            };
            if vals.len() != items.len() {
                return Err(bad(t, v, "wrong number of tuple elements"));
            }
            for ((_, it), val) in items.iter().zip(&vals) {
                encode_value(out, it, val)?;
            }
        }
        CType::Map(kt, vt) => {
            let pairs: Vec<(Value, Value)> = match compound_input(t, v)? {
                Value::Document(d) => d.into_iter().map(|(k, v)| (Value::Text(k), v)).collect(),
                Value::Array(a) => {
                    let mut ps = Vec::new();
                    for p in a {
                        match p {
                            Value::Array(kv) if kv.len() == 2 => {
                                let mut it = kv.into_iter();
                                ps.push((
                                    it.next().unwrap_or(Value::Null),
                                    it.next().unwrap_or(Value::Null),
                                ));
                            }
                            _ => return Err(bad(t, v, "expected [key, value] pairs")),
                        }
                    }
                    ps
                }
                _ => return Err(bad(t, v, "not a map")),
            };
            put_varuint(out, pairs.len() as u64);
            for (k, val) in &pairs {
                encode_value(out, kt, k)?;
                encode_value(out, vt, val)?;
            }
        }
        CType::Json => {
            let s = match v {
                Value::Json(s) | Value::Text(s) => s.clone(),
                Value::Array(_) | Value::Document(_) => {
                    return Err(bad(t, v, "pass JSON as text"));
                }
                other => scalar_text(other).ok_or_else(|| bad(t, v, "not JSON text"))?,
            };
            put_varuint(out, s.len() as u64);
            out.extend(s.as_bytes());
        }
        CType::Variant(_) | CType::Dynamic | CType::Nothing | CType::Unsupported(_) => {
            return Err(Error::Unsupported(format!(
                "writing {} values is not supported; use CAST in an INSERT ... SELECT",
                type_label(t)
            )));
        }
        CType::Nullable(_) | CType::LowCardinality(_) => unreachable!("handled above"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round(t: &str, v: Value) -> Value {
        let ty = CType::parse(t).unwrap();
        let mut buf = Vec::new();
        encode_value(&mut buf, &ty, &v).unwrap();
        let mut c = Cur::new(&buf);
        let out = decode_value(&mut c, &ty).unwrap();
        assert!(c.at_end(), "{t} left bytes");
        out
    }

    #[test]
    fn calendar_helpers() {
        assert_eq!(fmt_date(0), "1970-01-01");
        assert_eq!(fmt_date(19_723), "2024-01-01");
        assert_eq!(fmt_date(-1), "1969-12-31");
        assert_eq!(days_from_civil(2024, 2, 29), 19_782);
        assert_eq!(civil_from_days(days_from_civil(1900, 3, 1)), (1900, 3, 1));
        assert_eq!(fmt_timestamp(0, 0), "1970-01-01T00:00:00Z");
        assert_eq!(
            fmt_timestamp(1_700_000_000_123, 3),
            "2023-11-14T22:13:20.123Z"
        );
        assert_eq!(fmt_timestamp(-1, 3), "1969-12-31T23:59:59.999Z");
    }

    #[test]
    fn decimals_format() {
        assert_eq!(fmt_decimal(false, "12345", 4), "1.2345");
        assert_eq!(fmt_decimal(true, "5", 2), "-0.05");
        assert_eq!(fmt_decimal(false, "0", 3), "0.000");
        assert_eq!(fmt_decimal(true, "0", 3), "0.000");
        assert_eq!(fmt_decimal(false, "7", 0), "7");
    }

    #[test]
    fn big_integers_round_trip() {
        let max = "57896044618658097711785492504343953926634992332820282019728792003956564819967";
        assert_eq!(
            round("Int256", Value::Decimal(max.into())),
            Value::Decimal(max.into())
        );
        let min = format!(
            "-{}",
            "57896044618658097711785492504343953926634992332820282019728792003956564819968"
        );
        assert_eq!(
            round("Int256", Value::Decimal(min.clone())),
            Value::Decimal(min)
        );
        let umax = "115792089237316195423570985008687907853269984665640564039457584007913129639935";
        assert_eq!(
            round("UInt256", Value::Decimal(umax.into())),
            Value::Decimal(umax.into())
        );
        assert_eq!(
            round(
                "Int128",
                Value::Decimal("-170141183460469231731687303715884105728".into())
            ),
            Value::Decimal("-170141183460469231731687303715884105728".into())
        );
        assert_eq!(
            round(
                "UInt128",
                Value::Decimal("340282366920938463463374607431768211455".into())
            ),
            Value::Decimal("340282366920938463463374607431768211455".into())
        );
        assert_eq!(round("Int256", Value::Int(-1)), Value::Decimal("-1".into()));
        let ty = CType::parse("Int128").unwrap();
        assert!(
            encode_value(
                &mut Vec::new(),
                &ty,
                &Value::Decimal("170141183460469231731687303715884105728".into())
            )
            .is_err()
        );
    }

    #[test]
    fn scalars_round_trip() {
        assert_eq!(round("Int8", Value::Int(-5)), Value::Int(-5));
        assert_eq!(
            round("UInt64", Value::UInt(u64::MAX)),
            Value::UInt(u64::MAX)
        );
        assert_eq!(round("Float32", Value::Float(0.1)), Value::Float(0.1));
        assert_eq!(round("Bool", Value::Bool(true)), Value::Bool(true));
        assert_eq!(
            round("String", Value::Text("héllo".into())),
            Value::Text("héllo".into())
        );
        assert_eq!(
            round("String", Value::Bytes(vec![0xff, 0xfe])),
            Value::Bytes(vec![0xff, 0xfe])
        );
        assert_eq!(
            round("FixedString(4)", Value::Text("ab".into())),
            Value::Bytes(b"ab\0\0".to_vec())
        );
        assert_eq!(
            round("FixedString(2)", Value::Text("ab".into())),
            Value::Text("ab".into())
        );
        assert_eq!(
            round(
                "UUID",
                Value::Uuid("61f0c404-5cb3-11e7-907b-a6006ad3dba0".into())
            ),
            Value::Uuid("61f0c404-5cb3-11e7-907b-a6006ad3dba0".into())
        );
        assert_eq!(
            round("Date", Value::Date("2024-02-29".into())),
            Value::Date("2024-02-29".into())
        );
        assert_eq!(
            round("Date32", Value::Date("1900-01-01".into())),
            Value::Date("1900-01-01".into())
        );
        assert_eq!(
            round("DateTime", Value::Timestamp("2024-02-29T12:34:56Z".into())),
            Value::Timestamp("2024-02-29T12:34:56Z".into())
        );
        assert_eq!(
            round(
                "DateTime64(3,'UTC')",
                Value::Timestamp("2024-02-29 12:34:56.789".into())
            ),
            Value::Timestamp("2024-02-29T12:34:56.789Z".into())
        );
        assert_eq!(
            round("Decimal(18,4)", Value::Decimal("-12.5".into())),
            Value::Decimal("-12.5000".into())
        );
        assert_eq!(
            round(
                "Decimal(50,10)",
                Value::Decimal("123456789012345678901234567890.5".into())
            ),
            Value::Decimal("123456789012345678901234567890.5000000000".into())
        );
        assert_eq!(
            round("Enum8('a'=1,'b'=2)", Value::Text("b".into())),
            Value::Text("b".into())
        );
        assert_eq!(
            round("IPv4", Value::Text("10.0.0.1".into())),
            Value::Text("10.0.0.1".into())
        );
        assert_eq!(
            round("IPv6", Value::Text("::1".into())),
            Value::Text("::1".into())
        );
    }

    #[test]
    fn compounds_round_trip() {
        assert_eq!(round("Nullable(Int32)", Value::Null), Value::Null);
        assert_eq!(
            round(
                "Array(Nullable(String))",
                Value::Array(vec![Value::Text("a".into()), Value::Null])
            ),
            Value::Array(vec![Value::Text("a".into()), Value::Null])
        );
        assert_eq!(
            round(
                "Tuple(a UInt8, b String)",
                Value::Document(vec![
                    ("a".into(), Value::Int(1)),
                    ("b".into(), Value::Text("x".into()))
                ])
            ),
            Value::Document(vec![
                ("a".into(), Value::Int(1)),
                ("b".into(), Value::Text("x".into()))
            ])
        );
        assert_eq!(
            round(
                "Map(String, UInt8)",
                Value::Document(vec![("k".into(), Value::Int(7))])
            ),
            Value::Document(vec![("k".into(), Value::Int(7))])
        );
        assert_eq!(
            round(
                "Map(UInt8, String)",
                Value::Array(vec![Value::Array(vec![
                    Value::Int(1),
                    Value::Text("x".into())
                ])])
            ),
            Value::Array(vec![Value::Array(vec![
                Value::Int(1),
                Value::Text("x".into())
            ])])
        );
        assert_eq!(
            round("Array(Int32)", Value::Text("[1,2,3]".into())),
            Value::Array(vec![Value::Int(1), Value::Int(2), Value::Int(3)])
        );
        assert_eq!(
            round("JSON", Value::Json("{\"a\":1}".into())),
            Value::Json("{\"a\":1}".into())
        );
    }

    #[test]
    fn partial_input_asks_for_more() {
        let ty = CType::parse("String").unwrap();
        let mut buf = Vec::new();
        encode_value(&mut buf, &ty, &Value::Text("hello".into())).unwrap();
        for cut in 0..buf.len() {
            let mut c = Cur::new(&buf[..cut]);
            assert_eq!(decode_value(&mut c, &ty), Err(DecErr::NeedMore));
        }
    }

    #[test]
    fn header_decodes() {
        // 2 columns: a UInt8, b String
        let buf = [
            2, 1, b'a', 1, b'b', 5, b'U', b'I', b'n', b't', b'8', 6, b'S', b't', b'r', b'i', b'n',
            b'g', 1, 1, b'x',
        ];
        let (cols, used) = decode_header(&buf).unwrap();
        assert_eq!(
            cols,
            vec![("a".into(), "UInt8".into()), ("b".into(), "String".into())]
        );
        assert_eq!(used, buf.len() - 3);
        assert_eq!(decode_header(&buf[..4]), Err(DecErr::NeedMore));
    }

    #[test]
    fn invalid_values_are_rejected() {
        let enc = |t: &str, v: Value| encode_value(&mut Vec::new(), &CType::parse(t).unwrap(), &v);
        assert!(enc("UInt8", Value::Int(300)).is_err());
        assert!(enc("UInt8", Value::Int(-1)).is_err());
        assert!(enc("Int32", Value::Text("abc".into())).is_err());
        assert!(enc("Date", Value::Text("nope".into())).is_err());
        assert!(enc("Enum8('a'=1)", Value::Text("z".into())).is_err());
        assert!(enc("Int32", Value::Null).is_err());
        assert!(enc("FixedString(2)", Value::Text("abc".into())).is_err());
        assert!(enc("UUID", Value::Text("xyz".into())).is_err());
        assert!(enc("Decimal(9,2)", Value::Text("1.234".into())).is_err());
    }
}
