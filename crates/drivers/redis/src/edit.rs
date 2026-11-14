//! Applying and previewing `ChangeSet::Keys`.
//!
//! Every change runs as `WATCH keys` + `TYPE` check + `MULTI ... EXEC` on a dedicated connection.
//! Redis cannot roll back, so changes are applied one after the other and the first failure
//! stops the run (`ApplyResult.atomic == false`, `ChangeFailed.applied` = changes already done).

use dbstudio_core::{
    ApplyResult, ChangeSet, Editability, Error, KeyChange, KeyChangeSet, NotEditableReason, Result,
    decode_key_name,
};
use uuid::Uuid;

use crate::cli::{quote_arg, render_command};
use crate::conn::Shared;
use crate::convert::value_bytes;
use crate::error::map_err;
use crate::put::{PutPlan, preview_put, put_key};
use crate::tree::db_of;

type Args = Vec<Vec<u8>>;

/// What one change needs to run.
struct Plan {
    commands: Vec<Args>,
    watch: Vec<Vec<u8>>,
    /// Keys whose `TYPE` must be one of the listed types (or `none` when `allow_missing`).
    expects: Vec<Expect>,
    /// Indexes of commands whose integer reply must be 1 (`(index, message)`).
    must_one: Vec<(usize, &'static str)>,
    /// Index of the command whose integer reply is the affected count (default: 1).
    affected_from: Option<usize>,
    /// `LINDEX key index` must equal `value` (checked under WATCH) before `MULTI`.
    list_check: Option<ListCheck>,
}

struct ListCheck {
    key: Vec<u8>,
    index: i64,
    value: Vec<u8>,
}

/// Message of a failed optimistic check on a list item.
const LIST_CHANGED: &str = "list item changed";

struct Expect {
    key: Vec<u8>,
    types: &'static [&'static str],
    allow_missing: bool,
}

fn cmd(parts: &[&[u8]]) -> Args {
    parts.iter().map(|p| p.to_vec()).collect()
}

fn key_bytes(key: &str) -> Result<Vec<u8>> {
    decode_key_name(key)
}

fn parse_score(text: &str) -> Result<()> {
    let t = text.trim();
    match t.parse::<f64>() {
        Ok(f) if !f.is_nan() => Ok(()),
        _ => Err(Error::Other(format!("invalid score `{text}`"))),
    }
}

