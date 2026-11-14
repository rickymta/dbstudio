//! Contract v2 of the backlog batch: JSON fixtures of the types defined in this crate
//! (`contract/fixtures/app.json`), the stub commands and the per-session capabilities.

use std::sync::Arc;

use async_trait::async_trait;
use dbstudio_core::testing::write_or_check;
use dbstudio_core::{
    Capabilities, ConnectionConfig, DbKind, Driver, DriverDescriptor, DriverRegistry, DriverStatus,
    Error, ExecSummary, LeaseState, ObjectNode, ObjectPath, QueryRequest, Result, ResultSink,
    ScriptDialect, Secret, ServerInfo, Session, TxStatus,
};
use serde::Serialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::commands::app::{DiagnosticCheck, DriverDiagnostics};
use crate::commands::editor::{ScriptGenerated, lease_reopen_impl, script_generate_impl};
use crate::commands::{plan, sessions};
use crate::events::{SESSION_STATE, SessionHealth, SessionStateEvent};
use crate::lease::{LeaseInfo, LeaseReopened};
use crate::state::testing::test_state;

fn j<T: Serialize>(v: &T) -> Value {
    serde_json::to_value(v).unwrap()
}

#[test]
fn app_contract_fixtures() {
    let event = SessionStateEvent {
        session_id: Uuid::from_u128(1),
        connection_id: Uuid::from_u128(2),
        state: SessionHealth::Reconnecting,
        attempt: 2,
        error: Some(json!({"kind": "connection", "message": "connection refused"})),
        leases_broken: vec![Uuid::from_u128(3)],
        leases_reopened: vec![Uuid::from_u128(4)],
    };
    let checks = vec![
        DiagnosticCheck {
            id: "driverRegistered".into(),
            ok: true,
            value: None,
            detail: None,
        },
        DiagnosticCheck {
            id: "nativeTool:pgDump".into(),
            ok: true,
            value: Some("/usr/bin/pg_dump".into()),
            detail: Some("pg_dump (PostgreSQL) 17.2".into()),
        },
    ];
    let lease = LeaseInfo {
        lease_id: Uuid::from_u128(4),
        session_id: Uuid::from_u128(1),
        state: LeaseState {
            auto_commit: true,
            transaction: TxStatus::Idle,
            running: None,
            pending_result: None,
            database: Some("app".into()),
            schema: None,
            broken: false,
        },
    };
    write_or_check(
        "app",
        &json!({
            "SessionStateEvent": j(&event),
            "SessionHealth": j(&SessionHealth::GaveUp),
            "DiagnosticCheck": j(&checks[1]),
            "DriverDiagnostics": j(&DriverDiagnostics {
                status: DriverStatus::Available,
                message: "The PostgreSQL driver is available.".into(),
                help_url: None,
                checks,
            }),
            "LeaseReopened": j(&LeaseReopened {
                lease,
                transaction_lost: true,
            }),
            "ScriptGenerated": j(&ScriptGenerated {
                text: "SELECT * FROM t LIMIT 100".into(),
                dialect: ScriptDialect::Postgres,
            }),
        }),
    );
    assert_eq!(SESSION_STATE, "session://state");
}

#[tokio::test]
async fn package_e_commands_reject_unknown_ids() {
    let (state, _) = test_state(DriverRegistry::new());
    let not_found = |e: Error, what: &str| {
        assert_eq!(e.kind(), "notFound", "{what}: {e:?}");
    };
    not_found(
        sessions::reconnect_impl(&state, Uuid::new_v4(), None)
            .await
            .unwrap_err(),
        "session_reconnect",
    );
    not_found(
        lease_reopen_impl(&state, Uuid::new_v4()).await.unwrap_err(),
        "lease_reopen",
    );
    not_found(
        script_generate_impl(
            &state,
            Uuid::new_v4(),
            ObjectPath::root(),
            dbstudio_sql::ScriptTemplate::Select,
        )
        .await
        .unwrap_err(),
        "script_generate",
    );
    not_found(
        plan::history_get_impl(&state, Uuid::new_v4())
            .await
            .unwrap_err(),
        "plan_history_get",
    );
    not_found(
        plan::history_delete_impl(&state, Uuid::new_v4())
            .await
            .unwrap_err(),
        "plan_history_delete",
    );
    assert!(
        plan::history_list_impl(&state, None, 10, None)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(plan::history_clear_impl(&state, None).await.unwrap(), 0);
}

/// Driver whose sessions narrow the descriptor's capabilities (flavour / version gates).
#[derive(Debug)]
struct NarrowDriver;

#[derive(Debug)]
struct NarrowSession;

#[async_trait]
impl Driver for NarrowDriver {
    fn descriptor(&self) -> DriverDescriptor {
        DriverDescriptor::new(
            DbKind::CockroachDb,
            Capabilities::planned_for(DbKind::CockroachDb),
            DriverStatus::Available,
        )
    }

    async fn connect(
        &self,
        _config: &ConnectionConfig,
        _secret: Option<&Secret>,
    ) -> Result<Box<dyn Session>> {
        Ok(Box::new(NarrowSession))
    }
}

#[async_trait]
impl Session for NarrowSession {
    async fn ping(&self) -> Result<()> {
        Ok(())
    }
    async fn server_info(&self) -> Result<ServerInfo> {
        Ok(ServerInfo::default())
    }
    async fn execute(&self, _req: QueryRequest, _sink: &mut dyn ResultSink) -> Result<ExecSummary> {
        Err(Error::Unsupported("execute".into()))
    }
    async fn list_children(&self, _parent: &ObjectPath) -> Result<Vec<ObjectNode>> {
        Ok(vec![])
    }
    fn capabilities(&self) -> Option<Capabilities> {
        Some(Capabilities {
            triggers: true,
            ..Capabilities::planned_for(DbKind::CockroachDb)
        })
    }
    async fn close(&self) -> Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn session_open_reports_the_capabilities_of_the_server() {
    let mut registry = DriverRegistry::new();
    registry.register(Arc::new(NarrowDriver));
    let (state, _) = test_state(registry);
    assert!(!Capabilities::planned_for(DbKind::CockroachDb).triggers);
    let config = ConnectionConfig::new(DbKind::CockroachDb, "crdb", "localhost");
    let info = state.sessions.open(&config, None).await.unwrap();
    assert!(
        info.descriptor.capabilities.triggers,
        "Session::capabilities overrides the descriptor"
    );
}
