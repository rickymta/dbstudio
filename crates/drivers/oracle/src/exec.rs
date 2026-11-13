//! Running scripts: statement by statement on one connection, streaming results to an
//! [`Emitter`]. Used by the session (pooled connection) and by leases (dedicated connection).

use std::time::Instant;

use dbstudio_core::{
    EndOutcome, Error, ExecSummary, Result, StatementEnd, StatementStart, TextRange, Value,
};
use oracle::sql_type::OracleType;
use oracle::{Connection, ResultSet, Row};

use crate::error::{map_error, map_statement_error};
use crate::runtime::{Control, Emitter, Event};
use crate::script::{self, Stmt, StmtKind};
use crate::value::{ColPlan, decode, plan_column, unsupported_type_hint};

/// Parameters of one script run.
#[derive(Debug, Clone)]
pub struct ExecParams {
    pub text: String,
    /// Most rows of one result set (`None` = all).
    pub max_rows: Option<u64>,
    pub batch_size: usize,
    /// Commit after every statement that is not a transaction control statement.
    pub auto_commit: bool,
    pub read_only: bool,
    pub max_lob: u64,
    /// Keep the last result open when it hits `max_rows` and more rows exist.
    pub keep_open: bool,
}

/// A result set that stays open between calls (`Lease::fetch_more`).
pub struct Pending {
    pub rs: ResultSet<'static, Row>,
    pub plans: Vec<ColPlan>,
    /// Row already read to find out that more exist.
    pub peeked: Option<Vec<Value>>,
    pub max_lob: u64,
}

impl std::fmt::Debug for Pending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pending")
            .field("columns", &self.plans.len())
            .finish()
    }
}

/// Result of [`run_script`].
#[derive(Debug)]
pub struct ScriptOutcome {
    pub summary: ExecSummary,
    pub pending: Option<Pending>,
}

fn char_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Statements that a read-only connection may run.
pub fn read_only_violation(stmt: &Stmt) -> Option<String> {
    let words = script::leading_words(&stmt.text, 3);
    let first = words.first().map(String::as_str).unwrap_or("");
    let allowed = match stmt.kind {
        StmtKind::Ignored(_) => true,
        StmtKind::Exec | StmtKind::PlSql => false,
        StmtKind::Sql => match first {
            "SELECT" | "WITH" | "EXPLAIN" | "COMMIT" | "ROLLBACK" | "SAVEPOINT" | "(" => true,
            "SET" => matches!(words.get(1).map(String::as_str), Some("TRANSACTION")),
            "ALTER" => matches!(words.get(1).map(String::as_str), Some("SESSION")),
            _ => false,
        },
    };
    (!allowed).then(|| {
        format!(
            "{} is not allowed on a read-only connection",
            if first.is_empty() {
                "this statement"
            } else {
                first
            }
        )
    })
}

/// Is this statement transaction control (never auto-committed)?
fn is_tx_control(words: &[String]) -> bool {
    match words.first().map(String::as_str) {
        Some("COMMIT" | "ROLLBACK" | "SAVEPOINT") => true,
        Some("SET") => words.get(1).map(String::as_str) == Some("TRANSACTION"),
        _ => false,
    }
}

/// Run the statements of `p.text` one after the other.
pub fn run_script(
    conn: &Connection,
    p: &ExecParams,
    em: &Emitter,
    ctl: &Control,
) -> Result<ScriptOutcome> {
    let started = Instant::now();
    let stmts = script::split(&p.text);
    let total = stmts.len();
    let mut done = 0u32;
    let mut pending = None;
    for (idx, st) in stmts.iter().enumerate() {
        if ctl.is_cancelled() {
            return Err(Error::Cancelled);
        }
        em.send(Event::Start(StatementStart {
            index: char_u32(idx),
            range: TextRange {
                start: char_u32(st.start),
                end: char_u32(st.end),
            },
        }))?;
        if p.read_only
            && let Some(message) = read_only_violation(st)
        {
            return Err(Error::ReadOnlyViolation {
                message,
                position: Some(char_u32(st.start)),
            });
        }
        let t0 = Instant::now();
        let last = idx + 1 == total;
        let (outcome, keep) = run_statement(conn, p, st, last && p.keep_open, em, ctl)?;
        em.send(Event::End(StatementEnd {
            outcome,
            elapsed_ms: u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX),
        }))?;
        pending = keep;
        done += 1;
    }
    Ok(ScriptOutcome {
        summary: ExecSummary {
            statements: done,
            elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        },
        pending,
    })
}

