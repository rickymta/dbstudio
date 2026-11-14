//! ClickHouse column types: parsing of the type names found in the `RowBinaryWithNamesAndTypes`
//! header (and in `system.columns`) and the mapping onto [`ValueKind`].

use dbstudio_core::{Error, Result, ValueKind};

/// A parsed ClickHouse data type.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CType {
    Int8,
    Int16,
    Int32,
    Int64,
    Int128,
    Int256,
    UInt8,
    UInt16,
    UInt32,
    UInt64,
    UInt128,
    UInt256,
    Float32,
    Float64,
    BFloat16,
    Bool,
    String,
    FixedString(usize),
    Uuid,
    Date,
    Date32,
    DateTime(Option<String>),
    DateTime64(u8, Option<String>),
    /// `Decimal(P, S)`: precision and scale.
    Decimal(u8, u8),
    Enum8(Vec<(String, i16)>),
    Enum16(Vec<(String, i16)>),
    IPv4,
    IPv6,
    Nullable(Box<CType>),
    LowCardinality(Box<CType>),
    Array(Box<CType>),
    Tuple(Vec<(Option<String>, CType)>),
    Map(Box<CType>, Box<CType>),
    /// `JSON` read with `output_format_binary_write_json_as_string = 1`.
    Json,
    Variant(Vec<CType>),
    Dynamic,
    /// `Point`, `Ring`, ... are plain aliases of tuples / arrays.
    Nothing,
    /// A type this driver cannot decode from RowBinary (`AggregateFunction`, `Object`, ...).
    Unsupported(String),
}

impl CType {
    /// Parse a type name as printed by ClickHouse (`Array(Nullable(String))`, ...).
    pub(crate) fn parse(name: &str) -> Result<CType> {
        let mut p = Parser {
            s: name.as_bytes(),
            i: 0,
        };
        let t = p.ty()?;
        p.ws();
        if p.i != p.s.len() {
            return Err(Error::Other(format!(
                "cannot parse ClickHouse type `{name}` (trailing text)"
            )));
        }
        Ok(t)
    }

    /// Parse, mapping unknown names to [`CType::Unsupported`] instead of failing.
    pub(crate) fn parse_lenient(name: &str) -> CType {
        CType::parse(name).unwrap_or_else(|_| CType::Unsupported(name.to_string()))
    }

    /// Whether the RowBinary decoder understands this type completely.
    pub(crate) fn decodable(&self) -> bool {
        match self {
            CType::Unsupported(_) => false,
            CType::Nullable(t) | CType::LowCardinality(t) | CType::Array(t) => t.decodable(),
            CType::Tuple(ts) => ts.iter().all(|(_, t)| t.decodable()),
            CType::Map(k, v) => k.decodable() && v.decodable(),
            CType::Variant(ts) => ts.iter().all(CType::decodable),
            _ => true,
        }
    }

    pub(crate) fn is_nullable(&self) -> bool {
        match self {
            CType::Nullable(_) | CType::Dynamic | CType::Nothing => true,
            CType::LowCardinality(t) => t.is_nullable(),
            _ => false,
        }
    }

    /// Strip `Nullable` / `LowCardinality` wrappers.
    pub(crate) fn inner(&self) -> &CType {
        match self {
            CType::Nullable(t) | CType::LowCardinality(t) => t.inner(),
            t => t,
        }
    }

    /// The [`ValueKind`] values of this type decode to.
    pub(crate) fn value_kind(&self) -> ValueKind {
        match self {
            CType::Int8 | CType::Int16 | CType::Int32 | CType::Int64 => ValueKind::Int,
            CType::UInt8 | CType::UInt16 | CType::UInt32 => ValueKind::Int,
            CType::UInt64 => ValueKind::UInt,
            CType::Int128 | CType::Int256 | CType::UInt128 | CType::UInt256 => ValueKind::Decimal,
            CType::Decimal(..) => ValueKind::Decimal,
            CType::Float32 | CType::Float64 | CType::BFloat16 => ValueKind::Float,
            CType::Bool => ValueKind::Bool,
            CType::String | CType::FixedString(_) => ValueKind::Text,
            CType::Uuid => ValueKind::Uuid,
            CType::Date | CType::Date32 => ValueKind::Date,
            CType::DateTime(_) | CType::DateTime64(..) => ValueKind::Timestamp,
            CType::Enum8(_) | CType::Enum16(_) | CType::IPv4 | CType::IPv6 => ValueKind::Text,
            CType::Nullable(t) | CType::LowCardinality(t) => t.value_kind(),
            CType::Array(_) => ValueKind::Array,
            CType::Tuple(ts) => {
                if !ts.is_empty() && ts.iter().all(|(n, _)| n.is_some()) {
                    ValueKind::Document
                } else {
                    ValueKind::Array
                }
            }
            CType::Map(k, _) => {
                if matches!(k.inner(), CType::String | CType::FixedString(_)) {
                    ValueKind::Document
                } else {
                    ValueKind::Array
                }
            }
            CType::Json => ValueKind::Json,
            CType::Variant(_) | CType::Dynamic => ValueKind::Text,
            CType::Nothing => ValueKind::Null,
            CType::Unsupported(_) => ValueKind::Text,
        }
    }

