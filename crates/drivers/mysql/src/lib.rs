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
//! * TiDB (flavour detected from `VERSION()`): `SET SESSION TRANSACTION READ ONLY` and
//!   `START TRANSACTION READ ONLY` are rejected by the server, so there is NO server-side
//!   read-only layer. A read-only profile is enforced by the application's statement guard
//!   only; `server_info().extra["readOnlyEnforcement"] == "application"` (plus
//!   `readOnlyNote`) tells the UI to show that, and the capability `read_only_session` is
//!   false. A user-requested read-only transaction is refused (`Unsupported`).
//!
//! # Tested versions and version differences
//!
//! The integration suite runs on MySQL 8.4, MySQL 5.7.44, MariaDB 10.4.34 and MariaDB 10.6.
//!
//! * Metadata queries avoid 8.x-only `information_schema` columns; `SHOW INDEX` is used for
//!   indexes (functional-index `Expression` and `Visible` are read only when present).
//! * `SET NAMES utf8mb4` is a connection setup command: `COM_RESET_CONNECTION` restores the
//!   server's default character set on 5.7 / 10.4 (usually latin1), which would garble text
//!   after the first pool recycle.
//! * `information_schema_stats_expiry` (8.x only) is attempted and ignored elsewhere.
//! * `json` is a real type on MySQL (`Value::Json`) but an alias of `longtext` on MariaDB
//!   (`Value::Text`).
//! * MariaDB reports a nullable column without default as `'NULL'`; it is mapped to no default.
//! * Roles: the `roles` folder lists `mysql.user` accounts and only appears when readable.
//! * MySQL 5.7 may generate a self-signed certificate: `Prefer`/`Require` encrypt, the verifying
//!   modes fail against any other CA.
//! * `sha256_password` accounts are not supported by the client library.

mod browse;
mod bulk;
mod connect;
mod ddl_gen;
mod edit;
mod error;
mod explain;
mod lease;
pub mod model;
mod session;
mod sql;
mod stream;
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

/// True when a `VERSION()` string identifies TiDB (`8.0.11-TiDB-v8.5.7`).
pub(crate) fn detect_tidb(version: &str) -> bool {
    version.to_ascii_lowercase().contains("tidb")
}

/// Statement that opens a read-only transaction. TiDB rejects `START TRANSACTION READ ONLY`
/// (noop implementation), so a plain transaction is used there (the app-level guard and the
/// `read_only_session = false` capability cover read-only profiles).
pub(crate) fn read_tx_sql(is_tidb: bool) -> &'static str {
    if is_tidb {
        "START TRANSACTION"
    } else {
        "START TRANSACTION READ ONLY"
    }
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
    server_read_only: bool,
) -> std::result::Result<(Pool, mysql_async::Opts, Conn), TryError> {
    let opts = connect::build_opts(cfg, secret, ssl, tunables, server_read_only)
        .map_err(TryError::Core)?;
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
        connect_flavored(config, secret).await
    }
}

/// True when `sql_mode` makes `"x"` an identifier (`ANSI_QUOTES`, or the combined `ANSI` mode).
pub(crate) fn sql_mode_has_ansi_quotes(sql_mode: &str) -> bool {
    sql_mode
        .split(',')
        .map(|m| m.trim().to_ascii_uppercase())
        .any(|m| m == "ANSI_QUOTES" || m == "ANSI")
}

/// True when `sql_mode` turns off backslash escapes in string literals.
pub(crate) fn sql_mode_no_backslash_escapes(sql_mode: &str) -> bool {
    sql_mode
        .split(',')
        .any(|m| m.trim().eq_ignore_ascii_case("NO_BACKSLASH_ESCAPES"))
}

/// Which server of the host list a profile wants (`options.targetSessionAttrs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WantedRole {
    Any,
    ReadWrite,
    ReadOnly,
}

