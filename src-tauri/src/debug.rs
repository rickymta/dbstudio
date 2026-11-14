//! `DebugManager`: the live SQL debug sessions of the application.
//!
//! The engine (`dbstudio-debug`) is independent of Tauri; this module is the host side of the
//! contract (design `docs/design/sql-debugger.md` section 3.7):
//!
//! * [`DebugManager`] - registry `debug_id -> session`, one session per lease (the lease is
//!   claimed with `LeaseManager::claim_debug`, so editor commands answer `Busy` meanwhile), the
//!   hooks that end a session (lease closed or broken, session closed) and the idempotent stop.
//! * [`HostGate`] - the engine's [`StepGate`]: every step goes through `guard::run_guard` with the
//!   dialect of the lease (read-only blocks, production asks for a one-time token).
//! * [`EventSink`] - the engine's [`DebugSink`]: `debug://state` (throttled while running) and
//!   `debug://output`, ordered by the engine's `seq`.
//! * [`HostProxy`] - the engine's [`ProxyOpener`]: a second, hidden lease of the same session.
//!
//! The engine itself is reached through the small [`DebugBackend`] / [`SessionControl`] seam so
//! the glue is tested with a fake engine.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dbstudio_core::debug::{
    Breakpoint, BreakpointStatus, DebugAction, DebugEndAction, DebugEndReport, DebugOutputEvent,
    DebugStartRequest, DebugState, DebugStatus, DebugSupport, DebugVariable, EvalResult,
    PauseReason, RoutineBreakpoint, RoutineDebugSupport, RoutineRef, RoutineSource, SourceRef,
    TerminateReason, WatchResult, WatchSpec,
};
use dbstudio_core::{
    Capabilities, ConfirmationGrant, ConnectionConfig, DbKind, Error, Lease, LeaseOptions,
    ObjectKind, QueryRequest, Result, ScriptDialect, StatementClass, StatementOutcome, Value,
    execute_collect,
};
use dbstudio_debug::{
    DebugEngine, DebugHandle, DebugSettings, DebugSink, DialectProfile, GateRequest, GatedText,
    ProxyOpener, StepGate, StepRecord,
};
use dbstudio_store::{ConnectionService, NewHistoryEntry};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::events::{DEBUG_OUTPUT, DEBUG_STATE, EventEmitter, emit_json};
use crate::guard::{self, ConfirmTokens, GuardInput};
use crate::lease::{LeaseManager, MAX_LEASES_PER_SESSION};
use crate::session::SessionManager;
use crate::state::AppState;

/// Sessions per application.
pub const MAX_DEBUG_SESSIONS: usize = 8;

/// Shortest gap between two `debug://state` events while a session is `Running`.
const STATE_THROTTLE: Duration = Duration::from_millis(50);
/// How often a session checks that its lease and session are still alive.
const WATCHDOG_POLL: Duration = Duration::from_millis(500);
/// Finished sessions whose last state/report stay available (idempotent stop, late snapshot).
const ENDED_KEPT: usize = 16;
/// Bound text -> typed text pairs kept for the history of the steps.
const UNBOUND_KEPT: usize = 64;

// ---------------------------------------------------------------------------------------------
// Engine seam
// ---------------------------------------------------------------------------------------------

/// Client of one running session (the real one wraps `DebugHandle`).
#[async_trait]
pub trait SessionControl: Send + Sync {
    /// Id of the session.
    fn debug_id(&self) -> Uuid;
    /// False once the engine task is gone without ending the session (a panic).
    fn alive(&self) -> bool {
        true
    }
    /// Latest state.
    async fn snapshot(&self) -> Result<DebugState>;
    /// Resume / step / run to cursor / jump / pause.
    async fn act(
        &self,
        action: DebugAction,
        confirm: Option<ConfirmationGrant>,
    ) -> Result<DebugState>;
    /// End the session.
    async fn stop(&self, end: Option<DebugEndAction>) -> Result<DebugEndReport>;
    /// Evaluate in the read-only frame.
    async fn evaluate(&self, expr: &str, frame: Option<u32>, echo: bool) -> Result<EvalResult>;
    /// Replace the breakpoints of one source.
    async fn set_breakpoints(
        &self,
        source: SourceRef,
        breakpoints: Vec<Breakpoint>,
    ) -> Result<Vec<BreakpointStatus>>;
    /// Mute / unmute breakpoints.
    async fn set_muted(&self, muted: bool) -> Result<DebugState>;
    /// Change a variable.
    async fn set_variable(&self, reference: &str, value_text: &str) -> Result<DebugVariable>;
    /// Children of a variable.
    async fn variable_children(&self, reference: &str) -> Result<Vec<DebugVariable>>;
    /// Replace the watches.
    async fn set_watches(&self, watches: Vec<WatchSpec>) -> Result<Vec<WatchResult>>;
    /// Select a call-stack frame.
    async fn select_frame(&self, frame: u32) -> Result<DebugState>;
    /// Source of a routine through the session's proxy.
    async fn routine_source(&self, routine: &RoutineRef) -> Result<RoutineSource>;
}

/// Everything the engine needs to start a session.
pub struct StartParams {
    /// Facts about the server.
    pub profile: DialectProfile,
    /// The lease to debug.
    pub lease: Arc<dyn Lease>,
    /// Guard per step.
    pub gate: Arc<dyn StepGate>,
    /// Event sink.
    pub sink: Arc<dyn DebugSink>,
    /// Proxy connection opener.
    pub proxy: Arc<dyn ProxyOpener>,
    /// Limits from the application settings.
    pub settings: DebugSettings,
    /// The request of the frontend.
    pub request: DebugStartRequest,
}

/// Starts sessions: the real engine, or a fake in tests.
#[async_trait]
pub trait DebugBackend: Send + Sync {
    /// Start one session.
    async fn start(&self, params: StartParams) -> Result<Arc<dyn SessionControl>>;
}

struct EngineBackend;

#[async_trait]
impl DebugBackend for EngineBackend {
    async fn start(&self, p: StartParams) -> Result<Arc<dyn SessionControl>> {
        let handle = DebugEngine::start(
            p.profile, p.lease, p.gate, p.sink, p.proxy, p.settings, p.request,
        )
        .await?;
        Ok(Arc::new(HandleControl(handle)))
    }
}

struct HandleControl(DebugHandle);

#[async_trait]
impl SessionControl for HandleControl {
    fn debug_id(&self) -> Uuid {
        self.0.debug_id()
    }
    fn alive(&self) -> bool {
        self.0.is_alive()
    }
    async fn snapshot(&self) -> Result<DebugState> {
        self.0.snapshot().await
    }
    async fn act(
        &self,
        action: DebugAction,
        confirm: Option<ConfirmationGrant>,
    ) -> Result<DebugState> {
        self.0.act(action, confirm).await
    }
    async fn stop(&self, end: Option<DebugEndAction>) -> Result<DebugEndReport> {
        self.0.stop(end).await
    }
    async fn evaluate(&self, expr: &str, frame: Option<u32>, echo: bool) -> Result<EvalResult> {
        self.0.evaluate(expr, frame, echo).await
    }
    async fn set_breakpoints(
        &self,
        source: SourceRef,
        breakpoints: Vec<Breakpoint>,
    ) -> Result<Vec<BreakpointStatus>> {
        self.0.set_breakpoints(source, breakpoints).await
    }
    async fn set_muted(&self, muted: bool) -> Result<DebugState> {
        self.0.set_muted(muted).await
    }
    async fn set_variable(&self, reference: &str, value_text: &str) -> Result<DebugVariable> {
        self.0.set_variable(reference, value_text).await
    }
    async fn variable_children(&self, reference: &str) -> Result<Vec<DebugVariable>> {
        self.0.variable_children(reference).await
    }
    async fn set_watches(&self, watches: Vec<WatchSpec>) -> Result<Vec<WatchResult>> {
        self.0.set_watches(watches).await
    }
    async fn select_frame(&self, frame: u32) -> Result<DebugState> {
        self.0.select_frame(frame).await
    }
    async fn routine_source(&self, routine: &RoutineRef) -> Result<RoutineSource> {
        self.0.routine_source(routine).await
    }
}

// ---------------------------------------------------------------------------------------------
// HostGate
// ---------------------------------------------------------------------------------------------

/// The guard of the host as the engine's per-step gate.
pub struct HostGate {
    tokens: Arc<ConfirmTokens>,
    sessions: Arc<SessionManager>,
    connections: Arc<ConnectionService>,
    session_id: Uuid,
    lease_id: Uuid,
    record_history: bool,
    /// Bound text that passed the guard -> the text as typed, so the history never stores
    /// parameter values.
    unbound: Mutex<VecDeque<(String, String)>>,
}

impl HostGate {
    /// Gate for the lease `lease_id` of `session_id`.
    pub fn new(
        state: &AppState,
        session_id: Uuid,
        lease_id: Uuid,
        record_history: bool,
    ) -> HostGate {
        HostGate {
            tokens: Arc::clone(&state.confirm),
            sessions: Arc::clone(&state.sessions),
            connections: Arc::clone(&state.connections),
            session_id,
            lease_id,
            record_history,
            unbound: Mutex::new(VecDeque::new()),
        }
    }

    fn config(&self) -> Result<ConnectionConfig> {
        self.sessions
            .get_with_config(self.session_id)
            .map(|(_, c)| c)
    }

    fn target(&self) -> String {
        format!("lease:{}", self.lease_id)
    }

    fn remember(&self, bound: &str, typed: &str) {
        if bound == typed {
            return;
        }
        let mut q = self.unbound.lock().unwrap_or_else(|e| e.into_inner());
        if q.len() >= UNBOUND_KEPT {
            q.pop_front();
        }
        q.push_back((bound.to_string(), typed.to_string()));
    }

    fn typed_text(&self, bound: &str) -> String {
        let q = self.unbound.lock().unwrap_or_else(|e| e.into_inner());
        q.iter()
            .rev()
            .find(|(b, _)| b == bound)
            .map_or_else(|| bound.to_string(), |(_, t)| t.clone())
    }
}

impl StepGate for HostGate {
    fn check(&self, req: GateRequest<'_>) -> Result<GatedText> {
        let config = self.config()?;
        let (read_only, production) = guard::policy_flags(&config);
        let confirm_text = guard::confirm_text_for(&config, req.database);
        let target = self.target();
        // Same context the editor uses: a token issued in one database/schema cannot be spent
        // after the step moved to another.
        let context = format!(
            "{}/{}",
            req.database.unwrap_or_default(),
            req.schema.unwrap_or_default()
        );
        let guarded = guard::run_guard(
            &self.tokens,
            &GuardInput {
                dialect: req.dialect,
                text: req.text,
                params: req.params,
                read_only,
                production,
                confirm_text: &confirm_text,
                confirm: req.confirm,
                target_id: &target,
                context: &context,
            },
        )?;
        self.remember(&guarded.bound.text, req.text);
        Ok(GatedText {
            text: guarded.bound.text,
            map: guarded.bound.map,
            analysis: guarded.analysis,
        })
    }

