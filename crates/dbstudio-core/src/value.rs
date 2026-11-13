//! Cell values and their lossless JSON representation.
//!
//! A [`Value`] serializes as `{"t": <tag>, "v": <payload>}` (`Null` has no `v`):
//!
//! | variant | `v` |
//! |---|---|
//! | `bool` | JSON boolean |
//! | `int` / `uInt` | JSON number when within +-(2^53-1), otherwise a decimal **string** |
//! | `float` | JSON number, or `"NaN"`, `"Infinity"`, `"-Infinity"` as string |
//! | `decimal`, `text`, `date`, `time`, `dateTime`, `timestamp`, `uuid` | string |
//! | `bytes` | base64 string |
//! | `json` | string holding the raw JSON text exactly as the database returned it (key order and number digits preserved; the consumer parses it) |
//! | `array` | array of tagged values |
//! | `document` | array of `[name, value]` pairs (field order preserved) |
//! | `ext` | `{"type": <native type key>, "text": <canonical text>}` |

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Largest integer JavaScript represents exactly (2^53 - 1).
const JS_MAX_SAFE: i64 = 9_007_199_254_740_991;

/// A single database cell, document field or key-value payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", content = "v", rename_all = "camelCase")]
pub enum Value {
    /// SQL `NULL` / missing.
    Null,
    /// Boolean.
    Bool(bool),
    /// Signed 64-bit integer.
    #[serde(with = "int_repr")]
    Int(i64),
    /// Unsigned 64-bit integer.
    #[serde(with = "uint_repr")]
    UInt(u64),
    /// Double precision float.
    #[serde(with = "float_repr")]
    Float(f64),
    /// Exact numeric kept as its decimal text.
    Decimal(String),
    /// Text.
    Text(String),
    /// Binary data.
    #[serde(with = "bytes_repr")]
    Bytes(Vec<u8>),
    /// ISO-8601 date (`2024-05-31`).
    Date(String),
    /// ISO-8601 time (`13:45:00`).
    Time(String),
    /// ISO-8601 date and time without zone.
    DateTime(String),
    /// ISO-8601 instant with zone (`2024-05-31T13:45:00Z`).
    Timestamp(String),
    /// UUID in canonical text form.
    Uuid(String),
    /// Arbitrary JSON (JSON / JSONB columns).
    Json(String),
    /// Ordered list (arrays, Redis lists, ...).
    Array(Vec<Value>),
    /// Nested document; field order preserved. JSON: array of `[name, value]` pairs.
    Document(Vec<(String, Value)>),
    /// Database-specific scalar without a generic equivalent (ObjectId, BSON timestamp...).
    Ext(ExtValue),
}

/// Database-specific scalar. `ty` is a key of the closed list documented in the design
/// (`objectId`, `int64`, `bsonTimestamp`, `regex`, `binary`, `javascript`, `javascriptWithScope`,
/// `symbol`, `dbPointer`, `minKey`, `maxKey`, `undefined`, `dateMillis`, `truncatedText`,
/// `truncatedBytes`, `rowid`, `interval`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtValue {
    /// Native type key, e.g. `objectId`.
    #[serde(rename = "type")]
    pub ty: String,
    /// Canonical text of the value; round-trips exactly through the driver.
    pub text: String,
}

impl ExtValue {
    /// New extension value.
    pub fn new(ty: impl Into<String>, text: impl Into<String>) -> Self {
        ExtValue {
            ty: ty.into(),
            text: text.into(),
        }
    }
}

/// Coarse type tag of a [`Value`], also used to describe result columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ValueKind {
    /// Null only / unknown.
    Null,
    /// Boolean.
    Bool,
    /// Signed integer.
    Int,
    /// Unsigned integer.
    UInt,
    /// Float.
    Float,
    /// Exact decimal.
    Decimal,
    /// Text.
    Text,
    /// Binary.
    Bytes,
    /// Date.
    Date,
    /// Time.
    Time,
    /// Date and time.
    DateTime,
    /// Timestamp with zone.
    Timestamp,
    /// UUID.
    Uuid,
    /// JSON.
    Json,
    /// Array.
    Array,
    /// Document.
    Document,
    /// Database-specific scalar.
    Ext,
}

