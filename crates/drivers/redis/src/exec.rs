//! Console execution: one command per line, results streamed to a [`ResultSink`].

use std::time::{Duration, Instant};

use dbstudio_core::{
    ColumnMeta, EndOutcome, Error, ExecSummary, QueryRequest, Result, ResultSink, StatementEnd,
    StatementStart, TextRange, Value, ValueKind,
};
use redis::aio::MultiplexedConnection;

use crate::cli::{Line, script_lines, tokenize_line};
use crate::conn::Shared;
use crate::convert::{reply_text, reply_value};
use crate::error::map_err;

/// Pseudo command used by the `keys_delete_pattern` job: `DBSTUDIO.DELPATTERN <glob>` scans
/// with `SCAN MATCH` and `UNLINK`s in batches, reporting `deleted=<n>` messages.
pub const DELETE_PATTERN: &str = "DBSTUDIO.DELPATTERN";
const DELETE_BATCH: usize = 500;

/// Commands that make no sense on a request/response console.
const STREAMING: &[&str] = &["SUBSCRIBE", "PSUBSCRIBE", "SSUBSCRIBE", "MONITOR"];
/// Commands that can write or reconfigure without carrying the `write` flag.
const ALWAYS_WRITE: &[&str] = &[
    "EVAL",
    "EVALSHA",
    "FCALL",
    "FUNCTION",
    "SCRIPT",
    "SHUTDOWN",
    "DEBUG",
    "REPLICAOF",
    "SLAVEOF",
    "FAILOVER",
    "SAVE",
    "BGSAVE",
    "BGREWRITEAOF",
    "MODULE",
    "SWAPDB",
    "MIGRATE",
    "FLUSHDB",
    "FLUSHALL",
    DELETE_PATTERN,
];

fn is_write_subcommand(name: &str, sub: &str) -> bool {
    let sub = sub.to_ascii_uppercase();
    match name {
        "CONFIG" => matches!(sub.as_str(), "SET" | "REWRITE" | "RESETSTAT"),
        "ACL" => matches!(sub.as_str(), "SETUSER" | "DELUSER" | "SAVE" | "LOAD"),
        "CLIENT" => matches!(sub.as_str(), "KILL" | "PAUSE" | "UNPAUSE" | "NO-EVICT"),
        _ => false,
    }
}

/// Query ids that may run the reserved `DBSTUDIO.*` pseudo commands (set by in-process callers
/// such as the delete-by-pattern job; a console script cannot guess a random query id).
fn internal_ids() -> &'static std::sync::Mutex<std::collections::HashSet<uuid::Uuid>> {
    static IDS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<uuid::Uuid>>> =
        std::sync::OnceLock::new();
    IDS.get_or_init(Default::default)
}

/// Allows `DBSTUDIO.*` pseudo commands for requests with `query_id` until the guard is dropped.
#[derive(Debug)]
pub struct InternalGrant(uuid::Uuid);

impl Drop for InternalGrant {
    fn drop(&mut self) {
        internal_ids()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.0);
    }
}

/// Authorize the reserved pseudo commands for one request (see [`InternalGrant`]).
pub fn authorize_internal(query_id: uuid::Uuid) -> InternalGrant {
    internal_ids()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(query_id);
    InternalGrant(query_id)
}

fn is_internal(query_id: uuid::Uuid) -> bool {
    internal_ids()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&query_id)
}

/// Flags from one `COMMAND INFO` reply (`None` = unknown command).
fn parse_flags(info: redis::Value) -> Option<Vec<String>> {
    match info {
        redis::Value::Array(items) => match items.into_iter().next() {
            Some(redis::Value::Array(entry)) => Some(
                entry
                    .into_iter()
                    .nth(2)
                    .map(|f| match f {
                        redis::Value::Array(flags) | redis::Value::Set(flags) => flags
                            .iter()
                            .map(|x| reply_text(x).to_ascii_lowercase())
                            .collect(),
                        _ => Vec::new(),
                    })
                    .unwrap_or_default(),
            ),
            _ => None,
        },
        _ => None,
    }
}

