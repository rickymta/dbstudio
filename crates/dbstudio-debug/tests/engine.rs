//! Script debugger engine against a scripted lease and a fake guard (design 6.2, row D).

mod common;

use common::*;
use dbstudio_core::debug::{
    Breakpoint, DebugAction, DebugEndAction, DebugOutput, DebugStatus, EvalValue, PauseReason,
    SourceRef, TerminateReason, WatchSpec,
};
use dbstudio_core::{Error, Lease, ParamInput, ParamValue, ScriptDialect, TxStatus, Value};
use dbstudio_debug::DebugSettings;

const PG: ScriptDialect = ScriptDialect::Postgres;

fn bp(id: &str, line: u32) -> Breakpoint {
    Breakpoint {
        id: id.into(),
        line,
        enabled: true,
        suspend: true,
        condition: None,
        log_message: None,
        hit_count: None,
    }
}

fn three() -> &'static str {
    "SELECT 1;\nSELECT 2;\nSELECT 3;"
}

fn rollback_end() -> dbstudio_core::debug::DebugOptions {
    dbstudio_core::debug::DebugOptions {
        end_action: DebugEndAction::Rollback,
        ..Default::default()
    }
}

#[tokio::test]
async fn enters_paused_then_steps_over_to_the_end() {
    let mut req = request(three());
    req.options = rollback_end();
    let rig = begin(PG, req).await;
    let s = rig.paused().await;
    assert_eq!(reason(&s), Some(PauseReason::Entry));
    assert_eq!((step_of(&s), s.position.unwrap().step_count), (0, 3));
    assert!(s.transaction.owned);
    assert_eq!(rig.lease.ran("<begin>"), 1);
    assert_eq!(rig.lease.ran("SELECT 1"), 0, "nothing ran yet");

    let s = rig.step().await;
    assert_eq!((reason(&s), step_of(&s)), (Some(PauseReason::Step), 1));
    assert_eq!(rig.lease.ran("SELECT 1;"), 0);
    assert_eq!(rig.lease.ran("SELECT 1"), 1);
    let s = rig.step().await;
    assert_eq!(step_of(&s), 2);
    let s = rig.step().await;
    assert_eq!(terminated(&s), Some(TerminateReason::Finished));
    assert_eq!(rig.lease.ran("<rollback>"), 1);
    let DebugStatus::Terminated { report, .. } = s.status else {
        panic!()
    };
    let report = report.unwrap();
    assert_eq!(report.steps_run, 3);
    assert!(report.rolled_back && !report.committed);
    // history: one record per step, none for probes
    assert_eq!(rig.gate.records.lock().unwrap().len(), 3);
    // idempotent stop after the end
    assert_eq!(rig.h.stop(None).await.unwrap(), report);
}

#[tokio::test]
async fn console_gets_start_rows_and_end_of_every_step() {
    let mut req = request("SELECT id FROM t;\nUPDATE t SET a = 1");
    req.options = rollback_end();
    let lease = FakeLease::new(true);
    lease.on(
        "select id",
        vec![Reply::Rows(vec!["id"], vec![vec![int(5)]])],
    );
    lease.on("update t", vec![Reply::Affected(3)]);
    lease.on("notice me", vec![Reply::Messages(vec!["hello"])]);
    let rig = begin_with(PG, req, lease, DebugSettings::default(), None).await;
    rig.paused().await;
    rig.resume().await;
    let out = rig.sink.outputs();
    let kinds: Vec<&str> = out
        .iter()
        .filter_map(|o| match o {
            DebugOutput::StepStart { .. } => Some("start"),
            DebugOutput::Query { .. } => Some("query"),
            DebugOutput::StepEnd { .. } => Some("end"),
            _ => None,
        })
        .collect();
    assert_eq!(kinds, ["start", "query", "query", "end", "start", "end"]);
    let s = rig.h.snapshot().await.unwrap();
    assert!(
        s.inline_values
            .iter()
            .any(|v| v.text == "3 rows affected" && v.line == 2)
    );
    assert!(
        s.inline_values
            .iter()
            .any(|v| v.text == "1 row: id=5" && v.line == 1)
    );
    // seq is strictly increasing over states and outputs together
    let mut seqs: Vec<u64> = rig
        .sink
        .states
        .lock()
        .unwrap()
        .iter()
        .map(|s| s.seq)
        .collect();
    seqs.extend(rig.sink.outputs.lock().unwrap().iter().map(|o| o.seq));
    let mut sorted = seqs.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), seqs.len(), "no repeated seq");
}

#[tokio::test]
async fn notices_go_to_the_console_with_their_source() {
    let mut req = request("SELECT notice_me();");
    req.options = rollback_end();
    let lease = FakeLease::new(true);
    lease.on("notice_me", vec![Reply::Messages(vec!["NOTICE: hi"])]);
    let rig = begin_with(PG, req, lease, DebugSettings::default(), None).await;
    rig.paused().await;
    rig.resume().await;
    assert!(rig.sink.messages().contains(&(
        "NOTICE: hi".to_string(),
        dbstudio_core::debug::MessageSource::Notice
    )));
}

