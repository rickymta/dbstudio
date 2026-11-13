//! Building `mysql_async` options from a [`ConnectionConfig`], including the TLS modes.
//!
//! `Require` never verifies the certificate (even with a CA file); `Prefer`/`Disable` are open to
//! man-in-the-middle attacks; only `VerifyCa`/`VerifyFull` authenticate the server. Without a CA
//! file the bundled webpki-roots set is used, not the OS store. See the crate docs for the
//! `VerifyCa` host-name limitation.

use std::path::PathBuf;
use std::time::Duration;

use dbstudio_core::{ConnectionConfig, Error, Result, Secret, TlsConfig, TlsMode};
use mysql_async::{ClientIdentity, Opts, OptsBuilder, PoolConstraints, PoolOpts, SslOpts};

/// Default connect timeout when the configuration does not set one.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Default upper bound of pooled connections.
pub const DEFAULT_POOL_MAX: usize = 8;

/// Settings derived from the free-form `options` map of a configuration.
///
/// Recognised keys: `pool_max` (1-32, default 8) and `init_command` (a statement executed on
/// every new connection, e.g. `SET time_zone = '+00:00'`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tunables {
    /// Maximum number of pooled connections.
    pub pool_max: usize,
    /// Extra statement run on each new connection.
    pub init_command: Option<String>,
}

impl Tunables {
    /// Parse the `options` map; unknown keys are ignored.
    pub fn from_config(cfg: &ConnectionConfig) -> Result<Self> {
        let pool_max = match cfg.options.get("pool_max") {
            None => DEFAULT_POOL_MAX,
            Some(v) => v
                .trim()
                .parse::<usize>()
                .ok()
                .filter(|n| (1..=32).contains(n))
                .ok_or_else(|| Error::Other(format!("invalid pool_max `{v}` (expected 1-32)")))?,
        };
        let init_command = cfg
            .options
            .get("init_command")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        Ok(Tunables {
            pool_max,
            init_command,
        })
    }
}

/// Connect timeout of a configuration.
pub fn connect_timeout(cfg: &ConnectionConfig) -> Duration {
    cfg.connect_timeout_ms
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_CONNECT_TIMEOUT)
}

fn check_readable(kind: &str, path: &str) -> Result<PathBuf> {
    std::fs::metadata(path)
        .and_then(|m| {
            if m.is_file() {
                Ok(())
            } else {
                Err(std::io::Error::other("not a file"))
            }
        })
        .map_err(|e| Error::Tls(format!("cannot read {kind} `{path}`: {e}")))?;
    Ok(PathBuf::from(path))
}

/// `SslOpts` for `tls`, or `None` for [`TlsMode::Disable`].
///
/// Without a CA file the built-in Mozilla root store (webpki-roots) is used for the two verifying
/// modes; with a CA file only that CA is trusted.
pub fn ssl_opts(tls: &TlsConfig) -> Result<Option<SslOpts>> {
    if tls.mode == TlsMode::Disable {
        return Ok(None);
    }
    let mut ssl = SslOpts::default();
    match (&tls.client_cert_path, &tls.client_key_path) {
        (Some(cert), Some(key)) => {
            let cert = check_readable("client certificate", cert)?;
            let key = check_readable("client key", key)?;
            ssl = ssl.with_client_identity(Some(ClientIdentity::new(cert.into(), key.into())));
        }
        (None, None) => {}
        _ => {
            return Err(Error::Tls(
                "client certificate and client key must be configured together".into(),
            ));
        }
    }
    match tls.mode {
        TlsMode::Disable => unreachable!("handled above"),
        TlsMode::Prefer | TlsMode::Require => {
            ssl = ssl
                .with_danger_accept_invalid_certs(true)
                .with_danger_skip_domain_validation(true);
        }
        TlsMode::VerifyCa | TlsMode::VerifyFull => {
            if let Some(ca) = &tls.ca_cert_path {
                let ca = check_readable("CA certificate", ca)?;
                ssl = ssl
                    .with_root_certs(vec![ca.into()])
                    .with_disable_built_in_roots(true);
            }
            if tls.mode == TlsMode::VerifyCa {
                ssl = ssl.with_danger_skip_domain_validation(true);
            }
        }
    }
    Ok(Some(ssl))
}