    fn check_read_only(&self, dialect: ScriptDialect, text: &str) -> Result<GatedText> {
        let config = self.config()?;
        let confirm_text = guard::confirm_text_for(&config, None);
        let target = self.target();
        let guarded = guard::run_guard(
            &self.tokens,
            &GuardInput {
                dialect,
                text,
                params: &[],
                read_only: true,
                production: false,
                confirm_text: &confirm_text,
                confirm: None,
                target_id: &target,
                context: "",
            },
        )?;
        let real: Vec<_> = guarded
            .analysis
            .statements
            .iter()
            .filter(|s| s.class != StatementClass::Empty)
            .collect();
        let only_read = real.len() == 1
            && matches!(
                real[0].class,
                StatementClass::Read | StatementClass::Explain { writes: false, .. }
            );
        if !only_read {
            return Err(Error::ReadOnlyViolation {
                message: "an expression must be exactly one read-only statement".into(),
                position: None,
            });
        }
        Ok(GatedText {
            text: guarded.bound.text,
            map: guarded.bound.map,
            analysis: guarded.analysis,
        })
    }

    fn record(&self, step: &StepRecord) {
        if !self.record_history {
            return;
        }
        let Ok(config) = self.config() else { return };
        let svc = Arc::clone(&self.connections);
        let text = dbstudio_sql::redact::redact(step.dialect, &self.typed_text(&step.text));
        let entry = NewHistoryEntry {
            connection_id: config.id,
            database: step.database.clone().or_else(|| config.database.clone()),
            kind: "query".to_string(),
            text,
            started_at: iso_ago(step.elapsed_ms),
            status: if step.error.is_some() { "error" } else { "ok" }.to_string(),
            elapsed_ms: Some(step.elapsed_ms),
            statements: Some(1),
            rows: step.rows,
            error_message: step
                .error
                .as_ref()
                .map(|e| e.chars().take(500).collect::<String>()),
        };
        // Recording never fails a step: store errors are only logged.
        tokio::task::spawn_blocking(move || {
            let settings = match svc.settings() {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "debug history: cannot read settings");
                    return;
                }
            };
            if !settings.history_enabled {
                return;
            }
            let connection_id = entry.connection_id;
            if let Err(e) = svc.store().history_append(&entry) {
                tracing::warn!(error = %e, "debug history: cannot record a step");
                return;
            }
            if let Err(e) = svc
                .store()
                .history_trim(connection_id, settings.history_max_per_connection)
            {
                tracing::warn!(error = %e, "debug history: cannot trim");
            }
        });
    }
}