fn plan(change: &KeyChange) -> Result<Plan> {
    let mut p = Plan {
        commands: Vec::new(),
        watch: Vec::new(),
        expects: Vec::new(),
        must_one: Vec::new(),
        affected_from: None,
        list_check: None,
    };
    let watch = |p: &mut Plan, k: &[u8]| p.watch.push(k.to_vec());
    match change {
        KeyChange::SetString { key, value, ttl_ms } => {
            if *ttl_ms == Some(0) {
                return Err(Error::Other("the TTL must be at least 1 ms".into()));
            }
            let k = key_bytes(key)?;
            let v = value_bytes(value)?;
            watch(&mut p, &k);
            p.expects.push(Expect {
                key: k.clone(),
                types: &["string"],
                allow_missing: true,
            });
            let mut c = cmd(&[b"SET", &k, &v]);
            match ttl_ms {
                Some(ms) => {
                    c.push(b"PX".to_vec());
                    c.push(ms.to_string().into_bytes());
                }
                None => c.push(b"KEEPTTL".to_vec()),
            }
            p.commands.push(c);
        }
        KeyChange::HashSet { key, entries } => {
            let k = key_bytes(key)?;
            watch(&mut p, &k);
            p.expects.push(Expect {
                key: k.clone(),
                types: &["hash"],
                allow_missing: true,
            });
            let mut c = cmd(&[b"HSET", &k]);
            for (f, v) in entries {
                c.push(value_bytes(f)?);
                c.push(value_bytes(v)?);
            }
            p.commands.push(c);
        }
        KeyChange::HashDelete { key, fields } => {
            let k = key_bytes(key)?;
            watch(&mut p, &k);
            p.expects.push(Expect {
                key: k.clone(),
                types: &["hash"],
                allow_missing: false,
            });
            let mut c = cmd(&[b"HDEL", &k]);
            for f in fields {
                c.push(value_bytes(f)?);
            }
            p.commands.push(c);
            p.affected_from = Some(0);
        }
        KeyChange::ListSet {
            key,
            index,
            value,
            expected,
        } => {
            let k = key_bytes(key)?;
            watch(&mut p, &k);
            if let Some(e) = expected {
                p.list_check = Some(ListCheck {
                    key: k.clone(),
                    index: *index,
                    value: value_bytes(e)?,
                });
            }
            p.expects.push(Expect {
                key: k.clone(),
                types: &["list"],
                allow_missing: false,
            });
            p.commands.push(cmd(&[
                b"LSET",
                &k,
                index.to_string().as_bytes(),
                &value_bytes(value)?,
            ]));
        }
        KeyChange::ListPush { key, values, head } => {
            let k = key_bytes(key)?;
            watch(&mut p, &k);
            p.expects.push(Expect {
                key: k.clone(),
                types: &["list"],
                allow_missing: true,
            });
            let mut c = cmd(&[if *head { b"LPUSH" } else { b"RPUSH" }, &k]);
            for v in values {
                c.push(value_bytes(v)?);
            }
            p.commands.push(c);
        }
        KeyChange::ListRemoveAt {
            key,
            index,
            expected,
        } => {
            let k = key_bytes(key)?;
            watch(&mut p, &k);
            if let Some(e) = expected {
                p.list_check = Some(ListCheck {
                    key: k.clone(),
                    index: *index,
                    value: value_bytes(e)?,
                });
            }
            p.expects.push(Expect {
                key: k.clone(),
                types: &["list"],
                allow_missing: false,
            });
            let sentinel = format!("__dbstudio_remove_{}__", Uuid::new_v4().simple());
            p.commands.push(cmd(&[
                b"LSET",
                &k,
                index.to_string().as_bytes(),
                sentinel.as_bytes(),
            ]));
            p.commands
                .push(cmd(&[b"LREM", &k, b"1", sentinel.as_bytes()]));
        }
        KeyChange::SetAdd { key, members } | KeyChange::SetRemove { key, members } => {
            let add = matches!(change, KeyChange::SetAdd { .. });
            let k = key_bytes(key)?;
            watch(&mut p, &k);
            p.expects.push(Expect {
                key: k.clone(),
                types: &["set"],
                allow_missing: add,
            });
            let mut c = cmd(&[if add { b"SADD" } else { b"SREM" }, &k]);
            for m in members {
                c.push(value_bytes(m)?);
            }
            p.commands.push(c);
            p.affected_from = Some(0);
        }
        KeyChange::ZAdd { key, members } => {
            let k = key_bytes(key)?;
            watch(&mut p, &k);
            p.expects.push(Expect {
                key: k.clone(),
                types: &["zset"],
                allow_missing: true,
            });
            let mut c = cmd(&[b"ZADD", &k]);
            for (m, score) in members {
                parse_score(score)?;
                c.push(score.trim().as_bytes().to_vec());
                c.push(value_bytes(m)?);
            }
            p.commands.push(c);
        }
        KeyChange::ZRemove { key, members } => {
            let k = key_bytes(key)?;
            watch(&mut p, &k);
            p.expects.push(Expect {
                key: k.clone(),
                types: &["zset"],
                allow_missing: false,
            });
            let mut c = cmd(&[b"ZREM", &k]);
            for m in members {
                c.push(value_bytes(m)?);
            }
            p.commands.push(c);
            p.affected_from = Some(0);
        }
        KeyChange::StreamAdd { key, id, fields } => {
            let k = key_bytes(key)?;
            watch(&mut p, &k);
            p.expects.push(Expect {
                key: k.clone(),
                types: &["stream"],
                allow_missing: true,
            });
            let id = id.as_deref().filter(|i| !i.is_empty()).unwrap_or("*");
            let mut c = cmd(&[b"XADD", &k, id.as_bytes()]);
            if fields.is_empty() {
                return Err(Error::Other(
                    "a stream entry needs at least one field".into(),
                ));
            }
            for (f, v) in fields {
                c.push(value_bytes(f)?);
                c.push(value_bytes(v)?);
            }
            p.commands.push(c);
        }
        KeyChange::StreamDelete { key, ids } => {
            let k = key_bytes(key)?;
            watch(&mut p, &k);
            p.expects.push(Expect {
                key: k.clone(),
                types: &["stream"],
                allow_missing: false,
            });
            let mut c = cmd(&[b"XDEL", &k]);
            c.extend(ids.iter().map(|i| i.as_bytes().to_vec()));
            p.commands.push(c);
            p.affected_from = Some(0);
        }
        KeyChange::Expire { key, ttl_ms } => {
            if *ttl_ms == Some(0) {
                // PEXPIRE with 0 ms would delete the key instead of expiring it later.
                return Err(Error::Other("the TTL must be at least 1 ms".into()));
            }
            let k = key_bytes(key)?;
            watch(&mut p, &k);
            p.expects.push(Expect {
                key: k.clone(),
                types: &[],
                allow_missing: false,
            });
            match ttl_ms {
                Some(ms) => p
                    .commands
                    .push(cmd(&[b"PEXPIRE", &k, ms.to_string().as_bytes()])),
                None => p.commands.push(cmd(&[b"PERSIST", &k])),
            }
            // PERSIST answers 0 for a key without TTL: harmless, only PEXPIRE must be 1.
            if ttl_ms.is_some() {
                p.must_one.push((0, "key does not exist"));
            }
        }
        KeyChange::Rename { key, new_key } => {
            let k = key_bytes(key)?;
            let n = key_bytes(new_key)?;
            watch(&mut p, &k);
            watch(&mut p, &n);
            p.expects.push(Expect {
                key: k.clone(),
                types: &[],
                allow_missing: false,
            });
            p.commands.push(cmd(&[b"RENAMENX", &k, &n]));
            p.must_one.push((0, "the new key already exists"));
        }
        KeyChange::Delete { keys } => {
            let mut c = cmd(&[b"DEL"]);
            for key in keys {
                let k = key_bytes(key)?;
                watch(&mut p, &k);
                c.push(k);
            }
            p.commands.push(c);
            p.affected_from = Some(0);
        }
        // `Put` has its own build-then-RENAME path (`put_key`), never a WATCH/MULTI plan.
        KeyChange::Put { .. } => return Err(Error::Unsupported("key_put".into())),
        // `KeyChange` is exhaustive today; a new variant must get a plan before it compiles.
        #[allow(unreachable_patterns)]
        _ => return Err(Error::Unsupported("key_change".into())),
    }
    Ok(p)
}

