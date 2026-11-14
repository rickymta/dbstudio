//! Query Advisor for SQL Server and Oracle plans: golden advice per fixture
//! (`advice_fixtures/{mssql,oracle}_*.advice.json`, rewritten with `UPDATE_FIXTURES=1`) and
//! targeted checks of the rules of design sections 4.3 and 4.4.
//!
//! The `createIndex` actions have no SQL until the DDL renderer of package R2 knows SQL Server
//! and Oracle (until then the advice carries the caveat `noDdlForDialect`): regenerate these
//! goldens after merging R2.

use std::collections::BTreeMap;
use std::path::PathBuf;

use dbstudio_core::{ColumnDetail, IndexDetail, ObjectPath, PlanFormat, RawPlan, ValueKind};
use dbstudio_plan::{
    ActionKind, AdviceRule, AdvisorContext, Confidence, MetadataState, PlanAdvice, PlanRule,
    PlanRules, PlanTree, TableKey, TableMeta, advise, analyze, parse, parse_and_analyze,
};
use serde_json::{Value, json};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests")
}

struct Case {
    name: String,
    format: PlanFormat,
    text: String,
    statement: String,
}

fn corpus() -> Vec<Case> {
    let mut names: Vec<String> = std::fs::read_dir(root().join("fixtures"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("mssql_") || n.starts_with("oracle_"))
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|n| {
            let stem = n.rsplit_once('.').map_or(n.clone(), |(s, _)| s.to_string());
            Case {
                format: if n.starts_with("mssql_") {
                    PlanFormat::MsSqlShowplanXml
                } else {
                    PlanFormat::OraclePlanRows
                },
                text: std::fs::read_to_string(root().join("fixtures").join(&n)).unwrap(),
                statement: std::fs::read_to_string(
                    root().join("advice_fixtures").join(format!("{stem}.sql")),
                )
                .map(|s| s.trim().to_string())
                .unwrap_or_default(),
                name: stem,
            }
        })
        .collect()
}

fn raw(format: PlanFormat, text: &str, statement: &str) -> RawPlan {
    RawPlan {
        format,
        text: text.into(),
        statement: statement.into(),
        analyzed: false,
        rolled_back: false,
        server_version: "16.0".into(),
        captured_at: "2025-01-01T00:00:00Z".into(),
        extra: BTreeMap::new(),
    }
}

fn tree_of(c: &Case) -> PlanTree {
    parse_and_analyze(&raw(c.format, &c.text, &c.statement), &PlanRules::default())
        .unwrap_or_else(|e| panic!("{}: {e}", c.name))
}

fn case(name: &str) -> Case {
    corpus().into_iter().find(|c| c.name == name).unwrap()
}

fn summary(a: &PlanAdvice) -> Value {
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
        "actions": a.actions.iter().map(|x| json!({
            "kind": x.kind, "label": x.label_key, "scope": x.scope, "template": x.template,
            "lock": x.lock, "sql": x.sql,
        })).collect::<Vec<_>>(),
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
            "{} is missing; run `UPDATE_FIXTURES=1 cargo test -p dbstudio-plan --test advice_mssql_oracle`",
            path.display()
        )
    });
    assert_eq!(
        old, text,
        "golden {name} differs; review and run with UPDATE_FIXTURES=1"
    );
}

fn rules_of(t: &PlanTree) -> Vec<AdviceRule> {
    t.advice.iter().map(|a| a.rule).collect()
}

fn find(t: &PlanTree, rule: AdviceRule) -> &PlanAdvice {
    t.advice
        .iter()
        .find(|a| a.rule == rule)
        .unwrap_or_else(|| panic!("no {rule:?} in {:?}", rules_of(t)))
}

fn with_meta(c: &Case, tables: Vec<(Option<&str>, &str, TableMeta)>) -> Vec<PlanAdvice> {
    let raw = raw(c.format, &c.text, &c.statement);
    let rules = PlanRules::default();
    let mut ctx = AdvisorContext::basic(&raw, &rules);
    for (schema, name, meta) in tables {
        ctx.tables.insert(TableKey::new(schema, name), meta);
    }
    ctx.metadata = MetadataState::Checked;
    let mut tree = parse(&raw).unwrap();
    analyze(&mut tree, &rules);
    advise(&tree, &raw.statement, &ctx)
}

