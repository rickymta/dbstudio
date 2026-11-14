//! Read-only evaluation: watches, Evaluate, breakpoint conditions and `{expr}` of log messages
//! run inside the frame of the dialect (design 2.7) so they never change data (package D).

use std::time::Duration;

use dbstudio_core::debug::{EvalContext, EvalResult, EvalValue};
use dbstudio_core::{
    CollectingSink, Error, Lease, QueryRequest, Result, ScriptDialect, StatementOutcome, TxStatus,
    Value,
};
use tokio::time::Instant;

use crate::dialect::eval_text;
use crate::{DialectKit, StepGate};

/// Rows kept for a read-only evaluation.
pub const EVAL_MAX_ROWS: u64 = 50;

/// Text of a value as the Variables tree shows it (strings quoted and shortened).
pub fn display_value(v: &Value) -> String {
    const MAX: usize = 60;
    let short = |s: &str| {
        if s.chars().count() > MAX {
            format!("{}…", s.chars().take(MAX).collect::<String>())
        } else {
            s.to_string()
        }
    };
    match v {
        Value::Null => "NULL".into(),
        Value::Text(s) => format!("'{}'", short(&s.replace('\'', "''"))),
        Value::Bool(b) => b.to_string(),
        Value::Bytes(b) => format!("<{} bytes>", b.len()),
        Value::Array(a) => format!("[{} items]", a.len()),
        Value::Document(d) => format!("{{{} fields}}", d.len()),
        Value::Date(s)
        | Value::Time(s)
        | Value::DateTime(s)
        | Value::Timestamp(s)
        | Value::Uuid(s)
        | Value::Json(s) => format!("'{}'", short(s)),
        other => short(&value_plain(other)),
    }
}

/// Plain text of a value (no quotes), used to build literals.
pub fn value_plain(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => if *b { "1" } else { "0" }.into(),
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Float(f) => format!("{f:?}"),
        Value::Decimal(s)
        | Value::Text(s)
        | Value::Date(s)
        | Value::Time(s)
        | Value::DateTime(s)
        | Value::Timestamp(s)
        | Value::Uuid(s)
        | Value::Json(s) => s.clone(),
        Value::Bytes(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
        Value::Array(_) | Value::Document(_) | Value::Ext(_) => format!("{v:?}"),
    }
}

/// Truthiness of the first cell of a condition result (SQL `1`/`0`, Redis `nil`/`0`/empty).
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Int(i) => *i != 0,
        Value::UInt(u) => *u != 0,
        Value::Float(f) => *f != 0.0,
        Value::Decimal(s) => s
            .trim()
            .parse::<f64>()
            .map(|f| f != 0.0)
            .unwrap_or(!s.is_empty()),
        Value::Text(s) => {
            let t = s.trim().to_lowercase();
            !(t.is_empty() || t == "0" || t == "nil" || t == "false" || t == "f")
        }
        Value::Array(a) => !a.is_empty(),
        _ => true,
    }
}

/// Text of an [`EvalValue`] for console and log messages.
pub fn eval_display(v: &EvalValue) -> String {
    match v {
        EvalValue::Scalar { value, .. } => display_value(value),
        EvalValue::Rows { result } => format!("{{{} rows}}", result.rows.len()),
        EvalValue::Error { error } => format!(
            "<error: {}>",
            error.get("message").and_then(|m| m.as_str()).unwrap_or("?")
        ),
        EvalValue::Skipped { reason } => format!("<skipped: {reason}>"),
    }
}

/// Outcome of a statement run inside the read-only frame.
#[derive(Debug)]
pub enum Framed {
    /// Evaluation impossible: the reason.
    Skipped(String),
    /// The statement (or the guard) failed.
    Failed(Error),
    /// Results of the statement.
    Done(Vec<dbstudio_core::StatementResult>),
}

/// Evaluates read-only commands on a lease.
#[derive(Clone, Copy)]
pub struct Evaluator<'a> {
    /// Lease of the debugged tab.
    pub lease: &'a dyn Lease,
    /// Host guard.
    pub gate: &'a dyn StepGate,
    /// Dialect kit (frames, condition text).
    pub kit: &'a dyn DialectKit,
    /// Lexer dialect.
    pub dialect: ScriptDialect,
    /// `debugWatchTimeoutMs`.
    pub timeout_ms: u64,
    /// Evaluation is impossible for this reason (Redis `MULTI` open, MySQL counters kept).
    pub skip: Option<&'a str>,
}

