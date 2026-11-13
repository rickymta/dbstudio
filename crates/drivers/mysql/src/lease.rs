//! MySQL/MariaDB driver: dedicated connection (lease). Owned by work package B.
//!
//! Stub created by the contract scaffold (step 0); the owning work package replaces the bodies.

use dbstudio_core::{Error, Lease, LeaseOptions, Result};

use crate::session::MySqlSession;

/// Open a dedicated connection.
pub(crate) async fn open(session: &MySqlSession, opts: LeaseOptions) -> Result<Box<dyn Lease>> {
    let _ = (session, opts);
    Err(Error::Unsupported("leases".into()))
}