fn wanted_role(config: &ConnectionConfig) -> Result<WantedRole> {
    match config
        .options
        .get("targetSessionAttrs")
        .map(|v| v.trim().to_ascii_lowercase().replace('_', "-"))
        .as_deref()
    {
        None | Some("") | Some("any") => Ok(WantedRole::Any),
        Some("read-write") | Some("primary") => Ok(WantedRole::ReadWrite),
        Some("read-only") | Some("standby") | Some("replica") => Ok(WantedRole::ReadOnly),
        Some(other) => Err(Error::Invalid(format!(
            "options.targetSessionAttrs: expected `any`, `read-write` or `read-only`, got {other:?}"
        ))),
    }
}

/// Errors after which the next server of the host list is worth a try (the server could not be
/// reached); a rejected login or certificate would fail the same way everywhere.
fn try_next_node(e: &Error) -> bool {
    matches!(e, Error::Connection(_) | Error::Timeout | Error::Io(_))
}

/// Connect to the first server of `config.nodes()` that answers (and has the wanted role):
/// a MySQL replication set, Group Replication / InnoDB Cluster members, a Galera cluster.
async fn connect_any_node(
    config: &ConnectionConfig,
    secret: Option<&Secret>,
    ssl: Option<mysql_async::SslOpts>,
    tunables: &connect::Tunables,
) -> Result<(Pool, mysql_async::Opts, Conn)> {
    let want = wanted_role(config)?;
    let nodes = config.nodes();
    let mut last: Option<Error> = None;
    for (i, (host, port)) in nodes.iter().enumerate() {
        let node = if i == 0 {
            config.clone()
        } else {
            let mut c = config.clone();
            c.host = host.clone();
            c.port = *port;
            c
        };
        let more = i + 1 < nodes.len();
        let (pool, opts, mut conn) = match connect_node(&node, secret, ssl.clone(), tunables).await
        {
            Ok(v) => v,
            Err(e) if more && try_next_node(&e) => {
                last = Some(e);
                continue;
            }
            Err(e) => return Err(with_node(e, host, *port, nodes.len())),
        };
        if want != WantedRole::Any {
            let read_only: Option<i64> = mysql_async::prelude::Queryable::query_first(
                &mut conn,
                "SELECT @@global.read_only OR @@global.super_read_only",
            )
            .await
            .unwrap_or(None);
            let is_read_only = read_only.unwrap_or(0) != 0;
            if is_read_only != (want == WantedRole::ReadOnly) {
                drop(conn);
                let _ = pool.disconnect().await;
                last = Some(Error::Connection(format!(
                    "{host}:{port} is a {} server",
                    if is_read_only {
                        "read-only"
                    } else {
                        "read-write"
                    }
                )));
                continue;
            }
        }
        return Ok((pool, opts, conn));
    }
    Err(match last {
        Some(e) if nodes.len() > 1 => Error::Connection(format!(
            "none of the {} servers could be used; last: {e}",
            nodes.len()
        )),
        Some(e) => e,
        None => Error::Connection("no server to connect to".into()),
    })
}

/// Name the server in an error when the profile lists several.
fn with_node(e: Error, host: &str, port: u16, count: usize) -> Error {
    if count < 2 {
        return e;
    }
    match e {
        Error::Connection(m) => Error::Connection(format!("{host}:{port}: {m}")),
        other => other,
    }
}