fn iso_ago(elapsed_ms: u64) -> String {
    let ago = time::Duration::milliseconds(i64::try_from(elapsed_ms).unwrap_or(i64::MAX / 2));
    (OffsetDateTime::now_utc() - ago)
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

// ---------------------------------------------------------------------------------------------
// EventSink
// ---------------------------------------------------------------------------------------------

type EndHook = Arc<dyn Fn(&DebugState) + Send + Sync>;

#[derive(Default)]
struct SinkInner {
    last: Option<DebugState>,
    last_emit: Option<Instant>,
    last_running: bool,
    pending: Option<DebugState>,
    flush_scheduled: bool,
}

/// The engine's event sink: `debug://state` and `debug://output` through the event hub.
///
/// While a session is `Running` the engine may change the step many times a second; those
/// state events are coalesced to one per [`STATE_THROTTLE`] (the latest wins). Every other
/// transition is delivered at once, and a pending state is always flushed before the next
/// output event so the shared `seq` stays in order.
pub struct EventSink {
    emitter: Arc<dyn EventEmitter>,
    inner: Mutex<SinkInner>,
    on_end: Mutex<Option<EndHook>>,
    /// Why the host is ending the session; replaces the engine's plain `Stopped`.
    end_reason: Mutex<Option<TerminateReason>>,
    throttle: Duration,
    me: std::sync::Weak<EventSink>,
}

fn is_running(s: &DebugState) -> bool {
    matches!(s.status, DebugStatus::Running { .. })
}

fn is_terminated(s: &DebugState) -> bool {
    matches!(s.status, DebugStatus::Terminated { .. })
}

impl EventSink {
    /// Sink emitting through `emitter`.
    pub fn new(emitter: Arc<dyn EventEmitter>) -> Arc<EventSink> {
        EventSink::with_throttle(emitter, STATE_THROTTLE)
    }

    fn with_throttle(emitter: Arc<dyn EventEmitter>, throttle: Duration) -> Arc<EventSink> {
        Arc::new_cyclic(|me| EventSink {
            emitter,
            inner: Mutex::new(SinkInner::default()),
            on_end: Mutex::new(None),
            end_reason: Mutex::new(None),
            throttle,
            me: me.clone(),
        })
    }

    fn set_on_end(&self, hook: EndHook) {
        *self.on_end.lock().unwrap_or_else(|e| e.into_inner()) = Some(hook);
    }

    fn set_end_reason(&self, reason: TerminateReason) {
        *self.end_reason.lock().unwrap_or_else(|e| e.into_inner()) = Some(reason);
    }

    fn lock(&self) -> MutexGuard<'_, SinkInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Latest state the engine (or the manager) published.
    pub fn last_state(&self) -> Option<DebugState> {
        self.lock().last.clone()
    }

    fn emit_state(&self, inner: &mut SinkInner, state: &DebugState) {
        inner.last_emit = Some(Instant::now());
        inner.last_running = is_running(state);
        emit_json(&*self.emitter, DEBUG_STATE, state);
    }

    fn flush_pending(&self, inner: &mut SinkInner) {
        if let Some(p) = inner.pending.take() {
            self.emit_state(inner, &p);
        }
    }

    /// Publish a state the manager made up (the session ended because of the host, not the
    /// engine). Goes out at once.
    fn publish(&self, state: DebugState) {
        let mut inner = self.lock();
        inner.pending = None;
        inner.last = Some(state.clone());
        self.emit_state(&mut inner, &state);
    }
}

impl DebugSink for EventSink {
    fn state(&self, state: &DebugState) {
        let terminated = is_terminated(state);
        // One Terminated event per session: when the host ended it, the engine's own
        // Terminated(Stopped) carries the host's reason instead of a second event following it.
        let rewritten;
        let state = match (&state.status, terminated) {
            (
                DebugStatus::Terminated {
                    reason: TerminateReason::Stopped,
                    report,
                },
                true,
            ) => match *self.end_reason.lock().unwrap_or_else(|e| e.into_inner()) {
                Some(reason) => {
                    let mut st = state.clone();
                    st.status = DebugStatus::Terminated {
                        reason,
                        report: *report,
                    };
                    rewritten = st;
                    &rewritten
                }
                None => state,
            },
            _ => state,
        };
        {
            let mut inner = self.lock();
            inner.last = Some(state.clone());
            let throttled = is_running(state)
                && inner.last_running
                && inner.last_emit.is_some_and(|t| t.elapsed() < self.throttle);
            if throttled {
                inner.pending = Some(state.clone());
                if !inner.flush_scheduled {
                    inner.flush_scheduled = true;
                    let wait = self
                        .throttle
                        .saturating_sub(inner.last_emit.map_or(Duration::ZERO, |t| t.elapsed()));
                    match (self.me.upgrade(), tokio::runtime::Handle::try_current()) {
                        (Some(me), Ok(rt)) => {
                            rt.spawn(async move {
                                tokio::time::sleep(wait).await;
                                let mut inner = me.lock();
                                inner.flush_scheduled = false;
                                me.flush_pending(&mut inner);
                            });
                        }
                        // No runtime to wait on: deliver now rather than lose the update.
                        _ => {
                            inner.flush_scheduled = false;
                            self.flush_pending(&mut inner);
                        }
                    }
                }
                return;
            }
            inner.pending = None;
            self.emit_state(&mut inner, state);
        }
        if terminated {
            let hook = self
                .on_end
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if let Some(hook) = hook {
                hook(state);
            }
        }
    }

    fn output(&self, event: DebugOutputEvent) {
        let mut inner = self.lock();
        self.flush_pending(&mut inner);
        emit_json(&*self.emitter, DEBUG_OUTPUT, &event);
    }
}

// ---------------------------------------------------------------------------------------------
// HostProxy
// ---------------------------------------------------------------------------------------------

/// Opens the hidden second connection of a routine debugger: a lease of the same session
/// (same TLS / SSH / credentials), registered in the lease manager (it counts toward the
/// per-session limit and closes with the session) but never shown to the frontend.
pub struct HostProxy {
    sessions: Arc<SessionManager>,
    leases: Arc<LeaseManager>,
    session_id: Uuid,
    opened: Mutex<Vec<Uuid>>,
}

impl HostProxy {
    /// Opener for the session `session_id`.
    pub fn new(state: &AppState, session_id: Uuid) -> HostProxy {
        HostProxy {
            sessions: Arc::clone(&state.sessions),
            leases: Arc::clone(&state.leases),
            session_id,
            opened: Mutex::new(Vec::new()),
        }
    }

    /// Number of proxy leases opened and not yet closed.
    pub fn open_count(&self) -> usize {
        self.opened.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Close every proxy lease.
    pub async fn close_all(&self) {
        let ids: Vec<Uuid> =
            std::mem::take(&mut *self.opened.lock().unwrap_or_else(|e| e.into_inner()));
        for id in ids {
            if let Err(e) = self.leases.close(id).await {
                tracing::debug!(error = %e, "closing a debug proxy lease failed");
            }
        }
    }
}

#[async_trait]
impl ProxyOpener for HostProxy {
    async fn open_proxy(&self, database: Option<String>) -> Result<Arc<dyn Lease>> {
        let session = self.sessions.get(self.session_id)?;
        if self.leases.count_for_session(self.session_id) >= MAX_LEASES_PER_SESSION {
            return Err(Error::Busy(format!(
                "a session can hold at most {MAX_LEASES_PER_SESSION} leases"
            )));
        }
        let lease = session
            .open_lease(LeaseOptions {
                database,
                schema: None,
                auto_commit: true,
            })
            .await?;
        let info = self.leases.register(self.session_id, lease)?;
        self.opened
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(info.lease_id);
        self.leases.get(info.lease_id)
    }
}

// ---------------------------------------------------------------------------------------------
// DebugManager
// ---------------------------------------------------------------------------------------------

/// One live session.
struct Entry {
    debug_id: Uuid,
    lease_id: Uuid,
    session_id: Uuid,
    control: Arc<dyn SessionControl>,
    sink: Arc<EventSink>,
    proxy: Arc<HostProxy>,
    leases: Arc<LeaseManager>,
    /// The request as it stands now (breakpoints, watches, mute follow the user's edits) so
    /// Restart can start again with them.
    request: Mutex<DebugStartRequest>,
    cleaned: AtomicBool,
}

/// What stays of a finished session.
#[derive(Clone)]
struct Ended {
    report: Option<DebugEndReport>,
    state: Option<DebugState>,
}

#[derive(Default)]
struct Registry {
    live: HashMap<Uuid, Arc<Entry>>,
    /// Sessions being started (reserved against the limit).
    starting: usize,
    ended: VecDeque<(Uuid, Ended)>,
    support: HashMap<(Uuid, Option<String>), DebugSupport>,
}

struct Inner {
    reg: Mutex<Registry>,
    backend: Mutex<Arc<dyn DebugBackend>>,
    poll: Mutex<Duration>,
}

impl Inner {
    fn reg(&self) -> MutexGuard<'_, Registry> {
        self.reg.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn backend(&self) -> Arc<dyn DebugBackend> {
        Arc::clone(&self.backend.lock().unwrap_or_else(|e| e.into_inner()))
    }

    fn poll(&self) -> Duration {
        *self.poll.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Forget a finished session, free its lease and close its proxy leases. Idempotent.
    fn cleanup(&self, entry: &Arc<Entry>, report: Option<DebugEndReport>) {
        if entry.cleaned.swap(true, Ordering::SeqCst) {
            return;
        }
        {
            let mut reg = self.reg();
            reg.live.remove(&entry.debug_id);
            reg.support.retain(|(lease, _), _| *lease != entry.lease_id);
            reg.ended.push_back((
                entry.debug_id,
                Ended {
                    report,
                    state: entry.sink.last_state(),
                },
            ));
            while reg.ended.len() > ENDED_KEPT {
                reg.ended.pop_front();
            }
        }
        entry.leases.release_debug(entry.lease_id, entry.debug_id);
        if entry.proxy.open_count() > 0 {
            let proxy = Arc::clone(&entry.proxy);
            if let Ok(rt) = tokio::runtime::Handle::try_current() {
                rt.spawn(async move { proxy.close_all().await });
            }
        }
    }

    /// End a session because the host lost what it stands on: roll back, then tell the frontend
    /// the real reason (the engine only knows it was stopped).
    async fn terminate(&self, entry: &Arc<Entry>, reason: TerminateReason) {
        if entry.cleaned.load(Ordering::SeqCst) {
            return;
        }
        entry.sink.set_end_reason(reason);
        let report = entry
            .control
            .stop(Some(DebugEndAction::Rollback))
            .await
            .ok();
        // The engine reports its end through the sink (carrying `reason`); only when it did
        // not, the manager makes the final state up.
        if let Some(mut last) = entry.sink.last_state()
            && !is_terminated(&last)
        {
            last.seq += 1;
            last.status = DebugStatus::Terminated { reason, report };
            entry.sink.publish(last);
        }
        self.cleanup(entry, report);
    }

    fn find_by_lease(&self, lease_id: Uuid) -> Option<Arc<Entry>> {
        self.reg()
            .live
            .values()
            .find(|e| e.lease_id == lease_id)
            .cloned()
    }
}

/// Registry of debug sessions (`debug_id` -> session, `lease_id` -> `debug_id`).
pub struct DebugManager {
    inner: Arc<Inner>,
}

impl Default for DebugManager {
    fn default() -> Self {
        DebugManager::new()
    }
}

impl std::fmt::Debug for DebugManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DebugManager")
            .field("sessions", &self.count())
            .finish()
    }
}

/// How the watchdog sees a session's surroundings.
fn lost_reason(
    sessions: &SessionManager,
    leases: &LeaseManager,
    session_id: Uuid,
    lease_id: Uuid,
) -> Option<TerminateReason> {
    if sessions.get(session_id).is_err() {
        return Some(TerminateReason::SessionClosed);
    }
    match leases.state_of(lease_id) {
        Ok(s) if !s.broken => None,
        _ => Some(TerminateReason::LeaseLost),
    }
}

impl DebugManager {
    /// Empty registry running the real engine.
    pub fn new() -> Self {
        DebugManager {
            inner: Arc::new(Inner {
                reg: Mutex::new(Registry::default()),
                backend: Mutex::new(Arc::new(EngineBackend)),
                poll: Mutex::new(WATCHDOG_POLL),
            }),
        }
    }

    /// Replace the engine (tests).
    #[cfg(test)]
    pub fn with_backend(backend: Arc<dyn DebugBackend>) -> Self {
        let m = DebugManager::new();
        *m.inner.backend.lock().unwrap() = backend;
        m
    }

    /// Change how often sessions check their lease (tests).
    #[cfg(test)]
    pub fn set_poll(&self, d: Duration) {
        *self.inner.poll.lock().unwrap() = d;
    }

    /// Number of live sessions.
    pub fn count(&self) -> usize {
        self.inner.reg().live.len()
    }

    fn entry(&self, debug_id: Uuid) -> Result<Arc<Entry>> {
        self.inner
            .reg()
            .live
            .get(&debug_id)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("debug session {debug_id}")))
    }

    fn ended(&self, debug_id: Uuid) -> Option<Ended> {
        self.inner
            .reg()
            .ended
            .iter()
            .find(|(id, _)| *id == debug_id)
            .map(|(_, e)| e.clone())
    }

    /// The session holding `lease_id`, if any.
    #[allow(dead_code)] // used by the frontend-facing hooks of other packages
    pub fn session_of_lease(&self, lease_id: Uuid) -> Option<Uuid> {
        self.inner.find_by_lease(lease_id).map(|e| e.debug_id)
    }

    /// Start a session on the lease of `request` (design 2.1): claims the lease, builds the
    /// gate / sink / proxy of this lease and hands them to the engine.
    pub async fn start(
        &self,
        state: &AppState,
        mut request: DebugStartRequest,
    ) -> Result<DebugState> {
        let lease_id = request.lease_id;
        let (session_id, lease) = state.leases.get_with_session(lease_id)?;
        let (session, config) = state.sessions.get_with_config(session_id)?;
        // The routine breakpoints of the connection live in the store: the frontend loads them
        // only when a routine tab opens, so the engine starts with all of them.
        let svc = state.connections.clone();
        let connection_id = config.id;
        let stored =
            tokio::task::spawn_blocking(move || svc.store().routine_breakpoints_get(connection_id))
                .await
                .map_err(|e| Error::Other(format!("store task failed: {e}")))
                .and_then(|r| r);
        match stored {
            Ok(list) => merge_stored_routine_breakpoints(&mut request, list),
            Err(e) => tracing::warn!(error = %e, "stored routine breakpoints not loaded"),
        }
        let lease_state = state.leases.state_of(lease_id)?;
        if lease_state.running.is_some() {
            return Err(Error::Busy("the lease is running a statement".into()));
        }
        // Reserve a place first so two starts cannot both pass the limit.
        {
            let mut reg = self.inner.reg();
            if reg.live.len() + reg.starting >= MAX_DEBUG_SESSIONS {
                return Err(Error::Busy(format!(
                    "at most {MAX_DEBUG_SESSIONS} debug sessions at a time"
                )));
            }
            reg.starting += 1;
        }
        let result = self
            .start_reserved(state, request, session_id, lease, &session, &config)
            .await;
        self.inner.reg().starting -= 1;
        result
    }

    async fn start_reserved(
        &self,
        state: &AppState,
        request: DebugStartRequest,
        session_id: Uuid,
        lease: Arc<dyn Lease>,
        session: &Arc<dyn dbstudio_core::Session>,
        config: &ConnectionConfig,
    ) -> Result<DebugState> {
        let lease_id = request.lease_id;
        // The engine picks the id of the session; hold the lease under a provisional one until
        // it is known.
        let provisional = Uuid::new_v4();
        state.leases.claim_debug(lease_id, provisional)?;
        match self
            .launch(state, request, session_id, lease, session, config)
            .await
        {
            Ok((entry, first)) => {
                if entry.debug_id != provisional {
                    if let Err(e) =
                        state
                            .leases
                            .transfer_debug(lease_id, provisional, entry.debug_id)
                    {
                        let _ = entry.control.stop(Some(DebugEndAction::Rollback)).await;
                        entry.proxy.close_all().await;
                        return Err(e);
                    }
                    // The session may have ended inside `launch` (its cleanup could not release
                    // a lease still held under the provisional id): free it now.
                    if entry.cleaned.load(Ordering::SeqCst) {
                        state.leases.release_debug(lease_id, entry.debug_id);
                    }
                }
                Ok(first)
            }
            Err(e) => {
                state.leases.release_debug(lease_id, provisional);
                Err(e)
            }
        }
    }

    async fn launch(
        &self,
        state: &AppState,
        request: DebugStartRequest,
        session_id: Uuid,
        lease: Arc<dyn Lease>,
        session: &Arc<dyn dbstudio_core::Session>,
        config: &ConnectionConfig,
    ) -> Result<(Arc<Entry>, DebugState)> {
        let lease_id = request.lease_id;
        let settings = state.connections.settings()?;
        let info = session.server_info().await.unwrap_or_default();
        let caps = session_capabilities(state, session, config.kind);
        let profile = DialectProfile {
            kind: config.kind,
            flavor: info.extra.get("flavor").cloned(),
            version: (!info.version.is_empty()).then(|| info.version.clone()),
            dialect: lease
                .script_dialect()
                .unwrap_or_else(|| session.script_dialect()),
            caps,
        };
        let gate = Arc::new(HostGate::new(
            state,
            session_id,
            lease_id,
            request.options.record_history,
        ));
        let sink = EventSink::new(Arc::clone(&state.events) as Arc<dyn EventEmitter>);
        let proxy = Arc::new(HostProxy::new(state, session_id));
        let engine_settings = DebugSettings {
            tx_warn_secs: settings.debug_tx_warn_secs,
            paused_ttl_secs: settings.debug_paused_ttl_secs,
            watch_timeout_ms: settings.debug_watch_timeout_ms,
            max_rows_per_step: settings.debug_max_rows_per_step,
            inline_values: settings.debug_inline_values,
            query_timeout_ms: settings.query_timeout_ms,
        };
        let backend = self.inner.backend();
        let control = backend
            .start(StartParams {
                profile,
                lease,
                gate,
                sink: Arc::clone(&sink) as Arc<dyn DebugSink>,
                proxy: Arc::clone(&proxy) as Arc<dyn ProxyOpener>,
                settings: engine_settings,
                request: request.clone(),
            })
            .await;
        let control = match control {
            Ok(c) => c,
            Err(e) => {
                proxy.close_all().await;
                return Err(e);
            }
        };
        let entry = Arc::new(Entry {
            debug_id: control.debug_id(),
            lease_id,
            session_id,
            control,
            sink: Arc::clone(&sink),
            proxy,
            leases: Arc::clone(&state.leases),
            request: Mutex::new(request),
            cleaned: AtomicBool::new(false),
        });
        // The engine reports its end through the sink: free the lease then.
        let weak = Arc::downgrade(&self.inner);
        sink.set_on_end(Arc::new(move |s: &DebugState| {
            let Some(inner) = weak.upgrade() else { return };
            let entry = inner.reg().live.get(&s.debug_id).cloned();
            if let Some(entry) = entry {
                let report = match &s.status {
                    DebugStatus::Terminated { report, .. } => *report,
                    _ => None,
                };
                inner.cleanup(&entry, report);
            }
        }));
        self.inner
            .reg()
            .live
            .insert(entry.debug_id, Arc::clone(&entry));
        self.spawn_watchdog(state, &entry);
        let first = entry.control.snapshot().await;
        let first = match first {
            Ok(s) => s,
            Err(e) => {
                self.inner.cleanup(&entry, None);
                return Err(e);
            }
        };
        // The engine may have ended before it was registered.
        if let DebugStatus::Terminated { report, .. } = &first.status {
            self.inner.cleanup(&entry, *report);
        }
        Ok((entry, first))
    }

    /// Ends the session when its lease breaks or disappears, or its session closes: those
    /// events have no hook in the lease/session managers, so the session looks for them.
    fn spawn_watchdog(&self, state: &AppState, entry: &Arc<Entry>) {
        let inner = Arc::clone(&self.inner);
        let entry = Arc::clone(entry);
        let sessions = Arc::clone(&state.sessions);
        let leases = Arc::clone(&state.leases);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(inner.poll()).await;
                if entry.cleaned.load(Ordering::SeqCst) {
                    return;
                }
                // An actor that vanished without a final state (a panic) leaves the lease claimed.
                if !entry.control.alive() {
                    inner.terminate(&entry, TerminateReason::Error).await;
                    return;
                }
                if let Some(reason) =
                    lost_reason(&sessions, &leases, entry.session_id, entry.lease_id)
                {
                    inner.terminate(&entry, reason).await;
                    return;
                }
            }
        });
    }

    /// End the session that holds `lease_id`, if any (called before a lease is closed). The
    /// transaction the debugger owns is rolled back.
    pub async fn stop_for_lease(&self, lease_id: Uuid) {
        if let Some(entry) = self.inner.find_by_lease(lease_id) {
            self.inner
                .terminate(&entry, TerminateReason::LeaseLost)
                .await;
        }
    }

    /// End every session of `session_id` (called before the session closes, from the
    /// `session_close` command); the watchdog is the fallback for other ways it can vanish.
    pub async fn stop_for_session(&self, session_id: Uuid) {
        let doomed: Vec<Arc<Entry>> = self
            .inner
            .reg()
            .live
            .values()
            .filter(|e| e.session_id == session_id)
            .cloned()
            .collect();
        for entry in doomed {
            self.inner
                .terminate(&entry, TerminateReason::SessionClosed)
                .await;
        }
    }

    /// Latest state; a finished session answers with its last state.
    pub async fn snapshot(&self, debug_id: Uuid) -> Result<DebugState> {
        match self.entry(debug_id) {
            Ok(e) => e.control.snapshot().await,
            Err(not_found) => self.ended(debug_id).and_then(|e| e.state).ok_or(not_found),
        }
    }

    /// Resume / step / run to cursor / jump / pause.
    pub async fn act(
        &self,
        debug_id: Uuid,
        action: DebugAction,
        confirm: Option<ConfirmationGrant>,
    ) -> Result<DebugState> {
        self.entry(debug_id)?.control.act(action, confirm).await
    }

    /// End a session. `None` = the end action chosen at start. Idempotent: stopping a session
    /// that already ended answers with its report.
    pub async fn stop(
        &self,
        debug_id: Uuid,
        end: Option<DebugEndAction>,
    ) -> Result<DebugEndReport> {
        let entry = match self.entry(debug_id) {
            Ok(e) => e,
            Err(not_found) => {
                return self
                    .ended(debug_id)
                    .map(|e| {
                        e.report.unwrap_or(DebugEndReport {
                            steps_run: 0,
                            committed: false,
                            rolled_back: false,
                        })
                    })
                    .ok_or(not_found);
            }
        };
        if end == Some(DebugEndAction::Ask) {
            let awaiting = entry.control.snapshot().await.is_ok_and(|s| {
                matches!(
                    s.status,
                    DebugStatus::Paused {
                        reason: PauseReason::AwaitingEnd
                    }
                )
            });
            if awaiting {
                return Err(Error::Invalid(
                    "the session waits for Commit or Rollback; `ask` is not an answer".into(),
                ));
            }
        }
        let report = match entry.control.stop(end).await {
            Ok(r) => r,
            Err(e) => {
                // The engine is gone (panic, dropped): nothing is left to wait for.
                self.inner.cleanup(&entry, None);
                return Err(e);
            }
        };
        // `Ask` with an open transaction only moves the session to "awaiting the end decision":
        // it lives on, still owning the lease and the transaction.
        if self.session_ended(&entry).await {
            self.inner.cleanup(&entry, Some(report));
        }
        Ok(report)
    }

    /// True when the engine reports the session as terminated (or cannot answer any more).
    async fn session_ended(&self, entry: &Arc<Entry>) -> bool {
        match entry.control.snapshot().await {
            Ok(s) => is_terminated(&s),
            Err(_) => true,
        }
    }

    /// Stop and start again on the same lease with the breakpoints, watches and options the
    /// session has now; the new session has a new id.
    pub async fn restart(
        &self,
        state: &AppState,
        debug_id: Uuid,
        text: String,
        end: Option<DebugEndAction>,
    ) -> Result<DebugState> {
        let entry = self.entry(debug_id)?;
        let mut request = entry
            .request
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        request.text = text;
        request.range = None;
        // A session waiting for Commit / Rollback needs that answer first: restarting inside
        // the old transaction would mix the two runs.
        let awaiting = entry.control.snapshot().await.is_ok_and(|s| {
            matches!(
                s.status,
                DebugStatus::Paused {
                    reason: PauseReason::AwaitingEnd
                }
            )
        });
        if awaiting && matches!(end, None | Some(DebugEndAction::Ask)) {
            return Err(Error::Invalid(
                "the session waits for Commit or Rollback: restart with an explicit end action"
                    .into(),
            ));
        }
        self.stop(debug_id, end).await?;
        if !self.session_ended(&entry).await {
            return Err(Error::Busy(
                "the session has not ended: answer its end decision first".into(),
            ));
        }
        self.start(state, request).await
    }

    /// Evaluate an expression in the read-only frame.
    pub async fn evaluate(
        &self,
        debug_id: Uuid,
        expr: &str,
        frame: Option<u32>,
        echo: bool,
    ) -> Result<EvalResult> {
        self.entry(debug_id)?
            .control
            .evaluate(expr, frame, echo)
            .await
    }

    /// Replace the breakpoints of one source.
    pub async fn set_breakpoints(
        &self,
        debug_id: Uuid,
        source: SourceRef,
        breakpoints: Vec<Breakpoint>,
    ) -> Result<Vec<BreakpointStatus>> {
        let entry = self.entry(debug_id)?;
        let status = entry
            .control
            .set_breakpoints(source.clone(), breakpoints.clone())
            .await?;
        let mut req = entry.request.lock().unwrap_or_else(|e| e.into_inner());
        match source {
            SourceRef::Script => req.breakpoints = breakpoints,
            SourceRef::Routine { routine } => {
                let hash = req
                    .routine_breakpoints
                    .iter()
                    .find(|b| b.routine.signature == routine.signature)
                    .and_then(|b| b.source_hash.clone());
                req.routine_breakpoints
                    .retain(|b| b.routine.signature != routine.signature);
                req.routine_breakpoints
                    .extend(breakpoints.into_iter().map(|breakpoint| RoutineBreakpoint {
                        routine: routine.clone(),
                        source_hash: hash.clone(),
                        breakpoint,
                    }));
            }
        }
        Ok(status)
    }

    /// Mute / unmute all breakpoints.
    pub async fn set_muted(&self, debug_id: Uuid, muted: bool) -> Result<DebugState> {
        let entry = self.entry(debug_id)?;
        let state = entry.control.set_muted(muted).await?;
        entry
            .request
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .muted = muted;
        Ok(state)
    }

    /// Change a variable.
    pub async fn set_variable(
        &self,
        debug_id: Uuid,
        reference: &str,
        value_text: &str,
    ) -> Result<DebugVariable> {
        self.entry(debug_id)?
            .control
            .set_variable(reference, value_text)
            .await
    }

    /// Children of a variable.
    pub async fn variable_children(
        &self,
        debug_id: Uuid,
        reference: &str,
    ) -> Result<Vec<DebugVariable>> {
        self.entry(debug_id)?
            .control
            .variable_children(reference)
            .await
    }

    /// Replace the watches.
    pub async fn set_watches(
        &self,
        debug_id: Uuid,
        watches: Vec<WatchSpec>,
    ) -> Result<Vec<WatchResult>> {
        let entry = self.entry(debug_id)?;
        let results = entry.control.set_watches(watches.clone()).await?;
        entry
            .request
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .watches = watches;
        Ok(results)
    }

    /// Select a call-stack frame.
    pub async fn select_frame(&self, debug_id: Uuid, frame: u32) -> Result<DebugState> {
        self.entry(debug_id)?.control.select_frame(frame).await
    }

    /// Source of a routine: through the session's engine when `debug_id` is live, else read
    /// directly (so breakpoints can be set before a session exists).
    pub async fn routine_source(
        &self,
        state: &AppState,
        debug_id: Option<Uuid>,
        session_id: Uuid,
        routine: &RoutineRef,
    ) -> Result<RoutineSource> {
        if let Some(id) = debug_id {
            if let Ok(entry) = self.entry(id) {
                match entry.control.routine_source(routine).await {
                    Err(Error::Unsupported(_)) => {}
                    other => return other,
                }
            }
        }
        fetch_routine_source(state, session_id, routine).await
    }

    /// What can be debugged on the lease (cheap probe, cached per lease and database).
    pub async fn support(&self, state: &AppState, lease_id: Uuid) -> Result<DebugSupport> {
        let (session_id, _lease) = state.leases.get_with_session(lease_id)?;
        let (session, config) = state.sessions.get_with_config(session_id)?;
        let database = state.leases.state_of(lease_id)?.database;
        let key = (lease_id, database.clone());
        if let Some(hit) = self.inner.reg().support.get(&key) {
            return Ok(hit.clone());
        }
        let caps = session_capabilities(state, &session, config.kind);
        let routine = routine_support(&session, config.kind, &caps, database.as_deref()).await;
        let found = DebugSupport {
            script: true,
            routine,
            transactions: caps.manual_transactions,
            script_variables: matches!(
                config.kind,
                DbKind::MySql | DbKind::TiDb | DbKind::SqlServer
            ),
        };
        self.inner.reg().support.insert(key, found.clone());
        Ok(found)
    }
}

