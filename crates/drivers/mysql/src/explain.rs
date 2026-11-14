//! MySQL/MariaDB driver: EXPLAIN [ANALYZE].
//!
//! * MySQL: `EXPLAIN FORMAT=JSON` (plan only) or `EXPLAIN ANALYZE` (8.0.18+, tree text).
//! * MariaDB: `EXPLAIN FORMAT=JSON` or `ANALYZE FORMAT=JSON`.
//!
//! ANALYZE executes the statement. Only read statements are analysed (inside a read-only
//! transaction that is rolled back); write statements answer `Unsupported`, because tables of
//! a non-transactional engine cannot be rolled back.

use std::time::{SystemTime, UNIX_EPOCH};

use dbstudio_core::{
    Error, ExplainRequest, PlanFormat, RawPlan, Result, ScriptDialect, StatementClass,
};
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
    let database = req.database.as_deref().filter(|d| !d.is_empty());
    let mut conn = session.inner.acquire_in(database).await?;
    // Registers the statement for `Session::cancel` and arms the timeout (KILL QUERY).
    let guard = session.inner.begin(conn.id(), req.query_id, req.timeout_ms);
    let result = run(session, &mut conn, &req).await;
    let ctl = guard.end().await;
    let stopped = ctl.stopped();
    let result = ctl.finalize(result);
    let broken = stopped || matches!(&result, Err(Error::Connection(_)));
    session.inner.release(conn, broken).await;
    result
}

async fn run(session: &MySqlSession, conn: &mut Conn, req: &ExplainRequest) -> Result<RawPlan> {
    run_on(conn, req, session.inner.is_mariadb, false).await
}

/// Explain one statement on a dedicated (lease) connection. With `in_transaction` the user's
/// transaction is open: `START TRANSACTION` would commit it, so the analysed read runs inside it
/// as it is (reads change nothing, there is nothing to roll back).
pub(crate) async fn explain_on_lease(
    conn: &mut Conn,
    req: &ExplainRequest,
    mariadb: bool,
    in_transaction: bool,
) -> Result<RawPlan> {
    run_on(conn, req, mariadb, in_transaction).await
}

async fn run_on(
    conn: &mut Conn,
    req: &ExplainRequest,
    mariadb: bool,
    in_transaction: bool,
) -> Result<RawPlan> {
    let (raw_version, sql_mode): (String, String) = conn
        .query_first("SELECT VERSION(), @@SESSION.sql_mode")
        .await
        .map_err(map_query_error)?
        .unwrap_or_default();
    let version = normalize_version(&raw_version);
    // The dialect follows this very connection's sql_mode (backslash escapes in strings).
    let dialect = ScriptDialect::mysql(
        mariadb,
        !sql_mode
            .to_ascii_uppercase()
            .contains("NO_BACKSLASH_ESCAPES"),
    );
    let (statement, class) = single_statement(dialect, &req.text)?;
    if req.analyze {
        match kind_of(class, &statement) {
            Kind::Read => {}
            Kind::Write => return Err(Error::Unsupported("explain_analyze_writes".into())),
            Kind::Other => return Err(Error::Unsupported("explain_analyze_ddl".into())),
        }
        if !mariadb && !supports_explain_analyze(&version) {
            return Err(Error::Unsupported("explain_analyze".into()));
        }
    }
    let statement = statement.as_str();

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
    let text = fetch(conn, &sql, req.analyze && !in_transaction).await?;
    Ok(RawPlan {
        format,
        text,
        statement: statement.to_string(),
        analyzed: req.analyze,
        rolled_back: req.analyze && !in_transaction,
        server_version: version,
        captured_at: rfc3339_now(),
        extra: Default::default(),
    })
}

/// Exactly one statement of `text` (and its class). The connection has multi-statement mode
/// on, so anything that could hide a second statement is refused here: several statements,
/// lexer problems (unterminated strings/comments) and version-specific comments
/// (`/*! ... */` is executed by MySQL, ignored by every parser).
fn single_statement(dialect: ScriptDialect, text: &str) -> Result<(String, StatementClass)> {
    let analysis = dbstudio_sql::analyze(dialect, text);
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
    let one = match stmts.as_slice() {
        [] => {
            return Err(Error::Query {
                message: "nothing to explain".into(),
                position: None,
            });
        }
        [one] => *one,
        [_, second, ..] => {
            return Err(Error::Query {
                message: format!("EXPLAIN takes exactly one statement, found {}", stmts.len()),
                position: Some(second.range.start),
            });
        }
    };
    let body: String = text
        .chars()
        .skip(one.range.start as usize)
        .take((one.range.end - one.range.start) as usize)
        .collect();
    if let Some(i) = body.find("/*!").or_else(|| body.find("/*M!")) {
        return Err(Error::Query {
            message: "version-specific comments (/*! ... */) are not allowed in EXPLAIN".into(),
            position: Some(one.range.start + body[..i].chars().count() as u32),
        });
    }
    Ok((body, one.class))
}

