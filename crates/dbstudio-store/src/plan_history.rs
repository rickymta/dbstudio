//! Plan history (`plan_history` table, PLAN-07).
//!

use dbstudio_core::{Error, PlanFormat, RawPlan, Result};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::db::{Store, db_err, from_text, invalid, now_iso, opt_uuid, parse_uuid, to_text};

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

fn entry_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Result<PlanHistoryEntry>> {
    let id: String = r.get("id")?;
    let connection_id: Option<String> = r.get("connection_id")?;
    let format: String = r.get("format")?;
    let warnings: i64 = r.get("warnings")?;
    let created_at: String = r.get("created_at")?;
    let database: Option<String> = r.get("database_name")?;
    let statement: String = r.get("statement")?;
    let analyze: bool = r.get("analyze")?;
    let total_cost: Option<f64> = r.get("total_cost")?;
    let total_ms: Option<f64> = r.get("total_ms")?;
    Ok((|| {
        Ok(PlanHistoryEntry {
            id: parse_uuid(&id)?,
            connection_id: opt_uuid(connection_id)?,
            created_at,
            database,
            statement,
            analyze,
            format: from_text(&format)?,
            total_cost,
            total_ms,
            warnings: warnings.clamp(0, i64::from(u32::MAX)) as u32,
        })
    })())
}

const ENTRY_COLUMNS: &str = "id, connection_id, created_at, database_name, statement, analyze, \
    format, total_cost, total_ms, warnings";

