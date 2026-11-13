use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::Result;
use crate::script::TextRange;
use crate::value::{Value, ValueKind};

/// A request to execute one or more statements.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryRequest {
    /// Statement text (SQL, CQL, a MongoDB command, a Redis command line, ...).
    pub text: String,
    /// Identifier used to cancel the query via [`crate::Session::cancel`].
    pub query_id: Uuid,
    /// Maximum rows to materialize per result set; extra rows set `truncated`.
    #[serde(default)]
    pub max_rows: Option<u64>,
    /// Client side timeout in milliseconds.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Rows per batch pushed to the [`ResultSink`]; the driver picks a default when `None`.
    #[serde(default)]
    pub batch_size: Option<u32>,
    /// Database (or keyspace) to run in, overriding the session default.
    #[serde(default)]
    pub database: Option<String>,
}

impl QueryRequest {
    /// New request with a fresh query id and no limits.
    pub fn new(text: impl Into<String>) -> Self {
        QueryRequest {
            text: text.into(),
            query_id: Uuid::new_v4(),
            max_rows: None,
            timeout_ms: None,
            batch_size: None,
            database: None,
        }
    }
}

/// Metadata of a result column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ColumnMeta {
    /// Column name.
    pub name: String,
    /// Native type name as reported by the database (e.g. `int4`, `varchar(20)`).
    pub type_name: String,
    /// Closest [`ValueKind`] cells of this column are decoded into.
    pub value_kind: ValueKind,
    /// Whether the column may contain nulls, when known.
    #[serde(default)]
    pub nullable: Option<bool>,
}

/// Tabular data returned by a statement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResultSet {
    /// Column metadata.
    pub columns: Vec<ColumnMeta>,
    /// Row-major cells; each row has `columns.len()` values.
    pub rows: Vec<Vec<Value>>,
    /// True when more rows existed than `max_rows` allowed.
    pub truncated: bool,
}

/// What a single statement produced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum StatementOutcome {
    /// A result set.
    Rows(ResultSet),
    /// A data modification with an affected row count.
    Affected {
        /// Number of affected rows.
        rows: u64,
    },
    /// A statement that completed without data (DDL, `SET`, ...).
    Done,
}

/// Result of one statement in a [`QueryRequest`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatementResult {
    /// Outcome of the statement.
    pub outcome: StatementOutcome,
    /// Server execution plus transfer time in milliseconds.
    pub elapsed_ms: u64,
    /// Notices and warnings emitted while running the statement.
    #[serde(default)]
    pub messages: Vec<String>,
}

/// How a statement ended, as reported to a [`ResultSink`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum EndOutcome {
    /// A result set finished; its rows were delivered through `on_rows`.
    #[serde(rename_all = "camelCase")]
    Rows {
        /// Total rows delivered for this statement.
        row_count: u64,
        /// True when more rows existed than `max_rows` allowed.
        truncated: bool,
    },
    /// A data modification with an affected row count.
    Affected {
        /// Number of affected rows.
        rows: u64,
    },
    /// A statement that completed without data.
    Done,
}

/// End-of-statement event passed to [`ResultSink::on_statement_end`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatementEnd {
    /// What the statement produced.
    pub outcome: EndOutcome,
    /// Server execution plus transfer time in milliseconds.
    pub elapsed_ms: u64,
}

/// Totals returned by [`crate::Session::execute`] once the whole request finished.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecSummary {
    /// Number of statements executed.
    pub statements: u32,
    /// Total elapsed time in milliseconds.
    pub elapsed_ms: u64,
}

/// A statement begins; offsets are characters into the request text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatementStart {
    /// Zero-based statement index.
    pub index: u32,
    /// Range of the statement in the request text (MSSQL may report the whole batch).
    pub range: TextRange,
}

/// Receiver of streamed query results.
///
/// Methods are synchronous. Returning `Err` means the receiver wants the driver to stop; the
/// driver must stop work and propagate that error (use [`crate::Error::Cancelled`] for a
/// user-initiated stop). Event order per statement: optional `on_columns`, any number of
/// `on_rows` and `on_message`, then exactly one `on_statement_end`.
pub trait ResultSink: Send {
    /// A result set begins; `columns` describe the rows that follow.
    fn on_columns(&mut self, columns: Vec<ColumnMeta>) -> Result<()>;
    /// A batch of rows of the current result set.
    fn on_rows(&mut self, rows: Vec<Vec<Value>>) -> Result<()>;
    /// A notice, warning or `PRINT` message.
    fn on_message(&mut self, message: String) -> Result<()>;
    /// The current statement finished.
    fn on_statement_end(&mut self, end: StatementEnd) -> Result<()>;
    /// A statement begins. Optional for drivers; ignored by default.
    fn on_statement_start(&mut self, start: StatementStart) -> Result<()> {
        let _ = start;
        Ok(())
    }
}

