//! Commands managing saved connection profiles and groups.

use dbstudio_core::{Error, ServerInfo};
use dbstudio_store::{ConnectionGroup, ConnectionProfile, ConnectionsSnapshot, SecretChange};
use uuid::Uuid;

type Result<T> = std::result::Result<T, Error>;

fn not_implemented<T>() -> Result<T> {
    Err(Error::Unsupported("not implemented".into()))
}

/// All groups and profiles.
#[tauri::command]
pub async fn connections_list() -> Result<ConnectionsSnapshot> {
    not_implemented()
}

/// Insert or update (by id) a profile and apply the secret change.
#[tauri::command]
pub async fn connection_save(
    profile: ConnectionProfile,
    secret: SecretChange,
) -> Result<ConnectionProfile> {
    let _ = (profile, secret);
    not_implemented()
}

/// Delete a profile and its stored secret.
#[tauri::command]
pub async fn connection_delete(id: Uuid) -> Result<()> {
    let _ = id;
    not_implemented()
}

/// Copy a profile (without secret) under a new id.
#[tauri::command]
pub async fn connection_duplicate(id: Uuid) -> Result<ConnectionProfile> {
    let _ = id;
    not_implemented()
}

/// Insert or update (by id) a group.
#[tauri::command]
pub async fn group_save(group: ConnectionGroup) -> Result<ConnectionGroup> {
    let _ = group;
    not_implemented()
}

/// Delete a group; its profiles and subgroups move to the parent.
#[tauri::command]
pub async fn group_delete(id: Uuid) -> Result<()> {
    let _ = id;
    not_implemented()
}

/// Try to connect with an unsaved profile and report the server info.
#[tauri::command]
pub async fn connection_test(
    profile: ConnectionProfile,
    secret: SecretChange,
) -> Result<ServerInfo> {
    let _ = (profile, secret);
    not_implemented()
}

/// Export profiles (all when `ids` is `None`) as JSON without secrets; returns the count.
#[tauri::command]
pub async fn connections_export(path: String, ids: Option<Vec<Uuid>>) -> Result<u32> {
    let _ = (path, ids);
    not_implemented()
}

/// Import profiles from a JSON file and return the updated snapshot.
#[tauri::command]
pub async fn connections_import(path: String) -> Result<ConnectionsSnapshot> {
    let _ = path;
    not_implemented()
}
