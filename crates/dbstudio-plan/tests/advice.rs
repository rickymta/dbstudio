//! Query Advisor on every captured plan: golden advice per fixture (`advice_fixtures/*.advice.json`,
//! rewritten with `UPDATE_FIXTURES=1`) and the invariants of design section 11.1.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use dbstudio_core::{PlanFormat, RawPlan, ScriptDialect, StatementClass};
use dbstudio_plan::{
    ActionKind, ActionScope, AdvisorContext, PlanAdvice, PlanRules, PlanTree, advise, parse,
    parse_and_analyze,
};
use serde_json::{Value, json};

struct Case {
    /// Unique name, also the golden file stem.
    name: String,
    format: PlanFormat,
    version: &'static str,
    text: String,
    statement: String,
}

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests")
}

fn statement_of(name: &str) -> String {
    let stem = name.rsplit_once('.').map_or(name, |(s, _)| s);
    std::fs::read_to_string(root().join("advice_fixtures").join(format!("{stem}.sql")))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn corpus() -> Vec<Case> {
    let mut out = Vec::new();
    let mut add = |dir: &str, pick: &dyn Fn(&str) -> Option<(PlanFormat, &'static str)>| {
        let mut names: Vec<String> = std::fs::read_dir(root().join(dir))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| !n.starts_with('.'))
            .collect();
        names.sort();
        for name in names {
            let Some((format, version)) = pick(&name) else {
                continue;
            };
            let stem = name
                .rsplit_once('.')
                .map_or(name.clone(), |(s, _)| s.to_string());
            out.push(Case {
                text: std::fs::read_to_string(root().join(dir).join(&name)).unwrap(),
                statement: statement_of(&name),
                name: stem,
                format,
                version,
            });
        }
    };
    add("fixtures", &|n| {
        let f = match n {
            n if n.starts_with("pg_") => PlanFormat::PostgresJson,
            n if n.starts_with("mysql_jsonv2") => PlanFormat::MySqlJsonV2,
            n if n.starts_with("mysql_json") => PlanFormat::MySqlJson,
            n if n.starts_with("mysql_tree") => PlanFormat::MySqlTree,
            n if n.starts_with("mariadb_") => PlanFormat::MariaDbJson,
            n if n.starts_with("mssql_") => PlanFormat::MsSqlShowplanXml,
            n if n.starts_with("oracle_") => PlanFormat::OraclePlanRows,
            n if n.starts_with("mongo_") => PlanFormat::MongoExplain,
            _ => return None,
        };
        Some((f, "16.2"))
    });
    add("fixtures_cockroach", &|_| {
        Some((PlanFormat::CockroachText, "25.4.20"))
    });
    add("fixtures_tidb", &|_| {
        Some((PlanFormat::TiDbRows, "8.0.11-TiDB-v8.5.7"))
    });
    add("clickhouse", &|n| {
        n.ends_with(".json")
            .then_some((PlanFormat::ClickHouseJson, "26.3"))
    });
    out
}

fn raw_of(c: &Case) -> RawPlan {
    RawPlan {
        format: c.format,
        text: c.text.clone(),
        statement: c.statement.clone(),
        analyzed: false,
        rolled_back: false,
        server_version: c.version.into(),
        captured_at: "2025-01-01T00:00:00Z".into(),
        extra: BTreeMap::new(),
    }
}

fn tree_of(c: &Case) -> PlanTree {
    parse_and_analyze(&raw_of(c), &PlanRules::default())
        .unwrap_or_else(|e| panic!("{}: {e}", c.name))
}

/// What the golden file pins about one advice.
fn summary(a: &PlanAdvice) -> Value {
    let sqls: Vec<Value> = a
        .actions
        .iter()
        .map(|x| {
            json!({
                "kind": x.kind, "label": x.label_key, "scope": x.scope, "template": x.template,
                "lock": x.lock, "sql": x.sql,
            })
        })
        .collect();
    json!({
        "rule": a.rule,
        "severity": a.severity,
        "confidence": a.confidence,
        "nodeIds": a.node_ids,
        "table": a.table.as_ref().map(|t| match &t.schema {
            Some(s) => format!("{s}.{}", t.name),
            None => t.name.clone(),
        }),
        "params": a.params,
        "evidence": a.evidence.iter().map(|e| json!({
            "key": e.key, "nodeId": e.node_id, "value": e.value, "text": e.text,
            "unit": e.unit, "actual": e.actual,
        })).collect::<Vec<_>>(),
        "actions": sqls,
        "caveats": a.caveats.iter().map(|c| &c.key).collect::<Vec<_>>(),
        "basis": a.impact.basis,
        "share": a.impact.share,
        "fromWarnings": a.from_warnings,
        "statementRange": a.statement_range,
    })
}

