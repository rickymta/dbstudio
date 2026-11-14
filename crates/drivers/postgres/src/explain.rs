//! PostgreSQL driver: EXPLAIN [ANALYZE] (FORMAT JSON).
//!
//! `ANALYZE` really executes the statement, so it always runs inside a transaction that is
//! rolled back afterwards (also for plain SELECTs), which keeps data unchanged for
//! INSERT/UPDATE/DELETE. Side effects outside the transaction (sequences, foreign calls) stay;
//! the UI warns before analysing a write.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dbstudio_core::{
    Error, ExplainRequest, PlanFormat, RawPlan, Result, ScriptDialect, StatementClass,
};
use tokio_postgres::SimpleQueryMessage;
use tokio_postgres::types::{FromSql, Type};

use crate::conn::Conn;
use crate::error::map_error;
use crate::flavor::Flavor;
use crate::session::PostgresSession;

const SAVEPOINT: &str = "SAVEPOINT dbstudio_explain";
const UNDO_SAVEPOINT: &str =
    "ROLLBACK TO SAVEPOINT dbstudio_explain; RELEASE SAVEPOINT dbstudio_explain";

/// Explain one statement.
pub(crate) async fn explain(session: &PostgresSession, req: ExplainRequest) -> Result<RawPlan> {
    let (statement, offset) = single_statement(&req.text)?;
    if req.analyze && !analyzable(&statement) {
        return Err(Error::Unsupported("explain_analyze_ddl".into()));
    }
    let db = req
        .database
        .as_deref()
        .filter(|d| !d.is_empty())
        .unwrap_or(&session.shared.login_db)
        .to_string();
    let mut conn = session.shared.acquire(&db).await?;
    // Registered under the query id, so `Session::cancel` reaches this statement.
    let (running, _unregister) = session.register(Some(req.query_id), &conn);
    let token = conn.cancel.clone();
    let work = run(
        &mut conn,
        &req,
        &statement,
        offset,
        false,
        session.shared.flavor(),
    );
    let result = match req.timeout_ms {
        Some(ms) => match tokio::time::timeout(Duration::from_millis(ms), work).await {
            Ok(r) => r,
            Err(_) => {
                // Stop the statement on the server; the connection is not reused.
                let _ = running.send_cancel(&session.shared, &token).await;
                return Err(Error::Timeout);
            }
        },
        None => work.await,
    };
    // A cancelled statement fails with a server error; report it as a cancellation.
    let result = match result {
        Err(Error::Query { .. }) if running.is_cancelled() => Err(Error::Cancelled),
        other => other,
    };
    // A failed or cancelled ANALYZE leaves its transaction open: `end_request` rolls back
    // through `finish` (and drops connections that were cancelled).
    let clean = matches!(&result, Ok(_) | Err(Error::Query { .. } | Error::Cancelled));
    session.end_request(&mut conn, &running, clean).await;
    result
}

/// Explain one statement on a dedicated (lease) connection. Inside a transaction block the work
/// runs under a savepoint that is always rolled back, so the transaction keeps everything that
/// came before and never ends up aborted by the explain itself; outside one, `ANALYZE` uses its
/// own `BEGIN` .. `ROLLBACK`. Timeout and cancellation are the caller's job.
pub(crate) async fn explain_on_lease(
    conn: &mut Conn,
    req: &ExplainRequest,
    flavor: Flavor,
) -> Result<RawPlan> {
    let (statement, offset) = single_statement(&req.text)?;
    if req.analyze && !analyzable(&statement) {
        return Err(Error::Unsupported("explain_analyze_ddl".into()));
    }
    let nested = conn.in_txn;
    let result = run(conn, req, &statement, offset, nested, flavor).await;
    if result.is_err() && !nested && conn.in_txn {
        // A failed ANALYZE left its own `BEGIN` open: it is not the user's transaction.
        let _ = conn.client.batch_execute("ROLLBACK").await;
        conn.in_txn = false;
        conn.failed = false;
    }
    result
}

