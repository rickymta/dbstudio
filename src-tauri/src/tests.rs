//! Command-level tests: the plain `*_impl` functions run against mock drivers, an in-memory
//! database and an in-memory secret store (never the real keychain).

use std::sync::Arc;
use std::sync::atomic::Ordering;

use dbstudio_core::{
    ConnectionConfig, DbKind, DriverRegistry, EndOutcome, Error, ObjectKind, ObjectPath,
    PathSegment, QueryEvent, Secret, TableCountRequest, TableDataRequest,
};
use dbstudio_store::{ConnectionProfile, MemorySecretStore, SecretChange};
use uuid::Uuid;

use crate::commands::{browse, connections, sessions};
use crate::session::testing::{MockDriver, Probe};
use crate::state::AppState;
use crate::state::testing::test_state;

struct Fixture {
    state: AppState,
    secrets: Arc<MemorySecretStore>,
    probe: Arc<Probe>,
}

fn fixture_with(require_password: bool) -> Fixture {
    let (mut driver, probe) = MockDriver::new(DbKind::Postgres);
    Arc::get_mut(&mut driver).unwrap().require_password = require_password;
    let mut registry = DriverRegistry::new();
    registry.register(driver);
    let (state, secrets) = test_state(registry);
    Fixture {
        state,
        secrets,
        probe,
    }
}

fn fixture() -> Fixture {
    fixture_with(false)
}

fn profile(save_password: bool) -> ConnectionProfile {
    ConnectionProfile {
        config: ConnectionConfig::new(DbKind::Postgres, "mock", "localhost"),
        group_id: None,
        color: None,
        save_password,
        has_password: false,
        sort_order: 0,
        created_at: String::new(),
        updated_at: String::new(),
    }
}

fn set(v: &str) -> SecretChange {
    SecretChange::Set {
        value: Secret::new(v),
    }
}

fn last_secret(probe: &Probe) -> Option<String> {
    probe.last_secret.lock().unwrap().clone().unwrap()
}

fn table() -> ObjectPath {
    ObjectPath {
        segments: vec![PathSegment {
            kind: ObjectKind::Table,
            name: "t".into(),
        }],
    }
}

fn fetch_request() -> TableDataRequest {
    TableDataRequest {
        query_id: Uuid::new_v4(),
        table: table(),
        filters: vec![],
        raw_where: None,
        sort: vec![],
        offset: 0,
        limit: 100,
        batch_size: None,
    }
}

#[tokio::test]
async fn connection_test_uses_the_secret_change() {
    let f = fixture();
    let saved = connections::save_impl(&f.state, profile(true), set("stored"))
        .await
        .unwrap();

    let info = connections::test_impl(&f.state, saved.clone(), set("typed"))
        .await
        .unwrap();
    assert_eq!(info.product, "MockDB");
    assert_eq!(last_secret(&f.probe).as_deref(), Some("typed"));

    connections::test_impl(&f.state, saved.clone(), SecretChange::Keep)
        .await
        .unwrap();
    assert_eq!(last_secret(&f.probe).as_deref(), Some("stored"));

    connections::test_impl(&f.state, saved, SecretChange::Clear)
        .await
        .unwrap();
    assert_eq!(last_secret(&f.probe), None);

    // Testing never leaves a session behind and closes the probe connection each time.
    assert!(f.state.sessions.is_empty());
    assert_eq!(f.probe.closes.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn connection_test_works_for_unsaved_profile_and_reports_driver_errors() {
    let f = fixture_with(true);
    let unsaved = profile(true);
    let err = connections::test_impl(&f.state, unsaved.clone(), SecretChange::Keep)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Authentication(_)));
    connections::test_impl(&f.state, unsaved.clone(), set("pw"))
        .await
        .unwrap();
    // Nothing was persisted by testing.
    assert!(
        connections::list_impl(&f.state)
            .await
            .unwrap()
            .profiles
            .is_empty()
    );
    assert!(f.secrets.is_empty());
}

#[tokio::test]
async fn session_open_with_stored_password() {
    let f = fixture_with(true);
    let saved = connections::save_impl(&f.state, profile(true), set("stored"))
        .await
        .unwrap();
    // A password passed in is ignored when one is stored.
    let info = sessions::open_impl(&f.state, saved.config.id, Some(Secret::new("other")))
        .await
        .unwrap();
    assert_eq!(last_secret(&f.probe).as_deref(), Some("stored"));
    assert_eq!(info.connection_id, saved.config.id);
    assert_eq!(info.server_info.version, "1.2.3");
    assert_eq!(info.descriptor.kind, DbKind::Postgres);
    assert_eq!(f.state.sessions.len(), 1);
}

