//! The engine drives a `RoutineDebugger` for Step Into (a scripted fake stands in for pldbgapi).

mod common;

use std::future::Future;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use common::*;
use dbstudio_core::debug::{
    BreakpointStatus, DebugAction, DebugEndAction, DebugFrame, DebugVariable, EvalContext,
    EvalValue, FrameKind, PauseReason, RoutineBreakpoint, RoutineDebugSupport, RoutineRef,
    RoutineSource, SourceRef, VariableKind,
};
use dbstudio_core::{Lease, Result, ScriptDialect, Value};
use dbstudio_debug::{DebugSettings, RoutineCommand, RoutineDebugger, RoutineStop, Step};

fn f_outer() -> RoutineRef {
    RoutineRef {
        signature: "public.f_outer(integer)".into(),
        oid: None,
        path: None,
    }
}

struct FakeRoutine {
    lease: Arc<FakeLease>,
    line: u32,
    calls: Arc<Mutex<Vec<String>>>,
    available: bool,
}

#[async_trait]
impl RoutineDebugger for FakeRoutine {
    async fn support(&mut self, _t: &dyn Lease) -> Result<RoutineDebugSupport> {
        Ok(if self.available {
            RoutineDebugSupport::Available
        } else {
            RoutineDebugSupport::Unsupported {
                reason: "no".into(),
            }
        })
    }

    async fn step_into_targets(
        &mut self,
        _t: &dyn Lease,
        step: &Step,
        _f: bool,
    ) -> Result<Vec<RoutineRef>> {
        Ok(if step.text.contains("f_outer") {
            vec![f_outer()]
        } else {
            vec![]
        })
    }

    async fn arm(
        &mut self,
        targets: &[RoutineRef],
        _b: &[RoutineBreakpoint],
    ) -> Result<Vec<BreakpointStatus>> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("arm {}", targets.len()));
        Ok(vec![])
    }

    async fn wait(
        &mut self,
        _done: &mut (dyn Future<Output = ()> + Send + Unpin),
    ) -> Result<RoutineStop> {
        self.line = 1;
        Ok(RoutineStop::At {
            routine: f_outer(),
            line: 1,
            depth: 1,
        })
    }

    async fn command(
        &mut self,
        cmd: RoutineCommand,
        _done: &mut (dyn Future<Output = ()> + Send + Unpin),
    ) -> Result<RoutineStop> {
        self.calls.lock().unwrap().push(format!("{cmd:?}"));
        match cmd {
            RoutineCommand::StepOver | RoutineCommand::StepInto if self.line < 2 => {
                self.line += 1;
                Ok(RoutineStop::At {
                    routine: f_outer(),
                    line: self.line,
                    depth: 1,
                })
            }
            RoutineCommand::Abort => {
                self.lease.release();
                Ok(RoutineStop::Aborted)
            }
            _ => {
                self.lease.release();
                Ok(RoutineStop::TargetDone)
            }
        }
    }

    async fn frames(&mut self) -> Result<Vec<DebugFrame>> {
        Ok(vec![DebugFrame {
            id: 0,
            kind: FrameKind::Routine,
            name: format!("f_outer(integer):{}", self.line),
            source: SourceRef::Routine { routine: f_outer() },
            line: self.line,
            language: Some("plpgsql".into()),
            args: None,
        }])
    }

    async fn select_frame(&mut self, _f: u32) -> Result<()> {
        Ok(())
    }

    async fn variables(&mut self) -> Result<Vec<DebugVariable>> {
        Ok(vec![DebugVariable {
            reference: "r:x".into(),
            name: "x".into(),
            kind: VariableKind::RoutineArgument,
            value: Some(Value::Int(1)),
            display: "1".into(),
            type_name: Some("integer".into()),
            editable: true,
            has_children: false,
            changed: false,
        }])
    }

    async fn set_variable(&mut self, reference: &str, value_text: &str) -> Result<DebugVariable> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("set {reference}={value_text}"));
        Ok(self.variables().await?.remove(0))
    }

    async fn source(&mut self, _r: &RoutineRef) -> Result<RoutineSource> {
        Ok(RoutineSource {
            text: "BEGIN END".into(),
            source_hash: "h".into(),
            language: "plpgsql".into(),
            first_line: 1,
        })
    }

    async fn disarm(&mut self) -> Result<()> {
        self.calls.lock().unwrap().push("disarm".into());
        Ok(())
    }
}

async fn rig(available: bool) -> (Rig, Arc<Mutex<Vec<String>>>) {
    let lease = FakeLease::new(true);
    lease.on("f_outer", vec![Reply::Block]);
    let calls = Arc::new(Mutex::new(vec![]));
    let routine = FakeRoutine {
        lease: lease.clone(),
        line: 0,
        calls: calls.clone(),
        available,
    };
    let mut req = request("SELECT f_outer(1);\nSELECT 2;");
    req.options.end_action = DebugEndAction::Rollback;
    let rig = begin_with(
        ScriptDialect::Postgres,
        req,
        lease,
        DebugSettings::default(),
        Some(Box::new(routine)),
    )
    .await;
    (rig, calls)
}

