//! SSH commands (package A): trusting a host key after `Error::SshHostKey`.

use dbstudio_core::{Error, Result};
use tauri::State;

use super::blocking;
use crate::state::AppState;

/// Store the key the user accepted; the frontend then opens the session again.
pub(crate) async fn trust_host_impl(
    state: &AppState,
    host: String,
    port: u16,
    key_type: String,
    fingerprint: String,
) -> Result<()> {
    let svc = state.connections.clone();
    blocking(move || {
        svc.store()
            .known_host_add(&host, port, &key_type, &fingerprint)
            .map(|_| ())
    })
    .await
}

#[tauri::command]
pub async fn ssh_trust_host(
    state: State<'_, AppState>,
    host: String,
    port: u16,
    key_type: String,
    fingerprint: String,
) -> std::result::Result<(), Error> {
    tracing::debug!(command = "ssh_trust_host", host = %host, port, "ipc call");
    trust_host_impl(&state, host, port, key_type, fingerprint).await
}