/// Run one statement; returns how it ended and, for the last query of a lease, the open result.
fn run_statement(
    conn: &Connection,
    p: &ExecParams,
    st: &Stmt,
    keep_open: bool,
    em: &Emitter,
    ctl: &Control,
) -> Result<(EndOutcome, Option<Pending>)> {
    let (sql, plsql_like, position_start) = match &st.kind {
        StmtKind::Ignored(what) => {
            em.send(Event::Message(format!(
                "Ignored: {what} is a SQL*Plus command and has no effect here."
            )))?;
            return Ok((EndOutcome::Done, None));
        }
        StmtKind::Exec => (
            format!("BEGIN {}; END;", st.text.trim().trim_end_matches(';')),
            true,
            None,
        ),
        StmtKind::PlSql => (st.text.clone(), true, Some(st.start)),
        StmtKind::Sql => (st.text.clone(), false, Some(st.start)),
    };
    let fail = |e: oracle::Error| -> Error {
        let mapped = match position_start {
            Some(start) => map_statement_error(e, char_u32(start), &sql),
            None => map_error(e),
        };
        match mapped {
            Error::Query { message, position } => {
                let message = match unsupported_type_hint(&message) {
                    Some(h) => format!("{message}\n{h}"),
                    None => message,
                };
                Error::Query { message, position }
            }
            other => other,
        }
    };
    let words = script::leading_words(&sql, 3);
    let mut builder = conn.statement(&sql);
    builder.lob_locator();
    builder.fetch_array_size(u32::try_from(p.batch_size.clamp(1, 5000)).unwrap_or(500));
    let mut stmt = builder.build().map_err(fail)?;

    if stmt.is_query() {
        let for_update = script::contains_words(&sql, "FOR", "UPDATE");
        let mut rs = stmt.into_result_set::<Row>(&[]).map_err(fail)?;
        let (plans, metas): (Vec<_>, Vec<_>) = rs.column_info().iter().map(plan_column).unzip();
        em.send(Event::Columns(metas))?;
        let outcome = stream_rows(
            &mut rs,
            &plans,
            p.max_rows,
            p.batch_size,
            p.max_lob,
            None,
            em,
            ctl,
        )
        .map_err(|e| match e {
            StreamError::Oracle(e) => fail(e),
            StreamError::Other(e) => e,
        })?;
        let (row_count, truncated, peeked) = outcome;
        let keep = (keep_open && truncated).then_some(Pending {
            rs,
            plans,
            peeked,
            max_lob: p.max_lob,
        });
        if p.auto_commit && for_update && keep.is_none() {
            conn.commit().map_err(map_error)?;
        }
        return Ok((
            EndOutcome::Rows {
                row_count,
                truncated,
            },
            keep,
        ));
    }

    let dml = stmt.is_dml();
    let ddl = stmt.is_ddl();
    stmt.execute(&[]).map_err(fail)?;
    let affected = if dml || stmt.is_returning() {
        stmt.row_count().unwrap_or(0)
    } else {
        0
    };
    // Result sets returned by `DBMS_SQL.RETURN_RESULT`.
    if plsql_like {
        emit_implicit_results(&stmt, p, em, ctl)?;
    }
    drop(stmt);
    if p.auto_commit && !is_tx_control(&words) && !ddl {
        conn.commit().map_err(map_error)?;
    }
    if plsql_like || dml || is_call(&words) {
        for line in drain_output(conn)? {
            em.send(Event::Message(line))?;
        }
    }
    if ddl {
        for line in compile_errors(conn, &sql)? {
            em.send(Event::Message(line))?;
        }
    }
    let outcome = if dml || (affected > 0 && stmt_type_is_dml(&words)) {
        EndOutcome::Affected { rows: affected }
    } else {
        EndOutcome::Done
    };
    Ok((outcome, None))
}

fn stmt_type_is_dml(words: &[String]) -> bool {
    matches!(
        words.first().map(String::as_str),
        Some("INSERT" | "UPDATE" | "DELETE" | "MERGE")
    )
}

