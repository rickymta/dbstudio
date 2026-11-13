//! Key-value (Redis) reading types and the binary key-name encoding.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};

use crate::{Error, ObjectPath, Result, Value};

/// Marker that starts the `name` of a `Key` / `KeyPrefix` path segment holding non-UTF-8 bytes.
pub const BINARY_KEY_PREFIX: &str = "\u{0}b64:";

/// Name of a `Key` path segment for a raw key: the key itself when it is valid UTF-8 and does
/// not start with [`BINARY_KEY_PREFIX`], otherwise `"\0b64:" + base64(bytes)`.
pub fn encode_key_name(key: &[u8]) -> String {
    match std::str::from_utf8(key) {
        Ok(text) if !text.starts_with(BINARY_KEY_PREFIX) => text.to_string(),
        _ => format!("{BINARY_KEY_PREFIX}{}", STANDARD.encode(key)),
    }
}

/// Inverse of [`encode_key_name`]: raw key bytes of a segment name.
pub fn decode_key_name(name: &str) -> Result<Vec<u8>> {
    match name.strip_prefix(BINARY_KEY_PREFIX) {
        Some(encoded) => STANDARD
            .decode(encoded)
            .map_err(|e| Error::Other(format!("invalid binary key name: {e}"))),
        None => Ok(name.as_bytes().to_vec()),
    }
}

/// Request to read one key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyReadRequest {
    /// `[Database "0", (KeyPrefix...), Key k]`.
    pub key: ObjectPath,
    /// Cursor of the previous page.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Entries per page (clamped to `1..=10_000`).
    pub count: u32,
    /// `HSCAN`/`SSCAN`/`ZSCAN` `MATCH` pattern.
    #[serde(default)]
    pub pattern: Option<String>,
    /// Strings: bytes to read (default 1 MiB, `GETRANGE`).
    #[serde(default)]
    pub max_string_bytes: Option<u64>,
}

/// One page of a key's value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyValuePage {
    /// The key read.
    pub key: ObjectPath,
    /// `string|hash|list|set|zset|stream|ReJSON-RL|...`.
    pub key_type: String,
    /// Remaining TTL in milliseconds; `None` = persistent.
    pub ttl_ms: Option<i64>,
    /// `STRLEN`/`HLEN`/`LLEN`/`SCARD`/`ZCARD`/`XLEN`.
    pub length: Option<u64>,
    /// `MEMORY USAGE` (may be denied, then `None`).
    pub memory_bytes: Option<u64>,
    /// `OBJECT ENCODING`.
    pub encoding: Option<String>,
    /// The entries of this page.
    pub entries: KeyEntries,
    /// Cursor for the next page; `None` when done.
    pub next_cursor: Option<String>,
}

/// Entries of a key page, by data type. UTF-8 values are `Text`, others `Bytes`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum KeyEntries {
    /// A string.
    #[serde(rename_all = "camelCase")]
    String {
        /// The (possibly truncated) value.
        value: Value,
        /// Full length in bytes.
        total_bytes: u64,
        /// Only a prefix was read.
        truncated: bool,
    },
    /// Hash fields.
    Hash {
        /// `(field, value)` pairs.
        fields: Vec<(Value, Value)>,
    },
    /// List items; cursor = next start index.
    List {
        /// Index of the first item.
        start: u64,
        /// Items.
        items: Vec<Value>,
    },
    /// Set members.
    Set {
        /// Members.
        members: Vec<Value>,
    },
    /// Sorted set members with scores as Redis text (`inf` allowed).
    ZSet {
        /// `(member, score)` pairs.
        members: Vec<(Value, String)>,
    },
    /// Stream entries; cursor = next id (exclusive).
    Stream {
        /// Entries.
        entries: Vec<StreamEntry>,
        /// Number of consumer groups.
        groups: u64,
    },
    /// RedisJSON `JSON.GET`.
    Json {
        /// JSON text.
        text: String,
    },
    /// A type without a dedicated viewer.
    #[serde(rename_all = "camelCase")]
    Unsupported {
        /// Type name reported by the server.
        type_name: String,
    },
}

/// One stream entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamEntry {
    /// Entry id (`1700000000000-0`).
    pub id: String,
    /// `(field, value)` pairs.
    pub fields: Vec<(Value, Value)>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn utf8_keys_are_kept() {
        assert_eq!(encode_key_name(b"user:1:name"), "user:1:name");
        assert_eq!(encode_key_name("tên".as_bytes()), "tên");
        assert_eq!(encode_key_name(b""), "");
        assert_eq!(decode_key_name("user:1").unwrap(), b"user:1".to_vec());
    }

    #[test]
    fn binary_keys_roundtrip() {
        let raw = [0u8, 159, 146, 150, 255];
        let name = encode_key_name(&raw);
        assert!(name.starts_with("\u{0}b64:"));
        assert_eq!(name, "\u{0}b64:AJ+Slv8=");
        assert_eq!(decode_key_name(&name).unwrap(), raw.to_vec());
    }

    #[test]
    fn keys_starting_with_marker_are_escaped() {
        let raw = "\u{0}b64:literal".as_bytes();
        let name = encode_key_name(raw);
        assert_ne!(name.as_bytes(), raw);
        assert_eq!(decode_key_name(&name).unwrap(), raw.to_vec());
        assert!(decode_key_name("\u{0}b64:***").is_err());
    }

    #[test]
    fn key_entries_camel_case() {
        let s = KeyEntries::String {
            value: Value::Text("hi".into()),
            total_bytes: 2,
            truncated: false,
        };
        assert_eq!(
            serde_json::to_value(&s).unwrap(),
            json!({"type": "string", "value": {"t": "text", "v": "hi"}, "totalBytes": 2, "truncated": false})
        );
        let u = KeyEntries::Unsupported {
            type_name: "MBbloom--".into(),
        };
        let json = serde_json::to_value(&u).unwrap();
        assert_eq!(
            json,
            json!({"type": "unsupported", "typeName": "MBbloom--"})
        );
        assert_eq!(serde_json::from_value::<KeyEntries>(json).unwrap(), u);
        let z = KeyEntries::ZSet {
            members: vec![(Value::Text("a".into()), "1.5".into())],
        };
        assert_eq!(
            serde_json::to_value(&z).unwrap(),
            json!({"type": "zSet", "members": [[{"t": "text", "v": "a"}, "1.5"]]})
        );
    }

    #[test]
    fn page_roundtrip() {
        let page = KeyValuePage {
            key: ObjectPath::default(),
            key_type: "stream".into(),
            ttl_ms: Some(1000),
            length: Some(1),
            memory_bytes: None,
            encoding: Some("stream".into()),
            entries: KeyEntries::Stream {
                entries: vec![StreamEntry {
                    id: "1-0".into(),
                    fields: vec![(Value::Text("f".into()), Value::Bytes(vec![1]))],
                }],
                groups: 2,
            },
            next_cursor: None,
        };
        let json = serde_json::to_value(&page).unwrap();
        assert_eq!(json["keyType"], "stream");
        assert_eq!(json["ttlMs"], 1000);
        assert!(json["memoryBytes"].is_null());
        assert_eq!(serde_json::from_value::<KeyValuePage>(json).unwrap(), page);
    }
}