/// Run the explain. `nested`: the connection is inside the user's transaction block, so a
/// savepoint stands in for `BEGIN` / `ROLLBACK`.
async fn run(
    conn: &mut Conn,
    req: &ExplainRequest,
    statement: &str,
    offset: u32,
    nested: bool,
    flavor: Flavor,
) -> Result<RawPlan> {
    let cockroach = flavor.is_cockroach();
    let (version_num, version) = server_version(conn, flavor).await?;
    let prefix = if cockroach {
        // CockroachDB has no JSON plan: the verbose text tree is the plan.
        if req.analyze {
            "EXPLAIN ANALYZE (VERBOSE) ".to_string()
        } else {
            "EXPLAIN (VERBOSE) ".to_string()
        }
    } else {
        let mut options = vec!["FORMAT JSON".to_string()];
        if req.analyze {
            options.push("ANALYZE".into());
            if req.options.buffers {
                options.push("BUFFERS".into());
            }
        }
        if req.options.verbose {
            options.push("VERBOSE".into());
        }
        if req.options.settings && version_num >= 120_000 {
            options.push("SETTINGS".into());
        }
        format!("EXPLAIN ({}) ", options.join(", "))
    };
    let sql = format!("{prefix}{statement}");

    if nested {
        conn.client
            .batch_execute(SAVEPOINT)
            .await
            .map_err(map_error)?;
    } else if req.analyze {
        conn.client
            .batch_execute("BEGIN")
            .await
            .map_err(map_error)?;
        conn.in_txn = true;
    }
    if req.analyze {
        if let Some(ms) = req.timeout_ms {
            // `SET LOCAL` is undone together with the savepoint / transaction.
            conn.client
                .batch_execute(&format!(
                    "SET LOCAL statement_timeout = {}",
                    ms.min(i32::MAX as u64)
                ))
                .await
                .map_err(map_error)?;
        }
    }
    let fetched = if cockroach {
        fetch_lines(conn, &sql).await
    } else {
        fetch_text(conn, &sql).await
    };
    if nested {
        // Also after an error: the savepoint rollback revives a transaction the failed
        // statement aborted.
        let undo = conn.client.batch_execute(UNDO_SAVEPOINT).await;
        if fetched.is_ok() {
            undo.map_err(map_error)?;
        } else if undo.is_err() {
            conn.failed = true;
        }
    }
    let text = match fetched {
        Ok(t) => t,
        Err(e) => return Err(shift_position(e, prefix.len(), offset)),
    };
    if req.analyze && !nested {
        conn.client
            .batch_execute("ROLLBACK")
            .await
            .map_err(map_error)?;
        conn.in_txn = false;
    }
    Ok(RawPlan {
        format: if cockroach {
            PlanFormat::CockroachText
        } else {
            PlanFormat::PostgresJson
        },
        text,
        statement: statement.to_string(),
        analyzed: req.analyze,
        rolled_back: req.analyze,
        server_version: version,
        captured_at: rfc3339_now(),
        extra: Default::default(),
    })
}

/// `(server_version_num, short server_version)`.
async fn server_version(conn: &Conn, flavor: Flavor) -> Result<(u32, String)> {
    if flavor.is_cockroach() {
        // `server_version` of CockroachDB is the PostgreSQL version it imitates.
        let rows = conn
            .client
            .simple_query("SELECT version()")
            .await
            .map_err(map_error)?;
        for m in rows {
            if let SimpleQueryMessage::Row(r) = m {
                let full = r.get(0).unwrap_or("");
                let short = full
                    .split(" v")
                    .nth(1)
                    .and_then(|v| v.split_whitespace().next())
                    .unwrap_or("")
                    .to_string();
                return Ok((0, short));
            }
        }
        return Ok((0, String::new()));
    }
    let rows = conn
        .client
        .simple_query(
            "SELECT current_setting('server_version_num'), current_setting('server_version')",
        )
        .await
        .map_err(map_error)?;
    for m in rows {
        if let SimpleQueryMessage::Row(r) = m {
            let num = r.get(0).and_then(|v| v.parse().ok()).unwrap_or(0);
            let full = r.get(1).unwrap_or("").to_string();
            let short = full.split_whitespace().next().unwrap_or("").to_string();
            return Ok((num, short));
        }
    }
    Ok((0, String::new()))
}

/// The JSON document of an `EXPLAIN (FORMAT JSON)` result, read as text.
struct PlanJson(String);

impl<'a> FromSql<'a> for PlanJson {
    fn from_sql(
        _ty: &Type,
        raw: &'a [u8],
    ) -> std::result::Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(PlanJson(std::str::from_utf8(raw)?.to_string()))
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::JSON
    }
}

/// Run `sql` and return the single JSON cell. The statement goes through the extended
/// protocol (one Parse message), so the server itself refuses a text that holds several
/// statements: nothing can ride along behind the statement that is being explained.
async fn fetch_text(conn: &Conn, sql: &str) -> Result<String> {
    let stmt = conn.client.prepare(sql).await.map_err(map_error)?;
    let rows = conn.client.query(&stmt, &[]).await.map_err(map_error)?;
    let row = rows
        .first()
        .ok_or_else(|| Error::Other("EXPLAIN returned no plan".into()))?;
    Ok(row.try_get::<_, PlanJson>(0).map_err(map_error)?.0)
}

/// Run a CockroachDB EXPLAIN (one text column `info`, one row per line) and join the lines.
async fn fetch_lines(conn: &Conn, sql: &str) -> Result<String> {
    // Extended protocol: the server itself refuses several statements in one text.
    let stmt = conn.client.prepare(sql).await.map_err(map_error)?;
    let rows = conn.client.query(&stmt, &[]).await.map_err(map_error)?;
    let mut lines = Vec::with_capacity(rows.len());
    for row in &rows {
        lines.push(row.try_get::<_, String>(0).map_err(map_error)?);
    }
    if lines.is_empty() {
        return Err(Error::Other("EXPLAIN returned no plan".into()));
    }
    Ok(lines.join("\n"))
}

