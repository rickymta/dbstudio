//! JSON contract fixtures of the SQL debugger (`contract/fixtures/debug.json`), read by
//! `src/lib/types/contract.test.ts`. `UPDATE_FIXTURES=1 cargo test -p dbstudio-core --test
//! contract_debug` rewrites them. Every sample is also deserialised back and compared.

use dbstudio_core::debug::*;
use dbstudio_core::testing::write_or_check;
use dbstudio_core::*;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Map, Value as Json, json};
use uuid::Uuid;

fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

/// Serialise `v`, check the value deserialises back to itself, return the JSON.
fn rt<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(v: &T) -> Json {
    let json = serde_json::to_value(v).unwrap();
    let back: T = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(&back, v, "round trip of {json}");
    json
}

fn routine() -> RoutineRef {
    RoutineRef {
        signature: "public.f_tax(numeric)".into(),
        oid: Some("16421".into()),
        path: Some(
            ObjectPath::root()
                .child(ObjectKind::Database, "app")
                .child(ObjectKind::Folder, "functions")
                .child(ObjectKind::Function, "f_tax(numeric)"),
        ),
    }
}

fn breakpoint() -> Breakpoint {
    Breakpoint {
        id: "b1".into(),
        line: 12,
        enabled: true,
        suspend: true,
        condition: Some("i = 3".into()),
        log_message: Some("i = {i}".into()),
        hit_count: Some(2),
    }
}

fn confirmation() -> ConfirmationRequest {
    ConfirmationRequest {
        token: "0123456789abcdef0123456789abcdef".into(),
        confirm_text: "app".into(),
        statements: vec![DestructiveStatement {
            index: 0,
            range: TextRange { start: 0, end: 12 },
            text: "DROP TABLE t".into(),
            reason: DestructiveReason::Drop,
        }],
        expires_in_ms: 120_000,
    }
}

fn pos() -> DebugPosition {
    DebugPosition {
        source: SourceRef::Script,
        line: 8,
        range: Some(TextRange { start: 40, end: 71 }),
        step: 2,
        step_count: 12,
    }
}

fn frame(id: u32, kind: FrameKind) -> DebugFrame {
    DebugFrame {
        id,
        kind,
        name: "f_tax(numeric):12".into(),
        source: SourceRef::Routine { routine: routine() },
        line: 12,
        language: Some("plpgsql".into()),
        args: Some("amount=120.50".into()),
    }
}

fn variable() -> DebugVariable {
    DebugVariable {
        reference: "g:routine:0".into(),
        name: "v_total".into(),
        kind: VariableKind::RoutineLocal,
        value: Some(Value::Decimal("120.50".into())),
        display: "120.50".into(),
        type_name: Some("numeric".into()),
        editable: true,
        has_children: false,
        changed: true,
    }
}

fn rows_result() -> EvalResult {
    EvalResult {
        value: EvalValue::Rows {
            result: ResultSet {
                columns: vec![ColumnMeta {
                    name: "n".into(),
                    type_name: "int8".into(),
                    value_kind: ValueKind::Int,
                    nullable: Some(false),
                }],
                rows: vec![vec![Value::Int(1)], vec![Value::Int(2)]],
                truncated: false,
            },
        },
        context: EvalContext::Lease,
        elapsed_ms: 3,
    }
}

fn state() -> DebugState {
    DebugState {
        debug_id: id(1),
        lease_id: id(2),
        seq: 7,
        status: DebugStatus::Paused {
            reason: PauseReason::Breakpoint {
                ids: vec!["b1".into()],
            },
        },
        position: Some(pos()),
        frames: vec![frame(0, FrameKind::Routine), frame(1, FrameKind::Script)],
        selected_frame: 0,
        variables: vec![variable()],
        watches: vec![WatchResult {
            id: "w1".into(),
            expr: "count(*)".into(),
            result: rows_result(),
        }],
        inline_values: vec![InlineValue {
            source: SourceRef::Script,
            line: 8,
            text: "3 rows affected".into(),
            kind: InlineKind::Result,
        }],
        breakpoints: vec![BreakpointStatus {
            id: "b1".into(),
            verified: true,
            line: 8,
            message: None,
        }],
        transaction: DebugTxInfo {
            status: TxStatus::Active,
            owned: true,
            open_for_ms: Some(61_000),
            warn: true,
            ttl_left_secs: Some(839),
        },
        caps: DebugSessionCaps {
            step_into: true,
            step_out: true,
            pause_running: false,
            set_variable: true,
            routine: true,
            jump_to: true,
        },
        step_into_targets: vec![routine()],
        muted: false,
    }
}

