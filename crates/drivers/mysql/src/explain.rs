//! MySQL/MariaDB driver: EXPLAIN [ANALYZE].
//!
//! * MySQL: `EXPLAIN FORMAT=JSON` (plan only) or `EXPLAIN ANALYZE` (8.0.18+, tree text).
//! * MariaDB: `EXPLAIN FORMAT=JSON` or `ANALYZE FORMAT=JSON`.
//!
//! ANALYZE executes the statement. Only read statements are analysed (inside a read-only
//! transaction that is rolled back); write statements answer `Unsupported`, because tables of
//! a non-transactional engine cannot be rolled back.

use std::time::{SystemTime, UNIX_EPOCH};

use dbstudio_core::{Error, ExplainRequest, PlanFormat, RawPlan, Result};
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Row};

use crate::error::map_query_error;
use crate::normalize_version;
use crate::session::{MySqlSession, value_to_string};

/// What a statement does, as far as EXPLAIN ANALYZE is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Read,
    Write,
    /// DDL, SET, CALL, ... : cannot be analysed.
    Other,
}

/// Explain one statement.
pub(crate) async fn explain(session: &MySqlSession, req: ExplainRequest) -> Result<RawPlan> {
    let statement = req.text.trim().trim_end_matches(';').trim_end().to_string();
    if statement.is_empty() {
        return Err(Error::Query {
            message: "nothing to explain".into(),
            position: None,
        });
    }
    if req.analyze {
        match classify(&statement) {
            Kind::Read => {}
            Kind::Write => return Err(Error::Unsupported("explain_analyze_writes".into())),
            Kind::Other => return Err(Error::Unsupported("explain_analyze_ddl".into())),
        }
    }
    let database = req.database.as_deref().filter(|d| !d.is_empty());
    let mut conn = session.inner.acquire_in(database).await?;
    // Registers the statement for `Session::cancel` and arms the timeout (KILL QUERY).
    let guard = session.inner.begin(conn.id(), req.query_id, req.timeout_ms);
    let result = run(session, &mut conn, &req, &statement).await;
    let ctl = guard.end().await;
    let stopped = ctl.stopped();
    let result = ctl.finalize(result);
    let broken = stopped || matches!(&result, Err(Error::Connection(_)));
    session.inner.release(conn, broken).await;
    result
}

async fn run(
    session: &MySqlSession,
    conn: &mut Conn,
    req: &ExplainRequest,
    statement: &str,
) -> Result<RawPlan> {
    let raw_version: String = conn
        .query_first("SELECT VERSION()")
        .await
        .map_err(map_query_error)?
        .unwrap_or_default();
    let mariadb = session.inner.is_mariadb;
    let version = normalize_version(&raw_version);
    if req.analyze && !mariadb && !supports_explain_analyze(&version) {
        return Err(Error::Unsupported("explain_analyze".into()));
    }

    let (sql, format) = match (mariadb, req.analyze) {
        (true, false) => (
            format!("EXPLAIN FORMAT=JSON {statement}"),
            PlanFormat::MariaDbJson,
        ),
        (true, true) => (
            format!("ANALYZE FORMAT=JSON {statement}"),
            PlanFormat::MariaDbJson,
        ),
        (false, false) => (
            format!("EXPLAIN FORMAT=JSON {statement}"),
            PlanFormat::MySqlJson,
        ),
        (false, true) => (
            format!("EXPLAIN ANALYZE {statement}"),
            PlanFormat::MySqlTree,
        ),
    };
    let text = fetch(conn, &sql, req.analyze).await?;
    Ok(RawPlan {
        format,
        text,
        statement: statement.to_string(),
        analyzed: req.analyze,
        rolled_back: req.analyze,
        server_version: version,
        captured_at: rfc3339_now(),
        extra: Default::default(),
    })
}

/// Run the EXPLAIN statement (in a read-only transaction when it executes the query).
async fn fetch(conn: &mut Conn, sql: &str, analyze: bool) -> Result<String> {
    if analyze {
        conn.query_drop("START TRANSACTION READ ONLY")
            .await
            .map_err(map_query_error)?;
    }
    let row: std::result::Result<Option<Row>, _> = conn.query_first(sql).await;
    if analyze {
        // Always end the transaction, whatever happened.
        let _ = conn.query_drop("ROLLBACK").await;
    }
    let row = row.map_err(map_query_error)?;
    row.as_ref()
        .and_then(|r| r.as_ref(0))
        .and_then(value_to_string)
        .ok_or_else(|| Error::Other("EXPLAIN returned no plan".into()))
}

