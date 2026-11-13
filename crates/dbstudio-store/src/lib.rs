//! Persistent storage for DBStudio: connection profiles and groups in SQLite, passwords in the
//! OS keychain.
//!
//! * [`Store`] is the SQLite layer (versioned migrations, no secrets).
//! * [`SecretStore`] abstracts the keychain ([`KeyringSecretStore`], [`MemorySecretStore`]).
//! * [`ConnectionService`] combines both and implements import/export.

mod backup;
mod db;
mod erd;
mod history;
mod known_hosts;
mod model;
mod presets;
mod saved;
mod secret;
mod service;
mod settings;
mod tabs;
mod validate;

pub use backup::{BackupJobRow, BackupRunRow};
pub use db::Store;
pub use erd::{ERD_CONFLICT, ErdLayout, ErdLayoutSummary};
pub use history::{
    HISTORY_LIMIT_RANGE, HISTORY_TEXT_MAX_CHARS, HistoryEntry, HistoryQuery, NewHistoryEntry,
};
pub use known_hosts::KnownHost;
pub use model::{ConnectionGroup, ConnectionProfile, ConnectionsSnapshot, SecretChange};
pub use presets::ExportPreset;
pub use saved::{SavedFolder, SavedQueriesSnapshot, SavedQuery};
pub use secret::{
    KEYRING_SERVICE, KeyringSecretStore, MemorySecretStore, SecretStore, backup_secret_account,
    ssh_secret_account,
};
pub use service::{
    ConnectionService, EXPORT_FORMAT, EXPORT_VERSION, ImportReport, MAX_IMPORT_BYTES,
    MAX_IMPORT_GROUPS, MAX_IMPORT_PROFILES,
};
pub use settings::{
    AppSettings, CloseBehavior, EDITOR_ROWS_RANGE, GRID_CELLS_RANGE, GRID_ROWS_RANGE,
    HISTORY_MAX_RANGE, LOB_BYTES_RANGE, LogLevel, OracleClientSettings, PLAN_FACTOR_RANGE,
    QUERY_TIMEOUT_MAX_MS, TimestampDisplay,
};
pub use tabs::EditorTabState;
pub use validate::{IMPORT_OPTION_ALLOWLIST, MAX_FIELD_CHARS, is_valid_color};