fn meta(rows: Option<u64>, indexes: Vec<IndexDetail>, columns: Vec<ColumnDetail>) -> TableMeta {
    TableMeta {
        path: ObjectPath::root(),
        estimated_rows: rows,
        columns,
        indexes,
    }
}

fn index(name: &str, columns: &[&str]) -> IndexDetail {
    IndexDetail {
        name: name.into(),
        columns: columns.iter().map(|c| c.to_string()).collect(),
        unique: false,
        primary: false,
        method: None,
        definition: None,
        partial: false,
    }
}

#[test]
fn golden_advice_of_every_mssql_and_oracle_fixture() {
    let mut n = (0, 0);
    for c in corpus() {
        let tree = tree_of(&c);
        let value = Value::Array(tree.advice.iter().map(summary).collect());
        golden(&format!("{}.advice.json", c.name), &value);
        if c.format == PlanFormat::MsSqlShowplanXml {
            n.0 += 1;
        } else {
            n.1 += 1;
        }
    }
    assert!(n.0 >= 7 && n.1 >= 7, "{n:?}");
}

#[test]
fn every_warning_of_these_fixtures_has_advice() {
    for c in corpus() {
        let t = tree_of(&c);
        for w in &t.warnings {
            assert!(
                t.advice
                    .iter()
                    .any(|a| a.from_warnings.contains(&w.rule) && a.node_ids.contains(&w.node_id)),
                "{}: {:?} on node {}",
                c.name,
                w.rule,
                w.node_id
            );
        }
    }
}

// ---- SQL Server ------------------------------------------------------------------------------

#[test]
fn mssql_parser_keeps_what_the_advisor_needs() {
    let t = tree_of(&case("mssql_07_missing_index_convert"));
    let xml_root = &t.root;
    let mi: Value = serde_json::from_str(&xml_root.extra["missingIndexes"]).unwrap();
    assert_eq!(mi[0]["table"], "Orders");
    assert_eq!(mi[0]["equality"], json!(["CustomerId", "Code"]));
    assert_eq!(mi[0]["inequality"], json!(["OrderDate"]));
    assert_eq!(mi[0]["include"], json!(["Total", "Note"]));
    let pac: Value = serde_json::from_str(&xml_root.extra["planAffectingConvert"]).unwrap();
    assert_eq!(pac[0]["issue"], "Seek Plan");
    assert!(
        pac[0]["expression"]
            .as_str()
            .unwrap()
            .starts_with("CONVERT_IMPLICIT(nvarchar(50)")
    );
    assert_eq!(xml_root.extra["Output"], "o.Total, o.Note");
    let lookup = &xml_root.children[1];
    assert_eq!(lookup.op, "Key Lookup");
    assert_eq!(lookup.extra["Output"], "o.Total, o.Note");
    let seek = &xml_root.children[0];
    assert!(
        seek.predicates
            .iter()
            .any(|p| p.text == "[o].[CustomerId]=(42)"),
        "{:?}",
        seek.predicates
    );
}

#[test]
fn mssql_missing_index_key_lookup_and_implicit_conversion() {
    let t = tree_of(&case("mssql_07_missing_index_convert"));
    let mi = find(&t, AdviceRule::MssqlMissingIndex);
    assert_eq!(
        mi.params.get("columns").map(String::as_str),
        Some("CustomerId, Code, OrderDate")
    );
    assert_eq!(
        mi.params.get("include").map(String::as_str),
        Some("Total, Note")
    );
    assert_eq!(mi.table.as_ref().unwrap().name, "Orders");
    assert!(mi.node_ids.contains(&1) && mi.node_ids.contains(&2));
    // No catalog: confidence drops from medium and says so.
    assert_eq!(mi.confidence, Confidence::Low);
    assert!(mi.caveats.iter().any(|c| c.key == "metadataUnavailable"));

    let kl = find(&t, AdviceRule::MssqlKeyLookup);
    assert_eq!(kl.node_ids[0], 2);
    assert_eq!(
        kl.params.get("include").map(String::as_str),
        Some("Total, Note")
    );
    assert_eq!(
        kl.params.get("columns").map(String::as_str),
        Some("CustomerId")
    );
    assert!(kl.from_warnings.contains(&PlanRule::NestedLoopManyRows));
    // The generic pointer for that nested loop is folded into the key lookup advice.
    assert!(
        !rules_of(&t).contains(&AdviceRule::NestedLoopInnerScan),
        "{:?}",
        rules_of(&t)
    );

    let ic = find(&t, AdviceRule::ImplicitConversion);
    assert_eq!(ic.confidence, Confidence::High);
    assert_eq!(ic.params.get("column").map(String::as_str), Some("Code"));
    assert_eq!(
        ic.params.get("convertTo").map(String::as_str),
        Some("nvarchar(50)")
    );
    assert!(ic.actions.iter().any(|a| a.kind == ActionKind::Rewrite));
    assert!(ic.actions.iter().any(|a| {
        a.sql
            .as_deref()
            .is_some_and(|s| s.contains("INFORMATION_SCHEMA.COLUMNS"))
    }));
}