fn is_call(words: &[String]) -> bool {
    words.first().map(String::as_str) == Some("CALL")
}

enum StreamError {
    Oracle(oracle::Error),
    Other(Error),
}

impl From<Error> for StreamError {
    fn from(e: Error) -> Self {
        StreamError::Other(e)
    }
}

/// Decode a row.
fn decode_row(row: &Row, plans: &[ColPlan], max_lob: u64) -> Result<Vec<Value>> {
    row.sql_values()
        .iter()
        .zip(plans)
        .map(|(sv, plan)| decode(sv, *plan, max_lob))
        .collect()
}

/// Read rows of `rs` in batches of `batch` until `max_rows` (then peek one more row).
/// Returns (rows sent, truncated, the peeked row).
#[allow(clippy::too_many_arguments)]
fn stream_rows(
    rs: &mut ResultSet<'_, Row>,
    plans: &[ColPlan],
    max_rows: Option<u64>,
    batch: usize,
    max_lob: u64,
    first: Option<Vec<Value>>,
    em: &Emitter,
    ctl: &Control,
) -> std::result::Result<(u64, bool, Option<Vec<Value>>), StreamError> {
    let batch = batch.max(1);
    let mut buf: Vec<Vec<Value>> = Vec::with_capacity(batch.min(1024));
    let mut count = 0u64;
    if let Some(row) = first {
        buf.push(row);
        count += 1;
    }
    let mut truncated = false;
    let mut peeked = None;
    loop {
        if let Some(max) = max_rows
            && count >= max
        {
            // One more row tells whether the result is cut.
            match rs.next() {
                Some(Ok(row)) => {
                    truncated = true;
                    peeked = Some(decode_row(&row, plans, max_lob)?);
                }
                Some(Err(e)) => return Err(StreamError::Oracle(e)),
                None => {}
            }
            break;
        }
        match rs.next() {
            Some(Ok(row)) => {
                buf.push(decode_row(&row, plans, max_lob)?);
                count += 1;
                if buf.len() >= batch {
                    if ctl.is_cancelled() {
                        return Err(Error::Cancelled.into());
                    }
                    em.send(Event::Rows(std::mem::take(&mut buf)))?;
                }
            }
            Some(Err(e)) => return Err(StreamError::Oracle(e)),
            None => break,
        }
    }
    if !buf.is_empty() {
        em.send(Event::Rows(buf))?;
    }
    Ok((count, truncated, peeked))
}

/// Continue a kept result: send up to `max_rows` more rows.
pub fn fetch_pending(
    pending: &mut Pending,
    max_rows: u64,
    batch: usize,
    em: &Emitter,
    ctl: &Control,
) -> Result<(u64, bool)> {
    let first = pending.peeked.take();
    let plans = pending.plans.clone();
    let max_lob = pending.max_lob;
    let (count, truncated, peeked) = stream_rows(
        &mut pending.rs,
        &plans,
        Some(max_rows.max(1)),
        batch,
        max_lob,
        first,
        em,
        ctl,
    )
    .map_err(|e| match e {
        StreamError::Oracle(e) => map_error(e),
        StreamError::Other(e) => e,
    })?;
    pending.peeked = peeked;
    Ok((count, truncated))
}

fn emit_implicit_results(
    stmt: &oracle::Statement,
    p: &ExecParams,
    em: &Emitter,
    ctl: &Control,
) -> Result<()> {
    loop {
        let cursor = match stmt.implicit_result() {
            Ok(Some(c)) => c,
            // Older servers / clients have no implicit results.
            Ok(None) | Err(_) => return Ok(()),
        };
        let mut cursor = cursor;
        let mut rs = cursor.query().map_err(map_error)?;
        let (plans, metas): (Vec<_>, Vec<_>) = rs.column_info().iter().map(plan_column).unzip();
        em.send(Event::Columns(metas))?;
        let (row_count, truncated, _) = stream_rows(
            &mut rs,
            &plans,
            p.max_rows,
            p.batch_size,
            p.max_lob,
            None,
            em,
            ctl,
        )
        .map_err(|e| match e {
            StreamError::Oracle(e) => map_error(e),
            StreamError::Other(e) => e,
        })?;
        em.send(Event::End(StatementEnd {
            outcome: EndOutcome::Rows {
                row_count,
                truncated,
            },
            elapsed_ms: 0,
        }))?;
    }
}

