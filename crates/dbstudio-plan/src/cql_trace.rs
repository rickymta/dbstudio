//! Parser for the CQL tracing session JSON produced by the Cassandra / ScyllaDB driver.
//!
//! The document is `{ sessionId, request, command, coordinator, durationUs, startedAt,
//! parameters, rows, events: [{ activity, source, sourceElapsedUs, thread }] }` (the data of
//! `system_traces.sessions` and `system_traces.events`). The root node is the request (its
//! duration is the total time); its children are the events in server order. The time of an
//! event is the time since the previous event of the same node (`source_elapsed` is cumulative
//! per node), so the hot path is the slowest step.
//!
//! Findings that the generic plan rules cannot express are kept as node attributes:
//! `extra["warning"]` = `tombstones` (an event read dead cells) or `slow` (request over 100 ms).

use std::collections::BTreeMap;

use dbstudio_core::PlanFormat;

use crate::build::{finish, object};
use crate::json::{self, Js};
use crate::{PlanError, PlanNode, PlanNodeKind, PlanTotals, PlanTree, RawRef};

/// A request slower than this is flagged `slow`.
const SLOW_US: f64 = 100_000.0;

pub(crate) fn parse(text: &str) -> Result<PlanTree, PlanError> {
    let doc = json::parse(text).map_err(PlanError::Invalid)?;
    if !doc.is_obj() {
        return Err(PlanError::Invalid(
            "a CQL trace must be a JSON object".into(),
        ));
    }
    let Some(events) = doc.get("events") else {
        return Err(PlanError::Invalid("no \"events\" in the CQL trace".into()));
    };
    let duration_us = doc.num("durationUs");
    let mut root = PlanNode::new("CQL request", PlanNodeKind::Result);
    root.raw = RawRef::JsonPointer {
        pointer: String::new(),
    };
    root.actual_time_ms = duration_us.map(|d| d / 1000.0);
    root.loops = Some(1);
    if let Some(rows) = doc.num("rows") {
        root.actual_rows = Some(rows);
        root.actual_rows_total = Some(rows);
    }
    if let Some(table) = doc.text("request").and_then(|_| statement_table(&doc)) {
        root.object = Some(object(&table));
    }
    for key in [
        "sessionId",
        "command",
        "coordinator",
        "startedAt",
        "request",
    ] {
        if let Some(v) = doc.text(key) {
            root.put(key, v);
        }
    }
    if let Some(params) = doc.get("parameters") {
        for (k, v) in params.members() {
            root.put(k, v.display());
        }
    }
    if let Some(q) = doc.get("parameters").and_then(|p| p.text("query")) {
        root.condition = Some(q.to_string());
    }

    let mut last_by_source: BTreeMap<String, f64> = BTreeMap::new();
    let mut tombstones_total = 0u64;
    let mut warnings: Vec<&str> = Vec::new();
    for (i, e) in events.items().iter().enumerate() {
        let activity = e.text("activity").unwrap_or("").to_string();
        let source = e.text("source").unwrap_or("").to_string();
        let mut node = PlanNode::new(label(&activity), kind_of(&activity));
        node.raw = RawRef::JsonPointer {
            pointer: format!("/events/{i}"),
        };
        node.loops = Some(1);
        if let Some(elapsed) = e.num("sourceElapsedUs") {
            let prev = last_by_source
                .insert(source.clone(), elapsed)
                .unwrap_or(0.0);
            node.actual_time_ms = Some(((elapsed - prev).max(0.0)) / 1000.0);
            node.put("sourceElapsedUs", json::fmt_num(elapsed));
        }
        node.put("activity", activity.clone());
        node.put("source", source);
        if let Some(t) = e.text("thread") {
            node.put("thread", t);
        }
        if let Some(table) = event_table(&activity) {
            node.object = Some(object(&table));
        }
        if let Some((live, dead)) = rows_read(&activity) {
            node.actual_rows = Some(live as f64);
            node.actual_rows_total = Some(live as f64);
            if dead > 0 {
                node.put("tombstones", dead.to_string());
                node.put("warning", "tombstones");
                tombstones_total += dead;
                if !warnings.contains(&"tombstones") {
                    warnings.push("tombstones");
                }
            }
        }
        root.children.push(node);
    }
    if tombstones_total > 0 {
        root.put("tombstones", tombstones_total.to_string());
    }
    if duration_us.is_some_and(|d| d > SLOW_US) {
        warnings.push("slow");
    }
    if !warnings.is_empty() {
        root.put("warning", warnings.join(","));
    }
    let totals = PlanTotals {
        execution_time_ms: duration_us.map(|d| d / 1000.0),
        rows: doc.num("rows"),
        ..PlanTotals::default()
    };
    Ok(finish(PlanFormat::CqlTrace, true, vec![root], totals))
}

