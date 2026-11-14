//! Cassandra and ScyllaDB driver for DBStudio.
//!
//! One crate registers two drivers ([`CassandraDriver`] for `DbKind::Cassandra`,
//! [`ScyllaDriver`] for `DbKind::ScyllaDb`) that share the implementation on top of the
//! `scylla` crate (compatible with Apache Cassandra).
//!
//! * The object tree is built from `system_schema` (cached for a minute): keyspaces (system ones
//!   in a `system` folder when `cql.showSystem` is on) with folders for tables, materialized
//!   views, types, functions, aggregates and triggers.
//! * Statements run unprepared with explicit paging; cqlsh commands (`CONSISTENCY`,
//!   `SERIAL CONSISTENCY`, `TRACING`, `USE`, `DESCRIBE`) are handled by the driver.
//! * A lease keeps console state (keyspace, consistency, tracing) over a pool of driver sessions
//!   per keyspace; CQL has no transactions.
//! * Data editing uses lightweight transactions (`IF [NOT] EXISTS`) unless the profile option
//!   `cql.lwtEdits` is `false`.
//!
//! Profile options: `cql.localDc`, `cql.consistency` (default `LOCAL_ONE`), `cql.showSystem`,
//! `cql.lwtEdits`, `cql.singleNode`.

mod conn;
mod convert;
mod core;
mod data;
mod ddl;
mod describe;
mod edit;
mod error;
mod explain;
mod ident;
mod lease;
mod rows;
mod schema;
mod session;
mod tree;
mod types;

use std::sync::Arc;

use async_trait::async_trait;
use dbstudio_core::{
    Capabilities, ConnectionConfig, DbKind, Driver, DriverDescriptor, DriverRegistry, DriverStatus,
    Result, Secret, Session,
};

pub use session::CassandraSession;

/// Driver for Apache Cassandra (and compatible) servers.
#[derive(Debug, Default, Clone, Copy)]
pub struct CassandraDriver;

/// Driver for ScyllaDB servers.
#[derive(Debug, Default, Clone, Copy)]
pub struct ScyllaDriver;

macro_rules! cql_driver {
    ($driver:ty, $kind:expr) => {
        impl $driver {
            /// Create the driver.
            pub fn new() -> Self {
                Self
            }
        }

        #[async_trait]
        impl Driver for $driver {
            fn descriptor(&self) -> DriverDescriptor {
                DriverDescriptor::new(
                    $kind,
                    Capabilities::planned_for($kind),
                    DriverStatus::Available,
                )
            }

            async fn connect(
                &self,
                config: &ConnectionConfig,
                secret: Option<&Secret>,
            ) -> Result<Box<dyn Session>> {
                Ok(Box::new(CassandraSession::connect(config, secret).await?))
            }
        }
    };
}

cql_driver!(CassandraDriver, DbKind::Cassandra);
cql_driver!(ScyllaDriver, DbKind::ScyllaDb);

/// Register both drivers in `registry`.
pub fn register(registry: &mut DriverRegistry) {
    registry.register(Arc::new(CassandraDriver::new()));
    registry.register(Arc::new(ScyllaDriver::new()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_both_kinds_with_the_designed_capabilities() {
        let mut registry = DriverRegistry::new();
        register(&mut registry);
        for kind in [DbKind::Cassandra, DbKind::ScyllaDb] {
            let driver = registry.get(kind).expect("registered");
            let d = driver.descriptor();
            assert_eq!(d.kind, kind);
            assert_eq!(d.status, DriverStatus::Available);
            assert_eq!(d.default_port, 9042);
            assert_eq!(d.capabilities, Capabilities::planned_for(kind));
            assert!(d.capabilities.sql && !d.capabilities.transactions);
            assert!(!d.capabilities.table_sort && !d.capabilities.exact_count);
        }
        let cass = registry.get(DbKind::Cassandra).unwrap().descriptor();
        let scylla = registry.get(DbKind::ScyllaDb).unwrap().descriptor();
        assert!(cass.capabilities.triggers && !scylla.capabilities.triggers);
    }

    #[tokio::test]
    async fn unreachable_server_is_a_connection_error() {
        let mut cfg = ConnectionConfig::new(DbKind::Cassandra, "t", "127.0.0.1");
        cfg.port = 1;
        cfg.connect_timeout_ms = Some(800);
        let err = CassandraDriver::new()
            .connect(&cfg, None)
            .await
            .err()
            .unwrap();
        assert_eq!(err.kind(), "connection", "{err}");
    }
}