const DRAIN_OUTPUT: &str = "\
DECLARE
  l_lines DBMS_OUTPUT.CHARARR;
  l_n INTEGER;
  l_text VARCHAR2(32767);
BEGIN
  DBMS_LOB.CREATETEMPORARY(:out, TRUE);
  LOOP
    l_n := 1000;
    DBMS_OUTPUT.GET_LINES(l_lines, l_n);
    EXIT WHEN l_n = 0;
    FOR i IN 1 .. l_n LOOP
      l_text := l_lines(i);
      IF l_text IS NOT NULL THEN
        DBMS_LOB.WRITEAPPEND(:out, LENGTH(l_text), l_text);
      END IF;
      DBMS_LOB.WRITEAPPEND(:out, 1, CHR(10));
    END LOOP;
    EXIT WHEN l_n < 1000;
  END LOOP;
END;";

/// Lines written by `DBMS_OUTPUT.PUT_LINE` since the last call.
pub fn drain_output(conn: &Connection) -> Result<Vec<String>> {
    use std::io::Read;
    // The output travels in a temporary CLOB, so the variable must be a LOB locator.
    let mut builder = conn.statement(DRAIN_OUTPUT);
    builder.lob_locator();
    let mut stmt = builder.build().map_err(map_error)?;
    stmt.bind("out", &OracleType::CLOB).map_err(map_error)?;
    stmt.execute(&[]).map_err(map_error)?;
    let clob: Option<oracle::sql_type::Clob> = stmt.bind_value("out").map_err(map_error)?;
    let Some(mut clob) = clob else {
        return Ok(Vec::new());
    };
    let mut text = String::new();
    clob.read_to_string(&mut text).map_err(Error::from)?;
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let body = text.strip_suffix('\n').unwrap_or(&text);
    Ok(body.split('\n').map(str::to_string).collect())
}

/// `(type, owner, name)` created or altered by a PL/SQL object statement.
pub fn plsql_object_of(sql: &str) -> Option<(String, Option<String>, String)> {
    let chars: Vec<char> = sql.chars().collect();
    let tokens = script::tokenize(&chars);
    let sig: Vec<&script::Token> = tokens
        .iter()
        .filter(|t| !matches!(t.tok, script::Tok::Space | script::Tok::Comment))
        .collect();
    let word = |t: &script::Token| -> String { chars[t.start..t.end].iter().collect::<String>() };
    let first = word(sig.first()?).to_ascii_uppercase();
    if first != "CREATE" && first != "ALTER" {
        return None;
    }
    let mut i = 1;
    while i < sig.len() {
        let w = word(sig[i]).to_ascii_uppercase();
        if matches!(
            w.as_str(),
            "OR" | "REPLACE" | "EDITIONABLE" | "NONEDITIONABLE" | "AND" | "COMPILE" | "RESOLVE"
        ) {
            i += 1;
        } else {
            break;
        }
    }
    let kind = word(sig.get(i)?).to_ascii_uppercase();
    let mut kind_name = match kind.as_str() {
        "PROCEDURE" | "FUNCTION" | "TRIGGER" | "PACKAGE" | "TYPE" => kind.clone(),
        _ => return None,
    };
    i += 1;
    if matches!(kind.as_str(), "PACKAGE" | "TYPE")
        && sig.get(i).map(|t| word(t).to_ascii_uppercase()).as_deref() == Some("BODY")
    {
        kind_name.push_str(" BODY");
        i += 1;
    }
    let ident = |t: &script::Token| -> Option<String> {
        match t.tok {
            script::Tok::Word => Some(word(t).to_ascii_uppercase()),
            script::Tok::Quoted => {
                let w = word(t);
                Some(w.trim_matches('"').to_string())
            }
            _ => None,
        }
    };
    let first_part = ident(sig.get(i)?)?;
    if sig
        .get(i + 1)
        .is_some_and(|t| t.tok == script::Tok::Punct && chars[t.start] == '.')
        && let Some(second) = sig.get(i + 2).and_then(|t| ident(t))
    {
        return Some((kind_name, Some(first_part), second));
    }
    Some((kind_name, None, first_part))
}

