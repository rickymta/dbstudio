//! Commands managing saved connection profiles and groups.

use dbstudio_core::{Error, Secret, ServerInfo};
use dbstudio_store::{
    ConnectionGroup, ConnectionProfile, ConnectionsSnapshot, SecretChange, ssh_secret_account,
};
use tauri::State;
use uuid::Uuid;

use super::blocking;
use crate::state::AppState;

type Result<T> = std::result::Result<T, Error>;

pub(crate) async fn list_impl(state: &AppState) -> Result<ConnectionsSnapshot> {
    let svc = state.connections.clone();
    blocking(move || svc.snapshot()).await
}

#[cfg(test)]
pub(crate) async fn save_impl(
    state: &AppState,
    profile: ConnectionProfile,
    secret: SecretChange,
) -> Result<ConnectionProfile> {
    save_with_ssh_impl(state, profile, secret, None).await
}

/// Save a profile; `ssh_secret` (SSH password / key passphrase) goes to the keychain account
/// `<connId>#ssh`: `Set` stores it (only while the profile has an SSH tunnel), `Clear` removes
/// it, `None` / `Keep` leave it unchanged.
pub(crate) async fn save_with_ssh_impl(
    state: &AppState,
    profile: ConnectionProfile,
    secret: SecretChange,
    ssh_secret: Option<SecretChange>,
) -> Result<ConnectionProfile> {
    let svc = state.connections.clone();
    let secrets = state.secrets.clone();
    blocking(move || {
        let saved = svc.save(profile, secret)?;
        let account = ssh_secret_account(saved.config.id);
        match ssh_secret {
            Some(SecretChange::Set { value }) if saved.config.ssh.is_some() => {
                secrets.set_account(&account, &value)?;
            }
            Some(SecretChange::Clear) => secrets.delete_account(&account)?,
            _ => {}
        }
        Ok(saved)
    })
    .await
}

/// Delete the profile (and its secret) and close its open sessions.
pub(crate) async fn delete_impl(state: &AppState, id: Uuid) -> Result<()> {
    let svc = state.connections.clone();
    blocking(move || svc.delete(id)).await?;
    let closed = state.sessions.close_connection(id).await;
    tracing::info!(connection = %id, closed_sessions = closed, "connection deleted");
    Ok(())
}

pub(crate) async fn duplicate_impl(state: &AppState, id: Uuid) -> Result<ConnectionProfile> {
    let svc = state.connections.clone();
    blocking(move || svc.duplicate(id)).await
}

pub(crate) async fn group_save_impl(
    state: &AppState,
    group: ConnectionGroup,
) -> Result<ConnectionGroup> {
    let svc = state.connections.clone();
    blocking(move || svc.save_group(group)).await
}

pub(crate) async fn group_delete_impl(state: &AppState, id: Uuid) -> Result<()> {
    let svc = state.connections.clone();
    blocking(move || svc.delete_group(id)).await
}

/// Connect with an unsaved profile and the stored SSH secret.
#[cfg(test)]
pub(crate) async fn test_impl(
    state: &AppState,
    profile: ConnectionProfile,
    secret: SecretChange,
) -> Result<ServerInfo> {
    test_with_ssh_impl(state, profile, secret, None).await
}

/// The SSH secret a connection test uses: `Set` is what the user just typed, `Clear` means none,
/// `None` / `Keep` use the stored one.
fn effective_ssh_secret(stored: Option<Secret>, change: Option<SecretChange>) -> Option<Secret> {
    match change {
        Some(SecretChange::Set { value }) => Some(value),
        Some(SecretChange::Clear) => None,
        Some(SecretChange::Keep) | None => stored,
    }
}

/// Connect with an unsaved profile, read the server info and disconnect. `ssh_secret` overrides
/// the stored SSH password / key passphrase for this test only (nothing is persisted).
pub(crate) async fn test_with_ssh_impl(
    state: &AppState,
    profile: ConnectionProfile,
    secret: SecretChange,
    ssh_secret: Option<SecretChange>,
) -> Result<ServerInfo> {
    let svc = state.connections.clone();
    let password = {
        let profile = profile.clone();
        blocking(move || svc.resolve_secret(&profile, &secret)).await?
    };
    let mut ssh = super::ssh::ssh_env(state, &profile.config).await?;
    if let Some(env) = ssh.as_mut() {
        env.secret = effective_ssh_secret(env.secret.take(), ssh_secret);
    }
    state
        .sessions
        .test_with_ssh(&profile.config, password.as_ref(), ssh.as_ref())
        .await
}

pub(crate) async fn export_impl(
    state: &AppState,
    path: String,
    ids: Option<Vec<Uuid>>,
) -> Result<u32> {
    let svc = state.connections.clone();
    blocking(move || svc.export_to_file(&path, ids.as_deref())).await
}

pub(crate) async fn import_impl(state: &AppState, path: String) -> Result<ConnectionsSnapshot> {
    let svc = state.connections.clone();
    let (snapshot, report) = blocking(move || svc.import_from_file(&path)).await?;
    tracing::info!(
        profiles = report.profiles,
        groups = report.groups,
        "connections imported"
    );
    if report.dropped_options > 0 || report.reset_colors > 0 {
        tracing::warn!(
            dropped_options = report.dropped_options,
            reset_colors = report.reset_colors,
            "import removed unsafe or invalid values from the imported file"
        );
    }
    Ok(snapshot)
}

/// All groups and profiles.
#[tauri::command]
pub async fn connections_list(state: State<'_, AppState>) -> Result<ConnectionsSnapshot> {
    tracing::debug!(command = "connections_list", "ipc call");
    list_impl(&state).await
}

