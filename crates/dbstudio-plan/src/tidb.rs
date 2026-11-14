//! Parser for TiDB `EXPLAIN [ANALYZE] FORMAT='row'` results packed as JSON (gói N2).
//!
//! The driver packs the result rows as a JSON array of objects keyed by column name (`id`,
//! `estRows`, `actRows`, `task`, `access object`, `execution info`, `operator info`, `memory`,
//! `disk`; `{"rows": [...]}` is accepted too). The tree comes from the prefix of the `id` column
//! (`├─`, `└─`, `│ `, two characters per level), e.g. `  └─TableFullScan_5`.
//!
//! * Operator label = `id` without prefix and without the `_<number>` suffix (the number goes to
//!   `extra["tidb_id"]`); a trailing `(Build)` / `(Probe)` goes to `extra["role"]`.
//! * `estRows` / `actRows` fill the row estimates; `execution info` supplies the time of the
//!   operator (`time:` of the root task, else the cop task's `tikv_task` time) and `loops`.
//! * `memory` / `disk` are parsed to bytes (`memory_bytes`, `disk_bytes` in `extra`): a non-zero
//!   `disk` is a spill, reported by `analyze`.

use std::collections::BTreeMap;

use dbstudio_core::PlanFormat;

use crate::build::{finish, object};
use crate::json::{self, JVal, Js};
use crate::mysql::split_top;
use crate::{
    NodePredicate, PlanError, PlanNode, PlanNodeKind, PlanObject, PlanTotals, PlanTree,
    PredicateRole, RawRef,
};

/// Columns copied verbatim into `extra` (when not empty / `N/A`).
const EXTRA_COLUMNS: &[(&str, &str)] = &[("task", "task"), ("estCost", "est_cost_text")];

pub(crate) fn parse(text: &str) -> Result<PlanTree, PlanError> {
    let doc = json::parse(text).map_err(PlanError::Invalid)?;
    let list = match (&doc.val, doc.get("rows")) {
        (_, Some(rows)) => rows,
        (JVal::Arr(_), None) => &doc,
        _ => {
            return Err(PlanError::Invalid(
                "a TiDB plan must be a JSON array of EXPLAIN rows".into(),
            ));
        }
    };
    if list.items().is_empty() {
        return Err(PlanError::Invalid("the TiDB plan has no rows".into()));
    }

    struct Frame {
        depth: usize,
        node: PlanNode,
    }
    let mut roots: Vec<PlanNode> = Vec::new();
    let mut stack: Vec<Frame> = Vec::new();

    fn pop_to(stack: &mut Vec<Frame>, roots: &mut Vec<PlanNode>, depth: usize) {
        while stack.last().is_some_and(|f| f.depth >= depth) {
            let f = stack.pop().expect("checked non-empty");
            match stack.last_mut() {
                Some(parent) => parent.node.children.push(f.node),
                None => roots.push(f.node),
            }
        }
    }

    let mut analyzed = false;
    for (index, row) in list.items().iter().enumerate() {
        if !row.is_obj() {
            return Err(PlanError::Invalid(format!(
                "TiDB plan row {index} is not an object"
            )));
        }
        let id_text = cell(row, "id")
            .ok_or_else(|| PlanError::Invalid(format!("TiDB plan row {index} has no `id`")))?;
        let depth = id_text.chars().take_while(|c| !c.is_alphanumeric()).count() / 2;
        let node = row_node(row, &id_text, index as u32, &mut analyzed);
        pop_to(&mut stack, &mut roots, depth);
        if stack.len() >= json::MAX_DEPTH as usize {
            return Err(PlanError::Invalid(format!(
                "the TiDB plan is nested deeper than {} levels (row {index})",
                json::MAX_DEPTH
            )));
        }
        stack.push(Frame { depth, node });
    }
    pop_to(&mut stack, &mut roots, 0);

    let totals = PlanTotals {
        execution_time_ms: roots
            .first()
            .and_then(|r| r.actual_time_ms)
            .filter(|_| analyzed),
        ..PlanTotals::default()
    };
    Ok(finish(PlanFormat::TiDbRows, analyzed, roots, totals))
}

