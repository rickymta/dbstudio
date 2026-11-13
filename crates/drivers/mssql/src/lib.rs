//! Microsoft SQL Server driver for DBStudio (SQL Server 2016 and newer, Azure SQL Database).
//!
//! * Talks TDS through a minimal fork of `tiberius` 0.13 (`vendor/tiberius-patch`): `INFO`
//!   (`PRINT`) and `DONE` (row count) items, cooperative attention (query cancel) and exact
//!   `money` decoding. See `vendor/tiberius-patch/DBSTUDIO-PATCH.md`.
//! * A [`MssqlSession`] stands for a whole server: lazy pools per database (4 data + 2 catalog
//!   connections), one dedicated connection per lease.
//! * Scripts are split into batches at `GO [n]` lines by this crate (`batch` module);
//!   `dbstudio_sql::split` is not used because its contract carries no repeat count.
//!
//! # Login and TLS
//!
//! SQL login by default; `auth=windows` is NTLM (`DOMAIN\user`) on every OS through `sspi-rs`
//! (compiled in, not exercised by the integration tests: it needs a domain); `auth=integrated`
//! only exists on Windows. TLS: `Disable` does not encrypt, `Prefer`/`Require` encrypt without
//! verifying the certificate (TDS has no plain-text fallback once the server asks for
//! encryption), `VerifyCa`/`VerifyFull` verify chain and host name (`verifyCa` behaves like
//! `verifyFull`; the library cannot skip the name check). Without a CA file the operating
//! system's roots are used, a CA file is trusted in addition. `encrypt=strict` selects TDS 8.0.
//! Integrated security and Azure AD tokens beyond that are out of scope.
//!
//! # Behaviour notes
//!
//! * Every statement the server reports is one statement of the sink (`Rows`, `Affected` or
//!   `Done`), so a script of three statements gives three results; the statement range is the
//!   whole batch.
//! * `max_rows` cancels the rest of the batch (leases keep the result open for `fetch_more`).
//! * Search across databases is restricted to the session's default database.
//! * `TxOptions::read_only` has no T-SQL equivalent and is ignored (the application guard
//!   enforces read-only connections).

mod batch;
mod bulk;
mod browse;
mod catalog;
mod conn;
mod data;
mod ddl;
mod ddl_gen;
mod edit;
mod error;
mod exec;
mod explain;
mod lease;
mod model;
mod settings;
mod session;
mod sql;
mod value;

use async_trait::async_trait;
use dbstudio_core::{
    Capabilities, ConnectionConfig, DbKind, Driver, DriverDescriptor, DriverStatus, Result, Secret,
    Session,
};

pub use session::MssqlSession;

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
        let settings = settings::Settings::from_config(config, secret)?;
        let session = MssqlSession::connect(settings, config.read_only).await?;
        Ok(Box::new(session))
    }
}

/// Register the driver in `registry`.
pub fn register(registry: &mut dbstudio_core::DriverRegistry) {
    registry.register(std::sync::Arc::new(MssqlDriver::new()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_core::DriverRegistry;

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
    fn capabilities_follow_contract_table() {
        let c = MssqlDriver::new().descriptor().capabilities;
        assert!(c.leases && c.manual_transactions && c.query_params);
        assert!(c.explain && c.explain_analyze && c.explain_analyze_writes);
        assert!(c.foreign_keys && c.schema_model && c.ddl_generation);
        assert!(c.table_stream && c.bulk_insert && c.native_backup && c.logical_backup);
        assert!(c.data_editing && c.schemas && c.multiple_databases && c.ssh_tunnel);
        assert!(!c.document_store && !c.key_value_store && !c.paged_tree);
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
    async fn bad_options_fail_before_connecting() {
        let mut cfg = ConnectionConfig::new(DbKind::SqlServer, "t", "127.0.0.1");
        cfg.username = Some("sa".into());
        cfg.options.insert("pool_max".into(), "0".into());
        let err = MssqlDriver::new().connect(&cfg, None).await.err().unwrap();
        assert_eq!(err.kind(), "other");
    }
}