/// Insert or update (by id) a profile and apply the secret change.
#[tauri::command]
pub async fn connection_save(
    state: State<'_, AppState>,
    profile: ConnectionProfile,
    secret: SecretChange,
    ssh_secret: Option<SecretChange>,
) -> Result<ConnectionProfile> {
    tracing::debug!(command = "connection_save", connection = %profile.config.id, "ipc call");
    save_with_ssh_impl(&state, profile, secret, ssh_secret).await
}

/// Delete a profile and its stored secret.
#[tauri::command]
pub async fn connection_delete(state: State<'_, AppState>, id: Uuid) -> Result<()> {
    tracing::debug!(command = "connection_delete", connection = %id, "ipc call");
    delete_impl(&state, id).await
}

/// Copy a profile (including its stored secret, if any) under a new id.
#[tauri::command]
pub async fn connection_duplicate(
    state: State<'_, AppState>,
    id: Uuid,
) -> Result<ConnectionProfile> {
    tracing::debug!(command = "connection_duplicate", connection = %id, "ipc call");
    duplicate_impl(&state, id).await
}

/// Insert or update (by id) a group.
#[tauri::command]
pub async fn group_save(
    state: State<'_, AppState>,
    group: ConnectionGroup,
) -> Result<ConnectionGroup> {
    tracing::debug!(command = "group_save", group = %group.id, "ipc call");
    group_save_impl(&state, group).await
}

/// Delete a group; its profiles and subgroups move to the parent.
#[tauri::command]
pub async fn group_delete(state: State<'_, AppState>, id: Uuid) -> Result<()> {
    tracing::debug!(command = "group_delete", group = %id, "ipc call");
    group_delete_impl(&state, id).await
}

/// Try to connect with an unsaved profile and report the server info.
#[tauri::command]
pub async fn connection_test(
    state: State<'_, AppState>,
    profile: ConnectionProfile,
    secret: SecretChange,
    ssh_secret: Option<SecretChange>,
) -> Result<ServerInfo> {
    tracing::debug!(command = "connection_test", connection = %profile.config.id, "ipc call");
    test_with_ssh_impl(&state, profile, secret, ssh_secret).await
}

/// Export profiles (all when `ids` is `None`) as JSON without secrets; returns the count.
#[tauri::command]
pub async fn connections_export(
    state: State<'_, AppState>,
    path: String,
    ids: Option<Vec<Uuid>>,
) -> Result<u32> {
    tracing::debug!(command = "connections_export", "ipc call");
    export_impl(&state, path, ids).await
}

/// Import profiles from a JSON file and return the updated snapshot.
#[tauri::command]
pub async fn connections_import(
    state: State<'_, AppState>,
    path: String,
) -> Result<ConnectionsSnapshot> {
    tracing::debug!(command = "connections_import", "ipc call");
    import_impl(&state, path).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(v: &str) -> Option<SecretChange> {
        Some(SecretChange::Set {
            value: Secret::new(v),
        })
    }

    #[test]
    fn test_uses_the_typed_ssh_secret_over_the_stored_one() {
        let stored = || Some(Secret::new("stored"));
        let text = |s: Option<Secret>| s.map(|s| s.expose().to_string());
        assert_eq!(
            text(effective_ssh_secret(stored(), set("typed"))).as_deref(),
            Some("typed")
        );
        assert_eq!(
            text(effective_ssh_secret(stored(), None)).as_deref(),
            Some("stored")
        );
        assert_eq!(
            text(effective_ssh_secret(stored(), Some(SecretChange::Keep))).as_deref(),
            Some("stored")
        );
        assert_eq!(
            text(effective_ssh_secret(stored(), Some(SecretChange::Clear))),
            None
        );
        assert_eq!(text(effective_ssh_secret(None, None)), None);
    }

    fn profile_with_label(label: Option<&str>) -> ConnectionProfile {
        let mut config =
            dbstudio_core::ConnectionConfig::new(dbstudio_core::DbKind::Postgres, "p", "localhost");
        config.environment_label = label.map(str::to_string);
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

    #[tokio::test]
    async fn environment_labels_are_limited_trimmed_and_survive_export_and_import() {
        let (state, _) = crate::state::testing::test_state(dbstudio_core::DriverRegistry::new());
        let too_long = "x".repeat(33);
        let e = save_impl(
            &state,
            profile_with_label(Some(&too_long)),
            SecretChange::Keep,
        )
        .await
        .unwrap_err();
        assert_eq!(e.kind(), "invalid", "{e:?}");
        let saved = save_impl(
            &state,
            profile_with_label(Some("  Staging EU  ")),
            SecretChange::Keep,
        )
        .await
        .unwrap();
        assert_eq!(
            saved.config.environment_label.as_deref(),
            Some("Staging EU")
        );
        let blank = save_impl(&state, profile_with_label(Some("  ")), SecretChange::Keep)
            .await
            .unwrap();
        assert_eq!(blank.config.environment_label, None);
        // The label travels with exported profiles.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("profiles.json");
        let path = file.to_string_lossy().into_owned();
        export_impl(&state, path.clone(), Some(vec![saved.config.id]))
            .await
            .unwrap();
        let (importer, _) = crate::state::testing::test_state(dbstudio_core::DriverRegistry::new());
        let snap = import_impl(&importer, path).await.unwrap();
        assert_eq!(snap.profiles.len(), 1);
        assert_eq!(
            snap.profiles[0].config.environment_label.as_deref(),
            Some("Staging EU")
        );
    }
}
