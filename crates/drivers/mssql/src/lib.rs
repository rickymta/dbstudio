//! Microsoft SQL Server driver for DBStudio.
//!
//! Scaffold status: the driver registers and describes itself; `connect` returns
//! `Error::Unsupported`. Package I implements it.

use std::sync::Arc;

use async_trait::async_trait;
use dbstudio_core::{
    Capabilities, ConnectionConfig, DbKind, Driver, DriverDescriptor, DriverRegistry, DriverStatus,
    Error, Result, Secret, Session,
};

/// Driver for Microsoft SQL Server.
#[derive(Debug, Default, Clone, Copy)]
pub struct MssqlDriver;

impl MssqlDriver {
    /// Create the driver.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Driver for MssqlDriver {
    fn descriptor(&self) -> DriverDescriptor {
        DriverDescriptor::new(
            DbKind::SqlServer,
            Capabilities::planned_for(DbKind::SqlServer),
            DriverStatus::Available,
        )
    }

    async fn connect(
        &self,
        config: &ConnectionConfig,
        secret: Option<&Secret>,
    ) -> Result<Box<dyn Session>> {
        let _ = (config, secret);
        Err(Error::Unsupported("mssql".into()))
    }
}

/// Register the driver in `registry`.
pub fn register(registry: &mut DriverRegistry) {
    registry.register(Arc::new(MssqlDriver::new()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_matches_the_design() {
        let d = MssqlDriver::new().descriptor();
        assert_eq!(d.kind, DbKind::SqlServer);
        assert_eq!(d.status, DriverStatus::Available);
        assert_eq!(d.default_port, 1433);
        assert!(d.capabilities.sql && d.capabilities.cancel_query);
        assert_eq!(d.capabilities, Capabilities::planned_for(DbKind::SqlServer));
    }

    #[test]
    fn registers_as_available() {
        let mut r = DriverRegistry::new();
        register(&mut r);
        assert!(r.get(DbKind::SqlServer).is_some());
        let d = r
            .descriptors()
            .into_iter()
            .find(|d| d.kind == DbKind::SqlServer)
            .unwrap();
        assert_eq!(d.status, DriverStatus::Available);
    }

    #[tokio::test]
    async fn connect_is_unsupported() {
        let cfg = ConnectionConfig::new(DbKind::SqlServer, "test", "localhost");
        let err = MssqlDriver::new().connect(&cfg, None).await.err().unwrap();
        assert_eq!(err.kind(), "unsupported");
    }

    #[test]
    fn client_crate_is_linked() {
        let _ = std::any::type_name::<tiberius::Config>();
    }
}
