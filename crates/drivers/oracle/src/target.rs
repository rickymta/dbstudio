//! Turning a [`ConnectionConfig`] into what ODPI-C needs: user, password, connect string and
//! privilege.
//!
//! `config.database` is the service name (`FREEPDB1`), or a complete Easy Connect string
//! (`host:1521/service`, `tcps://host:2484/service?...`), or a TNS descriptor (`(DESCRIPTION=...)`).
//! Options understood (`config.options`):
//!
//! * `sid` - connect with this SID instead of a service name,
//! * `connect_as` - `sysdba` or `sysoper`,
//! * `wallet_dir` - directory with `cwallet.sso` / `ewallet.pem` (TLS and mTLS, Autonomous DB),
//! * `show_all_schemas` - `true` lists Oracle-maintained schemas in the tree too.
//!
//! TLS: `disable`/`prefer` use TCP; `require` uses TCPS without checking the server name;
//! `verifyCa`/`verifyFull` use TCPS and match the server certificate with the host name. A CA
//! certificate file (and client certificate + key for mutual TLS) is turned into a PEM wallet in a
//! temporary directory that lives as long as the session.

#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;

use dbstudio_core::{ConnectionConfig, Error, Result, Secret, TlsMode};
use oracle::Privilege;

/// Everything needed to open a physical connection.
pub struct Target {
    pub user: String,
    password: String,
    pub connect_string: String,
    pub privilege: Option<Privilege>,
    /// Keeps a generated PEM wallet alive (and deletes it on drop).
    _wallet: Option<TempWallet>,
    /// Connect timeout.
    pub timeout: std::time::Duration,
    /// Oracle-maintained schemas are shown in the tree.
    pub show_all_schemas: bool,
    /// Largest LOB loaded into a cell (bytes).
    pub max_lob_bytes: u64,
}

impl std::fmt::Debug for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Target")
            .field("user", &self.user)
            .field("password", &"***")
            .field("connect_string", &self.connect_string)
            .finish_non_exhaustive()
    }
}

impl Target {
    /// Password for `Connector::new` (never printed; `Debug` hides it).
    pub fn password(&self) -> &str {
        &self.password
    }

    /// Directory of the generated PEM wallet, if any.
    #[cfg(test)]
    pub fn wallet_dir(&self) -> Option<&Path> {
        self._wallet.as_ref().map(|w| w.dir.as_path())
    }
}

/// Default for `max_lob_bytes` (the `maxLobBytes` setting is not visible to the driver).
pub const DEFAULT_MAX_LOB_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug)]
struct TempWallet {
    dir: PathBuf,
}

impl Drop for TempWallet {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Build the [`Target`] of `config`.
pub fn build(config: &ConnectionConfig, secret: Option<&Secret>) -> Result<Target> {
    let user = config
        .username
        .clone()
        .filter(|u| !u.trim().is_empty())
        .ok_or_else(|| Error::Authentication("a user name is required".into()))?;
    let privilege = match config
        .options
        .get("connect_as")
        .map(|s| s.trim().to_ascii_lowercase())
        .as_deref()
    {
        None | Some("") | Some("normal") => None,
        Some("sysdba") => Some(Privilege::Sysdba),
        Some("sysoper") => Some(Privilege::Sysoper),
        Some(other) => {
            return Err(Error::Other(format!(
                "connect_as must be sysdba or sysoper, got \"{other}\""
            )));
        }
    };
    let wallet = write_pem_wallet(config)?;
    let wallet_dir = config
        .options
        .get("wallet_dir")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            wallet
                .as_ref()
                .map(|w| w.dir.to_string_lossy().into_owned())
        });
    let connect_string = connect_string(config, wallet_dir.as_deref())?;
    let max_lob_bytes = config
        .options
        .get("max_lob_bytes")
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_MAX_LOB_BYTES);
    Ok(Target {
        user,
        password: config
            .effective_secret(secret)
            .map(|s| s.expose().to_string())
            .unwrap_or_default(),
        connect_string,
        privilege,
        _wallet: wallet,
        timeout: std::time::Duration::from_millis(config.connect_timeout_ms.unwrap_or(15_000)),
        show_all_schemas: config.options.get("show_all_schemas").is_some_and(|v| {
            matches!(v.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes")
        }),
        max_lob_bytes,
    })
}

fn is_descriptor_or_url(s: &str) -> bool {
    s.starts_with('(') || s.contains("://")
}

/// Does `s` already name a host (`host:1521/service`, `host/service`)?
fn is_easy_connect(s: &str) -> bool {
    s.contains('/')
}

fn tls_needed(config: &ConnectionConfig, wallet_dir: Option<&str>) -> bool {
    matches!(
        config.tls.mode,
        TlsMode::Require | TlsMode::VerifyCa | TlsMode::VerifyFull
    ) || (wallet_dir.is_some() && !matches!(config.tls.mode, TlsMode::Disable))
}

