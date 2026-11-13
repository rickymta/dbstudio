//! Tests of the phase 2-8 command scaffolding: guarded `query_execute`, stub commands, jobs,
//! file dialogs and path grants, SSH secrets. Mock drivers, in-memory store and keychain.

use std::sync::Arc;

use dbstudio_core::{
    ConnectionConfig, DbKind, DriverRegistry, DriverStatus, Environment, Error, ExecuteRequest,
    JobKind, ObjectPath, QueryEvent, QueryRequest, Secret, SshAuth, SshConfig,
};
use dbstudio_store::{
    CloseBehavior, ConnectionProfile, MemorySecretStore, SecretChange, SecretStore,
    ssh_secret_account,
};
use serde_json::json;
use uuid::Uuid;

use crate::commands::background::BackgroundSettings;
use crate::commands::{
    app, background, backup, changes, connections, editor, export, files, jobs, keyvalue, sessions,
    ssh,
};
use crate::host::FileFilter;
use crate::session::testing::{MockDriver, Probe};
use crate::state::AppState;
use crate::state::testing::{FakeHost, test_state_with_host};

struct Fixture {
    state: AppState,
    secrets: Arc<MemorySecretStore>,
    host: Arc<FakeHost>,
    probe: Arc<Probe>,
}

fn fixture() -> Fixture {
    let (driver, probe) = MockDriver::new(DbKind::Postgres);
    let mut registry = DriverRegistry::new();
    registry.register(driver);
    let (state, secrets, host) = test_state_with_host(registry);
    Fixture {
        state,
        secrets,
        host,
        probe,
    }
}

fn profile(read_only: bool, environment: Option<Environment>) -> ConnectionProfile {
    let mut config = ConnectionConfig::new(DbKind::Postgres, "mock", "localhost");
    config.read_only = read_only;
    config.environment = environment;
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

async fn open(f: &Fixture, p: ConnectionProfile) -> Uuid {
    let saved = connections::save_impl(&f.state, p, SecretChange::Keep)
        .await
        .unwrap();
    sessions::open_impl(&f.state, saved.config.id, None)
        .await
        .unwrap()
        .session_id
}

fn exec_request(text: &str) -> ExecuteRequest {
    serde_json::from_value(json!({"text": text, "queryId": Uuid::new_v4()})).unwrap()
}

fn kinds(events: &[QueryEvent]) -> Vec<&'static str> {
    events
        .iter()
        .map(|e| match e {
            QueryEvent::Columns { .. } => "columns",
            QueryEvent::Rows { .. } => "rows",
            QueryEvent::Message { .. } => "message",
            QueryEvent::StatementEnd { .. } => "end",
            QueryEvent::StatementStart { .. } => "start",
        })
        .collect()
}

#[tokio::test]
async fn script_commands_use_the_session_dialect() {
    let f = fixture();
    let id = open(&f, profile(false, None)).await;
    let analysis = editor::analyze_impl(&f.state, id, "select 1").unwrap();
    assert_eq!(analysis.statements.len(), 1);
    assert_eq!(analysis.dialect, dbstudio_core::ScriptDialect::Opaque);
    assert!(
        editor::vocabulary_impl(&f.state, id)
            .unwrap()
            .keywords
            .is_empty()
    );
    assert!(matches!(
        editor::analyze_impl(&f.state, Uuid::new_v4(), "x"),
        Err(Error::NotFound(_))
    ));
}

#[tokio::test]
async fn query_execute_runs_the_guard_then_streams_the_driver() {
    let f = fixture();
    let id = open(&f, profile(false, None)).await;
    let mut events = Vec::new();
    let result = editor::execute_impl(&f.state, id, exec_request("select 1"), |e| {
        events.push(e);
        true
    })
    .await
    .unwrap();
    assert_eq!(kinds(&events), ["start", "columns", "rows", "end"]);
    assert_eq!(result.summary.statements, 1);
    assert!(result.state.is_none() && result.history_id.is_none());
    assert_eq!(f.probe.executed.lock().unwrap().as_slice(), ["select 1"]);
    // The result serializes the way the frontend expects.
    let json = serde_json::to_value(&result).unwrap();
    assert_eq!(json["state"], json!(null));
    assert_eq!(json["summary"]["statements"], 1);
}

