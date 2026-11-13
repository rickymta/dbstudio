//! `LeaseManager`: registry of the dedicated connections (leases) held by editor / console tabs.
//!
//! At most [`MAX_LEASES_PER_SESSION`] per session. Closing a session closes its leases first.
//! Package B owns this file.

#![allow(dead_code)] // scaffold: consumed by the editor package

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use dbstudio_core::{Error, Lease, LeaseCloseReport, LeaseState, Result};
use serde::Serialize;
use uuid::Uuid;

use crate::events::{EventEmitter, LEASE_STATE, emit_json};

/// Leases one session may hold at the same time.
pub const MAX_LEASES_PER_SESSION: usize = 32;

/// Returned by `lease_open`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaseInfo {
    /// Identifier of the lease.
    pub lease_id: Uuid,
    /// Session the lease belongs to.
    pub session_id: Uuid,
    /// State right after opening.
    pub state: LeaseState,
}

/// Payload of the `lease://state` event.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct LeaseStateEvent {
    lease_id: Uuid,
    state: LeaseState,
}

struct Entry {
    session_id: Uuid,
    lease: Arc<dyn Lease>,
}

/// Registry of open leases.
pub struct LeaseManager {
    events: Arc<dyn EventEmitter>,
    leases: Mutex<HashMap<Uuid, Entry>>,
}

impl std::fmt::Debug for LeaseManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LeaseManager")
            .field("open", &self.lock().len())
            .finish()
    }
}