#[tokio::test]
async fn session_open_with_supplied_password() {
    let f = fixture_with(true);
    let saved = connections::save_impl(&f.state, profile(false), SecretChange::Keep)
        .await
        .unwrap();
    let err = sessions::open_impl(&f.state, saved.config.id, None)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Authentication(_)));
    assert!(f.state.sessions.is_empty());

    sessions::open_impl(&f.state, saved.config.id, Some(Secret::new("typed")))
        .await
        .unwrap();
    assert_eq!(last_secret(&f.probe).as_deref(), Some("typed"));
    // The typed password was not persisted.
    assert!(f.secrets.is_empty());
}

#[tokio::test]
async fn session_open_unknown_connection_and_unsupported_kind() {
    let f = fixture();
    assert!(matches!(
        sessions::open_impl(&f.state, Uuid::new_v4(), None).await,
        Err(Error::NotFound(_))
    ));
    let mut redis = profile(false);
    redis.config.kind = DbKind::Redis;
    let redis = connections::save_impl(&f.state, redis, SecretChange::Keep)
        .await
        .unwrap();
    assert!(matches!(
        sessions::open_impl(&f.state, redis.config.id, None).await,
        Err(Error::Unsupported(_))
    ));
}

#[tokio::test]
async fn session_open_survives_keychain_failure_when_password_supplied() {
    let f = fixture();
    let saved = connections::save_impl(&f.state, profile(true), set("stored"))
        .await
        .unwrap();
    f.secrets.set_unavailable(true);
    assert!(matches!(
        sessions::open_impl(&f.state, saved.config.id, None).await,
        Err(Error::Storage(_))
    ));
    sessions::open_impl(&f.state, saved.config.id, Some(Secret::new("typed")))
        .await
        .unwrap();
    assert_eq!(last_secret(&f.probe).as_deref(), Some("typed"));
    // Profiles stay editable and listable while the keychain is down.
    let list = connections::list_impl(&f.state).await.unwrap();
    assert_eq!(list.profiles.len(), 1);
    let mut edit = saved;
    edit.config.name = "renamed".into();
    connections::save_impl(&f.state, edit.clone(), SecretChange::Keep)
        .await
        .unwrap();
    assert!(matches!(
        connections::save_impl(&f.state, edit, set("new")).await,
        Err(Error::Storage(m)) if m.contains("connection saved")
    ));
}

#[tokio::test]
async fn deleting_a_connection_closes_its_sessions_and_secret() {
    let f = fixture();
    let a = connections::save_impl(&f.state, profile(true), set("pw"))
        .await
        .unwrap();
    let b = connections::save_impl(&f.state, profile(false), SecretChange::Keep)
        .await
        .unwrap();
    let a1 = sessions::open_impl(&f.state, a.config.id, None)
        .await
        .unwrap();
    let a2 = sessions::open_impl(&f.state, a.config.id, None)
        .await
        .unwrap();
    let b1 = sessions::open_impl(&f.state, b.config.id, None)
        .await
        .unwrap();

    connections::delete_impl(&f.state, a.config.id)
        .await
        .unwrap();
    assert!(f.secrets.is_empty());
    assert!(f.state.sessions.get(a1.session_id).is_err());
    assert!(f.state.sessions.get(a2.session_id).is_err());
    assert!(f.state.sessions.get(b1.session_id).is_ok());
    assert!(matches!(
        sessions::children_impl(&f.state, a1.session_id, ObjectPath::default()).await,
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        connections::delete_impl(&f.state, a.config.id).await,
        Err(Error::NotFound(_))
    ));

    sessions::close_impl(&f.state, b1.session_id).await.unwrap();
    assert!(f.state.sessions.is_empty());
}