#[tokio::test]
async fn query_execute_refuses_before_the_driver_on_read_only() {
    let f = fixture();
    let id = open(&f, profile(true, None)).await;
    let err = editor::execute_impl(&f.state, id, exec_request("delete from t"), |_| true)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), "readOnly");
    assert!(f.probe.executed.lock().unwrap().is_empty());
    // Production without destructive statements (the scaffold analyzer finds none) just runs.
    let prod = open(&f, profile(false, Some(Environment::Production))).await;
    editor::execute_impl(&f.state, prod, exec_request("select 1"), |_| true)
        .await
        .unwrap();
}

#[tokio::test]
async fn query_execute_passes_driver_errors_and_receiver_stops() {
    let f = fixture();
    let id = open(&f, profile(false, None)).await;
    let err = editor::execute_impl(&f.state, id, exec_request("fail"), |_| true)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        Error::Query {
            position: Some(2),
            ..
        }
    ));
    // Parameters are bound by the real binder before the driver runs.
    let mut with_params = exec_request("select :a");
    with_params.params = vec![dbstudio_core::ParamValue {
        name: ":a".into(),
        value: dbstudio_core::ParamInput::Null,
    }];
    let before = f.probe.executed.lock().unwrap().len();
    editor::execute_impl(&f.state, id, with_params, |_| true)
        .await
        .unwrap();
    assert_eq!(f.probe.executed.lock().unwrap().len(), before + 1);
    // A receiver that is gone stops the driver.
    let err = editor::execute_impl(&f.state, id, exec_request("select 1"), |_| false)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Cancelled));
    assert!(matches!(
        editor::execute_impl(&f.state, Uuid::new_v4(), exec_request("x"), |_| true).await,
        Err(Error::NotFound(_))
    ));
}

#[tokio::test]
async fn lease_commands_are_unsupported_until_a_driver_opens_leases() {
    let f = fixture();
    let id = open(&f, profile(false, None)).await;
    let err = editor::lease_open_impl(
        &f.state,
        id,
        serde_json::from_value(json!({"autoCommit": true})).unwrap(),
    )
    .await
    .unwrap_err();
    assert!(matches!(&err, Error::Unsupported(c) if c == "leases"));
    // Unknown leases are NotFound; closing one is harmless.
    let lease = Uuid::new_v4();
    assert!(matches!(
        editor::lease_state_impl(&f.state, lease),
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        editor::lease_commit_impl(&f.state, lease).await,
        Err(Error::NotFound(_))
    ));
    assert!(
        !editor::lease_close_impl(&f.state, lease)
            .await
            .unwrap()
            .rolled_back
    );
}

#[tokio::test]
async fn stub_commands_answer_unsupported() {
    let f = fixture();
    let id = open(&f, profile(false, None)).await;
    let spec = json!({
        "connectionId": Uuid::nil(), "source": {"kind": "query", "text": "select 1"},
        "content": "data", "format": {"kind": "csv"},
        "destination": {"path": "/tmp/x.csv"}
    });
    let results: Vec<(&str, Error)> = vec![
        (
            "export_start",
            export::start_impl(&f.state, serde_json::from_value(spec).unwrap())
                .await
                .unwrap_err(),
        ),
        (
            "keys_delete_pattern",
            keyvalue::keys_delete_pattern_impl(&f.state, id, ObjectPath::root(), "a*".into(), None)
                .await
                .unwrap_err(),
        ),
        (
            "backup_jobs_list",
            backup::jobs_list_impl(&f.state).await.unwrap_err(),
        ),
        (
            "backup_job_delete",
            backup::job_delete_impl(&f.state, Uuid::new_v4())
                .await
                .unwrap_err(),
        ),
        (
            "schedule_preview",
            backup::schedule_preview_impl("* * * * *".into(), "utc".into(), 3).unwrap_err(),
        ),
        (
            "key_read",
            keyvalue::key_read_impl(
                &f.state,
                id,
                serde_json::from_value(json!({"key": {"segments": []}, "count": 10})).unwrap(),
            )
            .await
            .unwrap_err(),
        ),
        (
            "ddl_generate",
            crate::commands::erd::ddl_generate_impl(
                &f.state,
                id,
                serde_json::from_value(json!({"scope": {"segments": []}})).unwrap(),
            )
            .await
            .unwrap_err(),
        ),
        (
            "saved_queries_list",
            crate::commands::history::saved_queries_list_impl(&f.state)
                .await
                .unwrap_err(),
        ),
        (
            "editor_tabs_get",
            crate::commands::history::editor_tabs_get_impl(&f.state)
                .await
                .unwrap_err(),
        ),
        (
            "erd_layouts_list",
            crate::commands::erd::layouts_list_impl(&f.state, Uuid::new_v4())
                .await
                .unwrap_err(),
        ),
        (
            "export_presets_list",
            export::presets_list_impl(&f.state, None).await.unwrap_err(),
        ),
    ];
    for (name, err) in results {
        assert_eq!(err.kind(), "unsupported", "{name}: {err}");
    }
}