impl std::fmt::Debug for Evaluator<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Evaluator")
            .field("dialect", &self.dialect)
            .finish()
    }
}

/// Function (or construct) with side effects that a read-only transaction does not stop:
/// sequences, advisory and named locks, session control, files, other sessions. Evaluations
/// repeat at every stop, so they are refused outright. `None` when the text has none.
pub fn side_effect_call(dialect: ScriptDialect, text: &str) -> Option<String> {
    use dbstudio_sql::lexer::{TokenKind, tokenize};
    let chars: Vec<char> = text.chars().collect();
    let slice = |r: dbstudio_core::TextRange| -> String {
        chars[r.start as usize..r.end as usize].iter().collect()
    };
    let mysql = matches!(dialect, ScriptDialect::MySql { .. });
    let pg = matches!(dialect, ScriptDialect::Postgres);
    if !mysql && !pg {
        return None;
    }
    const PG: &[&str] = &[
        "nextval",
        "setval",
        "set_config",
        "pg_terminate_backend",
        "pg_cancel_backend",
        "pg_reload_conf",
        "pg_rotate_logfile",
        "pg_switch_wal",
        "pg_create_restore_point",
        "pg_notify",
        "dblink_exec",
        "dblink",
        "dblink_connect",
        "lo_export",
        "lo_import",
        "lo_unlink",
        "lo_create",
        "lo_from_bytea",
        "lowrite",
        "pg_logical_emit_message",
    ];
    const MYSQL: &[&str] = &[
        "get_lock",
        "release_lock",
        "release_all_locks",
        "load_file",
        "outfile",
        "dumpfile",
        "kill",
    ];
    for t in tokenize(dialect, text) {
        match t.kind {
            TokenKind::Word => {
                let w = slice(t.range).to_ascii_lowercase();
                let bad = if pg {
                    PG.contains(&w.as_str())
                        || w.starts_with("pg_advisory_")
                        || w.starts_with("pg_try_advisory_")
                } else {
                    MYSQL.contains(&w.as_str())
                };
                if bad {
                    return Some(w);
                }
            }
            // `@x := value` assigns a user variable.
            TokenKind::Punct if mysql && slice(t.range) == ":=" => return Some(":=".into()),
            _ => {}
        }
    }
    None
}

pub(crate) fn error_json(e: &Error) -> serde_json::Value {
    serde_json::to_value(e)
        .unwrap_or_else(|_| serde_json::json!({"kind": "other", "message": e.to_string()}))
}

