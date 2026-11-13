//! Parsing of the raw plans returned by `Session::explain` into one normalised tree, plus
//! analysis (self cost/time, hot path, warnings).
//!
//! Scaffold status: the types and `Default` rule values are final; `parse` returns
//! [`PlanError::Unsupported`] and `analyze` does nothing. Package E replaces them.

use std::collections::BTreeMap;

use dbstudio_core::{PlanFormat, RawPlan};
use serde::{Deserialize, Serialize};

/// Why a plan could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    /// No parser for this format yet.
    #[error("unsupported plan format: {0:?}")]
    Unsupported(PlanFormat),
    /// The text is not valid for its format.
    #[error("invalid plan: {0}")]
    Invalid(String),
}

/// Parse `raw` into a tree. Scaffold: always [`PlanError::Unsupported`].
pub fn parse(raw: &RawPlan) -> Result<PlanTree, PlanError> {
    Err(PlanError::Unsupported(raw.format))
}

/// Fill warnings, hot path and self cost/time. Scaffold: no-op.
pub fn analyze(tree: &mut PlanTree, rules: &PlanRules) {
    let _ = (tree, rules);
}

/// [`parse`] then [`analyze`].
pub fn parse_and_analyze(raw: &RawPlan, rules: &PlanRules) -> Result<PlanTree, PlanError> {
    let mut tree = parse(raw)?;
    analyze(&mut tree, rules);
    Ok(tree)
}

/// A normalised plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanTree {
    /// Source format.
    pub format: PlanFormat,
    /// Statistics are actual.
    pub analyzed: bool,
    /// Root node; multi-root sources get a synthetic `Result` root.
    pub root: PlanNode,
    /// Totals.
    pub totals: PlanTotals,
    /// Findings of [`analyze`].
    pub warnings: Vec<PlanWarning>,
    /// Node ids from the root following the maximum inclusive time (else cost).
    pub hot_path: Vec<u32>,
    /// Number of nodes.
    pub node_count: u32,
}

/// One operator of the plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanNode {
    /// Pre-order index, root = 0.
    pub id: u32,
    /// Native label: `Seq Scan`, `Clustered Index Seek`, `TABLE ACCESS FULL`, `IXSCAN`.
    pub op: String,
    /// Normalised kind.
    pub kind: PlanNodeKind,
    /// Accessed object.
    pub object: Option<PlanObject>,
    /// Filter / index cond / join cond summary.
    pub condition: Option<String>,
    /// Estimated startup cost.
    pub est_startup_cost: Option<f64>,
    /// Estimated cost, inclusive of the subtree.
    pub est_cost: Option<f64>,
    /// Estimated rows per execution (per loop).
    pub est_rows: Option<f64>,
    /// Actual rows per loop (average).
    pub actual_rows: Option<f64>,
    /// `actual_rows * loops`.
    pub actual_rows_total: Option<f64>,
    /// Actual time, inclusive, total over loops.
    pub actual_time_ms: Option<f64>,
    /// Number of executions.
    pub loops: Option<u64>,
    /// Cost of the node alone (computed by [`analyze`]).
    pub self_cost: Option<f64>,
    /// Time of the node alone (computed by [`analyze`]).
    pub self_time_ms: Option<f64>,
    /// Children.
    pub children: Vec<PlanNode>,
    /// Raw attributes flattened to text (`Sort Method`: `external merge`).
    pub extra: BTreeMap<String, String>,
    /// Where the node came from in the raw text.
    pub raw: RawRef,
}

/// Normalised operator kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PlanNodeKind {
    /// Table scan.
    FullScan,
    /// Index scan.
    IndexScan,
    /// Index only scan.
    IndexOnlyScan,
    /// Index seek.
    IndexSeek,
    /// Key / RID lookup.
    KeyLookup,
    /// Nested loop join.
    NestedLoopJoin,
    /// Hash join.
    HashJoin,
    /// Merge join.
    MergeJoin,
    /// Sort.
    Sort,
    /// Aggregate.
    Aggregate,
    /// Hash aggregate.
    HashAggregate,
    /// Window function.
    Window,
    /// Limit / top.
    Limit,
    /// Filter.
    Filter,
    /// Materialize.
    Materialize,
    /// Subquery / subplan.
    Subquery,
    /// Union / append.
    Union,
    /// Gather / parallelism.
    Gather,
    /// Insert / update / delete.
    Modify,
    /// Mongo FETCH.
    Fetch,
    /// Synthetic or plain result.
    Result,
    /// Anything else.
    Other,
}

