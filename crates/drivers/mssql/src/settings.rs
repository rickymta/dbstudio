//! Connection settings: the TLS policy mapping and the free-form `options` of a configuration.
//!
//! Recognised option keys: `auth` (`sql`, `windows`, `integrated`), `application_name`,
//! `pool_max` (1-32, default 4), `max_lob_bytes`, `encrypt` (`strict` = TDS 8.0).

use std::time::Duration;

use dbstudio_core::{ConnectionConfig, Error, Result, Secret, TlsConfig, TlsMode};
use tiberius::{AuthMethod, Config, EncryptionLevel};

use crate::value::DEFAULT_MAX_LOB_BYTES;

/// Default connect timeout.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Default number of pooled connections per database.
pub const DEFAULT_POOL_MAX: usize = 4;
/// Connections reserved for catalog queries (tree, search) per database.
pub const CATALOG_POOL_MAX: usize = 2;

/// How the login is authenticated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthKind {
    /// SQL Server login.
    Sql,
    /// Windows account (NTLM), `DOMAIN\user`.
    Windows,
    /// Current OS identity (SSPI, Windows only).
    Integrated,
}

/// Everything needed to open connections, minus the database.
#[derive(Clone)]
pub struct Settings {
    pub host: String,
    pub port: u16,
    pub user: Option<String>,
    pub password: Option<String>,
    pub tls: TlsConfig,
    pub strict: bool,
    pub app_name: String,
    pub connect_timeout: Duration,
    pub auth: AuthKind,
    pub pool_max: usize,
    pub max_lob_bytes: usize,
    pub login_db: String,
}

impl std::fmt::Debug for Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Settings")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("tls", &self.tls.mode)
            .field("auth", &self.auth)
            .finish_non_exhaustive()
    }
}

impl Settings {
    /// Read the settings of `cfg`; `secret` is the password.
    pub fn from_config(cfg: &ConnectionConfig, secret: Option<&Secret>) -> Result<Self> {
        let opt = |k: &str| {
            cfg.options
                .get(k)
                .map(|v| v.trim())
                .filter(|v| !v.is_empty())
        };
        let auth = match opt("auth").map(str::to_ascii_lowercase).as_deref() {
            None | Some("sql") | Some("sql_server") => AuthKind::Sql,
            Some("windows") | Some("ntlm") => AuthKind::Windows,
            Some("integrated") | Some("sspi") => AuthKind::Integrated,
            Some(other) => {
                return Err(Error::Invalid(format!(
                    "invalid auth `{other}` (expected sql, windows or integrated)"
                )));
            }
        };
        let pool_max = match opt("pool_max") {
            None => DEFAULT_POOL_MAX,
            Some(v) => v
                .parse::<usize>()
                .ok()
                .filter(|n| (1..=32).contains(n))
                .ok_or_else(|| Error::Invalid(format!("invalid pool_max `{v}` (expected 1-32)")))?,
        };
        let max_lob_bytes = match opt("max_lob_bytes") {
            None => DEFAULT_MAX_LOB_BYTES,
            Some(v) => v
                .parse::<usize>()
                .ok()
                .filter(|n| *n >= 1)
                .ok_or_else(|| Error::Invalid(format!("invalid max_lob_bytes `{v}`")))?,
        };
        let strict = opt("encrypt").is_some_and(|v| v.eq_ignore_ascii_case("strict"));
        let user = cfg.login_user().map(str::to_string);
        if auth != AuthKind::Integrated && user.is_none() {
            return Err(Error::Authentication(
                "a user name is required for this login method".into(),
            ));
        }
        Ok(Settings {
            host: cfg.host.clone(),
            port: cfg.port,
            user,
            password: cfg.effective_secret(secret).map(|s| s.expose().to_string()),
            tls: cfg.tls.clone(),
            strict,
            app_name: opt("application_name")
                .or_else(|| opt("applicationName"))
                .unwrap_or("DBStudio")
                .to_string(),
            connect_timeout: cfg
                .connect_timeout_ms
                .map(Duration::from_millis)
                .unwrap_or(DEFAULT_CONNECT_TIMEOUT),
            auth,
            pool_max,
            max_lob_bytes,
            login_db: cfg
                .database
                .clone()
                .filter(|d| !d.is_empty())
                .unwrap_or_else(|| "master".to_string()),
        })
    }