/// Add the stored routine breakpoints to the request. What the frontend sent wins per routine
/// (like DAP `setBreakpoints`, a source's list replaces the earlier one).
pub(crate) fn merge_stored_routine_breakpoints(
    request: &mut DebugStartRequest,
    stored: Vec<RoutineBreakpoint>,
) {
    let sent: Vec<String> = request
        .routine_breakpoints
        .iter()
        .map(|b| b.routine.signature.clone())
        .collect();
    request.routine_breakpoints.extend(
        stored
            .into_iter()
            .filter(|b| !sent.contains(&b.routine.signature)),
    );
}

/// Capabilities of a session: its own when it reports them, else its driver's descriptor.
fn session_capabilities(
    state: &AppState,
    session: &Arc<dyn dbstudio_core::Session>,
    kind: DbKind,
) -> Capabilities {
    session.capabilities().unwrap_or_else(|| {
        state
            .registry
            .get(kind)
            .map(|d| d.descriptor().capabilities)
            .unwrap_or_else(|| Capabilities::planned_for(kind))
    })
}

// ---------------------------------------------------------------------------------------------
// Routine support and routine source (PostgreSQL family), design 4.2.1
// ---------------------------------------------------------------------------------------------

fn text_cell(v: &Value) -> Option<String> {
    match v {
        Value::Text(t) => Some(t.clone()),
        _ => None,
    }
}