impl LeaseManager {
    /// Manager publishing `lease://state` through `events`.
    pub fn new(events: Arc<dyn EventEmitter>) -> Self {
        LeaseManager {
            events,
            leases: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<Uuid, Entry>> {
        self.leases.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Register a freshly opened lease of `session_id`. Fails with `Busy` at the per-session limit
    /// (the caller then closes the lease it just opened).
    pub fn register(&self, session_id: Uuid, lease: Box<dyn Lease>) -> Result<LeaseInfo> {
        let lease: Arc<dyn Lease> = Arc::from(lease);
        let lease_id = Uuid::new_v4();
        let state = lease.state();
        let mut map = self.lock();
        if map.values().filter(|e| e.session_id == session_id).count() >= MAX_LEASES_PER_SESSION {
            return Err(Error::Busy(format!(
                "a session can hold at most {MAX_LEASES_PER_SESSION} leases"
            )));
        }
        map.insert(lease_id, Entry { session_id, lease });
        Ok(LeaseInfo {
            lease_id,
            session_id,
            state,
        })
    }

    /// The lease, or `NotFound`.
    pub fn get(&self, lease_id: Uuid) -> Result<Arc<dyn Lease>> {
        self.get_with_session(lease_id).map(|(_, lease)| lease)
    }

    /// The lease and the session it belongs to, or `NotFound`.
    pub fn get_with_session(&self, lease_id: Uuid) -> Result<(Uuid, Arc<dyn Lease>)> {
        self.lock()
            .get(&lease_id)
            .map(|e| (e.session_id, Arc::clone(&e.lease)))
            .ok_or_else(|| Error::NotFound(format!("lease {lease_id}")))
    }

    /// Number of open leases of `session_id`.
    pub fn count_for_session(&self, session_id: Uuid) -> usize {
        self.lock()
            .values()
            .filter(|e| e.session_id == session_id)
            .count()
    }

    /// Forget a lease without closing it; returns it when it existed.
    pub fn remove(&self, lease_id: Uuid) -> Option<Arc<dyn Lease>> {
        self.lock().remove(&lease_id).map(|e| e.lease)
    }

    /// Remove and close a lease. Closing an unknown (already closed) lease reports nothing rolled
    /// back.
    pub async fn close(&self, lease_id: Uuid) -> Result<LeaseCloseReport> {
        match self.remove(lease_id) {
            Some(lease) => lease.close().await,
            None => Ok(LeaseCloseReport {
                rolled_back: false,
                forced: false,
            }),
        }
    }

    /// Close every lease of `session_id`; returns how many were closed.
    pub async fn close_all_for_session(&self, session_id: Uuid) -> usize {
        let doomed: Vec<(Uuid, Arc<dyn Lease>)> = {
            let mut map = self.lock();
            let ids: Vec<Uuid> = map
                .iter()
                .filter(|(_, e)| e.session_id == session_id)
                .map(|(id, _)| *id)
                .collect();
            ids.into_iter()
                .filter_map(|id| map.remove(&id).map(|e| (id, e.lease)))
                .collect()
        };
        let count = doomed.len();
        for (id, lease) in doomed {
            if let Err(e) = lease.close().await {
                tracing::warn!(lease = %id, error = %e, "lease close failed");
            }
        }
        count
    }

    /// Close every lease (application exit).
    pub async fn close_all(&self) {
        let all: Vec<(Uuid, Entry)> = self.lock().drain().collect();
        for (id, entry) in all {
            if let Err(e) = entry.lease.close().await {
                tracing::warn!(lease = %id, error = %e, "lease close failed");
            }
        }
    }

    /// Publish the current state of a lease on `lease://state` (call when it becomes broken).
    pub fn notify_state(&self, lease_id: Uuid) {
        if let Ok(lease) = self.get(lease_id) {
            emit_json(
                &*self.events,
                LEASE_STATE,
                &LeaseStateEvent {
                    lease_id,
                    state: lease.state(),
                },
            );
        }
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! A lease that records what happened to it.

    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use dbstudio_core::{
        ExecSummary, LeaseContext, LeaseOptions, QueryRequest, ResultSink, TxOptions,
    };

    use super::*;

    #[derive(Debug, Default)]
    pub struct FakeLease {
        pub closes: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Lease for FakeLease {
        async fn execute(&self, _: QueryRequest, _: &mut dyn ResultSink) -> Result<ExecSummary> {
            Err(Error::Unsupported("execute".into()))
        }
        async fn fetch_more(
            &self,
            _: Uuid,
            _: u64,
            _: Option<u32>,
            _: &mut dyn ResultSink,
        ) -> Result<ExecSummary> {
            Err(Error::NotFound("pending result".into()))
        }
        async fn discard_result(&self) -> Result<()> {
            Ok(())
        }
        async fn cancel(&self, _: Uuid) -> Result<()> {
            Ok(())
        }
        async fn begin(&self, _: TxOptions) -> Result<()> {
            Ok(())
        }
        async fn commit(&self) -> Result<()> {
            Ok(())
        }
        async fn rollback(&self) -> Result<()> {
            Ok(())
        }
        async fn set_auto_commit(&self, _: bool) -> Result<()> {
            Ok(())
        }
        async fn set_context(&self, _: LeaseContext) -> Result<()> {
            Ok(())
        }
        fn state(&self) -> LeaseState {
            LeaseState::initial(&LeaseOptions {
                database: None,
                schema: None,
                auto_commit: true,
            })
        }
        async fn ping(&self) -> Result<()> {
            Ok(())
        }
        async fn close(&self) -> Result<LeaseCloseReport> {
            self.closes.fetch_add(1, Ordering::SeqCst);
            Ok(LeaseCloseReport {
                rolled_back: false,
                forced: false,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::testing::FakeLease;
    use super::*;
    use crate::events::EventHub;

    fn manager() -> (LeaseManager, Arc<EventHub>) {
        let hub = EventHub::new();
        (LeaseManager::new(hub.clone()), hub)
    }

    fn fake(closes: &Arc<AtomicUsize>) -> Box<dyn Lease> {
        Box::new(FakeLease {
            closes: closes.clone(),
        })
    }

    #[tokio::test]
    async fn register_get_close() {
        let (leases, _) = manager();
        let closes = Arc::new(AtomicUsize::new(0));
        let session = Uuid::new_v4();
        let info = leases.register(session, fake(&closes)).unwrap();
        assert_eq!(info.session_id, session);
        assert!(info.state.auto_commit);
        let (owner, _) = leases.get_with_session(info.lease_id).unwrap();
        assert_eq!(owner, session);
        assert_eq!(leases.count_for_session(session), 1);

        assert!(!leases.close(info.lease_id).await.unwrap().rolled_back);
        assert_eq!(closes.load(Ordering::SeqCst), 1);
        assert!(matches!(leases.get(info.lease_id), Err(Error::NotFound(_))));
        // Closing again is harmless and does not close twice.
        leases.close(info.lease_id).await.unwrap();
        assert_eq!(closes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn at_most_32_leases_per_session() {
        let (leases, _) = manager();
        let closes = Arc::new(AtomicUsize::new(0));
        let session = Uuid::new_v4();
        for _ in 0..MAX_LEASES_PER_SESSION {
            leases.register(session, fake(&closes)).unwrap();
        }
        let err = leases.register(session, fake(&closes)).unwrap_err();
        assert_eq!(err.kind(), "busy");
        // Another session is unaffected.
        leases.register(Uuid::new_v4(), fake(&closes)).unwrap();
    }

    #[tokio::test]
    async fn closing_a_session_closes_only_its_leases() {
        let (leases, _) = manager();
        let closes = Arc::new(AtomicUsize::new(0));
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        leases.register(a, fake(&closes)).unwrap();
        leases.register(a, fake(&closes)).unwrap();
        let keep = leases.register(b, fake(&closes)).unwrap();
        assert_eq!(leases.close_all_for_session(a).await, 2);
        assert_eq!(closes.load(Ordering::SeqCst), 2);
        assert!(leases.get(keep.lease_id).is_ok());
        leases.close_all().await;
        assert_eq!(closes.load(Ordering::SeqCst), 3);
        assert_eq!(leases.count_for_session(b), 0);
    }

    #[tokio::test]
    async fn notify_state_publishes_lease_state_event() {
        let (leases, hub) = manager();
        let mut rx = hub.subscribe();
        let closes = Arc::new(AtomicUsize::new(0));
        let info = leases.register(Uuid::new_v4(), fake(&closes)).unwrap();
        leases.notify_state(info.lease_id);
        let ev = rx.try_recv().unwrap();
        assert_eq!(ev.name, "lease://state");
        assert_eq!(ev.payload["leaseId"], info.lease_id.to_string());
        assert_eq!(ev.payload["state"]["broken"], false);
        // Unknown lease: nothing is published.
        leases.notify_state(Uuid::new_v4());
        assert!(rx.try_recv().is_err());
    }
}
