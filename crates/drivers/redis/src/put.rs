//! `KeyChange::Put`: write a whole key in a temporary key, then `RENAME` (or `RENAMENX`) it into
//! place, so a reader never sees a half-written value. Big values go in chunks (strings by
//! byte ranges via `APPEND`, collections by batches bounded in count and size).

use std::ops::Range;

use dbstudio_core::{Error, KeyData, PutMode, Result, StreamEntry, Value, decode_key_name};
use redis::aio::MultiplexedConnection;
use uuid::Uuid;

use crate::cli::render_command;
use crate::convert::value_bytes;
use crate::error::map_err;

type Args = Vec<Vec<u8>>;

/// Bytes of a string written per `APPEND`.
const STRING_CHUNK: usize = 8 * 1024 * 1024;
/// Most items (fields, members, entries) in one batch.
const BATCH_ITEMS: usize = 1000;
/// Rough byte budget of one batch.
const BATCH_BYTES: usize = 4 * 1024 * 1024;
/// Lifetime of an unfinished temporary key (cleans up after a crash), in ms.
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
    Stream(Vec<(String, Vec<(Vec<u8>, Vec<u8>)>)>),
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
        Body::String(v) => Box::new(v.chunks(STRING_CHUNK).enumerate().map(move |(i, c)| {
            let name: &[u8] = if i == 0 { b"SET" } else { b"APPEND" };
            let mut a = head(name, tmp);
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
) -> std::result::Result<(), String> {
    let mut pipe = redis::pipe();
    for c in cmds {
        let cmd = pipe.cmd(&String::from_utf8_lossy(&c[0]));
        for a in &c[1..] {
            cmd.arg(a.as_slice());
        }
    }
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

async fn build(
    conn: &mut MultiplexedConnection,
    tmp: &[u8],
    p: &PutPlan,
) -> std::result::Result<(), String> {
    for (i, cmds) in batches(&p.body, tmp).enumerate() {
        run_pipeline(conn, &cmds).await?;
        if i == 0 {
            // From now on a crash cannot leave the temporary key behind for good.
            simple(
                conn,
                "PEXPIRE",
                &[tmp, TEMP_GUARD_MS.to_string().as_bytes()],
            )
            .await?;
        }
    }
    match p.ttl_ms {
        Some(ms) => simple(conn, "PEXPIRE", &[tmp, ms.to_string().as_bytes()]).await?,
        None => simple(conn, "PERSIST", &[tmp]).await?,
    };
    Ok(())
}

/// Run one `Put`; the result is the affected count (0 = kept an existing key, `IfAbsent`).
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
    let tmp = format!("__dbstudio_put_{}__", Uuid::new_v4().simple()).into_bytes();
    if let Err(e) = build(conn, &tmp, p).await {
        let _ = simple(conn, "DEL", &[&tmp]).await;
        return Err(e);
    }
    let done = match p.mode {
        PutMode::Replace => simple(conn, "RENAME", &[&tmp, &p.key]).await.map(|_| 1),
        PutMode::IfAbsent => match simple(conn, "RENAMENX", &[&tmp, &p.key]).await {
            Ok(redis::Value::Int(1)) => Ok(1),
            Ok(_) => Ok(0),
            Err(e) => Err(e),
        },
    };
    match done {
        Ok(1) => Ok(1),
        Ok(n) => {
            // Another client created the key between EXISTS and RENAMENX: keep theirs.
            let _ = simple(conn, "DEL", &[&tmp]).await;
            Ok(n)
        }
        Err(e) => {
            let _ = simple(conn, "DEL", &[&tmp]).await;
            Err(e)
        }
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
        assert_eq!(all[0][0][0], b"SET");
        assert_eq!(all[1][0][0], b"APPEND");
        assert_eq!(all[2][0][2].len(), 5);
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
}
