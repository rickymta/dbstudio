//! MySQL driver for DBStudio. The connection logic is implemented in phase 1.

use async_trait::async_trait;
use dbstudio_core::{
    Capabilities, ConnectionConfig, DbKind, Driver, DriverDescriptor, DriverStatus, Error, Result,
    Secret, Session,
};

/// Driver for MySQL servers.
#[derive(Debug, Default, Clone, Copy)]
pub struct MySqlDriver;

impl MySqlDriver {
    /// Create the driver.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Driver for MySqlDriver {
    fn descriptor(&self) -> DriverDescriptor {
        DriverDescriptor::new(
            DbKind::MySql,
            Capabilities::planned_for(DbKind::MySql),
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
        let d = MySqlDriver::new().descriptor();
        assert_eq!(d.kind, DbKind::MySql);
        assert_eq!(d.status, DriverStatus::Available);
        assert!(d.capabilities.sql && d.capabilities.cancel_query);
    }
}