#[tokio::test]
async fn breakpoint_stops_before_its_statement_and_snaps() {
    let mut req = request("SELECT 1;\n\n-- gap\nSELECT 2;\nSELECT 3;");
    req.options = rollback_end();
    req.breakpoints = vec![bp("a", 3), bp("b", 99)];
    let rig = begin(PG, req).await;
    // a bp is enabled, so no stop on entry: runs to it
    let s = rig.paused().await;
    assert_eq!(
        reason(&s),
        Some(PauseReason::Breakpoint {
            ids: vec!["a".into()]
        })
    );
    assert_eq!(step_of(&s), 1);
    assert_eq!(rig.lease.ran("SELECT 1"), 1);
    assert_eq!(rig.lease.ran("SELECT 2"), 0);
    let st = &s.breakpoints;
    assert!(st[0].verified && st[0].line == 3);
    assert!(!st[1].verified && st[1].message.is_some());
    let s = rig.resume().await;
    assert_eq!(terminated(&s), Some(TerminateReason::Finished));
}

#[tokio::test]
async fn muted_breakpoints_do_not_stop() {
    let mut req = request(three());
    req.options = rollback_end();
    req.options.stop_on_entry = Some(true);
    req.breakpoints = vec![bp("a", 2)];
    let rig = begin(PG, req).await;
    rig.paused().await;
    rig.h.set_muted(true).await.unwrap();
    let s = rig.resume().await;
    assert_eq!(terminated(&s), Some(TerminateReason::Finished));
}

#[tokio::test]
async fn disabled_breakpoint_is_skipped_and_enabled_after_edit_stops() {
    let mut req = request(three());
    req.options = rollback_end();
    req.options.stop_on_entry = Some(true);
    let mut off = bp("a", 2);
    off.enabled = false;
    req.breakpoints = vec![off];
    let rig = begin(PG, req).await;
    rig.paused().await;
    rig.h
        .set_breakpoints(SourceRef::Script, vec![bp("a", 3)])
        .await
        .unwrap();
    let s = rig.resume().await;
    assert_eq!(step_of(&s), 2);
}

#[tokio::test]
async fn breakpoint_condition_decides() {
    for (answer, stops) in [(0, false), (1, true)] {
        let mut req = request(three());
        req.options = rollback_end();
        let mut b = bp("a", 2);
        b.condition = Some("i = 3".into());
        req.breakpoints = vec![b];
        let lease = FakeLease::new(true);
        lease.on(
            "CASE WHEN (i = 3)",
            vec![Reply::Rows(vec!["c"], vec![vec![int(answer)]])],
        );
        let rig = begin_with(PG, req, lease, DebugSettings::default(), None).await;
        let s = rig.paused().await;
        assert_eq!(stops, reason(&s).is_some(), "answer {answer}");
        assert!(
            rig.lease
                .log()
                .iter()
                .any(|t| t == "SELECT CASE WHEN (i = 3) THEN 1 ELSE 0 END"),
            "condition ran through the read-only frame"
        );
    }
}

#[tokio::test]
async fn failing_condition_stops_with_a_message() {
    let mut req = request(three());
    req.options = rollback_end();
    let mut b = bp("a", 2);
    b.condition = Some("nope".into());
    req.breakpoints = vec![b];
    let lease = FakeLease::new(true);
    lease.on(
        "CASE WHEN (nope)",
        vec![Reply::Fail("column nope does not exist".into())],
    );
    let rig = begin_with(PG, req, lease, DebugSettings::default(), None).await;
    let s = rig.paused().await;
    assert!(matches!(reason(&s), Some(PauseReason::Breakpoint { .. })));
    assert!(
        rig.sink
            .messages()
            .iter()
            .any(|(m, _)| m.contains("condition failed"))
    );
}

#[tokio::test]
async fn hit_count_counts_repeated_batches() {
    // `GO 3` repeats the batch: the breakpoint stops on the 2nd and the 3rd hit
    let mut req = request("SELECT 1\nGO 3\n");
    req.options = rollback_end();
    let mut b = bp("a", 1);
    b.hit_count = Some(2);
    req.breakpoints = vec![b];
    let rig = begin(ScriptDialect::MsSql, req).await;
    let s = rig.paused().await;
    assert_eq!(step_of(&s), 1, "second copy of the batch");
    assert_eq!(rig.lease.ran("SELECT 1"), 1);
    let s = rig.resume().await;
    assert_eq!(step_of(&s), 2);
    let s = rig.resume().await;
    assert_eq!(terminated(&s), Some(TerminateReason::Finished));
}

#[tokio::test]
async fn log_message_expands_expressions_and_does_not_stop() {
    let mut req = request(three());
    req.options = rollback_end();
    let mut b = bp("a", 2);
    b.suspend = false;
    b.log_message = Some("two is {1 + 1}!".into());
    req.breakpoints = vec![b];
    let lease = FakeLease::new(true);
    lease.on(
        "SELECT 1 + 1",
        vec![Reply::Rows(vec!["?column?"], vec![vec![int(2)]])],
    );
    let rig = begin_with(PG, req, lease, DebugSettings::default(), None).await;
    let s = rig.paused().await;
    assert_eq!(terminated(&s), Some(TerminateReason::Finished));
    assert!(rig.sink.messages().contains(&(
        "two is 2!".to_string(),
        dbstudio_core::debug::MessageSource::Log
    )));
}

