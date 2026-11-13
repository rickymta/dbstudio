//! Phase 1 acceptance check: after saving, testing, opening, exporting and logging at `trace`
//! level, no password may appear anywhere on disk (SQLite file, WAL/SHM, log files, exports).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use dbstudio_core::{ConnectionConfig, DbKind, DriverRegistry, Secret};
use dbstudio_store::{
    ConnectionProfile, ConnectionService, LogLevel, MemorySecretStore, SecretChange, Store,
};
use uuid::Uuid;

use crate::commands::{app, connections, sessions};
use crate::logging;
use crate::session::testing::MockDriver;
use crate::state::AppState;
use crate::state::testing::FakeHost;

fn profile(kind: DbKind, save_password: bool) -> ConnectionProfile {
    let mut config = ConnectionConfig::new(kind, "leak-check", "127.0.0.1");
    config.username = Some("dbstudio".into());
    config.database = Some("dbstudio".into());
    config.connect_timeout_ms = Some(5000);
    ConnectionProfile {
        config,
        group_id: None,
        color: None,
        save_password,
        has_password: false,
        sort_order: 0,
        created_at: String::new(),
        updated_at: String::new(),
    }
}

fn state_in(root: &Path, registry: DriverRegistry) -> AppState {
    let store = Store::open(root.join("data/dbstudio.sqlite")).unwrap();
    let service = ConnectionService::new(store, Arc::new(MemorySecretStore::new()));
    let host = Arc::new(FakeHost {
        log_dir: root.join("logs"),
        ..Default::default()
    });
    AppState::new(registry, service, host)
}

fn utf16le(text: &str) -> Vec<u8> {
    text.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files_under(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// Fail when any file under `root` contains one of `needles` as UTF-8 or UTF-16LE.
fn assert_clean(root: &Path, needles: &[&str]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    files_under(root, &mut files);
    for file in &files {
        let bytes = std::fs::read(file).unwrap();
        for needle in needles {
            assert!(
                !contains(&bytes, needle.as_bytes()) && !contains(&bytes, &utf16le(needle)),
                "password found in {}",
                file.display()
            );
        }
    }
    files
}

fn password() -> String {
    format!(
        "Zq{}-Xw{}",
        Uuid::new_v4().simple(),
        Uuid::new_v4().simple()
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_password_is_written_to_disk() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("data")).unwrap();
    logging::init(Some(&root.path().join("logs")), false);
    logging::set_level(LogLevel::Trace);
    let secret = password();
    #[allow(unused_mut)]
    let mut needles = Vec::from([secret.clone()]);

    // A password echoed inside a connection string must be masked by the log writer.
    tracing::error!("driver said: postgres://dbstudio:{secret}@127.0.0.1/db failed");
    tracing::warn!("config dump password={secret} host=x");

    let (driver, _) = MockDriver::new(DbKind::Postgres);
    let mut registry = DriverRegistry::new();
    registry.register(driver);
    let state = state_in(root.path(), registry);
    let set = || SecretChange::Set {
        value: Secret::new(secret.clone()),
    };

    let saved = connections::save_impl(&state, profile(DbKind::Postgres, true), set())
        .await
        .unwrap();
    assert!(saved.has_password);
    connections::test_impl(&state, saved.clone(), set())
        .await
        .unwrap();
    connections::test_impl(&state, saved.clone(), SecretChange::Keep)
        .await
        .unwrap();
    let stored = sessions::open_impl(&state, saved.config.id, None)
        .await
        .unwrap();
    let ask = connections::save_impl(&state, profile(DbKind::Postgres, false), SecretChange::Keep)
        .await
        .unwrap();
    let typed = sessions::open_impl(&state, ask.config.id, Some(Secret::new(secret.clone())))
        .await
        .unwrap();
    let copy = connections::duplicate_impl(&state, saved.config.id)
        .await
        .unwrap();
    assert!(copy.has_password);
    let export = root.path().join("data/export.json");
    connections::export_impl(&state, export.to_string_lossy().into_owned(), None)
        .await
        .unwrap();
    connections::import_impl(&state, export.to_string_lossy().into_owned())
        .await
        .unwrap();
    app::settings_set_impl(&state, Default::default())
        .await
        .unwrap();

    // Real servers: successful and failing logins must not leak either.
    #[cfg(any(feature = "postgres", feature = "mysql"))]
    let real = if std::env::var("DBSTUDIO_IT").as_deref() == Ok("1") {
        needles.push("dbstudio_test".to_string());
        Some(real_servers(root.path(), &secret).await)
    } else {
        None
    };

    // Scan while everything is still open (the WAL file exists), then again after closing.
    let open_files = assert_clean(
        root.path(),
        &needles.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    assert!(open_files.iter().any(|f| f.ends_with("dbstudio.sqlite")));
    sessions::close_impl(&state, stored.session_id)
        .await
        .unwrap();
    sessions::close_impl(&state, typed.session_id)
        .await
        .unwrap();
    state.sessions.close_all().await;
    drop(state);
    #[cfg(any(feature = "postgres", feature = "mysql"))]
    drop(real);
    logging::shutdown();

    let files = assert_clean(
        root.path(),
        &needles.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    let names: Vec<String> = files
        .iter()
        .map(|f| f.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert!(names.contains(&"dbstudio.sqlite".to_string()), "{names:?}");
    assert!(names.contains(&"export.json".to_string()));
    let log_file = files
        .iter()
        .find(|f| {
            f.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("dbstudio.")
                && f.extension().is_some_and(|e| e == "log")
        })
        .expect("a log file was written");
    let log = std::fs::read_to_string(log_file).unwrap();
    // The log really captured this run (so the scan above means something) and masked the
    // password-bearing lines.
    assert!(
        log.contains("session opened"),
        "log is empty or logging not active"
    );
    assert!(
        log.contains("postgres://dbstudio:***@127.0.0.1/db"),
        "{log}"
    );
    assert!(log.contains("password=*** host=x"));
}

#[cfg(any(feature = "postgres", feature = "mysql"))]
async fn real_servers(root: &Path, wrong: &str) -> AppState {
    let state = state_in(root, crate::state::build_registry());
    let targets: Vec<(DbKind, u16)> = vec![
        #[cfg(feature = "postgres")]
        (DbKind::Postgres, 54316),
        #[cfg(feature = "mysql")]
        (DbKind::MySql, 33068),
    ];
    for (kind, port) in targets {
        let mut p = profile(kind, true);
        p.config.port = port;
        let right = || SecretChange::Set {
            value: Secret::new("dbstudio_test"),
        };
        let saved = connections::save_impl(&state, p.clone(), right())
            .await
            .unwrap();
        connections::test_impl(&state, saved.clone(), right())
            .await
            .expect("real connection_test");
        let session = sessions::open_impl(&state, saved.config.id, None)
            .await
            .expect("real session_open");
        sessions::close_impl(&state, session.session_id)
            .await
            .unwrap();
        // A wrong password takes the driver's authentication-error path.
        let err = connections::test_impl(
            &state,
            saved,
            SecretChange::Set {
                value: Secret::new(wrong),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(err.kind(), "authentication", "{err}");
    }
    state
}
