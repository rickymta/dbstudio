//! Application events (`job://progress`, `lease://state`, `backup://changed`).
//!
//! Managers hold an `Arc<dyn EventEmitter>`. The real [`EventHub`] fans every event out to the
//! Tauri webview (once [`EventHub::attach_tauri`] was called) and to a tokio broadcast channel
//! that the dev bridge (`GET /events`) and tests subscribe to.

use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::Value;
use tauri::Emitter as _;
use tokio::sync::broadcast;

/// Event name of [`dbstudio_core::JobEvent`] payloads.
pub const JOB_PROGRESS: &str = "job://progress";
/// Event name of `{leaseId, state}` payloads.
pub const LEASE_STATE: &str = "lease://state";
/// Event name emitted (payload `null`) when backup jobs changed.
#[allow(dead_code)] // emitted by the backup package
pub const BACKUP_CHANGED: &str = "backup://changed";

/// Event name of [`SessionStateEvent`] payloads (connection health, CONN-12).
#[allow(dead_code)] // emitted by the session health monitor (package E)
pub const SESSION_STATE: &str = "session://state";

/// Health of a live session as reported by `session://state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)] // produced by the session health monitor (package E)
pub enum SessionHealth {
    /// Connected (also sent after a manual reconnect).
    Connected,
    /// The connection was lost.
    Lost,
    /// A reconnect attempt is running (`attempt` counts from 1).
    Reconnecting,
    /// The session is connected again; its id did not change.
    Reconnected,
    /// Automatic reconnect gave up.
    GaveUp,
}

/// Payload of `session://state`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)] // produced by the session health monitor (package E)
pub struct SessionStateEvent {
    /// Session whose health changed.
    pub session_id: uuid::Uuid,
    /// Profile of the session.
    pub connection_id: uuid::Uuid,
    /// New health.
    pub state: SessionHealth,
    /// Reconnect attempt number (0 when not reconnecting).
    pub attempt: u32,
    /// The error that caused the state (a serialized `dbstudio_core::Error`), if any.
    pub error: Option<Value>,
    /// Leases that could not be reopened because a transaction was open.
    pub leases_broken: Vec<uuid::Uuid>,
}

/// Capacity of the broadcast channel; slow subscribers lose the oldest events.
const CHANNEL_CAPACITY: usize = 256;

/// One emitted event.
#[derive(Debug, Clone, PartialEq)]
pub struct AppEvent {
    /// Event name, e.g. [`JOB_PROGRESS`].
    pub name: String,
    /// JSON payload.
    pub payload: Value,
}

/// Something that can publish application events.
pub trait EventEmitter: Send + Sync + std::fmt::Debug {
    /// Publish `payload` under `name`. Never blocks and never fails.
    fn emit(&self, name: &str, payload: Value);
}

/// Serialize `payload` and publish it.
pub fn emit_json(emitter: &dyn EventEmitter, name: &str, payload: &impl Serialize) {
    match serde_json::to_value(payload) {
        Ok(v) => emitter.emit(name, v),
        Err(e) => tracing::warn!(event = name, error = %e, "cannot serialize event payload"),
    }
}

/// [`EventEmitter`] that drops everything (managers built outside the application).
#[derive(Debug, Default, Clone, Copy)]
pub struct NullEmitter;

impl EventEmitter for NullEmitter {
    fn emit(&self, _name: &str, _payload: Value) {}
}

/// Event fan-out shared by the Tauri app handle and the dev bridge.
pub struct EventHub {
    tx: broadcast::Sender<AppEvent>,
    tauri: Mutex<Option<tauri::AppHandle>>,
}

impl std::fmt::Debug for EventHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventHub")
            .field("subscribers", &self.tx.receiver_count())
            .finish_non_exhaustive()
    }
}

impl EventHub {
    /// Hub without a Tauri handle (tests, early startup).
    pub fn new() -> Arc<Self> {
        let (tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        Arc::new(EventHub {
            tx,
            tauri: Mutex::new(None),
        })
    }

    /// Also deliver every event to the webview through `handle`.
    pub fn attach_tauri(&self, handle: tauri::AppHandle) {
        *self.tauri.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
    }

    /// Receive every event emitted from now on.
    #[allow(dead_code)] // used by the dev bridge and tests
    pub fn subscribe(&self) -> broadcast::Receiver<AppEvent> {
        self.tx.subscribe()
    }
}

impl EventEmitter for EventHub {
    fn emit(&self, name: &str, payload: Value) {
        let handle = self.tauri.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(handle) = handle {
            if let Err(e) = handle.emit(name, payload.clone()) {
                tracing::debug!(event = name, error = %e, "cannot emit tauri event");
            }
        }
        // No subscriber is not an error.
        let _ = self.tx.send(AppEvent {
            name: name.to_string(),
            payload,
        });
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[tokio::test]
    async fn hub_broadcasts_to_every_subscriber() {
        let hub = EventHub::new();
        let mut a = hub.subscribe();
        let mut b = hub.subscribe();
        hub.emit(JOB_PROGRESS, json!({"type": "progress"}));
        emit_json(&*hub, BACKUP_CHANGED, &Value::Null);
        for rx in [&mut a, &mut b] {
            let first = rx.recv().await.unwrap();
            assert_eq!(first.name, "job://progress");
            assert_eq!(first.payload["type"], "progress");
            assert_eq!(rx.recv().await.unwrap().name, "backup://changed");
        }
    }

    #[test]
    fn session_state_event_shape() {
        let event = SessionStateEvent {
            session_id: uuid::Uuid::nil(),
            connection_id: uuid::Uuid::nil(),
            state: SessionHealth::GaveUp,
            attempt: 5,
            error: Some(json!({"kind": "connection", "message": "refused"})),
            leases_broken: vec![],
        };
        let v = serde_json::to_value(&event).unwrap();
        assert_eq!(v["state"], "gaveUp");
        assert_eq!(v["leasesBroken"], json!([]));
        assert_eq!(v["error"]["kind"], "connection");
        assert_eq!(SESSION_STATE, "session://state");
    }

    #[test]
    fn emitting_without_subscribers_is_harmless() {
        EventHub::new().emit(LEASE_STATE, json!(1));
        NullEmitter.emit(LEASE_STATE, json!(1));
    }
}