#[tokio::test]
async fn run_to_cursor_stops_at_the_target_and_force_skips_breakpoints() {
    let mut req = request("SELECT 1;\nSELECT 2;\nSELECT 3;\nSELECT 4;");
    req.options = rollback_end();
    req.options.stop_on_entry = Some(true);
    req.breakpoints = vec![bp("a", 2)];
    let rig = begin(PG, req).await;
    rig.paused().await;
    let to = |force| DebugAction::RunToCursor {
        source: SourceRef::Script,
        line: 4,
        force,
    };
    rig.h.act(to(false), None).await.unwrap();
    let s = rig.paused().await;
    assert_eq!(step_of(&s), 1, "stops at the earlier breakpoint first");
    rig.h.act(to(true), None).await.unwrap();
    let s = rig.paused().await;
    assert_eq!(
        (reason(&s), step_of(&s)),
        (Some(PauseReason::RunToCursor), 3)
    );
    assert_eq!(rig.lease.ran("SELECT 4"), 0);
    // behind the cursor is refused
    let back = DebugAction::RunToCursor {
        source: SourceRef::Script,
        line: 1,
        force: false,
    };
    assert!(matches!(
        rig.h.act(back, None).await,
        Err(Error::Invalid(_))
    ));
}

#[tokio::test]
async fn pause_stops_before_the_next_step() {
    let mut req = request(three());
    req.options = rollback_end();
    req.options.stop_on_entry = Some(true);
    let lease = FakeLease::new(true);
    lease.on("SELECT 1", vec![Reply::Block]);
    let rig = begin_with(PG, req, lease, DebugSettings::default(), None).await;
    rig.paused().await;
    rig.h.act(DebugAction::Resume, None).await.unwrap();
    rig.running().await;
    // while a statement runs only Pause and Stop are accepted
    assert!(matches!(
        rig.h.act(DebugAction::StepOver, None).await,
        Err(Error::Busy(_))
    ));
    assert!(matches!(
        rig.h.evaluate("1", None, false).await,
        Err(Error::Busy(_))
    ));
    rig.h.act(DebugAction::Pause, None).await.unwrap();
    rig.lease.release();
    let s = rig.paused().await;
    assert_eq!((reason(&s), step_of(&s)), (Some(PauseReason::Pause), 1));
}

#[tokio::test]
async fn stop_cancels_the_running_statement_and_rolls_back() {
    let mut req = request(three());
    req.options = rollback_end();
    req.options.stop_on_entry = Some(false);
    let lease = FakeLease::new(true);
    lease.on("SELECT 1", vec![Reply::Block]);
    let rig = begin_with(PG, req, lease, DebugSettings::default(), None).await;
    rig.running().await;
    let report = rig.h.stop(None).await.unwrap();
    assert!(report.rolled_back);
    let s = rig.h.snapshot().await.unwrap();
    assert_eq!(terminated(&s), Some(TerminateReason::Stopped));
    assert_eq!(rig.lease.ran("<rollback>"), 1);
    assert_eq!(rig.lease.ran("SELECT 2"), 0);
}

#[tokio::test]
async fn jump_to_moves_the_cursor_without_running() {
    let mut req = request(three());
    req.options = rollback_end();
    let rig = begin(PG, req).await;
    rig.paused().await;
    rig.h
        .act(DebugAction::JumpTo { line: 3 }, None)
        .await
        .unwrap();
    let s = rig.paused().await;
    assert_eq!(step_of(&s), 2);
    assert_eq!(rig.lease.ran("SELECT 1"), 0);
    let s = rig.step().await;
    assert_eq!(terminated(&s), Some(TerminateReason::Finished));
    assert_eq!(rig.lease.ran("SELECT 3"), 1);
    assert_eq!(rig.lease.ran("SELECT 2"), 0);
}

#[tokio::test]
async fn a_failing_statement_pauses_and_resume_skips_it() {
    let mut req = request(three());
    req.options = rollback_end();
    req.options.stop_on_entry = Some(false);
    let lease = FakeLease::new(true);
    lease.on("SELECT 2", vec![Reply::Fail("boom".into())]);
    let rig = begin_with(PG, req, lease, DebugSettings::default(), None).await;
    let s = rig.paused().await;
    let Some(PauseReason::Error { error }) = reason(&s) else {
        panic!("{:?}", s.status)
    };
    assert_eq!(error["message"], "boom");
    assert_eq!(step_of(&s), 1);
    assert!(s.inline_values.iter().any(|v| v.text == "boom"));
    let s = rig.resume().await;
    assert_eq!(terminated(&s), Some(TerminateReason::Finished));
    assert_eq!(
        rig.lease.ran("SELECT 2"),
        1,
        "the failed step is not retried"
    );
    assert_eq!(rig.lease.ran("SELECT 3"), 1);
}

#[tokio::test]
async fn without_break_on_error_the_script_ends_with_error() {
    let mut req = request(three());
    req.options = rollback_end();
    req.options.break_on_error = false;
    req.options.stop_on_entry = Some(false);
    let lease = FakeLease::new(true);
    lease.on("SELECT 2", vec![Reply::Fail("boom".into())]);
    let rig = begin_with(PG, req, lease, DebugSettings::default(), None).await;
    let s = rig.paused().await;
    assert_eq!(terminated(&s), Some(TerminateReason::Error));
    assert_eq!(rig.lease.ran("<rollback>"), 1);
    assert_eq!(rig.lease.ran("SELECT 3"), 0);
}

#[tokio::test]
async fn read_only_guard_blocks_and_jump_to_goes_on() {
    let mut req = request("DELETE FROM t WHERE a = 1;\nSELECT 2;");
    req.options = rollback_end();
    let lease = FakeLease::new(true);
    let rig = begin_with(PG, req, lease, DebugSettings::default(), None).await;
    *rig.gate.read_only.lock().unwrap() = true;
    rig.paused().await;
    let s = rig.step().await;
    let Some(PauseReason::Blocked { error }) = reason(&s) else {
        panic!("{:?}", s.status)
    };
    assert_eq!(error["kind"], "readOnly");
    assert_eq!(step_of(&s), 0);
    assert_eq!(
        rig.lease.ran("DELETE"),
        0,
        "the statement never reached the lease"
    );
    rig.h
        .act(DebugAction::JumpTo { line: 2 }, None)
        .await
        .unwrap();
    rig.paused().await;
    let s = rig.step().await;
    assert_eq!(terminated(&s), Some(TerminateReason::Finished));
    assert_eq!(rig.lease.ran("SELECT 2"), 1);
}

