//! PostgreSQL driver: EXPLAIN [ANALYZE] (FORMAT JSON).
//!
//! `ANALYZE` really executes the statement, so it always runs inside a transaction that is
//! rolled back afterwards (also for plain SELECTs), which keeps data unchanged for
//! INSERT/UPDATE/DELETE. Side effects outside the transaction (sequences, foreign calls) stay;
//! the UI warns before analysing a write.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dbstudio_core::{Error, ExplainRequest, PlanFormat, RawPlan, Result};
use tokio_postgres::SimpleQueryMessage;

use crate::conn::PooledConn;
use crate::error::map_error;
use crate::session::PostgresSession;

/// Explain one statement.
pub(crate) async fn explain(session: &PostgresSession, req: ExplainRequest) -> Result<RawPlan> {
    let statement = req.text.trim().trim_end_matches(';').trim_end().to_string();
    if statement.is_empty() {
        return Err(Error::Query {
            message: "nothing to explain".into(),
            position: None,
        });
    }
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
    let work = run(&mut conn, &req, &statement);
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

async fn run(conn: &mut PooledConn, req: &ExplainRequest, statement: &str) -> Result<RawPlan> {
    let (version_num, version) = server_version(conn).await?;
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
    let prefix = format!("EXPLAIN ({}) ", options.join(", "));
    let sql = format!("{prefix}{statement}");

    if req.analyze {
        conn.client
            .batch_execute("BEGIN")
            .await
            .map_err(map_error)?;
        conn.in_txn = true;
        if let Some(ms) = req.timeout_ms {
            conn.client
                .batch_execute(&format!("SET LOCAL statement_timeout = {ms}"))
                .await
                .map_err(map_error)?;
        }
    }
    let text = match fetch_text(conn, &sql).await {
        Ok(t) => t,
        Err(e) => return Err(shift_position(e, prefix.len())),
    };
    if req.analyze {
        conn.client
            .batch_execute("ROLLBACK")
            .await
            .map_err(map_error)?;
        conn.in_txn = false;
    }
    Ok(RawPlan {
        format: PlanFormat::PostgresJson,
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
async fn server_version(conn: &PooledConn) -> Result<(u32, String)> {
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

/// Run `sql` and return the single text cell (the JSON document).
async fn fetch_text(conn: &PooledConn, sql: &str) -> Result<String> {
    let rows = conn.client.simple_query(sql).await.map_err(map_error)?;
    for m in rows {
        if let SimpleQueryMessage::Row(r) = m {
            if let Some(t) = r.get(0) {
                return Ok(t.to_string());
            }
        }
    }
    Err(Error::Other("EXPLAIN returned no plan".into()))
}

/// Error positions refer to the prefixed text; shift them back to the user's statement.
fn shift_position(e: Error, prefix: usize) -> Error {
    match e {
        Error::Query {
            message,
            position: Some(p),
        } => Error::Query {
            message,
            position: p.checked_sub(prefix as u32),
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
            shift_position(q(Some(40)), 30),
            Error::Query {
                position: Some(10),
                ..
            }
        ));
        assert!(matches!(
            shift_position(q(Some(5)), 30),
            Error::Query { position: None, .. }
        ));
    }
}
