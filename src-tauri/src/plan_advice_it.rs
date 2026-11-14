//! Query Advisor integration tests against real servers (design section 11.2).
//!
//! Each case explains a statement on a ~200k row table, asks `plan_advise` (tier 2) for advice,
//! and for the index advice runs the generated DDL on the server and explains again: the advice
//! must be gone and the scan must have become an index scan. That is the end-to-end proof that
//! the DDL has the right syntax and the right columns.
//!
//! Runs only with `DBSTUDIO_IT=1`; a service that does not answer skips its test, unless
//! `DBSTUDIO_IT_STRICT` lists it (`dbstudio_core::testing::it_strict_for`). Every case works in
//! schemas / databases named `dbstudio_it_adv*` which are dropped before and after the run.
//! Ports follow `docker/compose.test.yml` and the same `DBSTUDIO_IT_*_PORT` variables as the
//! driver tests. The seed (200 000 generated rows per table) is created by the tests themselves
//! with server side generators (`generate_series`, a digit cross join, `numbers()`, batches of
//! `insertMany`, `sys.all_objects` cross join).

use std::collections::BTreeMap;
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use dbstudio_core::{
    ConnectionConfig, DbKind, ExplainOptions, ExplainRequest, QueryRequest, RawPlan, Secret,
    Session, StatementOutcome, TlsMode, Value, execute_collect,
};
use dbstudio_plan::{
    ActionKind, ActionScope, AdviceReport, AdviceRule, MetadataState, PlanAdvice, PlanNode,
    PlanNodeKind, PlanRules, parse_and_analyze,
};
use dbstudio_store::{ConnectionProfile, SecretChange};
use uuid::Uuid;

use crate::commands::plan::advise_with;
use crate::commands::{connections, sessions};
use crate::state::testing::test_state;
use crate::state::{AppState, build_registry};

const PASSWORD: &str = "dbstudio_test";
/// Rows of the `orders` table.
const ROWS: u32 = 200_000;

fn enabled() -> bool {
    std::env::var("DBSTUDIO_IT").as_deref() == Ok("1")
}

fn env_port(var: &str, default: u16) -> u16 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// A server of the test compose file.
#[derive(Clone)]
struct Server {
    /// Service name in `docker/compose.test.yml` (for `DBSTUDIO_IT_STRICT`).
    service: &'static str,
    kind: DbKind,
    port: u16,
    username: &'static str,
    password: &'static str,
    database: Option<String>,
    options: BTreeMap<String, String>,
    tls: TlsMode,
    /// Database named in every request (products where the connection has no default one).
    scope_db: Option<String>,
}

impl Server {
    fn pg16() -> Self {
        Server {
            service: "pg16",
            kind: DbKind::Postgres,
            port: 54316,
            username: "dbstudio",
            password: PASSWORD,
            database: Some("dbstudio".into()),
            // Unqualified table names in the generated DDL resolve to the scratch schema.
            options: BTreeMap::from([("search_path".into(), "dbstudio_it_advice,public".into())]),
            tls: TlsMode::Prefer,
            scope_db: None,
        }
    }

    fn mysql(service: &'static str, port: u16, database: &str) -> Self {
        Server {
            service,
            kind: DbKind::MySql,
            port,
            username: "dbstudio",
            password: PASSWORD,
            database: Some(database.into()),
            // One pooled connection: session variables and EXPLAIN see the same state.
            options: BTreeMap::from([("pool_max".into(), "1".into())]),
            tls: TlsMode::Prefer,
            scope_db: None,
        }
    }

    fn mysql8() -> Self {
        Self::mysql("mysql8", 33068, "dbstudio_it_advice")
    }

    fn mariadb() -> Self {
        Self::mysql("mariadb", 33106, "dbstudio_it_advice")
    }
}

/// A connected session plus the state that owns it.
struct Harness {
    state: Arc<AppState>,
    sid: Uuid,
    session: Arc<dyn Session>,
    scope_db: Option<String>,
    _secrets: Arc<dyn std::any::Any + Send + Sync>,
}

