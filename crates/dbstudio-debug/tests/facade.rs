//! `DebugEngine::start` / `DebugHandle` are the real actor, not a stub (scripted lease).

mod common;

use std::sync::Arc;

use common::*;
use dbstudio_core::ScriptDialect;
use dbstudio_core::debug::{DebugAction, DebugStatus, TerminateReason};
use dbstudio_debug::{DebugEngine, DebugSettings};

#[tokio::test]
async fn the_facade_starts_the_real_actor_and_forwards_calls() {
    let lease = FakeLease::new(true);
    let h = DebugEngine::start(
        profile(ScriptDialect::MySql {
            ansi_quotes: false,
            backslash_escapes: true,
            mariadb: false,
        }),
        lease.clone(),
        Arc::new(FakeGate::default()),
        Arc::new(RecSink::default()),
        Arc::new(NoProxy),
        DebugSettings::default(),
        request("SELECT 1;\nSELECT 2;"),
    )
    .await
    .expect("start");
    let first = h.snapshot().await.expect("snapshot");
    assert_eq!(first.debug_id, h.debug_id());
    assert!(matches!(first.status, DebugStatus::Paused { .. }));
    let after = h.act(DebugAction::StepOver, None).await.expect("act");
    assert!(matches!(
        after.status,
        DebugStatus::Running { .. } | DebugStatus::Paused { .. }
    ));
    let report = h.stop(None).await.expect("stop");
    let again = h.stop(None).await.expect("stop is idempotent");
    assert_eq!(report.steps_run, again.steps_run);
    let end = h.snapshot().await.expect("snapshot");
    assert!(matches!(
        end.status,
        DebugStatus::Terminated {
            reason: TerminateReason::Stopped,
            ..
        }
    ));
}