#[test]
fn mssql_key_lookup_with_the_catalog_knows_the_old_index_columns() {
    let c = case("mssql_07_missing_index_convert");
    let advice = with_meta(
        &c,
        vec![(
            Some("dbo"),
            "Orders",
            meta(
                Some(1_200_000),
                vec![index("IX_Orders_CustomerId", &["CustomerId", "OrderDate"])],
                vec![ColumnDetail {
                    name: "Code".into(),
                    ordinal: 5,
                    type_name: "varchar(50)".into(),
                    value_kind: ValueKind::Text,
                    nullable: false,
                    default: None,
                    comment: None,
                    primary_key: false,
                    extra: None,
                }],
            ),
        )],
    );
    let kl = advice
        .iter()
        .find(|a| a.rule == AdviceRule::MssqlKeyLookup)
        .unwrap();
    assert_eq!(kl.confidence, Confidence::High);
    assert_eq!(
        kl.params.get("columns").map(String::as_str),
        Some("CustomerId, OrderDate")
    );
    assert!(kl.caveats.iter().all(|c| c.key != "indexKeysFromSeek"));
    let ic = advice
        .iter()
        .find(|a| a.rule == AdviceRule::ImplicitConversion)
        .unwrap();
    assert_eq!(
        ic.params.get("columnType").map(String::as_str),
        Some("varchar(50)")
    );
    let mi = advice
        .iter()
        .find(|a| a.rule == AdviceRule::MssqlMissingIndex)
        .unwrap();
    assert_eq!(mi.confidence, Confidence::Medium);
}

#[test]
fn mssql_sort_spill_gets_statistics_and_a_grant_hint() {
    let t = tree_of(&case("mssql_04_sort_spill_actual"));
    let a = find(&t, AdviceRule::SortSpill);
    assert!(a.from_warnings.contains(&PlanRule::SpillToDisk));
    let sqls: Vec<&str> = a.actions.iter().filter_map(|x| x.sql.as_deref()).collect();
    assert!(
        sqls.contains(&"UPDATE STATISTICS [dbo].[BigTransactions];"),
        "{sqls:?}"
    );
    let hint = a
        .actions
        .iter()
        .find(|x| x.kind == ActionKind::Hint)
        .unwrap();
    assert!(hint.template);
    assert_eq!(hint.sql.as_deref(), Some("OPTION (MIN_GRANT_PERCENT = 10)"));
    assert!(a.evidence.iter().any(|e| e.key == "tempdbWrites"));
}

fn mini_plan(rel_op_extra: &str, query_plan_extra: &str) -> String {
    format!(
        r#"<ShowPlanXML xmlns="http://schemas.microsoft.com/sqlserver/2004/07/showplan" Version="1.6">
<BatchSequence><Batch><Statements>
<StmtSimple StatementText="select 1" StatementSubTreeCost="10" StatementEstRows="5">
<QueryPlan CompileTime="3">{query_plan_extra}
<RelOp NodeId="0" PhysicalOp="Nested Loops" LogicalOp="Inner Join" EstimateRows="5" EstimatedTotalSubtreeCost="10">
{rel_op_extra}
<NestedLoops>
<RelOp NodeId="1" PhysicalOp="Clustered Index Scan" LogicalOp="Clustered Index Scan" EstimateRows="100" EstimatedTotalSubtreeCost="4"><IndexScan><Object Schema="[dbo]" Table="[A]" Index="[PK_A]"/></IndexScan></RelOp>
<RelOp NodeId="2" PhysicalOp="Clustered Index Scan" LogicalOp="Clustered Index Scan" EstimateRows="100" EstimatedTotalSubtreeCost="4"><IndexScan><Object Schema="[dbo]" Table="[B]" Index="[PK_B]"/></IndexScan></RelOp>
</NestedLoops></RelOp></QueryPlan></StmtSimple></Statements></Batch></BatchSequence></ShowPlanXML>"#
    )
}

