//! `LeaseManager`: registry of the dedicated connections (leases) held by editor / console tabs.
//!
//! At most [`MAX_LEASES_PER_SESSION`] per session. Closing a session closes its leases first.
//! Package B owns this file.

#![allow(dead_code)] // scaffold: consumed by the editor package

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use dbstudio_core::{Error, Lease, LeaseCloseReport, LeaseOptions, LeaseState, Result, TxStatus};
use serde::Serialize;
use tokio::time::Instant;
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

/// Returned by `lease_reopen`: the lease keeps its id.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaseReopened {
    /// The reopened lease (same `lease_id`).
    pub lease: LeaseInfo,
    /// A transaction was open when the connection broke and its changes are lost.
    pub transaction_lost: bool,
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
    /// Last time a command used the lease (keepalive pings do not count).
    last_activity: Instant,
    /// Last keepalive ping sent to the lease.
    last_ping: Instant,
    /// Set when the connection under the lease is known to be dead (keepalive failed, or the
    /// session was reconnected while a transaction was open).
    lost: Option<Lost>,
}

/// What is known about a lease whose connection died.
#[derive(Debug, Clone)]
struct Lost {
    /// The state at the moment it was lost (a transaction may have been open).
    last_state: LeaseState,
}

/// What [`LeaseManager::survey_session`] found for one lease of a session.
#[derive(Debug, Clone)]
pub struct LeaseSurvey {
    /// The lease.
    pub lease_id: Uuid,
    /// Its state before the connection broke.
    pub state: LeaseState,
}

/// What to do with the leases of a session that was reconnected.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReopenReport {
    /// Idle leases that were reopened on the new connection (same ids).
    pub reopened: Vec<Uuid>,
    /// Leases with a transaction in flight: not reopened, waiting for `lease_reopen`.
    pub broken: Vec<Uuid>,
}

