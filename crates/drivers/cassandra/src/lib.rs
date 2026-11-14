//! Cassandra and ScyllaDB driver for DBStudio.
//!
//! Contract scaffold of the backlog contract v2 (package C fills it in): one crate registers two
//! drivers ([`CassandraDriver`] for `DbKind::Cassandra`, [`ScyllaDriver`] for `DbKind::ScyllaDb`)
//! that share the implementation. For now `connect` answers `Unsupported`.

use std::sync::Arc;

use async_trait::async_trait;
use dbstudio_core::{
    Capabilities, ConnectionConfig, DbKind, Driver, DriverDescriptor, DriverRegistry, DriverStatus,
    Error, Result, Secret, Session,
};

/// Driver for Apache Cassandra (and compatible) servers.
#[derive(Debug, Default, Clone, Copy)]
pub struct CassandraDriver;

/// Driver for ScyllaDB servers.
#[derive(Debug, Default, Clone, Copy)]
pub struct ScyllaDriver;

macro_rules! scaffold_driver {
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
                _config: &ConnectionConfig,
                _secret: Option<&Secret>,
            ) -> Result<Box<dyn Session>> {
                Err(Error::Unsupported($kind.id().into()))
            }
        }
    };
}

scaffold_driver!(CassandraDriver, DbKind::Cassandra);
scaffold_driver!(ScyllaDriver, DbKind::ScyllaDb);

/// Register both drivers in `registry`.
pub fn register(registry: &mut DriverRegistry) {
    registry.register(Arc::new(CassandraDriver::new()));
    registry.register(Arc::new(ScyllaDriver::new()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_both_kinds_and_connect_is_unsupported() {
        let mut registry = DriverRegistry::new();
        register(&mut registry);
        for kind in [DbKind::Cassandra, DbKind::ScyllaDb] {
            let driver = registry.get(kind).expect("registered");
            let d = driver.descriptor();
            assert_eq!(d.kind, kind);
            assert_eq!(d.default_port, 9042);
            assert!(d.capabilities.sql && !d.capabilities.transactions);
            let cfg = ConnectionConfig::new(kind, "t", "localhost");
            let err = futures_block(driver.connect(&cfg, None)).err().unwrap();
            assert!(matches!(err, Error::Unsupported(c) if c == kind.id()));
        }
    }

    fn futures_block<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(f)
    }
}
