//! Redis / Valkey driver for DBStudio.
//!
//! Scaffold status: the driver registers and describes itself; `connect` returns
//! `Error::Unsupported`. Package L implements it.

use std::sync::Arc;

use async_trait::async_trait;
use dbstudio_core::{
    Capabilities, ConnectionConfig, DbKind, Driver, DriverDescriptor, DriverRegistry, DriverStatus,
    Error, Result, Secret, Session,
};

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
        let _ = (config, secret);
        Err(Error::Unsupported("redis".into()))
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
        let d = r
            .descriptors()
            .into_iter()
            .find(|d| d.kind == DbKind::Redis)
            .unwrap();
        assert_eq!(d.status, DriverStatus::Available);
    }

    #[tokio::test]
    async fn connect_is_unsupported() {
        let cfg = ConnectionConfig::new(DbKind::Redis, "test", "localhost");
        let err = RedisDriver::new().connect(&cfg, None).await.err().unwrap();
        assert_eq!(err.kind(), "unsupported");
    }

    #[test]
    fn client_crate_is_linked() {
        let _ = std::any::type_name::<redis::Client>();
    }
}
