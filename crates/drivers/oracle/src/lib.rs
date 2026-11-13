//! Oracle driver for DBStudio (`oracle` crate on top of ODPI-C).
//!
//! ODPI-C is compiled into the crate but loads the Oracle Client (`libclntsh`) at run time, so a
//! machine without Instant Client still starts: [`OracleDriver::descriptor`] probes once and
//! reports `Unavailable { NativeClientMissing }`, and [`OracleDriver::connect`] fails with
//! `DriverUnavailable` and installation instructions. The directory of the client comes from the
//! `oracle_client` setting through [`configure_client`]; [`client_diagnostics`] explains the state.
//!
//! * Sessions keep a small pool of connections (at most 4); every blocking OCI call runs on the
//!   tokio blocking pool and streams its results back through a bounded channel ([`runtime`]).
//! * Scripts are split by an Oracle aware splitter ([`script`]): PL/SQL blocks, units ending with a
//!   `/` line and SQL*Plus conventions are handled; `DBMS_OUTPUT` becomes messages.
//! * Values are read as text from OCI so `NUMBER` never loses digits ([`value`]).
//! * Leases ([`lease`]) hold a dedicated connection with manual transactions and cancellation.

mod browse;
mod catalog;
mod client;
mod data;
mod ddl;
mod edit;
mod error;
mod exec;
mod explain;
mod lease;
mod model;
mod runtime;
mod script;
mod session;
mod sql;
mod target;
mod value;

use std::sync::Arc;

use async_trait::async_trait;
use dbstudio_core::{
    Capabilities, ConnectionConfig, DbKind, Driver, DriverDescriptor, DriverRegistry, DriverStatus,
    Error, Result, Secret, Session,
};

pub use client::{ClientDiagnostics, LIB_DIR_ENV};
pub use lease::OracleLease;
pub use session::OracleSession;

/// Set the directory with the Instant Client libraries (the `oracle_client` setting; `None` or an
/// empty string searches the default places). Has no effect once the client is loaded.
pub fn configure_client(lib_dir: Option<&str>) {
    client::configure(lib_dir);
}

/// Probe the client again (also after a failure) and describe the result.
pub fn client_diagnostics() -> ClientDiagnostics {
    client::diagnostics()
}

/// Result of the client probe: `Ok(version)` or the error text.
pub fn client_probe() -> std::result::Result<String, String> {
    match client::probe() {
        client::Probe::Loaded(v) => Ok(v),
        client::Probe::Failed { message, .. } => Err(message),
    }
}

/// Driver for Oracle Database.
#[derive(Debug, Default, Clone, Copy)]
pub struct OracleDriver;

impl OracleDriver {
    /// Create the driver.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Driver for OracleDriver {
    fn descriptor(&self) -> DriverDescriptor {
        let status = match client::probe().unavailable_reason() {
            None => DriverStatus::Available,
            Some(reason) => DriverStatus::Unavailable { reason },
        };
        DriverDescriptor::new(
            DbKind::Oracle,
            Capabilities::planned_for(DbKind::Oracle),
            status,
        )
    }

    async fn connect(
        &self,
        config: &ConnectionConfig,
        secret: Option<&Secret>,
    ) -> Result<Box<dyn Session>> {
        if let client::Probe::Failed { message, .. } = client::probe() {
            return Err(Error::DriverUnavailable(format!(
                "Oracle Client libraries (Instant Client) are not available: {message} {}",
                client::diagnostics().install_hint
            )));
        }
        let target = target::build(config, secret)?;
        let shared = runtime::Shared::new(target, config.read_only);
        // Open the first connection now: wrong credentials or an unreachable listener are
        // reported by `connect`, and the connection is kept in the pool.
        drop(shared.acquire().await?);
        Ok(Box::new(OracleSession::new(shared)))
    }
}

/// Register the driver in `registry`.
pub fn register(registry: &mut DriverRegistry) {
    registry.register(Arc::new(OracleDriver::new()));
}

#[cfg(test)]
mod tests {
    use dbstudio_core::UnavailableReason;

    use super::*;

    #[test]
    fn descriptor_matches_the_design_and_never_panics() {
        let d = OracleDriver::new().descriptor();
        assert_eq!(d.kind, DbKind::Oracle);
        assert_eq!(d.default_port, 1521);
        assert!(matches!(
            d.status,
            DriverStatus::Available
                | DriverStatus::Unavailable {
                    reason: UnavailableReason::NativeClientMissing
                }
        ));
        assert_eq!(d.capabilities, Capabilities::planned_for(DbKind::Oracle));
        assert!(d.capabilities.sql && d.capabilities.explain_analyze);
        assert!(d.capabilities.leases && d.capabilities.schema_model && d.capabilities.bulk_insert);
        // The probe is cached: a second call gives the same status.
        assert_eq!(OracleDriver::new().descriptor().status, d.status);
    }

    #[test]
    fn registry_keeps_unavailable_status() {
        let mut r = DriverRegistry::new();
        register(&mut r);
        assert!(r.get(DbKind::Oracle).is_some());
        let d = r
            .descriptors()
            .into_iter()
            .find(|d| d.kind == DbKind::Oracle)
            .unwrap();
        assert_eq!(d.status, OracleDriver::new().descriptor().status);
    }

    #[tokio::test]
    async fn connect_without_a_client_is_unavailable_not_a_panic() {
        let mut cfg = ConnectionConfig::new(DbKind::Oracle, "test", "127.0.0.1");
        cfg.database = Some("FREEPDB1".into());
        cfg.username = Some("scott".into());
        cfg.port = 1; // nothing listens here
        cfg.connect_timeout_ms = Some(500);
        let err = OracleDriver::new()
            .connect(&cfg, Some(&Secret::new("x")))
            .await
            .err()
            .unwrap();
        match OracleDriver::new().descriptor().status {
            DriverStatus::Available => assert_eq!(err.kind(), "connection"),
            _ => {
                assert_eq!(err.kind(), "driverUnavailable");
                assert!(err.message().contains("Instant Client"));
            }
        }
    }

    #[tokio::test]
    async fn connect_validates_the_user_before_touching_the_network() {
        let cfg = ConnectionConfig::new(DbKind::Oracle, "test", "127.0.0.1");
        let err = OracleDriver::new().connect(&cfg, None).await.err().unwrap();
        // Without a client the unavailable error wins; with one the missing user is reported.
        assert!(matches!(err.kind(), "driverUnavailable" | "authentication"));
    }

    #[test]
    fn diagnostics_report_the_same_state_as_the_descriptor() {
        let d = client_diagnostics();
        let status = OracleDriver::new().descriptor().status;
        assert_eq!(d.available, status == DriverStatus::Available);
        assert_eq!(client_probe().is_ok(), d.available);
    }
}
