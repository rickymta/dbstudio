//! `KeyChange::Put`: write a whole key in a temporary key, then `RENAME` (or `RENAMENX`) it into
//! place, so a reader never sees a half-written value. Big values go in chunks (strings by
//! byte ranges via `SETRANGE`, collections by batches bounded in count and size).

use std::ops::Range;

use dbstudio_core::{Error, KeyData, PutMode, Result, StreamEntry, Value, decode_key_name};
use redis::aio::MultiplexedConnection;
use uuid::Uuid;

use crate::cli::render_command;
use crate::convert::value_bytes;
use crate::error::map_err;

type Args = Vec<Vec<u8>>;
/// One stream entry: id and `(field, value)` pairs.
type StreamRow = (String, Vec<(Vec<u8>, Vec<u8>)>);

/// Bytes of a string written per `SETRANGE`.
const STRING_CHUNK: usize = 8 * 1024 * 1024;
/// Most items (fields, members, entries) in one batch.
const BATCH_ITEMS: usize = 1000;
/// Rough byte budget of one batch.
const BATCH_BYTES: usize = 4 * 1024 * 1024;
/// Lifetime of an unfinished temporary key (cleans up after a crash), renewed by every batch, in ms.
const TEMP_GUARD_MS: u64 = 3_600_000;

/// A validated `Put`, ready to run.
pub struct PutPlan {
    key: Vec<u8>,
    body: Body,
    ttl_ms: Option<u64>,
    mode: PutMode,
}

enum Body {
    String(Vec<u8>),
    Hash(Vec<(Vec<u8>, Vec<u8>)>),
    List(Vec<Vec<u8>>),
    Set(Vec<Vec<u8>>),
    ZSet(Vec<(Vec<u8>, String)>),
    Stream(Vec<StreamRow>),
    Json(String),
}

fn empty(kind: &str) -> Error {
    Error::Invalid(format!("cannot store an empty {kind} in Redis"))
}

impl PutPlan {
    /// Validate and convert; no I/O.
    pub fn new(key: &str, data: &KeyData, ttl_ms: Option<u64>, mode: PutMode) -> Result<PutPlan> {
        if ttl_ms == Some(0) {
            return Err(Error::Invalid("the TTL must be at least 1 ms".into()));
        }
        let key = decode_key_name(key)?;
        let pairs = |v: &[(Value, Value)]| -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
            v.iter()
                .map(|(a, b)| Ok((value_bytes(a)?, value_bytes(b)?)))
                .collect()
        };
        let items = |v: &[Value]| -> Result<Vec<Vec<u8>>> { v.iter().map(value_bytes).collect() };
        let body = match data {
            KeyData::String { value } => Body::String(value_bytes(value)?),
            KeyData::Hash { fields } if fields.is_empty() => return Err(empty("hash")),
            KeyData::Hash { fields } => Body::Hash(pairs(fields)?),
            KeyData::List { items: i } if i.is_empty() => return Err(empty("list")),
            KeyData::List { items: i } => Body::List(items(i)?),
            KeyData::Set { members } if members.is_empty() => return Err(empty("set")),
            KeyData::Set { members } => Body::Set(items(members)?),
            KeyData::ZSet { members } if members.is_empty() => return Err(empty("sorted set")),
            KeyData::ZSet { members } => {
                let mut out = Vec::with_capacity(members.len());
                for (m, score) in members {
                    let t = score.trim();
                    if !t.parse::<f64>().is_ok_and(|f| !f.is_nan()) {
                        return Err(Error::Invalid(format!("invalid score `{score}`")));
                    }
                    out.push((value_bytes(m)?, t.to_string()));
                }
                Body::ZSet(out)
            }
            KeyData::Stream { entries } if entries.is_empty() => return Err(empty("stream")),
            KeyData::Stream { entries } => {
                let mut out = Vec::with_capacity(entries.len());
                for StreamEntry { id, fields } in entries {
                    if fields.is_empty() {
                        return Err(Error::Invalid(
                            "a stream entry needs at least one field".into(),
                        ));
                    }
                    let id = if id.is_empty() { "*" } else { id.as_str() };
                    out.push((id.to_string(), pairs(fields)?));
                }
                Body::Stream(out)
            }
            KeyData::Json { text } => Body::Json(text.clone()),
        };
        Ok(PutPlan {
            key,
            body,
            ttl_ms,
            mode,
        })
    }

    fn type_name(&self) -> &'static str {
        match self.body {
            Body::String(_) => "string",
            Body::Hash(_) => "hash",
            Body::List(_) => "list",
            Body::Set(_) => "set",
            Body::ZSet(_) => "zset",
            Body::Stream(_) => "stream",
            Body::Json(_) => "json",
        }
    }

    fn items(&self) -> usize {
        match &self.body {
            Body::String(v) => v.len(),
            Body::Hash(v) => v.len(),
            Body::List(v) | Body::Set(v) => v.len(),
            Body::ZSet(v) => v.len(),
            Body::Stream(v) => v.len(),
            Body::Json(t) => t.len(),
        }
    }
}

