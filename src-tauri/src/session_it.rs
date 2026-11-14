//! Integration tests of the connection-resilience features against real servers (PostgreSQL 16
//! and MySQL 8.4 from `docker/compose.test.yml`): lease keepalive and automatic reconnect after
//! the server killed the connections. They run only when `DBSTUDIO_IT=1`.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dbstudio_core::{
    ConnectionConfig, DbKind, LeaseOptions, QueryRequest, Secret, TxOptions, TxStatus,
    execute_collect,
};
use dbstudio_store::{ConnectionProfile, SecretChange};
use uuid::Uuid;

use crate::commands::{connections, editor, sessions};
use crate::events::AppEvent;
use crate::session::{Credentials, StateCredentials};
use crate::state::testing::test_state;
use crate::state::{AppState, build_registry};

const PASSWORD: &str = "dbstudio_test";

fn enabled() -> bool {
    std::env::var("DBSTUDIO_IT").as_deref() == Ok("1")
}

struct Server {
    kind: DbKind,
    port: u16,
    options: BTreeMap<String, String>,
}

fn pg(app_name: &str) -> Server {
    Server {
        kind: DbKind::Postgres,
        port: 54316,
        options: BTreeMap::from([("application_name".to_string(), app_name.to_string())]),
    }
}

fn mysql() -> Server {
    Server {
        kind: DbKind::MySql,
        port: 33068,
        // One pooled connection, so its id is known.
        options: BTreeMap::from([("pool_max".to_string(), "1".to_string())]),
    }
}

async fn open(state: &AppState, server: &Server, name: &str) -> Uuid {
    let mut config = ConnectionConfig::new(server.kind, name, "127.0.0.1");
    config.port = server.port;
    config.database = Some("dbstudio".into());
    config.username = Some("dbstudio".into());
    config.connect_timeout_ms = Some(5000);
    config.options = server.options.clone();
    let profile = ConnectionProfile {
        config,
        group_id: None,
        color: None,
        save_password: true,
        has_password: false,
        sort_order: 0,
        created_at: String::new(),
        updated_at: String::new(),
    };
    let saved = connections::save_impl(
        state,
        profile,
        SecretChange::Set {
            value: Secret::new(PASSWORD),
        },
    )
    .await
    .unwrap();
    sessions::open_impl(state, saved.config.id, None)
        .await
        .unwrap()
        .session_id
}

async fn scalar(state: &AppState, sid: Uuid, sql: &str) -> String {
    let session = state.sessions.get(sid).unwrap();
    let results = execute_collect(&*session, QueryRequest::new(sql))
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    format!("{:?}", first_value(&results))
}

/// Ids (pids / connection ids) of the connections a session and its leases use, plus the
/// statement that kills them from another connection.
async fn kill_all(state: &AppState, victim: Uuid, admin: Uuid, server: &Server, leases: &[Uuid]) {
    let session = state.sessions.get(admin).unwrap();
    let kill: Vec<String> = match server.kind {
        DbKind::Postgres => {
            let app = server.options["application_name"].clone();
            vec![format!(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                 WHERE application_name = '{app}' AND pid <> pg_backend_pid()"
            )]
        }
        _ => {
            let mut ids = vec![id_of(state, victim).await];
            for lease in leases {
                let (_, lease) = state.leases.get_with_session(*lease).unwrap();
                let mut sink = dbstudio_core::CollectingSink::new();
                lease
                    .execute(QueryRequest::new("SELECT CONNECTION_ID()"), &mut sink)
                    .await
                    .unwrap();
                ids.push(first_cell(sink.into_results()));
            }
            ids.into_iter().map(|id| format!("KILL {id}")).collect()
        }
    };
    for sql in kill {
        execute_collect(&*session, QueryRequest::new(sql))
            .await
            .unwrap_or_else(|e| panic!("kill failed: {e}"));
    }
}

fn first_value(results: &[dbstudio_core::StatementResult]) -> dbstudio_core::Value {
    match &results[0].outcome {
        dbstudio_core::StatementOutcome::Rows(set) => set.rows[0][0].clone(),
        other => panic!("expected rows, got {other:?}"),
    }
}

fn first_cell(results: Vec<dbstudio_core::StatementResult>) -> String {
    match &first_value(&results) {
        dbstudio_core::Value::Int(i) => i.to_string(),
        dbstudio_core::Value::UInt(u) => u.to_string(),
        other => panic!("unexpected id {other:?}"),
    }
}

async fn id_of(state: &AppState, sid: Uuid) -> String {
    let session = state.sessions.get(sid).unwrap();
    first_cell(
        execute_collect(&*session, QueryRequest::new("SELECT CONNECTION_ID()"))
            .await
            .unwrap(),
    )
}

async fn wait_state(rx: &mut tokio::sync::broadcast::Receiver<AppEvent>, want: &str) -> u64 {
    let deadline = Instant::now() + Duration::from_secs(40);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let ev = tokio::time::timeout(left, rx.recv())
            .await
            .unwrap_or_else(|_| panic!("no `{want}` session event"))
            .unwrap();
        if ev.name == "session://state" && ev.payload["state"] == want {
            return ev.payload["attempt"].as_u64().unwrap();
        }
    }
}