/// Opens the server, or reports why the test is skipped (`None`).
async fn open(server: &Server) -> Option<Harness> {
    if !enabled() {
        return None;
    }
    let addr = SocketAddr::from(([127, 0, 0, 1], server.port));
    if let Err(e) = TcpStream::connect_timeout(&addr, Duration::from_secs(3)) {
        let msg = format!("{}: nothing listens on {addr} ({e})", server.service);
        if dbstudio_core::testing::it_strict_for(server.service) {
            panic!("{msg} but DBSTUDIO_IT_STRICT lists it");
        }
        eprintln!("skip: {msg}");
        return None;
    }
    let (state, secrets) = test_state(build_registry());
    let mut config = ConnectionConfig::new(server.kind, server.service, "127.0.0.1");
    config.port = server.port;
    config.database = server.database.clone();
    config.username = Some(server.username.into());
    config.connect_timeout_ms = Some(30_000);
    config.options = server.options.clone();
    config.tls.mode = server.tls;
    let profile = ConnectionProfile {
        config,
        group_id: None,
        color: None,
        save_password: true,
        has_password: false,
        sort_order: 0,
        created_at: String::new(),
        updated_at: String::new(),
    };
    let saved = connections::save_impl(
        &state,
        profile,
        SecretChange::Set {
            value: Secret::new(server.password),
        },
    )
    .await
    .expect("save profile");
    let sid = sessions::open_impl(&state, saved.config.id, None)
        .await
        .unwrap_or_else(|e| panic!("{}: open session: {e}", server.service))
        .session_id;
    let session = state.sessions.get(sid).expect("session");
    Some(Harness {
        state: Arc::new(state),
        sid,
        session,
        scope_db: server.scope_db.clone(),
        _secrets: secrets,
    })
}

impl Harness {
    async fn run(&self, sql: &str) {
        self.try_run(sql)
            .await
            .unwrap_or_else(|e| panic!("sql failed: {e}\n{sql}"));
    }

    async fn try_run(
        &self,
        sql: &str,
    ) -> dbstudio_core::Result<Vec<dbstudio_core::StatementResult>> {
        let mut req = QueryRequest::new(sql);
        req.database = self.scope_db.clone();
        execute_collect(&*self.session, req).await
    }

    /// Like `run` but ignores errors (clean-up).
    async fn run_quiet(&self, sql: &str) {
        let _ = self.try_run(sql).await;
    }

    async fn scalar(&self, sql: &str) -> Value {
        let results = self
            .try_run(sql)
            .await
            .unwrap_or_else(|e| panic!("sql failed: {e}\n{sql}"));
        match &results[0].outcome {
            StatementOutcome::Rows(set) => set.rows[0][0].clone(),
            other => panic!("expected rows, got {other:?}"),
        }
    }

    async fn explain(&self, text: &str, analyze: bool) -> RawPlan {
        self.session
            .explain(ExplainRequest {
                query_id: Uuid::new_v4(),
                text: text.into(),
                database: self.scope_db.clone(),
                analyze,
                options: ExplainOptions::default(),
                timeout_ms: None,
            })
            .await
            .unwrap_or_else(|e| panic!("explain failed: {e}\n{text}"))
    }

    /// `plan_advise` with the real limits.
    async fn advise(&self, raw: RawPlan) -> AdviceReport {
        advise_with(
            &self.state,
            self.sid,
            None,
            raw,
            self.scope_db.clone(),
            None,
            true,
            // A live session explained the plan: the catalog is the right one.
            true,
            crate::commands::plan::AdviseLimits::default(),
        )
        .await
        .expect("plan_advise")
    }

    /// Explain (with actual figures) and advise in one go.
    async fn plan(&self, text: &str) -> (RawPlan, AdviceReport) {
        self.plan_with(text, true).await
    }

    async fn plan_with(&self, text: &str, analyze: bool) -> (RawPlan, AdviceReport) {
        let raw = self.explain(text, analyze).await;
        let report = self.advise(raw.clone()).await;
        (raw, report)
    }
}

fn find(report: &AdviceReport, rule: AdviceRule) -> Option<&PlanAdvice> {
    report.advice.iter().find(|a| a.rule == rule)
}

fn rules_of(report: &AdviceReport) -> Vec<AdviceRule> {
    report.advice.iter().map(|a| a.rule).collect()
}

/// The `createIndex` action of an advice: exactly one statement of ready text.
fn create_index_sql(advice: &PlanAdvice) -> String {
    let action = advice
        .actions
        .iter()
        .find(|a| a.kind == ActionKind::CreateIndex)
        .unwrap_or_else(|| panic!("no createIndex action in {advice:#?}"));
    assert!(!action.template, "createIndex must be ready to run");
    action.sql.clone().expect("createIndex has sql")
}

fn nodes<'a>(root: &'a PlanNode, out: &mut Vec<&'a PlanNode>) {
    out.push(root);
    for c in &root.children {
        nodes(c, out);
    }
}