fn golden(name: &str, value: &Value) {
    let path = root().join("advice_fixtures").join(name);
    let mut text = serde_json::to_string_pretty(value).unwrap();
    text.push('\n');
    if dbstudio_core::testing::updating() {
        std::fs::write(&path, &text).unwrap();
        return;
    }
    let old = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "{} is missing; run `UPDATE_FIXTURES=1 cargo test -p dbstudio-plan --test advice`",
            path.display()
        )
    });
    assert_eq!(
        old, text,
        "golden {name} differs; review and run with UPDATE_FIXTURES=1"
    );
}

#[test]
fn golden_advice_of_every_postgres_fixture() {
    let mut n = 0;
    for c in corpus()
        .iter()
        .filter(|c| c.format == PlanFormat::PostgresJson)
    {
        let tree = tree_of(c);
        let value = Value::Array(tree.advice.iter().map(summary).collect());
        golden(&format!("{}.advice.json", c.name), &value);
        n += 1;
    }
    assert!(n >= 15, "only {n} PostgreSQL fixtures");
}

#[test]
fn every_warning_has_an_advice_that_names_it() {
    for c in corpus() {
        let tree = tree_of(&c);
        for w in &tree.warnings {
            assert!(
                tree.advice
                    .iter()
                    .any(|a| a.from_warnings.contains(&w.rule) && a.node_ids.contains(&w.node_id)),
                "{}: warning {:?} on node {} has no advice (advice: {:?})",
                c.name,
                w.rule,
                w.node_id,
                tree.advice
                    .iter()
                    .map(|a| (a.rule, &a.node_ids, &a.from_warnings))
                    .collect::<Vec<_>>()
            );
        }
    }
}

#[test]
fn advise_is_idempotent_and_ranked() {
    let rules = PlanRules::default();
    for c in corpus() {
        let raw = raw_of(&c);
        let tree = tree_of(&c);
        let ctx = AdvisorContext::basic(&raw, &rules);
        let again = advise(&tree, &raw.statement, &ctx);
        assert_eq!(again, tree.advice, "{}: not idempotent", c.name);
        let scores: Vec<f64> = tree.advice.iter().map(|a| a.impact.score).collect();
        assert!(
            scores.windows(2).all(|w| w[0] >= w[1]),
            "{}: scores not descending: {scores:?}",
            c.name
        );
        let ids: BTreeSet<&String> = tree.advice.iter().map(|a| &a.id).collect();
        assert_eq!(ids.len(), tree.advice.len(), "{}: duplicate ids", c.name);
        assert!(tree.advice.len() <= rules.max_advice as usize || !tree.warnings.is_empty());
        for a in &tree.advice {
            assert!(!a.fingerprint.is_empty(), "{}: empty fingerprint", c.name);
            // Every node id exists.
            for id in &a.node_ids {
                assert!(*id < tree.node_count, "{}: node {id} out of range", c.name);
            }
            // The JSON contract survives a round trip.
            let back: PlanAdvice =
                serde_json::from_value(serde_json::to_value(a).unwrap()).unwrap();
            assert_eq!(&back, a);
        }
    }
}

#[test]
fn actions_respect_scope_rules_and_ddl_is_one_statement() {
    for c in corpus() {
        let tree = tree_of(&c);
        let dialect = dbstudio_plan::advice::dialect_of_format(c.format, c.version);
        for a in &tree.advice {
            for act in &a.actions {
                if matches!(act.scope, ActionScope::Server | ActionScope::Database) {
                    assert!(
                        act.sql.is_none() || act.kind == ActionKind::Check,
                        "{}: {:?} has sql with scope {:?}",
                        c.name,
                        a.rule,
                        act.scope
                    );
                }
                if act.kind == ActionKind::CreateIndex && !act.template {
                    let sql = act.sql.as_ref().expect("createIndex has sql");
                    if matches!(dialect, ScriptDialect::MongoShell) {
                        continue;
                    }
                    let ranges = dbstudio_sql::split(dialect, sql);
                    assert_eq!(ranges.len(), 1, "{}: {sql}", c.name);
                    let analysis = dbstudio_sql::analyze(dialect, sql);
                    assert_eq!(
                        analysis.statements.first().map(|s| s.class),
                        Some(StatementClass::Ddl),
                        "{}: {sql}",
                        c.name
                    );
                    if dialect == ScriptDialect::Postgres {
                        let name = sql
                            .split(" ON ")
                            .next()
                            .and_then(|h| h.rsplit(' ').next())
                            .unwrap_or("");
                        assert!(name.trim_matches('"').len() <= 63, "{}: {name}", c.name);
                    }
                }
            }
        }
    }
}