fn failed(index: usize, message: impl Into<String>, applied: usize) -> Error {
    Error::ChangeFailed {
        index: index as u32,
        message: message.into(),
        applied: applied as u32,
    }
}

impl Shared {
    /// Editability of Redis keys: always by key, never atomic.
    pub fn editability(&self) -> Editability {
        Editability {
            editable: !self.read_only,
            key_columns: Vec::new(),
            read_only_columns: Vec::new(),
            reason: self
                .read_only
                .then_some(NotEditableReason::ReadOnlyConnection),
            atomic: false,
            notes: vec![],
        }
    }

    /// redis-cli lines that `apply_changes` would run (multi-command changes are wrapped in
    /// `MULTI` / `EXEC`).
    pub fn preview(&self, changes: &ChangeSet) -> Result<Vec<String>> {
        let ChangeSet::Keys(set) = changes else {
            return Err(Error::Unsupported("data_editing".into()));
        };
        set.changes
            .iter()
            .map(|c| {
                if let KeyChange::Put {
                    key,
                    data,
                    ttl_ms,
                    mode,
                } = c
                {
                    return preview_put(key, data, *ttl_ms, *mode);
                }
                let p = plan(c)?;
                let lines: Vec<String> = p.commands.iter().map(|a| render_command(a)).collect();
                let body = if lines.len() > 1 {
                    format!("MULTI\n{}\nEXEC", lines.join("\n"))
                } else {
                    lines.join("\n")
                };
                Ok(match &p.list_check {
                    Some(c) => format!(
                        "WATCH {}\n-- abort unless {} = {}\n{body}",
                        quote_arg(&c.key),
                        render_command(&cmd(&[b"LINDEX", &c.key, c.index.to_string().as_bytes()])),
                        quote_arg(&c.value)
                    ),
                    None => body,
                })
            })
            .collect()
    }