/// The connect string of `config` (see the module documentation).
pub fn connect_string(config: &ConnectionConfig, wallet_dir: Option<&str>) -> Result<String> {
    let database = config
        .database
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty());
    let sid = config
        .options
        .get("sid")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty());
    // A complete string given by the user is used as is.
    if let Some(db) = database
        && (is_descriptor_or_url(db) || is_easy_connect(db))
    {
        return Ok(db.to_string());
    }
    let host = config.host.trim();
    if host.is_empty() {
        return Err(Error::Connection("the host is empty".into()));
    }
    let tls = tls_needed(config, wallet_dir);
    let protocol = if tls { "TCPS" } else { "TCP" };
    if let Some(sid) = sid {
        // The descriptor form is the only way to name a SID.
        let mut security = String::new();
        if tls {
            let dn_match = matches!(config.tls.mode, TlsMode::VerifyCa | TlsMode::VerifyFull);
            security = format!(
                "(SECURITY=(SSL_SERVER_DN_MATCH={}){})",
                if dn_match { "YES" } else { "NO" },
                wallet_dir
                    .map(|d| format!("(MY_WALLET_DIRECTORY={d})"))
                    .unwrap_or_default()
            );
        }
        return Ok(format!(
            "(DESCRIPTION=(ADDRESS=(PROTOCOL={protocol})(HOST={host})(PORT={}))\
             (CONNECT_DATA=(SID={sid})){security})",
            config.port
        ));
    }
    let service = database.ok_or_else(|| {
        Error::Connection("set the service name (or a SID in the options)".into())
    })?;
    // Host names with a colon (IPv6) need brackets.
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    let mut params: Vec<String> = Vec::new();
    if tls {
        let dn_match = matches!(config.tls.mode, TlsMode::VerifyCa | TlsMode::VerifyFull);
        params.push(format!(
            "ssl_server_dn_match={}",
            if dn_match { "on" } else { "off" }
        ));
        if let Some(dir) = wallet_dir {
            params.push(format!("wallet_location={}", url_component(dir)));
        }
        if let Some(name) = config
            .tls
            .server_name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            && dn_match
        {
            params.push(format!(
                "ssl_server_cert_dn={}",
                url_component(&format!("CN={name}"))
            ));
        }
    }
    if let Some(ms) = config.connect_timeout_ms {
        params.push(format!(
            "transport_connect_timeout={}",
            ms.div_ceil(1000).max(1)
        ));
    }
    let mut out = format!(
        "{}{host}:{}/{service}",
        if tls { "tcps://" } else { "" },
        config.port
    );
    if !params.is_empty() {
        out.push('?');
        out.push_str(&params.join("&"));
    }
    Ok(out)
}

/// Percent-encode the characters that would break an Easy Connect parameter value.
fn url_component(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            ' ' | '&' | '?' | '#' | '%' | '=' | '"' | '\'' | '(' | ')' => {
                let mut buf = [0u8; 4];
                for b in c.encode_utf8(&mut buf).bytes() {
                    out.push_str(&format!("%{b:02X}"));
                }
            }
            c => out.push(c),
        }
    }
    out
}

