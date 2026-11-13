//! MongoDB `explain()` output (canonical extended JSON): `queryPlanner` and, when present,
//! `executionStats`; aggregation explains (`stages` with a `$cursor`) and sharded plans
//! (`shards`).

use dbstudio_core::PlanFormat;

use crate::build::{finish, flatten_scalars};
use crate::json::{self, Js};
use crate::{PlanError, PlanNode, PlanNodeKind, PlanObject, PlanTotals, PlanTree};

pub(crate) fn parse(text: &str) -> Result<PlanTree, PlanError> {
    let doc = json::parse(text).map_err(PlanError::Invalid)?;
    if !doc.is_obj() {
        return Err(PlanError::Invalid(
            "a MongoDB explain result must be a JSON object".into(),
        ));
    }
    if doc.get("queryPlanner").is_some() {
        return from_explain(&doc, None);
    }
    if let Some(stages) = doc.get("stages") {
        // Aggregation: the first stage is `$cursor` holding the query explain, later stages are
        // pipeline steps consuming it.
        let items = stages.items();
        if let Some(cursor) = items.first().and_then(|s| s.get("$cursor")) {
            return from_explain(cursor, Some(&items[1..]));
        }
    }
    Err(PlanError::Invalid(
        "no \"queryPlanner\" in the MongoDB explain output".into(),
    ))
}

fn from_explain(e: &Js, pipeline: Option<&[Js]>) -> Result<PlanTree, PlanError> {
    let planner = e
        .get("queryPlanner")
        .ok_or_else(|| PlanError::Invalid("no \"queryPlanner\" member".into()))?;
    let stats = e.get("executionStats");
    let namespace = planner.text("namespace").map(str::to_string);
    let stage = stats
        .and_then(|s| s.get("executionStages"))
        .or_else(|| planner.get("winningPlan"))
        .ok_or_else(|| PlanError::Invalid("no \"winningPlan\" in the MongoDB explain".into()))?;
    let analyzed = stats.is_some_and(|s| s.get("executionStages").is_some());
    let mut root = stage_node(stage, namespace.as_deref());
    if let Some(steps) = pipeline {
        for step in steps {
            root = pipeline_node(step, root);
        }
    }
    let totals = PlanTotals {
        execution_time_ms: stats
            .and_then(|s| s.num("executionTimeMillis"))
            .filter(|_| analyzed),
        rows: stats.and_then(|s| s.num("nReturned")),
        ..PlanTotals::default()
    };
    let mut tree = finish(PlanFormat::MongoExplain, analyzed, vec![root], totals);
    if let Some(ns) = namespace {
        tree.root.put("namespace", ns);
    }
    Ok(tree)
}

/// A later aggregation stage (`{"$group": {...}}`) wrapping what came before.
fn pipeline_node(step: &Js, input: PlanNode) -> PlanNode {
    let (name, body) = step
        .members()
        .iter()
        .find(|(k, _)| k.starts_with('$'))
        .map(|(k, v)| (k.as_str(), Some(v)))
        .unwrap_or(("$stage", None));
    let kind = match name {
        "$group" | "$bucket" | "$bucketAuto" | "$count" | "$sortByCount" => {
            PlanNodeKind::HashAggregate
        }
        "$sort" => PlanNodeKind::Sort,
        "$limit" | "$skip" | "$sample" => PlanNodeKind::Limit,
        "$match" => PlanNodeKind::Filter,
        "$lookup" | "$graphLookup" => PlanNodeKind::NestedLoopJoin,
        "$unionWith" => PlanNodeKind::Union,
        "$setWindowFields" => PlanNodeKind::Window,
        _ => PlanNodeKind::Other,
    };
    let mut n = PlanNode::new(name, kind);
    n.lines(step);
    if let Some(b) = body {
        n.condition = Some(b.compact());
    }
    n.actual_rows = step.num("nReturned");
    if let Some(rows) = step.num("nReturned") {
        n.set_actual(rows, 1);
    }
    n.actual_time_ms = step.num("executionTimeMillisEstimate");
    if let Some(d) = step.get("usedDisk") {
        n.put("usedDisk", d.display());
    }
    n.children.push(input);
    n
}