/// redis-cli lines describing a `Put` (the temporary key is shown as `<temp>`).
pub fn preview_put(
    key: &str,
    data: &KeyData,
    ttl_ms: Option<u64>,
    mode: PutMode,
) -> Result<String> {
    let p = PutPlan::new(key, data, ttl_ms, mode)?;
    let unit = if matches!(p.body, Body::String(_) | Body::Json(_)) {
        "byte(s)"
    } else {
        "item(s)"
    };
    let mut lines = vec![format!(
        "-- build the {} ({} {unit}) in a temporary key, in chunks",
        p.type_name(),
        p.items()
    )];
    let tmp = b"<temp>".to_vec();
    match p.ttl_ms {
        Some(ms) => lines.push(render_command(&[
            b"PEXPIRE".to_vec(),
            tmp.clone(),
            ms.to_string().into_bytes(),
        ])),
        None => lines.push(render_command(&[b"PERSIST".to_vec(), tmp.clone()])),
    }
    let verb: &[u8] = match p.mode {
        PutMode::Replace => b"RENAME",
        PutMode::IfAbsent => b"RENAMENX",
    };
    lines.push(render_command(&[verb.to_vec(), tmp, p.key.clone()]));
    Ok(lines.join("\n"))
}

/// Batches `0..len` by item count and rough byte size.
fn ranges(len: usize, size_of: impl Fn(usize) -> usize) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut bytes = 0;
    for i in 0..len {
        bytes += size_of(i) + 16;
        if i + 1 - start >= BATCH_ITEMS || bytes >= BATCH_BYTES {
            out.push(start..i + 1);
            start = i + 1;
            bytes = 0;
        }
    }
    if start < len {
        out.push(start..len);
    }
    out
}

fn head(name: &[u8], tmp: &[u8]) -> Args {
    vec![name.to_vec(), tmp.to_vec()]
}

/// Every pipeline (list of commands) that builds the value in `tmp`, in order.
fn batches<'a>(body: &'a Body, tmp: &'a [u8]) -> Box<dyn Iterator<Item = Vec<Args>> + Send + 'a> {
    match body {
        Body::String(v) if v.is_empty() => Box::new(std::iter::once(vec![vec![
            b"SET".to_vec(),
            tmp.to_vec(),
            Vec::new(),
        ]])),
        // Chunks go in with `SETRANGE`, LAST chunk first: the first command allocates the whole
        // string once and the others fill it in place (a growing `APPEND` can briefly need
        // twice the memory while the server reallocates).
        Body::String(v) => Box::new(v.chunks(STRING_CHUNK).enumerate().rev().map(move |(i, c)| {
            let mut a = head(b"SETRANGE", tmp);
            a.push((i * STRING_CHUNK).to_string().into_bytes());
            a.push(c.to_vec());
            vec![a]
        })),
        Body::Json(t) => Box::new(std::iter::once(vec![vec![
            b"JSON.SET".to_vec(),
            tmp.to_vec(),
            b"$".to_vec(),
            t.as_bytes().to_vec(),
        ]])),
        Body::Hash(v) => Box::new(
            ranges(v.len(), |i| v[i].0.len() + v[i].1.len())
                .into_iter()
                .map(move |r| {
                    let mut a = head(b"HSET", tmp);
                    for (f, x) in &v[r] {
                        a.push(f.clone());
                        a.push(x.clone());
                    }
                    vec![a]
                }),
        ),
        Body::List(v) | Body::Set(v) => {
            let name: &'static [u8] = if matches!(body, Body::List(_)) {
                b"RPUSH"
            } else {
                b"SADD"
            };
            Box::new(ranges(v.len(), |i| v[i].len()).into_iter().map(move |r| {
                let mut a = head(name, tmp);
                a.extend(v[r].iter().cloned());
                vec![a]
            }))
        }
        Body::ZSet(v) => Box::new(
            ranges(v.len(), |i| v[i].0.len() + v[i].1.len())
                .into_iter()
                .map(move |r| {
                    let mut a = head(b"ZADD", tmp);
                    for (m, s) in &v[r] {
                        a.push(s.clone().into_bytes());
                        a.push(m.clone());
                    }
                    vec![a]
                }),
        ),
        Body::Stream(v) => Box::new(
            ranges(v.len(), |i| {
                v[i].1.iter().map(|(f, x)| f.len() + x.len()).sum()
            })
            .into_iter()
            .map(move |r| {
                v[r].iter()
                    .map(|(id, fields)| {
                        let mut a = head(b"XADD", tmp);
                        a.push(id.clone().into_bytes());
                        for (f, x) in fields {
                            a.push(f.clone());
                            a.push(x.clone());
                        }
                        a
                    })
                    .collect()
            }),
        ),
    }
}