    /// Apply a key change set; see the module documentation.
    pub async fn apply(&self, changes: ChangeSet) -> Result<ApplyResult> {
        let ChangeSet::Keys(KeyChangeSet { database, changes }) = changes else {
            return Err(Error::Unsupported("data_editing".into()));
        };
        if self.read_only {
            return Err(Error::ReadOnlyViolation {
                message: "data editing is not allowed on a read-only connection".into(),
                position: None,
            });
        }
        // Validate everything first so a malformed change fails before anything is applied.
        let mut plans = Vec::with_capacity(changes.len());
        for (i, c) in changes.iter().enumerate() {
            plans.push(match c {
                KeyChange::Put {
                    key,
                    data,
                    ttl_ms,
                    mode,
                } => Step::Put(
                    PutPlan::new(key, data, *ttl_ms, *mode)
                        .map_err(|e| failed(i, e.message(), 0))?,
                ),
                other => Step::Plan(plan(other).map_err(|e| failed(i, e.message(), 0))?),
            });
        }
        let db = db_of(&database, self.default_db)?;
        let mut conn = self.dedicated(db).await?;
        let mut affected = Vec::with_capacity(plans.len());
        for (i, step) in plans.iter().enumerate() {
            let outcome = match step {
                Step::Plan(p) => apply_one(&mut conn, p).await,
                Step::Put(p) => put_key(&mut conn, p).await,
            };
            match outcome {
                Ok(n) => affected.push(n),
                Err(message) => return Err(failed(i, message, i)),
            }
        }
        Ok(ApplyResult {
            applied: affected.len() as u32,
            affected,
            atomic: false,
        })
    }
}

enum Step {
    Plan(Plan),
    Put(PutPlan),
}

