//! Plan history (`plan_history` table, PLAN-07).
//!
//! Contract scaffold of the backlog contract v2: types and repository signatures are final, the
//! bodies are filled in by package E (they return `Unsupported` until then).

use dbstudio_core::{Error, PlanFormat, RawPlan, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::db::Store;

/// Largest `raw_plan` JSON (bytes) kept in the history; bigger plans are not stored.
pub const PLAN_HISTORY_MAX_RAW_BYTES: usize = 8 * 1024 * 1024;

/// One plan history row without the raw plan (list view).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanHistoryEntry {
    /// Row id.
    pub id: Uuid,
    /// Connection the plan was explained on; `None` after the connection was deleted.
    pub connection_id: Option<Uuid>,
    /// RFC 3339 UTC time.
    pub created_at: String,
    /// Database the statement ran in.
    pub database: Option<String>,
    /// The explained statement, already redacted with `dbstudio_sql::redact`.
    pub statement: String,
    /// The plan carries actual statistics (ANALYZE).
    pub analyze: bool,
    /// Source format of the raw plan.
    pub format: PlanFormat,
    /// Total estimated cost, when the parser found one.
    pub total_cost: Option<f64>,
    /// Total actual time in milliseconds, when known.
    pub total_ms: Option<f64>,
    /// Number of plan warnings at capture time.
    pub warnings: u32,
}

/// A plan history row with its raw plan (same content as a `.dbplan.json` file).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanHistoryRecord {
    /// The list-view fields.
    #[serde(flatten)]
    pub entry: PlanHistoryEntry,
    /// The plan exactly as the driver returned it.
    pub raw_plan: RawPlan,
}

/// A plan to record.
#[derive(Debug, Clone, PartialEq)]
pub struct NewPlanHistory {
    /// Connection the plan belongs to.
    pub connection_id: Option<Uuid>,
    /// Database the statement ran in.
    pub database: Option<String>,
    /// Redacted statement.
    pub statement: String,
    /// The plan.
    pub raw_plan: RawPlan,
    /// Total estimated cost.
    pub total_cost: Option<f64>,
    /// Total actual time in milliseconds.
    pub total_ms: Option<f64>,
    /// Number of plan warnings.
    pub warnings: u32,
}

fn pending<T>() -> Result<T> {
    Err(Error::Unsupported("plan_history".into()))
}

impl Store {
    /// Record a plan, then trim the connection's history to `keep` rows (oldest first).
    pub fn plan_history_add(&self, entry: &NewPlanHistory, keep: u32) -> Result<PlanHistoryEntry> {
        let _ = (entry, keep);
        pending()
    }

    /// Newest-first page of entries; `connection_id = None` lists every connection. `before` is
    /// the `created_at` of the last entry of the previous page.
    pub fn plan_history_list(
        &self,
        connection_id: Option<Uuid>,
        limit: u32,
        before: Option<&str>,
    ) -> Result<Vec<PlanHistoryEntry>> {
        let _ = (connection_id, limit, before);
        pending()
    }

    /// One entry with its raw plan.
    pub fn plan_history_get(&self, id: Uuid) -> Result<PlanHistoryRecord> {
        let _ = id;
        pending()
    }

    /// Delete one entry.
    pub fn plan_history_delete(&self, id: Uuid) -> Result<()> {
        let _ = id;
        pending()
    }

    /// Delete the history of one connection (`None` = all); returns the number of rows removed.
    pub fn plan_history_clear(&self, connection_id: Option<Uuid>) -> Result<u32> {
        let _ = connection_id;
        pending()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_serializes_camel_case_and_record_flattens_it() {
        let entry = PlanHistoryEntry {
            id: Uuid::nil(),
            connection_id: None,
            created_at: "2026-11-14T00:00:00Z".into(),
            database: Some("app".into()),
            statement: "SELECT ?".into(),
            analyze: false,
            format: PlanFormat::PostgresJson,
            total_cost: Some(1.5),
            total_ms: None,
            warnings: 2,
        };
        let record = PlanHistoryRecord {
            entry: entry.clone(),
            raw_plan: RawPlan {
                format: PlanFormat::PostgresJson,
                text: "[]".into(),
                statement: "SELECT 1".into(),
                analyzed: false,
                rolled_back: false,
                server_version: "17".into(),
                captured_at: "2026-11-14T00:00:00Z".into(),
                extra: Default::default(),
            },
        };
        let json = serde_json::to_value(&record).unwrap();
        assert_eq!(json["connectionId"], serde_json::Value::Null);
        assert_eq!(json["totalCost"], 1.5);
        assert_eq!(json["rawPlan"]["format"], "postgresJson");
        assert_eq!(
            serde_json::from_value::<PlanHistoryRecord>(json).unwrap(),
            record
        );
    }

    #[test]
    fn scaffold_is_unsupported_until_package_e() {
        let store = Store::open_in_memory().unwrap();
        assert!(matches!(
            store.plan_history_list(None, 10, None),
            Err(Error::Unsupported(_))
        ));
        // The table exists (migration v7).
        let n: i64 = store
            .lock()
            .query_row("SELECT COUNT(*) FROM plan_history", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }
}
