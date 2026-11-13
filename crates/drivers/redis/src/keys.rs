//! Reading one key: type, TTL, metadata and a page of its entries.

use dbstudio_core::{
    Error, KeyEntries, KeyReadRequest, KeyValuePage, ObjectKind, Result, StreamEntry, Value,
    decode_key_name,
};

use crate::conn::Shared;
use crate::convert::{bytes_value, reply_bytes, reply_text, trim_partial_utf8};
use crate::tree::db_of;

const DEFAULT_MAX_STRING: u64 = 1024 * 1024;

fn pairs(flat: Vec<Vec<u8>>) -> Vec<(Value, Value)> {
    let mut it = flat.into_iter();
    let mut out = Vec::new();
    while let (Some(k), Some(v)) = (it.next(), it.next()) {
        out.push((bytes_value(k), bytes_value(v)));
    }
    out
}

impl Shared {
    /// Read one page of the key at `req.key`.
    pub async fn read_key(&self, req: KeyReadRequest) -> Result<KeyValuePage> {
        let seg = req
            .key
            .last()
            .filter(|s| s.kind == ObjectKind::Key)
            .ok_or_else(|| Error::Other("path is not a key".into()))?;
        let key = decode_key_name(&seg.name)?;
        let db = db_of(&req.key, self.default_db)?;
        let count = req.count.clamp(1, 10_000) as i64;

        let mut pipe = redis::pipe();
        pipe.cmd("TYPE").arg(key.as_slice());
        pipe.cmd("PTTL").arg(key.as_slice());
        let head: Vec<redis::Value> = self.run_pipe(db, &pipe).await?;
        let key_type = reply_text(&head[0]);
        if key_type == "none" || key_type.is_empty() {
            return Err(Error::NotFound(format!("key {}", seg.name)));
        }
        let ttl_ms = match head[1] {
            redis::Value::Int(i) if i >= 0 => Some(i),
            redis::Value::Int(-2) => return Err(Error::NotFound(format!("key {}", seg.name))),
            _ => None,
        };
        // Optional metadata: a denied or unsupported command only leaves the field empty.
        let encoding = self
            .run::<Option<String>>(db, redis::cmd("OBJECT").arg("ENCODING").arg(key.as_slice()))
            .await
            .ok()
            .flatten();
        let memory_bytes = self
            .run::<Option<u64>>(db, redis::cmd("MEMORY").arg("USAGE").arg(key.as_slice()))
            .await
            .ok()
            .flatten();

        let cursor = req.cursor.as_deref();
        let pattern = req
            .pattern
            .as_deref()
            .filter(|p| !p.is_empty() && *p != "*");
        let mut next_cursor = None;
        let (length, entries) = match key_type.as_str() {
            "string" => {
                let total: u64 = self
                    .run(db, redis::cmd("STRLEN").arg(key.as_slice()))
                    .await?;
                let max = req.max_string_bytes.unwrap_or(DEFAULT_MAX_STRING).max(1);
                let truncated = total > max;
                let data: Vec<u8> = self
                    .run(
                        db,
                        redis::cmd("GETRANGE")
                            .arg(key.as_slice())
                            .arg(0)
                            .arg(max.saturating_sub(1)),
                    )
                    .await?;
                let data = if truncated {
                    trim_partial_utf8(data)
                } else {
                    data
                };
                (
                    Some(total),
                    KeyEntries::String {
                        value: bytes_value(data),
                        total_bytes: total,
                        truncated,
                    },
                )
            }
            "hash" => {
                let len: u64 = self.run(db, redis::cmd("HLEN").arg(key.as_slice())).await?;
                let (next, flat) = self
                    .scan_kind(db, "HSCAN", &key, cursor, pattern, count)
                    .await?;
                next_cursor = next;
                (
                    Some(len),
                    KeyEntries::Hash {
                        fields: pairs(flat),
                    },
                )
            }
            "set" => {
                let len: u64 = self
                    .run(db, redis::cmd("SCARD").arg(key.as_slice()))
                    .await?;
                let (next, flat) = self
                    .scan_kind(db, "SSCAN", &key, cursor, pattern, count)
                    .await?;
                next_cursor = next;
                (
                    Some(len),
                    KeyEntries::Set {
                        members: flat.into_iter().map(bytes_value).collect(),
                    },
                )
            }
            "zset" => {
                let len: u64 = self
                    .run(db, redis::cmd("ZCARD").arg(key.as_slice()))
                    .await?;
                let members = if pattern.is_some() {
                    let (next, flat) = self
                        .scan_kind(db, "ZSCAN", &key, cursor, pattern, count)
                        .await?;
                    next_cursor = next;
                    flat
                } else {
                    let start = parse_index(cursor)?;
                    let flat: Vec<Vec<u8>> = self
                        .run(
                            db,
                            redis::cmd("ZRANGE")
                                .arg(key.as_slice())
                                .arg(start)
                                .arg(start + count - 1)
                                .arg("WITHSCORES"),
                        )
                        .await?;
                    let got = (flat.len() / 2) as u64;
                    if start as u64 + got < len {
                        next_cursor = Some((start as u64 + got).to_string());
                    }
                    flat
                };
                let mut out = Vec::new();
                let mut it = members.into_iter();
                while let (Some(m), Some(s)) = (it.next(), it.next()) {
                    out.push((bytes_value(m), String::from_utf8_lossy(&s).into_owned()));
                }
                (Some(len), KeyEntries::ZSet { members: out })
            }
            "list" => {
                let len: u64 = self.run(db, redis::cmd("LLEN").arg(key.as_slice())).await?;
                let start = parse_index(cursor)?;
                let items: Vec<Vec<u8>> = self
                    .run(
                        db,
                        redis::cmd("LRANGE")
                            .arg(key.as_slice())
                            .arg(start)
                            .arg(start + count - 1),
                    )
                    .await?;
                let end = start as u64 + items.len() as u64;
                if end < len {
                    next_cursor = Some(end.to_string());
                }
                (
                    Some(len),
                    KeyEntries::List {
                        start: start as u64,
                        items: items.into_iter().map(bytes_value).collect(),
                    },
                )
            }
            "stream" => {
                let len: u64 = self.run(db, redis::cmd("XLEN").arg(key.as_slice())).await?;
                let from = match cursor {
                    Some(id) if !id.is_empty() => format!("({id}"),
                    _ => "-".to_string(),
                };
                let reply: redis::Value = self
                    .run(
                        db,
                        redis::cmd("XRANGE")
                            .arg(key.as_slice())
                            .arg(from)
                            .arg("+")
                            .arg("COUNT")
                            .arg(count),
                    )
                    .await?;
                let entries = parse_stream_entries(reply);
                if entries.len() as i64 >= count && len > entries.len() as u64 {
                    next_cursor = entries.last().map(|e| e.id.clone());
                }
                let groups = self.stream_groups(db, &key).await;
                (Some(len), KeyEntries::Stream { entries, groups })
            }
            "ReJSON-RL" => {
                let max = req.max_string_bytes.unwrap_or(DEFAULT_MAX_STRING).max(1);
                // Ask the size first (RedisJSON `JSON.DEBUG MEMORY`) so a huge document is not
                // pulled over the wire; when that is denied, the length check below still applies.
                let size = self
                    .run::<u64>(
                        db,
                        redis::cmd("JSON.DEBUG").arg("MEMORY").arg(key.as_slice()),
                    )
                    .await
                    .ok();
                if size.is_some_and(|n| n > max) {
                    (
                        size,
                        KeyEntries::Unsupported {
                            type_name: format!(
                                "ReJSON-RL ({} bytes, over the size limit)",
                                size.unwrap_or(0)
                            ),
                        },
                    )
                } else {
                    let text: String = self
                        .run(db, redis::cmd("JSON.GET").arg(key.as_slice()))
                        .await?;
                    if text.len() as u64 > max {
                        (
                            Some(text.len() as u64),
                            KeyEntries::Unsupported {
                                type_name: format!(
                                    "ReJSON-RL ({} bytes, over the size limit)",
                                    text.len()
                                ),
                            },
                        )
                    } else {
                        (None, KeyEntries::Json { text })
                    }
                }
            }
            other => (
                None,
                KeyEntries::Unsupported {
                    type_name: other.to_string(),
                },
            ),
        };
        Ok(KeyValuePage {
            key: req.key,
            key_type,
            ttl_ms,
            length,
            memory_bytes,
            encoding,
            entries,
            next_cursor,
        })
    }