async fn run_pipeline(
    conn: &mut MultiplexedConnection,
    cmds: &[Args],
    tmp: &[u8],
) -> std::result::Result<(), String> {
    let mut pipe = redis::pipe();
    for c in cmds {
        let cmd = pipe.cmd(&String::from_utf8_lossy(&c[0]));
        for a in &c[1..] {
            cmd.arg(a.as_slice());
        }
    }
    // Every pipeline renews the lifetime of the temporary key: a crash leaves nothing behind
    // for long, and a build that takes a long time never loses the key to the guard.
    pipe.cmd("PEXPIRE").arg(tmp).arg(TEMP_GUARD_MS);
    pipe.query_async::<redis::Value>(conn)
        .await
        .map(|_| ())
        .map_err(|e| map_err(e).message())
}

async fn simple(
    conn: &mut MultiplexedConnection,
    name: &str,
    args: &[&[u8]],
) -> std::result::Result<redis::Value, String> {
    let mut c = redis::cmd(name);
    for a in args {
        c.arg(*a);
    }
    c.query_async(conn).await.map_err(|e| map_err(e).message())
}

/// Hashtag-aware temporary key name: `<key>.__dbstudio_tmp.<uuid>`. The key stays the prefix (an
/// ACL pattern such as `~app:*` that allows the key allows the temporary key too). In a cluster
/// both must hash to the same slot for `RENAME`: when the key has no `{tag}` of its own, the
/// tag `{<key>}` is added, which hashes like the whole key. A key that holds a `}` but no
/// valid tag cannot be tagged this way and gets the plain name (fine outside a cluster).
fn temp_name(key: &[u8], id: &str) -> Vec<u8> {
    let has_tag = key
        .iter()
        .position(|b| *b == b'{')
        .and_then(|open| key[open + 1..].iter().position(|b| *b == b'}'))
        .is_some_and(|len| len > 0);
    let mut name = key.to_vec();
    name.extend_from_slice(TEMP_MARKER.as_bytes());
    if !has_tag && !key.contains(&b'{') && !key.contains(&b'}') {
        name.push(b'{');
        name.extend_from_slice(key);
        name.extend_from_slice(b"}.");
    }
    name.extend_from_slice(id.as_bytes());
    name
}

/// Whether `key` is the temporary key of a `Put` (hidden by `scan_keys`).
pub fn is_temp_key(key: &[u8]) -> bool {
    let m = TEMP_MARKER.as_bytes();
    key.windows(m.len()).any(|w| w == m)
}

/// Marker inside the names of temporary keys.
pub const TEMP_MARKER: &str = ".__dbstudio_tmp.";

