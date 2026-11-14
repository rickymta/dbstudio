//! CockroachDB rules (design section 4.7). YugabyteDB YSQL plans are PostgreSQL JSON and are
//! served by `pg.rs`; TiDB has its own module.
//!
//! CockroachDB prints its own index recommendations under the plan (`index recommendations:`),
//! which the parser keeps as `crdbIndexRecommendations`. The advisor shows those statements
//! verbatim instead of guessing columns from the filter: the server saw the real statistics.

use std::collections::BTreeMap;

use dbstudio_core::PlanFormat;

use super::common::{ev, ev_text, table_ref};
use super::model::{
    ActionKind, ActionScope, AdviceAction, AdviceRule, Caveat, Confidence, EvidenceUnit, LockRisk,
    TableRef,
};
use super::{Cx, Draft};
use crate::json::{self, fmt_num};
use crate::{PlanNode, PlanNodeKind, PlanRule, Severity};

/// Rules evaluated for every node of a plan of this family.
pub(crate) fn node_rules(cx: &Cx<'_>, node: &PlanNode, out: &mut Vec<Draft>) {
    if cx.tree.format != PlanFormat::CockroachText || node.loops == Some(0) {
        return;
    }
    missing_stats(cx, node, out);
    temp_disk_spill(node, out);
}

/// Rules evaluated once per plan.
pub(crate) fn tree_rules(cx: &Cx<'_>, out: &mut Vec<Draft>) {
    if cx.tree.format != PlanFormat::CockroachText {
        return;
    }
    index_recommendations(cx, out);
}

fn caveat(key: &str, verify: bool) -> Caveat {
    Caveat {
        key: key.into(),
        verify,
        params: BTreeMap::new(),
    }
}

fn action(
    kind: ActionKind,
    label: &str,
    sql: String,
    scope: ActionScope,
    lock: LockRisk,
) -> AdviceAction {
    AdviceAction {
        kind,
        label_key: label.into(),
        sql: Some(sql),
        template: false,
        scope,
        lock: Some(lock),
        params: BTreeMap::new(),
    }
}

/// Quote a name for CockroachDB (PostgreSQL rules: only when it is not a plain lower-case word).
fn quote(name: &str) -> String {
    super::index::quote_ident(dbstudio_core::ScriptDialect::Postgres, name)
}

// ------------------------------------------------------------------------------------------
// crdbIndexRecommendation
// ------------------------------------------------------------------------------------------

/// The `CREATE INDEX` part of a statement: table, key and `STORING` columns.
struct CreateIndex {
    table: TableRef,
    columns: String,
    storing: String,
}

/// Text inside the first balanced parentheses of `s`, and what follows them.
fn parenthesised(s: &str) -> Option<(&str, &str)> {
    let open = s.find('(')?;
    let mut depth = 0;
    for (i, c) in s[open..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some((&s[open + 1..open + i], &s[open + i + 1..]));
                }
            }
            _ => {}
        }
    }
    None
}

fn unquote(s: &str) -> String {
    s.trim().trim_matches('"').to_string()
}

/// `CREATE [UNIQUE|INVERTED] INDEX [IF NOT EXISTS] [name] ON db.schema.table (cols) [STORING (cols)]`.
fn parse_create_index(stmt: &str) -> Option<CreateIndex> {
    let upper = stmt.to_ascii_uppercase();
    if !upper.trim_start().starts_with("CREATE") || !upper.contains(" INDEX ") {
        return None;
    }
    let on = upper.find(" ON ")? + 4;
    let rest = stmt.get(on..)?.trim_start();
    let name_end = rest.find(|c: char| c.is_whitespace() || c == '(')?;
    let qualified = &rest[..name_end];
    let parts: Vec<String> = qualified.split('.').map(unquote).collect();
    let table = match parts.as_slice() {
        [t] => TableRef {
            database: None,
            schema: None,
            name: t.clone(),
            alias: None,
            path: None,
        },
        [s, t] => TableRef {
            database: None,
            schema: Some(s.clone()),
            name: t.clone(),
            alias: None,
            path: None,
        },
        [d, s, t] => TableRef {
            database: Some(d.clone()),
            schema: Some(s.clone()),
            name: t.clone(),
            alias: None,
            path: None,
        },
        _ => return None,
    };
    let (columns, after) = parenthesised(&rest[name_end..])?;
    let storing = after
        .to_ascii_uppercase()
        .find("STORING")
        .or_else(|| after.to_ascii_uppercase().find("INCLUDE"))
        .and_then(|p| parenthesised(&after[p..]))
        .map(|(c, _)| c.trim().to_string())
        .unwrap_or_default();
    Some(CreateIndex {
        table,
        columns: columns.trim().to_string(),
        storing,
    })
}