/// Can this server debug routines (a probe on the session, not on the lease, so a lease in
/// the middle of a transaction is left alone). A probe that fails is reported as unsupported
/// with the reason; it never fails `debug_support`.
async fn routine_support(
    session: &Arc<dyn dbstudio_core::Session>,
    kind: DbKind,
    caps: &Capabilities,
    database: Option<&str>,
) -> RoutineDebugSupport {
    match kind {
        DbKind::CockroachDb => {
            return RoutineDebugSupport::Unsupported {
                reason: "CockroachDB has no debugger API".into(),
            };
        }
        DbKind::Postgres | DbKind::YugabyteDb if caps.routine_debug => {}
        _ => {
            return RoutineDebugSupport::Unsupported {
                reason: "this server has no routine debugger".into(),
            };
        }
    }
    // The engine's own detection on a short-lived lease of the same session: one function
    // decides for the UI and for the session, so they cannot disagree.
    let probe = async {
        let lease = session
            .open_lease(LeaseOptions {
                database: database.map(str::to_string),
                schema: None,
                auto_commit: true,
            })
            .await?;
        let found = dbstudio_debug::routine::pg::PgRoutineDebugger::probe_support(&*lease).await;
        let _ = lease.close().await;
        found
    }
    .await;
    match probe {
        Ok(support) => support,
        Err(e) => RoutineDebugSupport::Unsupported {
            reason: format!("cannot probe the server: {e}"),
        },
    }
}

/// Read the source of a PostgreSQL routine without a debug session.
async fn fetch_routine_source(
    state: &AppState,
    session_id: Uuid,
    routine: &RoutineRef,
) -> Result<RoutineSource> {
    let (session, config) = state.sessions.get_with_config(session_id)?;
    let caps = session_capabilities(state, &session, config.kind);
    if !matches!(config.kind, DbKind::Postgres | DbKind::YugabyteDb) || !caps.routine_debug {
        return Err(Error::Unsupported(
            "routine source is available on PostgreSQL only".into(),
        ));
    }
    let sig = &routine.signature;
    if sig.trim().is_empty() || sig.contains('\0') || sig.contains('\\') {
        return Err(Error::Invalid("invalid routine signature".into()));
    }
    let database = routine.path.as_ref().and_then(|p| {
        p.segments
            .iter()
            .find(|s| s.kind == ObjectKind::Database)
            .map(|s| s.name.clone())
    });
    // The signature is data, never SQL: it is a quoted literal handed to `to_regprocedure`.
    let sql = format!(
        "SELECT p.prosrc, l.lanname FROM pg_proc p JOIN pg_language l ON l.oid = p.prolang \
         WHERE p.oid = to_regprocedure('{}')::oid",
        sig.replace('\'', "''")
    );
    let mut req = QueryRequest::new(sql);
    req.max_rows = Some(1);
    req.database = database;
    let results = execute_collect(&*session, req).await?;
    let row = results
        .into_iter()
        .find_map(|r| match r.outcome {
            StatementOutcome::Rows(rs) => rs.rows.into_iter().next(),
            _ => None,
        })
        .ok_or_else(|| Error::NotFound(format!("routine {sig}")))?;
    let text = row.first().and_then(text_cell).unwrap_or_default();
    let language = row.get(1).and_then(text_cell).unwrap_or_default();
    Ok(RoutineSource {
        source_hash: source_hash(&text),
        text,
        language,
        first_line: 1,
    })
}

