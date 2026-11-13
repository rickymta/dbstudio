//! Execution plans as SHOWPLAN XML.
//!
//! Estimated plans use `SET SHOWPLAN_XML ON` (the statement is compiled, not run). `analyze`
//! runs the statement with `SET STATISTICS XML ON` inside a transaction that is always rolled
//! back, even for a `SELECT`.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use dbstudio_core::{
    ColumnMeta, Error, ExplainRequest, PlanFormat, RawPlan, Result, ResultSink, ScriptDialect,
    StatementClass, StatementEnd, Value,
};

use crate::conn::Conn;
use crate::exec::{Control, RunParams, SinkOut, run_script};
use crate::session::MssqlSession;
use crate::value::{int_at, str_at};

/// Collects the XML plan documents of a run.
#[derive(Default)]
struct PlanSink {
    xml_column: bool,
    plans: Vec<String>,
}

impl ResultSink for PlanSink {
    fn on_columns(&mut self, columns: Vec<ColumnMeta>) -> Result<()> {
        self.xml_column = columns
            .first()
            .is_some_and(|c| c.name.to_ascii_lowercase().contains("showplan"));
        Ok(())
    }
    fn on_rows(&mut self, rows: Vec<Vec<Value>>) -> Result<()> {
        if self.xml_column {
            for r in rows {
                if let Some(x) = r.first() {
                    let t = str_at(std::slice::from_ref(x), 0);
                    if !t.is_empty() {
                        self.plans.push(t);
                    }
                }
            }
        }
        Ok(())
    }
    fn on_message(&mut self, _: String) -> Result<()> {
        Ok(())
    }
    fn on_statement_end(&mut self, _: StatementEnd) -> Result<()> {
        self.xml_column = false;
        Ok(())
    }
}

/// Upper-cased words (outside strings, comments and quoted identifiers) and `;` marks of `text`;
/// `None` entries are statement terminators followed by more code.
fn words(text: &str) -> (Vec<String>, bool) {
    use dbstudio_sql::lexer::{TokenKind, tokenize};
    let chars: Vec<char> = text.chars().collect();
    let tokens = tokenize(ScriptDialect::MsSql, text);
    let mut words = Vec::new();
    let mut second_statement = false;
    let mut after_semicolon = false;
    for t in &tokens {
        if t.kind.is_trivia() {
            continue;
        }
        let piece: String = chars[t.range.start as usize..t.range.end as usize]
            .iter()
            .collect();
        if after_semicolon {
            second_statement = true;
        }
        match t.kind {
            TokenKind::Word => words.push(piece.to_ascii_uppercase()),
            TokenKind::Punct if piece == ";" => {
                after_semicolon = true;
                continue;
            }
            TokenKind::Punct if piece == "(" => words.push("(".into()),
            _ => {}
        }
        after_semicolon = false;
    }
    (words, second_statement)
}

fn reject(message: &str) -> Error {
    Error::Unsupported(format!("explain: {message}"))
}

/// Accept exactly one plain statement: no `GO`, no second statement, no transaction or session
/// control, no dynamic SQL, nothing that could switch the plan mode off or end our transaction.
/// For `ANALYZE` also nothing that cannot be undone (DDL, administration).
pub(crate) fn guard(text: &str, analyze: bool) -> Result<()> {
    let batches = dbstudio_sql::split::split_batches(ScriptDialect::MsSql, text);
    if batches.len() != 1 || batches[0].repeat != 1 {
        return Err(reject("exactly one statement without `GO` is allowed"));
    }
    let (words, second) = words(text);
    if words.is_empty() {
        return Err(reject("the text holds no statement"));
    }
    let a = dbstudio_sql::analyze(ScriptDialect::MsSql, text);
    let stmts: Vec<_> = a
        .statements
        .iter()
        .filter(|s| s.class != StatementClass::Empty)
        .collect();
    if second || stmts.len() != 1 || !a.problems.is_empty() {
        return Err(reject("exactly one statement is allowed"));
    }
    match stmts[0].class {
        StatementClass::Transaction { .. } | StatementClass::Session => {
            return Err(reject("transaction and session statements have no plan"));
        }
        StatementClass::Explain { .. } => {
            return Err(reject("the statement is already an explain"));
        }
        StatementClass::Ddl | StatementClass::Admin if analyze => {
            return Err(Error::Unsupported("explain_analyze_ddl".into()));
        }
        StatementClass::Unknown if analyze => {
            return Err(Error::Unsupported("explain_analyze_unknown".into()));
        }
        _ => {}
    }
    let has = |w: &str| words.iter().any(|x| x == w);
    for w in ["COMMIT", "ROLLBACK", "SAVE", "GO"] {
        if has(w) {
            return Err(reject(
                "transaction control inside the statement is not allowed",
            ));
        }
    }
    if words
        .windows(2)
        .any(|p| p[0] == "BEGIN" && matches!(p[1].as_str(), "TRAN" | "TRANSACTION" | "DISTRIBUTED"))
    {
        return Err(reject(
            "transaction control inside the statement is not allowed",
        ));
    }
    if words
        .iter()
        .any(|w| w.starts_with("SHOWPLAN") || w == "STATISTICS")
    {
        return Err(reject("plan mode switches are not allowed"));
    }
    let dynamic = has("SP_EXECUTESQL")
        || words
            .windows(2)
            .any(|p| matches!(p[0].as_str(), "EXEC" | "EXECUTE") && p[1] == "(");
    if dynamic {
        return Err(reject("dynamic SQL cannot be explained"));
    }
    if analyze {
        for w in [
            "CREATE",
            "ALTER",
            "DROP",
            "TRUNCATE",
            "BACKUP",
            "RESTORE",
            "DBCC",
            "GRANT",
            "REVOKE",
            "DENY",
            "KILL",
            "SHUTDOWN",
            "RECONFIGURE",
            "USE",
        ] {
            if has(w) {
                return Err(Error::Unsupported("explain_analyze_ddl".into()));
            }
        }
    }
    Ok(())
}