/// Statements of a recommendation (`a; b;`), without the trailing semicolons.
fn statements(sql: &str) -> Vec<String> {
    sql.split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn index_recommendations(cx: &Cx<'_>, out: &mut Vec<Draft>) {
    let Some(raw) = cx.tree.root.extra.get("crdbIndexRecommendations") else {
        return;
    };
    let Ok(doc) = json::parse(raw) else { return };
    for rec in doc.items() {
        let (Some(ty), Some(sql)) = (rec.text("type"), rec.text("sql")) else {
            continue;
        };
        let stmts = statements(sql);
        let Some(create) = stmts.iter().find_map(|s| parse_create_index(s)) else {
            continue;
        };
        // The scans of that table: full scans first.
        let mut scans: Vec<&PlanNode> = cx
            .nodes
            .iter()
            .copied()
            .filter(|n| {
                matches!(
                    n.kind,
                    PlanNodeKind::FullScan
                        | PlanNodeKind::IndexScan
                        | PlanNodeKind::IndexSeek
                        | PlanNodeKind::KeyLookup
                ) && n
                    .object
                    .as_ref()
                    .is_some_and(|o| o.name.eq_ignore_ascii_case(&create.table.name))
            })
            .collect();
        scans.sort_by_key(|n| n.kind != PlanNodeKind::FullScan);
        let mut d = Draft::new(
            AdviceRule::CrdbIndexRecommendation,
            Severity::Warning,
            Confidence::High,
        )
        .param("columns", create.columns.clone())
        .param("recommendation", ty.to_string());
        if !create.storing.is_empty() {
            d.params.insert("storing".into(), create.storing.clone());
        }
        d.fingerprint_key = stmts.join(";").to_lowercase();
        for n in &scans {
            d = d.node(n.id);
        }
        if let Some(n) = scans.first() {
            let (rows, actual) = match n.actual_rows_total {
                Some(a) => (Some(a), true),
                None => (n.est_rows, false),
            };
            if let Some(r) = rows {
                d.evidence
                    .push(ev("rowsRead", Some(n.id), r, EvidenceUnit::Rows, actual));
                d.params.insert("examined".into(), fmt_num(r));
                d.examined = Some(r);
            }
            if let Some(t) = n.self_time_ms {
                d.evidence
                    .push(ev("selfTime", Some(n.id), t, EvidenceUnit::Ms, true));
            }
            if cx
                .tree
                .warnings
                .iter()
                .any(|w| w.node_id == n.id && w.rule == PlanRule::FullScanLargeTable)
            {
                d.from_warnings.push(PlanRule::FullScanLargeTable);
            }
        }
        d.evidence.push(ev_text("recommendationType", None, ty));
        let mut created = false;
        for s in &stmts {
            let text = format!("{s};");
            let upper = s.to_ascii_uppercase();
            if upper.starts_with("CREATE") && upper.contains(" INDEX ") && !created {
                created = true;
                d.actions.insert(
                    0,
                    action(
                        ActionKind::CreateIndex,
                        "createIndex",
                        text,
                        ActionScope::Object,
                        LockRisk::None,
                    ),
                );
            } else if upper.starts_with("DROP INDEX") {
                d.actions.push(action(
                    ActionKind::Maintenance,
                    "dropReplacedIndex",
                    text,
                    ActionScope::Object,
                    LockRisk::None,
                ));
                d.caveats.push(caveat("dropAfterVerify", true));
            } else {
                d.actions.push(action(
                    ActionKind::Maintenance,
                    "serverRecommendedStatement",
                    text,
                    ActionScope::Object,
                    LockRisk::None,
                ));
            }
        }
        d.caveats.push(caveat("writeOverhead", false));
        d.table = Some(create.table);
        out.push(d);
    }
}

// ------------------------------------------------------------------------------------------
// crdbMissingStats
// ------------------------------------------------------------------------------------------

fn missing_stats(cx: &Cx<'_>, node: &PlanNode, out: &mut Vec<Draft>) {
    // A misestimate on the same node is already answered by `staleStatistics` (and `ANALYZE`
    // is `CREATE STATISTICS` in CockroachDB): do not say the same thing twice.
    if cx
        .tree
        .warnings
        .iter()
        .any(|w| w.node_id == node.id && w.rule == PlanRule::Misestimate)
    {
        return;
    }
    let missing = node
        .extra
        .get("estimated row count")
        .is_some_and(|v| v.contains("(missing stats)"));
    if !missing {
        return;
    }
    let Some(table) = table_ref(node) else { return };
    let severity = if node.kind == PlanNodeKind::Modify {
        Severity::Info
    } else {
        Severity::Warning
    };
    let mut d = Draft::new(AdviceRule::CrdbMissingStats, severity, Confidence::High).node(node.id);
    d.fingerprint_key = String::new();
    d.evidence.push(ev_text(
        "statistics",
        Some(node.id),
        "missing stats".to_string(),
    ));
    let name: String = table
        .name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let mut a = action(
        ActionKind::Statistics,
        "refreshStatistics",
        format!(
            "CREATE STATISTICS {} FROM {};",
            quote(&format!("s_{name}")),
            quote(&table.name)
        ),
        ActionScope::Object,
        LockRisk::None,
    );
    a.params.insert("table".into(), table.name.clone());
    d.actions.push(a);
    d.table = Some(table);
    out.push(d);
}

// ------------------------------------------------------------------------------------------
// sortSpill / hashSpill from the temporary disk usage
// ------------------------------------------------------------------------------------------

/// `12 MiB`, `340 KiB`, `1.5 GiB`, `0 B` to bytes.
fn bytes(text: &str) -> Option<f64> {
    let mut it = text.split_whitespace();
    let value: f64 = it.next()?.replace(',', "").parse().ok()?;
    let factor = match it.next()? {
        "B" => 1.0,
        "KiB" => 1024.0,
        "MiB" => 1024.0 * 1024.0,
        "GiB" => 1024.0 * 1024.0 * 1024.0,
        "TiB" => 1024.0_f64.powi(4),
        _ => return None,
    };
    Some(value * factor)
}

fn pow2_at_least(n: f64) -> u64 {
    let mut p = 1u64;
    while (p as f64) < n && p < (1 << 40) {
        p <<= 1;
    }
    p
}

fn temp_disk_spill(node: &PlanNode, out: &mut Vec<Draft>) {
    let measured = node
        .extra
        .get("max sql temp disk usage")
        .and_then(|v| bytes(v));
    let estimated = node
        .extra
        .get("estimated max sql temp disk usage")
        .and_then(|v| bytes(v));
    let (disk, actual) = match (measured, estimated) {
        (Some(m), _) if m > 0.0 => (m, true),
        (_, Some(e)) if e > 0.0 => (e, false),
        _ => return,
    };
    let rule = if node.kind == PlanNodeKind::Sort {
        AdviceRule::SortSpill
    } else {
        AdviceRule::HashSpill
    };
    let mut d = Draft::new(rule, Severity::Warning, Confidence::Medium).node(node.id);
    d.fingerprint_key = format!("node:{}", node.op);
    d.evidence.push(ev(
        "spillBytes",
        Some(node.id),
        disk,
        EvidenceUnit::Bytes,
        actual,
    ));
    if let Some(m) = node
        .extra
        .get("estimated max memory allocated")
        .or_else(|| node.extra.get("max memory allocated"))
    {
        d.evidence
            .push(ev_text("memoryUsed", Some(node.id), m.clone()));
    }
    // The data that went to disk had to fit in memory: ask for twice its size, rounded up.
    let mib = (pow2_at_least(2.0 * disk / (1024.0 * 1024.0)) as f64).clamp(128.0, 4096.0) as u64;
    d.params.insert("workmem".into(), format!("{mib}MiB"));
    d.actions.push(action(
        ActionKind::SessionSetting,
        "raiseWorkMem",
        format!("SET distsql_workmem = '{mib}MiB';"),
        ActionScope::Session,
        LockRisk::None,
    ));
    d.caveats.push(caveat("workMemMultiplies", false));
    d.caveats.push(caveat("spillEstimate", false));
    d.table = table_ref(node);
    out.push(d);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_index_parsing() {
        let c = parse_create_index("CREATE INDEX ON dbstudio.sv.big (name) STORING (grp, amount)")
            .unwrap();
        assert_eq!(
            (
                c.table.database.as_deref(),
                c.table.schema.as_deref(),
                c.table.name.as_str(),
                c.columns.as_str(),
                c.storing.as_str()
            ),
            (Some("dbstudio"), Some("sv"), "big", "name", "grp, amount")
        );
        let c = parse_create_index("CREATE INDEX IF NOT EXISTS ix ON t (a ASC, b DESC)").unwrap();
        assert_eq!(
            (c.table.name.as_str(), c.columns.as_str()),
            ("t", "a ASC, b DESC")
        );
        assert!(c.storing.is_empty());
        assert!(parse_create_index("DROP INDEX db.public.t@x").is_none());
        assert!(parse_create_index("CREATE INDEX").is_none());
        assert_eq!(statements("a; b;"), vec!["a".to_string(), "b".to_string()]);
    }

    use crate::advice::model::PlanAdvice;
    use crate::{PlanRules, parse_and_analyze};
    use dbstudio_core::RawPlan;

    fn advise(text: &str) -> Vec<PlanAdvice> {
        let raw = RawPlan {
            format: PlanFormat::CockroachText,
            text: text.into(),
            statement: String::new(),
            analyzed: false,
            rolled_back: false,
            server_version: "25.4.20".into(),
            captured_at: "2025-01-01T00:00:00Z".into(),
            extra: BTreeMap::new(),
        };
        parse_and_analyze(&raw, &PlanRules::default())
            .expect("plan parses")
            .advice
    }

    #[test]
    fn replacement_recommendation_has_create_and_drop() {
        let text = "distribution: local\n\n• scan\n  table: t@t_pkey\n  spans: FULL SCAN\n\nindex recommendations: 1\n\n1. type: index replacement\n   SQL commands: CREATE INDEX ON db.public.t (c) STORING (d); DROP INDEX db.public.t@old_idx;\n";
        let a = advise(text);
        let r = a
            .iter()
            .find(|x| x.rule == AdviceRule::CrdbIndexRecommendation)
            .unwrap();
        assert_eq!(r.actions[0].kind, ActionKind::CreateIndex);
        assert_eq!(
            r.actions[0].sql.as_deref(),
            Some("CREATE INDEX ON db.public.t (c) STORING (d);")
        );
        assert_eq!(r.actions[1].label_key, "dropReplacedIndex");
        assert!(
            r.caveats
                .iter()
                .any(|c| c.key == "dropAfterVerify" && c.verify)
        );
        assert_eq!(r.node_ids, vec![0]);
    }

    #[test]
    fn missing_stats_and_spill() {
        let text = "distribution: local\n\n• sort\n│ estimated row count: 100\n│ estimated max sql temp disk usage: 300 MiB\n│ order: +a\n│\n└── • scan\n      estimated row count: 5,000 (missing stats)\n      table: big@big_pkey\n      spans: FULL SCAN\n";
        let a = advise(text);
        let m = a
            .iter()
            .find(|x| x.rule == AdviceRule::CrdbMissingStats)
            .unwrap();
        assert_eq!(
            m.actions[0].sql.as_deref(),
            Some("CREATE STATISTICS s_big FROM big;")
        );
        let s = a.iter().find(|x| x.rule == AdviceRule::SortSpill).unwrap();
        assert_eq!(
            s.actions[0].sql.as_deref(),
            Some("SET distsql_workmem = '1024MiB';")
        );
    }

    #[test]
    fn other_formats_are_left_alone() {
        // TiDB plans also reach this module through the dispatcher.
        let mut raw = RawPlan {
            format: PlanFormat::TiDbRows,
            text: String::new(),
            statement: String::new(),
            analyzed: false,
            rolled_back: false,
            server_version: "8.0.11-TiDB-v8.5.7".into(),
            captured_at: String::new(),
            extra: BTreeMap::new(),
        };
        raw.text = "id\testRows\ttask\taccess object\toperator info\nTableReader_5\t10\troot\t\tdata:TableFullScan_4\n".into();
        let tree = parse_and_analyze(&raw, &PlanRules::default());
        if let Ok(t) = tree {
            assert!(
                t.advice
                    .iter()
                    .all(|a| a.rule != AdviceRule::CrdbIndexRecommendation)
            );
        }
    }

    proptest::proptest! {
        #[test]
        fn create_index_parser_never_panics(s in "[ -~]{0,160}") {
            let _ = parse_create_index(&s);
            let _ = statements(&s);
            let _ = bytes(&s);
        }
    }

    #[test]
    fn sizes() {
        assert_eq!(bytes("0 B"), Some(0.0));
        assert_eq!(bytes("340 KiB"), Some(340.0 * 1024.0));
        assert_eq!(bytes("1.5 GiB"), Some(1.5 * 1024.0 * 1024.0 * 1024.0));
        assert_eq!(bytes("lots"), None);
    }
}