/// MySQL 8.0.18 introduced EXPLAIN ANALYZE.
fn supports_explain_analyze(version: &str) -> bool {
    let nums: Vec<u32> = version
        .split(|c: char| !c.is_ascii_digit())
        .filter(|p| !p.is_empty())
        .take(3)
        .filter_map(|p| p.parse().ok())
        .collect();
    let (major, minor, patch) = (
        nums.first().copied().unwrap_or(0),
        nums.get(1).copied().unwrap_or(0),
        nums.get(2).copied().unwrap_or(0),
    );
    (major, minor, patch) >= (8, 0, 18)
}

/// Words of the statement outside comments, strings and quoted identifiers, upper case,
/// with the parenthesis depth at which each occurs.
fn words(text: &str) -> Vec<(String, u32)> {
    let b = text.as_bytes();
    let mut i = 0;
    let mut depth = 0u32;
    let mut out = Vec::new();
    while i < b.len() {
        match b[i] {
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            b'#' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
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
            q @ (b'\'' | b'"' | b'`') => {
                i += 1;
                while i < b.len() {
                    if b[i] == b'\\' && q != b'`' {
                        i += 2;
                        continue;
                    }
                    if b[i] == q {
                        if b.get(i + 1) == Some(&q) {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                i += 1;
            }
            c if c.is_ascii_alphabetic() || c == b'_' => {
                let s = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
                out.push((text[s..i].to_ascii_uppercase(), depth));
            }
            _ => i += 1,
        }
    }
    out
}

/// Read, write or other: writes include `WITH ... UPDATE`, `INSERT ... SELECT` and
/// `SELECT ... INTO` of any kind.
fn classify(statement: &str) -> Kind {
    let w = words(statement);
    let Some((first, _)) = w.first() else {
        return Kind::Other;
    };
    const WRITES: &[&str] = &["INSERT", "UPDATE", "DELETE", "REPLACE", "LOAD", "TRUNCATE"];
    match first.as_str() {
        "SELECT" | "TABLE" | "VALUES" | "WITH" => {
            // A write keyword at depth 0 after WITH, or an INTO of a SELECT, makes it a write.
            let writes = w.iter().any(|(word, depth)| {
                *depth == 0 && (WRITES.contains(&word.as_str()) || word == "INTO")
            });
            if writes { Kind::Write } else { Kind::Read }
        }
        f if WRITES.contains(&f) => Kind::Write,
        _ => Kind::Other,
    }
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
    fn classification() {
        for (sql, kind) in [
            ("select * from t", Kind::Read),
            ("  /* c */ (select 1) union (select 2)", Kind::Read),
            ("with x as (select 1) select * from x", Kind::Read),
            ("select 'update' as w, `delete` from t", Kind::Read),
            ("update t set a = 1", Kind::Write),
            ("-- x\ndelete from t", Kind::Write),
            ("insert into t select * from u", Kind::Write),
            ("with x as (select 1) update t set a = 1", Kind::Write),
            ("select * into outfile '/tmp/x' from t", Kind::Write),
            ("replace into t values (1)", Kind::Write),
            ("create table t (a int)", Kind::Other),
            ("call p()", Kind::Other),
            ("", Kind::Other),
        ] {
            assert_eq!(classify(sql), kind, "{sql}");
        }
    }

    #[test]
    fn explain_analyze_version_gate() {
        assert!(supports_explain_analyze("8.0.18"));
        assert!(supports_explain_analyze("8.4.11"));
        assert!(supports_explain_analyze("9.0.0"));
        assert!(!supports_explain_analyze("8.0.17"));
        assert!(!supports_explain_analyze("5.7.44"));
        assert!(!supports_explain_analyze(""));
    }

    #[test]
    fn rfc3339_known_dates() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_709_210_096), "2024-02-29T12:34:56Z");
    }
}