async fn step(conn: &mut Conn, ctl: &Control, params: &RunParams, sql: &str) -> Result<()> {
    let mut sink = PlanSink::default();
    run_script(conn, sql, params, ctl, None, &mut SinkOut(&mut sink))
        .await
        .map(|_| ())
}

/// `(@@TRANCOUNT, XACT_STATE())`.
async fn tx_state(conn: &mut Conn) -> Result<(i64, i64)> {
    let rows = conn.rows("SELECT @@TRANCOUNT, XACT_STATE()").await?;
    let r = rows.first().cloned().unwrap_or_default();
    Ok((int_at(&r, 0), int_at(&r, 1)))
}

/// Explain on `conn`; the connection ends clean (SET options off, no transaction of ours).
/// `before_cleanup` runs once the statement is done and before the clean-up statements, so a
/// cancel can no longer reach them (callers drop their cancel registration there).
pub(crate) async fn explain_on(
    conn: &mut Conn,
    ctl: &Control,
    req: &ExplainRequest,
    before_cleanup: &mut (dyn FnMut() + Send),
) -> Result<RawPlan> {
    guard(&req.text, req.analyze)?;
    let params = RunParams {
        max_rows: None,
        batch_size: 1000,
        timeout: req.timeout_ms.map(Duration::from_millis),
    };
    let quiet = RunParams {
        timeout: None,
        ..params.clone()
    };
    let quiet_ctl = Control::default();
    let mut sink = PlanSink::default();
    // Inside the caller's transaction (a lease) a savepoint keeps the outer work.
    let outer_count = if req.analyze {
        tx_state(conn).await?.0
    } else {
        0
    };
    let outer = outer_count > 0;
    let outcome: Result<()> = async {
        if req.analyze {
            let begin = if outer {
                "SAVE TRANSACTION dbstudio_explain"
            } else {
                "BEGIN TRANSACTION"
            };
            step(conn, &quiet_ctl, &quiet, begin).await?;
            step(conn, &quiet_ctl, &quiet, "SET STATISTICS XML ON").await?;
        } else {
            step(conn, &quiet_ctl, &quiet, "SET SHOWPLAN_XML ON").await?;
        }
        run_script(conn, &req.text, &params, ctl, None, &mut SinkOut(&mut sink))
            .await
            .map(|_| ())
    }
    .await;
    before_cleanup();
    // Always restore the connection, whatever happened.
    let mut doomed = false;
    let cleanup: Result<()> = if req.analyze {
        let expected = if outer { outer_count } else { 1 };
        let off = step(conn, &quiet_ctl, &quiet, "SET STATISTICS XML OFF").await;
        let state = tx_state(conn).await;
        let rollback = match state {
            Ok((count, xact)) if outer && (xact == -1 || count == 0) => {
                // The outer transaction was doomed or rolled back by the server (for example
                // `XACT_ABORT`): nothing but a rollback is left; undo everything.
                doomed = true;
                step(
                    conn,
                    &quiet_ctl,
                    &quiet,
                    "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION",
                )
                .await
            }
            Ok((count, _)) if count == expected => {
                let sql = if outer {
                    "ROLLBACK TRANSACTION dbstudio_explain"
                } else {
                    "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION"
                };
                step(conn, &quiet_ctl, &quiet, sql).await
            }
            Ok(_) => {
                // The statement changed the transaction state: undo all and drop the connection.
                let _ = step(
                    conn,
                    &quiet_ctl,
                    &quiet,
                    "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION",
                )
                .await;
                conn.broken = true;
                Err(Error::Other(
                    "the statement changed the transaction state; everything was rolled back"
                        .into(),
                ))
            }
            Err(e) => Err(e),
        };
        off.and(rollback)
    } else {
        step(conn, &quiet_ctl, &quiet, "SET SHOWPLAN_XML OFF").await
    };
    if cleanup.is_err() {
        conn.broken = true;
    }
    if doomed {
        return Err(Error::Other(match outcome {
            Err(e) => {
                format!("the transaction was aborted by the server and has been rolled back ({e})")
            }
            Ok(()) => "the transaction was aborted by the server and has been rolled back".into(),
        }));
    }
    outcome?;
    cleanup?;
    let mut plans = sink.plans.into_iter();
    let text = plans.next().ok_or_else(|| Error::Query {
        message: "the statement has no execution plan".into(),
        position: None,
    })?;
    let more = plans.count();
    let version = conn
        .rows("SELECT CAST(SERVERPROPERTY('ProductVersion') AS NVARCHAR(128))")
        .await
        .ok()
        .and_then(|r| r.first().map(|x| str_at(x, 0)))
        .unwrap_or_default();
    let mut extra = BTreeMap::new();
    if more > 0 {
        extra.insert("additionalPlans".to_string(), more.to_string());
    }
    Ok(RawPlan {
        format: PlanFormat::MsSqlShowplanXml,
        text,
        statement: req.text.clone(),
        analyzed: req.analyze,
        rolled_back: req.analyze,
        server_version: version,
        captured_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        extra,
    })
}

