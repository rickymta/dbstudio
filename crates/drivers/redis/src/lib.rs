//! Redis / Valkey driver for DBStudio (Redis 6/7, Valkey 8).
//!
//! * A [`RedisSession`] stands for a whole server: it keeps one multiplexed connection per
//!   database (created lazily, never `SELECT` on a shared connection).
//! * The object tree is `Root -> Database -> KeyPrefix* -> Key`, built from `SCAN` pages (never
//!   `KEYS`, so it works when `KEYS` is renamed) with a configurable delimiter option
//!   `delimiter` (default `:`).
//! * Values are read page by page with [`dbstudio_core::KeyReadRequest`] and edited through
//!   `ChangeSet::Keys` (`WATCH` + type check + `MULTI/EXEC`; not atomic across changes).
//! * The console executes one redis-cli line per command; leases are dedicated connections.

mod cli;
mod conn;
mod convert;
mod edit;
mod error;
mod exec;
mod keys;
mod lease;
mod session;
mod tree;

use std::sync::Arc;

use async_trait::async_trait;
use dbstudio_core::{
    Capabilities, ConnectionConfig, DbKind, Driver, DriverDescriptor, DriverRegistry, DriverStatus,
    Result, Secret, Session,
};

pub use cli::{quote_arg, render_command, tokenize_line};
pub use exec::{DELETE_PATTERN, InternalGrant, authorize_internal};
pub use session::RedisSession;

/// Driver for Redis / Valkey.
#[derive(Debug, Default, Clone, Copy)]
pub struct RedisDriver;

impl RedisDriver {
    /// Create the driver.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Driver for RedisDriver {
    fn descriptor(&self) -> DriverDescriptor {
        DriverDescriptor::new(
            DbKind::Redis,
            Capabilities::planned_for(DbKind::Redis),
            DriverStatus::Available,
        )
    }

    async fn connect(
        &self,
        config: &ConnectionConfig,
        secret: Option<&Secret>,
    ) -> Result<Box<dyn Session>> {
        Ok(Box::new(RedisSession::connect(config, secret).await?))
    }
}

/// Register the driver in `registry`.
pub fn register(registry: &mut DriverRegistry) {
    registry.register(Arc::new(RedisDriver::new()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_matches_the_design() {
        let d = RedisDriver::new().descriptor();
        assert_eq!(d.kind, DbKind::Redis);
        assert_eq!(d.status, DriverStatus::Available);
        assert_eq!(d.default_port, 6379);
        assert!(d.capabilities.key_value_store && !d.capabilities.sql);
        assert_eq!(d.capabilities, Capabilities::planned_for(DbKind::Redis));
    }

    #[test]
    fn registers_as_available() {
        let mut r = DriverRegistry::new();
        register(&mut r);
        assert!(r.get(DbKind::Redis).is_some());
    }

    #[tokio::test]
    async fn unreachable_server_is_a_connection_error() {
        let mut cfg = ConnectionConfig::new(DbKind::Redis, "test", "127.0.0.1");
        cfg.port = 1;
        cfg.tls.mode = dbstudio_core::TlsMode::Disable;
        cfg.connect_timeout_ms = Some(500);
        let err = RedisDriver::new().connect(&cfg, None).await.err().unwrap();
        assert_eq!(err.kind(), "connection");
    }
}
