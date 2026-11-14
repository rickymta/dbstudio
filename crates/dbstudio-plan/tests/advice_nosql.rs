//! Query Advisor on the MongoDB, ClickHouse and CockroachDB captures (package R5): golden advice
//! per fixture (`advice_fixtures/<name>.advice.json`, rewritten with `UPDATE_FIXTURES=1`) and a
//! few end-to-end checks of what the rules promise.
//!
//! The index commands of MongoDB are rendered by `index::render` (package R2): until that is
//! merged the goldens carry no `createIndex` action for MongoDB and the rule keeps only the
//! candidate. Re-run with `UPDATE_FIXTURES=1` after merging R2 and review the diff.

use std::collections::BTreeMap;
use std::path::PathBuf;

use dbstudio_core::{PlanFormat, RawPlan};
use dbstudio_plan::{
    ActionKind, AdviceRule, Confidence, PlanAdvice, PlanRules, PlanTree, parse_and_analyze,
};
use serde_json::{Value, json};

struct Case {
    /// Golden file stem.
    name: String,
    format: PlanFormat,
    version: &'static str,
    text: String,
}

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests")
}

fn corpus() -> Vec<Case> {
    let mut out = Vec::new();
    let mut add = |dir: &str,
                   prefix: &str,
                   keep: &dyn Fn(&str) -> bool,
                   format: PlanFormat,
                   version: &'static str| {
        let mut names: Vec<String> = std::fs::read_dir(root().join(dir))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| !n.starts_with('.') && keep(n))
            .collect();
        names.sort();
        for name in names {
            let stem = name.rsplit_once('.').map_or(name.as_str(), |(s, _)| s);
            out.push(Case {
                name: format!("{prefix}{stem}"),
                format,
                version,
                text: std::fs::read_to_string(root().join(dir).join(&name)).unwrap(),
            });
        }
    };
    add(
        "fixtures",
        "",
        &|n| n.starts_with("mongo_"),
        PlanFormat::MongoExplain,
        "7.0.14",
    );
    add(
        "clickhouse",
        "clickhouse_",
        &|n| n.ends_with(".json"),
        PlanFormat::ClickHouseJson,
        "26.3",
    );
    add(
        "fixtures_cockroach",
        "",
        &|n| n.ends_with(".txt"),
        PlanFormat::CockroachText,
        "25.4.20",
    );
    out
}

fn raw_of(c: &Case) -> RawPlan {
    RawPlan {
        format: c.format,
        text: c.text.clone(),
        statement: String::new(),
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

fn summary(a: &PlanAdvice) -> Value {
    let actions: Vec<Value> = a
        .actions
        .iter()
        .map(|x| {
            json!({
                "kind": x.kind, "label": x.label_key, "scope": x.scope, "template": x.template,
                "lock": x.lock, "sql": x.sql, "params": x.params,
            })
        })
        .collect();
    json!({
        "rule": a.rule,
        "severity": a.severity,
        "confidence": a.confidence,
        "nodeIds": a.node_ids,
        "table": a.table.as_ref().map(|t| match (&t.database, &t.schema) {
            (_, Some(s)) => format!("{s}.{}", t.name),
            (Some(d), None) => format!("{d}.{}", t.name),
            _ => t.name.clone(),
        }),
        "params": a.params,
        "evidence": a.evidence.iter().map(|e| json!({
            "key": e.key, "nodeId": e.node_id, "value": e.value, "text": e.text,
            "unit": e.unit, "actual": e.actual,
        })).collect::<Vec<_>>(),
        "actions": actions,
        "caveats": a.caveats.iter().map(|c| &c.key).collect::<Vec<_>>(),
        "basis": a.impact.basis,
        "share": a.impact.share,
        "fromWarnings": a.from_warnings,
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
            "{} is missing; run `UPDATE_FIXTURES=1 cargo test -p dbstudio-plan --test advice_nosql`",
            path.display()
        )
    });
    assert_eq!(
        old, text,
        "golden {name} differs; review and run with UPDATE_FIXTURES=1"
    );
}

fn golden_of(format: PlanFormat, expected: usize) {
    let mut n = 0;
    for c in corpus().iter().filter(|c| c.format == format) {
        let tree = tree_of(c);
        let value = Value::Array(tree.advice.iter().map(summary).collect());
        golden(&format!("{}.advice.json", c.name), &value);
        n += 1;
    }
    assert_eq!(n, expected, "fixture count of {format:?}");
}

#[test]
fn golden_advice_of_every_mongo_fixture() {
    golden_of(PlanFormat::MongoExplain, 6);
}

#[test]
fn golden_advice_of_every_clickhouse_fixture() {
    golden_of(PlanFormat::ClickHouseJson, 7);
}

#[test]
fn golden_advice_of_every_cockroach_fixture() {
    golden_of(PlanFormat::CockroachText, 8);
}

fn advice_of(name: &str) -> Vec<PlanAdvice> {
    let c = corpus()
        .into_iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("no fixture {name}"));
    tree_of(&c).advice
}

