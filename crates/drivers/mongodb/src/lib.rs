//! MongoDB driver for DBStudio (MongoDB 4.4 and newer).
//!
//! * Everything runs as raw database commands (`find`, `aggregate`, `insert`, ...), so the same
//!   code serves the pooled session and the lease (which adds a `ClientSession` for
//!   transactions).
//! * Collections are returned as one `$document` column; BSON maps losslessly onto `Value`
//!   (see [`convert`]).
//! * Scripts use the shell syntax parsed by `dbstudio_sql::mongo_shell`.

mod conn;
mod convert;
mod edit;
mod exec;
mod lease;
mod model;
mod query;
mod session;
mod shell;

use std::sync::Arc;

use async_trait::async_trait;
use dbstudio_core::{
    Capabilities, ConnectionConfig, DbKind, Driver, DriverDescriptor, DriverRegistry, DriverStatus,
    Result, Secret, Session,
};

pub use convert::{
    bson_to_value, doc_to_ejson, parse_ejson_doc, to_ejson, value_to_bson,
};
pub use session::MongoSession;

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
        let conn = conn::Conn::connect(config, secret).await?;
        Ok(Box::new(MongoSession::new(conn)))
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
    }
}