#[tokio::test]
async fn session_passthrough_commands() {
    let f = fixture();
    let p = connections::save_impl(&f.state, profile(false), SecretChange::Keep)
        .await
        .unwrap();
    let s = sessions::open_impl(&f.state, p.config.id, None)
        .await
        .unwrap();
    let id = s.session_id;
    assert!(
        sessions::children_impl(&f.state, id, ObjectPath::default())
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        sessions::ddl_impl(&f.state, id, table()).await.unwrap(),
        "CREATE TABLE t ();"
    );
    sessions::cancel_impl(&f.state, id, Uuid::new_v4())
        .await
        .unwrap();
    let count = browse::count_impl(
        &f.state,
        id,
        TableCountRequest {
            table: table(),
            filters: vec![],
            raw_where: None,
            query_id: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(count, 42);
    // Driver default for table_details is Unsupported; it is passed through unchanged.
    assert!(matches!(
        browse::details_impl(&f.state, id, table()).await,
        Err(Error::Unsupported(_))
    ));
}

#[tokio::test]
async fn table_fetch_streams_events_in_order() {
    let f = fixture();
    let p = connections::save_impl(&f.state, profile(false), SecretChange::Keep)
        .await
        .unwrap();
    let s = sessions::open_impl(&f.state, p.config.id, None)
        .await
        .unwrap();

    let mut events = Vec::new();
    let summary = browse::fetch_impl(&f.state, s.session_id, fetch_request(), |e| {
        events.push(e);
        true
    })
    .await
    .unwrap();
    assert_eq!(summary.statements, 1);
    let kinds: Vec<&str> = events
        .iter()
        .map(|e| match e {
            QueryEvent::Columns { .. } => "columns",
            QueryEvent::Rows { .. } => "rows",
            QueryEvent::Message { .. } => "message",
            QueryEvent::StatementEnd { .. } => "end",
            QueryEvent::StatementStart { .. } => "start",
        })
        .collect();
    assert_eq!(kinds, ["columns", "rows", "rows", "rows", "message", "end"]);
    assert!(matches!(
        events.last(),
        Some(QueryEvent::StatementEnd { end }) if matches!(end.outcome, EndOutcome::Rows { row_count: 6, .. })
    ));
    // Events serialize with the tag the frontend expects.
    assert_eq!(serde_json::to_value(&events[0]).unwrap()["type"], "columns");

    assert!(matches!(
        browse::fetch_impl(&f.state, Uuid::new_v4(), fetch_request(), |_| true).await,
        Err(Error::NotFound(_))
    ));
}

#[tokio::test]
async fn table_fetch_stops_with_cancelled_when_the_channel_fails() {
    let f = fixture();
    let p = connections::save_impl(&f.state, profile(false), SecretChange::Keep)
        .await
        .unwrap();
    let s = sessions::open_impl(&f.state, p.config.id, None)
        .await
        .unwrap();

    let mut delivered = 0;
    let result = browse::fetch_impl(&f.state, s.session_id, fetch_request(), |_| {
        delivered += 1;
        delivered < 2 // columns succeed, first rows batch fails
    })
    .await;
    assert!(matches!(result, Err(Error::Cancelled)));
    assert_eq!(delivered, 2, "no events are pushed after the failure");
}

#[test]
fn debug_output_of_command_arguments_hides_passwords() {
    let change = set("hunter2");
    let password = Some(Secret::new("hunter2"));
    let text = format!("{change:?} {password:?} {:?}", (&change, &password));
    assert!(!text.contains("hunter2"), "{text}");
    let json = serde_json::json!({"action": "set", "value": "hunter2"});
    let parsed: SecretChange = serde_json::from_value(json).unwrap();
    assert!(!format!("{parsed:#?}").contains("hunter2"));
    // A profile never carries a password field.
    let json = serde_json::to_string(&profile(true)).unwrap();
    assert!(!json.to_lowercase().contains("password\":\""));
}

#[tokio::test]
async fn export_import_commands_roundtrip_without_secrets() {
    let f = fixture();
    connections::save_impl(&f.state, profile(true), set("hunter2"))
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.json").to_string_lossy().into_owned();
    assert_eq!(
        connections::export_impl(&f.state, path.clone(), None)
            .await
            .unwrap(),
        1
    );
    assert!(!std::fs::read_to_string(&path).unwrap().contains("hunter2"));
    let snap = connections::import_impl(&f.state, path).await.unwrap();
    assert_eq!(snap.profiles.len(), 2);
    assert_eq!(snap.profiles.iter().filter(|p| p.has_password).count(), 1);
}

#[tokio::test]
async fn duplicate_and_groups_commands() {
    let f = fixture();
    let g = connections::group_save_impl(
        &f.state,
        dbstudio_store::ConnectionGroup {
            id: Uuid::new_v4(),
            name: "G".into(),
            parent_id: None,
            sort_order: 0,
        },
    )
    .await
    .unwrap();
    let mut p = profile(true);
    p.group_id = Some(g.id);
    let p = connections::save_impl(&f.state, p, set("pw"))
        .await
        .unwrap();
    let copy = connections::duplicate_impl(&f.state, p.config.id)
        .await
        .unwrap();
    assert!(copy.has_password && copy.group_id == Some(g.id));
    connections::group_delete_impl(&f.state, g.id)
        .await
        .unwrap();
    let snap = connections::list_impl(&f.state).await.unwrap();
    assert!(snap.groups.is_empty());
    assert!(snap.profiles.iter().all(|p| p.group_id.is_none()));
}

#[tokio::test]
async fn import_command_strips_unsafe_options_from_shared_files() {
    let f = fixture();
    let mut p = profile(false);
    p.config
        .options
        .insert("init_command".into(), "DROP DATABASE prod".into());
    p.config
        .options
        .insert("application_name".into(), "dbs".into());
    let doc = serde_json::json!({
        "format": "dbstudio-connections", "version": 1, "groups": [], "profiles": [p]
    });
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shared.json");
    std::fs::write(&path, doc.to_string()).unwrap();
    let snap = connections::import_impl(&f.state, path.to_string_lossy().into_owned())
        .await
        .unwrap();
    let options = &snap.profiles[0].config.options;
    assert!(!options.contains_key("init_command"));
    assert_eq!(
        options.get("application_name").map(String::as_str),
        Some("dbs")
    );
    // Export refuses non-.json targets.
    let bad = dir.path().join("out.txt").to_string_lossy().into_owned();
    assert!(connections::export_impl(&f.state, bad, None).await.is_err());
}