/// Install the process-wide rustls crypto provider (`ring`) if none is installed yet.
pub fn ensure_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Build the full connection options. `ssl` is passed separately so `Prefer` can retry without.
pub fn build_opts(
    cfg: &ConnectionConfig,
    secret: Option<&Secret>,
    ssl: Option<SslOpts>,
    tunables: &Tunables,
) -> Result<Opts> {
    let max = tunables.pool_max;
    let constraints = PoolConstraints::new(1, max)
        .ok_or_else(|| Error::Other("invalid pool constraints".into()))?;
    // `setup` commands run on every new connection *and* after every pool reset (which wipes
    // session state), unlike `init`. mysql_async pops them from the end, so reverse the order.
    let mut setup: Vec<String> = Vec::new();
    if cfg.read_only {
        // Best effort server side guard; fetch_table additionally uses READ ONLY transactions.
        setup.push("SET SESSION TRANSACTION READ ONLY".into());
    }
    if let Some(cmd) = &tunables.init_command {
        setup.push(cmd.clone());
    }
    let builder = OptsBuilder::default()
        .ip_or_hostname(cfg.host.clone())
        .tcp_port(cfg.port)
        .user(cfg.username.clone())
        .pass(secret.map(|s| s.expose().to_string()))
        .db_name(cfg.database.clone().filter(|d| !d.is_empty()))
        .ssl_opts(ssl)
        .prefer_socket(false)
        .tcp_nodelay(true)
        .setup(setup.into_iter().rev().collect::<Vec<_>>())
        .pool_opts(
            PoolOpts::default()
                .with_constraints(constraints)
                .with_inactive_connection_ttl(Duration::from_secs(300)),
        );
    Ok(builder.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_core::DbKind;

    fn cfg() -> ConnectionConfig {
        ConnectionConfig::new(DbKind::MySql, "t", "localhost")
    }

    #[test]
    fn disable_has_no_ssl() {
        let tls = TlsConfig {
            mode: TlsMode::Disable,
            ..Default::default()
        };
        assert!(ssl_opts(&tls).unwrap().is_none());
    }

    #[test]
    fn modes_configure_verification() {
        let mk = |mode| {
            ssl_opts(&TlsConfig {
                mode,
                ..Default::default()
            })
            .unwrap()
            .unwrap()
        };
        assert!(mk(TlsMode::Prefer).accept_invalid_certs());
        assert!(mk(TlsMode::Require).accept_invalid_certs());
        let ca = mk(TlsMode::VerifyCa);
        assert!(!ca.accept_invalid_certs() && ca.skip_domain_validation());
        let full = mk(TlsMode::VerifyFull);
        assert!(!full.accept_invalid_certs() && !full.skip_domain_validation());
    }

    #[test]
    fn missing_files_are_tls_errors() {
        let tls = TlsConfig {
            mode: TlsMode::VerifyFull,
            ca_cert_path: Some("/definitely/not/here.pem".into()),
            ..Default::default()
        };
        assert!(matches!(ssl_opts(&tls), Err(Error::Tls(_))));
        let tls = TlsConfig {
            mode: TlsMode::Require,
            client_cert_path: Some("/x".into()),
            ..Default::default()
        };
        assert!(matches!(ssl_opts(&tls), Err(Error::Tls(_))));
    }

    #[test]
    fn tunables_parse() {
        let mut c = cfg();
        assert_eq!(
            Tunables::from_config(&c).unwrap().pool_max,
            DEFAULT_POOL_MAX
        );
        c.options.insert("pool_max".into(), "3".into());
        c.options.insert("init_command".into(), " SET x=1 ".into());
        let t = Tunables::from_config(&c).unwrap();
        assert_eq!(
            (t.pool_max, t.init_command.as_deref()),
            (3, Some("SET x=1"))
        );
        c.options.insert("pool_max".into(), "0".into());
        assert!(Tunables::from_config(&c).is_err());
    }

    #[test]
    fn opts_do_not_panic() {
        let o = build_opts(
            &cfg(),
            Some(&Secret::new("pw")),
            None,
            &Tunables::from_config(&cfg()).unwrap(),
        )
        .unwrap();
        assert_eq!(o.ip_or_hostname(), "localhost");
        assert_eq!(o.pass(), Some("pw"));
    }
}
