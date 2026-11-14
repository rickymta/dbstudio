//! `DebugManager`: the live SQL debug sessions of the application.
//!
//! Step 0 (contract): the registry and the lease/session hooks exist so the rest of the app can
//! call them; package T adds `HostGate` (guard per step), `EventSink`, `HostProxy` and the real
//! start/stop. Design: `docs/design/sql-debugger.md` section 3.7.

#![allow(dead_code)] // contract scaffold: consumed by package T

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use dbstudio_debug::DebugHandle;
use uuid::Uuid;

/// Sessions per application.
pub const MAX_DEBUG_SESSIONS: usize = 8;

/// Registry of debug sessions (`debug_id` -> handle, `lease_id` -> `debug_id`).
#[derive(Default)]
pub struct DebugManager {
    sessions: Mutex<HashMap<Uuid, DebugHandle>>,
}

impl std::fmt::Debug for DebugManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DebugManager")
            .field("sessions", &self.lock().len())
            .finish()
    }
}

impl DebugManager {
    /// Empty registry.
    pub fn new() -> Self {
        DebugManager::default()
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<Uuid, DebugHandle>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Number of live sessions.
    pub fn count(&self) -> usize {
        self.lock().len()
    }

    /// End the session that holds `lease_id`, if any (called before a lease is closed or marked
    /// lost). A no-op until package T registers sessions.
    pub async fn stop_for_lease(&self, lease_id: Uuid) {
        let _ = lease_id;
    }

    /// End every session of `session_id` (the session closed or reconnected).
    pub async fn stop_for_session(&self, session_id: Uuid) {
        let _ = session_id;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hooks_are_harmless_without_sessions() {
        let m = DebugManager::new();
        m.stop_for_lease(Uuid::new_v4()).await;
        m.stop_for_session(Uuid::new_v4()).await;
        assert_eq!(m.count(), 0);
    }
}