#[tokio::test]
async fn data_editing_checks_read_only_before_the_session() {
    let f = fixture();
    let change_set: dbstudio_core::ChangeSet = serde_json::from_value(json!({
        "model": "rows", "table": {"segments": []}, "keyColumns": [], "changes": []
    }))
    .unwrap();
    let ro = open(&f, profile(true, None)).await;
    let err = changes::apply_impl(&f.state, ro, change_set.clone())
        .await
        .unwrap_err();
    assert_eq!(err.kind(), "readOnly");
    assert_eq!(
        changes::preview_impl(&f.state, ro, change_set.clone())
            .await
            .unwrap_err()
            .kind(),
        "readOnly"
    );
    let rw = open(&f, profile(false, None)).await;
    let err = changes::apply_impl(&f.state, rw, change_set)
        .await
        .unwrap_err();
    assert!(matches!(&err, Error::Unsupported(c) if c == "data_editing"));
}

#[tokio::test]
async fn jobs_commands_use_the_job_manager() {
    let f = fixture();
    assert!(jobs::list_impl(&f.state).is_empty());
    let (id, _) = f.state.jobs.create(JobKind::Export, "Export", None, true);
    f.state.jobs.start(id);
    assert_eq!(jobs::list_impl(&f.state).len(), 1);
    assert_eq!(jobs::get_impl(&f.state, id).unwrap().title, "Export");
    jobs::cancel_impl(&f.state, id).unwrap();
    f.state.jobs.finish(id, Err(Error::Cancelled));
    assert_eq!(jobs::clear_finished_impl(&f.state), 1);
    assert!(matches!(
        jobs::get_impl(&f.state, id),
        Err(Error::NotFound(_))
    ));
    assert_eq!(
        serde_json::to_value(jobs::list_impl(&f.state)).unwrap(),
        json!([])
    );
}

#[tokio::test]
async fn driver_diagnostics_report_the_registry_status() {
    let f = fixture();
    let pg = app::driver_diagnostics_impl(&f.state, DbKind::Postgres).unwrap();
    assert_eq!(pg.status, DriverStatus::Available);
    assert!(pg.help_url.is_none());
    let my = app::driver_diagnostics_impl(&f.state, DbKind::MySql).unwrap();
    assert!(matches!(my.status, DriverStatus::Planned { .. }));
    assert!(!my.message.is_empty());
    let json = serde_json::to_value(&pg).unwrap();
    assert_eq!(json["status"], json!({"state": "available"}));
    assert!(json["helpUrl"].is_null());
}

#[tokio::test]
async fn ssh_trust_host_stores_the_accepted_key() {
    let f = fixture();
    // the backend only trusts a key it has seen during a connection attempt
    ssh::record_observed("bastion", 22, "ssh-ed25519", "SHA256:abc");
    ssh::trust_host_impl(
        &f.state,
        "bastion".into(),
        22,
        "ssh-ed25519".into(),
        "SHA256:abc".into(),
    )
    .await
    .unwrap();
    let known = f
        .state
        .connections
        .store()
        .known_host_get("bastion", 22, "ssh-ed25519")
        .unwrap()
        .unwrap();
    assert_eq!(known.fingerprint, "SHA256:abc");
}

