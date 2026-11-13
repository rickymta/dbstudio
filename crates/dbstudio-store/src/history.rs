//! Query history (`query_history` table). Package B owns this file.

use dbstudio_core::{Error, Result};
use rusqlite::{ToSql, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::db::{Store, db_err, parse_uuid};

/// Longest history text kept, in characters (longer text is cut and flagged).
pub const HISTORY_TEXT_MAX_CHARS: usize = 100_000;
/// Smallest and largest accepted `HistoryQuery::limit`.
pub const HISTORY_LIMIT_RANGE: (u32, u32) = (1, 500);

/// One executed statement or script.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    /// Row id (monotonic).
    pub id: i64,
    /// Connection that ran it.
    pub connection_id: Uuid,
    /// Database the editor targeted.
    pub database: Option<String>,
    /// `query` | `explain` | `script` | `changes`.
    pub kind: String,
    /// Statement text (secrets redacted by the caller).
    pub text: String,
    /// True when `text` was cut at [`HISTORY_TEXT_MAX_CHARS`].
    pub text_truncated: bool,
    /// ISO-8601 start time.
    pub started_at: String,
    /// Run time in milliseconds once finished.
    pub elapsed_ms: Option<u64>,
    /// `running` | `ok` | `error` | `cancelled`.
    pub status: String,
    /// Number of statements (scripts).
    pub statements: Option<u32>,
    /// Rows returned or affected.
    pub rows: Option<u64>,
    /// Failure message.
    pub error_message: Option<String>,
}

/// A history entry to insert (the id is assigned by the database).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewHistoryEntry {
    /// Connection that runs it.
    pub connection_id: Uuid,
    /// Target database.
    pub database: Option<String>,
    /// `query` | `explain` | `script` | `changes`.
    pub kind: String,
    /// Statement text, already redacted.
    pub text: String,
    /// ISO-8601 start time.
    pub started_at: String,
    /// Initial status (usually `running`).
    pub status: String,
    /// Run time in milliseconds, when already known.
    #[serde(default)]
    pub elapsed_ms: Option<u64>,
    /// Number of statements.
    #[serde(default)]
    pub statements: Option<u32>,
    /// Rows returned or affected.
    #[serde(default)]
    pub rows: Option<u64>,
    /// Failure message.
    #[serde(default)]
    pub error_message: Option<String>,
}

/// Filter and page for [`Store::history_list`]; newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryQuery {
    /// Only this connection.
    #[serde(default)]
    pub connection_id: Option<Uuid>,
    /// Case-insensitive substring of the text.
    #[serde(default)]
    pub search: Option<String>,
    /// Only this status.
    #[serde(default)]
    pub status: Option<String>,
    /// Keyset cursor: only entries with a smaller id.
    #[serde(default)]
    pub before_id: Option<i64>,
    /// Page size, clamped to 1..=500.
    pub limit: u32,
}

impl HistoryQuery {
    /// `limit` clamped to [`HISTORY_LIMIT_RANGE`].
    pub fn effective_limit(&self) -> u32 {
        self.limit
            .clamp(HISTORY_LIMIT_RANGE.0, HISTORY_LIMIT_RANGE.1)
    }
}

const COLUMNS: &str = "id, connection_id, database_name, kind, text, text_truncated, started_at, \
    elapsed_ms, status, statements, rows, error_message";

fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<HistoryEntry>> {
    let connection_id: String = row.get(1)?;
    let elapsed: Option<i64> = row.get(7)?;
    let statements: Option<i64> = row.get(9)?;
    let rows: Option<i64> = row.get(10)?;
    let entry = HistoryEntry {
        id: row.get(0)?,
        connection_id: Uuid::nil(),
        database: row.get(2)?,
        kind: row.get(3)?,
        text: row.get(4)?,
        text_truncated: row.get(5)?,
        started_at: row.get(6)?,
        elapsed_ms: elapsed.map(|v| v.max(0) as u64),
        status: row.get(8)?,
        statements: statements.map(|v| v.clamp(0, i64::from(u32::MAX)) as u32),
        rows: rows.map(|v| v.max(0) as u64),
        error_message: row.get(11)?,
    };
    Ok(
        parse_uuid(&connection_id).map(|connection_id| HistoryEntry {
            connection_id,
            ..entry
        }),
    )
}

fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