/// Object accessed by a node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanObject {
    /// Schema.
    pub schema: Option<String>,
    /// Table / collection name.
    pub name: String,
    /// Alias in the query.
    pub alias: Option<String>,
    /// Index used.
    pub index: Option<String>,
}

/// Reference into the raw plan text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum RawRef {
    /// JSON pointer.
    JsonPointer {
        /// RFC 6901 pointer.
        pointer: String,
    },
    /// XML node.
    #[serde(rename_all = "camelCase")]
    XmlNode {
        /// `NodeId` attribute.
        node_id: String,
    },
    /// Lines of a text plan.
    Lines {
        /// First line (0-based).
        start: u32,
        /// Last line (inclusive).
        end: u32,
    },
    /// Row of a row-based plan (Oracle).
    Row {
        /// `ID` column.
        id: u32,
    },
    /// No reference.
    None,
}

/// Plan-wide numbers.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanTotals {
    /// Total estimated cost.
    pub total_cost: Option<f64>,
    /// Planning time.
    pub planning_time_ms: Option<f64>,
    /// Execution time.
    pub execution_time_ms: Option<f64>,
    /// Rows produced.
    pub rows: Option<f64>,
}

/// A finding about one node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanWarning {
    /// Node concerned.
    pub node_id: u32,
    /// Rule that fired.
    pub rule: PlanRule,
    /// Severity.
    pub severity: Severity,
    /// Values for the localized message (`rows`, `factor`...).
    pub params: BTreeMap<String, String>,
}

/// Warning rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PlanRule {
    /// Full scan of a large table.
    FullScanLargeTable,
    /// Estimate far from actual.
    Misestimate,
    /// Sort / hash spilled to disk.
    SpillToDisk,
    /// Nested loop with many inner executions.
    NestedLoopManyRows,
}

/// Warning severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Severity {
    /// Informational.
    Info,
    /// Should look at it.
    Warning,
    /// Likely a problem.
    Critical,
}

/// Thresholds of the warning rules.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PlanRules {
    /// `FullScanLargeTable` from this many rows.
    pub large_table_rows: f64,
    /// `Misestimate` from this ratio.
    pub misestimate_factor: f64,
    /// `Misestimate` only when max(actual, estimate) reaches this.
    pub misestimate_min_rows: f64,
    /// `NestedLoopManyRows` from this many inner loops.
    pub nested_loop_rows: f64,
}

