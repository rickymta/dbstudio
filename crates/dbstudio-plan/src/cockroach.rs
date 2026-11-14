//! Parser for the CockroachDB EXPLAIN text tree (gói N1).
//!
//! Contract scaffold of the backlog contract v2: the dispatch in `lib.rs` is final, the body is
//! filled in by the package that owns this module.

use dbstudio_core::PlanFormat;

use crate::{PlanError, PlanTree};

pub(crate) fn parse(text: &str) -> Result<PlanTree, PlanError> {
    let _ = text;
    Err(PlanError::Unsupported(PlanFormat::CockroachText))
}