#[test]
fn debug_fixtures() {
    let mut s = Map::new();
    let mut put = |name: &str, v: Json| {
        assert!(s.insert(name.to_string(), v).is_none(), "duplicate {name}");
    };
    put("SourceRef", rt(&SourceRef::Script));
    put(
        "SourceRef#routine",
        rt(&SourceRef::Routine { routine: routine() }),
    );
    put("RoutineRef", rt(&routine()));
    put("Breakpoint", rt(&breakpoint()));
    put(
        "RoutineBreakpoint",
        rt(&RoutineBreakpoint {
            routine: routine(),
            source_hash: Some("ab".repeat(32)),
            breakpoint: breakpoint(),
        }),
    );
    put(
        "BreakpointStatus",
        rt(&BreakpointStatus {
            id: "b1".into(),
            verified: false,
            line: 9,
            message: Some("cannot stop inside a block".into()),
        }),
    );
    put("DebugOptions", rt(&DebugOptions::default()));
    put(
        "DebugOptions#custom",
        rt(&DebugOptions {
            stop_on_entry: Some(false),
            tx_mode: DebugTxMode::Lease,
            end_action: DebugEndAction::Keep,
            break_on_error: false,
            statement_timeout_ms: Some(30_000),
            max_rows_per_step: Some(50),
            routine: false,
            tsql_step_unit: TsqlStepUnit::Batch,
            record_history: false,
        }),
    );
    put(
        "WatchSpec",
        rt(&WatchSpec {
            id: "w1".into(),
            expr: "count(*)".into(),
        }),
    );
    put(
        "DebugStartRequest",
        rt(&DebugStartRequest {
            lease_id: id(2),
            text: "SELECT 1;\nSELECT :n;".into(),
            range: Some(TextRange { start: 0, end: 9 }),
            params: vec![ParamValue {
                name: ":n".into(),
                value: ParamInput::Null,
            }],
            breakpoints: vec![breakpoint()],
            routine_breakpoints: vec![],
            watches: vec![],
            options: DebugOptions::default(),
            muted: true,
        }),
    );
    for (name, action) in [
        ("DebugAction", DebugAction::Resume),
        ("DebugAction#stepOver", DebugAction::StepOver),
        ("DebugAction#stepInto", DebugAction::StepInto),
        ("DebugAction#forceStepInto", DebugAction::ForceStepInto),
        (
            "DebugAction#smartStepInto",
            DebugAction::SmartStepInto {
                signature: "public.f_tax(numeric)".into(),
            },
        ),
        ("DebugAction#stepOut", DebugAction::StepOut),
        (
            "DebugAction#runToCursor",
            DebugAction::RunToCursor {
                source: SourceRef::Script,
                line: 12,
                force: false,
            },
        ),
        ("DebugAction#jumpTo", DebugAction::JumpTo { line: 3 }),
        ("DebugAction#pause", DebugAction::Pause),
    ] {
        put(name, rt(&action));
    }
    let err = json!({"kind": "query", "message": "boom"});
    for (name, reason) in [
        ("PauseReason", PauseReason::Entry),
        (
            "PauseReason#breakpoint",
            PauseReason::Breakpoint {
                ids: vec!["b1".into()],
            },
        ),
        ("PauseReason#step", PauseReason::Step),
        ("PauseReason#runToCursor", PauseReason::RunToCursor),
        ("PauseReason#pause", PauseReason::Pause),
        ("PauseReason#routineEntry", PauseReason::RoutineEntry),
        (
            "PauseReason#error",
            PauseReason::Error { error: err.clone() },
        ),
        (
            "PauseReason#blocked",
            PauseReason::Blocked { error: err.clone() },
        ),
        (
            "PauseReason#confirmation",
            PauseReason::Confirmation {
                request: confirmation(),
            },
        ),
        ("PauseReason#awaitingEnd", PauseReason::AwaitingEnd),
    ] {
        put(name, rt(&reason));
    }
    put(
        "DebugStatus",
        rt(&DebugStatus::Paused {
            reason: PauseReason::Breakpoint {
                ids: vec!["b1".into()],
            },
        }),
    );
    put("DebugStatus#starting", rt(&DebugStatus::Starting));
    put("DebugStatus#running", rt(&DebugStatus::Running { step: 3 }));
    put("DebugStatus#evaluating", rt(&DebugStatus::Evaluating));
    put(
        "DebugStatus#terminated",
        rt(&DebugStatus::Terminated {
            reason: TerminateReason::Finished,
            report: Some(DebugEndReport {
                steps_run: 12,
                committed: false,
                rolled_back: true,
            }),
        }),
    );
    put("DebugPosition", rt(&pos()));
    put("DebugFrame", rt(&frame(0, FrameKind::Routine)));
    put("DebugVariable", rt(&variable()));
    put(
        "EvalValue",
        rt(&EvalValue::Scalar {
            value: Value::Int(42),
            type_name: "int8".into(),
        }),
    );
    put("EvalValue#rows", rt(&rows_result().value));
    put(
        "EvalValue#error",
        rt(&EvalValue::Error { error: err.clone() }),
    );
    put(
        "EvalValue#skipped",
        rt(&EvalValue::Skipped {
            reason: "MULTI is open".into(),
        }),
    );
    put("EvalResult", rt(&rows_result()));
    put(
        "WatchResult",
        rt(&WatchResult {
            id: "w1".into(),
            expr: "count(*)".into(),
            result: rows_result(),
        }),
    );
    put(
        "InlineValue",
        rt(&InlineValue {
            source: SourceRef::Routine { routine: routine() },
            line: 5,
            text: "v_total = 120.50".into(),
            kind: InlineKind::Variable,
        }),
    );
    put(
        "DebugTxInfo",
        rt(&DebugTxInfo {
            status: TxStatus::Idle,
            owned: false,
            open_for_ms: None,
            warn: false,
            ttl_left_secs: None,
        }),
    );
    put(
        "DebugSessionCaps",
        rt(&DebugSessionCaps {
            step_into: false,
            step_out: false,
            pause_running: true,
            set_variable: false,
            routine: false,
            jump_to: true,
        }),
    );
    put("DebugState", rt(&state()));
    let step_end = StatementEnd {
        outcome: EndOutcome::Affected { rows: 3 },
        elapsed_ms: 4,
    };
    put(
        "DebugOutput",
        rt(&DebugOutput::StepStart {
            step: 1,
            line: 12,
            text: "UPDATE t SET a = 1".into(),
        }),
    );
    put(
        "DebugOutput#query",
        rt(&DebugOutput::Query {
            step: 1,
            event: QueryEvent::Message {
                message: "NOTICE: hi".into(),
            },
        }),
    );
    put(
        "DebugOutput#stepEnd",
        rt(&DebugOutput::StepEnd {
            step: 1,
            end: step_end,
            truncated_at: Some(200),
        }),
    );
    for (name, source) in [
        ("DebugOutput#message", MessageSource::Notice),
        ("DebugOutput#messagePrint", MessageSource::Print),
        ("DebugOutput#messageDbmsOutput", MessageSource::DbmsOutput),
        ("DebugOutput#messageLog", MessageSource::Log),
        ("DebugOutput#messageInfo", MessageSource::Info),
    ] {
        put(
            name,
            rt(&DebugOutput::Message {
                text: "text".into(),
                source,
            }),
        );
    }
    put(
        "DebugOutput#error",
        rt(&DebugOutput::Error {
            step: Some(2),
            error: err,
        }),
    );
    put(
        "DebugOutput#evaluated",
        rt(&DebugOutput::Evaluated {
            expr: "1 + 1".into(),
            result: rows_result(),
        }),
    );
    put(
        "DebugOutputEvent",
        rt(&DebugOutputEvent {
            debug_id: id(1),
            seq: 8,
            output: DebugOutput::Message {
                text: "done".into(),
                source: MessageSource::Info,
            },
        }),
    );
    put(
        "DebugEndReport",
        rt(&DebugEndReport {
            steps_run: 1,
            committed: true,
            rolled_back: false,
        }),
    );
    for (name, support) in [
        ("RoutineDebugSupport", RoutineDebugSupport::Available),
        (
            "RoutineDebugSupport#needsExtension",
            RoutineDebugSupport::NeedsExtension {
                statement: "CREATE EXTENSION pldbgapi;".into(),
            },
        ),
        (
            "RoutineDebugSupport#needsServerConfig",
            RoutineDebugSupport::NeedsServerConfig {
                hint: "add plugin_debugger to shared_preload_libraries".into(),
            },
        ),
        (
            "RoutineDebugSupport#noPermission",
            RoutineDebugSupport::NoPermission {
                hint: "superuser required".into(),
            },
        ),
        (
            "RoutineDebugSupport#unsupported",
            RoutineDebugSupport::Unsupported {
                reason: "no debugger API".into(),
            },
        ),
    ] {
        put(name, rt(&support));
    }
    put(
        "DebugSupport",
        rt(&DebugSupport {
            script: true,
            routine: RoutineDebugSupport::Available,
            transactions: true,
            script_variables: false,
        }),
    );
    put(
        "RoutineSource",
        rt(&RoutineSource {
            text: "BEGIN RETURN 1; END".into(),
            source_hash: "cd".repeat(32),
            language: "plpgsql".into(),
            first_line: 1,
        }),
    );
    for (name, v) in [
        ("DebugTxMode", rt(&DebugTxMode::Wrap)),
        ("DebugEndAction", rt(&DebugEndAction::Ask)),
        ("TerminateReason", rt(&TerminateReason::Ttl)),
        ("FrameKind", rt(&FrameKind::Block)),
        ("VariableKind", rt(&VariableKind::TempTable)),
        ("EvalContext", rt(&EvalContext::Proxy)),
        ("InlineKind", rt(&InlineKind::Error)),
        ("MessageSource", rt(&MessageSource::Notice)),
    ] {
        put(name, v);
    }
    write_or_check("debug", &Json::Object(s));
}

#[test]
fn defaults_apply_to_sparse_json() {
    let req: DebugStartRequest = serde_json::from_value(json!({
        "leaseId": "00000000-0000-0000-0000-000000000002",
        "text": "SELECT 1"
    }))
    .unwrap();
    assert!(req.options.break_on_error && req.options.routine && req.options.record_history);
    assert_eq!(req.options.tx_mode, DebugTxMode::Wrap);
    assert!(req.breakpoints.is_empty() && !req.muted);
    let bp: Breakpoint = serde_json::from_value(json!({"id": "x", "line": 3})).unwrap();
    assert!(bp.enabled && bp.suspend && bp.condition.is_none());
}
