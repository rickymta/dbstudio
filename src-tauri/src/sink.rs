//! Bridge from the driver-facing [`ResultSink`] to a stream of [`QueryEvent`]s.

use dbstudio_core::{
    ColumnMeta, Error, QueryEvent, Result, ResultSink, StatementEnd, StatementStart, Value,
};

/// A [`ResultSink`] that forwards every callback as a [`QueryEvent`] to `emit`.
///
/// `emit` returns `false` when the receiver is gone (e.g. the frontend closed the channel); the
/// sink then fails with [`Error::Cancelled`] so the driver stops reading.
pub struct EventSink<F: FnMut(QueryEvent) -> bool + Send> {
    emit: F,
}

impl<F: FnMut(QueryEvent) -> bool + Send> std::fmt::Debug for EventSink<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventSink").finish_non_exhaustive()
    }
}

impl<F: FnMut(QueryEvent) -> bool + Send> EventSink<F> {
    /// Wrap an emit function.
    pub fn new(emit: F) -> Self {
        EventSink { emit }
    }

    fn send(&mut self, event: QueryEvent) -> Result<()> {
        if (self.emit)(event) {
            Ok(())
        } else {
            Err(Error::Cancelled)
        }
    }
}

impl<F: FnMut(QueryEvent) -> bool + Send> ResultSink for EventSink<F> {
    fn on_columns(&mut self, columns: Vec<ColumnMeta>) -> Result<()> {
        self.send(QueryEvent::Columns { columns })
    }

    fn on_rows(&mut self, rows: Vec<Vec<Value>>) -> Result<()> {
        self.send(QueryEvent::Rows { rows })
    }

    fn on_message(&mut self, message: String) -> Result<()> {
        self.send(QueryEvent::Message { message })
    }

    fn on_statement_end(&mut self, end: StatementEnd) -> Result<()> {
        self.send(QueryEvent::StatementEnd { end })
    }

    fn on_statement_start(&mut self, start: StatementStart) -> Result<()> {
        self.send(QueryEvent::StatementStart { start })
    }
}