    /// Warning for the UI when NTLM credentials go over TLS whose certificate is not verified.
    pub fn ntlm_warning(&self) -> Option<String> {
        (self.auth == AuthKind::Windows
            && !matches!(self.tls.mode, TlsMode::VerifyCa | TlsMode::VerifyFull))
        .then(|| {
            "Windows (NTLM) credentials are sent over TLS without verifying the server \
             certificate; use verify-ca or verify-full"
                .to_string()
        })
    }

    /// The `tiberius` configuration for a connection to `database`.
    ///
    /// TLS policy: `Disable` encrypts only the login packet (everything when the server forces
    /// encryption), certificate unverified; `Prefer`
    /// and `Require` encrypt without verifying the certificate (TDS has no fallback to plain
    /// text once the server asks for encryption); `VerifyCa` and `VerifyFull` both verify chain
    /// and host name (the TLS library cannot skip the name check). Without a CA file the
    /// operating system's roots are used; a CA file is trusted in addition to them.
    pub fn tiberius_config(&self, database: &str) -> Result<Config> {
        let mut c = Config::new();
        c.host(&self.host);
        c.port(self.port);
        c.application_name(&self.app_name);
        // Statements may legitimately be silent for a long time (`WAITFOR`, big scans): the
        // time limit is the request's `timeout_ms`, not a per-round-trip bound.
        c.command_timeout(None);
        if !database.is_empty() {
            c.database(database);
        }
        let password = self.password.clone().unwrap_or_default();
        let user = self.user.clone().unwrap_or_default();
        match self.auth {
            AuthKind::Sql => c.authentication(AuthMethod::sql_server(user, password)),
            AuthKind::Windows => c.authentication(AuthMethod::windows(user, password)),
            AuthKind::Integrated => {
                #[cfg(windows)]
                c.authentication(AuthMethod::Integrated);
                #[cfg(not(windows))]
                return Err(Error::Unsupported("mssql_integrated_auth".into()));
            }
        }
        self.apply_tls(&mut c)?;
        Ok(c)
    }

    fn apply_tls(&self, c: &mut Config) -> Result<()> {
        let tls = &self.tls;
        if self.strict {
            c.encryption(EncryptionLevel::Strict);
        } else {
            c.encryption(match tls.mode {
                TlsMode::Disable => EncryptionLevel::Off,
                TlsMode::Prefer => EncryptionLevel::On,
                _ => EncryptionLevel::Required,
            });
        }
        match tls.mode {
            TlsMode::Disable | TlsMode::Prefer | TlsMode::Require => {
                // `Disable` asks for no encryption of the data, but the login is still encrypted
                // (and the whole connection when the server forces encryption): the certificate
                // is not verified in that case either.
                c.trust_cert();
            }
            TlsMode::VerifyCa | TlsMode::VerifyFull => {
                if let Some(ca) = &tls.ca_cert_path {
                    check_readable("CA certificate", ca)?;
                    c.trust_cert_ca(ca);
                }
                if let Some(name) = &tls.server_name {
                    c.hostname_in_certificate(name);
                }
            }
        }
        if tls.mode == TlsMode::Disable {
            return Ok(());
        }
        match (&tls.client_cert_path, &tls.client_key_path) {
            (Some(cert), Some(key)) => {
                check_readable("client certificate", cert)?;
                check_readable("client key", key)?;
                c.client_certificate(cert, key);
            }
            (None, None) => {}
            _ => {
                return Err(Error::Tls(
                    "client certificate and client key must be configured together".into(),
                ));
            }
        }
        Ok(())
    }
}