impl PutPlan {
    /// Command and length the finished temporary key must report, when it can be checked.
    fn length_check(&self) -> Option<(&'static str, u64)> {
        fn unique<'a>(items: impl Iterator<Item = &'a [u8]>) -> u64 {
            items.collect::<std::collections::HashSet<_>>().len() as u64
        }
        match &self.body {
            Body::String(v) => Some(("STRLEN", v.len() as u64)),
            Body::List(v) => Some(("LLEN", v.len() as u64)),
            Body::Hash(v) => Some(("HLEN", unique(v.iter().map(|(f, _)| f.as_slice())))),
            Body::Set(v) => Some(("SCARD", unique(v.iter().map(Vec::as_slice)))),
            Body::ZSet(v) => Some(("ZCARD", unique(v.iter().map(|(m, _)| m.as_slice())))),
            Body::Stream(v) => Some(("XLEN", v.len() as u64)),
            Body::Json(_) => None,
        }
    }
}

async fn build(
    conn: &mut MultiplexedConnection,
    tmp: &[u8],
    p: &PutPlan,
) -> std::result::Result<(), String> {
    for cmds in batches(&p.body, tmp) {
        run_pipeline(conn, &cmds, tmp).await?;
    }
    // The finished temporary key must hold exactly what was sent: it fails when the key was
    // deleted or replaced by someone else half way (the guard expired, a FLUSH, a rename...).
    if let Some((name, expected)) = p.length_check() {
        let got = simple(conn, name, &[tmp]).await?;
        if !matches!(got, redis::Value::Int(n) if n as u64 == expected) {
            return Err(format!(
                "the temporary key changed while it was being written ({name} = {got:?}, expected {expected}); nothing was applied"
            ));
        }
    }
    Ok(())
}

/// Run one `Put`; the result is the affected count (0 = kept an existing key, `IfAbsent`, or
/// the key already expired).
pub async fn put_key(
    conn: &mut MultiplexedConnection,
    p: &PutPlan,
) -> std::result::Result<u64, String> {
    if p.mode == PutMode::IfAbsent {
        let exists = simple(conn, "EXISTS", &[&p.key]).await?;
        if matches!(exists, redis::Value::Int(n) if n > 0) {
            return Ok(0);
        }
    }
    let tmp = temp_name(&p.key, &Uuid::new_v4().simple().to_string());
    let outcome = match build(conn, &tmp, p).await {
        Ok(()) => finish(conn, &tmp, p).await,
        Err(e) => Err(e),
    };
    match outcome {
        Ok(1) => Ok(1),
        other => {
            // Kept an existing key, expired, or failed: no temporary key may stay.
            let _ = simple(conn, "DEL", &[&tmp]).await;
            other
        }
    }
}