impl Default for PlanRules {
    fn default() -> Self {
        PlanRules {
            large_table_rows: 100_000.0,
            misestimate_factor: 10.0,
            misestimate_min_rows: 100.0,
            nested_loop_rows: 10_000.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_core::testing::write_or_check;
    use serde_json::{Value as Json, json};

    fn j<T: Serialize>(v: &T) -> Json {
        serde_json::to_value(v).unwrap()
    }

    fn leaf(id: u32) -> PlanNode {
        PlanNode {
            id,
            op: "Seq Scan".into(),
            kind: PlanNodeKind::FullScan,
            object: Some(PlanObject {
                schema: Some("public".into()),
                name: "orders".into(),
                alias: None,
                index: None,
            }),
            condition: Some("(status = 'x')".into()),
            est_startup_cost: Some(0.0),
            est_cost: Some(155.0),
            est_rows: Some(1000.0),
            actual_rows: Some(950.0),
            actual_rows_total: Some(950.0),
            actual_time_ms: Some(1.5),
            loops: Some(1),
            self_cost: Some(155.0),
            self_time_ms: Some(1.5),
            children: vec![],
            extra: BTreeMap::from([("Sort Method".to_string(), "quicksort".to_string())]),
            raw: RawRef::JsonPointer {
                pointer: "/0/Plan".into(),
            },
        }
    }

    fn tree() -> PlanTree {
        let mut root = leaf(0);
        root.op = "Sort".into();
        root.kind = PlanNodeKind::Sort;
        root.object = None;
        root.raw = RawRef::None;
        root.children = vec![leaf(1)];
        PlanTree {
            format: PlanFormat::PostgresJson,
            analyzed: true,
            root,
            totals: PlanTotals {
                total_cost: Some(160.0),
                planning_time_ms: Some(0.2),
                execution_time_ms: Some(2.0),
                rows: Some(950.0),
            },
            warnings: vec![warning()],
            hot_path: vec![0, 1],
            node_count: 2,
        }
    }

    fn warning() -> PlanWarning {
        PlanWarning {
            node_id: 1,
            rule: PlanRule::Misestimate,
            severity: Severity::Warning,
            params: BTreeMap::from([("factor".to_string(), "12.5".to_string())]),
        }
    }

    fn raw(format: PlanFormat) -> RawPlan {
        RawPlan {
            format,
            text: "[]".into(),
            statement: "select 1".into(),
            analyzed: false,
            rolled_back: false,
            server_version: "16".into(),
            captured_at: "2025-01-01T00:00:00Z".into(),
            extra: BTreeMap::new(),
        }
    }

    #[test]
    fn rules_default_values() {
        let r = PlanRules::default();
        assert_eq!(r.large_table_rows, 100_000.0);
        assert_eq!(r.misestimate_factor, 10.0);
        assert_eq!(r.misestimate_min_rows, 100.0);
        assert_eq!(r.nested_loop_rows, 10_000.0);
        // Missing fields in JSON take the defaults.
        let parsed: PlanRules = serde_json::from_value(json!({"nestedLoopRows": 5.0})).unwrap();
        assert_eq!(parsed.nested_loop_rows, 5.0);
        assert_eq!(parsed.large_table_rows, 100_000.0);
    }

    #[test]
    fn stub_contract_parse_is_unsupported_and_analyze_is_noop() {
        let r = raw(PlanFormat::MySqlJson);
        assert_eq!(
            parse(&r),
            Err(PlanError::Unsupported(PlanFormat::MySqlJson))
        );
        assert_eq!(
            parse_and_analyze(&r, &PlanRules::default()),
            Err(PlanError::Unsupported(PlanFormat::MySqlJson))
        );
        let mut t = tree();
        let before = t.clone();
        analyze(&mut t, &PlanRules::default());
        assert_eq!(t, before);
    }

    #[test]
    fn json_shapes() {
        let w = j(&warning());
        assert_eq!(
            w,
            json!({"nodeId": 1, "rule": "misestimate", "severity": "warning", "params": {"factor": "12.5"}})
        );
        assert_eq!(serde_json::from_value::<PlanWarning>(w).unwrap(), warning());
        assert_eq!(
            j(&RawRef::XmlNode {
                node_id: "3".into()
            }),
            json!({"kind": "xmlNode", "nodeId": "3"})
        );
        assert_eq!(j(&RawRef::None), json!({"kind": "none"}));
        assert_eq!(
            j(&RawRef::Lines { start: 1, end: 2 }),
            json!({"kind": "lines", "start": 1, "end": 2})
        );
        assert_eq!(j(&PlanNodeKind::NestedLoopJoin), json!("nestedLoopJoin"));
        let t = j(&tree());
        assert_eq!(t["hotPath"], json!([0, 1]));
        assert_eq!(t["root"]["children"][0]["estStartupCost"], json!(0.0));
        assert_eq!(serde_json::from_value::<PlanTree>(t).unwrap(), tree());
    }

    #[test]
    fn fixtures() {
        write_or_check(
            "plan",
            &json!({
                "PlanTree": j(&tree()),
                "PlanNode": j(&leaf(1)),
                "PlanWarning": j(&warning()),
                "PlanRules": j(&PlanRules::default()),
            }),
        );
    }
}
