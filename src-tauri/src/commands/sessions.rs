//! Commands managing live sessions and the object tree.

use dbstudio_core::{DriverDescriptor, Error, ObjectNode, ObjectPath, Secret, ServerInfo};
use serde::Serialize;
use uuid::Uuid;

type Result<T> = std::result::Result<T, Error>;

fn not_implemented<T>() -> Result<T> {
    Err(Error::Unsupported("not implemented".into()))
}

/// Returned by [`session_open`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    /// Identifier of the new session.
    pub session_id: Uuid,
    /// Profile the session was opened from.
    pub connection_id: Uuid,
    /// Server product and version.
    pub server_info: ServerInfo,
    /// Descriptor (capabilities) of the driver in use.
    pub descriptor: DriverDescriptor,
}

/// Open a session for a saved profile. `password` is only used when the profile does not store
/// its password.
#[tauri::command]
pub async fn session_open(connection_id: Uuid, password: Option<Secret>) -> Result<SessionInfo> {
    let _ = (connection_id, password);
    not_implemented()
}

/// Close a session.
#[tauri::command]
pub async fn session_close(session_id: Uuid) -> Result<()> {
    let _ = session_id;
    not_implemented()
}

/// Children of a node in the object tree.
#[tauri::command]
pub async fn object_children(session_id: Uuid, path: ObjectPath) -> Result<Vec<ObjectNode>> {
    let _ = (session_id, path);
    not_implemented()
}

/// DDL of an object.
#[tauri::command]
pub async fn object_ddl(session_id: Uuid, path: ObjectPath) -> Result<String> {
    let _ = (session_id, path);
    not_implemented()
}

/// Cancel a running query or table fetch.
#[tauri::command]
pub async fn query_cancel(session_id: Uuid, query_id: Uuid) -> Result<()> {
    let _ = (session_id, query_id);
    not_implemented()
}