fn advice_of_xml(xml: &str) -> PlanTree {
    parse_and_analyze(
        &raw(PlanFormat::MsSqlShowplanXml, xml, ""),
        &PlanRules::default(),
    )
    .unwrap()
}

#[test]
fn mssql_warning_elements_become_advice() {
    let xml = mini_plan(
        r#"<Warnings NoJoinPredicate="true"/>"#,
        r#"<Warnings><ColumnsWithNoStatistics><ColumnReference Database="[d]" Schema="[dbo]" Table="[A]" Column="[Code]"/></ColumnsWithNoStatistics>
<MemoryGrantWarning GrantWarningKind="Excessive Grant" RequestedMemory="1048576" GrantedMemory="1048576" MaxUsedMemory="2048"/></Warnings>"#,
    );
    let t = advice_of_xml(&xml);
    let j = find(&t, AdviceRule::MssqlNoJoinPredicate);
    assert_eq!(
        (j.confidence, j.node_ids.clone()),
        (Confidence::High, vec![0])
    );
    assert!(j.actions.iter().any(|a| a.label_key == "addJoinPredicate"));

    let s = find(&t, AdviceRule::MssqlNoStatistics);
    assert_eq!(s.confidence, Confidence::High);
    assert_eq!(
        s.actions[0].sql.as_deref(),
        Some("CREATE STATISTICS [st_A_Code] ON [dbo].[A] ([Code]);")
    );
    // The database-level option is described, never given as SQL.
    let db = s
        .actions
        .iter()
        .find(|a| a.label_key == "autoCreateStatistics")
        .unwrap();
    assert!(db.sql.is_none());

    let g = find(&t, AdviceRule::MssqlExcessiveGrant);
    assert_eq!(g.confidence, Confidence::Low);
    assert!(
        g.actions
            .iter()
            .any(|a| a.sql.as_deref() == Some("OPTION (MAX_GRANT_PERCENT = 5)") && a.template)
    );
}

#[test]
fn mssql_convert_of_a_parameter_is_an_info_for_the_estimate() {
    let xml = mini_plan(
        "",
        r#"<Warnings><PlanAffectingConvert ConvertIssue="Cardinality Estimate" Expression="CONVERT_IMPLICIT(varchar(8000),[@1],0)"/></Warnings>"#,
    );
    let t = advice_of_xml(&xml);
    let a = find(&t, AdviceRule::ImplicitConversion);
    assert_eq!(a.confidence, Confidence::Medium);
    assert_eq!(format!("{:?}", a.severity), "Info");
    assert!(!a.params.contains_key("column"));
    assert_eq!(a.actions[0].label_key, "matchParameterType");
}