impl Value {
    /// The coarse kind of this value.
    pub fn kind(&self) -> ValueKind {
        match self {
            Value::Null => ValueKind::Null,
            Value::Bool(_) => ValueKind::Bool,
            Value::Int(_) => ValueKind::Int,
            Value::UInt(_) => ValueKind::UInt,
            Value::Float(_) => ValueKind::Float,
            Value::Decimal(_) => ValueKind::Decimal,
            Value::Text(_) => ValueKind::Text,
            Value::Bytes(_) => ValueKind::Bytes,
            Value::Date(_) => ValueKind::Date,
            Value::Time(_) => ValueKind::Time,
            Value::DateTime(_) => ValueKind::DateTime,
            Value::Timestamp(_) => ValueKind::Timestamp,
            Value::Uuid(_) => ValueKind::Uuid,
            Value::Json(_) => ValueKind::Json,
            Value::Array(_) => ValueKind::Array,
            Value::Document(_) => ValueKind::Document,
            Value::Ext(_) => ValueKind::Ext,
        }
    }

    /// Field `key` of a [`Value::Document`] (first match); `None` for other variants or a missing
    /// key.
    pub fn doc_get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Document(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// True for [`Value::Null`].
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }
}

/// Number when JS-safe, string otherwise; accepts both when reading.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum NumOrStr<N> {
    Num(N),
    Str(String),
}

mod int_repr {
    use super::*;

    pub fn serialize<S: Serializer>(v: &i64, s: S) -> Result<S::Ok, S::Error> {
        if (-JS_MAX_SAFE..=JS_MAX_SAFE).contains(v) {
            s.serialize_i64(*v)
        } else {
            s.serialize_str(&v.to_string())
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
        match NumOrStr::<i64>::deserialize(d)? {
            NumOrStr::Num(n) => Ok(n),
            NumOrStr::Str(s) => s.parse().map_err(serde::de::Error::custom),
        }
    }
}

mod uint_repr {
    use super::*;

    pub fn serialize<S: Serializer>(v: &u64, s: S) -> Result<S::Ok, S::Error> {
        if *v <= JS_MAX_SAFE as u64 {
            s.serialize_u64(*v)
        } else {
            s.serialize_str(&v.to_string())
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
        match NumOrStr::<u64>::deserialize(d)? {
            NumOrStr::Num(n) => Ok(n),
            NumOrStr::Str(s) => s.parse().map_err(serde::de::Error::custom),
        }
    }
}

mod float_repr {
    use super::*;

    pub fn serialize<S: Serializer>(v: &f64, s: S) -> Result<S::Ok, S::Error> {
        if v.is_finite() {
            s.serialize_f64(*v)
        } else if v.is_nan() {
            s.serialize_str("NaN")
        } else if v.is_sign_positive() {
            s.serialize_str("Infinity")
        } else {
            s.serialize_str("-Infinity")
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
        match NumOrStr::<f64>::deserialize(d)? {
            NumOrStr::Num(n) => Ok(n),
            NumOrStr::Str(s) => match s.as_str() {
                "NaN" => Ok(f64::NAN),
                "Infinity" => Ok(f64::INFINITY),
                "-Infinity" => Ok(f64::NEG_INFINITY),
                other => other.parse().map_err(serde::de::Error::custom),
            },
        }
    }
}

mod bytes_repr {
    use super::*;

    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(v))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(d)?;
        STANDARD.decode(text).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn roundtrip(v: Value) -> serde_json::Value {
        let json = serde_json::to_value(&v).unwrap();
        let back: Value = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(back, v, "round-trip of {json}");
        json
    }

    #[test]
    fn null_and_scalars() {
        assert_eq!(roundtrip(Value::Null), json!({"t": "null"}));
        assert_eq!(
            roundtrip(Value::Bool(true)),
            json!({"t": "bool", "v": true})
        );
        assert_eq!(
            roundtrip(Value::Text("hi".into())),
            json!({"t": "text", "v": "hi"})
        );
        assert_eq!(
            roundtrip(Value::Float(1.5)),
            json!({"t": "float", "v": 1.5})
        );
        assert_eq!(
            roundtrip(Value::DateTime("2024-05-31T13:45:00".into()))["t"],
            "dateTime"
        );
    }