fn stage_node(s: &Js, namespace: Option<&str>) -> PlanNode {
    // Slot-based plans of MongoDB 5+ keep the classic tree under `queryPlan`.
    let s = s.get("queryPlan").filter(|q| q.is_obj()).unwrap_or(s);
    let stage = s.text("stage").unwrap_or("?");
    let kind = kind_of(stage);
    let mut n = PlanNode::new(stage, kind);
    n.lines(s);
    let scans = matches!(
        kind,
        PlanNodeKind::FullScan
            | PlanNodeKind::IndexScan
            | PlanNodeKind::IndexOnlyScan
            | PlanNodeKind::IndexSeek
            | PlanNodeKind::Fetch
    );
    if scans {
        if let Some(ns) = namespace {
            let (db, coll) = match ns.split_once('.') {
                Some((d, c)) => (Some(d.to_string()), c.to_string()),
                None => (None, ns.to_string()),
            };
            n.object = Some(PlanObject {
                schema: db,
                name: coll,
                alias: None,
                index: s.text("indexName").map(str::to_string),
            });
        } else if let Some(index) = s.text("indexName") {
            n.object = Some(PlanObject {
                schema: None,
                name: index.to_string(),
                alias: None,
                index: Some(index.to_string()),
            });
        }
    }
    n.condition = s
        .get("filter")
        .filter(|f| f.is_obj())
        .map(Js::compact)
        .or_else(|| s.get("indexBounds").filter(|b| b.is_obj()).map(Js::compact));
    if let Some(rows) = s.num("nReturned") {
        n.set_actual(rows, 1);
        n.actual_time_ms = s.num("executionTimeMillisEstimate");
    }
    flatten_scalars(
        &mut n,
        s,
        &[
            "stage",
            "nReturned",
            "executionTimeMillisEstimate",
            "indexName",
        ],
        "",
    );
    for key in ["sortPattern", "keyPattern", "transformBy"] {
        if let Some(v) = s.get(key) {
            n.put(key, v.compact());
        }
    }
    let mut kids: Vec<&Js> = Vec::new();
    if let Some(c) = s.get("inputStage") {
        kids.push(c);
    }
    if let Some(list) = s.get("inputStages") {
        kids.extend(list.items());
    }
    if let Some(shards) = s.get("shards") {
        for sh in shards.items() {
            // Each shard entry wraps its own plan.
            if let Some(plan) = sh
                .get("executionStages")
                .or_else(|| sh.get("winningPlan"))
                .or(Some(sh))
            {
                kids.push(plan);
            }
        }
    }
    n.children = kids.into_iter().map(|k| stage_node(k, namespace)).collect();
    n
}

fn kind_of(stage: &str) -> PlanNodeKind {
    use PlanNodeKind::*;
    match stage {
        "COLLSCAN" | "scan" | "CLUSTERED_IXSCAN" => FullScan,
        "IXSCAN" | "DISTINCT_SCAN" | "TEXT_MATCH" | "ixscan" | "ixseek" | "ixscan_generic" => {
            IndexScan
        }
        "IDHACK"
        | "EXPRESS_IXSCAN"
        | "EXPRESS_CLUSTERED_IXSCAN"
        | "EXPRESS_UPDATE"
        | "EXPRESS_DELETE" => IndexSeek,
        "COUNT_SCAN" => IndexOnlyScan,
        "FETCH" | "seek" => Fetch,
        "SORT" | "SORT_SIMPLE" | "SORT_DEFAULT" | "sort" | "SORT_KEY_GENERATOR" => Sort,
        "LIMIT" | "SKIP" | "limit" | "skip" => Limit,
        "AND_HASH" | "AND_SORTED" | "OR" | "SORT_MERGE" | "UNION" | "union" => Union,
        "SHARD_MERGE" | "SINGLE_SHARD" | "SHARDING_FILTER" => Gather,
        "GROUP" | "group" => HashAggregate,
        "COUNT" => Aggregate,
        "filter" => Filter,
        "nlj" => NestedLoopJoin,
        "hj" => HashJoin,
        "mj" => MergeJoin,
        "UPDATE" | "DELETE" | "BATCHED_DELETE" | "INSERT" | "REPLACE_ROOT"
        | "TIMESERIES_MODIFY" => Modify,
        "SUBPLAN" | "MULTI_PLAN" | "CACHED_PLAN" => Subquery,
        "EOF" => Result,
        "PROJECTION_SIMPLE" | "PROJECTION_COVERED" | "PROJECTION_DEFAULT" | "project" => Other,
        _ => Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_stats_tree() {
        let t = parse(
            r#"{"queryPlanner":{"namespace":"shop.orders","winningPlan":{"stage":"FETCH","inputStage":{"stage":"IXSCAN","indexName":"a_1"}}},
              "executionStats":{"nReturned":3,"executionTimeMillis":2,
                "executionStages":{"stage":"FETCH","nReturned":3,"executionTimeMillisEstimate":2,"docsExamined":3,
                  "inputStage":{"stage":"IXSCAN","nReturned":3,"executionTimeMillisEstimate":1,"indexName":"a_1","keysExamined":3}}}}"#,
        )
        .unwrap();
        assert!(t.analyzed);
        assert_eq!(t.node_count, 2);
        assert_eq!(t.root.kind, PlanNodeKind::Fetch);
        assert_eq!(t.totals.execution_time_ms, Some(2.0));
        let o = t.root.children[0].object.as_ref().unwrap();
        assert_eq!(
            (o.schema.as_deref(), o.name.as_str(), o.index.as_deref()),
            (Some("shop"), "orders", Some("a_1"))
        );
    }

    #[test]
    fn planner_only_and_errors() {
        let t = parse(r#"{"queryPlanner":{"namespace":"d.c","winningPlan":{"stage":"COLLSCAN"}}}"#)
            .unwrap();
        assert!(!t.analyzed);
        assert_eq!(t.root.kind, PlanNodeKind::FullScan);
        assert!(parse("[]").is_err());
        assert!(parse("{}").is_err());
    }
}