    async fn scan_kind(
        &self,
        db: i64,
        command: &str,
        key: &[u8],
        cursor: Option<&str>,
        pattern: Option<&str>,
        count: i64,
    ) -> Result<(Option<String>, Vec<Vec<u8>>)> {
        let mut cmd = redis::cmd(command);
        cmd.arg(key)
            .arg(cursor.filter(|c| !c.is_empty()).unwrap_or("0"));
        if let Some(p) = pattern {
            cmd.arg("MATCH").arg(p);
        }
        cmd.arg("COUNT").arg(count);
        let (next, flat): (String, Vec<Vec<u8>>) = self.run(db, &cmd).await?;
        Ok(((next != "0").then_some(next), flat))
    }

    async fn stream_groups(&self, db: i64, key: &[u8]) -> u64 {
        let reply: Result<redis::Value> = self
            .run(db, redis::cmd("XINFO").arg("STREAM").arg(key))
            .await;
        let Ok(reply) = reply else { return 0 };
        let flat = match reply {
            redis::Value::Array(items) => items,
            redis::Value::Map(pairs) => pairs.into_iter().flat_map(|(k, v)| [k, v]).collect(),
            _ => return 0,
        };
        let mut it = flat.into_iter();
        while let (Some(k), Some(v)) = (it.next(), it.next()) {
            if reply_text(&k) == "groups" {
                if let redis::Value::Int(n) = v {
                    return n.max(0) as u64;
                }
            }
        }
        0
    }
}