    #[test]
    fn safe_ints_are_numbers() {
        assert_eq!(roundtrip(Value::Int(42)), json!({"t": "int", "v": 42}));
        assert_eq!(
            roundtrip(Value::Int(-JS_MAX_SAFE)),
            json!({"t": "int", "v": -JS_MAX_SAFE})
        );
        assert_eq!(roundtrip(Value::UInt(JS_MAX_SAFE as u64))["v"], JS_MAX_SAFE);
    }

    #[test]
    fn unsafe_ints_are_strings() {
        assert_eq!(
            roundtrip(Value::Int(9_007_199_254_740_993)),
            json!({"t": "int", "v": "9007199254740993"})
        );
        assert_eq!(roundtrip(Value::Int(i64::MIN))["v"], "-9223372036854775808");
        assert_eq!(roundtrip(Value::Int(i64::MAX))["v"], "9223372036854775807");
        assert_eq!(
            roundtrip(Value::UInt(u64::MAX)),
            json!({"t": "uInt", "v": "18446744073709551615"})
        );
    }

    #[test]
    fn decimal_and_bytes() {
        assert_eq!(
            roundtrip(Value::Decimal("12345678901234567890.123456789".into())),
            json!({"t": "decimal", "v": "12345678901234567890.123456789"})
        );
        assert_eq!(
            roundtrip(Value::Bytes(vec![0, 1, 2, 255])),
            json!({"t": "bytes", "v": "AAEC/w=="})
        );
        assert_eq!(
            roundtrip(Value::Bytes(vec![])),
            json!({"t": "bytes", "v": ""})
        );
    }

    #[test]
    fn non_finite_floats() {
        let json = serde_json::to_value(Value::Float(f64::INFINITY)).unwrap();
        assert_eq!(json, json!({"t": "float", "v": "Infinity"}));
        let back: Value = serde_json::from_value(json!({"t": "float", "v": "NaN"})).unwrap();
        assert!(matches!(back, Value::Float(f) if f.is_nan()));
    }

    #[test]
    fn nested_and_json() {
        let doc = vec![
            ("zeta".to_string(), Value::Int(i64::MAX)),
            (
                "tags".to_string(),
                Value::Array(vec![Value::Text("a".into()), Value::Null]),
            ),
        ];
        let json = roundtrip(Value::Document(doc.clone()));
        // Pairs, in insertion order (not sorted).
        assert_eq!(
            json["v"][0],
            json!(["zeta", {"t": "int", "v": "9223372036854775807"}])
        );
        assert_eq!(json["v"][1][0], "tags");
        let value = Value::Document(doc);
        assert_eq!(value.doc_get("zeta"), Some(&Value::Int(i64::MAX)));
        assert_eq!(value.doc_get("missing"), None);
        assert_eq!(Value::Null.doc_get("x"), None);
        let raw = r#"{"b":1,"a":2,"n":12345678901234567890123,"i":9007199254740993}"#;
        assert_eq!(
            roundtrip(Value::Json(raw.into())),
            json!({"t": "json", "v": raw})
        );
        roundtrip(Value::Json("null".into()));
    }

    #[test]
    fn ext_value_shape() {
        let v = Value::Ext(ExtValue::new("objectId", "65f0a1b2c3d4e5f6a7b8c9d0"));
        assert_eq!(
            roundtrip(v.clone()),
            json!({"t": "ext", "v": {"type": "objectId", "text": "65f0a1b2c3d4e5f6a7b8c9d0"}})
        );
        assert_eq!(v.kind(), ValueKind::Ext);
        assert_eq!(serde_json::to_value(ValueKind::Ext).unwrap(), "ext");
    }

    #[test]
    fn document_rejects_object_form() {
        assert!(
            serde_json::from_value::<Value>(json!({"t": "document", "v": {"a": {"t": "null"}}}))
                .is_err()
        );
    }

    #[test]
    fn rejects_garbage() {
        assert!(serde_json::from_value::<Value>(json!({"t": "int", "v": "abc"})).is_err());
        assert!(serde_json::from_value::<Value>(json!({"t": "bytes", "v": "***"})).is_err());
        assert!(serde_json::from_value::<Value>(json!({"t": "wat"})).is_err());
    }

    #[test]
    fn kind_matches() {
        assert_eq!(Value::Uuid("x".into()).kind(), ValueKind::Uuid);
        assert!(Value::Null.is_null());
    }
}
