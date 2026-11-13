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

pub use db::Store;
pub use model::{ConnectionGroup, ConnectionProfile, ConnectionsSnapshot, SecretChange};
pub use secret::{KEYRING_SERVICE, KeyringSecretStore, MemorySecretStore, SecretStore};
pub use service::{ConnectionService, EXPORT_FORMAT, EXPORT_VERSION};