fn parse_index(cursor: Option<&str>) -> Result<i64> {
    match cursor.filter(|c| !c.is_empty()) {
        None => Ok(0),
        Some(c) => c
            .parse::<i64>()
            .ok()
            .filter(|n| *n >= 0)
            .ok_or_else(|| Error::Other(format!("invalid cursor `{c}`"))),
    }
}

/// `XRANGE` reply: `[[id, [field, value, ...]], ...]`.
pub fn parse_stream_entries(reply: redis::Value) -> Vec<StreamEntry> {
    let redis::Value::Array(items) = reply else {
        return Vec::new();
    };
    items
        .into_iter()
        .filter_map(|item| {
            let redis::Value::Array(parts) = item else {
                return None;
            };
            let mut it = parts.into_iter();
            let id = reply_text(&it.next()?);
            let fields = match it.next() {
                Some(redis::Value::Array(flat)) => {
                    let mut f = Vec::new();
                    let mut fi = flat.into_iter();
                    while let (Some(k), Some(v)) = (fi.next(), fi.next()) {
                        f.push((
                            bytes_value(reply_bytes(&k).unwrap_or_default()),
                            bytes_value(reply_bytes(&v).unwrap_or_default()),
                        ));
                    }
                    f
                }
                _ => Vec::new(),
            };
            Some(StreamEntry { id, fields })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_reply_is_parsed() {
        let b = |s: &str| redis::Value::BulkString(s.as_bytes().to_vec());
        let reply = redis::Value::Array(vec![redis::Value::Array(vec![
            b("1-0"),
            redis::Value::Array(vec![b("f"), b("v")]),
        ])]);
        let e = parse_stream_entries(reply);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].id, "1-0");
        assert_eq!(
            e[0].fields,
            vec![(Value::Text("f".into()), Value::Text("v".into()))]
        );
    }

    #[test]
    fn cursor_index_validation() {
        assert_eq!(parse_index(None).unwrap(), 0);
        assert_eq!(parse_index(Some("7")).unwrap(), 7);
        assert!(parse_index(Some("x")).is_err());
    }
}