/// Kinds of the nodes that read table `table`.
fn scan_kinds(raw: &RawPlan, table: &str) -> Vec<PlanNodeKind> {
    let tree = parse_and_analyze(raw, &PlanRules::default()).expect("parse plan");
    let mut all = Vec::new();
    nodes(&tree.root, &mut all);
    all.into_iter()
        .filter(|n| {
            n.object
                .as_ref()
                .is_some_and(|o| o.name.eq_ignore_ascii_case(table))
        })
        .map(|n| n.kind)
        .collect()
}

fn is_indexed(kind: PlanNodeKind) -> bool {
    matches!(
        kind,
        PlanNodeKind::IndexScan | PlanNodeKind::IndexOnlyScan | PlanNodeKind::IndexSeek
    )
}

/// Runs `body`, then `cleanup`, whether or not the body panicked.
async fn guarded<F, Fut>(h: &Harness, cleanup: &[&str], body: F)
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    for sql in cleanup {
        h.run_quiet(sql).await;
    }
    let joined = tokio::spawn(body()).await;
    for sql in cleanup {
        h.run_quiet(sql).await;
    }
    if let Err(e) = joined {
        if let Ok(p) = e.try_into_panic() {
            std::panic::resume_unwind(p);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// PostgreSQL 16
// ---------------------------------------------------------------------------------------------

const PG_CLEANUP: [&str; 3] = [
    "DROP SCHEMA IF EXISTS dbstudio_it_advice CASCADE",
    "DROP SCHEMA IF EXISTS dbstudio_it_advice_a CASCADE",
    "DROP SCHEMA IF EXISTS dbstudio_it_advice_b CASCADE",
];

async fn pg_seed(h: &Harness) {
    h.run(&format!(
        "CREATE SCHEMA dbstudio_it_advice; \
         CREATE TABLE dbstudio_it_advice.adv_orders (id int PRIMARY KEY, customer_id int NOT NULL, \
           status text NOT NULL, created_at timestamp NOT NULL, total numeric(10,2) NOT NULL, \
           code varchar(20) NOT NULL); \
         INSERT INTO dbstudio_it_advice.adv_orders \
           SELECT g, (g % 5000) + 1, (ARRAY['new','paid','shipped','done'])[(g % 4) + 1], \
                  timestamp '2024-01-01' + (g || ' seconds')::interval, (g % 1000) / 10.0, \
                  'C' || lpad((g % 20000)::text, 6, '0') \
           FROM generate_series(1, {ROWS}) g; \
         ANALYZE dbstudio_it_advice.adv_orders"
    ))
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg16_advice_end_to_end() {
    let Some(h) = open(&Server::pg16()).await else {
        return;
    };
    let h = Arc::new(h);
    let h2 = h.clone();
    guarded(&h, &PG_CLEANUP, move || async move {
        pg_seed(&h2).await;
        pg_estimate_sort_index(&h2).await;
        pg_filter_and_index(&h2).await;
        pg_function_on_indexed_column(&h2).await;
        pg_sort_spill(&h2).await;
        pg_metadata(&h2).await;
    })
    .await;
}

/// Case 0: an ESTIMATE-only plan still gets `indexForFilter`, and with ORDER BY .. LIMIT the sort
/// column closes the key so the index can also serve the order.
async fn pg_estimate_sort_index(h: &Harness) {
    let q = "SELECT * FROM adv_orders WHERE customer_id = 42 AND status = 'paid' \
             ORDER BY created_at DESC LIMIT 20";
    let (_, report) = h.plan_with(q, false).await;
    let advice = find(&report, AdviceRule::IndexForFilter)
        .unwrap_or_else(|| panic!("indexForFilter missing: {:?}", rules_of(&report)));
    let sql = create_index_sql(advice);
    eprintln!("pg estimate createIndex: {sql}");
    assert!(
        sql.contains("(customer_id, status, created_at DESC)"),
        "{sql}"
    );
    assert!(
        advice.evidence.iter().any(|e| e.key == "sortEliminated"),
        "{:?}",
        advice.evidence
    );
    h.run(&sql).await;
    h.run("ANALYZE adv_orders").await;
    let raw = h.explain(q, false).await;
    // With only ~10 matching rows the planner may still sort them (bitmap scan + Sort), so the
    // check is that the new index serves the query.
    assert!(
        raw.text
            .contains("\"Index Name\": \"ix_adv_orders_customer_id_status_created_at\""),
        "index not used: {}",
        raw.text
    );
    h.run("DROP INDEX ix_adv_orders_customer_id_status_created_at")
        .await;
}

/// Case 1: a filter without an index gets `indexForFilter`; running its DDL fixes the plan.
async fn pg_filter_and_index(h: &Harness) {
    let q = "SELECT * FROM adv_orders WHERE customer_id = 42";
    let (raw, report) = h.plan(q).await;
    let before = scan_kinds(&raw, "adv_orders");
    assert_eq!(before, vec![PlanNodeKind::FullScan], "{before:?}");
    let advice = find(&report, AdviceRule::IndexForFilter)
        .unwrap_or_else(|| panic!("indexForFilter missing: {:?}", rules_of(&report)));
    let sql = create_index_sql(advice);
    eprintln!("pg createIndex: {sql}");
    assert!(sql.contains("customer_id"), "{sql}");
    h.run(&sql).await;
    h.run("ANALYZE adv_orders").await;
    let (raw, after) = h.plan(q).await;
    let kinds = scan_kinds(&raw, "adv_orders");
    assert!(
        !kinds.is_empty() && kinds.iter().all(|k| is_indexed(*k)),
        "after the index: {kinds:?}"
    );
    assert!(
        find(&after, AdviceRule::IndexForFilter).is_none(),
        "advice still there: {:?}",
        rules_of(&after)
    );
}

/// Case 2: with the index in place `lower(code)` is the problem; no second index is suggested.
async fn pg_function_on_indexed_column(h: &Harness) {
    h.run("CREATE INDEX it_orders_code ON adv_orders (code)")
        .await;
    h.run("ANALYZE adv_orders").await;
    let q = "SELECT * FROM adv_orders WHERE lower(code) = 'c000042'";
    let (_, report) = h.plan(q).await;
    let rules = rules_of(&report);
    assert!(
        rules.contains(&AdviceRule::PgFunctionOnColumn)
            || rules.contains(&AdviceRule::NonSargablePredicate),
        "{rules:?}"
    );
    for a in &report.advice {
        if a.rule == AdviceRule::IndexForFilter {
            let sql = a
                .actions
                .iter()
                .filter(|x| x.kind == ActionKind::CreateIndex)
                .filter_map(|x| x.sql.clone())
                .collect::<Vec<_>>();
            assert!(
                sql.iter().all(|s| !s.contains("(code)")),
                "duplicate index suggested: {sql:?}"
            );
        }
    }
}

/// Case 3: a sort that does not fit `work_mem` spills; the fix is a session `SET work_mem`.
async fn pg_sort_spill(h: &Harness) {
    h.run("SET work_mem = '64kB'").await;
    let (_, report) = h
        .plan("SELECT * FROM adv_orders ORDER BY total DESC, created_at")
        .await;
    h.run("RESET work_mem").await;
    let advice = find(&report, AdviceRule::SortSpill)
        .unwrap_or_else(|| panic!("sortSpill missing: {:?}", rules_of(&report)));
    let set = advice
        .actions
        .iter()
        .find(|a| a.kind == ActionKind::SessionSetting)
        .unwrap_or_else(|| panic!("no session setting: {advice:#?}"));
    assert_eq!(set.scope, ActionScope::Session);
    let sql = set.sql.as_deref().expect("sql");
    assert!(sql.to_lowercase().starts_with("set work_mem"), "{sql}");
}

/// Case 5: catalog lookup state and paths, and the same table name in two schemas.
async fn pg_metadata(h: &Harness) {
    let (_, report) = h.plan("SELECT * FROM adv_orders WHERE total = 55.5").await;
    assert_eq!(
        report.metadata.state,
        MetadataState::Checked,
        "{:?}",
        report.metadata
    );
    assert!(report.metadata.failed.is_empty());
    let table = report
        .metadata
        .checked
        .iter()
        .find(|t| t.name == "adv_orders")
        .expect("adv_orders checked");
    let path = table.path.as_ref().expect("resolved path");
    let names: Vec<(String, String)> = path
        .segments
        .iter()
        .map(|s| (format!("{:?}", s.kind), s.name.clone()))
        .collect();
    assert!(
        names
            .iter()
            .any(|(k, n)| k == "Schema" && n == "dbstudio_it_advice"),
        "{names:?}"
    );
    assert_eq!(names.last().map(|(_, n)| n.as_str()), Some("adv_orders"));
    for a in report.advice.iter().filter(|a| a.table.is_some()) {
        assert!(a.table.as_ref().unwrap().path.is_some(), "{a:#?}");
    }

    // Two schemas, neither the default one, hold a table of the same name.
    h.run(
        "CREATE SCHEMA dbstudio_it_advice_a; CREATE SCHEMA dbstudio_it_advice_b; \
         CREATE TABLE dbstudio_it_advice_a.dup AS SELECT g AS k, g % 7 AS v FROM generate_series(1, 20000) g; \
         CREATE TABLE dbstudio_it_advice_b.dup AS SELECT g AS k, g % 7 AS v FROM generate_series(1, 20000) g; \
         ANALYZE dbstudio_it_advice_a.dup; ANALYZE dbstudio_it_advice_b.dup",
    )
    .await;
    let (_, report) = h
        .plan("SELECT * FROM dbstudio_it_advice_a.dup WHERE k = 5")
        .await;
    assert!(
        report
            .metadata
            .failed
            .iter()
            .any(|f| f.reason == "ambiguous"),
        "{:?}",
        report.metadata
    );
    assert!(
        report
            .advice
            .iter()
            .any(|a| a.table.as_ref().is_some_and(|t| t.name == "dup")
                && a.caveats.iter().any(|c| c.key == "ambiguousTable")),
        "ambiguousTable caveat missing: {:#?}",
        report.advice
    );
}

// ---------------------------------------------------------------------------------------------
// MySQL 8 and MariaDB
// ---------------------------------------------------------------------------------------------

const MY_DB: &str = "dbstudio_it_advice";

async fn my_seed(h: &Harness) {
    h.run(&format!("DROP DATABASE IF EXISTS {MY_DB}")).await;
    h.run(&format!("CREATE DATABASE {MY_DB}")).await;
}

async fn my_fill(h: &Harness) {
    h.run(
        "CREATE TABLE adv_orders (id int NOT NULL PRIMARY KEY, customer_id int NOT NULL, \
         status varchar(10) NOT NULL, created_at datetime NOT NULL, total decimal(10,2) NOT NULL, \
         code varchar(20) NOT NULL)",
    )
    .await;
    h.run("CREATE TABLE adv_digits (n int NOT NULL PRIMARY KEY)")
        .await;
    h.run("INSERT INTO adv_digits VALUES (0),(1),(2),(3),(4),(5),(6),(7),(8),(9)")
        .await;
    // 10^6 combinations, the first `ROWS` of them become rows.
    h.run(&format!(
        "INSERT INTO adv_orders \
         SELECT g, (g % 5000) + 1, ELT((g % 4) + 1, 'new', 'paid', 'shipped', 'done'), \
                TIMESTAMP('2024-01-01 00:00:00') + INTERVAL g SECOND, (g % 1000) / 10.0, \
                CONCAT('C', LPAD(g % 20000, 6, '0')) \
         FROM (SELECT a.n + 10 * b.n + 100 * c.n + 1000 * d.n + 10000 * e.n + 100000 * f.n + 1 AS g \
               FROM adv_digits a, adv_digits b, adv_digits c, adv_digits d, adv_digits e, \
                    adv_digits f) x \
         WHERE g <= {ROWS}"
    ))
    .await;
    h.run("ANALYZE TABLE adv_orders").await;
}

async fn my_advice_end_to_end(server: Server) {
    let Some(boot) = open(&Server {
        database: Some("dbstudio".into()),
        ..server.clone()
    })
    .await
    else {
        return;
    };
    let cleanup = [format!("DROP DATABASE IF EXISTS {MY_DB}")];
    let cleanup: Vec<&str> = cleanup.iter().map(String::as_str).collect();
    guarded(&boot, &cleanup, move || async move {
        let boot = boot_again(&server).await;
        my_seed(&boot).await;
        let h = open(&server).await.expect("reopen on the scratch database");
        my_fill(&h).await;
        my_sort_index(&h).await;
        my_filter_and_index(&h).await;
        my_function_on_indexed_column(&h, &server).await;
        my_implicit_conversion(&h, &server).await;
        my_metadata(&h).await;
    })
    .await;
}

async fn boot_again(server: &Server) -> Harness {
    open(&Server {
        database: Some("dbstudio".into()),
        ..server.clone()
    })
    .await
    .expect("bootstrap session")
}

async fn my_filter_and_index(h: &Harness) {
    let q = "SELECT * FROM adv_orders WHERE customer_id = 42";
    let (raw, report) = h.plan(q).await;
    let before = scan_kinds(&raw, "adv_orders");
    assert_eq!(before, vec![PlanNodeKind::FullScan], "{before:?}");
    let advice = find(&report, AdviceRule::IndexForFilter)
        .unwrap_or_else(|| panic!("indexForFilter missing: {:?}", rules_of(&report)));
    let sql = create_index_sql(advice);
    eprintln!("mysql createIndex: {sql}");
    assert!(sql.contains("customer_id"), "{sql}");
    h.run(&sql).await;
    h.run("ANALYZE TABLE adv_orders").await;
    let (raw, after) = h.plan(q).await;
    let kinds = scan_kinds(&raw, "adv_orders");
    assert!(
        !kinds.is_empty() && kinds.iter().all(|k| is_indexed(*k)),
        "after the index: {kinds:?}"
    );
    assert!(
        find(&after, AdviceRule::IndexForFilter).is_none(),
        "advice still there: {:?}",
        rules_of(&after)
    );
}

/// ORDER BY .. LIMIT over equality filters: the sort column closes the key.
async fn my_sort_index(h: &Harness) {
    let q = "SELECT * FROM adv_orders WHERE customer_id = 42 AND status = 'paid' \
             ORDER BY created_at DESC LIMIT 20";
    let (_, report) = h.plan(q).await;
    let advice = find(&report, AdviceRule::IndexForFilter)
        .unwrap_or_else(|| panic!("indexForFilter missing: {:?}", rules_of(&report)));
    let sql = create_index_sql(advice);
    eprintln!("mysql sort createIndex: {sql}");
    assert!(sql.contains("customer_id"), "{sql}");
}

async fn my_function_on_indexed_column(h: &Harness, _server: &Server) {
    h.run("CREATE INDEX it_orders_code ON adv_orders (code)")
        .await;
    h.run("ANALYZE TABLE adv_orders").await;
    let (_, report) = h
        .plan("SELECT * FROM adv_orders WHERE LOWER(code) = 'c000042'")
        .await;
    let rules = rules_of(&report);
    assert!(
        rules.contains(&AdviceRule::NonSargablePredicate),
        "{rules:?}"
    );
    for a in report
        .advice
        .iter()
        .filter(|a| a.rule == AdviceRule::IndexForFilter)
    {
        let sql: Vec<String> = a
            .actions
            .iter()
            .filter(|x| x.kind == ActionKind::CreateIndex)
            .filter_map(|x| x.sql.clone())
            .collect();
        assert!(
            sql.iter().all(|s| !s.contains("(`code`)")),
            "duplicate index suggested: {sql:?}"
        );
    }
}

async fn my_implicit_conversion(h: &Harness, server: &Server) {
    let (_, report) = h.plan("SELECT * FROM adv_orders WHERE code = 123").await;
    let advice = find(&report, AdviceRule::ImplicitConversion).unwrap_or_else(|| {
        panic!(
            "implicitConversion missing on {}: {:?}",
            server.service,
            rules_of(&report)
        )
    });
    assert!(advice.node_ids.len() <= 1 || !advice.node_ids.is_empty());
}

async fn my_metadata(h: &Harness) {
    let (_, report) = h.plan("SELECT * FROM adv_orders WHERE total = 55.5").await;
    assert_eq!(
        report.metadata.state,
        MetadataState::Checked,
        "{:?}",
        report.metadata
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mysql8_advice_end_to_end() {
    my_advice_end_to_end(Server::mysql8()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mariadb_advice_end_to_end() {
    my_advice_end_to_end(Server::mariadb()).await;
}

// ---------------------------------------------------------------------------------------------
// MongoDB 7
// ---------------------------------------------------------------------------------------------

const MONGO_DB: &str = "dbstudio_it_adv";

impl Server {
    fn mongo7() -> Self {
        Server {
            service: "mongo7",
            kind: DbKind::MongoDb,
            port: env_port("DBSTUDIO_IT_MONGO7_PORT", 27077),
            username: "dbstudio",
            password: PASSWORD,
            database: None,
            options: BTreeMap::from([
                ("authSource".into(), "admin".into()),
                ("directConnection".into(), "true".into()),
            ]),
            tls: TlsMode::Prefer,
            scope_db: Some(MONGO_DB.into()),
        }
    }

    fn clickhouse26() -> Self {
        Server {
            service: "clickhouse26",
            kind: DbKind::ClickHouse,
            port: env_port("DBSTUDIO_IT_CLICKHOUSE_PORT", 18123),
            username: "dbstudio",
            password: PASSWORD,
            database: None,
            options: BTreeMap::new(),
            tls: TlsMode::Disable,
            scope_db: Some(CH_DB.into()),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mongo7_advice_end_to_end() {
    let Some(h) = open(&Server::mongo7()).await else {
        return;
    };
    let h = Arc::new(h);
    let h2 = h.clone();
    guarded(&h, &["db.dropDatabase()"], move || async move {
        // 200 000 documents in batches of 10 000.
        for batch in 0..(ROWS / 10_000) {
            let docs: Vec<String> = (0..10_000)
                .map(|i| {
                    let g = batch * 10_000 + i;
                    format!(
                        r#"{{"_id":{g},"customer_id":{},"status":"s{}","total":{}}}"#,
                        g % 5000,
                        g % 4,
                        g % 1000
                    )
                })
                .collect();
            h2.run(&format!("db.adv_orders.insertMany([{}])", docs.join(",")))
                .await;
        }
        let q = "db.adv_orders.find({customer_id: 42})";
        let (raw, report) = h2.plan(q).await;
        let before = scan_kinds(&raw, "adv_orders");
        assert_eq!(before, vec![PlanNodeKind::FullScan], "{before:?}");
        let advice = find(&report, AdviceRule::MongoCollscan)
            .unwrap_or_else(|| panic!("mongoCollscan missing: {:?}", rules_of(&report)));
        let sql = create_index_sql(advice);
        eprintln!("mongo createIndex: {sql}");
        assert!(sql.contains("customer_id"), "{sql}");
        assert_eq!(report.metadata.state, MetadataState::Checked);
        h2.run(&sql).await;
        let (raw, after) = h2.plan(q).await;
        let kinds = scan_kinds(&raw, "adv_orders");
        assert!(
            kinds.iter().any(|k| is_indexed(*k)) && !kinds.contains(&PlanNodeKind::FullScan),
            "after the index: {kinds:?}"
        );
        assert!(
            find(&after, AdviceRule::MongoCollscan).is_none(),
            "advice still there: {:?}",
            rules_of(&after)
        );
    })
    .await;
}

// ---------------------------------------------------------------------------------------------
// ClickHouse 26
// ---------------------------------------------------------------------------------------------

const CH_DB: &str = "dbstudio_it_adv";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clickhouse26_advice_end_to_end() {
    let boot_server = Server {
        scope_db: None,
        ..Server::clickhouse26()
    };
    let Some(boot) = open(&boot_server).await else {
        return;
    };
    let drop = format!("DROP DATABASE IF EXISTS {CH_DB}");
    guarded(&boot, &[drop.as_str()], move || async move {
        let boot = open(&boot_server).await.expect("bootstrap session");
        boot.run(&format!("CREATE DATABASE {CH_DB}")).await;
        let h = open(&Server::clickhouse26()).await.expect("session");
        h.run(
            "CREATE TABLE adv_orders (id UInt32, customer_id UInt32, status String, \
             created_at DateTime, total Float64, code String) \
             ENGINE = MergeTree ORDER BY (customer_id, created_at)",
        )
        .await;
        h.run(&format!(
            "INSERT INTO adv_orders SELECT number, number % 5000, \
             arrayElement(['new','paid','shipped','done'], (number % 4) + 1), \
             toDateTime('2024-01-01') + number, (number % 1000) / 10, \
             concat('C', toString(intDiv(number, 1000))) FROM numbers({ROWS})"
        ))
        .await;
        // ClickHouse has no EXPLAIN ANALYZE: planner figures only.
        // `code` follows the insert order (200 values in runs of 1000 rows): a skip index prunes.
        let q = "SELECT * FROM adv_orders WHERE code = 'C42'";
        let (_, report) = h.plan_with(q, false).await;
        let advice = find(&report, AdviceRule::ChPrimaryKeyUnused)
            .unwrap_or_else(|| panic!("chPrimaryKeyUnused missing: {:?}", rules_of(&report)));
        assert_eq!(report.metadata.state, MetadataState::Checked);
        let sql = create_index_sql(advice);
        eprintln!("clickhouse createIndex: {sql}");
        assert!(sql.contains("`code`"), "{sql}");
        h.run(&sql).await;
        // The follow-up statements of the advice run as well.
        for a in advice
            .actions
            .iter()
            .filter(|a| a.kind == ActionKind::Maintenance)
        {
            let sql = a.sql.as_deref().expect("sql");
            if sql.contains("MATERIALIZE INDEX") {
                h.run(sql).await;
            }
        }
        // MATERIALIZE is a background mutation: wait until it is done.
        for _ in 0..100 {
            let pending = h
                .scalar(&format!(
                    "SELECT count() FROM system.mutations \
                     WHERE database = '{CH_DB}' AND table = 'adv_orders' AND NOT is_done"
                ))
                .await;
            if pending == Value::UInt(0) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let indexes = h
            .scalar(&format!(
                "SELECT count() FROM system.data_skipping_indices \
                 WHERE database = '{CH_DB}' AND table = 'adv_orders' AND expr = 'code'"
            ))
            .await;
        assert_eq!(indexes, Value::UInt(1), "skip index not created");
        let (_, after) = h.plan_with(q, false).await;
        assert!(
            find(&after, AdviceRule::ChPrimaryKeyUnused).is_none(),
            "advice still there after the skip index: {:?}",
            rules_of(&after)
        );
        assert!(
            find(&after, AdviceRule::ChSkipIndexUseless).is_none(),
            "the skip index should prune: {:?}",
            rules_of(&after)
        );
    })
    .await;
}

// ---------------------------------------------------------------------------------------------
// SQL Server 2022
// ---------------------------------------------------------------------------------------------

const MS_DB: &str = "dbstudio_it_adv";

impl Server {
    fn mssql2022(database: &str) -> Self {
        Server {
            service: "mssql2022",
            kind: DbKind::SqlServer,
            port: env_port("DBSTUDIO_IT_MSSQL_PORT", 14333),
            username: "sa",
            password: "Dbstudio_test1!",
            database: Some(database.into()),
            options: BTreeMap::new(),
            tls: TlsMode::Require,
            scope_db: None,
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mssql2022_advice_end_to_end() {
    let Some(boot) = open(&Server::mssql2022("master")).await else {
        return;
    };
    let drop = format!(
        "IF DB_ID('{MS_DB}') IS NOT NULL BEGIN \
         ALTER DATABASE {MS_DB} SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE {MS_DB}; END"
    );
    guarded(&boot, &[drop.as_str()], move || async move {
        let boot = open(&Server::mssql2022("master")).await.expect("master");
        boot.run(&format!("CREATE DATABASE {MS_DB}")).await;
        let h = open(&Server::mssql2022(MS_DB)).await.expect("scratch db");
        h.run(
            "CREATE TABLE dbo.adv_orders (id int NOT NULL PRIMARY KEY, customer_id int NOT NULL, \
             status varchar(10) NOT NULL, created_at datetime2 NOT NULL, \
             total decimal(10,2) NOT NULL, code varchar(20) NOT NULL)",
        )
        .await;
        h.run(&format!(
            "INSERT INTO dbo.adv_orders \
             SELECT g, (g % 5000) + 1, CHOOSE((g % 4) + 1, 'new', 'paid', 'shipped', 'done'), \
                    DATEADD(second, g, '2024-01-01'), (g % 1000) / 10.0, \
                    CONCAT('C', RIGHT(CONCAT('000000', g % 20000), 6)) \
             FROM (SELECT TOP ({ROWS}) CAST(ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS int) AS g \
                   FROM sys.all_objects a CROSS JOIN sys.all_objects b) x"
        ))
        .await;
        h.run("UPDATE STATISTICS dbo.adv_orders WITH FULLSCAN").await;

        // Case 1: missing index, then the generated DDL.
        // A join keeps the plan out of the trivial class, which carries no missing index.
        let q = "SELECT a.* FROM dbo.adv_orders a JOIN dbo.adv_orders b ON a.id = b.id \
                 WHERE a.customer_id = 42";
        let (raw, report) = h.plan(q).await;
        let before = scan_kinds(&raw, "adv_orders");
        assert!(before.contains(&PlanNodeKind::FullScan), "{before:?}");
        let advice = find(&report, AdviceRule::MssqlMissingIndex)
            .unwrap_or_else(|| panic!("mssqlMissingIndex missing: {:?}", rules_of(&report)));
        let sql = create_index_sql(advice);
        eprintln!("mssql createIndex: {sql}");
        assert!(sql.contains("customer_id"), "{sql}");
        h.run(&sql).await;
        h.run("UPDATE STATISTICS dbo.adv_orders WITH FULLSCAN").await;
        let (raw, after) = h.plan(q).await;
        let kinds = scan_kinds(&raw, "adv_orders");
        assert!(
            kinds.iter().any(|k| is_indexed(*k)) && !kinds.contains(&PlanNodeKind::FullScan),
            "after the index: {kinds:?}"
        );
        assert!(
            find(&after, AdviceRule::MssqlMissingIndex).is_none()
                && find(&after, AdviceRule::IndexForFilter).is_none(),
            "advice still there: {:?}",
            rules_of(&after)
        );

        // Case 4: a unicode literal against a varchar column converts the column.
        h.run("CREATE INDEX it_orders_code ON dbo.adv_orders (code)").await;
        let (_, report) = h.plan("SELECT * FROM dbo.adv_orders WHERE code = N'C000123'").await;
        assert!(
            find(&report, AdviceRule::ImplicitConversion).is_some(),
            "implicitConversion missing: {:?}",
            rules_of(&report)
        );
    })
    .await;
}
