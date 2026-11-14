//! Storage of SQL debugger data that is not part of an editor tab: routine breakpoints
//! (`routine_breakpoints`, created by migration v8). Script breakpoints and watches travel with
//! the editor tab (`EditorTabState::debug`).

use dbstudio_core::debug::RoutineBreakpoint;
use dbstudio_core::{Error, Result};
use rusqlite::params;
use uuid::Uuid;

use crate::db::{Store, db_err, now_iso};

/// Most routine breakpoints one connection may keep.
pub const MAX_ROUTINE_BREAKPOINTS: usize = 500;

impl Store {
    /// Routine breakpoints of a connection, oldest first (`NotFound` when it does not exist).
    pub fn routine_breakpoints_get(&self, connection_id: Uuid) -> Result<Vec<RoutineBreakpoint>> {
        let conn = self.lock();
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM connections WHERE id = ?1)",
                params![connection_id.to_string()],
                |r| r.get(0),
            )
            .map_err(db_err)?;
        if !exists {
            return Err(Error::NotFound(format!("connection {connection_id}")));
        }
        let mut stmt = conn
            .prepare(
                "SELECT body_json FROM routine_breakpoints WHERE connection_id = ?1
                 ORDER BY updated_at, rowid",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![connection_id.to_string()], |r| {
                r.get::<_, String>(0)
            })
            .map_err(db_err)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(db_err)?;
        // A damaged document is skipped: it must not hide the other breakpoints.
        Ok(rows
            .into_iter()
            .filter_map(|j| serde_json::from_str::<RoutineBreakpoint>(&j).ok())
            .collect())
    }

    /// Replace all routine breakpoints of a connection with `list` (atomically). A repeated
    /// breakpoint id keeps its last occurrence.
    pub fn routine_breakpoints_set(
        &self,
        connection_id: Uuid,
        list: &[RoutineBreakpoint],
    ) -> Result<()> {
        if list.len() > MAX_ROUTINE_BREAKPOINTS {
            return Err(Error::Invalid(format!(
                "at most {MAX_ROUTINE_BREAKPOINTS} routine breakpoints per connection"
            )));
        }
        if list.iter().any(|b| b.breakpoint.id.trim().is_empty()) {
            return Err(Error::Invalid("breakpoint id must not be empty".into()));
        }
        let now = now_iso();
        let mut conn = self.lock();
        let tx = conn.transaction().map_err(db_err)?;
        let exists: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM connections WHERE id = ?1)",
                params![connection_id.to_string()],
                |r| r.get(0),
            )
            .map_err(db_err)?;
        if !exists {
            return Err(Error::NotFound(format!("connection {connection_id}")));
        }
        tx.execute(
            "DELETE FROM routine_breakpoints WHERE connection_id = ?1",
            params![connection_id.to_string()],
        )
        .map_err(db_err)?;
        for b in list {
            // The key is (connection, id): a repeated id inside one list replaces the earlier one,
            // an id used by another connection is left alone.
            let body =
                serde_json::to_string(b).map_err(|e| Error::Storage(format!("breakpoint: {e}")))?;
            tx.execute(
                "INSERT OR REPLACE INTO routine_breakpoints
                     (id, connection_id, signature, source_hash, body_json, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    b.breakpoint.id,
                    connection_id.to_string(),
                    b.routine.signature,
                    b.source_hash,
                    body,
                    now
                ],
            )
            .map_err(db_err)?;
        }
        tx.commit().map_err(db_err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ConnectionProfile;
    use dbstudio_core::debug::{Breakpoint, RoutineRef};
    use dbstudio_core::{ConnectionConfig, DbKind};

    fn connection(s: &Store, name: &str) -> Uuid {
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

    fn bp(id: &str, signature: &str, line: u32) -> RoutineBreakpoint {
        RoutineBreakpoint {
            routine: RoutineRef {
                signature: signature.into(),
                oid: Some("16384".into()),
                path: None,
            },
            source_hash: Some("abc".into()),
            breakpoint: Breakpoint {
                id: id.into(),
                line,
                enabled: true,
                suspend: true,
                condition: Some("i = 3".into()),
                log_message: None,
                hit_count: None,
            },
        }
    }

    #[test]
    fn set_replaces_and_get_roundtrips() {
        let s = Store::open_in_memory().unwrap();
        let c = connection(&s, "a");
        assert!(s.routine_breakpoints_get(c).unwrap().is_empty());
        let list = vec![bp("b1", "public.f(int)", 3), bp("b2", "public.g()", 7)];
        s.routine_breakpoints_set(c, &list).unwrap();
        assert_eq!(s.routine_breakpoints_get(c).unwrap(), list);
        s.routine_breakpoints_set(c, &list[1..]).unwrap();
        assert_eq!(s.routine_breakpoints_get(c).unwrap(), list[1..].to_vec());
        s.routine_breakpoints_set(c, &[]).unwrap();
        assert!(s.routine_breakpoints_get(c).unwrap().is_empty());
    }

    #[test]
    fn connections_are_isolated_and_deleting_one_drops_its_breakpoints() {
        let s = Store::open_in_memory().unwrap();
        let (a, b) = (connection(&s, "a"), connection(&s, "b"));
        s.routine_breakpoints_set(a, &[bp("b1", "public.f()", 1)])
            .unwrap();
        s.routine_breakpoints_set(b, &[bp("b2", "public.g()", 2)])
            .unwrap();
        assert_eq!(s.routine_breakpoints_get(a).unwrap().len(), 1);
        s.delete_profile(a).unwrap();
        assert!(matches!(
            s.routine_breakpoints_get(a),
            Err(Error::NotFound(_))
        ));
        assert_eq!(s.routine_breakpoints_get(b).unwrap().len(), 1);
    }

    #[test]
    fn the_same_breakpoint_id_on_two_connections_does_not_steal_it() {
        let s = Store::open_in_memory().unwrap();
        let (a, b) = (connection(&s, "a"), connection(&s, "b"));
        s.routine_breakpoints_set(a, &[bp("same", "public.f()", 1)])
            .unwrap();
        s.routine_breakpoints_set(b, &[bp("same", "public.g()", 2)])
            .unwrap();
        assert_eq!(
            s.routine_breakpoints_get(a).unwrap()[0].routine.signature,
            "public.f()"
        );
        assert_eq!(
            s.routine_breakpoints_get(b).unwrap()[0].routine.signature,
            "public.g()"
        );
    }

    #[test]
    fn invalid_input_is_rejected_and_unknown_connection_is_not_found() {
        let s = Store::open_in_memory().unwrap();
        let c = connection(&s, "a");
        assert!(matches!(
            s.routine_breakpoints_set(c, &[bp(" ", "f", 1)]),
            Err(Error::Invalid(_))
        ));
        assert!(matches!(
            s.routine_breakpoints_set(Uuid::new_v4(), &[]),
            Err(Error::NotFound(_))
        ));
        let many: Vec<_> = (0..=MAX_ROUTINE_BREAKPOINTS)
            .map(|i| bp(&format!("b{i}"), "f", 1))
            .collect();
        assert!(matches!(
            s.routine_breakpoints_set(c, &many),
            Err(Error::Invalid(_))
        ));
    }

    #[test]
    fn a_damaged_document_is_skipped() {
        let s = Store::open_in_memory().unwrap();
        let c = connection(&s, "a");
        s.routine_breakpoints_set(c, &[bp("b1", "f()", 1), bp("b2", "g()", 2)])
            .unwrap();
        s.lock()
            .execute(
                "UPDATE routine_breakpoints SET body_json = '{broken' WHERE id = 'b1'",
                [],
            )
            .unwrap();
        let got = s.routine_breakpoints_get(c).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].breakpoint.id, "b2");
    }
}
