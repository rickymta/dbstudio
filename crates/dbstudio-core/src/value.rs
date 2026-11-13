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
//! | `json` | any JSON value |
//! | `array` | array of tagged values |
//! | `document` | object of name to tagged value |

use std::collections::BTreeMap;

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
    Json(serde_json::Value),
    /// Ordered list (arrays, Redis lists, ...).
    Array(Vec<Value>),
    /// Nested document (MongoDB documents, maps, hashes).
    Document(BTreeMap<String, Value>),
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
        let mut doc = BTreeMap::new();
        doc.insert("id".to_string(), Value::Int(i64::MAX));
        doc.insert(
            "tags".to_string(),
            Value::Array(vec![Value::Text("a".into()), Value::Null]),
        );
        let json = roundtrip(Value::Document(doc));
        assert_eq!(
            json["v"]["id"],
            json!({"t": "int", "v": "9223372036854775807"})
        );
        roundtrip(Value::Json(json!({"a": [1, 2, {"b": null}]})));
        roundtrip(Value::Json(serde_json::Value::Null));
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
