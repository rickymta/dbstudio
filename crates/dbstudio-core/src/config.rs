use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{DbKind, SshConfig};

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
    /// Require TLS and verify the certificate chain against the CA, but not the host name.
    VerifyCa,
    /// Require TLS and verify certificate chain and host name.
    VerifyFull,
}

/// TLS settings of a connection: the policy plus optional certificate files (paths on disk).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TlsConfig {
    /// TLS policy.
    pub mode: TlsMode,
    /// PEM file with the CA certificate(s) used to verify the server.
    #[serde(default)]
    pub ca_cert_path: Option<String>,
    /// PEM file with the client certificate (mutual TLS).
    #[serde(default)]
    pub client_cert_path: Option<String>,
    /// PEM file with the client private key (mutual TLS).
    #[serde(default)]
    pub client_key_path: Option<String>,
    /// SNI and certificate-name override (set to the original host when connecting through an
    /// SSH tunnel to `127.0.0.1`).
    #[serde(default)]
    pub server_name: Option<String>,
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

/// How a connection authenticates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthMode {
    /// Log in with a user name and password (the password may be stored or asked each time).
    #[default]
    #[serde(rename = "password")]
    Password,
    /// The server needs no authentication: no password is ever asked, stored or sent.
    #[serde(rename = "none")]
    NoAuth,
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
    /// How the connection authenticates; older profiles without the field use `password`.
    #[serde(default)]
    pub auth_mode: AuthMode,
    /// TLS settings.
    pub tls: TlsConfig,
    /// Connect timeout in milliseconds; the driver picks a default when `None`.
    #[serde(default)]
    pub connect_timeout_ms: Option<u64>,
    /// Refuse statements that modify data or schema.
    pub read_only: bool,
    /// Optional environment label.
    pub environment: Option<Environment>,
    /// Driver specific parameters.
    #[serde(default)]
    pub options: BTreeMap<String, String>,
    /// Optional SSH tunnel. The secret (password / key passphrase) lives in the keychain under
    /// `<connId>#ssh`.
    #[serde(default)]
    pub ssh: Option<SshConfig>,
    /// Custom environment label shown instead of the standard name (at most 32 characters).
    /// Safety rules still follow `environment` (production = confirmations).
    #[serde(default)]
    pub environment_label: Option<String>,
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
            auth_mode: AuthMode::Password,
            tls: TlsConfig::default(),
            connect_timeout_ms: None,
            read_only: false,
            environment: None,
            options: BTreeMap::new(),
            ssh: None,
            environment_label: None,
        }
    }

    /// Whether the profile is set to connect without authentication.
    pub fn is_no_auth(&self) -> bool {
        self.auth_mode == AuthMode::NoAuth
    }

    /// The secret to hand to a driver: always `None` when the profile needs no authentication,
    /// even if the caller supplied one (for instance a stale keychain entry).
    pub fn effective_secret<'a>(&self, secret: Option<&'a Secret>) -> Option<&'a Secret> {
        if self.is_no_auth() { None } else { secret }
    }

    /// The user name, if any: empty or blank names count as none. Drivers fall back to their
    /// own default user (PostgreSQL `postgres`, ClickHouse `default`, ...).
    pub fn login_user(&self) -> Option<&str> {
        self.username.as_deref().filter(|u| !u.trim().is_empty())
    }

    /// The user name for drivers whose servers cannot log in without a password (MongoDB,
    /// Redis, Cassandra): none when the profile needs no authentication.
    pub fn credential_user(&self) -> Option<&str> {
        if self.is_no_auth() {
            None
        } else {
            self.login_user()
        }
    }

    /// The servers to try, in order: `host:port` first, then the extra nodes of `options.hosts`
    /// (`host[:port]` separated by commas, spaces or new lines; `[v6]:port` for IPv6; a missing
    /// port is `port`). Duplicates are dropped. Behind an SSH tunnel only the first one is
    /// reachable (the tunnel forwards a single address), so the extra nodes are ignored.
    pub fn nodes(&self) -> Vec<(String, u16)> {
        if self.ssh.is_some() {
            return vec![(self.host.clone(), self.port)];
        }
        self.all_nodes()
    }

    /// Like [`ConnectionConfig::nodes`] but also behind an SSH tunnel, for a driver whose
    /// tunnel reaches every server (MongoDB goes through a SOCKS5 proxy).
    pub fn all_nodes(&self) -> Vec<(String, u16)> {
        let mut out = vec![(self.host.clone(), self.port)];
        for item in self
            .options
            .get(HOSTS_OPTION)
            .map(String::as_str)
            .unwrap_or("")
            .split(|c: char| c == ',' || c.is_whitespace())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if let Some(node) = parse_node(item, self.port) {
                if !out
                    .iter()
                    .any(|n| n.0.eq_ignore_ascii_case(&node.0) && n.1 == node.1)
                {
                    out.push(node);
                }
            }
        }
        out
    }

    /// `options.topology` (`standalone`, `cluster`, `sentinel`, …), lower-cased; empty when
    /// not set. Drivers decide which values they understand.
    pub fn topology(&self) -> String {
        self.options
            .get(TOPOLOGY_OPTION)
            .map(|t| t.trim().to_ascii_lowercase())
            .unwrap_or_default()
    }
}

/// Option holding the extra cluster nodes of a profile (see [`ConnectionConfig::nodes`]).
pub const HOSTS_OPTION: &str = "hosts";
/// Option naming how the servers of a profile form a cluster (see [`ConnectionConfig::topology`]).
pub const TOPOLOGY_OPTION: &str = "topology";