async fn apply_one(
    conn: &mut redis::aio::MultiplexedConnection,
    p: &Plan,
) -> std::result::Result<u64, String> {
    let q = |e: redis::RedisError| map_err(e).message();
    if !p.watch.is_empty() {
        let mut w = redis::cmd("WATCH");
        for k in &p.watch {
            w.arg(k.as_slice());
        }
        w.query_async::<()>(conn).await.map_err(q)?;
    }
    // Optimistic check of the list item the user saw; WATCH makes a later change abort EXEC.
    if let Some(c) = &p.list_check {
        // Name a deleted or retyped key clearly instead of a raw WRONGTYPE error. (A UI that
        // removes several items must send `ListRemoveAt` by descending index.)
        let ty: String = redis::cmd("TYPE")
            .arg(c.key.as_slice())
            .query_async(conn)
            .await
            .map_err(q)?;
        if ty != "list" {
            let _ = redis::cmd("UNWATCH").query_async::<()>(conn).await;
            let name = String::from_utf8_lossy(&c.key).into_owned();
            return Err(if ty == "none" {
                format!("key `{name}` does not exist")
            } else {
                format!("WRONGTYPE: key `{name}` is a {ty}, expected list")
            });
        }
        let current: Option<Vec<u8>> = redis::cmd("LINDEX")
            .arg(c.key.as_slice())
            .arg(c.index)
            .query_async(conn)
            .await
            .map_err(q)?;
        if current.as_deref() != Some(c.value.as_slice()) {
            let _ = redis::cmd("UNWATCH").query_async::<()>(conn).await;
            return Err(LIST_CHANGED.to_string());
        }
    }
    // Type checks happen under WATCH: a concurrent change invalidates EXEC.
    for e in &p.expects {
        let ty: String = redis::cmd("TYPE")
            .arg(e.key.as_slice())
            .query_async(conn)
            .await
            .map_err(q)?;
        let ok = if ty == "none" {
            e.allow_missing
        } else {
            e.types.is_empty() || e.types.contains(&ty.as_str())
        };
        if !ok {
            let _ = redis::cmd("UNWATCH").query_async::<()>(conn).await;
            let name = String::from_utf8_lossy(&e.key).into_owned();
            return Err(if ty == "none" {
                format!("key `{name}` does not exist")
            } else {
                format!(
                    "WRONGTYPE: key `{name}` is a {ty}, expected {}",
                    e.types.join("/")
                )
            });
        }
    }
    redis::cmd("MULTI")
        .query_async::<()>(conn)
        .await
        .map_err(q)?;
    for c in &p.commands {
        let mut cmd = redis::cmd(&String::from_utf8_lossy(&c[0]));
        for a in &c[1..] {
            cmd.arg(a.as_slice());
        }
        if let Err(e) = cmd.query_async::<redis::Value>(conn).await {
            let _ = redis::cmd("DISCARD").query_async::<()>(conn).await;
            return Err(q(e));
        }
    }
    let exec: redis::Value = redis::cmd("EXEC").query_async(conn).await.map_err(q)?;
    let replies = match exec {
        redis::Value::Nil if p.list_check.is_some() => return Err(LIST_CHANGED.to_string()),
        redis::Value::Nil => {
            return Err(
                "the key changed while the edit was being applied (WATCH aborted it); reload and retry".into(),
            );
        }
        redis::Value::Array(items) => items,
        other => return Err(format!("unexpected EXEC reply: {other:?}")),
    };
    for r in &replies {
        if let redis::Value::ServerError(e) = r {
            return Err(format!("{e:?}"));
        }
    }
    for (idx, message) in &p.must_one {
        if !matches!(replies.get(*idx), Some(redis::Value::Int(1))) {
            return Err((*message).to_string());
        }
    }
    Ok(match p.affected_from.and_then(|i| replies.get(i)) {
        Some(redis::Value::Int(n)) => (*n).max(0) as u64,
        _ => 1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_core::Value;

    fn t(s: &str) -> Value {
        Value::Text(s.into())
    }

    fn preview_of(change: KeyChange) -> String {
        let p = plan(&change).unwrap();
        let lines: Vec<String> = p.commands.iter().map(|a| render_command(a)).collect();
        lines.join(" ; ")
    }

    #[test]
    fn plans_render_as_redis_cli() {
        assert_eq!(
            preview_of(KeyChange::SetString {
                key: "k".into(),
                value: t("a b"),
                ttl_ms: Some(1500)
            }),
            r#"SET k "a b" PX 1500"#
        );
        assert_eq!(
            preview_of(KeyChange::SetString {
                key: "k".into(),
                value: t("v"),
                ttl_ms: None
            }),
            "SET k v KEEPTTL"
        );
        assert_eq!(
            preview_of(KeyChange::ZAdd {
                key: "z".into(),
                members: vec![(t("m"), "1.5".into())]
            }),
            "ZADD z 1.5 m"
        );
        assert_eq!(
            preview_of(KeyChange::Expire {
                key: "k".into(),
                ttl_ms: None
            }),
            "PERSIST k"
        );
        let remove = preview_of(KeyChange::ListRemoveAt {
            key: "l".into(),
            index: 2,
            expected: None,
        });
        assert!(
            remove.starts_with("LSET l 2 __dbstudio_remove_") && remove.contains("; LREM l 1 ")
        );
    }

    #[test]
    fn invalid_input_is_rejected_up_front() {
        assert!(
            plan(&KeyChange::ZAdd {
                key: "z".into(),
                members: vec![(t("m"), "abc".into())]
            })
            .is_err()
        );
        assert!(
            plan(&KeyChange::StreamAdd {
                key: "s".into(),
                id: None,
                fields: vec![]
            })
            .is_err()
        );
        assert!(
            plan(&KeyChange::HashSet {
                key: "h".into(),
                entries: vec![(Value::Array(vec![]), t("v"))]
            })
            .is_err()
        );
        // inf is a valid score.
        assert!(
            plan(&KeyChange::ZAdd {
                key: "z".into(),
                members: vec![(t("m"), "-inf".into())]
            })
            .is_ok()
        );
    }
}
