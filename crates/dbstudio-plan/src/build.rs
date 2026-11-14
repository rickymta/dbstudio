//! Helpers shared by the format parsers.

use std::collections::BTreeMap;

use dbstudio_core::PlanFormat;

use crate::json::Js;
use crate::{PlanNode, PlanNodeKind, PlanObject, PlanTotals, PlanTree, RawRef};

impl PlanNode {
    /// A node with only the label and kind set.
    pub(crate) fn new(op: impl Into<String>, kind: PlanNodeKind) -> Self {
        PlanNode {
            id: 0,
            op: op.into(),
            kind,
            object: None,
            condition: None,
            predicates: Vec::new(),
            est_startup_cost: None,
            est_cost: None,
            est_rows: None,
            actual_rows: None,
            actual_rows_total: None,
            actual_time_ms: None,
            loops: None,
            self_cost: None,
            self_time_ms: None,
            children: Vec::new(),
            extra: BTreeMap::new(),
            raw: RawRef::None,
        }
    }

    /// Record actual figures: `rows_per_loop` and `loops` give the total.
    pub(crate) fn set_actual(&mut self, rows_per_loop: f64, loops: u64) {
        self.actual_rows = Some(rows_per_loop);
        self.actual_rows_total = Some(rows_per_loop * loops as f64);
        self.loops = Some(loops);
    }

    /// Record actual figures from the total over all executions.
    pub(crate) fn set_actual_total(&mut self, rows_total: f64, loops: u64) {
        self.actual_rows_total = Some(rows_total);
        self.actual_rows = Some(if loops == 0 {
            0.0
        } else {
            rows_total / loops as f64
        });
        self.loops = Some(loops);
    }

    pub(crate) fn lines(&mut self, v: &Js) {
        self.raw = RawRef::Lines {
            start: v.start,
            end: v.end,
        };
    }

    pub(crate) fn put(&mut self, key: &str, value: impl Into<String>) {
        let value = value.into();
        if !value.is_empty() {
            self.extra.insert(key.to_string(), value);
        }
    }
}

/// Table object with only a name.
pub(crate) fn object(name: &str) -> PlanObject {
    PlanObject {
        schema: None,
        name: name.to_string(),
        alias: None,
        index: None,
    }
}

/// Join non-empty condition pieces.
pub(crate) fn join_conditions(parts: Vec<String>) -> Option<String> {
    let parts: Vec<String> = parts.into_iter().filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("; "))
    }
}

/// Copy the scalar members of `obj` (and arrays of scalars) into `node.extra`, skipping
/// `skip`. Containers of containers are ignored.
pub(crate) fn flatten_scalars(node: &mut PlanNode, obj: &Js, skip: &[&str], prefix: &str) {
    for (k, v) in obj.members() {
        if skip.contains(&k.as_str()) {
            continue;
        }
        let key = if prefix.is_empty() {
            k.clone()
        } else {
            format!("{prefix}{k}")
        };
        match &v.val {
            crate::json::JVal::Null => {}
            crate::json::JVal::Arr(items) => {
                if items.iter().all(|i| {
                    !matches!(i.val, crate::json::JVal::Obj(_) | crate::json::JVal::Arr(_))
                }) {
                    let joined = items.iter().map(Js::display).collect::<Vec<_>>().join(", ");
                    node.put(&key, joined);
                }
            }
            crate::json::JVal::Obj(_) => {}
            _ => node.put(&key, v.display()),
        }
    }
}

/// Assign pre-order ids and count nodes.
pub(crate) fn number(node: &mut PlanNode, next: &mut u32) {
    node.id = *next;
    *next += 1;
    for c in &mut node.children {
        number(c, next);
    }
}

/// Wrap `roots` (one or several) into a tree, adding a synthetic `Result` root for several.
pub(crate) fn finish(
    format: PlanFormat,
    analyzed: bool,
    mut roots: Vec<PlanNode>,
    mut totals: PlanTotals,
) -> PlanTree {
    let mut root = if roots.len() == 1 {
        roots.remove(0)
    } else {
        let mut r = PlanNode::new("Result", PlanNodeKind::Result);
        r.est_cost = roots.iter().filter_map(|c| c.est_cost).reduce(|a, b| a + b);
        r.actual_time_ms = roots
            .iter()
            .filter_map(|c| c.actual_time_ms)
            .reduce(|a, b| a + b);
        r.children = roots;
        r
    };
    if totals.total_cost.is_none() {
        totals.total_cost = root.est_cost;
    }
    if totals.rows.is_none() {
        totals.rows = root.actual_rows_total.or(root.est_rows);
    }
    let mut next = 0;
    number(&mut root, &mut next);
    PlanTree {
        format,
        analyzed,
        root,
        totals,
        warnings: Vec::new(),
        hot_path: Vec::new(),
        node_count: next,
        advice: Vec::new(),
    }
}

/// Give nodes without an estimated cost the cost of their most expensive child (wrappers such
/// as MySQL `ordering_operation` carry no cost of their own).
pub(crate) fn inherit_cost(node: &mut PlanNode) {
    for c in &mut node.children {
        inherit_cost(c);
    }
    if node.est_cost.is_none() {
        node.est_cost = node
            .children
            .iter()
            .filter_map(|c| c.est_cost)
            .reduce(f64::max);
    }
}
