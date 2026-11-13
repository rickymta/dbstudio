//! Application settings persisted in the `settings` table.

use std::collections::BTreeMap;

use dbstudio_core::Result;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::db::{Store, db_err, invalid};

/// Verbosity of the application log.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    /// Errors only.
    Error,
    /// Warnings and errors.
    Warn,
    /// Normal operation (default).
    #[default]
    Info,
    /// Per-command diagnostics.
    Debug,
    /// Everything.
    Trace,
}

/// How timestamps are shown in the data grid.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TimestampDisplay {
    /// Show instants in UTC (default).
    #[default]
    Utc,
    /// Convert instants to the local time zone.
    Local,
}

/// What closing the main window does.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CloseBehavior {
    /// Keep running in the tray only while a backup job has a schedule enabled (default).
    #[default]
    Auto,
    /// Always keep running in the tray.
    Always,
    /// Quit when the window closes.
    Never,
}

/// Smallest and largest accepted `editor_max_rows`.
pub const EDITOR_ROWS_RANGE: (u32, u32) = (100, 1_000_000);
/// Smallest and largest accepted `history_max_per_connection`.
pub const HISTORY_MAX_RANGE: (u32, u32) = (100, 100_000);
/// Smallest and largest accepted `max_lob_bytes` (the upper bound stays below 2^53 for JS).
pub const LOB_BYTES_RANGE: (u64, u64) = (1_024, 1_073_741_824);
/// Largest accepted `query_timeout_ms` (24 h; 0 means no timeout).
pub const QUERY_TIMEOUT_MAX_MS: u64 = 86_400_000;
/// Smallest and largest accepted `plan_misestimate_factor`.
pub const PLAN_FACTOR_RANGE: (u32, u32) = (2, 10_000);

/// Smallest and largest accepted `grid_max_rows`.
pub const GRID_ROWS_RANGE: (u32, u32) = (1_000, 5_000_000);
/// Smallest and largest accepted `grid_max_cells`.
pub const GRID_CELLS_RANGE: (u64, u64) = (100_000, 200_000_000);

/// User-adjustable settings. Every field has a default so older stored values stay readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AppSettings {
    /// Log verbosity (applied immediately; `RUST_LOG` still wins when set).
    pub log_level: LogLevel,
    /// Most rows the grid keeps in memory.
    pub grid_max_rows: u32,
    /// Most cells (rows x columns) the grid keeps in memory.
    pub grid_max_cells: u64,
    /// Timestamp rendering.
    pub timestamp_display: TimestampDisplay,
    /// Most rows a SQL editor result keeps per page of results.
    pub editor_max_rows: u32,
    /// Statement timeout in milliseconds; 0 disables it.
    pub query_timeout_ms: u64,
    /// Whether executed statements are recorded in the history.
    pub history_enabled: bool,
    /// History entries kept per connection (older ones are trimmed after each write).
    pub history_max_per_connection: u32,
    /// Largest LOB (bytes) loaded into the grid or viewer.
    pub max_lob_bytes: u64,
    /// Plan warning threshold: estimated vs actual rows off by at least this factor.
    pub plan_misestimate_factor: u32,
    /// Behaviour of closing the main window.
    pub close_to_tray: CloseBehavior,
}

impl Default for AppSettings {
    fn default() -> Self {
        AppSettings {
            log_level: LogLevel::Info,
            grid_max_rows: 1_000_000,
            grid_max_cells: 20_000_000,
            timestamp_display: TimestampDisplay::Utc,
            editor_max_rows: 1_000,
            query_timeout_ms: 0,
            history_enabled: true,
            history_max_per_connection: 5_000,
            max_lob_bytes: 16 * 1024 * 1024,
            plan_misestimate_factor: 10,
            close_to_tray: CloseBehavior::Auto,
        }
    }
}