fn check_readable(kind: &str, path: &str) -> Result<()> {
    match std::fs::metadata(path) {
        Ok(m) if m.is_file() => Ok(()),
        Ok(_) => Err(Error::Tls(format!("{kind} `{path}` is not a file"))),
        Err(e) => Err(Error::Tls(format!("cannot read {kind} `{path}`: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_core::DbKind;

    fn cfg() -> ConnectionConfig {
        let mut c = ConnectionConfig::new(DbKind::SqlServer, "t", "localhost");
        c.username = Some("sa".into());
        c
    }

    #[test]
    fn options_are_parsed() {
        let mut c = cfg();
        let s = Settings::from_config(&c, None).unwrap();
        assert_eq!((s.pool_max, s.auth), (DEFAULT_POOL_MAX, AuthKind::Sql));
        assert_eq!(s.login_db, "master");
        c.options.insert("auth".into(), "windows".into());
        c.options.insert("pool_max".into(), "7".into());
        c.options.insert("application_name".into(), "app".into());
        c.database = Some("shop".into());
        let s = Settings::from_config(&c, Some(&Secret::new("pw"))).unwrap();
        assert_eq!((s.pool_max, s.auth), (7, AuthKind::Windows));
        assert_eq!((s.app_name.as_str(), s.login_db.as_str()), ("app", "shop"));
        c.options.insert("pool_max".into(), "0".into());
        assert!(Settings::from_config(&c, None).is_err());
        c.options.insert("pool_max".into(), "2".into());
        c.options.insert("auth".into(), "kerberos".into());
        assert!(Settings::from_config(&c, None).is_err());
    }

    #[test]
    fn user_is_required_except_integrated() {
        let mut c = cfg();
        c.username = None;
        assert_eq!(
            Settings::from_config(&c, None).unwrap_err().kind(),
            "authentication"
        );
        c.options.insert("auth".into(), "integrated".into());
        assert!(Settings::from_config(&c, None).is_ok());
    }

    #[test]
    fn tls_modes_map_to_encryption() {
        let mut c = cfg();
        for (mode, level) in [
            (TlsMode::Disable, "Off"),
            (TlsMode::Prefer, "On"),
            (TlsMode::Require, "Required"),
            (TlsMode::VerifyFull, "Required"),
        ] {
            c.tls.mode = mode;
            let s = Settings::from_config(&c, None).unwrap();
            let t = s.tiberius_config("db").unwrap();
            let dbg = format!("{t:?}");
            assert!(dbg.contains(level), "{mode:?}: {dbg}");
        }
    }

    #[test]
    fn ntlm_over_unverified_tls_warns() {
        let mut c = cfg();
        c.options.insert("auth".into(), "windows".into());
        for (mode, warns) in [
            (TlsMode::Disable, true),
            (TlsMode::Prefer, true),
            (TlsMode::Require, true),
            (TlsMode::VerifyCa, false),
            (TlsMode::VerifyFull, false),
        ] {
            c.tls.mode = mode;
            let s = Settings::from_config(&c, None).unwrap();
            assert_eq!(s.ntlm_warning().is_some(), warns, "{mode:?}");
        }
        c.options.insert("auth".into(), "sql".into());
        c.tls.mode = TlsMode::Require;
        assert!(
            Settings::from_config(&c, None)
                .unwrap()
                .ntlm_warning()
                .is_none()
        );
    }

    #[test]
    fn missing_ca_file_is_a_tls_error() {
        let mut c = cfg();
        c.tls.mode = TlsMode::VerifyCa;
        c.tls.ca_cert_path = Some("/definitely/not/here.pem".into());
        let s = Settings::from_config(&c, None).unwrap();
        assert_eq!(s.tiberius_config("x").unwrap_err().kind(), "tls");
        c.tls.ca_cert_path = None;
        c.tls.mode = TlsMode::Require;
        c.tls.client_cert_path = Some("/x".into());
        let s = Settings::from_config(&c, None).unwrap();
        assert_eq!(s.tiberius_config("x").unwrap_err().kind(), "tls");
    }

    #[cfg(not(windows))]
    #[test]
    fn integrated_is_windows_only() {
        let mut c = cfg();
        c.options.insert("auth".into(), "integrated".into());
        let s = Settings::from_config(&c, None).unwrap();
        assert_eq!(s.tiberius_config("x").unwrap_err().kind(), "unsupported");
    }
}