    #[allow(dead_code)]
    pub(crate) fn is_numeric(&self) -> bool {
        matches!(
            self.inner(),
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
                | CType::UInt256
                | CType::Float32
                | CType::Float64
                | CType::BFloat16
                | CType::Decimal(..)
        )
    }
}

/// `ValueKind` of a type name (lenient: unknown types are text).
pub(crate) fn kind_of_name(name: &str) -> ValueKind {
    CType::parse_lenient(name).value_kind()
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while self.s.get(self.i).is_some_and(|c| c.is_ascii_whitespace()) {
            self.i += 1;
        }
    }

    fn eat(&mut self, c: u8) -> bool {
        self.ws();
        if self.s.get(self.i) == Some(&c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, c: u8) -> Result<()> {
        if self.eat(c) {
            Ok(())
        } else {
            Err(self.err(&format!("expected `{}`", c as char)))
        }
    }

    fn err(&self, msg: &str) -> Error {
        Error::Other(format!(
            "cannot parse ClickHouse type `{}`: {msg} at {}",
            String::from_utf8_lossy(self.s),
            self.i
        ))
    }

    fn ident(&mut self) -> Result<String> {
        self.ws();
        let start = self.i;
        while self
            .s
            .get(self.i)
            .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'.')
        {
            self.i += 1;
        }
        if start == self.i {
            return Err(self.err("expected a name"));
        }
        Ok(String::from_utf8_lossy(&self.s[start..self.i]).into_owned())
    }

    /// Quoted string `'..'` (backslash escapes and doubled quotes) or backtick/double quoted name.
    fn quoted(&mut self) -> Result<String> {
        self.ws();
        let q = *self
            .s
            .get(self.i)
            .ok_or_else(|| self.err("expected a quote"))?;
        if !matches!(q, b'\'' | b'`' | b'"') {
            return Err(self.err("expected a quote"));
        }
        self.i += 1;
        let mut out = Vec::new();
        loop {
            let c = *self
                .s
                .get(self.i)
                .ok_or_else(|| self.err("unterminated string"))?;
            self.i += 1;
            if c == b'\\' {
                let n = *self
                    .s
                    .get(self.i)
                    .ok_or_else(|| self.err("unterminated string"))?;
                self.i += 1;
                out.push(match n {
                    b'n' => b'\n',
                    b't' => b'\t',
                    b'r' => b'\r',
                    b'0' => 0,
                    other => other,
                });
            } else if c == q {
                if self.s.get(self.i) == Some(&q) {
                    out.push(q);
                    self.i += 1;
                } else {
                    break;
                }
            } else {
                out.push(c);
            }
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    fn number(&mut self) -> Result<i64> {
        self.ws();
        let start = self.i;
        if self.s.get(self.i) == Some(&b'-') {
            self.i += 1;
        }
        while self.s.get(self.i).is_some_and(u8::is_ascii_digit) {
            self.i += 1;
        }
        std::str::from_utf8(&self.s[start..self.i])
            .ok()
            .and_then(|t| t.parse().ok())
            .ok_or_else(|| self.err("expected a number"))
    }

    /// Skip a balanced `( ... )` argument list (the opening parenthesis is already consumed).
    fn skip_args(&mut self) -> Result<()> {
        let mut depth = 1;
        while depth > 0 {
            let c = *self
                .s
                .get(self.i)
                .ok_or_else(|| self.err("unbalanced parentheses"))?;
            match c {
                b'(' => depth += 1,
                b')' => depth -= 1,
                b'\'' | b'`' | b'"' => {
                    self.quoted()?;
                    continue;
                }
                _ => {}
            }
            self.i += 1;
        }
        Ok(())
    }

    fn ty(&mut self) -> Result<CType> {
        let name = self.ident()?;
        let has_args = {
            self.ws();
            self.s.get(self.i) == Some(&b'(')
        };
        Ok(match name.as_str() {
            "Int8" => CType::Int8,
            "Int16" => CType::Int16,
            "Int32" => CType::Int32,
            "Int64" => CType::Int64,
            "Int128" => CType::Int128,
            "Int256" => CType::Int256,
            "UInt8" => CType::UInt8,
            "UInt16" => CType::UInt16,
            "UInt32" => CType::UInt32,
            "UInt64" => CType::UInt64,
            "UInt128" => CType::UInt128,
            "UInt256" => CType::UInt256,
            "Float32" => CType::Float32,
            "Float64" => CType::Float64,
            "BFloat16" => CType::BFloat16,
            "Bool" => CType::Bool,
            "String" => CType::String,
            "UUID" => CType::Uuid,
            "Date" => CType::Date,
            "Date32" => CType::Date32,
            "IPv4" => CType::IPv4,
            "IPv6" => CType::IPv6,
            "Nothing" => CType::Nothing,
            "Dynamic" => {
                if has_args {
                    self.expect(b'(')?;
                    self.skip_args()?;
                }
                CType::Dynamic
            }
            "JSON" => {
                if has_args {
                    self.expect(b'(')?;
                    self.skip_args()?;
                }
                CType::Json
            }
            "Point" => CType::Tuple(vec![(None, CType::Float64), (None, CType::Float64)]),
            "Ring" | "LineString" => CType::Array(Box::new(CType::parse("Point")?)),
            "Polygon" | "MultiLineString" => CType::Array(Box::new(CType::parse("Ring")?)),
            "MultiPolygon" => CType::Array(Box::new(CType::parse("Polygon")?)),
            "FixedString" => {
                self.expect(b'(')?;
                let n = self.number()?;
                self.expect(b')')?;
                CType::FixedString(usize::try_from(n).map_err(|_| self.err("bad length"))?)
            }
            "DateTime" => {
                let mut tz = None;
                if has_args {
                    self.expect(b'(')?;
                    tz = Some(self.quoted()?);
                    self.expect(b')')?;
                }
                CType::DateTime(tz)
            }
            "DateTime64" => {
                self.expect(b'(')?;
                let p = self.number()?;
                let mut tz = None;
                if self.eat(b',') {
                    tz = Some(self.quoted()?);
                }
                self.expect(b')')?;
                CType::DateTime64(u8::try_from(p).map_err(|_| self.err("bad precision"))?, tz)
            }
            "Decimal" => {
                self.expect(b'(')?;
                let p = self.number()?;
                let mut s = 0;
                if self.eat(b',') {
                    s = self.number()?;
                }
                self.expect(b')')?;
                CType::Decimal(
                    u8::try_from(p).map_err(|_| self.err("bad precision"))?,
                    u8::try_from(s).map_err(|_| self.err("bad scale"))?,
                )
            }
            "Decimal32" | "Decimal64" | "Decimal128" | "Decimal256" => {
                let p = match name.as_str() {
                    "Decimal32" => 9,
                    "Decimal64" => 18,
                    "Decimal128" => 38,
                    _ => 76,
                };
                self.expect(b'(')?;
                let s = self.number()?;
                self.expect(b')')?;
                CType::Decimal(p, u8::try_from(s).map_err(|_| self.err("bad scale"))?)
            }
            "Enum8" | "Enum16" | "Enum" => {
                self.expect(b'(')?;
                let mut items = Vec::new();
                loop {
                    let label = self.quoted()?;
                    self.expect(b'=')?;
                    let v = self.number()?;
                    items.push((
                        label,
                        i16::try_from(v).map_err(|_| self.err("bad enum value"))?,
                    ));
                    if !self.eat(b',') {
                        break;
                    }
                }
                self.expect(b')')?;
                if name == "Enum16" {
                    CType::Enum16(items)
                } else {
                    CType::Enum8(items)
                }
            }
            "Nullable" | "LowCardinality" | "Array" => {
                self.expect(b'(')?;
                let inner = Box::new(self.ty()?);
                self.expect(b')')?;
                match name.as_str() {
                    "Nullable" => CType::Nullable(inner),
                    "LowCardinality" => CType::LowCardinality(inner),
                    _ => CType::Array(inner),
                }
            }
            "SimpleAggregateFunction" => {
                self.expect(b'(')?;
                // `func, [params...,] type`: the type is the last argument.
                let _ = self.ident()?;
                let mut last = None;
                while self.eat(b',') {
                    self.ws();
                    let save = self.i;
                    match self.ty() {
                        Ok(t) => last = Some(t),
                        Err(_) => {
                            self.i = save;
                            // A numeric / string parameter: skip it.
                            if self.number().is_err() {
                                self.quoted()?;
                            }
                        }
                    }
                }
                self.expect(b')')?;
                last.ok_or_else(|| self.err("missing type"))?
            }
            "Map" => {
                self.expect(b'(')?;
                let k = Box::new(self.ty()?);
                self.expect(b',')?;
                let v = Box::new(self.ty()?);
                self.expect(b')')?;
                CType::Map(k, v)
            }
            "Variant" => {
                self.expect(b'(')?;
                let mut items = vec![self.ty()?];
                while self.eat(b',') {
                    items.push(self.ty()?);
                }
                self.expect(b')')?;
                CType::Variant(items)
            }
            "Tuple" | "Nested" => {
                self.expect(b'(')?;
                let mut items = Vec::new();
                if !self.eat(b')') {
                    loop {
                        self.ws();
                        let save = self.i;
                        // `name Type` or just `Type`.
                        let mut label = None;
                        if matches!(self.s.get(self.i), Some(b'`' | b'"')) {
                            label = Some(self.quoted()?);
                        } else if let Ok(id) = self.ident() {
                            self.ws();
                            let next = self.s.get(self.i).copied();
                            if next.is_some_and(|c| c.is_ascii_alphabetic() || c == b'`') {
                                label = Some(id);
                            } else {
                                self.i = save;
                            }
                        }
                        items.push((label, self.ty()?));
                        if !self.eat(b',') {
                            break;
                        }
                    }
                    self.expect(b')')?;
                }
                if name == "Nested" {
                    CType::Array(Box::new(CType::Tuple(items)))
                } else {
                    CType::Tuple(items)
                }
            }
            other => {
                let mut text = other.to_string();
                if has_args {
                    let start = self.i;
                    self.expect(b'(')?;
                    self.skip_args()?;
                    text.push_str(&String::from_utf8_lossy(&self.s[start..self.i]));
                }
                CType::Unsupported(text)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_scalars_and_wrappers() {
        assert_eq!(CType::parse("UInt64").unwrap(), CType::UInt64);
        assert_eq!(
            CType::parse("LowCardinality(Nullable(String))").unwrap(),
            CType::LowCardinality(Box::new(CType::Nullable(Box::new(CType::String))))
        );
        assert_eq!(
            CType::parse("DateTime64(3, 'Europe/Paris')").unwrap(),
            CType::DateTime64(3, Some("Europe/Paris".into()))
        );
        assert_eq!(
            CType::parse("DateTime('UTC')").unwrap(),
            CType::DateTime(Some("UTC".into()))
        );
        assert_eq!(
            CType::parse("Decimal(18, 4)").unwrap(),
            CType::Decimal(18, 4)
        );
        assert_eq!(CType::parse("Decimal32(2)").unwrap(), CType::Decimal(9, 2));
        assert_eq!(
            CType::parse("FixedString(16)").unwrap(),
            CType::FixedString(16)
        );
    }

    #[test]
    fn parses_containers() {
        let t = CType::parse("Tuple(a UInt8, b Array(String))").unwrap();
        assert_eq!(
            t,
            CType::Tuple(vec![
                (Some("a".into()), CType::UInt8),
                (Some("b".into()), CType::Array(Box::new(CType::String)))
            ])
        );
        assert_eq!(t.value_kind(), ValueKind::Document);
        let t = CType::parse("Tuple(UInt8, String)").unwrap();
        assert_eq!(t.value_kind(), ValueKind::Array);
        let m = CType::parse("Map(String, Array(UInt8))").unwrap();
        assert_eq!(m.value_kind(), ValueKind::Document);
        let m = CType::parse("Map(UInt8, String)").unwrap();
        assert_eq!(m.value_kind(), ValueKind::Array);
        assert!(
            matches!(CType::parse("Variant(String, UInt8)").unwrap(), CType::Variant(v) if v.len() == 2)
        );
    }

    #[test]
    fn parses_enums_with_odd_labels() {
        let t = CType::parse("Enum8('a b' = 1, 'it''s' = -2, 'x\\'y' = 3)").unwrap();
        assert_eq!(
            t,
            CType::Enum8(vec![
                ("a b".into(), 1),
                ("it's".into(), -2),
                ("x'y".into(), 3)
            ])
        );
    }

    #[test]
    fn geo_types_expand() {
        assert!(matches!(CType::parse("Point").unwrap(), CType::Tuple(v) if v.len() == 2));
        assert!(matches!(
            CType::parse("MultiPolygon").unwrap(),
            CType::Array(_)
        ));
    }

    #[test]
    fn unsupported_types_are_flagged() {
        let t = CType::parse("AggregateFunction(sum, UInt64)").unwrap();
        assert!(!t.decodable());
        assert!(
            !CType::parse("Array(AggregateFunction(uniq, String))")
                .unwrap()
                .decodable()
        );
        assert!(CType::parse("SimpleAggregateFunction(sum, UInt64)").unwrap() == CType::UInt64);
        assert!(CType::parse("Array(Nullable(Int8))").unwrap().decodable());
    }

    #[test]
    fn rejects_garbage() {
        assert!(CType::parse("Array(").is_err());
        assert!(CType::parse("").is_err());
        assert_eq!(
            CType::parse_lenient("Array("),
            CType::Unsupported("Array(".into())
        );
    }
}
