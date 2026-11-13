//! Lossless conversion between BSON and the generic [`Value`], plus ordered Extended JSON.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use bson::spec::BinarySubtype;
use bson::{Binary, Bson, Document};
use dbstudio_core::{Error, ExtValue, Result, Value};

/// Smallest/largest millisecond value that still formats as a year 0001..=9999 timestamp.
const MIN_MILLIS: i64 = -62_135_596_800_000;
const MAX_MILLIS: i64 = 253_402_300_799_999;

fn ext(ty: &str, text: impl Into<String>) -> Value {
    Value::Ext(ExtValue::new(ty, text))
}

/// Format milliseconds since the epoch as `YYYY-MM-DDTHH:MM:SS.mmmZ`.
pub fn millis_to_iso(ms: i64) -> Option<String> {
    if !(MIN_MILLIS..=MAX_MILLIS).contains(&ms) {
        return None;
    }
    let dt = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)?;
    Some(dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
}

/// BSON -> [`Value`] following the design table (3.3).
pub fn bson_to_value(b: &Bson) -> Value {
    match b {
        Bson::Double(d) => Value::Float(*d),
        Bson::String(s) => Value::Text(s.clone()),
        Bson::Array(a) => Value::Array(a.iter().map(bson_to_value).collect()),
        Bson::Document(d) => doc_to_value(d),
        Bson::Boolean(v) => Value::Bool(*v),
        Bson::Null => Value::Null,
        Bson::RegularExpression(r) => ext("regex", format!("/{}/{}", r.pattern, r.options)),
        Bson::JavaScriptCode(c) => ext(
            "javascript",
            to_ejson(&Bson::JavaScriptCode(c.clone()), false, true),
        ),
        Bson::JavaScriptCodeWithScope(c) => ext(
            "javascriptWithScope",
            to_ejson(&Bson::JavaScriptCodeWithScope(c.clone()), false, true),
        ),
        Bson::Int32(i) => Value::Int(i64::from(*i)),
        Bson::Int64(i) => ext("int64", i.to_string()),
        Bson::Timestamp(t) => ext("bsonTimestamp", format!("{},{}", t.time, t.increment)),
        Bson::Binary(bin) => match bin.subtype {
            BinarySubtype::Generic => Value::Bytes(bin.bytes.clone()),
            BinarySubtype::Uuid if bin.bytes.len() == 16 => {
                let mut raw = [0u8; 16];
                raw.copy_from_slice(&bin.bytes);
                Value::Uuid(uuid::Uuid::from_bytes(raw).to_string())
            }
            other => ext(
                "binary",
                format!("{:02x}:{}", u8::from(other), B64.encode(&bin.bytes)),
            ),
        },
        Bson::ObjectId(o) => ext("objectId", o.to_hex()),
        Bson::DateTime(dt) => match millis_to_iso(dt.timestamp_millis()) {
            Some(s) => Value::Timestamp(s),
            None => ext("dateMillis", dt.timestamp_millis().to_string()),
        },
        Bson::Symbol(_) => ext("symbol", to_ejson(b, false, true)),
        Bson::Decimal128(d) => Value::Decimal(d.to_string()),
        Bson::Undefined => ext("undefined", ""),
        Bson::MaxKey => ext("maxKey", ""),
        Bson::MinKey => ext("minKey", ""),
        Bson::DbPointer(_) => ext("dbPointer", to_ejson(b, false, true)),
    }
}

/// Document -> [`Value::Document`] keeping field order.
pub fn doc_to_value(d: &Document) -> Value {
    Value::Document(
        d.iter()
            .map(|(k, v)| (k.clone(), bson_to_value(v)))
            .collect(),
    )
}

fn bad(msg: impl Into<String>) -> Error {
    Error::Query {
        message: msg.into(),
        position: None,
    }
}