#[tokio::test]
async fn production_guard_asks_and_the_grant_lets_the_statement_run() {
    let mut req = request("DELETE FROM t;\nSELECT 2;");
    req.options = rollback_end();
    let rig = begin(PG, req).await;
    *rig.gate.production.lock().unwrap() = true;
    rig.paused().await;
    let s = rig.step().await;
    let Some(PauseReason::Confirmation { request }) = reason(&s) else {
        panic!("{:?}", s.status)
    };
    assert_eq!(request.token, TOKEN);
    assert_eq!(rig.lease.ran("DELETE"), 0);
    // resuming without the grant asks again; with it the statement runs
    let s = rig.step().await;
    assert!(matches!(reason(&s), Some(PauseReason::Confirmation { .. })));
    rig.h
        .act(DebugAction::StepOver, Some(grant()))
        .await
        .unwrap();
    let s = rig.paused().await;
    assert_eq!((reason(&s), step_of(&s)), (Some(PauseReason::Step), 1));
    assert_eq!(rig.lease.ran("DELETE"), 1);
}

#[tokio::test]
async fn lease_loss_ends_the_session() {
    let mut req = request(three());
    req.options = rollback_end();
    let rig = begin(PG, req).await;
    rig.paused().await;
    rig.lease.break_it();
    let s = rig.step().await;
    assert_eq!(terminated(&s), Some(TerminateReason::LeaseLost));
}

// ---------------------------------------------------------------------------- transactions

#[tokio::test]
async fn end_actions_commit_rollback_keep_and_ask() {
    for (end, commit, rollback, left_open) in [
        (DebugEndAction::Commit, 1, 0, false),
        (DebugEndAction::Rollback, 0, 1, false),
        (DebugEndAction::Keep, 0, 0, true),
    ] {
        let mut req = request("SELECT 1;");
        req.options.end_action = end;
        let rig = begin(PG, req).await;
        rig.paused().await;
        let s = rig.step().await;
        assert_eq!(terminated(&s), Some(TerminateReason::Finished), "{end:?}");
        assert_eq!(rig.lease.ran("<commit>"), commit, "{end:?}");
        assert_eq!(rig.lease.ran("<rollback>"), rollback, "{end:?}");
        assert_eq!(
            rig.lease.state().transaction != TxStatus::Idle,
            left_open,
            "{end:?}"
        );
    }
    // Ask: the script end waits for the decision, Rollback is the default
    let rig = begin(PG, request("SELECT 1;")).await;
    rig.paused().await;
    let s = rig.step().await;
    assert_eq!(reason(&s), Some(PauseReason::AwaitingEnd));
    assert!(matches!(
        rig.h.act(DebugAction::Resume, None).await,
        Err(Error::Invalid(_))
    ));
    let report = rig.h.stop(Some(DebugEndAction::Commit)).await.unwrap();
    assert!(report.committed);
    let rig = begin(PG, request("SELECT 1;")).await;
    rig.paused().await;
    rig.step().await;
    let report = rig.h.stop(None).await.unwrap();
    assert!(report.rolled_back && !report.committed);
}

#[tokio::test]
async fn stop_with_ask_waits_for_the_decision_when_a_transaction_is_owned() {
    let rig = begin(PG, request(three())).await;
    rig.paused().await;
    rig.step().await;
    rig.h.stop(None).await.unwrap();
    let s = rig.h.snapshot().await.unwrap();
    assert_eq!(reason(&s), Some(PauseReason::AwaitingEnd));
    rig.h.stop(Some(DebugEndAction::Rollback)).await.unwrap();
    let s = rig.h.snapshot().await.unwrap();
    assert_eq!(terminated(&s), Some(TerminateReason::Stopped));
}

#[tokio::test]
async fn lease_mode_never_touches_the_tab_transaction() {
    let mut req = request("SELECT 1;");
    req.options.tx_mode = dbstudio_core::debug::DebugTxMode::Lease;
    req.options.end_action = DebugEndAction::Rollback;
    let lease = FakeLease::new(false);
    lease.set_tx(TxStatus::Active);
    let rig = begin_with(PG, req, lease, DebugSettings::default(), None).await;
    let s = rig.paused().await;
    assert!(!s.transaction.owned);
    rig.step().await;
    assert_eq!(
        rig.lease.ran("<begin>") + rig.lease.ran("<rollback>") + rig.lease.ran("<commit>"),
        0
    );
    assert_eq!(rig.lease.state().transaction, TxStatus::Active);
}

#[tokio::test(start_paused = true)]
async fn paused_too_long_warns_then_ends_with_rollback() {
    let rig = begin(PG, request(three())).await;
    let s = rig.paused().await;
    assert!(s.transaction.owned && !s.transaction.warn);
    assert_eq!(s.transaction.ttl_left_secs, Some(900));
    tokio::time::advance(std::time::Duration::from_secs(61)).await;
    let s = rig.h.wait_until(|s| s.transaction.warn).await;
    assert!(s.transaction.open_for_ms.unwrap() >= 60_000);
    assert!(matches!(s.status, DebugStatus::Paused { .. }));
    tokio::time::advance(std::time::Duration::from_secs(900)).await;
    let s = rig
        .h
        .wait_until(|s| matches!(s.status, DebugStatus::Terminated { .. }))
        .await;
    assert_eq!(terminated(&s), Some(TerminateReason::Ttl));
    assert_eq!(rig.lease.ran("<rollback>"), 1);
}