fn rule(a: &[PlanAdvice], r: AdviceRule) -> &PlanAdvice {
    a.iter().find(|x| x.rule == r).unwrap_or_else(|| {
        panic!(
            "no {r:?} in {:?}",
            a.iter().map(|x| x.rule).collect::<Vec<_>>()
        )
    })
}

fn sql_of(a: &PlanAdvice, kind: ActionKind) -> Vec<String> {
    a.actions
        .iter()
        .filter(|x| x.kind == kind)
        .filter_map(|x| x.sql.clone())
        .collect()
}

#[test]
fn mongo_collscan_with_a_blocking_sort_gets_one_esr_index() {
    let a = advice_of("mongo_03_collscan_sort_disk_stats");
    let c = rule(&a, AdviceRule::MongoCollscan);
    assert_eq!(c.confidence, Confidence::High);
    assert_eq!(c.params["columns"], "kind, ts DESC");
    assert_eq!(c.node_ids, vec![1, 0]);
    // The sort is answered by the same advice: no second index for it.
    assert!(a.iter().all(|x| x.rule != AdviceRule::MongoBlockingSort));
    assert!(
        a.iter()
            .filter(|x| x.rule == AdviceRule::MongoCollscan)
            .count()
            == 1
    );
}

#[test]
fn mongo_planner_only_collscan_is_low() {
    let a = advice_of("mongo_01_collscan_planner");
    let c = rule(&a, AdviceRule::MongoCollscan);
    assert_eq!(c.confidence, Confidence::Low);
    assert_eq!(c.params["columns"], "status");
    assert!(c.caveats.iter().any(|x| x.key == "estimatedOnly"));
}

#[test]
fn mongo_healthy_plans_stay_quiet() {
    for n in [
        "mongo_02_ixscan_fetch_stats",
        "mongo_05_idhack_stats",
        "mongo_06_or_inputstages_planner",
    ] {
        assert!(advice_of(n).is_empty(), "{n}: {:?}", advice_of(n));
    }
}

#[test]
fn clickhouse_unused_primary_key_offers_rewrite_skip_index_and_projection() {
    let a = advice_of("clickhouse_02_full_scan");
    let p = rule(&a, AdviceRule::ChPrimaryKeyUnused);
    assert_eq!(p.params["columns"], "payload");
    assert!(
        p.from_warnings
            .contains(&dbstudio_plan::PlanRule::FullScanLargeTable)
    );
    assert_eq!(
        sql_of(p, ActionKind::CreateIndex),
        vec![
            "ALTER TABLE `plan_fx`.`events` ADD INDEX `ix_events_payload` `payload` TYPE bloom_filter(0.01) GRANULARITY 4;"
                .to_string()
        ]
    );
    let maintenance = sql_of(p, ActionKind::Maintenance);
    assert_eq!(maintenance.len(), 3);
    assert!(maintenance[0].contains("MATERIALIZE INDEX"));
    assert!(maintenance[1].contains("ADD PROJECTION `p_payload` (SELECT * ORDER BY `payload`)"));
    // Without the catalog the key is unknown: the first step is to look it up.
    assert!(sql_of(p, ActionKind::Check)[0].contains("sorting_key"));
    assert!(p.caveats.iter().any(|c| c.key == "metadataUnavailable"));
}