/// [`Value`] -> BSON (inverse of [`bson_to_value`]).
pub fn value_to_bson(v: &Value) -> Result<Bson> {
    Ok(match v {
        Value::Null => Bson::Null,
        Value::Bool(b) => Bson::Boolean(*b),
        Value::Int(i) => match i32::try_from(*i) {
            Ok(i) => Bson::Int32(i),
            Err(_) => Bson::Int64(*i),
        },
        Value::UInt(u) => match i64::try_from(*u) {
            Ok(i) => value_to_bson(&Value::Int(i))?,
            Err(_) => Bson::Double(*u as f64),
        },
        Value::Float(f) => Bson::Double(*f),
        Value::Decimal(s) => Bson::Decimal128(
            s.parse::<bson::Decimal128>()
                .map_err(|e| bad(format!("invalid Decimal128 '{s}': {e}")))?,
        ),
        Value::Text(s) | Value::Date(s) | Value::Time(s) | Value::DateTime(s) => {
            Bson::String(s.clone())
        }
        Value::Bytes(b) => Bson::Binary(Binary {
            subtype: BinarySubtype::Generic,
            bytes: b.clone(),
        }),
        Value::Timestamp(s) => {
            let dt = bson::DateTime::parse_rfc3339_str(s)
                .map_err(|e| bad(format!("invalid timestamp '{s}': {e}")))?;
            Bson::DateTime(dt)
        }
        Value::Uuid(s) => {
            let u = uuid::Uuid::parse_str(s).map_err(|e| bad(format!("invalid uuid: {e}")))?;
            Bson::Binary(Binary {
                subtype: BinarySubtype::Uuid,
                bytes: u.as_bytes().to_vec(),
            })
        }
        Value::Json(text) => parse_ejson_value(text)?,
        Value::Array(a) => Bson::Array(a.iter().map(value_to_bson).collect::<Result<_>>()?),
        Value::Document(fields) => {
            let mut d = Document::new();
            for (k, v) in fields {
                d.insert(k.clone(), value_to_bson(v)?);
            }
            Bson::Document(d)
        }
        Value::Ext(e) => ext_to_bson(e)?,
    })
}

fn ext_to_bson(e: &ExtValue) -> Result<Bson> {
    let t = e.text.as_str();
    Ok(match e.ty.as_str() {
        "objectId" => Bson::ObjectId(
            bson::oid::ObjectId::parse_str(t).map_err(|x| bad(format!("invalid ObjectId: {x}")))?,
        ),
        "int64" => Bson::Int64(t.parse().map_err(|_| bad(format!("invalid Int64 '{t}'")))?),
        "bsonTimestamp" => {
            let (a, b) = t
                .split_once(',')
                .ok_or_else(|| bad("invalid timestamp".to_string()))?;
            Bson::Timestamp(bson::Timestamp {
                time: a.trim().parse().map_err(|_| bad("invalid timestamp"))?,
                increment: b.trim().parse().map_err(|_| bad("invalid timestamp"))?,
            })
        }
        "regex" => {
            let body = t.strip_prefix('/').ok_or_else(|| bad("invalid regex"))?;
            let at = body.rfind('/').ok_or_else(|| bad("invalid regex"))?;
            Bson::RegularExpression(bson::Regex {
                pattern: body[..at]
                    .to_string()
                    .try_into()
                    .map_err(|_| bad("regex"))?,
                options: body[at + 1..]
                    .to_string()
                    .try_into()
                    .map_err(|_| bad("regex"))?,
            })
        }
        "binary" => {
            let (st, data) = t.split_once(':').ok_or_else(|| bad("invalid binary"))?;
            let st = u8::from_str_radix(st, 16).map_err(|_| bad("invalid binary subtype"))?;
            Bson::Binary(Binary {
                subtype: BinarySubtype::from(st),
                bytes: B64.decode(data).map_err(|_| bad("invalid base64"))?,
            })
        }
        "dateMillis" => Bson::DateTime(bson::DateTime::from_millis(
            t.parse().map_err(|_| bad("invalid date millis"))?,
        )),
        "minKey" => Bson::MinKey,
        "maxKey" => Bson::MaxKey,
        "undefined" => Bson::Undefined,
        "javascript" | "javascriptWithScope" | "symbol" => parse_ejson_value(t)?,
        "dbPointer" => return Err(bad("DBPointer values cannot be stored")),
        other => return Err(bad(format!("value of type '{other}' cannot be stored"))),
    })
}