/// Options that recreate a lease in the state it was in.
fn options_of(state: &LeaseState) -> LeaseOptions {
    LeaseOptions {
        database: state.database.clone(),
        schema: state.schema.clone(),
        auto_commit: state.auto_commit,
    }
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
        map.insert(
            lease_id,
            Entry {
                session_id,
                lease,
                last_activity: Instant::now(),
                last_ping: Instant::now(),
                lost: None,
            },
        );
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
        let mut map = self.lock();
        let entry = map
            .get_mut(&lease_id)
            .ok_or_else(|| Error::NotFound(format!("lease {lease_id}")))?;
        // Every command goes through here, so this is the lease's activity clock.
        entry.last_activity = Instant::now();
        Ok((entry.session_id, Arc::clone(&entry.lease)))
    }

    /// State of a lease as the tabs should see it: a lease known to be lost is `broken` even
    /// when its connection task has not noticed yet.
    pub fn state_of(&self, lease_id: Uuid) -> Result<LeaseState> {
        let map = self.lock();
        let entry = map
            .get(&lease_id)
            .ok_or_else(|| Error::NotFound(format!("lease {lease_id}")))?;
        Ok(Self::visible_state(entry))
    }

    fn visible_state(entry: &Entry) -> LeaseState {
        let mut state = entry.lease.state();
        if entry.lost.is_some() {
            state.broken = true;
        }
        state
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
        if let Ok(state) = self.state_of(lease_id) {
            emit_json(
                &*self.events,
                LEASE_STATE,
                &LeaseStateEvent { lease_id, state },
            );
        }
    }

    /// The event sink this manager publishes on (shared with the session health monitor).
    pub fn emitter(&self) -> Arc<dyn EventEmitter> {
        Arc::clone(&self.events)
    }

    /// Ids and states of the leases of `session_id`.
    pub fn survey_session(&self, session_id: Uuid) -> Vec<LeaseSurvey> {
        self.lock()
            .iter()
            .filter(|(_, e)| e.session_id == session_id)
            .map(|(id, e)| LeaseSurvey {
                lease_id: *id,
                state: e
                    .lost
                    .as_ref()
                    .map_or_else(|| e.lease.state(), |lost| lost.last_state.clone()),
            })
            .collect()
    }

    /// Whether the lease is unusable and `lease_reopen` has something to do.
    pub fn needs_reopen(&self, lease_id: Uuid) -> Result<bool> {
        let map = self.lock();
        let entry = map
            .get(&lease_id)
            .ok_or_else(|| Error::NotFound(format!("lease {lease_id}")))?;
        Ok(entry.lost.is_some() || entry.lease.state().broken)
    }

    /// Record that the connection under a lease is dead and tell the tab. The state seen at
    /// that moment is kept: `lease_reopen` reports whether a transaction was lost.
    pub fn mark_lost(&self, lease_id: Uuid) {
        {
            let mut map = self.lock();
            let Some(entry) = map.get_mut(&lease_id) else {
                return;
            };
            if entry.lost.is_none() {
                entry.lost = Some(Lost {
                    last_state: entry.lease.state(),
                });
            }
        }
        self.notify_state(lease_id);
    }

    /// Put a freshly opened lease under an existing id (reopen after the connection broke).
    /// Returns whether a transaction was lost, and closes the dead lease.
    pub async fn replace(&self, lease_id: Uuid, new_lease: Box<dyn Lease>) -> Result<bool> {
        let new_lease: Arc<dyn Lease> = Arc::from(new_lease);
        let (old, lost_tx) = {
            let mut map = self.lock();
            let entry = map
                .get_mut(&lease_id)
                .ok_or_else(|| Error::NotFound(format!("lease {lease_id}")))?;
            let before = entry
                .lost
                .take()
                .map_or_else(|| entry.lease.state(), |l| l.last_state);
            let old = std::mem::replace(&mut entry.lease, new_lease);
            entry.last_activity = Instant::now();
            entry.last_ping = Instant::now();
            (old, before.transaction != TxStatus::Idle)
        };
        // The old connection is dead or being dropped; a failing close changes nothing.
        let _ = old.close().await;
        self.notify_state(lease_id);
        Ok(lost_tx)
    }

    /// The options to reopen `lease_id` with, taken from its last known state.
    pub fn reopen_options(&self, lease_id: Uuid) -> Result<(Uuid, LeaseOptions)> {
        let map = self.lock();
        let entry = map
            .get(&lease_id)
            .ok_or_else(|| Error::NotFound(format!("lease {lease_id}")))?;
        let state = entry
            .lost
            .as_ref()
            .map_or_else(|| entry.lease.state(), |l| l.last_state.clone());
        Ok((entry.session_id, options_of(&state)))
    }

    /// One keepalive round: ping every lease that is not running a statement and whose last
    /// command is at least `interval` old. A failing ping marks the lease lost. Returns the
    /// leases that were pinged.
    pub async fn keepalive_tick(&self, interval: Duration) -> Vec<Uuid> {
        let now = Instant::now();
        let due: Vec<(Uuid, Arc<dyn Lease>)> = self
            .lock()
            .iter()
            .filter(|(_, e)| {
                e.lost.is_none()
                    && now.saturating_duration_since(e.last_activity.max(e.last_ping)) >= interval
                    && {
                        let state = e.lease.state();
                        state.running.is_none() && !state.broken
                    }
            })
            .map(|(id, e)| (*id, Arc::clone(&e.lease)))
            .collect();
        let mut pinged = Vec::with_capacity(due.len());
        for (id, lease) in due {
            pinged.push(id);
            if let Some(entry) = self.lock().get_mut(&id) {
                entry.last_ping = Instant::now();
            }
            if let Err(e) = lease.ping().await {
                tracing::info!(lease = %id, error = %e, "lease keepalive failed");
                self.mark_lost(id);
            }
        }
        pinged
    }

    /// Run [`LeaseManager::keepalive_tick`] in the background. `interval` is asked before every
    /// round (the setting may change at runtime); `None` skips the round (keepalive off).
    pub fn spawn_keepalive(
        self: &Arc<Self>,
        interval: impl Fn() -> Option<Duration> + Send + Sync + 'static,
    ) {
        let manager = Arc::downgrade(self);
        crate::session::spawn_background(async move {
            loop {
                let Some(period) = interval() else {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    if manager.strong_count() == 0 {
                        return;
                    }
                    continue;
                };
                // Rounds are a quarter of the interval apart, so a lease is pinged at most
                // one quarter late.
                tokio::time::sleep((period / 4).max(Duration::from_millis(10))).await;
                let Some(manager) = manager.upgrade() else {
                    return;
                };
                manager.keepalive_tick(period).await;
            }
        });
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

    /// A lease whose keepalive behaviour the test scripts.
    #[derive(Debug, Default)]
    pub struct PingLease {
        pub pings: AtomicUsize,
        pub fail: std::sync::atomic::AtomicBool,
        pub running: std::sync::atomic::AtomicBool,
        pub in_transaction: std::sync::atomic::AtomicBool,
    }

    #[async_trait]
    impl Lease for PingLease {
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
            let mut state = LeaseState::initial(&LeaseOptions {
                database: Some("app".into()),
                schema: None,
                auto_commit: true,
            });
            if self.running.load(Ordering::SeqCst) {
                state.running = Some(Uuid::nil());
            }
            if self.in_transaction.load(Ordering::SeqCst) {
                state.transaction = dbstudio_core::TxStatus::Active;
            }
            state
        }
        async fn ping(&self) -> Result<()> {
            self.pings.fetch_add(1, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) {
                return Err(Error::Connection("gone".into()));
            }
            Ok(())
        }
        async fn close(&self) -> Result<LeaseCloseReport> {
            Ok(LeaseCloseReport {
                rolled_back: false,
                forced: false,
            })
        }
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

    // ---- keepalive ----

    use super::testing::PingLease;
    use std::sync::atomic::Ordering as AtomicOrdering;

    fn pinging() -> (Arc<PingLease>, Box<dyn Lease>) {
        let lease = Arc::new(PingLease::default());
        // `register` takes a Box; share the counters through a thin wrapper.
        struct Shared(Arc<PingLease>);
        #[async_trait::async_trait]
        impl Lease for Shared {
            async fn execute(
                &self,
                r: dbstudio_core::QueryRequest,
                s: &mut dyn dbstudio_core::ResultSink,
            ) -> Result<dbstudio_core::ExecSummary> {
                self.0.execute(r, s).await
            }
            async fn fetch_more(
                &self,
                q: Uuid,
                m: u64,
                b: Option<u32>,
                s: &mut dyn dbstudio_core::ResultSink,
            ) -> Result<dbstudio_core::ExecSummary> {
                self.0.fetch_more(q, m, b, s).await
            }
            async fn discard_result(&self) -> Result<()> {
                Ok(())
            }
            async fn cancel(&self, q: Uuid) -> Result<()> {
                self.0.cancel(q).await
            }
            async fn begin(&self, o: dbstudio_core::TxOptions) -> Result<()> {
                self.0.begin(o).await
            }
            async fn commit(&self) -> Result<()> {
                Ok(())
            }
            async fn rollback(&self) -> Result<()> {
                Ok(())
            }
            async fn set_auto_commit(&self, on: bool) -> Result<()> {
                self.0.set_auto_commit(on).await
            }
            async fn set_context(&self, c: dbstudio_core::LeaseContext) -> Result<()> {
                self.0.set_context(c).await
            }
            fn state(&self) -> LeaseState {
                self.0.state()
            }
            async fn ping(&self) -> Result<()> {
                self.0.ping().await
            }
            async fn close(&self) -> Result<LeaseCloseReport> {
                self.0.close().await
            }
        }
        (lease.clone(), Box::new(Shared(lease)))
    }

    const TICK: Duration = Duration::from_millis(60);

    #[tokio::test]
    async fn keepalive_pings_idle_leases_once_per_interval() {
        let (leases, _) = manager();
        let (probe, lease) = pinging();
        let info = leases.register(Uuid::new_v4(), lease).unwrap();
        // Not idle long enough yet.
        assert!(leases.keepalive_tick(TICK).await.is_empty());
        tokio::time::sleep(TICK + Duration::from_millis(20)).await;
        assert_eq!(leases.keepalive_tick(TICK).await, vec![info.lease_id]);
        assert_eq!(probe.pings.load(AtomicOrdering::SeqCst), 1);
        // A ping does not make the next one due at once.
        assert!(leases.keepalive_tick(TICK).await.is_empty());
        tokio::time::sleep(TICK + Duration::from_millis(20)).await;
        assert_eq!(leases.keepalive_tick(TICK).await.len(), 1);
        assert_eq!(probe.pings.load(AtomicOrdering::SeqCst), 2);
        assert!(!leases.state_of(info.lease_id).unwrap().broken);
    }

    #[tokio::test]
    async fn keepalive_skips_running_leases_and_commands_count_as_activity() {
        let (leases, _) = manager();
        let (running, lease) = pinging();
        let a = leases.register(Uuid::new_v4(), lease).unwrap();
        let (busy, lease) = pinging();
        let b = leases.register(Uuid::new_v4(), lease).unwrap();
        busy.running.store(true, AtomicOrdering::SeqCst);
        tokio::time::sleep(TICK + Duration::from_millis(20)).await;
        // Using lease `a` (any command goes through `get`) renews its activity.
        leases.get(a.lease_id).unwrap();
        assert!(leases.keepalive_tick(TICK).await.is_empty());
        assert_eq!(running.pings.load(AtomicOrdering::SeqCst), 0);
        assert_eq!(busy.pings.load(AtomicOrdering::SeqCst), 0);
        busy.running.store(false, AtomicOrdering::SeqCst);
        assert_eq!(leases.keepalive_tick(TICK).await, vec![b.lease_id]);
    }

    #[tokio::test]
    async fn a_failing_keepalive_marks_the_lease_lost_and_publishes_its_state() {
        let (leases, hub) = manager();
        let mut rx = hub.subscribe();
        let (probe, lease) = pinging();
        let info = leases.register(Uuid::new_v4(), lease).unwrap();
        probe.in_transaction.store(true, AtomicOrdering::SeqCst);
        probe.fail.store(true, AtomicOrdering::SeqCst);
        tokio::time::sleep(TICK + Duration::from_millis(20)).await;
        leases.keepalive_tick(TICK).await;
        let ev = rx.try_recv().unwrap();
        assert_eq!(ev.name, "lease://state");
        assert_eq!(ev.payload["state"]["broken"], true);
        assert!(leases.state_of(info.lease_id).unwrap().broken);
        assert!(leases.needs_reopen(info.lease_id).unwrap());
        // Not pinged again once lost.
        tokio::time::sleep(TICK + Duration::from_millis(20)).await;
        assert!(leases.keepalive_tick(TICK).await.is_empty());
        // Reopening reports the lost transaction and clears the flag.
        let (probe2, fresh) = pinging();
        assert!(leases.replace(info.lease_id, fresh).await.unwrap());
        assert!(!leases.needs_reopen(info.lease_id).unwrap());
        assert!(!leases.state_of(info.lease_id).unwrap().broken);
        drop(probe2);
    }

    #[tokio::test]
    async fn the_background_keepalive_follows_the_interval_and_can_be_turned_off() {
        let (leases, _) = manager();
        let leases = Arc::new(leases);
        let (probe, lease) = pinging();
        leases.register(Uuid::new_v4(), lease).unwrap();
        let on = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let flag = on.clone();
        leases.spawn_keepalive(move || {
            flag.load(AtomicOrdering::SeqCst)
                .then_some(Duration::from_millis(80))
        });
        tokio::time::sleep(Duration::from_millis(450)).await;
        let pings = probe.pings.load(AtomicOrdering::SeqCst);
        assert!((2..=6).contains(&pings), "pings = {pings}");
        // Turning it off stops the pings (the loop re-checks every 30 s, so the in-flight
        // round may still ping once more).
        on.store(false, AtomicOrdering::SeqCst);
        tokio::time::sleep(Duration::from_millis(120)).await;
        let after_off = probe.pings.load(AtomicOrdering::SeqCst);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(probe.pings.load(AtomicOrdering::SeqCst), after_off);
    }
}
