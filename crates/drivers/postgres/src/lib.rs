//! PostgreSQL driver for DBStudio. The connection logic is implemented in phase 1.

use async_trait::async_trait;
use dbstudio_core::{
    Capabilities, ConnectionConfig, DbKind, Driver, DriverDescriptor, DriverStatus, Error, Result,
    Secret, Session,
};

/// Driver for PostgreSQL servers.
#[derive(Debug, Default, Clone, Copy)]
pub struct PostgresDriver;

impl PostgresDriver {
    /// Create the driver.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Driver for PostgresDriver {
    fn descriptor(&self) -> DriverDescriptor {
        DriverDescriptor::new(
            DbKind::Postgres,
            Capabilities::planned_for(DbKind::Postgres),
            DriverStatus::Available,
        )
    }

    async fn connect(
        &self,
        _config: &ConnectionConfig,
        _secret: Option<&Secret>,
    ) -> Result<Box<dyn Session>> {
        Err(Error::Unsupported("not implemented".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_is_available() {
        let d = PostgresDriver::new().descriptor();
        assert_eq!(d.kind, DbKind::Postgres);
        assert_eq!(d.status, DriverStatus::Available);
        assert!(d.capabilities.sql && d.capabilities.cancel_query);
    }
}
