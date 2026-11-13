//! Execution plans as SHOWPLAN XML.
//!
//! Estimated plans use `SET SHOWPLAN_XML ON` (the statement is compiled, not run). `analyze`
//! runs the statement with `SET STATISTICS XML ON` inside a transaction that is always rolled
//! back, even for a `SELECT`.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use dbstudio_core::{
    ColumnMeta, Error, ExplainRequest, PlanFormat, RawPlan, Result, ResultSink, StatementEnd, Value,
};

use crate::conn::Conn;
use crate::exec::{Control, RunParams, SinkOut, run_script};
use crate::session::MssqlSession;
use crate::value::str_at;

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

/// First keyword of a statement, upper-cased (comments skipped).
fn first_keyword(text: &str) -> String {
    let mut rest = text.trim_start();
    loop {
        if rest.starts_with("--") {
            rest = rest.split_once('\n').map_or("", |(_, r)| r).trim_start();
        } else if rest.starts_with("/*") {
            rest = rest.split_once("*/").map_or("", |(_, r)| r).trim_start();
        } else {
            break;
        }
    }
    rest.split(|c: char| !c.is_ascii_alphabetic())
        .next()
        .unwrap_or("")
        .to_ascii_uppercase()
}

/// Statements `ANALYZE` cannot undo.
fn is_ddl(text: &str) -> bool {
    matches!(
        first_keyword(text).as_str(),
        "CREATE"
            | "ALTER"
            | "DROP"
            | "TRUNCATE"
            | "BACKUP"
            | "RESTORE"
            | "DBCC"
            | "GRANT"
            | "REVOKE"
            | "DENY"
    )
}

async fn step(conn: &mut Conn, ctl: &Control, params: &RunParams, sql: &str) -> Result<()> {
    let mut sink = PlanSink::default();
    run_script(conn, sql, params, ctl, None, &mut SinkOut(&mut sink))
        .await
        .map(|_| ())
}

/// Explain on `conn`; the connection ends clean (SET options off, no open transaction).
pub(crate) async fn explain_on(
    conn: &mut Conn,
    ctl: &Control,
    req: &ExplainRequest,
) -> Result<RawPlan> {
    if req.analyze && is_ddl(&req.text) {
        return Err(Error::Unsupported("explain_analyze_ddl".into()));
    }
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
    let outer = req.analyze
        && conn
            .rows("SELECT @@TRANCOUNT")
            .await?
            .first()
            .is_some_and(|r| crate::value::int_at(r, 0) > 0);
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
    // Always restore the connection, whatever happened.
    let cleanup = if req.analyze {
        let off = step(conn, &quiet_ctl, &quiet, "SET STATISTICS XML OFF").await;
        let rollback = if outer {
            "ROLLBACK TRANSACTION dbstudio_explain"
        } else {
            "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION"
        };
        let rb = step(conn, &quiet_ctl, &quiet, rollback).await;
        off.and(rb)
    } else {
        step(conn, &quiet_ctl, &quiet, "SET SHOWPLAN_XML OFF").await
    };
    if cleanup.is_err() {
        conn.broken = true;
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
    let db = req
        .database
        .clone()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| sess.login_db().to_string());
    let mut conn = sess.shared.acquire(&db).await?;
    let ctl = Arc::new(Control::default());
    let _reg = sess.register(Some(req.query_id), &ctl, &conn);
    let r = explain_on(&mut conn, &ctl, &req).await;
    if !conn.broken {
        conn.release();
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyword_detection() {
        assert_eq!(first_keyword("  select 1"), "SELECT");
        assert_eq!(first_keyword("-- c\n/* x */ drop table t"), "DROP");
        assert_eq!(first_keyword("(select 1)"), "");
        assert!(is_ddl("ALTER TABLE t ADD c INT"));
        assert!(is_ddl("-- x\nTruncate table t"));
        assert!(!is_ddl("UPDATE t SET a = 1"));
        assert!(!is_ddl("WITH x AS (SELECT 1) SELECT * FROM x"));
    }
}
