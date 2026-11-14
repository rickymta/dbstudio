//! EXPLAIN, driver side: request and raw plan text. Parsing into trees lives in `dbstudio-plan`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Request to explain exactly one statement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExplainRequest {
    /// Identifier used to cancel via `Session/Lease::cancel`.
    pub query_id: Uuid,
    /// Exactly one statement (the command layer checks).
    pub text: String,
    /// Database to run in.
    #[serde(default)]
    pub database: Option<String>,
    /// Run the statement and collect actual statistics (always rolled back).
    #[serde(default)]
    pub analyze: bool,
    /// Extra options.
    #[serde(default)]
    pub options: ExplainOptions,
    /// Client side timeout.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// Extra EXPLAIN options (PostgreSQL).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ExplainOptions {
    /// `BUFFERS`.
    pub buffers: bool,
    /// `VERBOSE`.
    pub verbose: bool,
    /// `SETTINGS`.
    pub settings: bool,
}

/// Plan exactly as the server returned it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawPlan {
    /// Source format.
    pub format: PlanFormat,
    /// Unparsed source exactly as received.
    pub text: String,
    /// The explained statement.
    pub statement: String,
    /// Statistics are actual (ANALYZE).
    pub analyzed: bool,
    /// ANALYZE ran inside a transaction that was rolled back.
    pub rolled_back: bool,
    /// Server version string.
    pub server_version: String,
    /// RFC 3339 UTC capture time.
    pub captured_at: String,
    /// Extras, e.g. `xplan` (Oracle `DBMS_XPLAN` text).
    #[serde(default)]
    pub extra: BTreeMap<String, String>,
}

/// Format of a [`RawPlan::text`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PlanFormat {
    /// `EXPLAIN (FORMAT JSON[, ANALYZE, BUFFERS, VERBOSE, SETTINGS])`.
    PostgresJson,
    /// `EXPLAIN FORMAT=JSON` (v1).
    MySqlJson,
    /// 8.3+ `explain_json_format_version=2`.
    MySqlJsonV2,
    /// `EXPLAIN ANALYZE` / `FORMAT=TREE` text.
    MySqlTree,
    /// MariaDB `EXPLAIN FORMAT=JSON` / `ANALYZE FORMAT=JSON`.
    MariaDbJson,
    /// `SET SHOWPLAN_XML ON` / `SET STATISTICS XML ON`.
    MsSqlShowplanXml,
    /// JSON array of `PLAN_TABLE` / `V$SQL_PLAN_STATISTICS_ALL` rows.
    OraclePlanRows,
    /// Canonical EJSON of `explain("queryPlanner"|"executionStats")`.
    MongoExplain,
    /// ClickHouse `EXPLAIN json = 1, indexes = 1, actions = 1 PLAN ...`; `extra["estimate"]` =
    /// JSON rows of `EXPLAIN ESTIMATE`, `extra["pipeline"]` = text of `EXPLAIN PIPELINE`.
    ClickHouseJson,
    /// CockroachDB `EXPLAIN [ANALYZE] (VERBOSE)` text tree (`• node`, `│ attr: value`).
    CockroachText,
    /// TiDB `EXPLAIN [ANALYZE] FORMAT='row'` rows packed by the driver as a JSON array of objects
    /// (`id`, `estRows`, `actRows`, `task`, `access object`, `execution info`, `operator info`,
    /// `memory`, `disk`).
    TiDbRows,
    /// CQL tracing session (`system_traces.events`) as JSON.
    CqlTrace,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn raw_plan_shape() {
        let mut extra = BTreeMap::new();
        extra.insert("xplan".to_string(), "Plan hash value: 1".to_string());
        let p = RawPlan {
            format: PlanFormat::MsSqlShowplanXml,
            text: "<ShowPlanXML/>".into(),
            statement: "SELECT 1".into(),
            analyzed: true,
            rolled_back: true,
            server_version: "16.0".into(),
            captured_at: "2026-11-13T00:00:00Z".into(),
            extra,
        };
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(
            json,
            json!({"format": "msSqlShowplanXml", "text": "<ShowPlanXML/>", "statement": "SELECT 1",
                   "analyzed": true, "rolledBack": true, "serverVersion": "16.0",
                   "capturedAt": "2026-11-13T00:00:00Z", "extra": {"xplan": "Plan hash value: 1"}})
        );
        assert_eq!(serde_json::from_value::<RawPlan>(json).unwrap(), p);
        // extra is optional on input.
        let q: RawPlan = serde_json::from_value(json!({
            "format": "postgresJson", "text": "[]", "statement": "s", "analyzed": false,
            "rolledBack": false, "serverVersion": "17", "capturedAt": "x"
        }))
        .unwrap();
        assert!(q.extra.is_empty());
    }

    #[test]
    fn formats_serialize_camel_case() {
        for (f, s) in [
            (PlanFormat::PostgresJson, "postgresJson"),
            (PlanFormat::MySqlJson, "mySqlJson"),
            (PlanFormat::MySqlJsonV2, "mySqlJsonV2"),
            (PlanFormat::MySqlTree, "mySqlTree"),
            (PlanFormat::MariaDbJson, "mariaDbJson"),
            (PlanFormat::OraclePlanRows, "oraclePlanRows"),
            (PlanFormat::MongoExplain, "mongoExplain"),
            (PlanFormat::ClickHouseJson, "clickHouseJson"),
            (PlanFormat::CockroachText, "cockroachText"),
            (PlanFormat::TiDbRows, "tiDbRows"),
            (PlanFormat::CqlTrace, "cqlTrace"),
        ] {
            assert_eq!(serde_json::to_value(f).unwrap(), s);
        }
    }

    #[test]
    fn explain_request_defaults() {
        let r: ExplainRequest = serde_json::from_value(json!({
            "queryId": "00000000-0000-0000-0000-000000000000", "text": "SELECT 1"
        }))
        .unwrap();
        assert!(!r.analyze && r.options == ExplainOptions::default() && r.timeout_ms.is_none());
        let o: ExplainOptions = serde_json::from_value(json!({"buffers": true})).unwrap();
        assert!(o.buffers && !o.verbose && !o.settings);
    }
}
