//! Advice rules of this dialect family (design section 4). Filled in by its own package; the
//! contract is [`super::Draft`] and the two entry points below.

use super::{Cx, Draft};
use crate::PlanNode;

/// Rules evaluated for every node of a plan of this family.
pub(crate) fn node_rules(_cx: &Cx<'_>, _node: &PlanNode, _out: &mut Vec<Draft>) {}

/// Rules evaluated once per plan.
pub(crate) fn tree_rules(_cx: &Cx<'_>, _out: &mut Vec<Draft>) {}