#[tokio::test(start_paused = true)]
async fn any_command_restarts_the_ttl() {
    let rig = begin(PG, request(three())).await;
    rig.paused().await;
    tokio::time::advance(std::time::Duration::from_secs(800)).await;
    rig.h.set_muted(false).await.unwrap();
    tokio::time::advance(std::time::Duration::from_secs(800)).await;
    let s = rig.h.snapshot().await.unwrap();
    assert!(
        matches!(s.status, DebugStatus::Paused { .. }),
        "restarted at 800 s"
    );
    tokio::time::advance(std::time::Duration::from_secs(200)).await;
    let s = rig
        .h
        .wait_until(|s| matches!(s.status, DebugStatus::Terminated { .. }))
        .await;
    assert_eq!(terminated(&s), Some(TerminateReason::Ttl));
}

#[tokio::test]
async fn dropping_every_handle_rolls_the_transaction_back() {
    let rig = begin(PG, request(three())).await;
    rig.paused().await;
    let Rig { h, lease, .. } = rig;
    let probe = h.clone();
    drop(h);
    drop(probe);
    for _ in 0..50 {
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        if lease.ran("<rollback>") == 1 {
            return;
        }
    }
    panic!("not rolled back");
}

// ------------------------------------------------------------------ read-only frame, watches

async fn paused_pg(text: &str, tx: Option<TxStatus>) -> Rig {
    let mut req = request(text);
    req.options.end_action = DebugEndAction::Rollback;
    if tx.is_some() {
        req.options.tx_mode = dbstudio_core::debug::DebugTxMode::Lease;
    }
    let lease = FakeLease::new(true);
    if let Some(t) = tx {
        lease.set_tx(t);
    }
    let rig = begin_with(PG, req, lease, DebugSettings::default(), None).await;
    rig.paused().await;
    rig
}

fn tail(rig: &Rig, n: usize) -> Vec<String> {
    let log = rig.lease.log();
    log[log.len().saturating_sub(n)..].to_vec()
}

#[tokio::test]
async fn evaluate_uses_the_frame_of_the_transaction_state() {
    // idle lease: BEGIN READ ONLY .. ROLLBACK
    let rig = paused_pg(three(), Some(TxStatus::Idle)).await;
    let r = rig.h.evaluate("1 + 1", None, false).await.unwrap();
    assert!(matches!(
        r.value,
        EvalValue::Scalar { .. } | EvalValue::Rows { .. } | EvalValue::Skipped { .. }
    ));
    assert_eq!(
        tail(&rig, 3),
        ["BEGIN READ ONLY", "SELECT 1 + 1", "ROLLBACK"]
    );
    // open transaction: savepoint, rolled back and released
    let rig = paused_pg(three(), None).await;
    rig.h.evaluate("1 + 1", None, true).await.unwrap();
    assert_eq!(
        tail(&rig, 4),
        [
            "SAVEPOINT __dbg",
            "SELECT 1 + 1",
            "ROLLBACK TO SAVEPOINT __dbg",
            "RELEASE SAVEPOINT __dbg"
        ]
    );
    assert!(
        rig.sink
            .outputs()
            .iter()
            .any(|o| matches!(o, DebugOutput::Evaluated { .. }))
    );
    // failed transaction: nothing is evaluated
    let rig = paused_pg(three(), Some(TxStatus::Failed)).await;
    let before = rig.lease.log().len();
    let r = rig.h.evaluate("1", None, false).await.unwrap();
    assert!(matches!(r.value, EvalValue::Skipped { .. }));
    assert_eq!(rig.lease.log().len(), before);
}

#[tokio::test]
async fn the_closing_statement_runs_even_when_the_expression_fails() {
    let rig = paused_pg(three(), None).await;
    rig.lease.on("boom()", vec![Reply::Fail("boom".into())]);
    let r = rig.h.evaluate("boom()", None, false).await.unwrap();
    let EvalValue::Error { error } = r.value else {
        panic!()
    };
    assert_eq!(error["message"], "boom");
    assert_eq!(
        tail(&rig, 2),
        ["ROLLBACK TO SAVEPOINT __dbg", "RELEASE SAVEPOINT __dbg"]
    );
}

#[tokio::test]
async fn watches_cannot_write() {
    let rig = paused_pg(three(), None).await;
    let before = rig.lease.ran("DELETE");
    let w = rig
        .h
        .set_watches(vec![
            WatchSpec {
                id: "1".into(),
                expr: "count(*) FROM t".into(),
            },
            WatchSpec {
                id: "2".into(),
                expr: "DELETE FROM t".into(),
            },
        ])
        .await
        .unwrap();
    let EvalValue::Error { error } = &w[1].result.value else {
        panic!("{:?}", w[1].result.value)
    };
    assert_eq!(error["kind"], "readOnly");
    assert_eq!(rig.lease.ran("DELETE"), before, "never reached the lease");
    // watches are re-evaluated at every pause
    let n = rig.lease.ran("count(*) FROM t");
    rig.step().await;
    assert_eq!(rig.lease.ran("count(*) FROM t"), n + 1);
    let s = rig.h.snapshot().await.unwrap();
    assert_eq!(s.watches.len(), 2);
}

