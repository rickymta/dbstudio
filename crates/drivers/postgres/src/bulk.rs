//! PostgreSQL driver: bulk insert for import, called by the lease. Owned by work package G.
//!
//! Stub created by the contract scaffold (step 0); the owning work package replaces the bodies.

use dbstudio_core::{BulkInsertRequest, BulkInsertResult, Error, Result};

use crate::session::PostgresSession;

/// Insert rows in bulk.
#[allow(dead_code)]
pub(crate) async fn bulk_insert(
    session: &PostgresSession,
    req: BulkInsertRequest,
) -> Result<BulkInsertResult> {
    let _ = (session, req);
    Err(Error::Unsupported("bulk_insert".into()))
}