/// Keys that turn a JSON object into an Extended JSON scalar wrapper.
const WRAPPER_KEYS: [&str; 16] = [
    "$oid",
    "$date",
    "$numberInt",
    "$numberLong",
    "$numberDouble",
    "$numberDecimal",
    "$binary",
    "$uuid",
    "$timestamp",
    "$regularExpression",
    "$symbol",
    "$code",
    "$minKey",
    "$maxKey",
    "$undefined",
    "$dbPointer",
];

struct Parser<'a> {
    src: &'a str,
    chars: Vec<(usize, char)>,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn err(&self, msg: &str) -> Error {
        let at = self
            .chars
            .get(self.pos)
            .map(|c| c.0)
            .unwrap_or(self.src.len());
        let before = &self.src[..at];
        let line = before.matches('\n').count() + 1;
        let col = before
            .rsplit('\n')
            .next()
            .map(|l| l.chars().count())
            .unwrap_or(0)
            + 1;
        bad(format!("invalid JSON: {msg} at line {line} column {col}"))
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).map(|c| c.1)
    }

    fn ws(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.pos += 1;
        }
    }

    fn byte_at(&self, pos: usize) -> usize {
        self.chars.get(pos).map(|c| c.0).unwrap_or(self.src.len())
    }

    fn value(&mut self, depth: usize) -> Result<Bson> {
        if depth > 200 {
            return Err(self.err("nesting too deep"));
        }
        self.ws();
        match self.peek() {
            Some('{') => self.object(depth),
            Some('[') => {
                self.pos += 1;
                let mut items = Vec::new();
                self.ws();
                if self.peek() == Some(']') {
                    self.pos += 1;
                    return Ok(Bson::Array(items));
                }
                loop {
                    items.push(self.value(depth + 1)?);
                    self.ws();
                    match self.peek() {
                        Some(',') => self.pos += 1,
                        Some(']') => {
                            self.pos += 1;
                            return Ok(Bson::Array(items));
                        }
                        _ => return Err(self.err("expected ',' or ']'")),
                    }
                }
            }
            Some('"') => Ok(Bson::String(self.string()?)),
            Some('t') => self.word("true", Bson::Boolean(true)),
            Some('f') => self.word("false", Bson::Boolean(false)),
            Some('n') => self.word("null", Bson::Null),
            Some(c) if c == '-' || c.is_ascii_digit() => self.number(),
            _ => Err(self.err("unexpected token")),
        }
    }

    fn word(&mut self, w: &str, v: Bson) -> Result<Bson> {
        for expected in w.chars() {
            if self.peek() != Some(expected) {
                return Err(self.err("unexpected token"));
            }
            self.pos += 1;
        }
        Ok(v)
    }

    fn number(&mut self) -> Result<Bson> {
        let start = self.pos;
        while self
            .peek()
            .is_some_and(|c| c.is_ascii_digit() || matches!(c, '-' | '+' | '.' | 'e' | 'E'))
        {
            self.pos += 1;
        }
        let text = &self.src[self.byte_at(start)..self.byte_at(self.pos)];
        if text.contains(['.', 'e', 'E']) {
            return text
                .parse::<f64>()
                .map(Bson::Double)
                .map_err(|_| self.err("invalid number"));
        }
        match text.parse::<i64>() {
            // Plain integers keep the smallest integer type (Int32 when it fits).
            Ok(i) => Ok(i32::try_from(i).map(Bson::Int32).unwrap_or(Bson::Int64(i))),
            Err(_) => text
                .parse::<f64>()
                .map(Bson::Double)
                .map_err(|_| self.err("invalid number")),
        }
    }

    fn string(&mut self) -> Result<String> {
        // Delegate escapes to serde_json on the exact literal.
        let start = self.pos;
        self.pos += 1;
        loop {
            match self.peek() {
                None => return Err(self.err("unterminated string")),
                Some('\\') => self.pos += 2,
                Some('"') => {
                    self.pos += 1;
                    break;
                }
                Some(_) => self.pos += 1,
            }
        }
        let lit = &self.src[self.byte_at(start)..self.byte_at(self.pos)];
        serde_json::from_str::<String>(lit).map_err(|_| self.err("invalid string"))
    }

    fn object(&mut self, depth: usize) -> Result<Bson> {
        let start = self.pos;
        self.pos += 1;
        let mut d = Document::new();
        self.ws();
        if self.peek() == Some('}') {
            self.pos += 1;
            return Ok(Bson::Document(d));
        }
        loop {
            self.ws();
            if self.peek() != Some('"') {
                return Err(self.err("expected a string key"));
            }
            let k = self.string()?;
            self.ws();
            if self.peek() != Some(':') {
                return Err(self.err("expected ':'"));
            }
            self.pos += 1;
            let v = self.value(depth + 1)?;
            d.insert(k, v);
            self.ws();
            match self.peek() {
                Some(',') => self.pos += 1,
                Some('}') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(self.err("expected ',' or '}'")),
            }
        }
        let is_wrapper = d
            .keys()
            .next()
            .is_some_and(|k| WRAPPER_KEYS.contains(&k.as_str()))
            && d.keys().all(|k| k.starts_with('$'));
        if is_wrapper {
            // Scalar wrappers are small; let `bson` interpret exactly this slice.
            let lit = &self.src[self.byte_at(start)..self.byte_at(self.pos)];
            return serde_json::from_str::<Bson>(lit)
                .map_err(|e| bad(format!("invalid Extended JSON value: {e}")));
        }
        Ok(Bson::Document(d))
    }
}

