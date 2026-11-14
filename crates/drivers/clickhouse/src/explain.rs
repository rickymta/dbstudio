//! `EXPLAIN` for ClickHouse: `json = 1, indexes = 1, actions = 1 PLAN` plus `EXPLAIN ESTIMATE`
//! and `EXPLAIN PIPELINE` kept in `RawPlan.extra`. ClickHouse has no `EXPLAIN ANALYZE`.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use dbstudio_core::{Error, ExplainRequest, PlanFormat, RawPlan, Result};

use crate::conn::{Core, Req, cell_text, strip_format_clause};
use crate::exec::Registry;
use crate::rowbinary::fmt_timestamp;

fn now_text() -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(0));
    fmt_timestamp(ms, 3)
}

/// Statement to explain: a single statement without terminator or `FORMAT` clause.
fn clean_statement(text: &str) -> Result<String> {
    let stmts = crate::exec::statements(text);
    match stmts.as_slice() {
        [one] => Ok(strip_format_clause(one)),
        [] => Err(Error::Invalid("nothing to explain".into())),
        _ => Err(Error::Invalid("EXPLAIN takes exactly one statement".into())),
    }
}

pub(crate) async fn explain(
    core: &Core,
    reg: &Registry,
    session_id: Option<&str>,
    req: &ExplainRequest,
) -> Result<RawPlan> {
    if req.analyze {
        return Err(Error::Unsupported("explain_analyze".into()));
    }
    let stmt = clean_statement(&req.text)?;
    let guard = reg.register(req.query_id);
    let base = Req {
        database: req.database.clone().filter(|d| !d.is_empty()),
        query_id: Some(req.query_id.to_string()),
        session_id: session_id.map(str::to_string),
        timeout_ms: req.timeout_ms,
        settings: Vec::new(),
    };
    let run = async {
        let plan = core
            .rows(
                &base,
                &format!("EXPLAIN PLAN json = 1, indexes = 1, actions = 1 {stmt}"),
            )
            .await?;
        let text = plan
            .iter()
            .map(|r| cell_text(r, 0))
            .collect::<Vec<_>>()
            .join("\n");
        let mut extra = BTreeMap::new();
        // Both extras are best effort (not every statement supports them).
        let noid = Req {
            query_id: None,
            ..base.clone()
        };
        if let Ok(rows) = core.rows(&noid, &format!("EXPLAIN ESTIMATE {stmt}")).await {
            let items: Vec<serde_json::Value> = rows
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "database": cell_text(r, 0),
                        "table": cell_text(r, 1),
                        "parts": cell_text(r, 2).parse::<u64>().unwrap_or(0),
                        "rows": cell_text(r, 3).parse::<u64>().unwrap_or(0),
                        "marks": cell_text(r, 4).parse::<u64>().unwrap_or(0),
                    })
                })
                .collect();
            extra.insert("estimate".to_string(), serde_json::Value::Array(items).to_string());
        }
        if let Ok(rows) = core.rows(&noid, &format!("EXPLAIN PIPELINE {stmt}")).await {
            extra.insert(
                "pipeline".to_string(),
                rows.iter().map(|r| cell_text(r, 0)).collect::<Vec<_>>().join("\n"),
            );
        }
        Ok::<_, Error>((text, extra))
    };
    let (text, extra) = tokio::select! {
        r = run => r?,
        () = guard.ctl.cancelled_pub() => {
            let _ = core.kill(&req.query_id.to_string()).await;
            return Err(Error::Cancelled);
        }
    };
    let version = core
        .scalar_text("SELECT version()")
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    Ok(RawPlan {
        format: PlanFormat::ClickHouseJson,
        text,
        statement: stmt,
        analyzed: false,
        rolled_back: false,
        server_version: version,
        captured_at: now_text(),
        extra,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statement_is_cleaned() {
        assert_eq!(clean_statement("SELECT 1;").unwrap(), "SELECT 1");
        assert_eq!(clean_statement("SELECT 1 FORMAT JSON").unwrap(), "SELECT 1");
        assert!(clean_statement("  ").is_err());
    }
}
