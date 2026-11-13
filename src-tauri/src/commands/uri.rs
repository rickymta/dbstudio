//! Connection URI parsing (package K).

use dbstudio_core::Error;
use dbstudio_store::ConnectionProfile;
use serde::Serialize;
use tauri::State;

use crate::state::AppState;

type Result<T> = std::result::Result<T, Error>;

/// Result of `connection_parse_uri`; the password stays in the backend (`pending_secret_id`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParsedConnectionUri {
    pub profile: ConnectionProfile,
    pub has_password: bool,
    pub pending_secret_id: Option<String>,
}

pub(crate) fn parse_uri_impl(state: &AppState, uri: &str) -> Result<ParsedConnectionUri> {
    let _ = (state, uri);
    Err(Error::Unsupported("connection_uri".into()))
}

#[tauri::command]
pub fn connection_parse_uri(
    state: State<'_, AppState>,
    uri: String,
) -> Result<ParsedConnectionUri> {
    tracing::debug!(command = "connection_parse_uri", "ipc call");
    parse_uri_impl(&state, &uri)
}
