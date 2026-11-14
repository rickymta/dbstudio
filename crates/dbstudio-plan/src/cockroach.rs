//! Parser for the CockroachDB `EXPLAIN (VERBOSE)` / `EXPLAIN ANALYZE (VERBOSE)` text tree.
//!
//! Shape of the text (one row per line):
//!
//! ```text
//! planning time: 2ms                 <- header: statement-wide `key: value` lines
//! distribution: local
//!
//! • group (hash)                     <- root operator
//! │ columns: (grp, count)            <- its properties
//! │ estimated row count: 50
//! │
//! └── • scan                         <- child operators: `└── • ` / `├── • ` after 4 columns
//!       table: big@big_pkey              of indentation per level
//!       spans: FULL SCAN
//!
//! index recommendations: 1           <- footer: kept as `crdbIndexRecommendations`
//! 1. type: index creation
//!    SQL command: CREATE INDEX ON db.sv.big (name) STORING (grp);
//! ```

use std::collections::BTreeMap;

use dbstudio_core::PlanFormat;

use crate::build::{finish, join_conditions};
use crate::{PlanError, PlanNode, PlanNodeKind, PlanObject, PlanTotals, PlanTree, RawRef};

pub(crate) fn parse(text: &str) -> Result<PlanTree, PlanError> {
    struct Frame {
        depth: usize,
        node: PlanNode,
        first: u32,
        last: u32,
    }
    let mut roots: Vec<PlanNode> = Vec::new();
    let mut stack: Vec<Frame> = Vec::new();
    let mut header: BTreeMap<String, String> = BTreeMap::new();
    let mut in_tree = false;
    let mut footer = false;
    let mut footer_lines: Vec<String> = Vec::new();

    fn close(stack: &mut Vec<Frame>, roots: &mut Vec<PlanNode>, depth: usize) {
        while stack.last().is_some_and(|f| f.depth >= depth) {
            let mut f = stack.pop().expect("checked non-empty");
            finish_node(&mut f.node, f.first, f.last);
            match stack.last_mut() {
                Some(parent) => parent.node.children.push(f.node),
                None => roots.push(f.node),
            }
        }
    }

    for (line_no, line) in text.lines().enumerate() {
        let line_no = line_no as u32;
        let chars: Vec<char> = line.chars().collect();
        if let Some(pos) = node_marker(&chars) {
            if footer {
                continue;
            }
            in_tree = true;
            let depth = pos / 4;
            let name: String = chars[pos + 2..].iter().collect();
            close(&mut stack, &mut roots, depth);
            if stack.len() >= crate::json::MAX_DEPTH as usize {
                return Err(PlanError::Invalid(format!(
                    "the CockroachDB plan is nested deeper than {} levels (line {})",
                    crate::json::MAX_DEPTH,
                    line_no + 1
                )));
            }
            stack.push(Frame {
                depth,
                node: PlanNode::new(name.trim(), PlanNodeKind::Other),
                first: line_no,
                last: line_no,
            });
            continue;
        }
        if !in_tree {
            if let Some((k, v)) = split_prop(line.trim()) {
                header.insert(k, v);
            }
            continue;
        }
        // After the first operator a line that starts in column 0 with neither a tree glyph nor
        // a space is the footer (index recommendations, notices).
        match chars.first() {
            Some(c) if !matches!(c, ' ' | '│' | '└' | '├') => footer = true,
            None => continue,
            _ => {}
        }
        if footer {
            footer_lines.push(line.to_string());
            continue;
        }
        let Some(frame) = stack.last_mut() else {
            continue;
        };
        frame.last = line_no;
        // The property text starts two columns after the node marker.
        let skip = frame_marker_col(&frame.node, frame.depth);
        let body: String = chars.iter().skip(skip).collect();
        let body = body.trim();
        if let Some((k, v)) = split_prop(body) {
            // Repeated keys (`render 0:`, `aggregate 0:`) stay distinct.
            frame.node.extra.insert(k, v);
        }
    }
    close(&mut stack, &mut roots, 0);
    if roots.is_empty() {
        return Err(PlanError::Invalid(
            "no \"• \" operator lines found in the CockroachDB plan".into(),
        ));
    }
    let analyzed = header.contains_key("execution time") || any_actual(&roots[0]);
    let mut totals = PlanTotals {
        planning_time_ms: header.get("planning time").and_then(|v| duration_ms(v)),
        execution_time_ms: header.get("execution time").and_then(|v| duration_ms(v)),
        ..PlanTotals::default()
    };
    if analyzed {
        totals.rows = roots[0].actual_rows_total;
    }
    let mut tree = finish(PlanFormat::CockroachText, analyzed, roots, totals);
    for (k, v) in &header {
        if matches!(
            k.as_str(),
            "distribution"
                | "vectorized"
                | "plan type"
                | "isolation level"
                | "rows decoded from KV"
        ) {
            tree.root.extra.insert(format!("plan {k}"), v.clone());
        }
    }
    if let Some(d) = header.get("distribution") {
        tree.root.extra.insert("distribution".into(), d.clone());
    }
    let recs = index_recommendations(&footer_lines);
    if !recs.is_empty() {
        tree.root.extra.insert(
            "crdbIndexRecommendations".into(),
            serde_json::to_string(&recs).unwrap_or_default(),
        );
    }
    Ok(tree)
}