impl Shared {
    /// Flags of `name` (`cmd` or `cmd|sub`) from `COMMAND INFO`; `Ok(None)` = unknown command.
    /// Asked on the shared connection (never inside the user's `MULTI`); only answers are cached,
    /// a transient or ACL error is returned and retried next time.
    async fn command_flags(&self, name: &str) -> Result<Option<Vec<String>>> {
        let key = name.to_ascii_lowercase();
        if let Some(hit) = self
            .cmd_flags
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
        {
            return Ok(hit.clone());
        }
        let info: redis::Value = self
            .run(self.default_db, redis::cmd("COMMAND").arg("INFO").arg(&key))
            .await?;
        let flags = parse_flags(info);
        self.cmd_flags
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, flags.clone());
        Ok(flags)
    }

    /// Defence in depth for read-only connections: the command layer already classifies
    /// statements, the driver refuses anything that is not verified to be a read.
    async fn ensure_allowed(&self, name: &str, tokens: &[Vec<u8>], position: u32) -> Result<()> {
        if !self.read_only {
            return Ok(());
        }
        let upper = name.to_ascii_uppercase();
        let sub = tokens
            .get(1)
            .map(|t| String::from_utf8_lossy(t).to_ascii_lowercase())
            .unwrap_or_default();
        let deny = |why: &str| Error::ReadOnlyViolation {
            message: format!("{upper} {why} on a read-only connection"),
            position: Some(position),
        };
        if ALWAYS_WRITE.contains(&upper.as_str()) || is_write_subcommand(&upper, &sub) {
            return Err(deny("is not allowed"));
        }
        let unverified = |_: Error| deny("cannot be verified");
        match self.command_flags(&upper).await.map_err(unverified)? {
            None => Err(deny("is unknown and cannot be verified")),
            Some(f) if f.iter().any(|x| x == "write") => Err(deny("is not allowed")),
            Some(f) if !f.is_empty() => Ok(()),
            // A container command (Redis 7 / Valkey 8: XINFO, OBJECT, CONFIG...) has no flags of
            // its own: judge its sub-command, and refuse when that cannot be verified.
            Some(_) => {
                if sub.is_empty() {
                    return Err(deny("cannot be verified"));
                }
                let sub_flags = self
                    .command_flags(&format!("{upper}|{sub}"))
                    .await
                    .map_err(unverified)?;
                match sub_flags {
                    Some(f) if !f.is_empty() && !f.iter().any(|x| x == "write") => Ok(()),
                    _ => Err(deny("cannot be verified or is not allowed")),
                }
            }
        }
    }
}

fn column(name: &str, kind: ValueKind) -> ColumnMeta {
    ColumnMeta {
        name: name.into(),
        type_name: "redis".into(),
        value_kind: kind,
        nullable: Some(true),
        origin: None,
    }
}

fn kind_of(v: &Value) -> ValueKind {
    match v {
        Value::Int(_) => ValueKind::Int,
        Value::Null => ValueKind::Null,
        Value::Array(_) => ValueKind::Array,
        Value::Bytes(_) => ValueKind::Bytes,
        _ => ValueKind::Text,
    }
}

