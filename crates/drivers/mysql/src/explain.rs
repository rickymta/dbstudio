//! MySQL/MariaDB driver: EXPLAIN. Owned by work package E.
//!
//! Stub created by the contract scaffold (step 0); the owning work package replaces the bodies.

use dbstudio_core::{Error, ExplainRequest, RawPlan, Result};

use crate::session::MySqlSession;

/// Explain one statement.
pub(crate) async fn explain(session: &MySqlSession, req: ExplainRequest) -> Result<RawPlan> {
    let _ = (session, req);
    Err(Error::Unsupported("explain".into()))
}
