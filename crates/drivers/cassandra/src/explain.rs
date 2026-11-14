//! `explain_analyze` of a SELECT through CQL tracing.

use std::collections::BTreeMap;
use std::sync::Arc;

use dbstudio_core::{Error, ExplainRequest, PlanFormat, RawPlan, Result};
use scylla::client::session::Session;
use scylla::response::PagingState;
use scylla::statement::Statement;
use serde_json::json;

use crate::convert::format_timestamp;
use crate::core::{Core, ExecState, Kind, classify};
use crate::error;

/// Current time as RFC 3339 (UTC, milliseconds).
pub(crate) fn now_rfc3339() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    format_timestamp(ms)
}

impl Core {
    /// Run the (first page of the) SELECT with tracing on and return the trace as JSON.
    pub(crate) async fn explain_select(
        &self,
        state: &ExecState,
        session: Arc<Session>,
        req: &ExplainRequest,
    ) -> Result<RawPlan> {
        if !req.analyze {
            return Err(Error::Unsupported("explain".into()));
        }
        let text = req.text.trim().trim_end_matches(';').trim().to_string();
        match classify(&text) {
            Kind::Select => {}
            _ => return Err(Error::Unsupported("explain_analyze_writes".into())),
        }
        let guard = self.register(req.query_id);
        let mut stmt = Statement::new(text.clone());
        stmt.set_consistency(state.consistency);
        stmt.set_serial_consistency(state.serial);
        stmt.set_tracing(true);
        stmt.set_page_size(1000);
        if let Some(ms) = req.timeout_ms.filter(|m| *m > 0) {
            stmt.set_request_timeout(Some(std::time::Duration::from_millis(ms)));
        }
        let run = async {
            let (result, response) = session
                .query_single_page(stmt, &[], PagingState::start())
                .await
                .map_err(|e| error::from_execution(&e, &text))?;
            let id = result
                .tracing_id()
                .ok_or_else(|| Error::Other("the server returned no tracing session".into()))?;
            let rows = result.into_rows_result().map(|r| r.rows_num()).unwrap_or(0);
            let info = session
                .get_tracing_info(&id)
                .await
                .map_err(|e| Error::Other(format!("tracing session {id} is not available: {e}")))?;
            Ok::<_, Error>((id, rows, response.finished(), info))
        };
        let (id, rows, finished, info) = tokio::select! {
            r = run => r?,
            _ = guard.token.cancelled() => return Err(Error::Cancelled),
        };
        let events: Vec<_> = info
            .events
            .iter()
            .map(|e| {
                json!({
                    "activity": e.activity,
                    "source": e.source.map(|s| s.to_string()),
                    "sourceElapsedUs": e.source_elapsed,
                    "thread": e.thread,
                })
            })
            .collect();
        let doc = json!({
            "sessionId": id.to_string(),
            "request": info.request,
            "command": info.command,
            "coordinator": info.coordinator.map(|c| c.to_string()),
            "durationUs": info.duration,
            "startedAt": info.started_at.map(|t| format_timestamp(t.0)),
            "parameters": info.parameters,
            "rows": rows,
            "events": events,
        });
        let mut extra = BTreeMap::new();
        extra.insert("sessionId".to_string(), id.to_string());
        extra.insert("rows".to_string(), rows.to_string());
        if !finished {
            extra.insert("note".to_string(), "first page only".to_string());
        }
        Ok(RawPlan {
            format: PlanFormat::CqlTrace,
            text: serde_json::to_string_pretty(&doc).map_err(|e| Error::Other(e.to_string()))?,
            statement: text,
            analyzed: true,
            rolled_back: false,
            server_version: self.facts.version.clone(),
            captured_at: now_rfc3339(),
            extra,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_shape() {
        let t = now_rfc3339();
        assert!(t.ends_with('Z') && t.len() == 24, "{t}");
        assert!(t.starts_with("20"));
    }
}