#[test]
fn mssql_missing_index_below_the_threshold_is_ignored_and_many_includes_are_dropped() {
    let mi = |impact: f64, include: usize| {
        let inc: String = (0..include)
            .map(|i| format!(r#"<Column Name="[c{i}]"/>"#))
            .collect();
        format!(
            r#"<MissingIndexes><MissingIndexGroup Impact="{impact}"><MissingIndex Database="[d]" Schema="[dbo]" Table="[A]"><ColumnGroup Usage="EQUALITY"><Column Name="[x]"/></ColumnGroup><ColumnGroup Usage="INCLUDE">{inc}</ColumnGroup></MissingIndex></MissingIndexGroup></MissingIndexes>"#
        )
    };
    let low = advice_of_xml(&mini_plan("", &mi(12.0, 1)));
    assert!(!rules_of(&low).contains(&AdviceRule::MssqlMissingIndex));
    let wide = advice_of_xml(&mini_plan("", &mi(80.0, 12)));
    let a = find(&wide, AdviceRule::MssqlMissingIndex);
    assert!(!a.params.contains_key("include"));
    assert!(a.caveats.iter().any(|c| c.key == "includeTrimmed"));
    let some = advice_of_xml(&mini_plan("", &mi(80.0, 7)));
    let a = find(&some, AdviceRule::MssqlMissingIndex);
    assert_eq!(a.params["include"], "c0, c1, c2, c3, c4, c5");
}

#[test]
fn mssql_stale_statistics_use_fullscan_or_a_sample() {
    let c = case("mssql_05_nested_loops_many_actual");
    let t = tree_of(&c);
    for a in t
        .advice
        .iter()
        .filter(|a| a.rule == AdviceRule::StaleStatistics)
    {
        for x in a
            .actions
            .iter()
            .filter(|x| x.kind == ActionKind::Statistics)
        {
            assert!(
                x.sql.as_deref().unwrap().ends_with(" WITH FULLSCAN;"),
                "{:?}",
                x.sql
            );
        }
    }
}

// ---- Oracle -----------------------------------------------------------------------------------

#[test]
fn oracle_parser_names_the_table_of_an_index_row_and_keeps_predicates() {
    let t = tree_of(&case("oracle_07_skip_scan"));
    let table_access = &t.root.children[0];
    assert_eq!(
        table_access.predicates.len(),
        1,
        "{:?}",
        table_access.predicates
    );
    let idx = &table_access.children[0];
    let o = idx.object.as_ref().unwrap();
    // The alias `O` is not the table: the TABLE ACCESS row above names ORDERS.
    assert_eq!(
        (o.name.as_str(), o.alias.as_deref(), o.schema.as_deref()),
        ("ORDERS", Some("O"), Some("SHOP"))
    );
    assert_eq!(o.index.as_deref(), Some("IX_ORDERS_CUST_STATUS"));
    assert_eq!(idx.extra["LAST_STARTS"], "1");
    assert_eq!(idx.predicates.len(), 2);
}

#[test]
fn oracle_skip_scan_and_residual_filter() {
    let t = tree_of(&case("oracle_07_skip_scan"));
    let s = find(&t, AdviceRule::OracleIndexSkipScan);
    assert_eq!(s.node_ids, vec![2]);
    assert_eq!(s.params.get("columns").map(String::as_str), Some("STATUS"));
    assert_eq!(
        s.params.get("usedIndex").map(String::as_str),
        Some("IX_ORDERS_CUST_STATUS")
    );
    assert!(s.impact.share.unwrap() >= 0.2);
    let r = find(&t, AdviceRule::IndexResidualFilter);
    assert_eq!(
        r.params.get("columns").map(String::as_str),
        Some("STATUS, TOTAL")
    );
    assert_eq!(r.node_ids, vec![1, 2]);
}

#[test]
fn oracle_full_scan_filter_needs_num_rows_for_a_measured_plan() {
    let c = case("oracle_03_nested_loops_index_actual");
    let none = tree_of(&c);
    assert!(!rules_of(&none).contains(&AdviceRule::IndexForFilter));
    let with = with_meta(
        &c,
        vec![(
            Some("SHOP"),
            "ORDERS",
            meta(Some(50_000_000), vec![], vec![]),
        )],
    );
    let a = with
        .iter()
        .find(|a| a.rule == AdviceRule::IndexForFilter)
        .unwrap_or_else(|| panic!("{:?}", with.iter().map(|a| a.rule).collect::<Vec<_>>()));
    assert_eq!(a.params.get("columns").map(String::as_str), Some("TOTAL"));
    assert_eq!(a.confidence, Confidence::Medium);
    // A small table is cheaper to scan than to index.
    let small = with_meta(
        &c,
        vec![(Some("SHOP"), "ORDERS", meta(Some(5_000), vec![], vec![]))],
    );
    assert!(!small.iter().any(|a| a.rule == AdviceRule::IndexForFilter));
}

#[test]
fn oracle_stale_statistics_and_work_area_spill() {
    let t = tree_of(&case("oracle_04_group_by_spill_actual"));
    let stale: Vec<&PlanAdvice> = t
        .advice
        .iter()
        .filter(|a| a.rule == AdviceRule::StaleStatistics && a.table.is_some())
        .collect();
    for a in &stale {
        let st = a
            .actions
            .iter()
            .find(|x| x.kind == ActionKind::Statistics)
            .expect("DBMS_STATS action");
        let sql = st.sql.as_deref().unwrap();
        assert!(
            sql.starts_with("BEGIN DBMS_STATS.GATHER_TABLE_STATS("),
            "{sql}"
        );
        assert!(sql.ends_with("\n/"), "{sql}");
    }
    let spill = t
        .advice
        .iter()
        .find(|a| a.rule == AdviceRule::HashSpill)
        .expect("hash spill");
    assert_eq!(spill.confidence, Confidence::Low);
    assert!(spill.from_warnings.contains(&PlanRule::SpillToDisk));
    // Words only, and never an ALTER SESSION / SYSTEM statement.
    assert!(spill.actions.iter().all(|a| a.sql.is_none()));
}

fn oracle_rows(rows: &[Value]) -> PlanTree {
    parse_and_analyze(
        &raw(
            PlanFormat::OraclePlanRows,
            &Value::Array(rows.to_vec()).to_string(),
            "",
        ),
        &PlanRules::default(),
    )
    .unwrap()
}

#[test]
fn oracle_cartesian_join_and_implicit_conversion() {
    let t = oracle_rows(&[
        json!({"ID":0,"OPERATION":"SELECT STATEMENT","CARDINALITY":2500,"COST":90}),
        json!({"ID":1,"PARENT_ID":0,"POSITION":1,"OPERATION":"MERGE JOIN","OPTIONS":"CARTESIAN","CARDINALITY":2500,"COST":90}),
        json!({"ID":2,"PARENT_ID":1,"POSITION":1,"OPERATION":"TABLE ACCESS","OPTIONS":"FULL","OBJECT_OWNER":"SHOP","OBJECT_NAME":"A","OBJECT_ALIAS":"A@SEL$1","CARDINALITY":50,"COST":3}),
        json!({"ID":3,"PARENT_ID":1,"POSITION":2,"OPERATION":"BUFFER","OPTIONS":"SORT","CARDINALITY":50,"COST":87}),
        json!({"ID":4,"PARENT_ID":3,"POSITION":1,"OPERATION":"TABLE ACCESS","OPTIONS":"FULL","OBJECT_OWNER":"SHOP","OBJECT_NAME":"B","OBJECT_ALIAS":"B@SEL$1","CARDINALITY":50,"COST":1,
              "FILTER_PREDICATES":"TO_NUMBER(\"CODE\")=123"}),
    ]);
    let c = find(&t, AdviceRule::OracleCartesian);
    assert_eq!(
        (c.confidence, c.node_ids.clone()),
        (Confidence::High, vec![1])
    );
    let i = find(&t, AdviceRule::ImplicitConversion);
    assert_eq!(i.params.get("column").map(String::as_str), Some("CODE"));
    assert_eq!(
        i.params.get("function").map(String::as_str),
        Some("TO_NUMBER")
    );
    assert!(i.actions.iter().any(|a| a.sql.as_deref().is_some_and(
        |s| s.contains("all_tab_columns")
            && s.contains("owner = 'SHOP'")
            && s.contains("table_name = 'B'")
    )));
    // A one-row side is not a Cartesian problem.
    let one = oracle_rows(&[
        json!({"ID":0,"OPERATION":"SELECT STATEMENT","CARDINALITY":50,"COST":4}),
        json!({"ID":1,"PARENT_ID":0,"POSITION":1,"OPERATION":"MERGE JOIN","OPTIONS":"CARTESIAN","CARDINALITY":50,"COST":4}),
        json!({"ID":2,"PARENT_ID":1,"POSITION":1,"OPERATION":"TABLE ACCESS","OPTIONS":"FULL","OBJECT_NAME":"A","CARDINALITY":1,"COST":1}),
        json!({"ID":3,"PARENT_ID":1,"POSITION":2,"OPERATION":"BUFFER","OPTIONS":"SORT","CARDINALITY":50,"COST":3}),
    ]);
    assert!(!rules_of(&one).contains(&AdviceRule::OracleCartesian));
}

#[test]
fn oracle_internal_function_only_counts_on_a_bare_column() {
    let rows = |pred: &str| {
        oracle_rows(&[
            json!({"ID":0,"OPERATION":"SELECT STATEMENT","CARDINALITY":5,"COST":4}),
            json!({"ID":1,"PARENT_ID":0,"POSITION":1,"OPERATION":"TABLE ACCESS","OPTIONS":"FULL","OBJECT_OWNER":"SHOP","OBJECT_NAME":"T","OBJECT_ALIAS":"T@SEL$1","CARDINALITY":5,"COST":4,"FILTER_PREDICATES":pred}),
        ])
    };
    let hit = rows("INTERNAL_FUNCTION(\"CODE\")=5");
    assert!(rules_of(&hit).contains(&AdviceRule::ImplicitConversion));
    let miss = rows("INTERNAL_FUNCTION(\"A\"+\"B\")=5");
    assert!(!rules_of(&miss).contains(&AdviceRule::ImplicitConversion));
}
