//! Object tree: databases, virtual key prefixes and keys, paged with `SCAN`.

use std::collections::{BTreeMap, HashSet};
use std::time::{Duration, Instant};

use dbstudio_core::{
    ChildrenPage, ChildrenPageRequest, Error, ObjectKind, ObjectNode, ObjectPath, Result,
    decode_key_name, encode_key_name,
};

use crate::conn::{Shared, parse_db};
use crate::convert::reply_text;

/// `SCAN COUNT` per round trip.
const SCAN_COUNT: u32 = 1000;
/// Children returned by the unpaged `list_children`.
const UNPAGED_LIMIT: u32 = 1000;
/// Longest time one page may keep scanning before it is returned with a cursor.
const PAGE_BUDGET: Duration = Duration::from_secs(2);

/// Escape glob metacharacters so `text` matches literally in `SCAN MATCH`.
pub fn escape_glob(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    for &b in text {
        if matches!(b, b'\\' | b'*' | b'?' | b'[' | b']') {
            out.push(b'\\');
        }
        out.push(b);
    }
    out
}

/// Database number of the first `Database` segment of `path`.
pub fn db_of(path: &ObjectPath, default_db: i64) -> Result<i64> {
    match path
        .segments
        .iter()
        .find(|s| s.kind == ObjectKind::Database)
    {
        Some(seg) => {
            parse_db(&seg.name).ok_or_else(|| Error::NotFound(format!("database {}", seg.name)))
        }
        None => Ok(default_db),
    }
}