/// Functions that act on the server outside any transaction.
const IMPURE_CALLS: &[&str] = &["GET_LOCK", "RELEASE_LOCK", "RELEASE_ALL_LOCKS", "LOAD_FILE"];

/// How EXPLAIN ANALYZE must treat a statement: reads only when the parser says so and the
/// text holds no `INTO` (OUTFILE/DUMPFILE/variable) or lock function.
fn kind_of(class: StatementClass, statement: &str) -> Kind {
    match class {
        StatementClass::Read => {
            let w = words(statement);
            let impure = w.iter().any(|(word, depth)| {
                (*depth == 0 && word == "INTO") || IMPURE_CALLS.contains(&word.as_str())
            });
            if impure { Kind::Write } else { Kind::Read }
        }
        StatementClass::Write => Kind::Write,
        _ => Kind::Other,
    }
}

/// Run the EXPLAIN statement as a *prepared* statement: `COM_STMT_PREPARE` takes exactly one
/// statement, so even with multi-statement mode on nothing can follow it. In a read-only
/// transaction when it executes the query.
async fn fetch(conn: &mut Conn, sql: &str, own_tx: bool) -> Result<String> {
    let stmt = conn.prep(sql).await.map_err(map_query_error)?;
    if own_tx {
        conn.query_drop("START TRANSACTION READ ONLY")
            .await
            .map_err(map_query_error)?;
    }
    let row: std::result::Result<Option<Row>, _> = conn.exec_first(&stmt, ()).await;
    if own_tx {
        // Always end the transaction, whatever happened.
        let _ = conn.query_drop("ROLLBACK").await;
    }
    let _ = conn.close(stmt).await;
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

    fn kind(sql: &str) -> Kind {
        let d = ScriptDialect::mysql(false, true);
        let (body, class) = single_statement(d, sql).unwrap();
        kind_of(class, &body)
    }

    #[test]
    fn classification_uses_the_parser() {
        for (sql, expected) in [
            ("select * from t", Kind::Read),
            ("  /* c */ (select 1) union (select 2)", Kind::Read),
            ("with x as (select 1) select * from x", Kind::Read),
            ("select 'update' as w, `delete` from t", Kind::Read),
            ("update t set a = 1", Kind::Write),
            ("-- x\ndelete from t", Kind::Write),
            ("insert into t select * from u", Kind::Write),
            ("with x as (select 1) update t set a = 1", Kind::Write),
            ("select * into outfile '/tmp/x' from t", Kind::Write),
            ("select get_lock('a', 1)", Kind::Write),
            ("select 1 from t where release_lock('a')", Kind::Write),
            ("replace into t values (1)", Kind::Write),
            ("create table t (a int)", Kind::Other),
            ("call p()", Kind::Other),
            ("set @a = 1", Kind::Other),
        ] {
            assert_eq!(kind(sql), expected, "{sql}");
        }
    }

    #[test]
    fn only_one_statement_and_no_versioned_comments() {
        let d = ScriptDialect::mysql(false, true);
        for bad in [
            "SELECT 1; DROP TABLE t",
            "select 1; commit; /*!update t set a=2*/",
            "select 1;select 2",
            "select /*!50000 1 */ from t",
            "select /*M!100100 1 */ from t",
        ] {
            let e = single_statement(d, bad).unwrap_err();
            assert!(matches!(e, Error::Query { .. }), "{bad}: {e:?}");
        }
        // Semicolons inside strings, identifiers and comments are fine.
        for ok in [
            "select 'a;b' from t",
            "select `a;b` from t",
            "select 1 /* ; */ from t;",
            "select '\\'; select' from t",
        ] {
            assert!(single_statement(d, ok).is_ok(), "{ok}");
        }
        // With NO_BACKSLASH_ESCAPES the backslash does not escape the quote.
        let no_bs = ScriptDialect::mysql(false, false);
        assert!(single_statement(no_bs, "select '\\'; drop table t; select '").is_err());
        assert!(matches!(
            single_statement(d, "  ").unwrap_err(),
            Error::Query { position: None, .. }
        ));
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