pub async fn explain(sess: &MssqlSession, req: ExplainRequest) -> Result<RawPlan> {
    guard(&req.text, req.analyze)?;
    let db = req
        .database
        .clone()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| sess.login_db().to_string());
    let mut conn = sess.shared.acquire(&db).await?;
    let ctl = Arc::new(Control::default());
    let mut reg = sess.register(Some(req.query_id), &ctl, &conn);
    let r = explain_on(&mut conn, &ctl, &req, &mut || drop(reg.take())).await;
    drop(reg);
    sess.finish(conn, Some(&req.text)).await;
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(r: Result<()>) -> String {
        match r {
            Ok(()) => "ok".into(),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn guard_accepts_one_plain_statement() {
        assert_eq!(code(guard("SELECT 1", false)), "ok");
        assert_eq!(
            code(guard("  select * from t where a = 'x;y' -- c;\n;", false)),
            "ok"
        );
        assert_eq!(code(guard("UPDATE t SET a = 1", true)), "ok");
        assert_eq!(code(guard("EXEC dbo.p @a = 1", false)), "ok");
        assert_eq!(
            code(guard("WITH x AS (SELECT 1 AS a) SELECT * FROM x", true)),
            "ok"
        );
    }

    #[test]
    fn guard_rejects_what_could_escape_the_sandbox() {
        for bad in [
            "DELETE FROM t; COMMIT",
            "SELECT 1; TRUNCATE TABLE t",
            "SELECT 1\nGO\nDELETE FROM t",
            "GO\nSET SHOWPLAN_XML OFF\nGO\nDELETE FROM t",
            "SET SHOWPLAN_XML OFF",
            "COMMIT",
            "BEGIN TRAN",
            "USE master",
            "SET NOCOUNT ON",
            "EXEC('DELETE FROM t')",
            "EXECUTE sp_executesql N'DELETE FROM t'",
            "DELETE FROM t COMMIT",
            "",
            "-- only a comment",
        ] {
            assert_ne!(code(guard(bad, false)), "ok", "{bad:?}");
            assert_ne!(code(guard(bad, true)), "ok", "{bad:?} (analyze)");
        }
        for ddl in [
            "SELECT 1; TRUNCATE TABLE t",
            "TRUNCATE TABLE t",
            "DROP TABLE t",
            "ALTER TABLE t ADD c INT",
        ] {
            assert_ne!(code(guard(ddl, true)), "ok", "{ddl}");
        }
        assert!(code(guard("DROP TABLE t", true)).contains("explain_analyze_ddl"));
    }
}
