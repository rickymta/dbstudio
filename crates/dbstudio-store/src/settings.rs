//! Application settings persisted in the `settings` table.

use serde::{Deserialize, Serialize};

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
}

impl Default for AppSettings {
    fn default() -> Self {
        AppSettings {
            log_level: LogLevel::Info,
            grid_max_rows: 1_000_000,
            grid_max_cells: 20_000_000,
            timestamp_display: TimestampDisplay::Utc,
        }
    }
}

impl AppSettings {
    /// Clamp numeric fields into their allowed ranges.
    pub fn normalized(mut self) -> Self {
        self.grid_max_rows = self
            .grid_max_rows
            .clamp(GRID_ROWS_RANGE.0, GRID_ROWS_RANGE.1);
        self.grid_max_cells = self
            .grid_max_cells
            .clamp(GRID_CELLS_RANGE.0, GRID_CELLS_RANGE.1);
        self
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
                "timestampDisplay": "utc"
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