#[tokio::test]
async fn a_slow_watch_is_cancelled_after_the_timeout() {
    let mut req = request(three());
    req.options.end_action = DebugEndAction::Rollback;
    let lease = FakeLease::new(true);
    lease.on("slow_fn", vec![Reply::Block]);
    let settings = DebugSettings {
        watch_timeout_ms: 30,
        ..DebugSettings::default()
    };
    let rig = begin_with(PG, req, lease, settings, None).await;
    rig.paused().await;
    let r = rig.h.evaluate("slow_fn()", None, false).await.unwrap();
    let EvalValue::Error { error } = r.value else {
        panic!("{:?}", r.value)
    };
    assert_eq!(error["kind"], "timeout");
    assert_eq!(tail(&rig, 1), ["RELEASE SAVEPOINT __dbg"]);
}

// ----------------------------------------------------------------------------- variables

#[tokio::test]
async fn parameters_are_editable_and_bound_per_step() {
    let mut req = request("SELECT :a;\nSELECT :a + 1;");
    req.options.end_action = DebugEndAction::Rollback;
    req.params = vec![ParamValue {
        name: ":a".into(),
        value: ParamInput::Number("1".into()),
    }];
    let rig = begin(PG, req).await;
    let s = rig.paused().await;
    assert!(s.variables.iter().any(|v| v.name == "Parameters"));
    let kids = rig.h.variable_children("g:Parameters").await.unwrap();
    assert_eq!(
        (
            kids[0].name.as_str(),
            kids[0].display.as_str(),
            kids[0].editable
        ),
        (":a", "1", true)
    );
    rig.step().await;
    let v = rig.h.set_variable("p::a", "9").await.unwrap();
    assert_eq!(v.display, "9");
    rig.step().await;
    assert_eq!(rig.lease.ran("SELECT 1;"), 0);
    assert_eq!(rig.lease.ran("SELECT 1"), 1, "first step bound with 1");
    assert_eq!(
        rig.lease.ran("SELECT 9 + 1"),
        1,
        "second step bound with the edited value"
    );
}

#[tokio::test]
async fn session_group_loads_at_once_and_transaction_group_shows_ownership() {
    let rig = paused_pg(three(), None).await;
    rig.lease.on(
        "current_database()",
        vec![Reply::Rows(
            vec!["database", "pid"],
            vec![vec![Value::Text("app".into()), int(42)]],
        )],
    );
    let s = rig.step().await;
    let names: Vec<_> = s.variables.iter().map(|v| v.name.as_str()).collect();
    assert!(
        names.contains(&"Session") && names.contains(&"Settings") && names.contains(&"Last result")
    );
    let sess = rig.h.variable_children("g:Session").await.unwrap();
    assert!(sess.iter().any(|v| v.name == "pid" && v.display == "42"));
    let tx = rig.h.variable_children("g:Transaction").await.unwrap();
    assert!(
        tx.iter()
            .any(|v| v.name == "owned by the debugger" && v.display == "true")
    );
    let last = rig.h.variable_children("g:Last result").await.unwrap();
    assert!(last[0].display.contains("done in"));
    assert!(rig.h.variable_children("nope").await.is_err());
}

#[tokio::test]
async fn settings_are_changed_through_set_and_the_guard() {
    let rig = paused_pg(three(), None).await;
    rig.lease.on(
        "pg_settings",
        vec![Reply::Rows(
            vec!["name", "setting", "unit"],
            vec![vec![
                Value::Text("work_mem".into()),
                Value::Text("64".into()),
                Value::Text("kB".into()),
            ]],
        )],
    );
    let kids = rig.h.variable_children("g:Settings").await.unwrap();
    assert_eq!(kids[0].reference, "st:work_mem");
    assert!(kids[0].editable);
    rig.h.set_variable("st:work_mem", "128kB").await.unwrap();
    assert_eq!(tail(&rig, 1), ["SET work_mem = '128kB'"]);
    assert!(rig.h.set_variable("zz:what", "1").await.is_err());
}

#[tokio::test]
async fn mysql_script_variables_and_the_row_count_rule() {
    let my = ScriptDialect::mysql(false, true);
    let mut req = request("SET @x = 5;\nSELECT ROW_COUNT();\nSELECT @x;");
    req.options.end_action = DebugEndAction::Rollback;
    req.watches = vec![WatchSpec {
        id: "w".into(),
        expr: "@x".into(),
    }];
    let lease = FakeLease::new(true);
    lease.on(
        "SELECT @x AS `@x`",
        vec![Reply::Rows(vec!["@x"], vec![vec![int(5)]])],
    );
    let rig = begin_with(my, req, lease, DebugSettings::default(), None).await;
    let s = rig.paused().await;
    // the statement about to run (SET @x) keeps no counter: the watch evaluates
    assert!(matches!(
        s.watches[0].result.value,
        EvalValue::Scalar { .. }
    ));
    // now at `SELECT ROW_COUNT()`: probes and watches wait so the counter survives
    let s = rig.step().await;
    assert!(
        matches!(&s.watches[0].result.value, EvalValue::Skipped { reason } if reason.contains("ROW_COUNT"))
    );
    let kids = rig.h.variable_children("g:Script variables").await.unwrap();
    assert!(kids[0].display.contains("ROW_COUNT"), "{kids:?}");
    // at `SELECT @x` nothing keeps the counters any more
    let s = rig.step().await;
    assert!(matches!(
        s.watches[0].result.value,
        EvalValue::Scalar { .. }
    ));
    let kids = rig.h.variable_children("g:Script variables").await.unwrap();
    assert_eq!(kids[0].display, "5");
    // frame of an open transaction on MySQL
    assert!(rig.lease.log().iter().any(|t| t == "SAVEPOINT __dbg"));
    let v = rig.h.set_variable("sv:@x", "7").await.unwrap();
    assert_eq!(v.display, "'7'");
    assert_eq!(tail(&rig, 1), ["SET @x = 7"]);
}

