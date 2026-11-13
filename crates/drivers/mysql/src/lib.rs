//! MySQL / MariaDB driver for DBStudio.
//!
//! Supports MySQL 5.7+/8.x and MariaDB 10.4+ through [`mysql_async`] with TLS provided by
//! rustls (`ring` provider). A [`Session`](dbstudio_core::Session) wraps a small connection pool;
//! see the `session` module docs for the streaming and cancellation design and the `value`
//! module docs for how column types map to [`dbstudio_core::Value`].
//!
//! # Security notes
//!
//! * `Disable` and `Prefer` do not protect against an active man-in-the-middle (`Prefer` even
//!   falls back to plain text); without TLS, `caching_sha2_password` may request the server's
//!   RSA public key over the same unauthenticated channel. Only `VerifyCa` and `VerifyFull`
//!   authenticate the server.
//! * `Require` encrypts but never verifies the certificate, even when `ca_cert_path` is set.
//! * Without a CA file the verifying modes use the bundled Mozilla root set (`webpki-roots`),
//!   NOT the operating system's certificate store.
//! * Known limitation: `VerifyCa` should skip the host-name check, but `mysql_async` detects a
//!   name mismatch by matching an old rustls error text that rustls 0.23 no longer produces, so
//!   `VerifyCa` currently behaves like `VerifyFull` (stricter, never looser). A clear error
//!   message is returned when the host is not in the certificate.
//! * `Contains` / `Eq` style filters compare with the connection collation (case-insensitive
//!   for the default `utf8mb4` collations).
//! * `raw_where` is user-trusted SQL, but must be a single expression: a lexer rejects `;` and
//!   unbalanced parentheses and the server confirms the statement with `COM_STMT_PREPARE`.
//!
//! Metadata queries avoid 8.x-only features so they should also work on 5.7, but this has not
//! been tested against a real 5.7 server.

mod browse;
mod connect;
mod error;
mod session;
mod sql;
mod value;

use async_trait::async_trait;
use dbstudio_core::{
    Capabilities, ConnectionConfig, DbKind, Driver, DriverDescriptor, DriverStatus, Error, Result,
    Secret, Session, TlsMode,
};
use mysql_async::{Conn, Pool};

pub use session::MySqlSession;

/// True when a `VERSION()` string identifies MariaDB.
pub(crate) fn detect_mariadb(version: &str) -> bool {
    version.to_ascii_lowercase().contains("mariadb")
}

/// Strip the `5.5.5-` replication-compat prefix MariaDB prepends to its version string.
pub(crate) fn normalize_version(raw: &str) -> String {
    match raw.strip_prefix("5.5.5-") {
        Some(rest) if detect_mariadb(raw) => rest.to_string(),
        _ => raw.to_string(),
    }
}

/// Driver for MySQL and MariaDB servers.
#[derive(Debug, Default, Clone, Copy)]
pub struct MySqlDriver;

impl MySqlDriver {
    /// Create the driver.
    pub fn new() -> Self {
        Self
    }
}

async fn try_connect(
    cfg: &ConnectionConfig,
    secret: Option<&Secret>,
    ssl: Option<mysql_async::SslOpts>,
    tunables: &connect::Tunables,
) -> std::result::Result<(Pool, mysql_async::Opts, Conn), TryError> {
    let opts = connect::build_opts(cfg, secret, ssl, tunables).map_err(TryError::Core)?;
    let pool = Pool::new(opts.clone());
    let timeout = connect::connect_timeout(cfg);
    match tokio::time::timeout(timeout, pool.get_conn()).await {
        Err(_) => {
            let _ = pool.disconnect().await;
            Err(TryError::Core(Error::Timeout))
        }
        Ok(Err(e)) => {
            let _ = pool.disconnect().await;
            Err(TryError::My(e))
        }
        Ok(Ok(conn)) => Ok((pool, opts, conn)),
    }
}

enum TryError {
    Core(Error),
    My(mysql_async::Error),
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
        config: &ConnectionConfig,
        secret: Option<&Secret>,
    ) -> Result<Box<dyn Session>> {
        connect::ensure_crypto_provider();
        let tunables = connect::Tunables::from_config(config)?;
        let ssl = connect::ssl_opts(&config.tls)?;
        let secret_text = secret.map(|s| s.expose().to_string());

        let mut attempt = try_connect(config, secret, ssl, &tunables).await;
        // `Prefer`: a server without TLS (or a failed handshake) falls back to plain text.
        if config.tls.mode == TlsMode::Prefer {
            if let Err(TryError::My(e)) = &attempt {
                if error::is_tls_unavailable(e) {
                    attempt = try_connect(config, secret, None, &tunables).await;
                }
            }
        }
        let (pool, opts, mut conn) = match attempt {
            Ok(v) => v,
            Err(TryError::Core(e)) => return Err(e),
            Err(TryError::My(e)) => {
                let err = error::map_connect_error(e, secret_text.as_deref());
                return Err(match err {
                    Error::Tls(m) if m.contains("not valid for name") => Error::Tls(format!(
                        "{m}. The server certificate does not list host `{}`. Connect with a host \
                         name or address that appears in the certificate, or re-issue it with \
                         that name. Note: with this driver verify-ca also checks the host name \
                         (the TLS library cannot skip it).",
                        config.host
                    )),
                    other => other,
                });
            }
        };

        // Identify the product from the version string.
        let version: Option<String> =
            mysql_async::prelude::Queryable::query_first(&mut conn, "SELECT VERSION()")
                .await
                .map_err(|e| error::map_connect_error(e, secret_text.as_deref()))?;
        drop(conn);
        let is_mariadb = detect_mariadb(&version.unwrap_or_default());
        let inner = session::Inner::new(
            pool,
            opts,
            connect::connect_timeout(config),
            secret_text,
            is_mariadb,
            config.database.clone().filter(|d| !d.is_empty()),
        );
        Ok(Box::new(MySqlSession::new(inner)))
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

    #[test]
    fn mariadb_detection() {
        assert!(detect_mariadb(
            "5.5.5-10.6.18-MariaDB-1:10.6.18+maria~ubu2004"
        ));
        assert!(detect_mariadb("10.11.2-mariadb"));
        assert!(!detect_mariadb("8.4.0"));
        assert!(!detect_mariadb("5.7.44-log"));
        assert_eq!(
            normalize_version("5.5.5-10.6.18-MariaDB-1:10.6.18+maria~ubu2004"),
            "10.6.18-MariaDB-1:10.6.18+maria~ubu2004"
        );
        assert_eq!(normalize_version("8.4.0"), "8.4.0");
    }
}