/// Write `ewallet.pem` for a CA certificate (and a client certificate + key for mutual TLS).
fn write_pem_wallet(config: &ConnectionConfig) -> Result<Option<TempWallet>> {
    if !matches!(
        config.tls.mode,
        TlsMode::Require | TlsMode::VerifyCa | TlsMode::VerifyFull
    ) {
        return Ok(None);
    }
    let tls = &config.tls;
    let read = |path: &str, what: &str| -> Result<String> {
        std::fs::read_to_string(path)
            .map_err(|e| Error::Tls(format!("cannot read the {what} \"{path}\": {e}")))
    };
    let mut pem = String::new();
    if let Some(key) = tls.client_key_path.as_deref().filter(|p| !p.is_empty()) {
        pem.push_str(&read(key, "client key")?);
        pem.push('\n');
    }
    if let Some(cert) = tls.client_cert_path.as_deref().filter(|p| !p.is_empty()) {
        pem.push_str(&read(cert, "client certificate")?);
        pem.push('\n');
    }
    if let Some(ca) = tls.ca_cert_path.as_deref().filter(|p| !p.is_empty()) {
        pem.push_str(&read(ca, "CA certificate")?);
        pem.push('\n');
    }
    if pem.trim().is_empty() {
        return Ok(None);
    }
    let dir = std::env::temp_dir().join(format!("dbstudio-oracle-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let wallet = TempWallet { dir };
    std::fs::write(wallet.dir.join("ewallet.pem"), pem)?;
    Ok(Some(wallet))
}

#[cfg(test)]
mod tests {
    use dbstudio_core::{DbKind, TlsConfig};

    use super::*;

    fn cfg(db: Option<&str>) -> ConnectionConfig {
        let mut c = ConnectionConfig::new(DbKind::Oracle, "t", "db.example.com");
        c.database = db.map(str::to_string);
        c.username = Some("scott".into());
        c
    }

    #[test]
    fn service_name_becomes_easy_connect() {
        assert_eq!(
            connect_string(&cfg(Some("FREEPDB1")), None).unwrap(),
            "db.example.com:1521/FREEPDB1"
        );
        let mut c = cfg(Some("svc"));
        c.port = 1999;
        c.host = "::1".into();
        assert_eq!(connect_string(&c, None).unwrap(), "[::1]:1999/svc");
    }

    #[test]
    fn complete_strings_are_kept() {
        assert_eq!(
            connect_string(&cfg(Some("other:1522/svc")), None).unwrap(),
            "other:1522/svc"
        );
        let d = "(DESCRIPTION=(ADDRESS=(PROTOCOL=TCP)(HOST=h)(PORT=1)))";
        assert_eq!(connect_string(&cfg(Some(d)), None).unwrap(), d);
        assert_eq!(
            connect_string(&cfg(Some("tcps://h:2484/s")), None).unwrap(),
            "tcps://h:2484/s"
        );
    }

    #[test]
    fn sid_builds_a_descriptor() {
        let mut c = cfg(None);
        c.options.insert("sid".into(), "ORCL".into());
        assert_eq!(
            connect_string(&c, None).unwrap(),
            "(DESCRIPTION=(ADDRESS=(PROTOCOL=TCP)(HOST=db.example.com)(PORT=1521))(CONNECT_DATA=(SID=ORCL)))"
        );
    }

    #[test]
    fn missing_service_is_an_error() {
        assert_eq!(
            connect_string(&cfg(None), None).unwrap_err().kind(),
            "connection"
        );
        let mut c = cfg(Some("x"));
        c.host = " ".into();
        assert_eq!(connect_string(&c, None).unwrap_err().kind(), "connection");
    }

    #[test]
    fn tls_modes() {
        let mut c = cfg(Some("svc"));
        c.tls = TlsConfig {
            mode: TlsMode::Require,
            ..Default::default()
        };
        assert_eq!(
            connect_string(&c, None).unwrap(),
            "tcps://db.example.com:1521/svc?ssl_server_dn_match=off"
        );
        c.tls.mode = TlsMode::VerifyFull;
        c.tls.server_name = Some("db.corp".into());
        let s = connect_string(&c, Some("/tmp/w w")).unwrap();
        assert!(s.starts_with(
            "tcps://db.example.com:1521/svc?ssl_server_dn_match=on&wallet_location=/tmp/w%20w"
        ));
        assert!(s.contains("ssl_server_cert_dn=CN%3Ddb.corp"));
        c.tls.mode = TlsMode::Prefer;
        assert!(!connect_string(&c, None).unwrap().starts_with("tcps"));
        c.tls.mode = TlsMode::Disable;
        assert_eq!(
            connect_string(&c, Some("/w")).unwrap(),
            "db.example.com:1521/svc"
        );
    }

    #[test]
    fn connect_timeout_is_passed_in_seconds() {
        let mut c = cfg(Some("svc"));
        c.connect_timeout_ms = Some(2500);
        assert_eq!(
            connect_string(&c, None).unwrap(),
            "db.example.com:1521/svc?transport_connect_timeout=3"
        );
    }

    #[test]
    fn privilege_and_user_validation() {
        let mut c = cfg(Some("svc"));
        c.options.insert("connect_as".into(), "SYSDBA".into());
        let t = build(&c, Some(&Secret::new("pw"))).unwrap();
        assert!(matches!(t.privilege, Some(Privilege::Sysdba)));
        assert_eq!(t.password(), "pw");
        assert!(
            !format!("{t:?}").contains("pw\""),
            "debug must not print the password"
        );
        c.options.insert("connect_as".into(), "root".into());
        assert!(build(&c, None).is_err());
        c.options.clear();
        c.username = None;
        assert_eq!(build(&c, None).unwrap_err().kind(), "authentication");
    }

    #[test]
    fn pem_wallet_is_written_and_removed() {
        let dir =
            std::env::temp_dir().join(format!("dbstudio-oracle-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let ca = dir.join("ca.pem");
        std::fs::write(
            &ca,
            "-----BEGIN CERTIFICATE-----\nAAA\n-----END CERTIFICATE-----\n",
        )
        .unwrap();
        let mut c = cfg(Some("svc"));
        c.tls = TlsConfig {
            mode: TlsMode::VerifyCa,
            ca_cert_path: Some(ca.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let wallet_path;
        {
            let t = build(&c, None).unwrap();
            let w = t.wallet_dir().expect("wallet").to_path_buf();
            assert!(
                std::fs::read_to_string(w.join("ewallet.pem"))
                    .unwrap()
                    .contains("BEGIN CERTIFICATE")
            );
            assert!(t.connect_string.contains(&format!(
                "wallet_location={}",
                url_component(&w.to_string_lossy())
            )));
            wallet_path = w;
        }
        assert!(!wallet_path.exists(), "wallet removed on drop");
        c.tls.ca_cert_path = Some("/nonexistent/ca.pem".into());
        assert_eq!(build(&c, None).unwrap_err().kind(), "tls");
        let _ = std::fs::remove_dir_all(dir);
    }
}