#[tokio::test]
async fn mysql_wraps_a_statement_with_inner_semicolons_for_the_driver() {
    let my = ScriptDialect::mysql(false, true);
    let mut req = request(
        "DELIMITER $$\nCREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END$$\nDELIMITER ;\nSELECT 3;",
    );
    req.options.end_action = DebugEndAction::Rollback;
    let rig = begin_with(
        my,
        req,
        FakeLease::new(true),
        DebugSettings::default(),
        None,
    )
    .await;
    let s = rig.paused().await;
    assert_eq!(s.position.as_ref().unwrap().step_count, 2);
    rig.step().await;
    let sent = rig
        .lease
        .log()
        .into_iter()
        .find(|t| t.contains("CREATE PROCEDURE"))
        .unwrap();
    assert!(sent.starts_with("DELIMITER "), "{sent}");
}

// -------------------------------------------------------------------------------- T-SQL

#[tokio::test]
async fn tsql_shim_carries_variables_between_statements() {
    let mut req = request("DECLARE @a int = 1;\nSET @a = @a + 1;\nSELECT @a");
    req.options.end_action = DebugEndAction::Rollback;
    let lease = FakeLease::new(true);
    lease.on(
        "SET @a = @a + 1",
        vec![Reply::Rows(vec!["@a"], vec![vec![int(2)]])],
    );
    lease.on(
        "DECLARE @a int = 1",
        vec![Reply::Rows(vec!["@a"], vec![vec![int(1)]])],
    );
    let rig = begin_with(
        ScriptDialect::MsSql,
        req,
        lease,
        DebugSettings::default(),
        None,
    )
    .await;
    let s = rig.paused().await;
    assert_eq!(s.position.as_ref().unwrap().step_count, 3);
    rig.step().await;
    assert_eq!(
        tail(&rig, 4)
            .iter()
            .find(|t| t.starts_with("DECLARE"))
            .unwrap(),
        "DECLARE @a int = 1; SELECT @a AS [@a]"
    );
    rig.step().await;
    let second = rig
        .lease
        .log()
        .into_iter()
        .find(|t| t.contains("SET @a = @a + 1"))
        .unwrap();
    assert_eq!(
        second,
        "DECLARE @a int; SELECT @a = CAST(N'1' AS int); SET @a = @a + 1; SELECT @a AS [@a]"
    );
    let kids = rig.h.variable_children("g:Script variables").await.unwrap();
    assert_eq!(
        (kids[0].name.as_str(), kids[0].display.as_str()),
        ("@a", "2")
    );
    // the read-out never reaches the Console
    let rows_out = rig
        .sink
        .outputs()
        .into_iter()
        .filter(|o| matches!(o, DebugOutput::Query { .. }))
        .count();
    assert_eq!(rows_out, 0);
    // Set Value feeds the next preamble
    rig.h.set_variable("sv:@a", "41").await.unwrap();
    rig.step().await;
    assert!(
        rig.lease
            .log()
            .iter()
            .any(|t| t.contains("CAST(N'41' AS int)"))
    );
}

#[tokio::test]
async fn tsql_unsafe_batch_runs_as_one_block() {
    let mut req = request("DECLARE @a int = 1;\nIF @a = 1 SELECT 2;\nSELECT 3");
    req.options.end_action = DebugEndAction::Rollback;
    let rig = begin(ScriptDialect::MsSql, req).await;
    let s = rig.paused().await;
    assert_eq!(s.position.as_ref().unwrap().step_count, 1);
    assert_eq!(s.frames[0].kind, dbstudio_core::debug::FrameKind::Block);
    let s = rig.step().await;
    assert_eq!(terminated(&s), Some(TerminateReason::Finished));
    assert_eq!(rig.lease.ran("IF @a = 1"), 1);
    assert_eq!(rig.lease.ran("CAST"), 0, "no shim for a fallback batch");
}

#[tokio::test]
async fn tsql_print_goes_to_the_console_as_print() {
    let mut req = request("PRINT 'hi'");
    req.options.end_action = DebugEndAction::Rollback;
    let lease = FakeLease::new(true);
    lease.on("PRINT", vec![Reply::Messages(vec!["hi"])]);
    let rig = begin_with(
        ScriptDialect::MsSql,
        req,
        lease,
        DebugSettings::default(),
        None,
    )
    .await;
    rig.paused().await;
    rig.resume().await;
    assert!(
        rig.sink
            .messages()
            .contains(&("hi".into(), dbstudio_core::debug::MessageSource::Print))
    );
}

// ------------------------------------------------------------------------- other dialects