/// sha256 hex of a routine source (the identity breakpoints are checked against).
pub(crate) fn source_hash(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    //! Glue tests with a fake engine: the real engine (package D) is not needed to prove that
    //! the guard runs per step, the lease is held, the hooks end sessions and events are ordered.

    use std::sync::atomic::AtomicU64;

    use dbstudio_core::debug::{DebugOutput, DebugSessionCaps, DebugTxInfo, MessageSource};
    use dbstudio_core::{ConnectionConfig, DriverRegistry, Environment, LeaseOptions, TxStatus};
    use dbstudio_store::{ConnectionProfile, SecretChange};
    use serde_json::{Value as Json, json};

    use super::*;
    use crate::commands::{connections, debug as cmd, editor, sessions};
    use crate::events::AppEvent;
    use crate::guard::test_support::{TestClock, use_keyword_classifier};
    use crate::session::testing::{MockDriver, Probe};
    use crate::state::testing::test_state;

    // ----- fake engine -----

    struct FakeSession {
        debug_id: Uuid,
        lease_id: Uuid,
        request: DebugStartRequest,
        gate: Arc<dyn StepGate>,
        sink: Arc<dyn DebugSink>,
        seq: AtomicU64,
        stopped: AtomicBool,
        ask_end: AtomicBool,
        awaiting: AtomicBool,
        dead: AtomicBool,
    }

    impl FakeSession {
        fn next_seq(&self) -> u64 {
            self.seq.fetch_add(1, Ordering::SeqCst) + 1
        }

        fn state(&self, status: DebugStatus) -> DebugState {
            DebugState {
                debug_id: self.debug_id,
                lease_id: self.lease_id,
                seq: self.next_seq(),
                status,
                position: None,
                frames: vec![],
                selected_frame: 0,
                variables: vec![],
                watches: vec![],
                inline_values: vec![],
                breakpoints: vec![],
                transaction: DebugTxInfo {
                    status: TxStatus::Idle,
                    owned: false,
                    open_for_ms: None,
                    warn: false,
                    ttl_left_secs: None,
                },
                caps: DebugSessionCaps {
                    step_into: false,
                    step_out: false,
                    pause_running: true,
                    set_variable: false,
                    routine: false,
                    jump_to: false,
                },
                step_into_targets: vec![],
                muted: self.request.muted,
            }
        }

        fn publish(&self, status: DebugStatus) -> DebugState {
            let s = self.state(status);
            self.sink.state(&s);
            s
        }
    }

    #[async_trait]
    impl SessionControl for FakeSession {
        fn debug_id(&self) -> Uuid {
            self.debug_id
        }
        async fn snapshot(&self) -> Result<DebugState> {
            if self.stopped.load(Ordering::SeqCst) {
                return Ok(self.state(DebugStatus::Terminated {
                    reason: TerminateReason::Finished,
                    report: Some(DebugEndReport {
                        steps_run: 0,
                        committed: false,
                        rolled_back: false,
                    }),
                }));
            }
            if self.awaiting.load(Ordering::SeqCst) {
                return Ok(self.state(DebugStatus::Paused {
                    reason: PauseReason::AwaitingEnd,
                }));
            }
            Ok(self.state(DebugStatus::Paused {
                reason: PauseReason::Entry,
            }))
        }
        async fn act(
            &self,
            _action: DebugAction,
            confirm: Option<ConfirmationGrant>,
        ) -> Result<DebugState> {
            let gated = self.gate.check(GateRequest {
                dialect: ScriptDialect::Postgres,
                text: &self.request.text,
                params: &self.request.params,
                confirm: confirm.as_ref(),
                database: Some("appdb"),
                schema: None,
            });
            let status = match gated {
                Ok(g) => {
                    self.gate.record(&StepRecord {
                        dialect: ScriptDialect::Postgres,
                        text: g.text,
                        database: Some("appdb".into()),
                        elapsed_ms: 3,
                        rows: Some(1),
                        error: None,
                    });
                    self.sink.output(DebugOutputEvent {
                        debug_id: self.debug_id,
                        seq: self.next_seq(),
                        output: DebugOutput::Message {
                            text: "ran".into(),
                            source: MessageSource::Info,
                        },
                    });
                    PauseReason::Step
                }
                Err(Error::ConfirmationRequired(request)) => {
                    PauseReason::Confirmation { request: *request }
                }
                Err(e) => PauseReason::Blocked {
                    error: serde_json::to_value(&e).unwrap(),
                },
            };
            Ok(self.publish(DebugStatus::Paused { reason: status }))
        }
        async fn stop(&self, end: Option<DebugEndAction>) -> Result<DebugEndReport> {
            let report = DebugEndReport {
                steps_run: 1,
                committed: false,
                rolled_back: true,
            };
            if self.ask_end.load(Ordering::SeqCst)
                && matches!(end, None | Some(DebugEndAction::Ask))
                && !self.awaiting.swap(true, Ordering::SeqCst)
            {
                self.publish(DebugStatus::Paused {
                    reason: PauseReason::AwaitingEnd,
                });
                return Ok(DebugEndReport {
                    rolled_back: false,
                    ..report
                });
            }
            if !self.stopped.swap(true, Ordering::SeqCst) {
                self.publish(DebugStatus::Terminated {
                    reason: TerminateReason::Stopped,
                    report: Some(report),
                });
            }
            Ok(report)
        }
        async fn evaluate(&self, expr: &str, _f: Option<u32>, _e: bool) -> Result<EvalResult> {
            Err(Error::Unsupported(format!("fake evaluate {expr}")))
        }
        async fn set_breakpoints(
            &self,
            _source: SourceRef,
            breakpoints: Vec<Breakpoint>,
        ) -> Result<Vec<BreakpointStatus>> {
            Ok(breakpoints
                .iter()
                .map(|b| BreakpointStatus {
                    id: b.id.clone(),
                    verified: true,
                    line: b.line,
                    message: None,
                })
                .collect())
        }
        async fn set_muted(&self, _muted: bool) -> Result<DebugState> {
            self.snapshot().await
        }
        async fn set_variable(&self, _r: &str, _v: &str) -> Result<DebugVariable> {
            Err(Error::Unsupported("fake".into()))
        }
        async fn variable_children(&self, _r: &str) -> Result<Vec<DebugVariable>> {
            Ok(vec![])
        }
        async fn set_watches(&self, _w: Vec<WatchSpec>) -> Result<Vec<WatchResult>> {
            Ok(vec![])
        }
        async fn select_frame(&self, _f: u32) -> Result<DebugState> {
            self.snapshot().await
        }
        async fn routine_source(&self, _r: &RoutineRef) -> Result<RoutineSource> {
            Err(Error::Unsupported("fake".into()))
        }
    }

    #[derive(Default)]
    struct FakeBackend {
        requests: Mutex<Vec<DebugStartRequest>>,
        fail: AtomicBool,
        /// The engine ends inside `start` (a script with nothing to run).
        end_at_start: AtomicBool,
        /// `stop(None | Ask)` moves the session to AwaitingEnd (open transaction, `Ask`).
        ask_end: AtomicBool,
        sessions: Mutex<Vec<Arc<FakeSession>>>,
    }

    struct Shared(Arc<FakeSession>);

    #[async_trait]
    impl SessionControl for Shared {
        fn debug_id(&self) -> Uuid {
            self.0.debug_id()
        }
        fn alive(&self) -> bool {
            !self.0.dead.load(Ordering::SeqCst)
        }
        async fn snapshot(&self) -> Result<DebugState> {
            self.0.snapshot().await
        }
        async fn act(&self, a: DebugAction, c: Option<ConfirmationGrant>) -> Result<DebugState> {
            self.0.act(a, c).await
        }
        async fn stop(&self, e: Option<DebugEndAction>) -> Result<DebugEndReport> {
            self.0.stop(e).await
        }
        async fn evaluate(&self, x: &str, f: Option<u32>, e: bool) -> Result<EvalResult> {
            self.0.evaluate(x, f, e).await
        }
        async fn set_breakpoints(
            &self,
            s: SourceRef,
            b: Vec<Breakpoint>,
        ) -> Result<Vec<BreakpointStatus>> {
            self.0.set_breakpoints(s, b).await
        }
        async fn set_muted(&self, m: bool) -> Result<DebugState> {
            self.0.set_muted(m).await
        }
        async fn set_variable(&self, r: &str, v: &str) -> Result<DebugVariable> {
            self.0.set_variable(r, v).await
        }
        async fn variable_children(&self, r: &str) -> Result<Vec<DebugVariable>> {
            self.0.variable_children(r).await
        }
        async fn set_watches(&self, w: Vec<WatchSpec>) -> Result<Vec<WatchResult>> {
            self.0.set_watches(w).await
        }
        async fn select_frame(&self, f: u32) -> Result<DebugState> {
            self.0.select_frame(f).await
        }
        async fn routine_source(&self, r: &RoutineRef) -> Result<RoutineSource> {
            self.0.routine_source(r).await
        }
    }

    #[async_trait]
    impl DebugBackend for FakeBackend {
        async fn start(&self, p: StartParams) -> Result<Arc<dyn SessionControl>> {
            if self.fail.load(Ordering::SeqCst) {
                return Err(Error::Invalid("engine refused".into()));
            }
            self.requests.lock().unwrap().push(p.request.clone());
            let fake = Arc::new(FakeSession {
                debug_id: Uuid::new_v4(),
                lease_id: p.request.lease_id,
                request: p.request,
                gate: p.gate,
                sink: p.sink,
                seq: AtomicU64::new(0),
                stopped: AtomicBool::new(false),
                ask_end: AtomicBool::new(self.ask_end.load(Ordering::SeqCst)),
                awaiting: AtomicBool::new(false),
                dead: AtomicBool::new(false),
            });
            if self.end_at_start.load(Ordering::SeqCst) {
                fake.stopped.store(true, Ordering::SeqCst);
                fake.publish(DebugStatus::Terminated {
                    reason: TerminateReason::Finished,
                    report: Some(DebugEndReport {
                        steps_run: 0,
                        committed: false,
                        rolled_back: false,
                    }),
                });
            } else {
                fake.publish(DebugStatus::Paused {
                    reason: PauseReason::Entry,
                });
            }
            self.sessions.lock().unwrap().push(Arc::clone(&fake));
            Ok(Arc::new(Shared(fake)))
        }
    }

    // ----- rig -----

    struct Rig {
        state: AppState,
        probe: Arc<Probe>,
        backend: Arc<FakeBackend>,
        sid: Uuid,
        lease: Uuid,
        events: tokio::sync::broadcast::Receiver<AppEvent>,
    }

    fn profile(read_only: bool, environment: Option<Environment>) -> ConnectionProfile {
        let mut config = ConnectionConfig::new(DbKind::Postgres, "mock", "localhost");
        config.read_only = read_only;
        config.environment = environment;
        config.database = Some("appdb".into());
        ConnectionProfile {
            config,
            group_id: None,
            color: None,
            save_password: false,
            has_password: false,
            sort_order: 0,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    async fn rig_with(
        read_only: bool,
        environment: Option<Environment>,
        tweak: impl FnOnce(&mut AppState),
    ) -> Rig {
        let (driver, probe) = MockDriver::new(DbKind::Postgres);
        probe.rich.store(true, Ordering::SeqCst);
        let mut registry = DriverRegistry::new();
        registry.register(driver);
        let (mut state, _) = test_state(registry);
        let backend = Arc::new(FakeBackend::default());
        state.debug = Arc::new(DebugManager::with_backend(backend.clone()));
        tweak(&mut state);
        let saved =
            connections::save_impl(&state, profile(read_only, environment), SecretChange::Keep)
                .await
                .unwrap();
        let sid = sessions::open_impl(&state, saved.config.id, None)
            .await
            .unwrap();
        let sid = sid.session_id;
        let lease = editor::lease_open_impl(&state, sid, LeaseOptions::default())
            .await
            .unwrap()
            .lease_id;
        let events = state.events.subscribe();
        Rig {
            state,
            probe,
            backend,
            sid,
            lease,
            events,
        }
    }

    async fn rig() -> Rig {
        rig_with(false, None, |_| {}).await
    }

    fn start_request(lease: Uuid, text: &str) -> DebugStartRequest {
        serde_json::from_value(json!({"leaseId": lease, "text": text})).unwrap()
    }

    fn drain(rx: &mut tokio::sync::broadcast::Receiver<AppEvent>) -> Vec<AppEvent> {
        let mut out = vec![];
        while let Ok(e) = rx.try_recv() {
            out.push(e);
        }
        out
    }

    fn history(state: &AppState) -> Vec<dbstudio_store::HistoryEntry> {
        state
            .connections
            .store()
            .history_list(&dbstudio_store::HistoryQuery {
                connection_id: None,
                search: None,
                status: None,
                before_id: None,
                limit: 500,
            })
            .unwrap()
    }

    async fn wait_for<T>(mut f: impl FnMut() -> Option<T>) -> T {
        for _ in 0..200 {
            if let Some(v) = f() {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("condition not reached in time");
    }

    fn pause_of(state: &DebugState) -> &PauseReason {
        match &state.status {
            DebugStatus::Paused { reason } => reason,
            other => panic!("not paused: {other:?}"),
        }
    }

    // ----- tests -----

    #[tokio::test]
    async fn start_claims_the_lease_and_editor_commands_answer_busy() {
        let _c = use_keyword_classifier();
        let mut m = rig().await;
        let first = cmd::debug_start_impl(&m.state, start_request(m.lease, "select 1"))
            .await
            .unwrap();
        assert!(matches!(pause_of(&first), PauseReason::Entry));
        assert_eq!(m.state.debug.count(), 1);
        assert_eq!(m.state.leases.debug_owner(m.lease), Some(first.debug_id));
        let events = drain(&mut m.events);
        assert!(
            events
                .iter()
                .any(|e| e.name == DEBUG_STATE && e.payload["debugId"] == json!(first.debug_id))
        );

        // The editor cannot run anything on the lease meanwhile.
        let req: dbstudio_core::ExecuteRequest =
            serde_json::from_value(json!({"text": "select 1", "queryId": Uuid::new_v4()})).unwrap();
        let err = editor::lease_execute_impl(&m.state, m.lease, req, |_| true)
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::Busy(ref s) if s == "debugging"),
            "{err:?}"
        );
        // A second session on the same lease is refused too.
        let err = cmd::debug_start_impl(&m.state, start_request(m.lease, "select 2"))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Busy(_)));

        let report = cmd::debug_stop_impl(&m.state, first.debug_id, None)
            .await
            .unwrap();
        assert!(report.rolled_back);
        assert_eq!(m.state.debug.count(), 0);
        assert_eq!(m.state.leases.debug_owner(m.lease), None);
    }

    #[tokio::test]
    async fn a_session_that_ends_inside_start_does_not_keep_the_lease() {
        let _c = use_keyword_classifier();
        let m = rig().await;
        m.backend.end_at_start.store(true, Ordering::SeqCst);
        let first = cmd::debug_start_impl(&m.state, start_request(m.lease, "-- nothing"))
            .await
            .unwrap();
        assert!(matches!(first.status, DebugStatus::Terminated { .. }));
        assert_eq!(m.state.debug.count(), 0);
        assert_eq!(m.state.leases.debug_owner(m.lease), None);
        m.state.leases.ensure_not_debugging(m.lease).unwrap();
    }

    #[tokio::test]
    async fn stop_with_ask_keeps_the_session_and_the_lease_until_the_decision() {
        let _c = use_keyword_classifier();
        let m = rig().await;
        m.backend.ask_end.store(true, Ordering::SeqCst);
        let first = cmd::debug_start_impl(&m.state, start_request(m.lease, "select 1"))
            .await
            .unwrap();
        cmd::debug_stop_impl(&m.state, first.debug_id, None)
            .await
            .unwrap();
        // Still alive, still owning the lease (its transaction is open).
        assert_eq!(m.state.debug.count(), 1);
        assert_eq!(m.state.leases.debug_owner(m.lease), Some(first.debug_id));
        let s = cmd::debug_snapshot_impl(&m.state, first.debug_id)
            .await
            .unwrap();
        assert!(matches!(pause_of(&s), PauseReason::AwaitingEnd));
        // A restart without an explicit end is refused and changes nothing.
        let err = cmd::debug_restart_impl(&m.state, first.debug_id, "select 2".into(), None)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Invalid(_)), "{err:?}");
        assert_eq!(m.state.debug.count(), 1);
        // The decision ends it.
        cmd::debug_stop_impl(&m.state, first.debug_id, Some(DebugEndAction::Rollback))
            .await
            .unwrap();
        assert_eq!(m.state.debug.count(), 0);
        assert_eq!(m.state.leases.debug_owner(m.lease), None);
    }

    #[tokio::test]
    async fn a_failed_start_frees_the_lease() {
        let m = rig().await;
        m.backend.fail.store(true, Ordering::SeqCst);
        let err = cmd::debug_start_impl(&m.state, start_request(m.lease, "select 1"))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Invalid(_)));
        assert_eq!(m.state.leases.debug_owner(m.lease), None);
        assert_eq!(m.state.debug.count(), 0);
        // Unknown lease.
        let err = cmd::debug_start_impl(&m.state, start_request(Uuid::new_v4(), "select 1"))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::NotFound(_)));
    }

    #[tokio::test]
    async fn a_step_goes_through_the_guard_and_is_recorded_in_the_history() {
        let _c = use_keyword_classifier();
        let m = rig().await;
        let s = cmd::debug_start_impl(
            &m.state,
            start_request(m.lease, "UPDATE t SET a = 1 WHERE b = 2"),
        )
        .await
        .unwrap();
        let after = cmd::debug_step_impl(&m.state, s.debug_id, DebugAction::StepOver, None)
            .await
            .unwrap();
        assert!(matches!(pause_of(&after), PauseReason::Step));
        let rows = wait_for(|| {
            let h = history(&m.state);
            (!h.is_empty()).then_some(h)
        })
        .await;
        assert_eq!(rows[0].text, "UPDATE t SET a = 1 WHERE b = 2");
        assert_eq!((rows[0].status.as_str(), rows[0].rows), ("ok", Some(1)));

        // Opting out of the history records nothing more.
        cmd::debug_stop_impl(&m.state, s.debug_id, None)
            .await
            .unwrap();
        let mut quiet = start_request(m.lease, "SELECT 1");
        quiet.options.record_history = false;
        let s = cmd::debug_start_impl(&m.state, quiet).await.unwrap();
        cmd::debug_step_impl(&m.state, s.debug_id, DebugAction::StepOver, None)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(history(&m.state).len(), 1);
    }

    #[tokio::test]
    async fn read_only_blocks_a_write_step_and_records_nothing() {
        let _c = use_keyword_classifier();
        let m = rig_with(true, None, |_| {}).await;
        let s = cmd::debug_start_impl(
            &m.state,
            start_request(m.lease, "DELETE FROM t WHERE a = 1"),
        )
        .await
        .unwrap();
        let after = cmd::debug_step_impl(&m.state, s.debug_id, DebugAction::StepOver, None)
            .await
            .unwrap();
        match pause_of(&after) {
            PauseReason::Blocked { error } => assert_eq!(error["kind"], "readOnly"),
            other => panic!("{other:?}"),
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(history(&m.state).is_empty());
        // A read passes on the same read-only connection.
        cmd::debug_stop_impl(&m.state, s.debug_id, None)
            .await
            .unwrap();
        let s = cmd::debug_start_impl(&m.state, start_request(m.lease, "SELECT 1"))
            .await
            .unwrap();
        let after = cmd::debug_step_impl(&m.state, s.debug_id, DebugAction::StepOver, None)
            .await
            .unwrap();
        assert!(matches!(pause_of(&after), PauseReason::Step));
    }

    fn confirmation_of(state: &DebugState) -> dbstudio_core::ConfirmationRequest {
        match pause_of(state) {
            PauseReason::Confirmation { request } => request.clone(),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn production_needs_a_one_time_token_bound_to_the_step() {
        let _c = use_keyword_classifier();
        let clock = TestClock::new();
        let c2 = clock.clone();
        let m = rig_with(false, Some(Environment::Production), move |st| {
            st.confirm = Arc::new(ConfirmTokens::with_clock(Duration::from_secs(120), c2));
        })
        .await;
        let s = cmd::debug_start_impl(&m.state, start_request(m.lease, "DROP TABLE t"))
            .await
            .unwrap();
        let id = s.debug_id;
        let grant = |token: &str, typed: &str| {
            Some(ConfirmationGrant {
                token: token.into(),
                typed: typed.into(),
            })
        };
        let step = |c| cmd::debug_step_impl(&m.state, id, DebugAction::StepOver, c);

        let first = confirmation_of(&step(None).await.unwrap());
        assert_eq!(first.confirm_text, "appdb");
        // Wrong text, then a token nobody issued: both ask again.
        confirmation_of(&step(grant(&first.token, "other")).await.unwrap());
        confirmation_of(
            &step(grant("0123456789abcdef0123456789abcdef", "appdb"))
                .await
                .unwrap(),
        );
        // The right token with the exact text runs ...
        let good = confirmation_of(&step(None).await.unwrap());
        let ran = step(grant(&good.token, "appdb")).await.unwrap();
        assert!(matches!(pause_of(&ran), PauseReason::Step));
        // ... once: spent tokens do not run again.
        confirmation_of(&step(grant(&good.token, "appdb")).await.unwrap());
        // Expired tokens are refused.
        let old = confirmation_of(&step(None).await.unwrap());
        clock.advance(Duration::from_secs(121));
        confirmation_of(&step(grant(&old.token, "appdb")).await.unwrap());
    }

    #[tokio::test]
    async fn closing_the_lease_stops_the_session_with_lease_lost() {
        let m = rig().await;
        let mut events = m.state.events.subscribe();
        let s = cmd::debug_start_impl(&m.state, start_request(m.lease, "select 1"))
            .await
            .unwrap();
        editor::lease_close_impl(&m.state, m.lease).await.unwrap();
        assert_eq!(m.state.debug.count(), 0);
        assert_eq!(m.state.leases.debug_owner(m.lease), None);
        let last = drain(&mut events)
            .into_iter()
            .rfind(|e| e.name == DEBUG_STATE)
            .expect("a final state event");
        assert_eq!(last.payload["status"]["kind"], "terminated");
        assert_eq!(last.payload["status"]["reason"], "leaseLost");
        assert_eq!(last.payload["status"]["report"]["rolledBack"], true);
        // The session is gone for commands but stays idempotent for Stop.
        assert!(matches!(
            cmd::debug_step_impl(&m.state, s.debug_id, DebugAction::Resume, None).await,
            Err(Error::NotFound(_))
        ));
        cmd::debug_stop_impl(&m.state, s.debug_id, None)
            .await
            .unwrap();
        let snap = cmd::debug_snapshot_impl(&m.state, s.debug_id)
            .await
            .unwrap();
        assert!(matches!(snap.status, DebugStatus::Terminated { .. }));
    }

    fn last_terminated(events: Vec<AppEvent>) -> Option<Json> {
        events
            .into_iter()
            .rfind(|e| e.name == DEBUG_STATE && e.payload["status"]["kind"] == "terminated")
            .map(|e| e.payload["status"].clone())
    }

    #[tokio::test]
    async fn a_dead_engine_frees_the_lease() {
        let m = rig().await;
        m.state.debug.set_poll(Duration::from_millis(10));
        let mut events = m.state.events.subscribe();
        cmd::debug_start_impl(&m.state, start_request(m.lease, "select 1"))
            .await
            .unwrap();
        m.backend.sessions.lock().unwrap()[0]
            .dead
            .store(true, Ordering::SeqCst);
        let ended = wait_for(|| last_terminated(drain(&mut events))).await;
        assert_eq!(ended["reason"], "error");
        assert_eq!(m.state.debug.count(), 0);
        assert_eq!(m.state.leases.debug_owner(m.lease), None);
    }

    #[tokio::test]
    async fn reopening_a_lease_ends_its_debug_session_first() {
        let m = rig().await;
        cmd::debug_start_impl(&m.state, start_request(m.lease, "select 1"))
            .await
            .unwrap();
        m.probe.kill_all();
        m.state.leases.keepalive_tick(Duration::ZERO).await;
        editor::lease_reopen_impl(&m.state, m.lease).await.unwrap();
        assert_eq!(m.state.debug.count(), 0);
        assert_eq!(m.state.leases.debug_owner(m.lease), None);
    }

    #[tokio::test]
    async fn a_broken_lease_ends_the_session() {
        let m = rig().await;
        m.state.debug.set_poll(Duration::from_millis(10));
        let mut events = m.state.events.subscribe();
        cmd::debug_start_impl(&m.state, start_request(m.lease, "select 1"))
            .await
            .unwrap();
        // The connection dies and the keepalive notices.
        m.probe.kill_all();
        m.state.leases.keepalive_tick(Duration::ZERO).await;
        let ended = wait_for(|| last_terminated(drain(&mut events))).await;
        assert_eq!(ended["reason"], "leaseLost");
        assert_eq!(m.state.debug.count(), 0);
        assert_eq!(m.state.leases.debug_owner(m.lease), None);
    }

    #[tokio::test]
    async fn closing_the_session_ends_the_session_with_session_closed() {
        let m = rig().await;
        m.state.debug.set_poll(Duration::from_millis(10));
        let mut events = m.state.events.subscribe();
        cmd::debug_start_impl(&m.state, start_request(m.lease, "select 1"))
            .await
            .unwrap();
        sessions::close_impl(&m.state, m.sid).await.unwrap();
        let ended = wait_for(|| last_terminated(drain(&mut events))).await;
        assert_eq!(ended["reason"], "sessionClosed");
        assert_eq!(m.state.debug.count(), 0);
    }

    #[tokio::test]
    async fn stop_for_session_hook_ends_only_that_session() {
        let m = rig().await;
        let other = editor::lease_open_impl(&m.state, m.sid, LeaseOptions::default())
            .await
            .unwrap()
            .lease_id;
        cmd::debug_start_impl(&m.state, start_request(m.lease, "select 1"))
            .await
            .unwrap();
        cmd::debug_start_impl(&m.state, start_request(other, "select 2"))
            .await
            .unwrap();
        assert_eq!(m.state.debug.count(), 2);
        m.state.debug.stop_for_session(Uuid::new_v4()).await;
        assert_eq!(m.state.debug.count(), 2);
        m.state.debug.stop_for_session(m.sid).await;
        assert_eq!(m.state.debug.count(), 0);
    }

    #[tokio::test]
    async fn stop_is_idempotent_and_unknown_ids_are_not_found() {
        let m = rig().await;
        let s = cmd::debug_start_impl(&m.state, start_request(m.lease, "select 1"))
            .await
            .unwrap();
        let a = cmd::debug_stop_impl(&m.state, s.debug_id, None)
            .await
            .unwrap();
        let b = cmd::debug_stop_impl(&m.state, s.debug_id, Some(DebugEndAction::Commit))
            .await
            .unwrap();
        assert_eq!(a, b);
        assert!(matches!(
            cmd::debug_stop_impl(&m.state, Uuid::new_v4(), None).await,
            Err(Error::NotFound(_))
        ));
        assert!(matches!(
            cmd::debug_snapshot_impl(&m.state, Uuid::new_v4()).await,
            Err(Error::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn at_most_eight_sessions_at_a_time() {
        let m = rig().await;
        let mut ids = vec![];
        for i in 0..MAX_DEBUG_SESSIONS {
            let lease = if i == 0 {
                m.lease
            } else {
                editor::lease_open_impl(&m.state, m.sid, LeaseOptions::default())
                    .await
                    .unwrap()
                    .lease_id
            };
            ids.push(
                cmd::debug_start_impl(&m.state, start_request(lease, "select 1"))
                    .await
                    .unwrap()
                    .debug_id,
            );
        }
        let extra = editor::lease_open_impl(&m.state, m.sid, LeaseOptions::default())
            .await
            .unwrap()
            .lease_id;
        let err = cmd::debug_start_impl(&m.state, start_request(extra, "select 1"))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Busy(_)));
        assert_eq!(
            m.state.leases.debug_owner(extra),
            None,
            "no claim was left behind"
        );
        cmd::debug_stop_impl(&m.state, ids[0], None).await.unwrap();
        cmd::debug_start_impl(&m.state, start_request(extra, "select 1"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn restart_starts_again_with_the_edited_breakpoints_watches_and_a_new_id() {
        let m = rig().await;
        let s = cmd::debug_start_impl(&m.state, start_request(m.lease, "select 1"))
            .await
            .unwrap();
        let bp = Breakpoint {
            id: "b1".into(),
            line: 3,
            enabled: true,
            suspend: true,
            condition: None,
            log_message: None,
            hit_count: None,
        };
        let status = cmd::debug_set_breakpoints_impl(
            &m.state,
            s.debug_id,
            SourceRef::Script,
            vec![bp.clone()],
        )
        .await
        .unwrap();
        assert_eq!(status[0].line, 3);
        cmd::debug_watches_set_impl(
            &m.state,
            s.debug_id,
            vec![WatchSpec {
                id: "w".into(),
                expr: "1+1".into(),
            }],
        )
        .await
        .unwrap();
        cmd::debug_set_muted_impl(&m.state, s.debug_id, true)
            .await
            .unwrap();
        let routine = RoutineRef {
            signature: "public.f(int)".into(),
            oid: None,
            path: None,
        };
        cmd::debug_set_breakpoints_impl(
            &m.state,
            s.debug_id,
            SourceRef::Routine {
                routine: routine.clone(),
            },
            vec![bp.clone()],
        )
        .await
        .unwrap();

        let again = cmd::debug_restart_impl(&m.state, s.debug_id, "select 2".into(), None)
            .await
            .unwrap();
        assert_ne!(again.debug_id, s.debug_id);
        let reqs = m.backend.requests.lock().unwrap();
        let second = reqs.last().unwrap();
        assert_eq!(second.text, "select 2");
        assert_eq!(second.breakpoints, vec![bp.clone()]);
        assert_eq!(second.watches.len(), 1);
        assert!(second.muted);
        assert_eq!(second.routine_breakpoints[0].routine, routine);
        drop(reqs);
        assert_eq!(m.state.debug.count(), 1);
        assert_eq!(m.state.leases.debug_owner(m.lease), Some(again.debug_id));
    }

    #[tokio::test]
    async fn debug_support_is_cached_per_lease() {
        let m = rig().await;
        let a = cmd::debug_support_impl(&m.state, m.lease).await.unwrap();
        assert!(a.script && a.transactions);
        let b = cmd::debug_support_impl(&m.state, m.lease).await.unwrap();
        assert_eq!(a, b);
        assert!(matches!(
            cmd::debug_support_impl(&m.state, Uuid::new_v4()).await,
            Err(Error::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn start_loads_the_stored_routine_breakpoints_of_the_connection() {
        let _c = use_keyword_classifier();
        let m = rig().await;
        let connection_id = m.state.sessions.get_with_config(m.sid).unwrap().1.id;
        let mk = |sig: &str, id: &str, line: u32| RoutineBreakpoint {
            routine: RoutineRef {
                signature: sig.into(),
                oid: None,
                path: None,
            },
            source_hash: Some(source_hash("body")),
            breakpoint: Breakpoint {
                id: id.into(),
                line,
                enabled: true,
                suspend: true,
                condition: None,
                log_message: None,
                hit_count: None,
            },
        };
        cmd::routine_breakpoints_set_impl(
            &m.state,
            connection_id,
            vec![mk("public.f(int)", "a", 3), mk("public.g(int)", "b", 4)],
        )
        .await
        .unwrap();
        // The frontend already sent its own list for g: that one replaces the stored one.
        let mut req = start_request(m.lease, "select f(1)");
        req.routine_breakpoints = vec![mk("public.g(int)", "b2", 9)];
        cmd::debug_start_impl(&m.state, req).await.unwrap();
        let reqs = m.backend.requests.lock().unwrap();
        let got: Vec<(String, u32)> = reqs[0]
            .routine_breakpoints
            .iter()
            .map(|b| (b.routine.signature.clone(), b.breakpoint.line))
            .collect();
        assert_eq!(
            got,
            vec![
                ("public.g(int)".to_string(), 9),
                ("public.f(int)".to_string(), 3)
            ]
        );
    }

    #[tokio::test]
    async fn routine_breakpoints_roundtrip_through_the_commands() {
        let m = rig().await;
        let connection_id = m.state.sessions.get_with_config(m.sid).unwrap().1.id;
        let list = vec![RoutineBreakpoint {
            routine: RoutineRef {
                signature: "public.f(int)".into(),
                oid: Some("1".into()),
                path: None,
            },
            source_hash: Some(source_hash("body")),
            breakpoint: Breakpoint {
                id: "r1".into(),
                line: 2,
                enabled: true,
                suspend: true,
                condition: Some("i = 3".into()),
                log_message: None,
                hit_count: Some(2),
            },
        }];
        cmd::routine_breakpoints_set_impl(&m.state, connection_id, list.clone())
            .await
            .unwrap();
        assert_eq!(
            cmd::routine_breakpoints_get_impl(&m.state, connection_id)
                .await
                .unwrap(),
            list
        );
        assert!(matches!(
            cmd::routine_breakpoints_get_impl(&m.state, Uuid::new_v4()).await,
            Err(Error::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn the_read_only_gate_accepts_one_read_and_nothing_else() {
        let _c = use_keyword_classifier();
        let m = rig().await;
        let gate = HostGate::new(&m.state, m.sid, m.lease, false);
        assert!(
            gate.check_read_only(ScriptDialect::Postgres, "SELECT 1 + 1")
                .is_ok()
        );
        for bad in [
            "DELETE FROM t WHERE a = 1",
            "SELECT 1; SELECT 2",
            "INSERT INTO t VALUES (1)",
            "DROP TABLE t",
        ] {
            let err = gate
                .check_read_only(ScriptDialect::Postgres, bad)
                .unwrap_err();
            assert_eq!(err.kind(), "readOnly", "{bad}");
        }
    }

    // ----- event sink -----

    fn plain_state(debug_id: Uuid, seq: u64, status: DebugStatus) -> DebugState {
        FakeSession {
            debug_id,
            lease_id: Uuid::nil(),
            request: start_request(Uuid::nil(), ""),
            gate: Arc::new(NoGate),
            sink: Arc::new(NoSink),
            seq: AtomicU64::new(seq - 1),
            stopped: AtomicBool::new(false),
            ask_end: AtomicBool::new(false),
            awaiting: AtomicBool::new(false),
            dead: AtomicBool::new(false),
        }
        .state(status)
    }

    struct NoGate;
    impl StepGate for NoGate {
        fn check(&self, _r: GateRequest<'_>) -> Result<GatedText> {
            Err(Error::Unsupported("no gate".into()))
        }
        fn check_read_only(&self, _d: ScriptDialect, _t: &str) -> Result<GatedText> {
            Err(Error::Unsupported("no gate".into()))
        }
        fn record(&self, _s: &StepRecord) {}
    }
    struct NoSink;
    impl DebugSink for NoSink {
        fn state(&self, _s: &DebugState) {}
        fn output(&self, _e: DebugOutputEvent) {}
    }

    fn running(id: Uuid, seq: u64) -> DebugState {
        plain_state(id, seq, DebugStatus::Running { step: seq as u32 })
    }

    fn seqs(events: &[AppEvent]) -> Vec<(String, u64)> {
        events
            .iter()
            .map(|e| (e.name.clone(), e.payload["seq"].as_u64().unwrap()))
            .collect()
    }

    #[tokio::test(start_paused = true)]
    async fn running_states_are_coalesced_and_the_latest_wins() {
        let hub = crate::events::EventHub::new();
        let mut rx = hub.subscribe();
        let sink = EventSink::new(hub);
        let id = Uuid::new_v4();
        for seq in 1..=100 {
            sink.state(&running(id, seq));
        }
        // First one went out at once; the other 99 collapsed into one pending update.
        assert_eq!(seqs(&drain(&mut rx)), vec![(DEBUG_STATE.to_string(), 1)]);
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(seqs(&drain(&mut rx)), vec![(DEBUG_STATE.to_string(), 100)]);
        // Transitions to anything but Running are never delayed.
        sink.state(&running(id, 101));
        sink.state(&plain_state(
            id,
            102,
            DebugStatus::Paused {
                reason: PauseReason::Step,
            },
        ));
        let got = seqs(&drain(&mut rx));
        assert_eq!(got.last().unwrap().1, 102);
        // Nothing pending is left to arrive late.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(drain(&mut rx).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_pending_state_goes_out_before_the_next_output_so_seq_stays_ordered() {
        let hub = crate::events::EventHub::new();
        let mut rx = hub.subscribe();
        let sink = EventSink::new(hub);
        let id = Uuid::new_v4();
        sink.state(&running(id, 1));
        sink.state(&running(id, 2));
        sink.output(DebugOutputEvent {
            debug_id: id,
            seq: 3,
            output: DebugOutput::Message {
                text: "x".into(),
                source: MessageSource::Log,
            },
        });
        let got = seqs(&drain(&mut rx));
        assert_eq!(
            got,
            vec![
                (DEBUG_STATE.to_string(), 1),
                (DEBUG_STATE.to_string(), 2),
                (DEBUG_OUTPUT.to_string(), 3)
            ]
        );
        let sorted = {
            let mut v: Vec<u64> = got.iter().map(|g| g.1).collect();
            v.sort_unstable();
            v
        };
        assert_eq!(sorted, got.iter().map(|g| g.1).collect::<Vec<_>>());
    }

    // ----- pure helpers -----

    #[test]
    fn source_hash_is_sha256_hex() {
        assert_eq!(
            source_hash("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