/// `FROM ks.table` of the request text.
fn statement_table(doc: &Js) -> Option<String> {
    let query = doc
        .get("parameters")
        .and_then(|p| p.text("query"))
        .or_else(|| doc.text("request"))?;
    let lower = query.to_ascii_lowercase();
    let at = lower.find(" from ")? + 6;
    let rest = query[at..].trim_start();
    let end = rest
        .find(|c: char| c.is_whitespace() || c == ';')
        .unwrap_or(rest.len());
    let name = rest[..end].trim_matches('"');
    (!name.is_empty()).then(|| name.to_string())
}

/// Table named by `Executing single-partition query on <table>` style events.
fn event_table(activity: &str) -> Option<String> {
    let lower = activity.to_ascii_lowercase();
    let at = lower.find("query on ").map(|i| i + "query on ".len())?;
    let rest = &activity[at..];
    let end = rest
        .find(|c: char| c.is_whitespace() || c == '[' || c == ',')
        .unwrap_or(rest.len());
    let name = &rest[..end];
    (!name.is_empty()).then(|| name.to_string())
}

/// Live and dead cells of a read summary event. Cassandra: `Read 5 live rows and 2 tombstone
/// cells`; ScyllaDB: `Page stats: ... (3 live, 1 dead) ... and 0 range tombstone(s)`.
fn rows_read(activity: &str) -> Option<(u64, u64)> {
    let lower = activity.to_ascii_lowercase();
    if let Some(i) = lower.find("read ") {
        let rest = &lower[i + 5..];
        let live: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if !live.is_empty() && rest[live.len()..].trim_start().starts_with("live row") {
            let tail = &rest[live.len()..];
            if let Some(j) = tail.find(" and ") {
                let dead: String = tail[j + 5..]
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect();
                return Some((live.parse().ok()?, dead.parse().unwrap_or(0)));
            }
            return Some((live.parse().ok()?, 0));
        }
    }
    if lower.contains("page stats") {
        // "... 420 clustering row(s) (419 live, 1 dead), 0 range tombstone(s) and 419 cell(s)
        // (399 live, 20 dead)"
        let group_after = |marker: &str| -> Option<(u64, u64)> {
            let at = lower.find(marker)? + marker.len();
            let rest = &lower[at..];
            let open = rest.find('(')?;
            let close = rest.find(')')?;
            let inner = rest.get(open + 1..close)?;
            let (l, d) = inner.split_once(" live, ")?;
            Some((
                l.trim().parse().ok()?,
                d.strip_suffix(" dead")?.trim().parse().ok()?,
            ))
        };
        let (live, row_dead) = group_after("clustering row(s)")?;
        let range_tombstones = lower
            .find(" range tombstone(s)")
            .and_then(|i| {
                lower[..i]
                    .rsplit(|c: char| !c.is_ascii_digit())
                    .next()
                    .and_then(|n| n.parse::<u64>().ok())
            })
            .unwrap_or(0);
        let cell_dead = group_after("cell(s)").map_or(0, |(_, d)| d);
        return Some((live, (row_dead + range_tombstones).max(cell_dead)));
    }
    None
}

fn kind_of(activity: &str) -> PlanNodeKind {
    let a = activity.to_ascii_lowercase();
    if a.contains("seq scan")
        || a.contains("range slice")
        || a.contains("computing ranges")
        || a.contains("token range")
        || (a.contains("singular range") && a.contains("inf"))
    {
        PlanNodeKind::FullScan
    } else if a.contains("single-partition query")
        || a.contains("single partition")
        || a.contains("querying singular range")
    {
        PlanNodeKind::IndexSeek
    } else if a.contains("using index") || a.contains("index mean cardinalities") {
        PlanNodeKind::IndexScan
    } else if a.contains("request complete") {
        PlanNodeKind::Result
    } else if a.contains("mutation") || a.contains("applying") {
        PlanNodeKind::Modify
    } else {
        PlanNodeKind::Other
    }
}