/// `MULTI; PEXPIRE|PERSIST tmp; RENAME[NX] tmp key; EXEC` in one block, so the TTL and the final
/// name appear together.
async fn finish(
    conn: &mut MultiplexedConnection,
    tmp: &[u8],
    p: &PutPlan,
) -> std::result::Result<u64, String> {
    let q = |e: redis::RedisError| map_err(e).message();
    redis::cmd("MULTI")
        .query_async::<()>(conn)
        .await
        .map_err(q)?;
    let ttl = match p.ttl_ms {
        Some(ms) => {
            let mut c = redis::cmd("PEXPIRE");
            c.arg(tmp).arg(ms);
            c
        }
        None => {
            let mut c = redis::cmd("PERSIST");
            c.arg(tmp);
            c
        }
    };
    ttl.query_async::<redis::Value>(conn).await.map_err(q)?;
    let verb = match p.mode {
        PutMode::Replace => "RENAME",
        PutMode::IfAbsent => "RENAMENX",
    };
    redis::cmd(verb)
        .arg(tmp)
        .arg(p.key.as_slice())
        .query_async::<redis::Value>(conn)
        .await
        .map_err(q)?;
    let exec: redis::Value = redis::cmd("EXEC").query_async(conn).await.map_err(q)?;
    let redis::Value::Array(replies) = exec else {
        return Err(format!("unexpected EXEC reply: {exec:?}"));
    };
    match replies.get(1) {
        Some(redis::Value::ServerError(e)) => {
            let text = format!("{e:?}").to_ascii_lowercase();
            if text.contains("no such key") {
                // The temporary key was gone before the rename: with a (very short) TTL it
                // already expired, which is the outcome the TTL asks for; without one somebody
                // removed it.
                return match p.ttl_ms {
                    Some(_) => Ok(0),
                    None => Err("the temporary key disappeared before the rename".into()),
                };
            }
            Err(format!("{e:?}"))
        }
        Some(redis::Value::Int(1)) | Some(redis::Value::Okay) => Ok(1),
        Some(redis::Value::SimpleString(s)) if s == "OK" => Ok(1),
        // RENAMENX: the key appeared since the EXISTS check; theirs stays.
        _ => Ok(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> Value {
        Value::Text(s.into())
    }

    #[test]
    fn empty_collections_are_invalid() {
        for data in [
            KeyData::Hash { fields: vec![] },
            KeyData::List { items: vec![] },
            KeyData::Set { members: vec![] },
            KeyData::ZSet { members: vec![] },
            KeyData::Stream { entries: vec![] },
        ] {
            assert!(matches!(
                PutPlan::new("k", &data, None, PutMode::Replace),
                Err(Error::Invalid(_))
            ));
        }
        assert!(
            PutPlan::new(
                "k",
                &KeyData::String { value: t("") },
                None,
                PutMode::Replace
            )
            .is_ok()
        );
    }

    #[test]
    fn bad_ttl_and_score_are_invalid() {
        let s = KeyData::String { value: t("v") };
        assert!(PutPlan::new("k", &s, Some(0), PutMode::Replace).is_err());
        let z = KeyData::ZSet {
            members: vec![(t("m"), "nan".into())],
        };
        assert!(PutPlan::new("k", &z, None, PutMode::Replace).is_err());
    }

    #[test]
    fn big_string_is_chunked_and_first_chunk_is_set() {
        let data = KeyData::String {
            value: Value::Bytes(vec![7u8; STRING_CHUNK * 2 + 5]),
        };
        let p = PutPlan::new("k", &data, None, PutMode::Replace).unwrap();
        let all: Vec<_> = batches(&p.body, b"tmp").collect();
        assert_eq!(all.len(), 3);
        // Last chunk first, so the string is allocated once at full size.
        assert!(all.iter().all(|b| b[0][0] == b"SETRANGE"));
        assert_eq!(all[0][0][3].len(), 5);
        assert_eq!(all[2][0][2], b"0".to_vec());
    }

    #[test]
    fn collections_are_batched() {
        let items: Vec<Value> = (0..2500).map(|i| t(&i.to_string())).collect();
        let p = PutPlan::new("k", &KeyData::List { items }, None, PutMode::Replace).unwrap();
        assert_eq!(batches(&p.body, b"tmp").count(), 3);
        let r = ranges(3, |_| BATCH_BYTES);
        assert_eq!(r.len(), 3);
    }

    #[test]
    fn preview_names_the_rename() {
        let s = KeyData::String { value: t("v") };
        let text = preview_put("k", &s, Some(5000), PutMode::IfAbsent).unwrap();
        assert!(text.contains("PEXPIRE <temp> 5000") && text.ends_with("RENAMENX <temp> k"));
    }

    #[test]
    fn temp_names_follow_the_key() {
        let n = String::from_utf8(temp_name(b"app:user:1", "ID")).unwrap();
        assert_eq!(n, "app:user:1.__dbstudio_tmp.{app:user:1}.ID");
        // An existing tag is kept as is (same slot).
        let n = String::from_utf8(temp_name(b"user:{42}:x", "ID")).unwrap();
        assert_eq!(n, "user:{42}:x.__dbstudio_tmp.ID");
        // No safe tag possible: plain name.
        let n = String::from_utf8(temp_name(b"a}b", "ID")).unwrap();
        assert_eq!(n, "a}b.__dbstudio_tmp.ID");
        assert!(is_temp_key(&temp_name(b"k", "ID")));
        assert!(!is_temp_key(b"normal:key"));
    }

    #[test]
    fn length_check_counts_unique_items() {
        let h = KeyData::Hash {
            fields: vec![(t("a"), t("1")), (t("a"), t("2")), (t("b"), t("3"))],
        };
        let p = PutPlan::new("k", &h, None, PutMode::Replace).unwrap();
        assert_eq!(p.length_check(), Some(("HLEN", 2)));
    }
}
