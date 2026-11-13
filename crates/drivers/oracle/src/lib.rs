//! Oracle driver for DBStudio (`oracle` crate on top of ODPI-C).
//!
//! ODPI-C is compiled into the crate but loads the Oracle Client (`libclntsh`) at run time, so a
//! machine without Instant Client still starts: [`OracleDriver::descriptor`] probes once and
//! reports `Unavailable { NativeClientMissing }`.
//!
//! Scaffold status: `connect` returns `DriverUnavailable` (with a hint) when the client is
//! missing, otherwise `Unsupported`. Package J implements it.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use dbstudio_core::{
    Capabilities, ConnectionConfig, DbKind, Driver, DriverDescriptor, DriverRegistry, DriverStatus,
    Error, Result, Secret, Session, UnavailableReason,
};

/// Result of the one-time Oracle Client probe: `Ok(version)` or the error text.
static CLIENT_PROBE: OnceLock<std::result::Result<String, String>> = OnceLock::new();

/// Load (once) the Oracle Client and report its version or why it is missing.
pub fn client_probe() -> &'static std::result::Result<String, String> {
    CLIENT_PROBE.get_or_init(|| match oracle::Version::client() {
        Ok(v) => Ok(v.to_string()),
        Err(e) => Err(e.to_string()),
    })
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
        let status = match client_probe() {
            Ok(_) => DriverStatus::Available,
            Err(_) => DriverStatus::Unavailable {
                reason: UnavailableReason::NativeClientMissing,
            },
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
        let _ = (config, secret);
        match client_probe() {
            Err(reason) => Err(Error::DriverUnavailable(format!(
                "Oracle Client libraries (Instant Client) were not found: {reason}. \
                 Install Oracle Instant Client and set its directory in the settings."
            ))),
            Ok(_) => Err(Error::Unsupported("oracle".into())),
        }
    }
}

/// Register the driver in `registry`.
pub fn register(registry: &mut DriverRegistry) {
    registry.register(Arc::new(OracleDriver::new()));
}

#[cfg(test)]
mod tests {
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
    async fn connect_reports_unavailable_or_unsupported() {
        let cfg = ConnectionConfig::new(DbKind::Oracle, "test", "localhost");
        let err = OracleDriver::new().connect(&cfg, None).await.err().unwrap();
        match OracleDriver::new().descriptor().status {
            DriverStatus::Available => assert_eq!(err.kind(), "unsupported"),
            _ => assert_eq!(err.kind(), "driverUnavailable"),
        }
    }
}