impl AppSettings {
    /// Read settings field by field from a JSON document: a field that is missing or has an
    /// unusable value falls back to its default without discarding the others.
    pub fn from_value_lenient(value: &serde_json::Value) -> Self {
        fn field<T: serde::de::DeserializeOwned>(
            value: &serde_json::Value,
            key: &str,
            default: T,
        ) -> T {
            value
                .get(key)
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or(default)
        }
        let d = AppSettings::default();
        let s = AppSettings {
            log_level: field(value, "logLevel", d.log_level),
            grid_max_rows: field(value, "gridMaxRows", d.grid_max_rows),
            grid_max_cells: field(value, "gridMaxCells", d.grid_max_cells),
            timestamp_display: field(value, "timestampDisplay", d.timestamp_display),
            editor_max_rows: field(value, "editorMaxRows", d.editor_max_rows),
            query_timeout_ms: field(value, "queryTimeoutMs", d.query_timeout_ms),
            history_enabled: field(value, "historyEnabled", d.history_enabled),
            history_max_per_connection: field(
                value,
                "historyMaxPerConnection",
                d.history_max_per_connection,
            ),
            max_lob_bytes: field(value, "maxLobBytes", d.max_lob_bytes),
            plan_misestimate_factor: field(
                value,
                "planMisestimateFactor",
                d.plan_misestimate_factor,
            ),
            close_to_tray: field(value, "closeToTray", d.close_to_tray),
        };
        s.normalized()
    }

    /// Clamp numeric fields into their allowed ranges.
    pub fn normalized(mut self) -> Self {
        self.grid_max_rows = self
            .grid_max_rows
            .clamp(GRID_ROWS_RANGE.0, GRID_ROWS_RANGE.1);
        self.grid_max_cells = self
            .grid_max_cells
            .clamp(GRID_CELLS_RANGE.0, GRID_CELLS_RANGE.1);
        self.editor_max_rows = self
            .editor_max_rows
            .clamp(EDITOR_ROWS_RANGE.0, EDITOR_ROWS_RANGE.1);
        self.query_timeout_ms = self.query_timeout_ms.min(QUERY_TIMEOUT_MAX_MS);
        self.history_max_per_connection = self
            .history_max_per_connection
            .clamp(HISTORY_MAX_RANGE.0, HISTORY_MAX_RANGE.1);
        self.max_lob_bytes = self
            .max_lob_bytes
            .clamp(LOB_BYTES_RANGE.0, LOB_BYTES_RANGE.1);
        self.plan_misestimate_factor = self
            .plan_misestimate_factor
            .clamp(PLAN_FACTOR_RANGE.0, PLAN_FACTOR_RANGE.1);
        self
    }
}

/// Settings key holding the map of native tool name to executable path.
const NATIVE_TOOLS_KEY: &str = "native_tools";
/// Settings key holding the Oracle client configuration.
const ORACLE_CLIENT_KEY: &str = "oracle_client";

/// Oracle Instant Client location (`oracle_client` setting).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct OracleClientSettings {
    /// Directory containing the Instant Client libraries; `None` = search the default places.
    pub lib_dir: Option<String>,
}