/// Stream one reply: bulk/simple -> one `value` row; array -> `#`, `value`; nil -> NULL.
fn emit_reply(
    sink: &mut dyn ResultSink,
    reply: redis::Value,
    max_rows: Option<u64>,
    batch: usize,
    elapsed_ms: u64,
) -> Result<()> {
    let (columns, rows): (Vec<ColumnMeta>, Vec<Vec<Value>>) = match reply {
        redis::Value::Array(items) | redis::Value::Set(items) => (
            vec![
                column("#", ValueKind::Int),
                column("value", ValueKind::Text),
            ],
            items
                .into_iter()
                .enumerate()
                .map(|(i, v)| vec![Value::Int(i as i64), reply_value(v)])
                .collect(),
        ),
        redis::Value::Map(pairs) => (
            vec![
                column("key", ValueKind::Text),
                column("value", ValueKind::Text),
            ],
            pairs
                .into_iter()
                .map(|(k, v)| vec![reply_value(k), reply_value(v)])
                .collect(),
        ),
        other => {
            let v = reply_value(other);
            (vec![column("value", kind_of(&v))], vec![vec![v]])
        }
    };
    let total = rows.len() as u64;
    let limit = max_rows.unwrap_or(u64::MAX);
    let truncated = total > limit;
    let mut rows = rows;
    rows.truncate(limit.min(total) as usize);
    let count = rows.len() as u64;
    sink.on_columns(columns)?;
    let mut it = rows.into_iter().peekable();
    while it.peek().is_some() {
        let chunk: Vec<Vec<Value>> = it.by_ref().take(batch.max(1)).collect();
        sink.on_rows(chunk)?;
    }
    sink.on_statement_end(StatementEnd {
        outcome: EndOutcome::Rows {
            row_count: count,
            truncated,
        },
        elapsed_ms,
    })
}

/// Commands that need one connection across several lines: not possible on a cluster console.
const CLUSTER_UNSUPPORTED: &[&str] = &["MULTI", "EXEC", "DISCARD", "WATCH", "UNWATCH"];

/// Hook called after each successful command with its upper-case name and arguments.
pub type Hook<'a> = &'a mut (dyn FnMut(&str, &[Vec<u8>]) + Send);

