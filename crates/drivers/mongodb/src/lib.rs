//! MongoDB driver for DBStudio.
//!
//! Scaffold status: the driver registers and describes itself; `connect` returns
//! `Error::Unsupported`. Package K implements it.

use std::sync::Arc;

use async_trait::async_trait;
use dbstudio_core::{
    Capabilities, ConnectionConfig, DbKind, Driver, DriverDescriptor, DriverRegistry, DriverStatus,
    Error, Result, Secret, Session,
};

/// Driver for MongoDB.
#[derive(Debug, Default, Clone, Copy)]
pub struct MongoDriver;

impl MongoDriver {
    /// Create the driver.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Driver for MongoDriver {
    fn descriptor(&self) -> DriverDescriptor {
        DriverDescriptor::new(
            DbKind::MongoDb,
            Capabilities::planned_for(DbKind::MongoDb),
            DriverStatus::Available,
        )
    }

    async fn connect(
        &self,
        config: &ConnectionConfig,
        secret: Option<&Secret>,
    ) -> Result<Box<dyn Session>> {
        let _ = (config, secret);
        Err(Error::Unsupported("mongodb".into()))
    }
}

/// Register the driver in `registry`.
pub fn register(registry: &mut DriverRegistry) {
    registry.register(Arc::new(MongoDriver::new()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_matches_the_design() {
        let d = MongoDriver::new().descriptor();
        assert_eq!(d.kind, DbKind::MongoDb);
        assert_eq!(d.status, DriverStatus::Available);
        assert_eq!(d.default_port, 27017);
        assert!(d.capabilities.document_store && !d.capabilities.sql);
        assert_eq!(d.capabilities, Capabilities::planned_for(DbKind::MongoDb));
    }

    #[test]
    fn registers_as_available() {
        let mut r = DriverRegistry::new();
        register(&mut r);
        assert!(r.get(DbKind::MongoDb).is_some());
        let d = r
            .descriptors()
            .into_iter()
            .find(|d| d.kind == DbKind::MongoDb)
            .unwrap();
        assert_eq!(d.status, DriverStatus::Available);
    }

    #[tokio::test]
    async fn connect_is_unsupported() {
        let cfg = ConnectionConfig::new(DbKind::MongoDb, "test", "localhost");
        let err = MongoDriver::new().connect(&cfg, None).await.err().unwrap();
        assert_eq!(err.kind(), "unsupported");
    }

    #[test]
    fn client_crate_is_linked() {
        let _ = std::any::type_name::<bson::Document>();
    }
}