/// Text of a cell; `None` for a missing key, JSON null, `N/A` or an empty string.
fn cell(row: &Js, key: &str) -> Option<String> {
    let v = row.get_ci(key)?;
    let s = match &v.val {
        JVal::Null => return None,
        JVal::Str(s) => s.clone(),
        _ => v.display(),
    };
    let t = s.trim_end();
    (!t.is_empty() && t != "N/A").then(|| t.to_string())
}

fn row_node(row: &Js, id_text: &str, index: u32, analyzed: &mut bool) -> PlanNode {
    let label = id_text.trim_start_matches(|c: char| !c.is_alphanumeric());
    let (label, role) = match label.rfind('(') {
        Some(i) if label.ends_with(')') => (&label[..i], Some(&label[i + 1..label.len() - 1])),
        _ => (label, None),
    };
    let (op, tidb_id) = match label.rsplit_once('_') {
        Some((op, n)) if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) => (op, Some(n)),
        _ => (label, None),
    };
    let mut n = PlanNode::new(op, kind_of(op));
    n.raw = RawRef::Row { id: index };
    if let Some(n_id) = tidb_id {
        n.put("tidb_id", n_id);
    }
    if let Some(role) = role {
        n.put("role", role.to_ascii_lowercase());
    }
    for (col, key) in EXTRA_COLUMNS {
        if let Some(v) = cell(row, col) {
            n.put(key, v);
        }
    }
    n.est_rows = cell(row, "estRows").and_then(|v| v.trim().parse::<f64>().ok());
    n.est_cost = cell(row, "estCost").and_then(|v| v.trim().parse::<f64>().ok());
    n.object = cell(row, "access object").and_then(|a| access_object(&a));
    n.condition = cell(row, "operator info");
    if let Some(info) = n.condition.clone() {
        n.predicates = predicates_of(op, &info);
    }
    if let Some(cols) = cell(row, "access object").and_then(|a| index_columns_of(&a)) {
        n.put("index_columns", cols);
    }

    let info = cell(row, "execution info");
    let act_rows = cell(row, "actRows").and_then(|v| v.trim().parse::<f64>().ok());
    if let Some(rows) = act_rows {
        *analyzed = true;
        // TiDB's `loops` counts `Next` calls, not executions: every operator ran once.
        n.set_actual(rows, 1);
    }
    if let Some(info) = &info {
        let stats = ExecInfo::parse(info);
        n.actual_time_ms = stats.time_ms;
        if let Some(l) = stats.loops {
            n.put("next_calls", l);
        }
        if let Some(rows) = stats.cop_tasks {
            n.put("cop_tasks", rows);
        }
        n.put("execution_info", info.clone());
    }
    if let Some(m) = cell(row, "memory") {
        if let Some(b) = parse_bytes(&m) {
            n.put("memory_bytes", json::fmt_num(b));
        }
        n.put("memory", m);
    }
    if let Some(d) = cell(row, "disk") {
        if let Some(b) = parse_bytes(&d) {
            n.put("disk_bytes", json::fmt_num(b));
        }
        n.put("disk", d);
    }
    n
}