#[tokio::test]
async fn connection_save_stores_the_ssh_secret_under_its_own_account() {
    let f = fixture();
    let mut p = profile(false, None);
    p.config.ssh = Some(SshConfig {
        host: "bastion".into(),
        port: 22,
        username: "me".into(),
        auth: SshAuth::Password,
        keepalive_secs: None,
        connect_timeout_ms: None,
    });
    let id = p.config.id;
    let account = ssh_secret_account(id);
    let set = |v: &str| {
        Some(SecretChange::Set {
            value: Secret::new(v),
        })
    };
    connections::save_with_ssh_impl(&f.state, p.clone(), SecretChange::Keep, set("tunnel"))
        .await
        .unwrap();
    assert_eq!(
        f.secrets.get_account(&account).unwrap().unwrap().expose(),
        "tunnel"
    );
    // Missing or `keep` leaves it unchanged; the connection password account is separate.
    connections::save_with_ssh_impl(&f.state, p.clone(), SecretChange::Keep, None)
        .await
        .unwrap();
    connections::save_with_ssh_impl(
        &f.state,
        p.clone(),
        SecretChange::Keep,
        Some(SecretChange::Keep),
    )
    .await
    .unwrap();
    assert!(f.secrets.get_account(&account).unwrap().is_some());
    assert!(f.secrets.get(id).unwrap().is_none());
    connections::save_with_ssh_impl(
        &f.state,
        p.clone(),
        SecretChange::Keep,
        Some(SecretChange::Clear),
    )
    .await
    .unwrap();
    assert!(f.secrets.get_account(&account).unwrap().is_none());
    // Without an SSH tunnel a secret is not stored.
    p.config.ssh = None;
    connections::save_with_ssh_impl(&f.state, p, SecretChange::Keep, set("ignored"))
        .await
        .unwrap();
    assert!(f.secrets.get_account(&account).unwrap().is_none());
}

#[tokio::test]
async fn file_dialogs_grant_the_paths_they_return() {
    let f = fixture();
    let dir = tempfile::tempdir().unwrap();
    let filters = vec![FileFilter {
        name: "CSV".into(),
        extensions: vec!["csv".into()],
    }];

    // Cancelled dialogs grant nothing.
    assert_eq!(
        files::pick_save_impl(&f.state, "a.csv".into(), filters.clone())
            .await
            .unwrap(),
        None
    );
    let target = dir.path().join("out.csv");
    *f.host.save_to.lock().unwrap() = Some(target.clone());
    let picked = files::pick_save_impl(&f.state, "../a.csv".into(), filters.clone())
        .await
        .unwrap()
        .unwrap();
    assert!(picked.ends_with("out.csv"));
    assert_eq!(
        f.host.suggested.lock().unwrap().as_slice(),
        ["a.csv", "a.csv"]
    );
    assert_eq!(f.host.save_filters.lock().unwrap()[1], filters);
    f.state
        .grants
        .check(&target, crate::grants::Purpose::Write)
        .unwrap();

    // Open: file (read grant) and directory (dir grant).
    let file = dir.path().join("in.csv");
    std::fs::write(&file, "a,b").unwrap();
    *f.host.open_from.lock().unwrap() = Some(file.clone());
    files::pick_open_impl(&f.state, filters.clone(), false)
        .await
        .unwrap()
        .unwrap();
    f.state
        .grants
        .check(&file, crate::grants::Purpose::Read)
        .unwrap();
    *f.host.open_from.lock().unwrap() = Some(dir.path().to_path_buf());
    files::pick_open_impl(&f.state, vec![], true)
        .await
        .unwrap()
        .unwrap();
    assert!(f.host.open_requests.lock().unwrap()[1].1);
    f.state
        .grants
        .check(&dir.path().join("x.sql"), crate::grants::Purpose::Write)
        .unwrap();

    // Saving bytes writes through the dialog path.
    let bytes_target = dir.path().join("erd.png");
    *f.host.save_to.lock().unwrap() = Some(bytes_target.clone());
    files::save_bytes_impl(&f.state, "erd.png".into(), "aGk=".into(), vec![])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(std::fs::read(&bytes_target).unwrap(), b"hi");
    assert!(
        files::save_bytes_impl(&f.state, "x".into(), "!!".into(), vec![])
            .await
            .is_err()
    );
}