/// Run every line of `req.text` on `conn`; stops at the first error.
pub async fn run_script(
    shared: &Shared,
    conn: &mut MultiplexedConnection,
    req: &QueryRequest,
    sink: &mut dyn ResultSink,
    hook: Option<Hook<'_>>,
) -> Result<ExecSummary> {
    let started = Instant::now();
    let batch = req.batch_size.unwrap_or(1000) as usize;
    let mut hook = hook;
    let lines: Vec<Line<'_>> = script_lines(&req.text);
    let mut executed = 0u32;
    for (index, line) in lines.iter().enumerate() {
        let tokens = tokenize_line(line.text).map_err(|message| Error::Query {
            message,
            position: Some(line.start),
        })?;
        let Some(first) = tokens.first() else {
            continue;
        };
        let name = String::from_utf8_lossy(first).to_ascii_uppercase();
        if STREAMING.contains(&name.as_str()) {
            return Err(Error::Unsupported(format!(
                "{name} is not supported in the console"
            )));
        }
        // `DBSTUDIO.*` is reserved for in-process callers: typing it must not bypass the
        // command layer's confirmations.
        if name.starts_with("DBSTUDIO.") && !(name == DELETE_PATTERN && is_internal(req.query_id)) {
            return Err(Error::Permission(format!("{name} is a reserved command")));
        }
        shared.ensure_allowed(&name, &tokens, line.start).await?;
        sink.on_statement_start(StatementStart {
            index: index as u32,
            range: TextRange {
                start: line.start,
                end: line.end,
            },
        })?;
        let t0 = Instant::now();
        if shared.is_cluster() && CLUSTER_UNSUPPORTED.contains(&name.as_str()) {
            return Err(Error::Unsupported(format!(
                "{name} is not available on a Redis Cluster in the console: each line may run on \
                 another primary, so a transaction cannot span lines"
            )));
        }
        if name == DELETE_PATTERN {
            let pattern = tokens
                .get(1)
                .ok_or_else(|| Error::Invalid(format!("{DELETE_PATTERN} needs a pattern")))?;
            // In a cluster every primary holds part of the keys: delete on each of them.
            let deleted = if shared.is_cluster() {
                let mut total = 0;
                for mut node in shared.primary_conns().await? {
                    total += delete_pattern(&mut node, pattern, sink).await?;
                }
                total
            } else {
                delete_pattern(conn, pattern, sink).await?
            };
            sink.on_statement_end(StatementEnd {
                outcome: EndOutcome::Affected { rows: deleted },
                elapsed_ms: t0.elapsed().as_millis() as u64,
            })?;
            executed += 1;
            continue;
        }
        let mut cmd = redis::cmd(&name);
        for a in &tokens[1..] {
            cmd.arg(a.as_slice());
        }
        let remaining = req
            .timeout_ms
            .map(|t| Duration::from_millis(t).saturating_sub(started.elapsed()));
        // A cluster routes each command to the primary owning its key (and answers SCAN,
        // DBSIZE, INFO keyspace for the whole cluster); a single server uses `conn`.
        let call = async {
            if shared.is_cluster() {
                shared.run::<redis::Value>(0, &cmd).await
            } else {
                cmd.query_async::<redis::Value>(conn).await.map_err(map_err)
            }
        };
        let reply = match remaining {
            Some(d) => tokio::time::timeout(d, call)
                .await
                .map_err(|_| Error::Timeout)?,
            None => call.await,
        };
        let reply = reply.map_err(|e| match e {
            Error::Query { message, .. } => Error::Query {
                message,
                position: Some(line.start),
            },
            other => other,
        })?;
        if let Some(h) = hook.as_mut() {
            h(&name, &tokens[1..]);
        }
        emit_reply(
            sink,
            reply,
            req.max_rows,
            batch,
            t0.elapsed().as_millis() as u64,
        )?;
        executed += 1;
    }
    Ok(ExecSummary {
        statements: executed,
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

/// `SCAN MATCH pattern` + `UNLINK` in batches; reports progress through `sink.on_message`
/// (`deleted=<n> scanned=<rounds>`), where a sink error (cancellation) stops the loop.
async fn delete_pattern(
    conn: &mut MultiplexedConnection,
    pattern: &[u8],
    sink: &mut dyn ResultSink,
) -> Result<u64> {
    let mut cursor = 0u64;
    let mut deleted = 0u64;
    let mut rounds = 0u64;
    let mut buffer: Vec<Vec<u8>> = Vec::new();
    loop {
        let (next, keys): (String, Vec<Vec<u8>>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(pattern)
            .arg("COUNT")
            .arg(1000)
            .query_async(conn)
            .await
            .map_err(map_err)?;
        cursor = next.parse().unwrap_or(0);
        rounds += 1;
        buffer.extend(keys);
        while buffer.len() >= DELETE_BATCH || (cursor == 0 && !buffer.is_empty()) {
            let n = buffer.len().min(DELETE_BATCH);
            let chunk: Vec<Vec<u8>> = buffer.drain(..n).collect();
            let mut unlink = redis::cmd("UNLINK");
            for k in &chunk {
                unlink.arg(k.as_slice());
            }
            let n: u64 = unlink.query_async(conn).await.map_err(map_err)?;
            deleted += n;
        }
        sink.on_message(format!("deleted={deleted} scanned={rounds}"))?;
        if cursor == 0 {
            return Ok(deleted);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_core::CollectingSink;

    #[test]
    fn replies_become_rows() {
        let mut sink = CollectingSink::new();
        emit_reply(
            &mut sink,
            redis::Value::Array(vec![
                redis::Value::BulkString(b"a".to_vec()),
                redis::Value::Nil,
            ]),
            Some(1),
            10,
            1,
        )
        .unwrap();
        let results = sink.into_results();
        match &results[0].outcome {
            dbstudio_core::StatementOutcome::Rows(rs) => {
                assert_eq!(rs.columns.len(), 2);
                assert_eq!(rs.rows.len(), 1);
                assert!(rs.truncated);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn scalar_reply_is_one_value_row() {
        let mut sink = CollectingSink::new();
        emit_reply(&mut sink, redis::Value::Int(7), None, 10, 0).unwrap();
        match &sink.into_results()[0].outcome {
            dbstudio_core::StatementOutcome::Rows(rs) => {
                assert_eq!(rs.rows, vec![vec![Value::Int(7)]]);
                assert_eq!(rs.columns[0].name, "value");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn subcommand_writes() {
        assert!(is_write_subcommand("CONFIG", "set"));
        assert!(!is_write_subcommand("CONFIG", "get"));
        assert!(!is_write_subcommand("GET", "x"));
    }
}