/// `host`, `host:port`, `[v6]` or `[v6]:port`; `None` for an unusable entry.
fn parse_node(item: &str, default_port: u16) -> Option<(String, u16)> {
    if let Some(rest) = item.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        let port = match tail.strip_prefix(':') {
            Some(p) => p.parse().ok()?,
            None if tail.is_empty() => default_port,
            None => return None,
        };
        return (!host.is_empty()).then(|| (host.to_string(), port));
    }
    match item.rsplit_once(':') {
        // A bare IPv6 address has several colons: take it whole.
        Some((host, port)) if !host.contains(':') => {
            let port = port.parse().ok()?;
            (!host.is_empty()).then(|| (host.to_string(), port))
        }
        _ => Some((item.to_string(), default_port)),
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
    #[test]
    fn nodes_list_the_main_host_first_then_the_extra_ones() {
        let mut c = ConnectionConfig::new(DbKind::Postgres, "pg", "db1");
        assert_eq!(c.nodes(), vec![("db1".to_string(), 5432)]);
        c.options.insert(
            "hosts".into(),
            "db2:5433, db1
[fe80::1]:6000 [::1] fe80::2  bad:port"
                .into(),
        );
        assert_eq!(
            c.nodes(),
            vec![
                ("db1".to_string(), 5432),
                ("db2".to_string(), 5433),
                ("fe80::1".to_string(), 6000),
                ("::1".to_string(), 5432),
                ("fe80::2".to_string(), 5432),
            ]
        );
        c.options.insert("topology".into(), " Cluster ".into());
        assert_eq!(c.topology(), "cluster");
        // Behind an SSH tunnel only the tunnelled address is reachable (unless the driver's
        // tunnel reaches them all).
        assert_eq!(c.all_nodes().len(), 5);
        c.ssh = Some(serde_json::from_value(serde_json::json!({"host": "bastion", "username": "u", "auth": {"method": "agent"}})).unwrap());
        assert_eq!(c.nodes().len(), 1);
    }

    #[test]
    fn auth_mode_defaults_to_password_for_old_profiles() {
        let mut v =
            serde_json::to_value(ConnectionConfig::new(DbKind::Redis, "r", "localhost")).unwrap();
        assert_eq!(v["authMode"], "password");
        v.as_object_mut().unwrap().remove("authMode");
        let old: ConnectionConfig = serde_json::from_value(v).unwrap();
        assert_eq!(old.auth_mode, AuthMode::Password);
        assert!(!old.is_no_auth());
    }

    #[test]
    fn no_auth_drops_secret_and_credential_user() {
        let mut c = ConnectionConfig::new(DbKind::MongoDb, "m", "localhost");
        c.username = Some("admin".into());
        let pw = Secret::new("pw");
        assert!(c.effective_secret(Some(&pw)).is_some());
        assert_eq!(c.credential_user(), Some("admin"));
        c.auth_mode = AuthMode::NoAuth;
        assert!(c.effective_secret(Some(&pw)).is_none());
        assert_eq!(c.credential_user(), None);
        // Drivers whose server may need only a user name still see it.
        assert_eq!(c.login_user(), Some("admin"));
        c.username = Some("  ".into());
        assert_eq!(c.login_user(), None);
        let v = serde_json::to_value(&c).unwrap();
        assert_eq!(v["authMode"], "none");
    }

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
        assert_eq!(json["tls"]["mode"], "prefer");
        assert!(json["connectTimeoutMs"].is_null());
        let back: ConnectionConfig = serde_json::from_value(json).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn tls_config_serializes_camel_case() {
        let tls = TlsConfig {
            mode: TlsMode::VerifyCa,
            ca_cert_path: Some("/ca.pem".into()),
            ..TlsConfig::default()
        };
        let json = serde_json::to_value(&tls).unwrap();
        assert_eq!(json["mode"], "verifyCa");
        assert_eq!(json["caCertPath"], "/ca.pem");
        assert!(json["clientKeyPath"].is_null());
        // Optional fields may be omitted by the frontend.
        let back: TlsConfig =
            serde_json::from_value(serde_json::json!({"mode": "disable"})).unwrap();
        assert_eq!(back.mode, TlsMode::Disable);
    }

    #[test]
    fn ssh_and_server_name_are_optional_on_input() {
        let json = serde_json::json!({
            "id": "00000000-0000-0000-0000-000000000001", "name": "n", "kind": "postgres",
            "host": "h", "port": 5432, "database": null, "username": null,
            "tls": {"mode": "prefer"}, "readOnly": false, "environment": null
        });
        let cfg: ConnectionConfig = serde_json::from_value(json).unwrap();
        assert!(cfg.ssh.is_none());
        assert!(cfg.environment_label.is_none());
        assert!(cfg.tls.server_name.is_none());
        let mut cfg = cfg;
        cfg.tls.server_name = Some("db.internal".into());
        cfg.ssh = Some(crate::SshConfig {
            host: "bastion".into(),
            port: 22,
            username: "me".into(),
            auth: crate::SshAuth::Agent,
            keepalive_secs: None,
            connect_timeout_ms: None,
        });
        let json = serde_json::to_value(&cfg).unwrap();
        assert_eq!(json["tls"]["serverName"], "db.internal");
        assert_eq!(json["ssh"]["auth"], serde_json::json!({"method": "agent"}));
        assert_eq!(
            serde_json::from_value::<ConnectionConfig>(json).unwrap(),
            cfg
        );
    }
}