#[test]
fn old_plans_without_statement_or_advice_still_work() {
    // A raw plan with no statement text gives tier 1 advice from the plan alone.
    let c = corpus()
        .into_iter()
        .find(|c| c.name == "pg_04_sort_spill_analyze")
        .unwrap();
    let mut raw = raw_of(&c);
    raw.statement.clear();
    let tree = parse_and_analyze(&raw, &PlanRules::default()).unwrap();
    assert!(!tree.advice.is_empty());
    // `parse` alone never advises.
    assert!(parse(&raw).unwrap().advice.is_empty());
}

/// Names the UI must translate: rules, evidence keys, caveat keys and action labels that any
/// fixture produces (the frontend test reads this file).
#[test]
fn translation_keys_of_the_corpus() {
    let mut rules = BTreeSet::new();
    let mut evidence = BTreeSet::new();
    let mut caveats = BTreeSet::new();
    let mut actions = BTreeSet::new();
    for c in corpus() {
        for a in tree_of(&c).advice {
            rules.insert(
                serde_json::to_value(a.rule)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_string(),
            );
            evidence.extend(a.evidence.iter().map(|e| e.key.clone()));
            caveats.extend(a.caveats.iter().map(|c| c.key.clone()));
            actions.extend(a.actions.iter().map(|x| x.label_key.clone()));
        }
    }
    golden(
        "keys.json",
        &json!({"rules": rules, "evidence": evidence, "caveats": caveats, "actions": actions}),
    );
}

/// Captured plans of the browser mock (`src/lib/mock/plan.ts`); the advice the engine gives for
/// each is written to `src/lib/mock/planAdvice.json`, so the mock never drifts from the backend.
const MOCK_PLANS: [&str; 10] = [
    "pg_02_hashjoin_agg_analyze.json",
    "pg_08_cte_subplan.json",
    "mysql_json_02_join_group.json",
    "mysql_tree_01_analyze_join.txt",
    "mssql_01_clustered_scan_estimated.xml",
    "mssql_03_parallel_hash_join_actual.xml",
    "oracle_01_full_scan_estimated.json",
    "oracle_04_group_by_spill_actual.json",
    "mongo_01_collscan_planner.json",
    "mongo_03_collscan_sort_disk_stats.json",
];

#[test]
fn mock_advice_matches_the_engine() {
    let cases = corpus();
    let mut map = serde_json::Map::new();
    for file in MOCK_PLANS {
        let stem = file.rsplit_once('.').map_or(file, |(s, _)| s);
        let c = cases
            .iter()
            .find(|c| c.name == stem)
            .unwrap_or_else(|| panic!("mock plan {file} is not in the corpus"));
        map.insert(
            file.to_string(),
            serde_json::to_value(&tree_of(c).advice).unwrap(),
        );
    }
    let mut text = serde_json::to_string_pretty(&Value::Object(map)).unwrap();
    text.push('\n');
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../src/lib/mock/planAdvice.json");
    if dbstudio_core::testing::updating() {
        std::fs::write(&path, &text).unwrap();
        return;
    }
    let old = std::fs::read_to_string(&path).unwrap_or_default();
    // The file is formatted by prettier after being written: compare the data, not the layout.
    let old: Value = serde_json::from_str(&old).unwrap_or(Value::Null);
    assert_eq!(
        old,
        serde_json::from_str::<Value>(&text).unwrap(),
        "src/lib/mock/planAdvice.json is stale; run `UPDATE_FIXTURES=1 cargo test -p dbstudio-plan --test advice` then `pnpm prettier --write src/lib/mock/planAdvice.json`"
    );
}