impl Store {
    /// Append an entry (text longer than [`HISTORY_TEXT_MAX_CHARS`] is cut and flagged) and
    /// return it with its id. Trimming to `historyMaxPerConnection` is the caller's job
    /// ([`Store::history_trim`]).
    pub fn history_append(&self, entry: &NewHistoryEntry) -> Result<HistoryEntry> {
        let (text, truncated) = if entry.text.chars().count() > HISTORY_TEXT_MAX_CHARS {
            (
                entry.text.chars().take(HISTORY_TEXT_MAX_CHARS).collect(),
                true,
            )
        } else {
            (entry.text.clone(), false)
        };
        let conn = self.lock();
        conn.execute(
            "INSERT INTO query_history (connection_id, database_name, kind, text, text_truncated,
             started_at, elapsed_ms, status, statements, rows, error_message)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                entry.connection_id.to_string(),
                entry.database,
                entry.kind,
                text,
                truncated,
                entry.started_at,
                entry.elapsed_ms.map(|v| v as i64),
                entry.status,
                entry.statements.map(i64::from),
                entry.rows.map(|v| v as i64),
                entry.error_message,
            ],
        )
        .map_err(|e| match e {
            rusqlite::Error::SqliteFailure(f, _)
                if f.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                Error::NotFound(format!("connection {}", entry.connection_id))
            }
            other => db_err(other),
        })?;
        let id = conn.last_insert_rowid();
        Ok(HistoryEntry {
            id,
            connection_id: entry.connection_id,
            database: entry.database.clone(),
            kind: entry.kind.clone(),
            text,
            text_truncated: truncated,
            started_at: entry.started_at.clone(),
            elapsed_ms: entry.elapsed_ms,
            status: entry.status.clone(),
            statements: entry.statements,
            rows: entry.rows,
            error_message: entry.error_message.clone(),
        })
    }

    /// Record the outcome of a `running` entry.
    pub fn history_finish(
        &self,
        id: i64,
        status: &str,
        elapsed_ms: Option<u64>,
        statements: Option<u32>,
        rows: Option<u64>,
        error_message: Option<&str>,
    ) -> Result<()> {
        let n = self
            .lock()
            .execute(
                "UPDATE query_history SET status = ?1, elapsed_ms = ?2, statements = ?3,
                 rows = ?4, error_message = ?5 WHERE id = ?6",
                params![
                    status,
                    elapsed_ms.map(|v| v as i64),
                    statements.map(i64::from),
                    rows.map(|v| v as i64),
                    error_message,
                    id
                ],
            )
            .map_err(db_err)?;
        if n == 0 {
            return Err(Error::NotFound(format!("history entry {id}")));
        }
        Ok(())
    }

    /// Entries matching `query`, newest first.
    pub fn history_list(&self, query: &HistoryQuery) -> Result<Vec<HistoryEntry>> {
        let mut sql = format!("SELECT {COLUMNS} FROM query_history WHERE 1 = 1");
        let mut args: Vec<Box<dyn ToSql>> = Vec::new();
        if let Some(c) = query.connection_id {
            args.push(Box::new(c.to_string()));
            sql.push_str(&format!(" AND connection_id = ?{}", args.len()));
        }
        if let Some(s) = query.search.as_deref().filter(|s| !s.is_empty()) {
            args.push(Box::new(format!("%{}%", escape_like(s))));
            sql.push_str(&format!(" AND text LIKE ?{} ESCAPE '\\'", args.len()));
        }
        if let Some(s) = &query.status {
            args.push(Box::new(s.clone()));
            sql.push_str(&format!(" AND status = ?{}", args.len()));
        }
        if let Some(b) = query.before_id {
            args.push(Box::new(b));
            sql.push_str(&format!(" AND id < ?{}", args.len()));
        }
        args.push(Box::new(i64::from(query.effective_limit())));
        sql.push_str(&format!(" ORDER BY id DESC LIMIT ?{}", args.len()));
        let conn = self.lock();
        let mut stmt = conn.prepare(&sql).map_err(db_err)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(args.iter()), from_row)
            .map_err(db_err)?;
        rows.map(|r| r.map_err(db_err)?).collect()
    }

    /// Entries still `running` belong to a process that is gone (a crash, a kill): record them
    /// as failed with an explanation. Call once at start-up, before anything new is recorded.
    pub fn history_mark_interrupted(&self) -> Result<u64> {
        let n = self
            .lock()
            .execute(
                "UPDATE query_history SET status = 'error', error_message = 'interrupted: the \
                 application stopped before the statement finished' WHERE status = 'running'",
                [],
            )
            .map_err(db_err)?;
        Ok(n as u64)
    }

    /// Delete one entry.
    pub fn history_delete(&self, id: i64) -> Result<()> {
        let n = self
            .lock()
            .execute("DELETE FROM query_history WHERE id = ?1", params![id])
            .map_err(db_err)?;
        if n == 0 {
            return Err(Error::NotFound(format!("history entry {id}")));
        }
        Ok(())
    }

    /// Delete the history of one connection, or of all when `None`; returns the count.
    pub fn history_clear(&self, connection_id: Option<Uuid>) -> Result<u64> {
        let conn = self.lock();
        let n = match connection_id {
            Some(c) => conn.execute(
                "DELETE FROM query_history WHERE connection_id = ?1",
                params![c.to_string()],
            ),
            None => conn.execute("DELETE FROM query_history", []),
        }
        .map_err(db_err)?;
        Ok(n as u64)
    }

    /// Keep only the newest `keep` entries of a connection; returns how many were removed.
    pub fn history_trim(&self, connection_id: Uuid, keep: u32) -> Result<u64> {
        let n = self
            .lock()
            .execute(
                "DELETE FROM query_history WHERE connection_id = ?1 AND id NOT IN (
                    SELECT id FROM query_history WHERE connection_id = ?1
                    ORDER BY id DESC LIMIT ?2)",
                params![connection_id.to_string(), i64::from(keep)],
            )
            .map_err(db_err)?;
        Ok(n as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_core::{ConnectionConfig, DbKind};

    use crate::model::ConnectionProfile;

    fn conn(s: &Store, name: &str) -> Uuid {
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

    fn new_entry(connection_id: Uuid, text: &str) -> NewHistoryEntry {
        NewHistoryEntry {
            connection_id,
            database: Some("app".into()),
            kind: "query".into(),
            text: text.into(),
            started_at: "2024-01-01T00:00:00Z".into(),
            status: "running".into(),
            elapsed_ms: None,
            statements: None,
            rows: None,
            error_message: None,
        }
    }

    fn query(connection_id: Option<Uuid>) -> HistoryQuery {
        HistoryQuery {
            connection_id,
            search: None,
            status: None,
            before_id: None,
            limit: 100,
        }
    }

    #[test]
    fn append_list_filter_finish_delete_trim() {
        let s = Store::open_in_memory().unwrap();
        let a = conn(&s, "a");
        let b = conn(&s, "b");
        let e1 = s.history_append(&new_entry(a, "select 100%_x")).unwrap();
        let e2 = s.history_append(&new_entry(a, "update t set x=1")).unwrap();
        s.history_append(&new_entry(b, "select 2")).unwrap();
        assert!(e2.id > e1.id);

        s.history_finish(e2.id, "error", Some(12), Some(1), None, Some("boom"))
            .unwrap();
        let all = s.history_list(&query(Some(a))).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, e2.id);
        assert_eq!(all[0].status, "error");
        assert_eq!(all[0].error_message.as_deref(), Some("boom"));
        assert_eq!(s.history_list(&query(None)).unwrap().len(), 3);

        let mut q = query(Some(a));
        q.search = Some("100%_".into());
        assert_eq!(s.history_list(&q).unwrap().len(), 1);
        q.search = Some("SELECT".into());
        assert_eq!(s.history_list(&q).unwrap().len(), 1);
        let mut q = query(None);
        q.status = Some("error".into());
        assert_eq!(s.history_list(&q).unwrap().len(), 1);
        let mut q = query(Some(a));
        q.before_id = Some(e2.id);
        q.limit = 0; // clamps to 1
        assert_eq!(s.history_list(&q).unwrap()[0].id, e1.id);

        assert_eq!(s.history_trim(a, 1).unwrap(), 1);
        assert_eq!(s.history_list(&query(Some(a))).unwrap().len(), 1);
        s.history_delete(e2.id).unwrap();
        assert!(matches!(s.history_delete(e2.id), Err(Error::NotFound(_))));
        assert_eq!(s.history_clear(None).unwrap(), 1);
    }

    #[test]
    fn running_entries_left_by_a_dead_process_become_errors() {
        let s = Store::open_in_memory().unwrap();
        let a = conn(&s, "a");
        let done = s.history_append(&new_entry(a, "select 1")).unwrap();
        s.history_finish(done.id, "ok", Some(1), Some(1), Some(1), None)
            .unwrap();
        let stale = s.history_append(&new_entry(a, "select 2")).unwrap();
        assert_eq!(s.history_mark_interrupted().unwrap(), 1);
        let list = s.history_list(&query(Some(a))).unwrap();
        let find = |id| list.iter().find(|e| e.id == id).unwrap();
        assert_eq!(find(stale.id).status, "error");
        assert!(
            find(stale.id)
                .error_message
                .as_deref()
                .is_some_and(|m| m.starts_with("interrupted"))
        );
        assert_eq!(find(done.id).status, "ok");
        assert_eq!(s.history_mark_interrupted().unwrap(), 0);
    }

    #[test]
    fn long_text_is_truncated_and_unknown_connection_rejected() {
        let s = Store::open_in_memory().unwrap();
        let a = conn(&s, "a");
        let long = "x".repeat(HISTORY_TEXT_MAX_CHARS + 5);
        let e = s.history_append(&new_entry(a, &long)).unwrap();
        assert!(e.text_truncated);
        assert_eq!(e.text.chars().count(), HISTORY_TEXT_MAX_CHARS);
        assert!(matches!(
            s.history_append(&new_entry(Uuid::new_v4(), "x")),
            Err(Error::NotFound(_))
        ));
    }

    #[test]
    fn deleting_the_connection_cascades_its_history() {
        let s = Store::open_in_memory().unwrap();
        let a = conn(&s, "a");
        let b = conn(&s, "b");
        s.history_append(&new_entry(a, "1")).unwrap();
        s.history_append(&new_entry(b, "2")).unwrap();
        s.delete_profile(a).unwrap();
        let left = s.history_list(&query(None)).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].connection_id, b);
    }
}