/// One entry of the footer `index recommendations`: the server's own advice with the SQL it
/// would run (`SQL command:` / `SQL commands:`, several statements for a replacement).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
struct IndexRecommendation {
    /// `index creation`, `index replacement`...
    r#type: String,
    sql: String,
}

/// Parse the footer lines that follow the `index recommendations: N` header (that header is the
/// first footer line when the plan has recommendations; other footer lines are ignored).
fn index_recommendations(lines: &[String]) -> Vec<IndexRecommendation> {
    let mut out: Vec<IndexRecommendation> = Vec::new();
    let mut open = false;
    for line in lines {
        let t = line.trim();
        let rest = t
            .split_once(". ")
            .filter(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
        if let Some((_, body)) = rest {
            if let Some(ty) = body.strip_prefix("type:") {
                out.push(IndexRecommendation {
                    r#type: ty.trim().to_string(),
                    sql: String::new(),
                });
                open = true;
                continue;
            }
        }
        let sql = t
            .strip_prefix("SQL commands:")
            .or_else(|| t.strip_prefix("SQL command:"));
        match (sql, out.last_mut()) {
            (Some(sql), Some(rec)) if open => rec.sql = sql.trim().to_string(),
            // A long statement continues on the next, indented line.
            (None, Some(rec)) if open && !rec.sql.is_empty() && line.starts_with(' ') => {
                rec.sql.push(' ');
                rec.sql.push_str(t);
            }
            _ => open = false,
        }
    }
    out.retain(|r| !r.sql.is_empty());
    out
}

/// Column of the `•` of an operator line (after the tree glyphs and spaces), if the line is one.
fn node_marker(chars: &[char]) -> Option<usize> {
    let mut i = 0;
    while i < chars.len() && matches!(chars[i], ' ' | '│' | '└' | '├' | '─') {
        i += 1;
    }
    (chars.get(i) == Some(&'•') && chars.get(i + 1) == Some(&' ')).then_some(i)
}

/// Column where property text starts for a node at `depth` (marker column + 2).
fn frame_marker_col(_node: &PlanNode, depth: usize) -> usize {
    depth * 4 + 2
}

fn split_prop(text: &str) -> Option<(String, String)> {
    let (k, v) = text.split_once(':')?;
    let k = k.trim();
    if k.is_empty() || k.contains('(') && !k.contains(')') {
        return None;
    }
    Some((k.to_string(), v.trim().to_string()))
}

fn any_actual(n: &PlanNode) -> bool {
    n.actual_rows_total.is_some() || n.children.iter().any(any_actual)
}

/// `1,234` or `2,000 (100% of the table; ...)` to a number.
fn number(text: &str) -> Option<f64> {
    let first = text.split_whitespace().next()?;
    first.replace(',', "").parse().ok()
}

/// `895µs`, `22ms`, `1.5s`, `1m30s` to milliseconds.
fn duration_ms(text: &str) -> Option<f64> {
    let text = text.trim();
    let mut total = 0.0;
    let mut rest = text;
    let mut any = false;
    while !rest.is_empty() {
        let end = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        if end == 0 {
            return None;
        }
        let value: f64 = rest[..end].parse().ok()?;
        rest = &rest[end..];
        let (factor, len) = if rest.starts_with("ms") {
            (1.0, 2)
        } else if rest.starts_with("µs") {
            (0.001, "µs".len())
        } else if rest.starts_with("us") {
            (0.001, 2)
        } else if rest.starts_with("ns") {
            (0.000_001, 2)
        } else if rest.starts_with('h') {
            (3_600_000.0, 1)
        } else if rest.starts_with('m') {
            (60_000.0, 1)
        } else if rest.starts_with('s') {
            (1_000.0, 1)
        } else {
            return None;
        };
        total += value * factor;
        rest = &rest[len..];
        any = true;
    }
    any.then_some(total)
}

/// Turn the collected properties of a node into the normalised fields.
fn finish_node(node: &mut PlanNode, first: u32, last: u32) {
    node.raw = RawRef::Lines {
        start: first,
        end: last,
    };
    let label = node.op.clone();
    let ex = node.extra.clone();
    let get = |k: &str| ex.get(k).map(String::as_str);
    node.kind = kind_of(&label, get("spans"));
    node.est_rows = get("estimated row count").and_then(number);
    if let Some(rows) = get("actual row count").and_then(number) {
        node.set_actual(rows, 1);
        node.actual_time_ms = get("execution time").and_then(duration_ms);
    }
    // Accessed object: `table: big@big_pkey` or `into: ords(oid, bid)`.
    if let Some(t) = get("table") {
        let (name, index) = match t.split_once('@') {
            Some((n, i)) => (n, Some(i.to_string())),
            None => (t, None),
        };
        node.object = Some(PlanObject {
            schema: None,
            name: name.to_string(),
            alias: None,
            index,
        });
    } else if let Some(into) = get("into").or_else(|| get("from")) {
        let name = into.split('(').next().unwrap_or(into).trim();
        node.object = Some(PlanObject {
            schema: None,
            name: name.to_string(),
            alias: None,
            index: None,
        });
    }
    node.condition = match node.kind {
        PlanNodeKind::Filter => get("filter").map(String::from),
        PlanNodeKind::HashJoin
        | PlanNodeKind::MergeJoin
        | PlanNodeKind::NestedLoopJoin
        | PlanNodeKind::KeyLookup => join_conditions(
            ["equality", "left cols are key", "pred", "on", "lookup expr"]
                .iter()
                .filter_map(|k| {
                    get(k).map(|v| {
                        if *k == "equality" || *k == "pred" || *k == "on" || *k == "lookup expr" {
                            v.to_string()
                        } else {
                            String::new()
                        }
                    })
                })
                .collect(),
        ),
        PlanNodeKind::FullScan | PlanNodeKind::IndexScan | PlanNodeKind::IndexSeek => {
            get("filter").map(String::from)
        }
        _ => None,
    };
}

fn kind_of(label: &str, spans: Option<&str>) -> PlanNodeKind {
    use PlanNodeKind::*;
    let l = label.to_ascii_lowercase();
    let word = l.split([' ', '(']).next().unwrap_or("");
    match word {
        "scan" | "revscan" => match spans.map(str::trim) {
            // Unbounded spans read the whole index.
            Some(s) if s.starts_with("FULL SCAN") => FullScan,
            Some(s) if s.contains('-') => IndexScan,
            Some(_) => IndexSeek,
            None => FullScan,
        },
        "index" => KeyLookup,
        "lookup" | "inverted" | "cross" => NestedLoopJoin,
        "hash" => HashJoin,
        "merge" => MergeJoin,
        "zigzag" => IndexScan,
        "group" | "distinct" => {
            if l.contains("hash") {
                HashAggregate
            } else {
                Aggregate
            }
        }
        "sort" | "top-k" => Sort,
        "limit" | "offset" => Limit,
        "filter" => Filter,
        "window" => Window,
        "union" | "except" | "intersect" => Union,
        "insert" | "update" | "delete" | "upsert" | "insert-fast-path" | "delete-range" => Modify,
        "buffer" | "spool" => Materialize,
        "subquery" | "root" | "postquery" | "apply" => Subquery,
        "values" | "emptyrow" | "virtual" => Result,
        _ => Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(duration_ms("22ms"), Some(22.0));
        assert_eq!(duration_ms("895µs"), Some(0.895));
        assert_eq!(duration_ms("1.5s"), Some(1500.0));
        assert_eq!(duration_ms("1m30s"), Some(90_000.0));
        assert_eq!(duration_ms("abc"), None);
    }

    #[test]
    fn numbers() {
        assert_eq!(
            number("6,000 (100% of the table; stats collected 1 minute ago)"),
            Some(6000.0)
        );
        assert_eq!(number("0 (missing stats)"), Some(0.0));
        assert_eq!(number("n/a"), None);
    }

    #[test]
    fn footer_index_recommendations() {
        let text = "distribution: local\n\n• scan\n  table: t@t_pkey\n  spans: FULL SCAN\n\nindex recommendations: 2\n\n1. type: index creation\n   SQL command: CREATE INDEX ON db.public.t (a) STORING (b);\n\n2. type: index replacement\n   SQL commands: CREATE INDEX ON db.public.t (c); DROP INDEX db.public.t@old_idx;\n";
        let t = parse(text).unwrap();
        let json = &t.root.extra["crdbIndexRecommendations"];
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 2);
        assert_eq!(v[0]["type"], "index creation");
        assert_eq!(v[0]["sql"], "CREATE INDEX ON db.public.t (a) STORING (b);");
        assert_eq!(v[1]["type"], "index replacement");
        assert!(
            v[1]["sql"]
                .as_str()
                .unwrap()
                .ends_with("DROP INDEX db.public.t@old_idx;")
        );
        // The footer does not become an operator and a plan without one has no key.
        assert_eq!(t.node_count, 1);
        let plain = parse("• scan\n  table: t@t_pkey\n  spans: FULL SCAN\n").unwrap();
        assert!(!plain.root.extra.contains_key("crdbIndexRecommendations"));
    }

    proptest::proptest! {
        #[test]
        fn footer_parser_never_panics(lines in proptest::collection::vec("[ -~]{0,80}", 0..8)) {
            let lines: Vec<String> = lines;
            let _ = index_recommendations(&lines);
        }
    }

    #[test]
    fn empty_and_garbage_are_invalid() {
        assert!(parse("no operators here").is_err());
        assert!(matches!(parse(""), Err(PlanError::Invalid(_))));
    }
}