impl Store {
    /// Record a plan, then trim the connection's history to `keep` rows (oldest first). A plan
    /// whose JSON exceeds [`PLAN_HISTORY_MAX_RAW_BYTES`] is refused with `Invalid` (the caller
    /// skips recording it).
    pub fn plan_history_add(&self, entry: &NewPlanHistory, keep: u32) -> Result<PlanHistoryEntry> {
        let raw = serde_json::to_string(&entry.raw_plan).map_err(invalid)?;
        if raw.len() > PLAN_HISTORY_MAX_RAW_BYTES {
            return Err(Error::Invalid(format!(
                "plan is larger than {PLAN_HISTORY_MAX_RAW_BYTES} bytes and is not kept in the history"
            )));
        }
        let stored = PlanHistoryEntry {
            id: Uuid::new_v4(),
            connection_id: entry.connection_id,
            created_at: now_iso(),
            database: entry.database.clone(),
            statement: entry.statement.clone(),
            analyze: entry.raw_plan.analyzed,
            format: entry.raw_plan.format,
            total_cost: entry.total_cost,
            total_ms: entry.total_ms,
            warnings: entry.warnings,
        };
        let conn_id = stored.connection_id.map(|c| c.to_string());
        let mut conn = self.lock();
        let tx = conn.transaction().map_err(db_err)?;
        tx.execute(
            "INSERT INTO plan_history (id, connection_id, created_at, database_name, statement,
                 analyze, format, raw_plan, total_cost, total_ms, warnings)
             VALUES (?1, (SELECT id FROM connections WHERE id = ?2), ?3, ?4, ?5, ?6, ?7, ?8, ?9,
                 ?10, ?11)",
            params![
                stored.id.to_string(),
                conn_id,
                stored.created_at,
                stored.database,
                stored.statement,
                stored.analyze,
                to_text(&stored.format)?,
                raw,
                stored.total_cost,
                stored.total_ms,
                i64::from(stored.warnings),
            ],
        )
        .map_err(db_err)?;
        // Newest rows (by insertion order) survive; `IS` also matches a NULL connection.
        tx.execute(
            "DELETE FROM plan_history WHERE connection_id IS ?1 AND rowid NOT IN
                 (SELECT rowid FROM plan_history WHERE connection_id IS ?1
                  ORDER BY rowid DESC LIMIT ?2)",
            params![conn_id, i64::from(keep.max(1))],
        )
        .map_err(db_err)?;
        tx.commit().map_err(db_err)?;
        Ok(stored)
    }

    /// Newest-first page of entries; `connection_id = None` lists every connection. `before` is
    /// the `created_at` of the last entry of the previous page.
    pub fn plan_history_list(
        &self,
        connection_id: Option<Uuid>,
        limit: u32,
        before: Option<&str>,
    ) -> Result<Vec<PlanHistoryEntry>> {
        let limit = i64::from(limit.clamp(1, 500));
        let conn = self.lock();
        // Insertion order is chronological; `before` becomes a row position so rows created in
        // the same instant are not skipped.
        let before_rowid: Option<i64> = match before {
            Some(at) => conn
                .query_row(
                    "SELECT MIN(rowid) FROM plan_history WHERE created_at = ?1",
                    params![at],
                    |r| r.get(0),
                )
                .map_err(db_err)?,
            None => None,
        };
        let sql = format!(
            "SELECT {ENTRY_COLUMNS} FROM plan_history
             WHERE (?1 IS NULL OR connection_id = ?1)
               AND (?2 IS NULL OR rowid < ?2)
               AND (?3 IS NULL OR created_at < ?3 OR ?2 IS NOT NULL)
             ORDER BY rowid DESC LIMIT ?4"
        );
        let mut stmt = conn.prepare(&sql).map_err(db_err)?;
        let rows = stmt
            .query_map(
                params![
                    connection_id.map(|c| c.to_string()),
                    before_rowid,
                    before,
                    limit
                ],
                entry_from_row,
            )
            .map_err(db_err)?;
        rows.map(|r| r.map_err(db_err)?).collect()
    }

    /// One entry with its raw plan.
    pub fn plan_history_get(&self, id: Uuid) -> Result<PlanHistoryRecord> {
        let conn = self.lock();
        let sql = format!("SELECT {ENTRY_COLUMNS}, raw_plan FROM plan_history WHERE id = ?1");
        let found = conn
            .query_row(&sql, params![id.to_string()], |r| {
                let raw: String = r.get("raw_plan")?;
                Ok((entry_from_row(r)?, raw))
            })
            .optional()
            .map_err(db_err)?;
        let Some((entry, raw)) = found else {
            return Err(Error::NotFound(format!("plan history entry {id}")));
        };
        Ok(PlanHistoryRecord {
            entry: entry?,
            raw_plan: serde_json::from_str(&raw).map_err(invalid)?,
        })
    }

    /// Delete one entry.
    pub fn plan_history_delete(&self, id: Uuid) -> Result<()> {
        let n = self
            .lock()
            .execute(
                "DELETE FROM plan_history WHERE id = ?1",
                params![id.to_string()],
            )
            .map_err(db_err)?;
        if n == 0 {
            return Err(Error::NotFound(format!("plan history entry {id}")));
        }
        Ok(())
    }

    /// Delete the history of one connection (`None` = all); returns the number of rows removed.
    pub fn plan_history_clear(&self, connection_id: Option<Uuid>) -> Result<u32> {
        let n = self
            .lock()
            .execute(
                "DELETE FROM plan_history WHERE (?1 IS NULL OR connection_id = ?1)",
                params![connection_id.map(|c| c.to_string())],
            )
            .map_err(db_err)?;
        Ok(u32::try_from(n).unwrap_or(u32::MAX))
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

    fn new_plan(conn: Option<Uuid>, statement: &str) -> NewPlanHistory {
        NewPlanHistory {
            connection_id: conn,
            database: Some("app".into()),
            statement: statement.into(),
            raw_plan: RawPlan {
                format: PlanFormat::PostgresJson,
                text: "[]".into(),
                statement: statement.into(),
                analyzed: true,
                rolled_back: true,
                server_version: "17".into(),
                captured_at: "2026-11-14T00:00:00Z".into(),
                extra: Default::default(),
            },
            total_cost: Some(12.5),
            total_ms: Some(3.0),
            warnings: 1,
        }
    }

    fn connection(s: &Store, name: &str) -> Uuid {
        use crate::model::ConnectionProfile;
        use dbstudio_core::{ConnectionConfig, DbKind};
        s.save_profile(&ConnectionProfile {
            config: ConnectionConfig::new(DbKind::Postgres, name, "localhost"),
            group_id: None,
            color: None,
            save_password: false,
            has_password: false,
            sort_order: 0,
            created_at: String::new(),
            updated_at: String::new(),
        })
        .unwrap()
        .config
        .id
    }

    #[test]
    fn add_get_list_delete() {
        let s = Store::open_in_memory().unwrap();
        let c = connection(&s, "a");
        let added = s
            .plan_history_add(&new_plan(Some(c), "select 1"), 10)
            .unwrap();
        assert!(added.analyze);
        assert_eq!(added.total_cost, Some(12.5));
        let rec = s.plan_history_get(added.id).unwrap();
        assert_eq!(rec.entry, added);
        assert_eq!(rec.raw_plan.statement, "select 1");
        assert_eq!(
            s.plan_history_list(Some(c), 10, None).unwrap(),
            vec![added.clone()]
        );
        s.plan_history_delete(added.id).unwrap();
        assert!(matches!(
            s.plan_history_get(added.id),
            Err(Error::NotFound(_))
        ));
        assert!(matches!(
            s.plan_history_delete(added.id),
            Err(Error::NotFound(_))
        ));
    }

    #[test]
    fn history_is_trimmed_per_connection_and_pages_newest_first() {
        let s = Store::open_in_memory().unwrap();
        let (a, b) = (connection(&s, "a"), connection(&s, "b"));
        for i in 0..5 {
            s.plan_history_add(&new_plan(Some(a), &format!("a{i}")), 3)
                .unwrap();
        }
        s.plan_history_add(&new_plan(Some(b), "b0"), 3).unwrap();
        let all = s.plan_history_list(None, 100, None).unwrap();
        let texts: Vec<_> = all.iter().map(|e| e.statement.as_str()).collect();
        assert_eq!(texts, ["b0", "a4", "a3", "a2"]);
        // Paging with `before` continues after the last entry of the previous page.
        let first = s.plan_history_list(Some(a), 2, None).unwrap();
        let second = s
            .plan_history_list(Some(a), 2, Some(&first.last().unwrap().created_at))
            .unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].statement, "a2");
        assert_eq!(s.plan_history_clear(Some(a)).unwrap(), 3);
        assert_eq!(s.plan_history_list(None, 10, None).unwrap().len(), 1);
        assert_eq!(s.plan_history_clear(None).unwrap(), 1);
    }

    #[test]
    fn deleting_the_connection_cascades_and_big_plans_are_refused() {
        let s = Store::open_in_memory().unwrap();
        let c = connection(&s, "a");
        s.plan_history_add(&new_plan(Some(c), "x"), 10).unwrap();
        s.plan_history_add(&new_plan(None, "orphan"), 10).unwrap();
        s.delete_profile(c).unwrap();
        let left = s.plan_history_list(None, 10, None).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].statement, "orphan");
        let mut big = new_plan(None, "big");
        big.raw_plan.text = "x".repeat(PLAN_HISTORY_MAX_RAW_BYTES + 1);
        assert!(matches!(
            s.plan_history_add(&big, 10),
            Err(Error::Invalid(_))
        ));
        // Records survive reopening the store.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let id = {
            let st = Store::open(&path).unwrap();
            st.plan_history_add(&new_plan(None, "keep"), 10).unwrap().id
        };
        assert_eq!(
            Store::open(&path)
                .unwrap()
                .plan_history_get(id)
                .unwrap()
                .entry
                .statement,
            "keep"
        );
        assert_eq!(s.plan_history_list(None, 10, None).unwrap().len(), 1);
    }

    #[test]
    fn table_exists_after_migration() {
        let store = Store::open_in_memory().unwrap();
        let n: i64 = store
            .lock()
            .query_row("SELECT COUNT(*) FROM plan_history", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }
}
