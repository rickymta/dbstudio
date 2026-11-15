//! Integration tests of the connection-resilience features against real servers (PostgreSQL 16
//! and MySQL 8.4 from `docker/compose.test.yml`): lease keepalive and automatic reconnect after
//! the server killed the connections. They run only when `DBSTUDIO_IT=1`.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dbstudio_core::{
    ConnectionConfig, DbKind, LeaseOptions, ObjectPath, QueryRequest, Secret, TxOptions, TxStatus,
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

/// A TCP forwarder in front of the real server that can simulate an outage: `sever` closes
/// every connection and refuses new ones, `resume` lets clients in again. (Killing backends is
/// not an outage: the drivers' pools quietly replace a dead connection.)
struct Proxy {
    port: u16,
    target: u16,
    state: std::sync::Arc<std::sync::Mutex<ProxyState>>,
}

#[derive(Default)]
struct ProxyState {
    streams: Vec<std::net::TcpStream>,
    stop: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

impl Proxy {
    fn start(target: u16) -> Proxy {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let proxy = Proxy {
            port,
            target,
            state: Default::default(),
        };
        proxy.serve(listener);
        proxy
    }

    fn serve(&self, listener: std::net::TcpListener) {
        use std::sync::atomic::{AtomicBool, Ordering};
        let stop = Arc::new(AtomicBool::new(false));
        self.state.lock().unwrap().stop = Some(stop.clone());
        let state = self.state.clone();
        let target = self.target;
        listener.set_nonblocking(true).unwrap();
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((client, _)) => {
                        client.set_nonblocking(false).unwrap();
                        let Ok(server) = std::net::TcpStream::connect(("127.0.0.1", target)) else {
                            continue;
                        };
                        {
                            let mut st = state.lock().unwrap();
                            st.streams.push(client.try_clone().unwrap());
                            st.streams.push(server.try_clone().unwrap());
                        }
                        for (mut from, mut to) in [
                            (client.try_clone().unwrap(), server.try_clone().unwrap()),
                            (server, client),
                        ] {
                            std::thread::spawn(move || {
                                let _ = std::io::copy(&mut from, &mut to);
                                let _ = to.shutdown(std::net::Shutdown::Both);
                                let _ = from.shutdown(std::net::Shutdown::Both);
                            });
                        }
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(10)),
                }
            }
        });
    }

    /// Close every connection and refuse new ones.
    fn sever(&self) {
        let mut st = self.state.lock().unwrap();
        if let Some(stop) = st.stop.take() {
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        for stream in st.streams.drain(..) {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    }

    /// Accept connections again on the same port.
    fn resume(&self) {
        // The accept loop notices the stop flag within 10 ms and drops its listener.
        for _ in 0..100 {
            if let Ok(listener) = std::net::TcpListener::bind(("127.0.0.1", self.port)) {
                self.serve(listener);
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("cannot rebind the proxy port");
    }
}

async fn reconnect_scenario(server: Server) {
    let (state, _) = test_state(build_registry());
    let mut rx = state.events.subscribe();
    let proxy = Proxy::start(server.port);
    let through_proxy = Server {
        kind: server.kind,
        port: proxy.port,
        options: server.options.clone(),
    };
    let sid = open(&state, &through_proxy, "victim").await;
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

    // Outage: every connection is cut and the server cannot be reached for a while.
    proxy.sever();
    assert!(state.sessions.get(sid).unwrap().ping().await.is_err());
    let creds: Arc<dyn Credentials> = Arc::new(StateCredentials(state.clone()));
    let lost = state.sessions.health_pass(&creds, true).await;
    assert_eq!(lost, vec![sid], "the health round finds the session lost");
    wait_state(&mut rx, "lost").await;
    // The first attempt (after 1 s) still finds nobody; then the network comes back.
    assert_eq!(wait_state(&mut rx, "reconnecting").await, 1);
    tokio::time::sleep(Duration::from_millis(200)).await;
    proxy.resume();
    let attempt = wait_state(&mut rx, "reconnected").await;
    assert!(attempt >= 2, "reconnected at attempt {attempt}");

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
    proxy.sever();
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
async fn postgres_session_reconnects_after_an_outage() {
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
async fn mysql_session_reconnects_after_an_outage() {
    if enabled() {
        reconnect_scenario(mysql()).await;
    }
}

// -- editing a result inside the tab's transaction -------------------------------------------

async fn lease_scalar(state: &AppState, lease: Uuid, sql: &str) -> String {
    let lease = state.leases.get(lease).unwrap();
    let mut sink = dbstudio_core::CollectingSink::new();
    lease
        .execute(QueryRequest::new(sql), &mut sink)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    format!("{:?}", first_value(&sink.into_results()))
}

fn update(id: i64, text: &str) -> dbstudio_core::ChangeSet {
    use dbstudio_core::{CellEdit, ChangeSet, EditValue, RowChange, RowChangeSet, Value};
    ChangeSet::Rows(RowChangeSet {
        table: ObjectPath::root(), // replaced by the caller
        key_columns: vec!["id".into()],
        changes: vec![RowChange::Update {
            key: vec![Value::Int(id)],
            set: vec![CellEdit {
                column: "a".into(),
                value: EditValue::Text(text.into()),
            }],
        }],
    })
}

fn with_table(mut cs: dbstudio_core::ChangeSet, table: ObjectPath) -> dbstudio_core::ChangeSet {
    if let dbstudio_core::ChangeSet::Rows(r) = &mut cs {
        r.table = table;
    }
    cs
}

async fn lease_edit_scenario(server: Server, table: ObjectPath, name: &str) {
    use crate::commands::changes::apply_impl;
    let (state, _) = test_state(build_registry());
    let sid = open(&state, &server, "result-edit").await;
    let session = state.sessions.get(sid).unwrap();
    execute_collect(
        &*session,
        QueryRequest::new(format!(
            "DROP TABLE IF EXISTS {name}; \
             CREATE TABLE {name} (id int PRIMARY KEY, a varchar(20)); \
             INSERT INTO {name} VALUES (1, 'one'), (2, 'two')"
        )),
    )
    .await
    .unwrap();
    // A manual-commit tab: the edit runs in the lease's own transaction.
    let lease = editor::lease_open_impl(
        &state,
        sid,
        LeaseOptions {
            auto_commit: false,
            ..LeaseOptions::default()
        },
    )
    .await
    .unwrap()
    .lease_id;
    let apply = |cs| apply_impl(&state, sid, with_table(cs, table.clone()), Some(lease));

    let r = apply(update(1, "edited")).await.unwrap();
    assert_eq!(r.applied, 1);
    assert_eq!(
        lease_scalar(&state, lease, &format!("SELECT a FROM {name} WHERE id = 1")).await,
        "Text(\"edited\")"
    );
    // Not committed: another connection still sees the old value.
    assert_eq!(
        scalar(&state, sid, &format!("SELECT a FROM {name} WHERE id = 1")).await,
        "Text(\"one\")"
    );
    assert_eq!(
        state.leases.state_of(lease).unwrap().transaction,
        TxStatus::Active
    );

    // A key that matches nothing is an error (MySQL reports 0 CHANGED rows for a no-op update
    // too, so this needs the matched count), the earlier work in the transaction survives, and
    // the transaction stays usable.
    let err = apply(update(99, "x")).await.unwrap_err();
    assert!(
        matches!(err, dbstudio_core::Error::ChangeFailed { index: 0, .. }),
        "{err:?}"
    );
    assert_eq!(
        lease_scalar(&state, lease, &format!("SELECT a FROM {name} WHERE id = 1")).await,
        "Text(\"edited\")"
    );
    // An update to the value the row already has is not "no row matched".
    apply(update(2, "two")).await.unwrap();

    editor::lease_rollback_impl(&state, lease).await.unwrap();
    assert_eq!(
        scalar(&state, sid, &format!("SELECT a FROM {name} WHERE id = 1")).await,
        "Text(\"one\")"
    );
    execute_collect(&*session, QueryRequest::new(format!("DROP TABLE {name}")))
        .await
        .unwrap();
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_result_edit_runs_in_the_tabs_transaction() {
    use dbstudio_core::ObjectKind as K;
    if enabled() {
        let table = ObjectPath::root()
            .child(K::Database, "dbstudio")
            .child(K::Schema, "public")
            .child(K::Table, "dbs_result_edit_lease");
        lease_edit_scenario(
            pg("dbstudio-it-result-edit"),
            table,
            "dbs_result_edit_lease",
        )
        .await;
    }
}

#[cfg(feature = "mysql")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mysql_result_edit_runs_in_the_tabs_transaction() {
    use dbstudio_core::ObjectKind as K;
    if enabled() {
        let table = ObjectPath::root()
            .child(K::Database, "dbstudio")
            .child(K::Table, "dbs_result_edit_lease");
        lease_edit_scenario(mysql(), table, "dbs_result_edit_lease").await;
    }
}