/// One server: the TiDB read-only retry and the TLS `Prefer` fallback, errors mapped.
async fn connect_node(
    config: &ConnectionConfig,
    secret: Option<&Secret>,
    ssl: Option<mysql_async::SslOpts>,
    tunables: &connect::Tunables,
) -> Result<(Pool, mysql_async::Opts, Conn)> {
    let secret_text = secret.map(|s| s.expose().to_string());
    // Server-side `SET SESSION TRANSACTION READ ONLY` is a setup command of read-only profiles;
    // TiDB rejects it, which fails the connection: retry once without it.
    let mut server_read_only = true;
    let mut attempt = try_connect(config, secret, ssl.clone(), tunables, server_read_only).await;
    if config.read_only {
        if let Err(TryError::My(e)) = &attempt {
            if error::is_tidb_noop_error(e) {
                server_read_only = false;
                attempt = try_connect(config, secret, ssl.clone(), tunables, false).await;
            }
        }
    }
    // `Prefer`: a server without TLS (or a failed handshake) falls back to plain text.
    if config.tls.mode == TlsMode::Prefer {
        if let Err(TryError::My(e)) = &attempt {
            if error::is_tls_unavailable(e) {
                attempt = try_connect(config, secret, None, tunables, server_read_only).await;
            }
        }
    }
    match attempt {
        Ok(v) => Ok(v),
        Err(TryError::Core(e)) => Err(e),
        Err(TryError::My(e)) => {
            let err = error::map_connect_error(e, secret_text.as_deref());
            Err(match err {
                Error::Tls(m) if m.contains("not valid for name") => Error::Tls(format!(
                    "{m}. The server certificate does not list host `{}`. Connect with a host \
                     name or address that appears in the certificate, or re-issue it with \
                     that name. Note: with this driver verify-ca also checks the host name \
                     (the TLS library cannot skip it).",
                    config.tls.server_name.as_deref().unwrap_or(&config.host)
                )),
                other => other,
            })
        }
    }
}

/// Shared by the MySQL and TiDB drivers: the flavour is read from `VERSION()` after connecting,
/// whatever kind the profile names.
async fn connect_flavored(
    config: &ConnectionConfig,
    secret: Option<&Secret>,
) -> Result<Box<dyn Session>> {
    connect::ensure_crypto_provider();
    let tunables = connect::Tunables::from_config(config)?;
    let ssl = connect::ssl_opts(&config.tls)?;
    let secret_text = secret.map(|s| s.expose().to_string());

    let (pool, opts, mut conn) = connect_any_node(config, secret, ssl, &tunables).await?;

    // Identify the product from the version string.
    let version: Option<String> =
        mysql_async::prelude::Queryable::query_first(&mut conn, "SELECT VERSION()")
            .await
            .map_err(|e| error::map_connect_error(e, secret_text.as_deref()))?;
    // Cached once: whether `\` escapes inside string literals and `"x"` is an identifier (script
    // lexer dialect).
    let sql_mode: Option<String> =
        mysql_async::prelude::Queryable::query_first(&mut conn, "SELECT @@sql_mode")
            .await
            .map_err(|e| error::map_connect_error(e, secret_text.as_deref()))?;
    let sql_mode = sql_mode.unwrap_or_default();
    let backslash_escapes = !sql_mode_no_backslash_escapes(&sql_mode);
    let ansi_quotes = sql_mode_has_ansi_quotes(&sql_mode);
    drop(conn);
    let version = version.unwrap_or_default();
    let is_mariadb = detect_mariadb(&version);
    let is_tidb = detect_tidb(&version);
    let inner = session::Inner::new(
        pool,
        opts,
        connect::connect_timeout(config),
        secret_text,
        is_mariadb,
        is_tidb,
        backslash_escapes,
        ansi_quotes,
        config.database.clone().filter(|d| !d.is_empty()),
        tunables.search_timeout_ms,
        tunables.debug_search_sleep_ms,
    );
    Ok(Box::new(MySqlSession::new(inner)))
}

/// Driver of the TiDB flavour (MySQL protocol): the shared MySQL implementation; the session
/// detects TiDB from `VERSION()` and reports its own capabilities.
#[derive(Debug, Default, Clone, Copy)]
pub struct TiDbDriver;

impl TiDbDriver {
    /// Create the driver.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Driver for TiDbDriver {
    fn descriptor(&self) -> DriverDescriptor {
        DriverDescriptor::new(
            DbKind::TiDb,
            Capabilities::planned_for(DbKind::TiDb),
            DriverStatus::Available,
        )
    }

    async fn connect(
        &self,
        config: &ConnectionConfig,
        secret: Option<&Secret>,
    ) -> Result<Box<dyn Session>> {
        connect_flavored(config, secret).await
    }
}

