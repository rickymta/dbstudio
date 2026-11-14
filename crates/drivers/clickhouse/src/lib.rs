//! ClickHouse driver for DBStudio (HTTP interface).
//!
//! Contract scaffold of the backlog contract v2 (package H fills it in). For now `connect`
//! answers `Unsupported`.

use std::sync::Arc;

use async_trait::async_trait;
use dbstudio_core::{
    Capabilities, ConnectionConfig, DbKind, Driver, DriverDescriptor, DriverRegistry, DriverStatus,
    Error, Result, Secret, Session,
};

/// Driver for ClickHouse servers.
#[derive(Debug, Default, Clone, Copy)]
pub struct ClickHouseDriver;

impl ClickHouseDriver {
    /// Create the driver.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Driver for ClickHouseDriver {
    fn descriptor(&self) -> DriverDescriptor {
        DriverDescriptor::new(
            DbKind::ClickHouse,
            Capabilities::planned_for(DbKind::ClickHouse),
            DriverStatus::Available,
        )
    }

    async fn connect(
        &self,
        _config: &ConnectionConfig,
        _secret: Option<&Secret>,
    ) -> Result<Box<dyn Session>> {
        Err(Error::Unsupported(DbKind::ClickHouse.id().into()))
    }
}

/// Register the driver in `registry`.
pub fn register(registry: &mut DriverRegistry) {
    registry.register(Arc::new(ClickHouseDriver::new()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_and_connect_is_unsupported() {
        let mut registry = DriverRegistry::new();
        register(&mut registry);
        let driver = registry.get(DbKind::ClickHouse).expect("registered");
        let d = driver.descriptor();
        assert_eq!(d.default_port, 8123);
        assert!(d.capabilities.sql && d.capabilities.explain && !d.capabilities.transactions);
        let cfg = ConnectionConfig::new(DbKind::ClickHouse, "t", "localhost");
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let err = rt.block_on(driver.connect(&cfg, None)).err().unwrap();
        assert!(matches!(err, Error::Unsupported(c) if c == "clickhouse"));
    }
}