#[tokio::test]
async fn redis_multi_suspends_watches() {
    let mut req = request("SET a 1\nMULTI\nSET b 2\nEXEC");
    req.options.end_action = DebugEndAction::Rollback;
    req.options.stop_on_entry = Some(true);
    req.watches = vec![WatchSpec {
        id: "w".into(),
        expr: "GET a".into(),
    }];
    let lease = FakeLease::new(true);
    lease.on(
        "GET a",
        vec![Reply::Rows(
            vec!["value"],
            vec![vec![Value::Text("1".into())]],
        )],
    );
    let rig = begin_with(
        ScriptDialect::RedisCli,
        req,
        lease,
        DebugSettings::default(),
        None,
    )
    .await;
    let s = rig.paused().await;
    assert!(matches!(
        s.watches[0].result.value,
        EvalValue::Scalar { .. }
    ));
    rig.step().await;
    let s = rig.step().await; // MULTI ran
    assert!(
        matches!(&s.watches[0].result.value, EvalValue::Skipped { reason } if reason.contains("MULTI"))
    );
    let s = rig.step().await; // SET b queued
    assert!(matches!(
        s.watches[0].result.value,
        EvalValue::Skipped { .. }
    ));
    let sess = rig.h.variable_children("g:Session").await.unwrap();
    assert!(
        sess.iter()
            .any(|v| v.name == "MULTI" && v.display.contains("1 queued"))
    );
    rig.step().await; // EXEC
    let s = rig.h.snapshot().await.unwrap();
    assert!(terminated(&s).is_some());
}

#[tokio::test]
async fn mongo_and_oracle_scripts_step_statement_by_statement() {
    let mut req = request("db.a.find({});\ndb.b.find({})");
    req.options.end_action = DebugEndAction::Rollback;
    let rig = begin(ScriptDialect::MongoShell, req).await;
    let s = rig.paused().await;
    assert_eq!(s.position.unwrap().step_count, 2);
    rig.resume().await;
    assert_eq!(rig.lease.ran("db.b.find"), 1);

    let mut req = request("SET SERVEROUTPUT ON\nSELECT 1 FROM dual;\nBEGIN null; END;\n/\n");
    req.options.end_action = DebugEndAction::Rollback;
    let rig = begin(ScriptDialect::Oracle, req).await;
    let s = rig.paused().await;
    assert_eq!(s.position.unwrap().step_count, 3);
    assert_eq!(
        rig.lease.ran("SET SERVEROUTPUT"),
        1,
        "sent although it is a SQL*Plus command"
    );
    let s = rig.step().await;
    assert_eq!(s.frames[0].kind, dbstudio_core::debug::FrameKind::Block);
    rig.resume().await;
    assert_eq!(rig.lease.ran("BEGIN null; END;"), 1);
    assert!(
        rig.lease.log().iter().any(|t| t == "SAVEPOINT dbg")
            || rig
                .lease
                .log()
                .iter()
                .any(|t| t.contains("SET TRANSACTION READ ONLY"))
    );
}

#[tokio::test]
async fn databases_without_transactions_run_in_lease_mode() {
    let mut req = request("SELECT 1;");
    req.options.end_action = DebugEndAction::Ask;
    let lease = FakeLease::new(true);
    let gate = std::sync::Arc::new(FakeGate::default());
    let sink = std::sync::Arc::new(RecSink::default());
    let mut p = profile(ScriptDialect::ClickHouse);
    p.caps.manual_transactions = false;
    let h = dbstudio_debug::actor::start(
        p,
        lease.clone(),
        gate,
        sink,
        std::sync::Arc::new(NoProxy),
        DebugSettings::default(),
        req,
        None,
    )
    .await
    .unwrap();
    let s = h
        .wait_until(|s| matches!(s.status, DebugStatus::Paused { .. }))
        .await;
    assert!(!s.transaction.owned);
    assert!(!s.variables.iter().any(|v| v.name == "Transaction"));
    h.act(DebugAction::Resume, None).await.unwrap();
    let s = h
        .wait_until(|s| matches!(s.status, DebugStatus::Terminated { .. }))
        .await;
    assert_eq!(terminated(&s), Some(TerminateReason::Finished));
    assert_eq!(lease.ran("<begin>"), 0);
}

#[tokio::test]
async fn step_into_without_a_routine_debugger_is_step_over_with_a_note() {
    let mut req = request(three());
    req.options = rollback_end();
    let rig = begin(PG, req).await;
    rig.paused().await;
    rig.h.act(DebugAction::StepInto, None).await.unwrap();
    let s = rig.paused().await;
    assert_eq!((reason(&s), step_of(&s)), (Some(PauseReason::Step), 1));
    assert!(
        rig.sink
            .messages()
            .iter()
            .any(|(m, _)| m.contains("Step Into is not available"))
    );
    assert!(!s.caps.step_into);
}

#[tokio::test]
async fn step_out_is_refused_at_script_level() {
    let rig = begin(PG, request(three())).await;
    rig.paused().await;
    assert!(matches!(
        rig.h.act(DebugAction::StepOut, None).await,
        Err(Error::Invalid(_))
    ));
}

#[tokio::test]
async fn range_and_empty_scripts() {
    let mut req = request("SELECT 1;\nSELECT 2;\nSELECT 3;");
    req.range = Some(dbstudio_core::TextRange { start: 10, end: 19 });
    req.options = rollback_end();
    let rig = begin(PG, req).await;
    let s = rig.paused().await;
    assert_eq!(s.position.unwrap().step_count, 1);
    rig.step().await;
    assert_eq!(rig.lease.ran("SELECT 2"), 1);
    assert_eq!(rig.lease.ran("SELECT 1"), 0);

    let mut req = request("-- only a comment\n");
    req.options = rollback_end();
    let rig = begin(PG, req).await;
    let s = rig
        .h
        .wait_until(|s| matches!(s.status, DebugStatus::Terminated { .. }))
        .await;
    assert_eq!(terminated(&s), Some(TerminateReason::Finished));
}
