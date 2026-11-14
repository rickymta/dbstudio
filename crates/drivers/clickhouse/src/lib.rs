//! ClickHouse driver for DBStudio (HTTP interface, `RowBinaryWithNamesAndTypes`).
//!
//! * Transport: the official `clickhouse` client over hyper with our own rustls configuration
//!   (CA, mTLS, verification modes, SSH host-name override; see the `http` module). Port 8123
//!   (HTTP) or 8443 (HTTPS). `Require` / `VerifyCa` / `VerifyFull` (or option `secure`) select
//!   HTTPS, `Prefer` uses HTTPS only on 8443/443.
//! * Results: every statement is one request; rows are decoded by our own RowBinary decoder
//!   (`rowbinary`), exactly (no lossy 64-bit / Decimal / byte handling). A column type the
//!   decoder does not know (`AggregateFunction`, ...) makes the driver re-request that statement
//!   as `JSONCompactEachRowWithNamesAndTypes`.
//! * Cancel: `KILL QUERY` by query id plus dropping the stream; timeouts use
//!   `max_execution_time`. Read-only profiles send `readonly = 2` with every request.
//! * Leases: an HTTP session (`session_id`) keeps `SET` and temporary tables; `USE` is tracked by
//!   the driver. There are no transactions.
//! * Editing: only plain `*MergeTree` tables, rows identified by the sorting key with a
//!   `count() = 1` check before every mutation (see `edit`).
//!
//! # Connection options
//!
//! `secure` (`true`/`false`), `compression` (`lz4` default, `none`), `editing` (`insertOnly`
//! disables update/delete), `show_system` (`true` lists `system`/`INFORMATION_SCHEMA` databases)
//! and `setting.<name>` (a ClickHouse setting sent with every request, e.g. `setting.max_threads`).
//! Every option is also accepted with the prefix `clickhouse.`.
//!
//! # Tested versions
//!
//! ClickHouse 26.3 LTS (HTTP and HTTPS) and 23.8 (oldest supported). Settings that older
//! servers reject are only sent after probing `system.settings`.

mod browse;
mod bulk;
mod conn;
mod edit;
mod error;
mod exec;
mod explain;
mod http;
mod lease;
mod rowbinary;
mod session;
mod sql;
mod types;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dbstudio_core::{
    Capabilities, ConnectionConfig, DbKind, Driver, DriverDescriptor, DriverRegistry, DriverStatus,
    Error, Result, Secret, ServerInfo, Session,
};

use conn::{Core, Req, Version, cell_text, option};

pub use session::ClickHouseSession;

/// Driver for ClickHouse servers.
#[derive(Debug, Default, Clone, Copy)]
pub struct ClickHouseDriver;

impl ClickHouseDriver {
    /// Create the driver.
    pub fn new() -> Self {
        Self
    }
}

/// `setting.<name>` options as request settings.
fn user_settings(cfg: &ConnectionConfig) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for (key, value) in &cfg.options {
        let name = key
            .strip_prefix("clickhouse.setting.")
            .or_else(|| key.strip_prefix("setting."));
        if let Some(name) = name {
            if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                return Err(Error::Invalid(format!("invalid setting name `{name}`")));
            }
            out.push((name.to_string(), value.clone()));
        }
    }
    Ok(out)
}

fn flag(cfg: &ConnectionConfig, name: &str) -> Result<bool> {
    match option(cfg, name).map(|v| v.trim().to_ascii_lowercase()) {
        None => Ok(false),
        Some(v) if v == "true" || v == "1" => Ok(true),
        Some(v) if v == "false" || v == "0" => Ok(false),
        Some(v) => Err(Error::Invalid(format!("invalid option {name} `{v}`"))),
    }
}

#[async_trait]
impl Driver for ClickHouseDriver {
    fn descriptor(&self) -> DriverDescriptor {
        DriverDescriptor::new(
            DbKind::ClickHouse,
            Capabilities::planned_for(DbKind::ClickHouse),
            DriverStatus::Available,
        )
    }

