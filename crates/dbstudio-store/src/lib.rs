//! Persistent storage for DBStudio: connection profiles, groups and secrets.
//!
//! Phase 1 contract: only the data model exists here; the SQLite store and keychain access are
//! implemented later.

mod model;

pub use model::{ConnectionGroup, ConnectionProfile, ConnectionsSnapshot, SecretChange};
