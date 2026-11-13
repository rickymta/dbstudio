//! MySQL/MariaDB driver: schema model (ERD, autocomplete). Owned by work package F.
//!
//! Stub created by the contract scaffold (step 0); the owning work package replaces the bodies.

use dbstudio_core::{Result, SchemaModel, SchemaModelRequest};

use crate::session::MySqlSession;

/// Schema model; falls back to the core implementation based on `table_details`.
pub(crate) async fn schema_model(
    session: &MySqlSession,
    req: SchemaModelRequest,
) -> Result<SchemaModel> {
    dbstudio_core::fallback::schema_model_via_details(session, req).await
}