#[tokio::test]
async fn sql_files_go_through_dialogs_and_grants() {
    let f = fixture();
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(editor::sql_file_open_impl(&f.state).await.unwrap(), None);

    let file = dir.path().join("q.sql");
    std::fs::write(&file, b"\xEF\xBB\xBFselect 1;").unwrap();
    *f.host.open_from.lock().unwrap() = Some(file.clone());
    let opened = editor::sql_file_open_impl(&f.state).await.unwrap().unwrap();
    assert_eq!(opened.text, "select 1;");
    // The opened path may be saved back without another dialog.
    let saved = editor::sql_file_save_impl(&f.state, Some(opened.path.clone()), "select 2;".into())
        .await
        .unwrap();
    assert_eq!(saved.as_deref(), Some(opened.path.as_str()));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "select 2;");

    // A path nobody granted is refused.
    let other = dir.path().join("other.sql");
    let err = editor::sql_file_save_impl(
        &f.state,
        Some(other.to_string_lossy().into_owned()),
        "x".into(),
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind(), "permission");
    assert!(!other.exists());

    // Without a path the Save dialog decides.
    *f.host.save_to.lock().unwrap() = Some(other.clone());
    editor::sql_file_save_impl(&f.state, None, "x".into())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(std::fs::read_to_string(&other).unwrap(), "x");

    // Not UTF-8: refused.
    let binary = dir.path().join("b.sql");
    std::fs::write(&binary, [0xff, 0xfe, 0x00, 0x61]).unwrap();
    *f.host.open_from.lock().unwrap() = Some(binary);
    assert!(editor::sql_file_open_impl(&f.state).await.is_err());
}

#[tokio::test]
async fn path_taking_commands_require_a_grant() {
    let f = fixture();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("data.csv");
    std::fs::write(&file, "a").unwrap();
    let path = file.to_string_lossy().into_owned();
    let preview = || -> dbstudio_export::ImportPreviewRequest {
        serde_json::from_value(json!({
            "file": path, "format": {"kind": "sql", "continueOnError": false}
        }))
        .unwrap()
    };
    assert_eq!(
        export::import_preview_impl(&f.state, preview())
            .await
            .unwrap_err()
            .kind(),
        "permission"
    );
    assert_eq!(
        backup::inspect_impl(&f.state, path.clone(), None)
            .await
            .unwrap_err()
            .kind(),
        "permission"
    );
    f.state
        .grants
        .grant(&file, crate::grants::Purpose::Read)
        .unwrap();
    // Granted: the (scaffold) engines answer.
    assert_eq!(
        export::import_preview_impl(&f.state, preview())
            .await
            .unwrap_err()
            .kind(),
        "unsupported"
    );
    assert_eq!(
        backup::inspect_impl(&f.state, path, None)
            .await
            .unwrap_err()
            .kind(),
        "unsupported"
    );
}

#[tokio::test]
async fn background_settings_persist_close_behavior() {
    let f = fixture();
    let initial = background::get_impl(&f.state).await.unwrap();
    assert_eq!(initial.close_to_tray, CloseBehavior::Auto);
    assert!(!initial.autostart);
    let stored = background::set_impl(
        &f.state,
        BackgroundSettings {
            close_to_tray: CloseBehavior::Always,
            autostart: true,
        },
    )
    .await
    .unwrap();
    assert_eq!(stored.close_to_tray, CloseBehavior::Always);
    assert_eq!(
        background::get_impl(&f.state).await.unwrap().close_to_tray,
        CloseBehavior::Always
    );
    let json = serde_json::to_value(stored).unwrap();
    assert_eq!(json["closeToTray"], "always");
}

#[tokio::test]
async fn paged_children_work_through_the_fallback() {
    let f = fixture();
    let id = open(&f, profile(false, None)).await;
    let page = keyvalue::children_page_impl(
        &f.state,
        id,
        serde_json::from_value(json!({"parent": {"segments": []}, "limit": 10})).unwrap(),
    )
    .await
    .unwrap();
    assert!(page.nodes.is_empty() && page.next_cursor.is_none());
}

#[tokio::test]
async fn native_tools_detect_and_set_path_need_a_dialog_path() {
    let f = fixture();
    let tools = backup::tools_detect_impl(&f.state).await.unwrap();
    assert!(!tools.is_empty());
    let err = backup::tool_set_path_impl(
        &f.state,
        dbstudio_backup::NativeTool::PgDump,
        Some("/usr/bin/pg_dump".into()),
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind(), "permission");
    // Clearing needs no path.
    let info = backup::tool_set_path_impl(&f.state, dbstudio_backup::NativeTool::PgDump, None)
        .await
        .unwrap();
    assert_eq!(info.tool, dbstudio_backup::NativeTool::PgDump);
}

#[test]
fn query_request_is_flattened_in_execute_request() {
    let q = QueryRequest::new("select 1");
    let req: ExecuteRequest = serde_json::from_value(json!({
        "text": q.text, "queryId": q.query_id, "confirm": {"token": "t", "typed": "db"}
    }))
    .unwrap();
    assert!(req.record_history);
    assert_eq!(req.confirm.unwrap().typed, "db");
}
