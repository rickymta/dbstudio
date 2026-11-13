//! PostgreSQL driver: DDL generation. Owned by work package F.
//!
//! Stub created by the contract scaffold (step 0); the owning work package replaces the bodies.

use dbstudio_core::{DdlItem, DdlRequest, Error, Result};

use crate::session::PostgresSession;

/// Ordered DDL of the objects in a scope.
pub(crate) async fn generate(session: &PostgresSession, req: DdlRequest) -> Result<Vec<DdlItem>> {
    let _ = (session, req);
    Err(Error::Unsupported("ddl_generation".into()))
}