impl Store {
    fn get_json_setting<T: serde::de::DeserializeOwned + Default>(&self, key: &str) -> Result<T> {
        let text: Option<String> = self
            .lock()
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![key],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_err)?;
        Ok(text
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default())
    }

    fn set_json_setting<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        let json = serde_json::to_string(value).map_err(invalid)?;
        self.lock()
            .execute(
                "INSERT INTO settings (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, json],
            )
            .map_err(db_err)?;
        Ok(())
    }

    /// Configured native tool paths (`pg_dump`, `mysqldump`, ...); empty when unset or unreadable.
    pub fn get_native_tools(&self) -> Result<BTreeMap<String, String>> {
        self.get_json_setting(NATIVE_TOOLS_KEY)
    }

    /// Replace the native tool paths.
    pub fn set_native_tools(&self, tools: &BTreeMap<String, String>) -> Result<()> {
        self.set_json_setting(NATIVE_TOOLS_KEY, tools)
    }

    /// Oracle client configuration (default when unset or unreadable).
    pub fn get_oracle_client(&self) -> Result<OracleClientSettings> {
        self.get_json_setting(ORACLE_CLIENT_KEY)
    }

    /// Store the Oracle client configuration.
    pub fn set_oracle_client(&self, client: &OracleClientSettings) -> Result<()> {
        self.set_json_setting(ORACLE_CLIENT_KEY, client)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_and_partial_documents() {
        let d = AppSettings::default();
        assert_eq!(
            serde_json::to_value(d).unwrap(),
            json!({
                "logLevel": "info", "gridMaxRows": 1_000_000, "gridMaxCells": 20_000_000,
                "timestampDisplay": "utc", "editorMaxRows": 1000, "queryTimeoutMs": 0,
                "historyEnabled": true, "historyMaxPerConnection": 5000,
                "maxLobBytes": 16_777_216, "planMisestimateFactor": 10, "closeToTray": "auto"
            })
        );
        let old: AppSettings = serde_json::from_value(json!({"logLevel": "debug"})).unwrap();
        assert_eq!(old.log_level, LogLevel::Debug);
        assert_eq!(old.grid_max_rows, d.grid_max_rows);
        let all: AppSettings = serde_json::from_value(json!({
            "logLevel": "trace", "timestampDisplay": "local"
        }))
        .unwrap();
        assert_eq!(all.timestamp_display, TimestampDisplay::Local);
        assert!(serde_json::from_value::<AppSettings>(json!({"logLevel": "loud"})).is_err());
    }

    #[test]
    fn new_fields_parse_clamp_and_default() {
        let s: AppSettings = serde_json::from_value(json!({
            "closeToTray": "always", "historyEnabled": false, "editorMaxRows": 5
        }))
        .unwrap();
        assert_eq!(s.close_to_tray, CloseBehavior::Always);
        assert!(!s.history_enabled);
        let s = AppSettings {
            editor_max_rows: 5,
            query_timeout_ms: u64::MAX,
            history_max_per_connection: 0,
            max_lob_bytes: u64::MAX,
            plan_misestimate_factor: 0,
            ..AppSettings::default()
        }
        .normalized();
        assert_eq!(s.editor_max_rows, 100);
        assert_eq!(s.query_timeout_ms, QUERY_TIMEOUT_MAX_MS);
        assert_eq!(s.history_max_per_connection, 100);
        assert_eq!(s.max_lob_bytes, LOB_BYTES_RANGE.1);
        assert_eq!(s.plan_misestimate_factor, 2);
        // An unusable value only resets its own field.
        let s = AppSettings::from_value_lenient(&json!({
            "closeToTray": "sometimes", "editorMaxRows": 2500, "maxLobBytes": "big"
        }));
        assert_eq!(s.close_to_tray, CloseBehavior::Auto);
        assert_eq!(s.editor_max_rows, 2500);
        assert_eq!(s.max_lob_bytes, 16 * 1024 * 1024);
        assert!(serde_json::from_value::<AppSettings>(json!({"closeToTray": "x"})).is_err());
    }

    #[test]
    fn native_tools_and_oracle_client_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.get_native_tools().unwrap().is_empty());
        assert_eq!(
            store.get_oracle_client().unwrap(),
            OracleClientSettings::default()
        );
        let tools = BTreeMap::from([("pg_dump".to_string(), "/opt/pg/bin/pg_dump".to_string())]);
        store.set_native_tools(&tools).unwrap();
        assert_eq!(store.get_native_tools().unwrap(), tools);
        let client = OracleClientSettings {
            lib_dir: Some("/opt/oracle/instantclient".into()),
        };
        store.set_oracle_client(&client).unwrap();
        assert_eq!(store.get_oracle_client().unwrap(), client);
        assert_eq!(
            serde_json::to_value(&client).unwrap(),
            json!({"libDir": "/opt/oracle/instantclient"})
        );
        // The separate keys never disturb the app settings.
        assert_eq!(store.get_settings().unwrap(), AppSettings::default());
    }

    #[test]
    fn clamping() {
        let s = AppSettings {
            grid_max_rows: 1,
            grid_max_cells: u64::MAX,
            ..AppSettings::default()
        }
        .normalized();
        assert_eq!(s.grid_max_rows, 1_000);
        assert_eq!(s.grid_max_cells, 200_000_000);
        let s = AppSettings {
            grid_max_rows: u32::MAX,
            grid_max_cells: 0,
            ..AppSettings::default()
        }
        .normalized();
        assert_eq!(s.grid_max_rows, 5_000_000);
        assert_eq!(s.grid_max_cells, 100_000);
    }
}