/// Exactly one statement of `text`: its text and the character offset where it starts.
fn single_statement(text: &str) -> Result<(String, u32)> {
    let analysis = dbstudio_sql::analyze(ScriptDialect::Postgres, text);
    if let Some(p) = analysis.problems.first() {
        return Err(Error::Query {
            message: format!("cannot explain: {}", p.message),
            position: Some(p.range.start),
        });
    }
    let stmts: Vec<_> = analysis
        .statements
        .iter()
        .filter(|s| s.class != StatementClass::Empty)
        .collect();
    match stmts.as_slice() {
        [] => Err(Error::Query {
            message: "nothing to explain".into(),
            position: None,
        }),
        [one] => {
            let body: String = text
                .chars()
                .skip(one.range.start as usize)
                .take((one.range.end - one.range.start) as usize)
                .collect();
            Ok((body, one.range.start))
        }
        [_, second, ..] => Err(Error::Query {
            message: format!("EXPLAIN takes exactly one statement, found {}", stmts.len()),
            position: Some(second.range.start),
        }),
    }
}

/// Error positions refer to the prefixed text; map them back to the user's original text.
fn shift_position(e: Error, prefix: usize, offset: u32) -> Error {
    match e {
        Error::Query {
            message,
            position: Some(p),
        } => Error::Query {
            message,
            position: p.checked_sub(prefix as u32).map(|q| q + offset),
        },
        other => other,
    }
}

/// Statements EXPLAIN ANALYZE can run: queries and DML (and the CREATE ... AS forms).
fn analyzable(statement: &str) -> bool {
    let words = leading_words(statement, 3);
    match words.first().map(String::as_str) {
        Some(
            "SELECT" | "WITH" | "VALUES" | "TABLE" | "INSERT" | "UPDATE" | "DELETE" | "MERGE"
            | "EXECUTE" | "DECLARE",
        ) => true,
        Some("CREATE") => {
            let w1 = words.get(1).map(String::as_str);
            let w2 = words.get(2).map(String::as_str);
            matches!(
                (w1, w2),
                (Some("TABLE"), _) | (Some("MATERIALIZED"), Some("VIEW"))
            )
        }
        _ => false,
    }
}

/// First `n` words (upper case), skipping whitespace, comments and opening parentheses.
fn leading_words(text: &str, n: usize) -> Vec<String> {
    let b = text.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() && out.len() < n {
        match b[i] {
            c if c.is_ascii_whitespace() || c == b'(' => i += 1,
            b'-' if b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            }
            c if c.is_ascii_alphabetic() => {
                let s = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
                out.push(text[s..i].to_ascii_uppercase());
            }
            _ => break,
        }
    }
    out
}

/// Current time as RFC 3339 UTC (second precision).
fn rfc3339_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    rfc3339(secs)
}

fn rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_known_dates() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_709_210_096), "2024-02-29T12:34:56Z");
        assert_eq!(rfc3339(1_735_689_599), "2024-12-31T23:59:59Z");
    }

    #[test]
    fn analyzable_statements() {
        for ok in [
            "select 1",
            "  /* c */ (select 1)",
            "-- x\nupdate t set a=1",
            "with x as (select 1) select * from x",
            "create table t2 as select 1",
            "CREATE MATERIALIZED VIEW v AS select 1",
            "insert into t values (1)",
        ] {
            assert!(analyzable(ok), "{ok}");
        }
        for no in [
            "create index i on t(a)",
            "drop table t",
            "alter table t add c int",
            "vacuum t",
            "truncate t",
            "",
        ] {
            assert!(!analyzable(no), "{no}");
        }
    }

    #[test]
    fn positions_shift_back() {
        let q = |p| Error::Query {
            message: "m".into(),
            position: p,
        };
        assert!(matches!(
            shift_position(q(Some(40)), 30, 7),
            Error::Query {
                position: Some(17),
                ..
            }
        ));
        assert!(matches!(
            shift_position(q(Some(5)), 30, 7),
            Error::Query { position: None, .. }
        ));
    }

    #[test]
    fn exactly_one_statement() {
        let (stmt, at) = single_statement(
            "  -- c
  SELECT 1;  ",
        )
        .unwrap();
        // The range starts after the leading whitespace and keeps the comment.
        assert_eq!((stmt.as_str(), at), ("-- c\n  SELECT 1", 2));
        // A multi-byte character before the statement counts as one position.
        let (body, at) = single_statement("  /* é */ select 2").unwrap();
        assert_eq!((body.as_str(), at), ("/* é */ select 2", 2));
        for bad in [
            "UPDATE t SET a = 2; COMMIT",
            "SELECT 1; DELETE FROM t",
            "select 1; select 2",
        ] {
            let e = single_statement(bad).unwrap_err();
            assert!(
                matches!(&e, Error::Query { message, position: Some(_) } if message.contains("exactly one")),
                "{bad}: {e:?}"
            );
        }
        for empty in ["", "  ;  ", "-- nothing"] {
            assert!(matches!(
                single_statement(empty).unwrap_err(),
                Error::Query { position: None, .. }
            ));
        }
        // Unterminated string: the lexer problem is reported, nothing is sent.
        assert!(single_statement("select 'abc").is_err());
    }
}
