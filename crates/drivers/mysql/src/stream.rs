//! MySQL/MariaDB driver: whole-table streaming for export/backup, called by the lease. Owned by work package G.
//!
//! Stub created by the contract scaffold (step 0); the owning work package replaces the bodies.

use dbstudio_core::{Error, ExecSummary, Result, ResultSink, TableStreamRequest};

use crate::session::MySqlSession;

/// Stream a whole table into `sink`.
#[allow(dead_code)]
pub(crate) async fn stream_table(
    session: &MySqlSession,
    req: TableStreamRequest,
    sink: &mut dyn ResultSink,
) -> Result<ExecSummary> {
    let _ = (session, req, sink);
    Err(Error::Unsupported("table_stream".into()))
}
