//! PostgreSQL driver for DBStudio (PostgreSQL 12 and newer).
//!
//! * Connections use `tokio-postgres` with TLS through `rustls` (`ring` provider). A
//!   [`PostgresSession`] stands for a whole server: it keeps a small lazily-created pool per
//!   database (at most 4 connections each).
//! * Result values are fetched in PostgreSQL's *text* format and decoded by type (see
//!   [`decode`]); unknown types stay text, so no column can make a query fail.
//! * Large results are streamed through server-side cursors in batches (see [`exec`]).

mod browse;
mod conn;
mod ddl;
mod decode;
mod details;
mod error;
mod exec;
mod session;
mod sql;
mod tls;

use async_trait::async_trait;
use dbstudio_core::{
    Capabilities, ConnectionConfig, DbKind, Driver, DriverDescriptor, DriverStatus, Result, Secret,
    Session,
};

pub use session::PostgresSession;

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
        config: &ConnectionConfig,
        secret: Option<&Secret>,
    ) -> Result<Box<dyn Session>> {
        let shared = conn::Shared::new(config, secret)?;
        Ok(Box::new(PostgresSession::new(shared).await?))
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
