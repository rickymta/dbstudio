//! Persistent storage for DBStudio: connection profiles and groups in SQLite, passwords in the
//! OS keychain.
//!
//! * [`Store`] is the SQLite layer (versioned migrations, no secrets).
//! * [`SecretStore`] abstracts the keychain ([`KeyringSecretStore`], [`MemorySecretStore`]).
//! * [`ConnectionService`] combines both and implements import/export.

mod db;
mod model;
mod secret;
mod service;
mod settings;
mod validate;

pub use db::Store;
pub use model::{ConnectionGroup, ConnectionProfile, ConnectionsSnapshot, SecretChange};
pub use secret::{KEYRING_SERVICE, KeyringSecretStore, MemorySecretStore, SecretStore};
pub use service::{
    ConnectionService, EXPORT_FORMAT, EXPORT_VERSION, ImportReport, MAX_IMPORT_BYTES,
    MAX_IMPORT_GROUPS, MAX_IMPORT_PROFILES,
};
pub use settings::{AppSettings, GRID_CELLS_RANGE, GRID_ROWS_RANGE, LogLevel, TimestampDisplay};
pub use validate::{IMPORT_OPTION_ALLOWLIST, MAX_FIELD_CHARS, is_valid_color};