/// Register the flavour drivers (TiDB) next to [`MySqlDriver`].
pub fn register_flavors(registry: &mut dbstudio_core::DriverRegistry) {
    registry.register(std::sync::Arc::new(TiDbDriver::new()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tidb_registers() {
        let mut registry = dbstudio_core::DriverRegistry::new();
        register_flavors(&mut registry);
        let driver = registry.get(DbKind::TiDb).expect("registered");
        assert_eq!(driver.descriptor().default_port, 4000);
        assert_eq!(driver.descriptor().status, DriverStatus::Available);
    }

    #[test]
    fn tidb_detection_and_sql_mode_flags() {
        assert!(detect_tidb("8.0.11-TiDB-v8.5.7"));
        assert!(!detect_tidb("8.4.0"));
        assert!(sql_mode_has_ansi_quotes("STRICT_TRANS_TABLES,ANSI_QUOTES"));
        assert!(sql_mode_has_ansi_quotes("ansi"));
        assert!(!sql_mode_has_ansi_quotes(
            "STRICT_TRANS_TABLES,ONLY_FULL_GROUP_BY"
        ));
        assert!(sql_mode_no_backslash_escapes("A,no_backslash_escapes"));
        assert!(!sql_mode_no_backslash_escapes("ANSI_QUOTES"));
        assert_eq!(read_tx_sql(true), "START TRANSACTION");
        assert_eq!(read_tx_sql(false), "START TRANSACTION READ ONLY");
    }

    #[test]
    fn descriptor_is_available() {
        let d = MySqlDriver::new().descriptor();
        assert_eq!(d.kind, DbKind::MySql);
        assert_eq!(d.status, DriverStatus::Available);
        assert!(d.capabilities.sql && d.capabilities.cancel_query && d.capabilities.object_search);
    }

    #[test]
    fn capabilities_follow_contract_table() {
        let c = MySqlDriver::new().descriptor().capabilities;
        assert!(c.leases && c.manual_transactions && c.query_params);
        assert!(c.explain && c.explain_analyze && !c.explain_analyze_writes);
        assert!(c.foreign_keys && c.schema_model && c.ddl_generation);
        assert!(c.table_stream && c.bulk_insert && c.native_backup && c.logical_backup);
        assert!(c.data_editing && c.read_only_session && c.ssh_tunnel && c.multiple_databases);
        assert!(!c.schemas && !c.document_store && !c.key_value_store && !c.paged_tree);
        assert!(!c.schema_inference);
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

#[cfg(test)]
mod cluster_tests {
    use super::*;

    #[test]
    fn target_session_attrs_are_read_like_libpq() {
        let mut c = ConnectionConfig::new(DbKind::MySql, "m", "db1");
        assert_eq!(wanted_role(&c).unwrap(), WantedRole::Any);
        for (value, role) in [
            ("read-write", WantedRole::ReadWrite),
            ("PRIMARY", WantedRole::ReadWrite),
            ("read_only", WantedRole::ReadOnly),
            ("replica", WantedRole::ReadOnly),
        ] {
            c.options.insert("targetSessionAttrs".into(), value.into());
            assert_eq!(wanted_role(&c).unwrap(), role, "{value}");
        }
        c.options
            .insert("targetSessionAttrs".into(), "leader".into());
        assert!(wanted_role(&c).is_err());
    }

    #[test]
    fn only_unreachable_servers_move_on_to_the_next_one() {
        assert!(try_next_node(&Error::Connection("refused".into())));
        assert!(try_next_node(&Error::Timeout));
        assert!(!try_next_node(&Error::Authentication(
            "bad password".into()
        )));
        assert!(!try_next_node(&Error::Tls("bad cert".into())));
        let named = with_node(Error::Connection("refused".into()), "db2", 3307, 2);
        assert!(named.to_string().contains("db2:3307"), "{named}");
    }
}