/// `index:idx_customer(customer_id, status)` of an access object → `customer_id, status`.
fn index_columns_of(text: &str) -> Option<String> {
    let i = text.find("index:")?;
    let rest = &text[i + 6..];
    let open = rest.find('(')?;
    let mut depth = 0i32;
    for (k, c) in rest[open..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(rest[open + 1..open + k].trim().to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// Name and argument text of a prefix function call `name(args)` that spans all of `t`.
fn call(t: &str) -> Option<(&str, &str)> {
    let open = t.find('(')?;
    let name = &t[..open];
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut chars = t[open..].char_indices();
    while let Some((i, c)) = chars.next() {
        if let Some(q) = quote {
            if c == '\\' {
                chars.next();
            } else if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' | '`' => quote = Some(c),
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return (open + i + 1 == t.len()).then(|| (name, &t[open + 1..open + i]));
                }
            }
            _ => {}
        }
    }
    None
}

/// TiDB prints expressions in prefix form (`gt(db.t.a, 5)`); rewrite the known operators as
/// infix SQL so the advisor can read them. Unknown functions keep their name.
fn expr_sql(t: &str) -> String {
    let t = t.trim();
    let Some((name, args)) = call(t) else {
        return t.to_string();
    };
    let parts: Vec<String> = split_top(args).iter().map(|a| expr_sql(a)).collect();
    let bin = |op: &str| format!("{} {op} {}", parts[0], parts[1]);
    match (name, parts.len()) {
        ("eq", 2) => bin("="),
        ("ne", 2) => bin("<>"),
        ("gt", 2) => bin(">"),
        ("ge", 2) => bin(">="),
        ("lt", 2) => bin("<"),
        ("le", 2) => bin("<="),
        ("plus", 2) => bin("+"),
        ("minus", 2) => bin("-"),
        ("mul", 2) => bin("*"),
        ("div", 2) => bin("/"),
        ("and", 2) => format!("({}) AND ({})", parts[0], parts[1]),
        ("or", 2) => format!("({}) OR ({})", parts[0], parts[1]),
        ("isnull", 1) => format!("{} IS NULL", parts[0]),
        ("not", 1) => match parts[0].strip_suffix(" IS NULL") {
            Some(col) => format!("{col} IS NOT NULL"),
            None => format!("NOT ({})", parts[0]),
        },
        ("in", n) if n >= 2 => format!("{} IN ({})", parts[0], parts[1..].join(", ")),
        ("like", 2 | 3) => bin("LIKE"),
        _ => format!("{name}({})", parts.join(", ")),
    }
}

/// Conditions of an operator with their roles (the `operator info` text of a row).
fn predicates_of(op: &str, info: &str) -> Vec<NodePredicate> {
    let mut out = Vec::new();
    let mut add = |role: PredicateRole, text: String| {
        if !text.trim().is_empty() {
            out.push(NodePredicate { role, text });
        }
    };
    match op {
        "Selection" => {
            for c in split_top(info) {
                add(PredicateRole::Filter, expr_sql(&c));
            }
        }
        "Sort" | "TopN" => {
            for k in split_top(info) {
                if k.starts_with("offset:") || k.starts_with("count:") {
                    continue;
                }
                let text = match k.rsplit_once(':') {
                    Some((col, "desc")) => format!("{} DESC", expr_sql(col)),
                    Some((col, "asc")) => expr_sql(col),
                    _ => expr_sql(&k),
                };
                add(PredicateRole::SortKey, text);
            }
        }
        "HashJoin" | "MergeJoin" | "IndexJoin" | "IndexHashJoin" | "IndexMergeJoin" => {
            if let Some(i) = info.find("equal:[") {
                let rest = &info[i + 7..];
                let inner = rest.split_once(']').map_or(rest, |(a, _)| a);
                for c in split_top(inner) {
                    add(PredicateRole::JoinCond, expr_sql(&c));
                }
            } else if let Some(i) = info.find("equal cond:") {
                for c in split_top(&info[i + 11..]) {
                    if call(&c).is_some() {
                        add(PredicateRole::JoinCond, expr_sql(&c));
                    } else {
                        break;
                    }
                }
            }
        }
        _ => {}
    }
    out
}

/// `table:orders, index:idx_customer(customer_id), partition:p0` → table and index.
fn access_object(text: &str) -> Option<PlanObject> {
    let mut table = None;
    let mut index = None;
    // Split on `, ` outside parentheses (index column lists contain commas).
    let mut depth = 0u32;
    let mut start = 0;
    let bytes = text.as_bytes();
    let mut parts = Vec::new();
    for (i, b) in bytes.iter().enumerate() {
        match b {
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                parts.push(&text[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&text[start..]);
    for p in parts {
        let p = p.trim();
        if let Some(t) = p.strip_prefix("table:") {
            table = Some(t.trim().to_string());
        } else if let Some(i) = p.strip_prefix("index:") {
            let name = i.split('(').next().unwrap_or(i).trim();
            index = Some(name.to_string());
        }
    }
    let table = table?;
    let mut o = object(&table);
    o.index = index;
    Some(o)
}

fn kind_of(op: &str) -> PlanNodeKind {
    match op {
        "TableFullScan" => PlanNodeKind::FullScan,
        "IndexFullScan" | "IndexRangeScan" | "TableRangeScan" => PlanNodeKind::IndexScan,
        "Point_Get" | "Batch_Point_Get" => PlanNodeKind::IndexSeek,
        "TableRowIDScan" | "IndexLookUp" | "IndexMerge" => PlanNodeKind::KeyLookup,
        "TableReader" | "IndexReader" | "ExchangeSender" | "ExchangeReceiver" | "Shuffle"
        | "ShuffleReceiver" => PlanNodeKind::Gather,
        "HashJoin" => PlanNodeKind::HashJoin,
        "MergeJoin" => PlanNodeKind::MergeJoin,
        "IndexJoin" | "IndexHashJoin" | "IndexMergeJoin" | "Apply" => PlanNodeKind::NestedLoopJoin,
        "HashAgg" => PlanNodeKind::HashAggregate,
        "StreamAgg" => PlanNodeKind::Aggregate,
        "Sort" | "TopN" => PlanNodeKind::Sort,
        "Limit" => PlanNodeKind::Limit,
        "Selection" => PlanNodeKind::Filter,
        "Window" => PlanNodeKind::Window,
        "Union" => PlanNodeKind::Union,
        "CTEFullScan" | "CTE" => PlanNodeKind::Materialize,
        "TableDual" => PlanNodeKind::Result,
        "Insert" | "Update" | "Delete" | "Replace" => PlanNodeKind::Modify,
        _ => PlanNodeKind::Other,
    }
}

/// What the `execution info` column tells about one operator.
#[derive(Debug, Default, PartialEq)]
struct ExecInfo {
    time_ms: Option<f64>,
    loops: Option<String>,
    cop_tasks: Option<String>,
}

impl ExecInfo {
    fn parse(info: &str) -> ExecInfo {
        let mut out = ExecInfo::default();
        let mut tikv_time = None;
        for seg in top_level_segments(info) {
            let Some((key, value)) = seg.split_once(':') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "time" if out.time_ms.is_none() => out.time_ms = parse_duration_ms(value),
                "loops" if out.loops.is_none() => out.loops = Some(value.to_string()),
                "cop_task" => {
                    // `{num: 4, max: ...}`
                    let inner = value.trim_start_matches('{');
                    out.cop_tasks = inner
                        .split(',')
                        .find_map(|p| p.trim().strip_prefix("num:"))
                        .map(|n| n.trim().to_string());
                }
                "tikv_task" => {
                    let inner = value.trim_start_matches('{').trim_end_matches('}');
                    for part in top_level_segments(inner) {
                        let Some((k, v)) = part.split_once(':') else {
                            continue;
                        };
                        match k.trim() {
                            "time" => tikv_time = tikv_time.or(parse_duration_ms(v.trim())),
                            "proc max" => tikv_time = tikv_time.or(parse_duration_ms(v.trim())),
                            "loops" if out.loops.is_none() => {
                                out.loops = Some(v.trim().to_string());
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        if out.time_ms.is_none() {
            out.time_ms = tikv_time;
        }
        out
    }
}

/// Comma separated segments of `text` outside `{}`, `[]` and `()`.
fn top_level_segments(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (i, c) in text.char_indices() {
        match c {
            '{' | '[' | '(' => depth += 1,
            '}' | ']' | ')' => depth -= 1,
            ',' if depth == 0 => {
                out.push(text[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(text[start..].trim());
    out.retain(|s| !s.is_empty());
    out
}

/// Go style durations: `6.69ms`, `516.3µs`, `1.2s`, `1m2.5s`, `5ns`.
fn parse_duration_ms(text: &str) -> Option<f64> {
    let mut rest = text.trim();
    if rest.is_empty() {
        return None;
    }
    let mut total = 0.0;
    while !rest.is_empty() {
        let end = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        if end == 0 {
            return None;
        }
        let value: f64 = rest[..end].parse().ok()?;
        rest = &rest[end..];
        let unit_end = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        let factor = match &rest[..unit_end] {
            "ns" => 1e-6,
            "µs" | "μs" | "us" => 1e-3,
            "ms" => 1.0,
            "s" => 1_000.0,
            "m" => 60_000.0,
            "h" => 3_600_000.0,
            _ => return None,
        };
        total += value * factor;
        rest = &rest[unit_end..];
    }
    Some(total)
}

/// `2.98 KB`, `0 Bytes`, `1.2 GB` → bytes.
fn parse_bytes(text: &str) -> Option<f64> {
    let mut it = text.split_whitespace();
    let value: f64 = it.next()?.parse().ok()?;
    let unit = it.next().unwrap_or("Bytes");
    let factor = match unit {
        "Bytes" | "bytes" | "B" => 1.0,
        "KB" => 1024.0,
        "MB" => 1024.0 * 1024.0,
        "GB" => 1024.0 * 1024.0 * 1024.0,
        "TB" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some(value * factor)
}

/// Used by `analyze`: bytes spilled to disk by `node` (TiDB `disk` column), when any.
pub(crate) fn spilled_bytes(extra: &BTreeMap<String, String>) -> Option<f64> {
    extra
        .get("disk_bytes")
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|b| *b > 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration_ms("6.69ms"), Some(6.69));
        assert_eq!(parse_duration_ms("1.5s"), Some(1500.0));
        assert_eq!(parse_duration_ms("1m2.5s"), Some(62_500.0));
        assert_eq!(parse_duration_ms("500µs"), Some(0.5));
        assert_eq!(parse_duration_ms("2ns"), Some(2e-6));
        assert_eq!(parse_duration_ms("nonsense"), None);
        assert_eq!(parse_duration_ms(""), None);
    }

    #[test]
    fn byte_sizes() {
        assert_eq!(parse_bytes("2.5 KB"), Some(2560.0));
        assert_eq!(parse_bytes("0 Bytes"), Some(0.0));
        assert_eq!(parse_bytes("1 GB"), Some(1_073_741_824.0));
        assert_eq!(parse_bytes("N/A"), None);
    }

    #[test]
    fn exec_info_prefers_the_root_task_time() {
        let i = ExecInfo::parse(
            "time:2.97ms, loops:2, cop_task: {num: 1, max: 183.2µs, proc_keys: 0}, rpc_info:{Cop:{num_rpc:2}}",
        );
        assert_eq!(i.time_ms, Some(2.97));
        assert_eq!(i.loops.as_deref(), Some("2"));
        assert_eq!(i.cop_tasks.as_deref(), Some("1"));
        let cop = ExecInfo::parse("tikv_task:{time:118.1µs, loops:0}");
        assert_eq!(cop.time_ms, Some(0.1181));
        assert_eq!(cop.loops.as_deref(), Some("0"));
        let proc = ExecInfo::parse("tikv_task:{proc max:13.7ms, min:547.8µs, tasks:9}");
        assert_eq!(proc.time_ms, Some(13.7));
    }

    #[test]
    fn access_objects() {
        let o = access_object("table:orders, index:idx_customer(customer_id, status)").unwrap();
        assert_eq!(o.name, "orders");
        assert_eq!(o.index.as_deref(), Some("idx_customer"));
        let o = access_object("table:c, partition:p0").unwrap();
        assert_eq!((o.name.as_str(), o.index), ("c", None));
        assert!(access_object("data:Selection_6").is_none());
    }

    #[test]
    fn depth_comes_from_the_prefix() {
        let text = r#"[
          {"id":"TableReader_7","estRows":"10.00","task":"root","access object":"","operator info":"data:Selection_6"},
          {"id":"└─Selection_6","estRows":"10.00","task":"cop[tikv]","access object":"","operator info":"eq(t.a.v, \"x\")"},
          {"id":"  └─TableFullScan_5","estRows":"10000.00","task":"cop[tikv]","access object":"table:a","operator info":"keep order:false, stats:pseudo"}
        ]"#;
        let t = parse(text).unwrap();
        assert!(!t.analyzed);
        assert_eq!(t.node_count, 3);
        assert_eq!(t.root.op, "TableReader");
        assert_eq!(t.root.children[0].children[0].op, "TableFullScan");
        assert_eq!(t.root.children[0].children[0].kind, PlanNodeKind::FullScan);
        assert_eq!(t.root.children[0].children[0].est_rows, Some(10000.0));
    }

    #[test]
    fn rejects_bad_input() {
        assert!(parse("[]").is_err());
        assert!(parse("{}").is_err());
        assert!(parse("[1]").is_err());
        assert!(parse(r#"[{"estRows":"1"}]"#).is_err());
    }
}