    async fn connect(
        &self,
        config: &ConnectionConfig,
        secret: Option<&Secret>,
    ) -> Result<Box<dyn Session>> {
        let candidates = http::targets(config)?;
        let mut last = None;
        for (i, target) in candidates.iter().enumerate() {
            match connect_target(config, secret, target).await {
                Ok(s) => {
                    if i > 0 {
                        tracing::warn!(
                            host = %config.host,
                            "ClickHouse: TLS is not available, connected over plain HTTP; the \
                             password and the data travel unencrypted"
                        );
                    }
                    return Ok(s);
                }
                // Only a transport problem justifies falling back from HTTPS to HTTP.
                Err(e @ (Error::Tls(_) | Error::Connection(_) | Error::Timeout))
                    if i + 1 < candidates.len() =>
                {
                    last = Some(e);
                }
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap_or_else(|| Error::Connection("no target".into())))
    }
}

async fn connect_target(
    config: &ConnectionConfig,
    secret: Option<&Secret>,
    target: &http::Target,
) -> Result<Box<dyn Session>> {
    {
        let client = http::build_client(config, secret, target)?;
        let insert_only = match option(config, "editing").map(str::trim) {
            None | Some("full") => false,
            Some(v) if v.eq_ignore_ascii_case("insertOnly") => true,
            Some(v) => {
                return Err(Error::Invalid(format!(
                    "invalid option editing `{v}` (expected full or insertOnly)"
                )));
            }
        };
        let mut core = Core {
            client,
            // Decided after the probe (below): the probe must not see our own `readonly`.
            readonly: false,
            default_db: config.database.clone().filter(|d| !d.is_empty()),
            version: Version::default(),
            settings: user_settings(config)?,
            show_system: flag(config, "show_system")?,
            json_as_string: false,
            utc_in_json: true,
            insert_only,
            config_read_only: config.read_only,
            settings_locked: false,
        };
        let limit = config
            .connect_timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(http::DEFAULT_CONNECT_TIMEOUT);
        let probe = tokio::time::timeout(limit, async {
            let rows = core
                .rows(
                    &Req::default(),
                    "SELECT version(), timezone(), currentUser(), hostName(), uptime()",
                )
                .await?;
            // Settings newer servers understand and older ones reject.
            let known: Vec<String> = core
                .rows(
                    &Req::default(),
                    "SELECT name FROM system.settings WHERE name IN \
                     ('output_format_binary_write_json_as_string', \
                      'input_format_binary_read_json_as_string')",
                )
                .await
                .map(|r| r.iter().map(|r| cell_text(r, 0)).collect())
                .unwrap_or_default();
            // A profile that is already read-only is enforced by the server itself (and refuses
            // a `readonly` setting sent by us).
            let profile_readonly = core
                .rows(
                    &Req::default(),
                    "SELECT value FROM system.settings WHERE name = 'readonly'",
                )
                .await
                .ok()
                .and_then(|r| r.first().map(|r| cell_text(r, 0)))
                .and_then(|v| v.parse::<u8>().ok())
                .unwrap_or(0);
            Ok::<_, Error>((rows, known, profile_readonly > 0))
        })
        .await
        .map_err(|_| Error::Timeout)??;
        let (rows, known_settings, profile_readonly) = probe;
        // A profile that is read-only already is enforced by the server, which also refuses a
        // `readonly` setting sent by us.
        core.readonly = config.read_only && !profile_readonly;
        core.config_read_only = config.read_only || profile_readonly;
        let row = rows
            .first()
            .ok_or_else(|| Error::Other("the server returned no version".into()))?;
        let version_text = cell_text(row, 0);
        core.version = Version::parse(&version_text);
        if profile_readonly {
            core.settings_locked = true;
            core.utc_in_json = false;
        } else {
            core.json_as_string = known_settings
                .iter()
                .any(|n| n == "output_format_binary_write_json_as_string");
            core.utc_in_json = core.version.at_least(23, 6);
            for name in known_settings {
                core.settings.push((name, "1".into()));
            }
        }
        let mut extra = BTreeMap::new();
        extra.insert("timezone".to_string(), cell_text(row, 1));
        extra.insert("user".to_string(), cell_text(row, 2));
        extra.insert("host".to_string(), cell_text(row, 3));
        extra.insert("uptime_seconds".to_string(), cell_text(row, 4));
        extra.insert(
            "protocol".to_string(),
            if target.secure { "https" } else { "http" }.into(),
        );
        let info = ServerInfo {
            product: "ClickHouse".into(),
            version: version_text,
            extra,
        };
        Ok(Box::new(ClickHouseSession::new(Arc::new(core), info)))
    }
}

/// Register the driver in `registry`.
pub fn register(registry: &mut DriverRegistry) {
    registry.register(Arc::new(ClickHouseDriver::new()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_with_the_expected_capabilities() {
        let mut registry = DriverRegistry::new();
        register(&mut registry);
        let driver = registry.get(DbKind::ClickHouse).expect("registered");
        let d = driver.descriptor();
        assert_eq!(d.default_port, 8123);
        assert!(d.capabilities.sql && d.capabilities.explain && !d.capabilities.transactions);
        assert!(!d.capabilities.explain_analyze);
        assert_eq!(d.status, DriverStatus::Available);
    }

    #[tokio::test]
    async fn refused_connections_are_connection_errors() {
        let mut cfg = ConnectionConfig::new(DbKind::ClickHouse, "t", "127.0.0.1");
        cfg.port = 1;
        cfg.connect_timeout_ms = Some(3000);
        let err = ClickHouseDriver::new()
            .connect(&cfg, None)
            .await
            .err()
            .expect("must fail");
        assert!(
            matches!(err, Error::Connection(_) | Error::Timeout),
            "{err:?}"
        );
    }

    #[test]
    fn settings_options_are_validated() {
        let mut cfg = ConnectionConfig::new(DbKind::ClickHouse, "t", "h");
        cfg.options.insert("setting.max_threads".into(), "2".into());
        cfg.options
            .insert("clickhouse.setting.max_memory_usage".into(), "1000".into());
        let s = user_settings(&cfg).unwrap();
        assert_eq!(s.len(), 2);
        cfg.options.insert("setting.bad name".into(), "1".into());
        assert!(user_settings(&cfg).is_err());
    }
}