#[test]
fn clickhouse_useless_skip_index_is_reported_and_the_other_column_is_indexed() {
    let a = advice_of("clickhouse_03_skip_index");
    let u = rule(&a, AdviceRule::ChSkipIndexUseless);
    assert_eq!(u.params["index"], "idx_kind");
    // `kind` already has a (useless) skip index: the new one goes on `payload`.
    let p = rule(&a, AdviceRule::ChPrimaryKeyUnused);
    assert_eq!(p.params["columns"], "payload");
}

#[test]
fn clickhouse_selective_reads_and_small_joins_stay_quiet() {
    for n in [
        "clickhouse_01_pk_index",
        "clickhouse_04_join",
        "clickhouse_07_mv_read",
    ] {
        let a = advice_of(n);
        assert!(
            a.iter().all(|x| !matches!(
                x.rule,
                AdviceRule::ChPrimaryKeyUnused
                    | AdviceRule::ChJoinRightLarge
                    | AdviceRule::ChSkipIndexUseless
            )),
            "{n}: {:?}",
            a.iter().map(|x| x.rule).collect::<Vec<_>>()
        );
    }
}

#[test]
fn cockroach_footer_recommendation_is_offered_verbatim() {
    let a = advice_of("cockroach_01_full_scan");
    let r = rule(&a, AdviceRule::CrdbIndexRecommendation);
    assert_eq!(r.confidence, Confidence::High);
    assert_eq!(
        sql_of(r, ActionKind::CreateIndex),
        vec!["CREATE INDEX ON dbstudio.sv.big (name) STORING (grp, amount);".to_string()]
    );
    assert_eq!(r.node_ids, vec![1]);
    assert_eq!(r.params["columns"], "name");
}

#[test]
fn cockroach_missing_statistics_on_a_misestimated_node_say_it_once() {
    // The insert is flagged `(missing stats)` and misestimated: `staleStatistics` answers it
    // (`ANALYZE` is `CREATE STATISTICS` in CockroachDB), `crdbMissingStats` stays quiet.
    let a = advice_of("cockroach_05_insert_analyze");
    assert!(a.iter().all(|x| x.rule != AdviceRule::CrdbMissingStats));
    let s = rule(&a, AdviceRule::StaleStatistics);
    assert_eq!(
        sql_of(s, ActionKind::Statistics),
        vec!["ANALYZE ords;".to_string()]
    );
}

#[test]
fn yugabyte_reuses_the_postgres_rules_without_concurrently() {
    // A PostgreSQL plan from a YugabyteDB server: the PostgreSQL rule, YugabyteDB DDL.
    let text = std::fs::read_to_string(root().join("fixtures/pg_01_seqscan_filter.json")).unwrap();
    let raw = RawPlan {
        format: PlanFormat::PostgresJson,
        text,
        statement: "select * from orders where status = 'void'".into(),
        analyzed: false,
        rolled_back: false,
        server_version: "PostgreSQL 11.2-YB-2.20.0.0".into(),
        captured_at: "2025-01-01T00:00:00Z".into(),
        extra: BTreeMap::new(),
    };
    let tree = parse_and_analyze(&raw, &PlanRules::default()).unwrap();
    let ddl: Vec<String> = tree
        .advice
        .iter()
        .flat_map(|a| sql_of(a, ActionKind::CreateIndex))
        .collect();
    assert!(!ddl.is_empty(), "{:?}", tree.advice);
    assert!(ddl.iter().all(|s| !s.contains("CONCURRENTLY")), "{ddl:?}");
}