/// Event streamed to the frontend (through a Tauri channel) while a query or table fetch runs.
/// Mirrors the [`ResultSink`] callbacks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum QueryEvent {
    /// A result set begins.
    Columns {
        /// Column metadata of the rows that follow.
        columns: Vec<ColumnMeta>,
    },
    /// A batch of rows.
    Rows {
        /// Row-major cells.
        rows: Vec<Vec<Value>>,
    },
    /// A notice or warning.
    Message {
        /// Message text.
        message: String,
    },
    /// The current statement finished.
    StatementEnd {
        /// End-of-statement details.
        end: StatementEnd,
    },
    /// A statement begins (optional; drivers that cannot split statements never send it).
    StatementStart {
        /// Index and character range.
        start: StatementStart,
    },
}

/// A [`ResultSink`] that gathers all events into [`StatementResult`]s (for tests and small
/// results; large results should be streamed instead).
#[derive(Debug, Default)]
pub struct CollectingSink {
    results: Vec<StatementResult>,
    columns: Vec<ColumnMeta>,
    rows: Vec<Vec<Value>>,
    messages: Vec<String>,
}

impl CollectingSink {
    /// Empty sink.
    pub fn new() -> Self {
        Self::default()
    }

    /// Consume the sink and return the collected statement results.
    pub fn into_results(self) -> Vec<StatementResult> {
        self.results
    }
}

impl ResultSink for CollectingSink {
    fn on_columns(&mut self, columns: Vec<ColumnMeta>) -> Result<()> {
        self.columns = columns;
        self.rows.clear();
        Ok(())
    }

    fn on_rows(&mut self, mut rows: Vec<Vec<Value>>) -> Result<()> {
        self.rows.append(&mut rows);
        Ok(())
    }

    fn on_message(&mut self, message: String) -> Result<()> {
        self.messages.push(message);
        Ok(())
    }

    fn on_statement_end(&mut self, end: StatementEnd) -> Result<()> {
        let columns = std::mem::take(&mut self.columns);
        let rows = std::mem::take(&mut self.rows);
        let outcome = match end.outcome {
            EndOutcome::Rows { truncated, .. } => StatementOutcome::Rows(ResultSet {
                columns,
                rows,
                truncated,
            }),
            EndOutcome::Affected { rows } => StatementOutcome::Affected { rows },
            EndOutcome::Done => StatementOutcome::Done,
        };
        self.results.push(StatementResult {
            outcome,
            elapsed_ms: end.elapsed_ms,
            messages: std::mem::take(&mut self.messages),
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn statement_result_shape() {
        let r = StatementResult {
            outcome: StatementOutcome::Affected { rows: 3 },
            elapsed_ms: 12,
            messages: vec![],
        };
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            json!({"outcome": {"kind": "affected", "rows": 3}, "elapsedMs": 12, "messages": []})
        );
        let rs = StatementOutcome::Rows(ResultSet {
            columns: vec![ColumnMeta {
                name: "id".into(),
                type_name: "int8".into(),
                value_kind: ValueKind::Int,
                nullable: Some(false),
            }],
            rows: vec![vec![Value::Int(1)]],
            truncated: false,
        });
        let back: StatementOutcome =
            serde_json::from_value(serde_json::to_value(&rs).unwrap()).unwrap();
        assert_eq!(back, rs);
    }

    #[test]
    fn query_event_shape() {
        let e = QueryEvent::Rows {
            rows: vec![vec![Value::Int(1)]],
        };
        assert_eq!(
            serde_json::to_value(&e).unwrap(),
            json!({"type": "rows", "rows": [[{"t": "int", "v": 1}]]})
        );
        let e = QueryEvent::StatementEnd {
            end: StatementEnd {
                outcome: EndOutcome::Done,
                elapsed_ms: 1,
            },
        };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["type"], "statementEnd");
        assert_eq!(serde_json::from_value::<QueryEvent>(v).unwrap(), e);
    }

    #[test]
    fn statement_start_event_shape() {
        let e = QueryEvent::StatementStart {
            start: StatementStart {
                index: 0,
                range: TextRange { start: 0, end: 17 },
            },
        };
        let json = serde_json::to_value(&e).unwrap();
        assert_eq!(
            json,
            json!({"type": "statementStart", "start": {"index": 0, "range": {"start": 0, "end": 17}}})
        );
        assert_eq!(serde_json::from_value::<QueryEvent>(json).unwrap(), e);
    }

    #[test]
    fn collecting_sink_ignores_statement_start() {
        let mut sink = CollectingSink::new();
        sink.on_statement_start(StatementStart {
            index: 0,
            range: TextRange { start: 0, end: 1 },
        })
        .unwrap();
        assert!(sink.into_results().is_empty());
    }
}
