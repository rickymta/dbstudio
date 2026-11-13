//! JSON contract fixtures for the store types (`contract/fixtures/history.json`), shared with
//! the TypeScript contract test. Regenerate with `UPDATE_FIXTURES=1 cargo test -p dbstudio-store`.

use dbstudio_core::testing::write_or_check;
use dbstudio_core::{DbKind, ObjectKind, ObjectPath};
use dbstudio_store::{
    AppSettings, CloseBehavior, EditorTabState, ErdLayout, ErdLayoutSummary, ExportPreset,
    HistoryEntry, HistoryQuery, SavedFolder, SavedQueriesSnapshot, SavedQuery,
};
use serde::Serialize;
use serde_json::{Value, json};
use uuid::Uuid;

fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

fn j<T: Serialize>(v: &T) -> Value {
    serde_json::to_value(v).unwrap()
}

#[test]
fn store_contract_fixtures() {
    let scope = ObjectPath::root()
        .child(ObjectKind::Database, "app")
        .child(ObjectKind::Schema, "public");
    let folder = SavedFolder {
        id: id(1),
        name: "Reports".into(),
        parent_id: None,
        sort_order: 0,
    };
    let query = SavedQuery {
        id: id(2),
        folder_id: Some(id(1)),
        name: "Active users".into(),
        text: "SELECT * FROM users WHERE active".into(),
        is_snippet: false,
        db_kind: Some(DbKind::Postgres),
        connection_id: Some(id(3)),
        description: Some("daily".into()),
        sort_order: 1,
        created_at: "2024-05-01T10:00:00Z".into(),
        updated_at: "2024-05-02T10:00:00Z".into(),
    };
    let samples = json!({
        "HistoryEntry": j(&HistoryEntry {
            id: 42,
            connection_id: id(3),
            database: Some("app".into()),
            kind: "query".into(),
            text: "SELECT 1".into(),
            text_truncated: false,
            started_at: "2024-05-01T10:00:00Z".into(),
            elapsed_ms: Some(12),
            status: "ok".into(),
            statements: Some(1),
            rows: Some(1),
            error_message: None,
        }),
        "HistoryQuery": j(&HistoryQuery {
            connection_id: Some(id(3)),
            search: Some("users".into()),
            status: Some("error".into()),
            before_id: Some(100),
            limit: 50,
        }),
        "SavedFolder": j(&folder),
        "SavedQuery": j(&query),
        "SavedQueriesSnapshot": j(&SavedQueriesSnapshot {
            folders: vec![folder],
            queries: vec![query],
        }),
        "EditorTabState": j(&EditorTabState {
            id: id(4),
            position: 0,
            title: "Query 1".into(),
            connection_id: Some(id(3)),
            database: Some("app".into()),
            schema: Some("public".into()),
            text: "SELECT 1".into(),
            file_path: Some("/tmp/q.sql".into()),
            cursor: 8,
            auto_commit: true,
        }),
        "ErdLayout": j(&ErdLayout {
            id: id(5),
            connection_id: id(3),
            name: "Main".into(),
            scope: scope.clone(),
            tables: json!([{"path": {"segments": []}, "x": 10.0, "y": 20.0, "collapsed": false}]),
            options: json!({"columns": "all", "showTypes": true, "showComments": false, "notation": "crowsFoot"}),
            viewport: Some(json!({"x": 0.0, "y": 0.0, "zoom": 1.0})),
            snapshot: json!({"public.users": [["id", "integer"]]}),
            created_at: "2024-05-01T10:00:00Z".into(),
            updated_at: "2024-05-02T10:00:00Z".into(),
        }),
        "ErdLayoutSummary": j(&ErdLayoutSummary {
            id: id(5),
            name: "Main".into(),
            scope,
            table_count: 12,
            updated_at: "2024-05-02T10:00:00Z".into(),
        }),
        "ExportPreset": j(&ExportPreset {
            id: id(6),
            name: "Nightly CSV".into(),
            connection_id: Some(id(3)),
            spec: json!({"format": "csv"}),
            created_at: "2024-05-01T10:00:00Z".into(),
            updated_at: "2024-05-02T10:00:00Z".into(),
        }),
        "AppSettings": j(&AppSettings {
            close_to_tray: CloseBehavior::Always,
            ..AppSettings::default()
        }),
    });
    write_or_check("history", &samples);
}