/// Short label: the activity without trailing shard info (`[shard 0]`).
fn label(activity: &str) -> String {
    let t = activity.trim();
    let t = match t.rfind(" [shard") {
        Some(i) if t.ends_with(']') => &t[..i],
        _ => t,
    };
    if t.is_empty() {
        "event".into()
    } else {
        t.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PlanRules, analyze};

    const SAMPLE: &str = r#"{
      "sessionId": "5e5e5e5e-0000-1000-8000-000000000000",
      "request": "Execute CQL3 query",
      "command": "QUERY",
      "coordinator": "172.18.0.2",
      "durationUs": 1500,
      "startedAt": "2026-11-14T10:00:00.000Z",
      "parameters": {"query": "SELECT * FROM shop.orders WHERE id = 1", "consistency_level": "LOCAL_ONE"},
      "rows": 3,
      "events": [
        {"activity": "Parsing a statement [shard 0]", "source": "172.18.0.2", "sourceElapsedUs": 10, "thread": "shard 0"},
        {"activity": "Executing single-partition query on orders", "source": "172.18.0.2", "sourceElapsedUs": 410},
        {"activity": "Read 3 live rows and 2 tombstone cells", "source": "172.18.0.2", "sourceElapsedUs": 1200},
        {"activity": "Request complete", "source": "172.18.0.2", "sourceElapsedUs": 1500}
      ]
    }"#;

    #[test]
    fn builds_request_with_events() {
        let tree = parse(SAMPLE).unwrap();
        assert_eq!(tree.format, PlanFormat::CqlTrace);
        assert!(tree.analyzed);
        assert_eq!(tree.node_count, 5);
        assert_eq!(tree.root.op, "CQL request");
        assert_eq!(tree.root.actual_time_ms, Some(1.5));
        assert_eq!(tree.root.actual_rows, Some(3.0));
        assert_eq!(tree.root.object.as_ref().unwrap().name, "shop.orders");
        assert_eq!(tree.totals.execution_time_ms, Some(1.5));
        let kids = &tree.root.children;
        assert_eq!(kids[0].op, "Parsing a statement");
        assert_eq!(kids[1].kind, PlanNodeKind::IndexSeek);
        assert_eq!(kids[1].object.as_ref().unwrap().name, "orders");
        // Deltas per node.
        assert_eq!(kids[1].actual_time_ms, Some(0.4));
        assert_eq!(kids[2].actual_time_ms, Some(0.79));
        assert_eq!(kids[3].kind, PlanNodeKind::Result);
    }

    #[test]
    fn tombstones_are_flagged() {
        let tree = parse(SAMPLE).unwrap();
        let read = &tree.root.children[2];
        assert_eq!(read.actual_rows, Some(3.0));
        assert_eq!(read.extra.get("tombstones").map(String::as_str), Some("2"));
        assert_eq!(
            read.extra.get("warning").map(String::as_str),
            Some("tombstones")
        );
        assert_eq!(
            tree.root.extra.get("warning").map(String::as_str),
            Some("tombstones")
        );
        assert_eq!(
            tree.root.extra.get("tombstones").map(String::as_str),
            Some("2")
        );
    }

    #[test]
    fn slow_requests_are_flagged() {
        let slow = SAMPLE.replace("\"durationUs\": 1500", "\"durationUs\": 250000");
        let tree = parse(&slow).unwrap();
        assert!(tree.root.extra["warning"].contains("slow"));
        assert!(
            !parse(SAMPLE).unwrap().root.extra["warning"].contains("slow"),
            "fast request"
        );
    }

    #[test]
    fn scylla_page_stats() {
        assert_eq!(
            rows_read(
                "Page stats: 1 partition(s), 0 static row(s) (0 live, 0 dead), 2 clustering row(s) (2 live, 1 dead) and 1 range tombstone(s)"
            ),
            Some((2, 2))
        );
        assert_eq!(
            rows_read("Read 5 live rows and 0 tombstone cells"),
            Some((5, 0))
        );
        assert_eq!(rows_read("Parsing a statement"), None);
    }

    #[test]
    fn full_scan_kind() {
        assert_eq!(
            kind_of("Executing seq scan across 1 sstables for (min, max)"),
            PlanNodeKind::FullScan
        );
        assert_eq!(kind_of("Computing ranges to query"), PlanNodeKind::FullScan);
        assert_eq!(
            label("Sending a mutation to /10.0.0.1 [shard 1]"),
            "Sending a mutation to /10.0.0.1"
        );
    }

    #[test]
    fn analyze_finds_hot_path_and_is_stable() {
        let mut tree = parse(SAMPLE).unwrap();
        analyze(&mut tree, &PlanRules::default());
        // The slowest step (0.79 ms) is the hot child.
        assert_eq!(tree.hot_path, vec![0, 3]);
        let again = {
            let mut t = tree.clone();
            analyze(&mut t, &PlanRules::default());
            t
        };
        assert_eq!(tree, again);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(matches!(parse("[]"), Err(PlanError::Invalid(_))));
        assert!(matches!(parse("{}"), Err(PlanError::Invalid(_))));
        assert!(matches!(parse("not json"), Err(PlanError::Invalid(_))));
    }
}
