//! Schema model, ERD layouts and DDL generation (package F).

use dbstudio_core::{DdlItem, DdlRequest, Error, SchemaModel, SchemaModelRequest};
use dbstudio_store::{ErdLayout, ErdLayoutSummary};
use tauri::State;
use uuid::Uuid;

use super::blocking;
use crate::state::AppState;

type Result<T> = std::result::Result<T, Error>;

pub(crate) async fn schema_model_impl(
    state: &AppState,
    session_id: Uuid,
    request: SchemaModelRequest,
) -> Result<SchemaModel> {
    state.sessions.get(session_id)?.schema_model(request).await
}

pub(crate) async fn ddl_generate_impl(
    state: &AppState,
    session_id: Uuid,
    request: DdlRequest,
) -> Result<Vec<DdlItem>> {
    state.sessions.get(session_id)?.generate_ddl(request).await
}

pub(crate) async fn layouts_list_impl(
    state: &AppState,
    connection_id: Uuid,
) -> Result<Vec<ErdLayoutSummary>> {
    let svc = state.connections.clone();
    blocking(move || svc.store().erd_list(connection_id)).await
}

pub(crate) async fn layout_get_impl(state: &AppState, id: Uuid) -> Result<ErdLayout> {
    let svc = state.connections.clone();
    blocking(move || svc.store().erd_get(id)).await
}

pub(crate) async fn layout_save_impl(state: &AppState, layout: ErdLayout) -> Result<ErdLayout> {
    let svc = state.connections.clone();
    blocking(move || svc.store().erd_save(&layout)).await
}

pub(crate) async fn layout_delete_impl(state: &AppState, id: Uuid) -> Result<()> {
    let svc = state.connections.clone();
    blocking(move || svc.store().erd_delete(id)).await
}

#[tauri::command]
pub async fn schema_model(
    state: State<'_, AppState>,
    session_id: Uuid,
    request: SchemaModelRequest,
) -> Result<SchemaModel> {
    tracing::debug!(command = "schema_model", session = %session_id, "ipc call");
    schema_model_impl(&state, session_id, request).await
}

#[tauri::command]
pub async fn ddl_generate(
    state: State<'_, AppState>,
    session_id: Uuid,
    request: DdlRequest,
) -> Result<Vec<DdlItem>> {
    tracing::debug!(command = "ddl_generate", session = %session_id, "ipc call");
    ddl_generate_impl(&state, session_id, request).await
}

#[tauri::command]
pub async fn erd_layouts_list(
    state: State<'_, AppState>,
    connection_id: Uuid,
) -> Result<Vec<ErdLayoutSummary>> {
    tracing::debug!(command = "erd_layouts_list", connection = %connection_id, "ipc call");
    layouts_list_impl(&state, connection_id).await
}

#[tauri::command]
pub async fn erd_layout_get(state: State<'_, AppState>, id: Uuid) -> Result<ErdLayout> {
    tracing::debug!(command = "erd_layout_get", layout = %id, "ipc call");
    layout_get_impl(&state, id).await
}

#[tauri::command]
pub async fn erd_layout_save(state: State<'_, AppState>, layout: ErdLayout) -> Result<ErdLayout> {
    tracing::debug!(command = "erd_layout_save", layout = %layout.id, "ipc call");
    layout_save_impl(&state, layout).await
}

#[tauri::command]
pub async fn erd_layout_delete(state: State<'_, AppState>, id: Uuid) -> Result<()> {
    tracing::debug!(command = "erd_layout_delete", layout = %id, "ipc call");
    layout_delete_impl(&state, id).await
}

#[cfg(test)]
mod tests {
    use dbstudio_core::{ConnectionConfig, DbKind, DdlOptions, ObjectKind, ObjectPath};
    use dbstudio_store::{ConnectionProfile, SecretChange};
    use serde_json::json;

    use super::*;
    use crate::state::{build_registry, testing::test_state};

    fn layout(connection_id: Uuid) -> ErdLayout {
        ErdLayout {
            id: Uuid::new_v4(),
            connection_id,
            name: "Main".into(),
            scope: ObjectPath::root().child(ObjectKind::Database, "app"),
            tables: json!([{"path": {"segments": []}, "x": 1.0, "y": 2.0, "collapsed": false}]),
            options: json!({"columns": "all"}),
            viewport: None,
            snapshot: json!({}),
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[tokio::test]
    async fn layout_commands_roundtrip() {
        let (state, _) = test_state(build_registry());
        let profile = ConnectionProfile {
            config: ConnectionConfig::new(DbKind::Postgres, "pg", "localhost"),
            group_id: None,
            color: None,
            save_password: false,
            has_password: false,
            sort_order: 0,
            created_at: String::new(),
            updated_at: String::new(),
        };
        let saved = crate::commands::connections::save_impl(&state, profile, SecretChange::Keep)
            .await
            .unwrap();
        let cid = saved.config.id;
        let l = layout_save_impl(&state, layout(cid)).await.unwrap();
        assert_eq!(layout_get_impl(&state, l.id).await.unwrap().name, "Main");
        let list = layouts_list_impl(&state, cid).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].table_count, 1);
        // Duplicate name on the same connection is refused.
        assert!(layout_save_impl(&state, layout(cid)).await.is_err());
        layout_delete_impl(&state, l.id).await.unwrap();
        assert!(matches!(
            layout_get_impl(&state, l.id).await,
            Err(Error::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn model_and_ddl_need_a_session() {
        let (state, _) = test_state(build_registry());
        let scope = ObjectPath::root().child(ObjectKind::Database, "app");
        let req = SchemaModelRequest {
            scope: scope.clone(),
            tables: vec![],
            include_views: false,
            include_comments: false,
            detail: Default::default(),
            sample_size: None,
        };
        assert!(
            schema_model_impl(&state, Uuid::new_v4(), req)
                .await
                .is_err()
        );
        let req = DdlRequest {
            scope,
            objects: vec![],
            options: DdlOptions::default(),
        };
        assert!(
            ddl_generate_impl(&state, Uuid::new_v4(), req)
                .await
                .is_err()
        );
    }
}