#[tokio::test]
async fn step_into_enters_the_routine_and_returns_to_the_script() {
    let (rig, calls) = rig(true).await;
    let s = rig.paused().await;
    assert!(s.caps.step_into && s.caps.routine);
    assert_eq!(s.step_into_targets, vec![f_outer()]);
    rig.h.act(DebugAction::StepInto, None).await.unwrap();
    let s = rig.paused().await;
    assert_eq!(reason(&s), Some(PauseReason::RoutineEntry));
    let pos = s.position.clone().unwrap();
    assert_eq!(
        (pos.source, pos.line),
        (SourceRef::Routine { routine: f_outer() }, 1)
    );
    assert_eq!(s.frames.len(), 2, "routine frame + script frame");
    assert_eq!(
        (s.frames[0].kind, s.frames[1].kind),
        (FrameKind::Routine, FrameKind::Script)
    );
    assert!(s.caps.step_out);
    assert!(s.variables.iter().any(|v| v.name == "Routine"));
    let vars = rig.h.variable_children("g:Routine").await.unwrap();
    assert_eq!(vars[0].name, "x");
    let r = rig.h.evaluate("x", None, false).await.unwrap();
    assert_eq!(r.context, EvalContext::RoutineVariable);
    assert!(matches!(
        r.value,
        EvalValue::Scalar {
            value: Value::Int(1),
            ..
        }
    ));
    rig.h.set_variable("r:x", "5").await.unwrap();
    assert!(calls.lock().unwrap().contains(&"set r:x=5".to_string()));
    // step over inside the routine, then the routine returns
    rig.h.act(DebugAction::StepOver, None).await.unwrap();
    let s = rig.paused().await;
    assert_eq!(
        (reason(&s), s.position.unwrap().line),
        (Some(PauseReason::Step), 2)
    );
    rig.h.act(DebugAction::StepOver, None).await.unwrap();
    let s = rig.paused().await;
    assert_eq!((reason(&s), step_of(&s)), (Some(PauseReason::Step), 1));
    assert_eq!(s.frames.len(), 1, "back at script level");
    assert_eq!(
        rig.gate.records.lock().unwrap().len(),
        1,
        "the call statement was recorded when it ended"
    );
    assert!(calls.lock().unwrap().contains(&"disarm".to_string()));
}

#[tokio::test]
async fn stop_inside_a_routine_aborts_the_target() {
    let (rig, calls) = rig(true).await;
    rig.paused().await;
    rig.h.act(DebugAction::StepInto, None).await.unwrap();
    rig.paused().await;
    let report = rig.h.stop(None).await.unwrap();
    assert!(report.rolled_back);
    assert!(calls.lock().unwrap().contains(&"Abort".to_string()));
}

#[tokio::test]
async fn resume_inside_a_routine_runs_the_rest_of_the_script() {
    let (rig, _) = rig(true).await;
    rig.paused().await;
    rig.h.act(DebugAction::StepInto, None).await.unwrap();
    rig.paused().await;
    rig.h.act(DebugAction::Resume, None).await.unwrap();
    let s = rig.paused().await;
    assert!(terminated(&s).is_some(), "{:?}", s.status);
    assert_eq!(rig.lease.ran("SELECT 2"), 1);
}

#[tokio::test]
async fn step_out_is_simulated_by_stepping_over_and_run_to_cursor_is_refused() {
    let (rig, calls) = rig(true).await;
    rig.paused().await;
    rig.h.act(DebugAction::StepInto, None).await.unwrap();
    rig.paused().await;
    assert!(
        rig.h
            .act(DebugAction::JumpTo { line: 2 }, None)
            .await
            .is_err()
    );
    rig.h.act(DebugAction::StepOut, None).await.unwrap();
    let s = rig.paused().await;
    assert_eq!(step_of(&s), 1);
    let n = calls
        .lock()
        .unwrap()
        .iter()
        .filter(|c| *c == "StepOver")
        .count();
    assert_eq!(n, 2, "stepped over until the routine returned");
}

#[tokio::test]
async fn smart_step_into_filters_targets_and_unavailable_servers_step_over() {
    let (rig, _) = rig(true).await;
    rig.paused().await;
    rig.lease.release(); // the plain step over runs the (blocking) call statement
    rig.h
        .act(
            DebugAction::SmartStepInto {
                signature: "other()".into(),
            },
            None,
        )
        .await
        .unwrap();
    let s = rig.paused().await;
    assert_eq!(
        (reason(&s), step_of(&s)),
        (Some(PauseReason::Step), 1),
        "no such target: plain step over"
    );

    let (rig, _) = rig_unavailable().await;
    let s = rig.paused().await;
    assert!(!s.caps.step_into);
}

async fn rig_unavailable() -> (Rig, Arc<Mutex<Vec<String>>>) {
    rig(false).await
}