impl Evaluator<'_> {
    async fn run(&self, text: &str) -> Result<Vec<dbstudio_core::StatementResult>> {
        let mut req = QueryRequest::new(text);
        req.max_rows = Some(EVAL_MAX_ROWS);
        req.timeout_ms = Some(self.timeout_ms.max(1));
        let qid = req.query_id;
        let mut sink = CollectingSink::new();
        let res = {
            let exec = self.lease.execute(req, &mut sink);
            tokio::pin!(exec);
            let deadline = Duration::from_millis(self.timeout_ms.max(1));
            tokio::select! {
                r = &mut exec => r,
                () = tokio::time::sleep(deadline) => {
                    let _ = self.lease.cancel(qid).await;
                    (&mut exec).await.and(Err(Error::Timeout))
                }
            }
        };
        match res {
            Ok(_) => Ok(sink.into_results()),
            Err(Error::Cancelled) => Err(Error::Timeout),
            Err(e) => Err(e),
        }
    }

    /// Run the read statement `text` inside the read-only frame; the closing statements of the
    /// frame run even when the statement failed.
    pub async fn framed(&self, text: &str) -> Framed {
        self.framed_with(text, false).await
    }

    /// `trusted`: the text is a probe of the dialect kit (a constant, e.g. Redis `CLIENT INFO`
    /// that the read-only guard of the host does not know as a read): no guard, no denylist.
    async fn framed_with(&self, text: &str, trusted: bool) -> Framed {
        if let Some(reason) = self.skip {
            return Framed::Skipped(reason.to_string());
        }
        let tx = self.lease.state().transaction;
        if tx == TxStatus::Unknown {
            // Wrapping blindly could roll back a transaction nobody knows about.
            return Framed::Skipped(
                "the transaction state is unknown: nothing is evaluated until it is known".into(),
            );
        }
        let frame = self.kit.eval_frame(tx);
        if let Some(reason) = frame.skip {
            return Framed::Skipped(reason);
        }
        if let Some(name) = side_effect_call(self.dialect, text).filter(|_| !trusted) {
            return Framed::Failed(Error::ReadOnlyViolation {
                message: format!("`{name}` has side effects and is not evaluated by the debugger"),
                position: None,
            });
        }
        let gated = if trusted {
            crate::GatedText {
                text: text.to_string(),
                map: dbstudio_sql::OffsetMap::identity(),
                analysis: dbstudio_sql::analyze(self.dialect, text),
            }
        } else {
            match self.gate.check_read_only(self.dialect, text) {
                Ok(g) => g,
                Err(e) => return Framed::Failed(e),
            }
        };
        let mut outcome: Result<Vec<dbstudio_core::StatementResult>> = Ok(vec![]);
        let mut opened = 0;
        for before in &frame.before {
            match self.run(before).await {
                Ok(_) => opened += 1,
                Err(e) => {
                    outcome = Err(e);
                    break;
                }
            }
        }
        if outcome.is_ok() {
            outcome = self.run(&gated.text).await;
        }
        // The closing statements undo what the opening ones did: when none of them worked
        // there is nothing to undo (a ROLLBACK would hit the user's own transaction).
        if opened > 0 {
            for after in &frame.after {
                let _ = self.run(after).await;
            }
        }
        match outcome {
            Ok(r) => Framed::Done(r),
            Err(e) => Framed::Failed(e),
        }
    }

    /// Evaluate the command `text` (already a full read statement) in the read-only frame.
    pub async fn eval_text(&self, text: &str) -> EvalResult {
        let started = Instant::now();
        let value = match self.framed(text).await {
            Framed::Skipped(reason) => EvalValue::Skipped { reason },
            Framed::Failed(e) => EvalValue::Error {
                error: error_json(&e),
            },
            Framed::Done(results) => to_value(results),
        };
        EvalResult {
            value,
            context: EvalContext::Lease,
            elapsed_ms: started.elapsed().as_millis() as u64,
        }
    }

    /// Rows of the last result set of the read statement `text` (probes).
    pub async fn rows(&self, text: &str) -> std::result::Result<dbstudio_core::ResultSet, String> {
        self.rows_with(text, false).await
    }

    /// Rows of a probe of the dialect kit (constant text, not checked by the host guard).
    pub async fn probe_rows(
        &self,
        text: &str,
    ) -> std::result::Result<dbstudio_core::ResultSet, String> {
        self.rows_with(text, true).await
    }

    async fn rows_with(
        &self,
        text: &str,
        trusted: bool,
    ) -> std::result::Result<dbstudio_core::ResultSet, String> {
        match self.framed_with(text, trusted).await {
            Framed::Skipped(r) => Err(r),
            Framed::Failed(e) => Err(e.message()),
            Framed::Done(results) => match results.into_iter().next_back().map(|r| r.outcome) {
                Some(StatementOutcome::Rows(rs)) => Ok(rs),
                _ => Err("no rows".into()),
            },
        }
    }

    /// Evaluate a user expression (`SELECT <expr>` where the dialect needs it).
    pub async fn eval_expr(&self, expr: &str) -> EvalResult {
        self.eval_text(&eval_text(self.dialect, expr)).await
    }

    /// Condition of a breakpoint: `Ok(true)` stops. A failing condition is an error (the caller
    /// stops and says why, like JetBrains).
    pub async fn condition(&self, cond: &str) -> Result<bool> {
        let r = self.eval_text(&self.kit.condition_query(cond)).await;
        match r.value {
            EvalValue::Scalar { value, .. } => Ok(truthy(&value)),
            EvalValue::Rows { result } => Ok(result
                .rows
                .first()
                .and_then(|r| r.first())
                .is_some_and(truthy)),
            EvalValue::Skipped { reason } => Err(Error::Busy(reason)),
            EvalValue::Error { error } => Err(Error::query(
                error
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("condition failed")
                    .to_string(),
            )),
        }
    }

    /// Expand the `{expr}` placeholders of a log message (`{{` and `}}` are literal braces).
    pub async fn render(&self, template: &str) -> String {
        let chars: Vec<char> = template.chars().collect();
        let mut out = String::new();
        let mut i = 0;
        while i < chars.len() {
            match chars[i] {
                '{' if chars.get(i + 1) == Some(&'{') => {
                    out.push('{');
                    i += 2;
                }
                '}' if chars.get(i + 1) == Some(&'}') => {
                    out.push('}');
                    i += 2;
                }
                '{' => {
                    let rest = &chars[i + 1..];
                    if let Some(close) = rest.iter().position(|c| *c == '}') {
                        let expr: String = rest[..close].iter().collect();
                        let r = self.eval_expr(expr.trim()).await;
                        out.push_str(&eval_display(&r.value));
                        i += close + 2;
                    } else {
                        out.push('{');
                        i += 1;
                    }
                }
                c => {
                    out.push(c);
                    i += 1;
                }
            }
        }
        out
    }
}

