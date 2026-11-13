use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::DbKind;

/// TLS policy for a connection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TlsMode {
    /// Never use TLS.
    Disable,
    /// Use TLS when the server offers it.
    #[default]
    Prefer,
    /// Require TLS but do not verify the certificate chain.
    Require,
    /// Require TLS and verify certificate chain and host name.
    VerifyFull,
}

/// Deployment environment label, used for visual safeguards (e.g. red production tabs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Environment {
    /// Local or development server.
    Development,
    /// Pre-production.
    Staging,
    /// Live data; destructive operations deserve extra confirmation.
    Production,
}

/// A saved connection definition. It never contains a password: pass a [`Secret`] at connect time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionConfig {
    /// Stable identifier.
    pub id: Uuid,
    /// User visible name.
    pub name: String,
    /// Database product.
    pub kind: DbKind,
    /// Host name or address.
    pub host: String,
    /// TCP port.
    pub port: u16,
    /// Initial database, keyspace or service name.
    pub database: Option<String>,
    /// Login user.
    pub username: Option<String>,
    /// TLS policy.
    pub tls: TlsMode,
    /// Refuse statements that modify data or schema.
    pub read_only: bool,
    /// Optional environment label.
    pub environment: Option<Environment>,
    /// Driver specific parameters.
    #[serde(default)]
    pub options: BTreeMap<String, String>,
}

impl ConnectionConfig {
    /// Create a config with a fresh id and the default port of `kind`.
    pub fn new(kind: DbKind, name: impl Into<String>, host: impl Into<String>) -> Self {
        ConnectionConfig {
            id: Uuid::new_v4(),
            name: name.into(),
            kind,
            host: host.into(),
            port: kind.default_port(),
            database: None,
            username: None,
            tls: TlsMode::default(),
            read_only: false,
            environment: None,
            options: BTreeMap::new(),
        }
    }
}

/// A password or token. `Debug` never prints the content and the type is not `Serialize`.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    /// Wrap a secret string.
    pub fn new(value: impl Into<String>) -> Self {
        Secret(value.into())
    }

    /// Access the underlying value. Use sparingly and never log it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

impl From<String> for Secret {
    fn from(value: String) -> Self {
        Secret(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_is_not_leaked_by_debug() {
        let s = Secret::new("hunter2");
        assert_eq!(format!("{s:?}"), "***");
        assert!(!format!("{:?}", Some(&s)).contains("hunter2"));
        assert_eq!(s.expose(), "hunter2");
    }

    #[test]
    fn config_roundtrip_has_no_password_field() {
        let mut cfg = ConnectionConfig::new(DbKind::Postgres, "local", "localhost");
        cfg.environment = Some(Environment::Staging);
        let json = serde_json::to_value(&cfg).unwrap();
        assert_eq!(json["port"], 5432);
        assert_eq!(json["readOnly"], false);
        assert_eq!(json["kind"], "postgres");
        assert!(json.get("password").is_none());
        let back: ConnectionConfig = serde_json::from_value(json).unwrap();
        assert_eq!(back, cfg);
    }
}