async fn keepalive_scenario(server: Server) {
    let (state, _) = test_state(build_registry());
    let sid = open(&state, &server, "victim").await;
    let admin_server = Server {
        kind: server.kind,
        port: server.port,
        options: if server.kind == DbKind::Postgres {
            BTreeMap::from([("application_name".into(), "dbstudio-it-admin".into())])
        } else {
            server.options.clone()
        },
    };
    let admin = open(&state, &admin_server, "admin").await;
    let lease = editor::lease_open_impl(&state, sid, LeaseOptions::default())
        .await
        .unwrap();
    // The lease works, then its backend is killed from another connection.
    state
        .leases
        .get(lease.lease_id)
        .unwrap()
        .ping()
        .await
        .unwrap();
    let healthy = state.leases.state_of(lease.lease_id).unwrap();
    assert!(!healthy.broken);
    kill_all(&state, sid, admin, &server, &[lease.lease_id]).await;

    // The real background loop with a short interval: detected within two intervals.
    let state = Arc::new(state);
    let interval = Duration::from_millis(400);
    state.leases.spawn_keepalive(move || Some(interval));
    let started = Instant::now();
    loop {
        if state.leases.state_of(lease.lease_id).unwrap().broken {
            break;
        }
        assert!(
            started.elapsed() < interval * 2 + Duration::from_millis(600),
            "lease not flagged broken within two keepalive intervals"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(state.leases.needs_reopen(lease.lease_id).unwrap());
}

async fn reconnect_scenario(server: Server) {
    let (state, _) = test_state(build_registry());
    let mut rx = state.events.subscribe();
    let sid = open(&state, &server, "victim").await;
    let admin_server = Server {
        kind: server.kind,
        port: server.port,
        options: if server.kind == DbKind::Postgres {
            BTreeMap::from([("application_name".into(), "dbstudio-it-admin".into())])
        } else {
            server.options.clone()
        },
    };
    let admin = open(&state, &admin_server, "admin").await;
    // The session's own connection is in use, and two leases exist: one idle, one inside a
    // transaction.
    scalar(&state, sid, "SELECT 1").await;
    let idle = editor::lease_open_impl(&state, sid, LeaseOptions::default())
        .await
        .unwrap();
    let in_tx = editor::lease_open_impl(
        &state,
        sid,
        LeaseOptions {
            auto_commit: false,
            ..LeaseOptions::default()
        },
    )
    .await
    .unwrap();
    state
        .leases
        .get(in_tx.lease_id)
        .unwrap()
        .begin(TxOptions::default())
        .await
        .unwrap();
    assert_eq!(
        state.leases.state_of(in_tx.lease_id).unwrap().transaction,
        TxStatus::Active
    );

    kill_all(
        &state,
        sid,
        admin,
        &server,
        &[idle.lease_id, in_tx.lease_id],
    )
    .await;
    assert!(state.sessions.get(sid).unwrap().ping().await.is_err());

    // One health round finds the dead session and reconnects it by itself.
    let creds: Arc<dyn Credentials> = Arc::new(StateCredentials(state.clone()));
    let lost = state.sessions.health_pass(&creds, true).await;
    assert!(lost.contains(&sid), "the victim is reported lost: {lost:?}");
    wait_state(&mut rx, "lost").await;
    let attempt = wait_state(&mut rx, "reconnected").await;
    assert!(attempt >= 1);

    // Same session id, working again.
    assert_eq!(scalar(&state, sid, "SELECT 1").await, "Int(1)");
    let root = sessions::children_impl(&state, sid, dbstudio_core::ObjectPath::root())
        .await
        .unwrap();
    assert!(!root.is_empty());
    // The idle lease lives again under the same id; the one with a transaction is flagged.
    assert!(!state.leases.state_of(idle.lease_id).unwrap().broken);
    state
        .leases
        .get(idle.lease_id)
        .unwrap()
        .ping()
        .await
        .unwrap();
    assert!(state.leases.state_of(in_tx.lease_id).unwrap().broken);
    // `lease_reopen` brings it back and reports the lost transaction.
    let reopened = editor::lease_reopen_impl(&state, in_tx.lease_id)
        .await
        .unwrap();
    assert!(reopened.transaction_lost);
    assert_eq!(reopened.lease.lease_id, in_tx.lease_id);
    assert!(!reopened.lease.state.broken);
    state
        .leases
        .get(in_tx.lease_id)
        .unwrap()
        .ping()
        .await
        .unwrap();
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_keepalive_detects_a_killed_lease() {
    if enabled() {
        keepalive_scenario(pg("dbstudio-it-keepalive")).await;
    }
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_session_reconnects_after_its_connections_are_terminated() {
    if enabled() {
        reconnect_scenario(pg("dbstudio-it-reconnect")).await;
    }
}

#[cfg(feature = "mysql")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mysql_keepalive_detects_a_killed_lease() {
    if enabled() {
        keepalive_scenario(mysql()).await;
    }
}

#[cfg(feature = "mysql")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mysql_session_reconnects_after_its_connections_are_killed() {
    if enabled() {
        reconnect_scenario(mysql()).await;
    }
}