/// Parse relaxed/canonical Extended JSON (any value). Field order is kept and plain integer
/// literals become Int32 when they fit (negative ones too), Int64 otherwise.
pub fn parse_ejson_value(text: &str) -> Result<Bson> {
    let mut p = Parser {
        src: text,
        chars: text.char_indices().collect(),
        pos: 0,
    };
    let v = p.value(0)?;
    p.ws();
    if p.pos < p.chars.len() {
        return Err(p.err("trailing characters"));
    }
    Ok(v)
}

/// Parse an Extended JSON object.
pub fn parse_ejson_doc(text: &str) -> Result<Document> {
    match parse_ejson_value(text)? {
        Bson::Document(d) => Ok(d),
        _ => Err(bad("expected a JSON object")),
    }
}

// -- Extended JSON writer (ordered) -----------------------------------------------------------

fn esc(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

struct W {
    out: String,
    pretty: bool,
    canonical: bool,
}

impl W {
    fn nl(&mut self, depth: usize) {
        if self.pretty {
            self.out.push('\n');
            for _ in 0..depth {
                self.out.push_str("  ");
            }
        }
    }

    fn wrapper(&mut self, key: &str, inner: &str) {
        self.out.push_str("{\"");
        self.out.push_str(key);
        self.out.push_str("\":");
        if self.pretty {
            self.out.push(' ');
        }
        self.out.push_str(inner);
        self.out.push('}');
    }

    fn quoted(s: &str) -> String {
        let mut o = String::new();
        esc(s, &mut o);
        o
    }

    fn float(&mut self, d: f64) {
        if d.is_nan() || d.is_infinite() {
            let s = if d.is_nan() {
                "NaN"
            } else if d > 0.0 {
                "Infinity"
            } else {
                "-Infinity"
            };
            self.wrapper("$numberDouble", &Self::quoted(s));
        } else if self.canonical {
            self.wrapper("$numberDouble", &Self::quoted(&fmt_double(d)));
        } else {
            self.out.push_str(&fmt_double(d));
        }
    }

    fn write(&mut self, b: &Bson, depth: usize) {
        match b {
            Bson::Double(d) => self.float(*d),
            Bson::String(s) => esc(s, &mut self.out),
            Bson::Array(a) => {
                if a.is_empty() {
                    self.out.push_str("[]");
                    return;
                }
                self.out.push('[');
                for (i, v) in a.iter().enumerate() {
                    if i > 0 {
                        self.out.push(',');
                    }
                    self.nl(depth + 1);
                    self.write(v, depth + 1);
                }
                self.nl(depth);
                self.out.push(']');
            }
            Bson::Document(d) => self.doc(d, depth),
            Bson::Boolean(v) => self.out.push_str(if *v { "true" } else { "false" }),
            Bson::Null => self.out.push_str("null"),
            Bson::RegularExpression(r) => {
                let mut o = String::from("{\"pattern\":");
                esc(&r.pattern.to_string(), &mut o);
                o.push_str(",\"options\":");
                esc(&r.options.to_string(), &mut o);
                o.push('}');
                self.wrapper("$regularExpression", &o);
            }
            Bson::JavaScriptCode(c) => self.wrapper("$code", &Self::quoted(c)),
            Bson::JavaScriptCodeWithScope(c) => {
                let mut sub = W {
                    out: String::new(),
                    pretty: false,
                    canonical: true,
                };
                sub.doc(&c.scope, 0);
                self.out.push_str(&format!(
                    "{{\"$code\":{},\"$scope\":{}}}",
                    Self::quoted(&c.code),
                    sub.out
                ));
            }
            Bson::Int32(i) => {
                if self.canonical {
                    self.wrapper("$numberInt", &Self::quoted(&i.to_string()));
                } else {
                    self.out.push_str(&i.to_string());
                }
            }
            Bson::Int64(i) => {
                if self.canonical {
                    self.wrapper("$numberLong", &Self::quoted(&i.to_string()));
                } else {
                    self.out.push_str(&i.to_string());
                }
            }
            Bson::Timestamp(t) => self.wrapper(
                "$timestamp",
                &format!("{{\"t\":{},\"i\":{}}}", t.time, t.increment),
            ),
            Bson::Binary(bin) => {
                let inner = format!(
                    "{{\"base64\":\"{}\",\"subType\":\"{:02x}\"}}",
                    B64.encode(&bin.bytes),
                    u8::from(bin.subtype)
                );
                self.wrapper("$binary", &inner);
            }
            Bson::ObjectId(o) => self.wrapper("$oid", &Self::quoted(&o.to_hex())),
            Bson::DateTime(dt) => {
                let ms = dt.timestamp_millis();
                match (self.canonical, millis_to_iso(ms)) {
                    (false, Some(s)) => self.wrapper("$date", &Self::quoted(&s)),
                    _ => self.wrapper("$date", &format!("{{\"$numberLong\":\"{ms}\"}}")),
                }
            }
            Bson::Symbol(s) => self.wrapper("$symbol", &Self::quoted(s)),
            Bson::Decimal128(d) => self.wrapper("$numberDecimal", &Self::quoted(&d.to_string())),
            Bson::Undefined => self.wrapper("$undefined", "true"),
            Bson::MaxKey => self.wrapper("$maxKey", "1"),
            Bson::MinKey => self.wrapper("$minKey", "1"),
            Bson::DbPointer(_) => {
                // Deprecated legacy type; only shown, never produced by the UI.
                self.wrapper("$dbPointer", &Self::quoted(&format!("{b:?}")));
            }
        }
    }

    fn doc(&mut self, d: &Document, depth: usize) {
        if d.is_empty() {
            self.out.push_str("{}");
            return;
        }
        self.out.push('{');
        for (i, (k, v)) in d.iter().enumerate() {
            if i > 0 {
                self.out.push(',');
            }
            self.nl(depth + 1);
            esc(k, &mut self.out);
            self.out.push(':');
            if self.pretty {
                self.out.push(' ');
            }
            self.write(v, depth + 1);
        }
        self.nl(depth);
        self.out.push('}');
    }
}

fn fmt_double(d: f64) -> String {
    if d.fract() == 0.0 && d.abs() < 1e15 {
        format!("{d:.1}")
    } else {
        format!("{d}")
    }
}

/// Ordered Extended JSON text (`canonical` = type-preserving numbers, else relaxed).
pub fn to_ejson(b: &Bson, pretty: bool, canonical: bool) -> String {
    let mut w = W {
        out: String::new(),
        pretty,
        canonical,
    };
    w.write(b, 0);
    w.out
}

/// Canonical Extended JSON of a document.
pub fn doc_to_ejson(d: &Document, pretty: bool, canonical: bool) -> String {
    to_ejson(&Bson::Document(d.clone()), pretty, canonical)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bson::{doc, oid::ObjectId};

    fn roundtrip(b: Bson) -> Bson {
        let v = bson_to_value(&b);
        // Through the JSON wire form of Value as well.
        let json = serde_json::to_string(&v).unwrap();
        let v: Value = serde_json::from_str(&json).unwrap();
        value_to_bson(&v).unwrap()
    }

    #[test]
    fn exact_types_round_trip() {
        let oid = ObjectId::parse_str("65f0a1b2c3d4e5f607182930").unwrap();
        let dec: bson::Decimal128 = "12345678901234567890.123456789".parse().unwrap();
        let d = doc! {
            "_id": oid, "dec": dec, "date": bson::DateTime::from_millis(1_700_000_000_123),
            "i64": 9_007_199_254_740_993i64, "i32": 7, "f": 1.5, "s": "x", "b": true, "n": Bson::Null,
            "bin": Binary { subtype: BinarySubtype::Generic, bytes: vec![1, 2, 3] },
            "bin5": Binary { subtype: BinarySubtype::Md5, bytes: vec![9; 16] },
            "uuid": Binary { subtype: BinarySubtype::Uuid, bytes: vec![7; 16] },
            "ts": Bson::Timestamp(bson::Timestamp { time: 5, increment: 9 }),
            "nested": { "a": [1, { "b": 2i64 }] },
            "min": Bson::MinKey, "max": Bson::MaxKey,
        };
        let back = roundtrip(Bson::Document(d.clone()));
        assert_eq!(back, Bson::Document(d));
    }

    #[test]
    fn mapping_follows_design() {
        let oid = ObjectId::parse_str("65f0a1b2c3d4e5f607182930").unwrap();
        assert_eq!(
            bson_to_value(&Bson::ObjectId(oid)),
            Value::Ext(ExtValue::new("objectId", "65f0a1b2c3d4e5f607182930"))
        );
        assert_eq!(bson_to_value(&Bson::Int32(3)), Value::Int(3));
        assert_eq!(
            bson_to_value(&Bson::Int64(3)),
            Value::Ext(ExtValue::new("int64", "3"))
        );
        assert_eq!(
            bson_to_value(&Bson::DateTime(bson::DateTime::from_millis(0))),
            Value::Timestamp("1970-01-01T00:00:00.000Z".into())
        );
        // Out of range dates stay exact.
        let far = bson::DateTime::from_millis(MAX_MILLIS + 1);
        let v = bson_to_value(&Bson::DateTime(far));
        assert_eq!(
            v,
            Value::Ext(ExtValue::new("dateMillis", (MAX_MILLIS + 1).to_string()))
        );
        assert_eq!(value_to_bson(&v).unwrap(), Bson::DateTime(far));
    }

    #[test]
    fn ejson_is_ordered_and_parses_back() {
        let d = doc! { "z": 1, "a": { "$gt": 2i64 }, "o": ObjectId::parse_str("65f0a1b2c3d4e5f607182930").unwrap(), "d": bson::DateTime::from_millis(1_700_000_000_123) };
        let canon = doc_to_ejson(&d, false, true);
        assert!(
            canon.starts_with("{\"z\":{\"$numberInt\":\"1\"}"),
            "{canon}"
        );
        assert!(canon.contains("\"$oid\":\"65f0a1b2c3d4e5f607182930\""));
        assert_eq!(parse_ejson_doc(&canon).unwrap(), d);
        let relaxed = doc_to_ejson(&d, true, false);
        assert!(
            relaxed.contains("\"$date\": \"2023-11-14T22:13:20.123Z\""),
            "{relaxed}"
        );
        assert_eq!(
            parse_ejson_doc(&relaxed).unwrap().keys().next().unwrap(),
            "z"
        );
        let keys: Vec<_> = parse_ejson_doc(&relaxed).unwrap().keys().cloned().collect();
        assert_eq!(keys, ["z", "a", "o", "d"]);
    }

    #[test]
    fn plain_integers_keep_their_type_and_order() {
        let d = parse_ejson_doc(r#"{"z": 5, "b": -5, "c": {"$numberLong": "-5"}, "d": 3000000000, "e": -1.5, "n": [1, -2]}"#).unwrap();
        assert_eq!(d.get("z"), Some(&Bson::Int32(5)));
        assert_eq!(
            d.get("b"),
            Some(&Bson::Int32(-5)),
            "negative Int32 stays Int32"
        );
        assert_eq!(d.get("c"), Some(&Bson::Int64(-5)));
        assert_eq!(d.get("d"), Some(&Bson::Int64(3_000_000_000)));
        assert_eq!(d.get("e"), Some(&Bson::Double(-1.5)));
        assert_eq!(d.keys().next().unwrap(), "z");
        // The frontend form of negative Int32 / Int64.
        let d =
            parse_ejson_doc(r#"{"a": 5, "b": {"$numberInt": "-5"}, "c": {"$numberLong": "-5"}}"#)
                .unwrap();
        assert_eq!(d.get("b"), Some(&Bson::Int32(-5)));
        // Query operators are not wrappers.
        let d = parse_ejson_doc(r#"{"age": {"$gt": -3, "$lt": 4}}"#).unwrap();
        assert_eq!(
            d.get_document("age").unwrap().get("$gt"),
            Some(&Bson::Int32(-3))
        );
        assert!(parse_ejson_value("[1, 2").is_err());
        assert!(parse_ejson_value("{\"a\": 1} x").is_err());
        let v =
            parse_ejson_value(r#"{"s": "q\"\u00e9\n", "o": {"$oid": "65f0a1b2c3d4e5f607182930"}}"#)
                .unwrap();
        assert_eq!(v.as_document().unwrap().get_str("s").unwrap(), "q\"é\n");
    }

    #[test]
    fn code_symbol_text_is_canonical_ejson() {
        let js = Bson::JavaScriptCode("return 1".into());
        let v = bson_to_value(&js);
        assert_eq!(
            v,
            Value::Ext(ExtValue::new("javascript", "{\"$code\":\"return 1\"}"))
        );
        assert_eq!(value_to_bson(&v).unwrap(), js);
        let sym = Bson::Symbol("s".into());
        assert_eq!(value_to_bson(&bson_to_value(&sym)).unwrap(), sym);
        let scoped = Bson::JavaScriptCodeWithScope(bson::JavaScriptCodeWithScope {
            code: "x".into(),
            scope: doc! {"a": 1},
        });
        assert_eq!(value_to_bson(&bson_to_value(&scoped)).unwrap(), scoped);
        let e = value_to_bson(&Value::Ext(ExtValue::new("dbPointer", "{}"))).unwrap_err();
        assert_eq!(e.kind(), "query");
    }

    #[test]
    fn invalid_ejson_is_a_query_error() {
        assert_eq!(parse_ejson_doc("{oops").unwrap_err().kind(), "query");
        assert_eq!(parse_ejson_doc("[1]").unwrap_err().kind(), "query");
    }
}