fn to_value(results: Vec<dbstudio_core::StatementResult>) -> EvalValue {
    let Some(last) = results.into_iter().next_back() else {
        return EvalValue::Skipped {
            reason: "no result".into(),
        };
    };
    match last.outcome {
        StatementOutcome::Rows(rs) => {
            if rs.columns.len() == 1 && rs.rows.len() == 1 && !rs.truncated {
                EvalValue::Scalar {
                    value: rs.rows[0][0].clone(),
                    type_name: rs.columns[0].type_name.clone(),
                }
            } else {
                EvalValue::Rows { result: rs }
            }
        }
        StatementOutcome::Affected { rows } => EvalValue::Scalar {
            value: Value::Int(rows as i64),
            type_name: "affected".into(),
        },
        StatementOutcome::Done => EvalValue::Scalar {
            value: Value::Text("OK".into()),
            type_name: "done".into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn side_effect_calls_are_found_outside_strings_and_comments() {
        let pg = ScriptDialect::Postgres;
        assert_eq!(
            side_effect_call(pg, "SELECT nextval('s')"),
            Some("nextval".into())
        );
        assert_eq!(
            side_effect_call(pg, "select pg_advisory_lock(1)"),
            Some("pg_advisory_lock".into())
        );
        assert_eq!(
            side_effect_call(pg, "SELECT PG_TERMINATE_BACKEND(5)"),
            Some("pg_terminate_backend".into())
        );
        assert_eq!(side_effect_call(pg, "SELECT 'nextval', 1 -- dblink"), None);
        assert_eq!(side_effect_call(pg, "SELECT a + 1 FROM t"), None);
        let my = ScriptDialect::MySql {
            ansi_quotes: false,
            backslash_escapes: true,
            mariadb: false,
        };
        assert_eq!(
            side_effect_call(my, "SELECT GET_LOCK('a', 1)"),
            Some("get_lock".into())
        );
        assert_eq!(side_effect_call(my, "SELECT @x := 3"), Some(":=".into()));
        assert_eq!(
            side_effect_call(my, "SELECT 1 INTO OUTFILE '/tmp/x'"),
            Some("outfile".into())
        );
        assert_eq!(
            side_effect_call(my, "SELECT a FROM t WHERE b = 'get_lock'"),
            None
        );
        assert_eq!(
            side_effect_call(ScriptDialect::Oracle, "SELECT nextval FROM dual"),
            None
        );
    }

    #[test]
    fn display_and_truthy() {
        assert_eq!(display_value(&Value::Text("it's".into())), "'it''s'");
        assert_eq!(display_value(&Value::Null), "NULL");
        assert_eq!(display_value(&Value::Int(5)), "5");
        assert!(truthy(&Value::Int(1)) && !truthy(&Value::Int(0)));
        assert!(!truthy(&Value::Null) && !truthy(&Value::Text("nil".into())));
        assert!(truthy(&Value::Text("x".into())) && truthy(&Value::Bool(true)));
    }
}