/// Compilation errors of the PL/SQL object created by `sql`, as readable lines.
pub fn compile_errors(conn: &Connection, sql: &str) -> Result<Vec<String>> {
    let Some((kind, owner, name)) = plsql_object_of(sql) else {
        return Ok(Vec::new());
    };
    let rows = conn
        .query(
            "SELECT line, position, text, attribute FROM all_errors \
             WHERE owner = NVL(:1, SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA')) \
               AND name = :2 AND type = :3 ORDER BY sequence",
            &[&owner, &name, &kind],
        )
        .map_err(map_error)?;
    let mut out = Vec::new();
    for row in rows {
        let row = row.map_err(map_error)?;
        let line: i64 = row.get(0).map_err(map_error)?;
        let col: i64 = row.get(1).map_err(map_error)?;
        let text: String = row.get(2).map_err(map_error)?;
        let attribute: Option<String> = row.get(3).map_err(map_error)?;
        let level = if attribute.as_deref() == Some("WARNING") {
            "Warning"
        } else {
            "Error"
        };
        out.push(format!("{level} at line {line}, column {col}: {text}"));
    }
    if !out.is_empty() {
        out.insert(
            0,
            format!("{kind} {name} was created with compilation errors:"),
        );
    }
    Ok(out)
}

/// Run one query generated by the driver (never split: its text may hold a user `WHERE`
/// clause) and end the statement. Used by `fetch_table` and `stream_table`.
pub fn run_single_query(
    conn: &Connection,
    sql: &str,
    p: &ExecParams,
    em: &Emitter,
    ctl: &Control,
) -> Result<()> {
    let st = Stmt {
        start: 0,
        end: sql.chars().count(),
        kind: StmtKind::Sql,
        text: sql.to_string(),
    };
    let t0 = Instant::now();
    let (outcome, _) = run_statement(conn, p, &st, false, em, ctl)?;
    em.send(Event::End(StatementEnd {
        outcome,
        elapsed_ms: u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stmt(text: &str) -> Stmt {
        script::split(text).into_iter().next().unwrap()
    }

    #[test]
    fn read_only_allows_only_reads() {
        for ok in [
            "select 1 from dual",
            "WITH x AS (SELECT 1 a FROM dual) SELECT * FROM x",
            "explain plan for select 1 from dual",
            "commit",
            "rollback",
            "alter session set current_schema = hr",
            "set transaction read only",
            "SET SERVEROUTPUT ON",
        ] {
            assert!(read_only_violation(&stmt(ok)).is_none(), "{ok}");
        }
        for bad in [
            "insert into t values (1)",
            "update t set a = 1",
            "delete from t",
            "drop table t",
            "create table t (a number)",
            "begin null; end;",
            "call p()",
            "truncate table t",
            "merge into t using dual on (1=1) when matched then update set a = 1",
            "alter table t add b number",
            "exec p",
        ] {
            assert!(read_only_violation(&stmt(bad)).is_some(), "{bad}");
        }
    }

    #[test]
    fn tx_control_detection() {
        let w = |s: &str| script::leading_words(s, 3);
        assert!(is_tx_control(&w("COMMIT")));
        assert!(is_tx_control(&w("set transaction read only")));
        assert!(!is_tx_control(&w("set role x")));
        assert!(!is_tx_control(&w("update t set a=1")));
    }

    #[test]
    fn plsql_object_names_are_extracted() {
        assert_eq!(
            plsql_object_of("CREATE OR REPLACE PROCEDURE hr.p1 IS BEGIN NULL; END;"),
            Some(("PROCEDURE".into(), Some("HR".into()), "P1".into()))
        );
        assert_eq!(
            plsql_object_of("create or replace editionable package body \"MixedCase\" as end;"),
            Some(("PACKAGE BODY".into(), None, "MixedCase".into()))
        );
        assert_eq!(
            plsql_object_of("CREATE TRIGGER trg BEFORE INSERT ON t BEGIN NULL; END;"),
            Some(("TRIGGER".into(), None, "TRG".into()))
        );
        assert_eq!(plsql_object_of("CREATE TABLE t (a number)"), None);
        assert_eq!(plsql_object_of("select 1 from dual"), None);
        assert_eq!(
            plsql_object_of("ALTER PROCEDURE p COMPILE"),
            Some(("PROCEDURE".into(), None, "P".into()))
        );
    }
}