fn find_sub(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// `INFO keyspace` as db number -> number of keys.
fn parse_keyspace(info: &str) -> BTreeMap<i64, u64> {
    let mut map = BTreeMap::new();
    for line in info.lines() {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        let Some(n) = name.strip_prefix("db").and_then(|n| n.parse::<i64>().ok()) else {
            continue;
        };
        let keys = rest
            .split(',')
            .find_map(|kv| kv.strip_prefix("keys="))
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        map.insert(n, keys);
    }
    map
}

impl Shared {
    /// Databases of the server: the non-empty ones plus the current one (all with
    /// `show_empty_dbs`).
    pub async fn databases(&self) -> Result<Vec<ObjectNode>> {
        let count = match self
            .run::<Vec<String>>(
                self.default_db,
                redis::cmd("CONFIG").arg("GET").arg("databases"),
            )
            .await
        {
            Ok(v) => v.get(1).and_then(|n| n.parse::<i64>().ok()).unwrap_or(16),
            // CONFIG may be denied or renamed.
            Err(_) => 16,
        };
        let info: String = self
            .run(self.default_db, redis::cmd("INFO").arg("keyspace"))
            .await?;
        let keyspace = parse_keyspace(&info);
        // The keyspace may list a database beyond a restricted `CONFIG` answer.
        let count = count.max(keyspace.keys().next_back().map_or(0, |m| m + 1));
        Ok((0..count)
            .filter(|n| {
                self.show_empty_dbs
                    || *n == self.default_db
                    || keyspace.get(n).is_some_and(|k| *k > 0)
            })
            .map(|n| {
                let name = n.to_string();
                ObjectNode {
                    path: ObjectPath::root().child(ObjectKind::Database, name.clone()),
                    kind: ObjectKind::Database,
                    name,
                    has_children: true,
                    detail: Some(keyspace.get(&n).copied().unwrap_or(0).to_string()),
                }
            })
            .collect())
    }

    /// One page of the children of a database or key prefix.
    pub async fn children_page(&self, req: ChildrenPageRequest) -> Result<ChildrenPage> {
        if req.parent.is_root() {
            let nodes = self.databases().await?;
            let total = nodes.len() as u64;
            return Ok(ChildrenPage {
                nodes,
                next_cursor: None,
                approximate_total: Some(total),
            });
        }
        let db = db_of(&req.parent, self.default_db)?;
        let last = req.parent.last().expect("non-root");
        let prefix: Vec<u8> = match last.kind {
            ObjectKind::Database => Vec::new(),
            ObjectKind::KeyPrefix => decode_key_name(&last.name)?,
            _ => {
                return Ok(ChildrenPage {
                    nodes: Vec::new(),
                    next_cursor: None,
                    approximate_total: None,
                });
            }
        };
        let limit = req.limit.clamp(1, 5000) as usize;
        let pattern = req
            .pattern
            .as_deref()
            .filter(|p| !p.is_empty() && *p != "*");
        // The pattern is relative to the node: `MATCH <escaped prefix><pattern>`; without a
        // pattern, everything under the prefix.
        let match_arg: Vec<u8> = {
            let mut m = escape_glob(&prefix);
            m.extend_from_slice(pattern.unwrap_or("*").as_bytes());
            m
        };
        let deadline = Instant::now() + PAGE_BUDGET;
        let delimiter = self.delimiter.as_bytes();
        let mut cursor = match req.cursor.as_deref() {
            Some(c) => c
                .parse::<u64>()
                .map_err(|_| Error::Other(format!("invalid cursor `{c}`")))?,
            None => 0,
        };
        let mut prefixes: Vec<Vec<u8>> = Vec::new();
        let mut seen_prefixes: HashSet<Vec<u8>> = HashSet::new();
        let mut keys: Vec<Vec<u8>> = Vec::new();
        loop {
            let (next, batch): (String, Vec<Vec<u8>>) = self
                .run(
                    db,
                    redis::cmd("SCAN")
                        .arg(cursor)
                        .arg("MATCH")
                        .arg(match_arg.as_slice())
                        .arg("COUNT")
                        .arg(SCAN_COUNT),
                )
                .await?;
            cursor = next.parse::<u64>().unwrap_or(0);
            for key in batch {
                let Some(rest) = key.strip_prefix(prefix.as_slice()) else {
                    continue;
                };
                match find_sub(rest, delimiter) {
                    Some(pos) => {
                        let mut child = prefix.clone();
                        child.extend_from_slice(&rest[..pos + delimiter.len()]);
                        if seen_prefixes.insert(child.clone()) {
                            prefixes.push(child);
                        }
                    }
                    None => keys.push(key),
                }
            }
            // A sparse pattern on a huge database must not make one page run forever: return early
            // with the cursor (the UI shows "load more").
            if cursor == 0 || prefixes.len() + keys.len() >= limit || Instant::now() >= deadline {
                break;
            }
        }
        prefixes.sort();
        keys.sort();
        keys.dedup();
        let mut nodes: Vec<ObjectNode> = prefixes
            .into_iter()
            .map(|p| {
                let name = encode_key_name(&p);
                ObjectNode {
                    path: req.parent.child(ObjectKind::KeyPrefix, name.clone()),
                    kind: ObjectKind::KeyPrefix,
                    name,
                    has_children: true,
                    detail: None,
                }
            })
            .collect();
        let details = self.key_details(db, &keys).await?;
        for (key, detail) in keys.iter().zip(details) {
            let Some(detail) = detail else { continue };
            let name = encode_key_name(key);
            nodes.push(ObjectNode {
                path: req.parent.child(ObjectKind::Key, name.clone()),
                kind: ObjectKind::Key,
                name,
                has_children: false,
                detail: Some(detail),
            });
        }
        let approximate_total = if pattern.is_none() && prefix.is_empty() {
            self.run::<u64>(db, &redis::cmd("DBSIZE")).await.ok()
        } else {
            None
        };
        Ok(ChildrenPage {
            nodes,
            next_cursor: (cursor != 0).then(|| cursor.to_string()),
            approximate_total,
        })
    }

    /// `"<type>"` or `"<type> ttl:<seconds>"` for each key (`None` = vanished meanwhile).
    async fn key_details(&self, db: i64, keys: &[Vec<u8>]) -> Result<Vec<Option<String>>> {
        let mut out = Vec::with_capacity(keys.len());
        for chunk in keys.chunks(500) {
            let mut pipe = redis::pipe();
            for k in chunk {
                pipe.cmd("TYPE").arg(k.as_slice());
                pipe.cmd("PTTL").arg(k.as_slice());
            }
            let replies: Vec<redis::Value> = self.run_pipe(db, &pipe).await?;
            for pair in replies.chunks(2) {
                let ty = reply_text(&pair[0]);
                let pttl = match &pair[1] {
                    redis::Value::Int(i) => *i,
                    _ => -1,
                };
                out.push(if ty.is_empty() || ty == "none" {
                    None
                } else if pttl >= 0 {
                    Some(format!("{ty} ttl:{}", (pttl + 999) / 1000))
                } else {
                    Some(ty)
                });
            }
        }
        Ok(out)
    }

    /// Unpaged children (first [`UNPAGED_LIMIT`]).
    pub async fn children(&self, parent: &ObjectPath) -> Result<Vec<ObjectNode>> {
        Ok(self
            .children_page(ChildrenPageRequest {
                parent: parent.clone(),
                cursor: None,
                limit: UNPAGED_LIMIT,
                pattern: None,
            })
            .await?
            .nodes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_escape_is_literal() {
        assert_eq!(escape_glob(b"a*b[c]\\"), b"a\\*b\\[c\\]\\\\".to_vec());
    }

    #[test]
    fn keyspace_parsing() {
        let info = "# Keyspace\r\ndb0:keys=5,expires=1,avg_ttl=0\r\ndb3:keys=12,expires=0\r\n";
        let m = parse_keyspace(info);
        assert_eq!(m.get(&0), Some(&5));
        assert_eq!(m.get(&3), Some(&12));
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn database_of_path() {
        let p = ObjectPath::root()
            .child(ObjectKind::Database, "4")
            .child(ObjectKind::KeyPrefix, "a:");
        assert_eq!(db_of(&p, 0).unwrap(), 4);
        assert_eq!(db_of(&ObjectPath::root(), 2).unwrap(), 2);
    }
}
