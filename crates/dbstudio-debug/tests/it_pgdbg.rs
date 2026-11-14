//! Integration tests of the PostgreSQL routine debugger against `pg16dbg` (PostgreSQL 16 +
//! pldebugger, port 54317, see `docker/pg16dbg`). They only run with `DBSTUDIO_IT=1`:
//!
//! ```text
//! docker compose -f docker/compose.test.yml --profile debug up -d --wait pg16dbg
//! DBSTUDIO_IT=1 cargo test -p dbstudio-debug --test it_pgdbg
//! ```
//!
//! Tests that need a server without the package use `pg16` (port 54316) and skip when it is down.

mod common;

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use dbstudio_core::debug::{
    Breakpoint, RoutineBreakpoint, RoutineDebugSupport, RoutineRef, SourceRef,
};
use dbstudio_core::{
    CollectingSink, ConnectionConfig, DbKind, Driver, Error, Lease, LeaseOptions, QueryRequest,
    Result, Secret, Session, StatementClass, StatementInfo, StatementOutcome, TextRange, TlsConfig,
    TlsMode, Value, execute_collect,
};
use dbstudio_debug::routine::pg::{PgRoutineDebugger, flavor_from_version};
use dbstudio_debug::{ProxyOpener, RoutineCommand, RoutineDebugger, RoutineStop, Step, StepKind};
use dbstudio_driver_postgres::PostgresDriver;
use tokio::sync::{OnceCell, oneshot};

const PGDBG: u16 = 54317;
const PG16: u16 = 54316;
const PASSWORD: &str = "dbstudio_test";
const LIMIT: Duration = Duration::from_secs(30);

fn enabled() -> bool {
    std::env::var("DBSTUDIO_IT").as_deref() == Ok("1")
}

fn pgdbg_port() -> u16 {
    std::env::var("DBSTUDIO_IT_PGDBG_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(PGDBG)
}

async fn timed<T>(f: impl Future<Output = T>) -> T {
    tokio::time::timeout(LIMIT, f)
        .await
        .expect("timed out: a debugger call hung")
}

async fn try_connect(
    port: u16,
    database: &str,
    user: &str,
    password: &str,
) -> Result<Arc<dyn Session>> {
    let mut cfg = ConnectionConfig::new(DbKind::Postgres, "it", "127.0.0.1");
    cfg.port = port;
    cfg.database = Some(database.into());
    cfg.username = Some(user.into());
    cfg.tls = TlsConfig {
        mode: TlsMode::Prefer,
        ..TlsConfig::default()
    };
    cfg.connect_timeout_ms = Some(5000);
    let s = PostgresDriver::new()
        .connect(&cfg, Some(&Secret::new(password)))
        .await?;
    Ok(Arc::from(s))
}

async fn connect() -> Arc<dyn Session> {
    try_connect(pgdbg_port(), "dbstudio", "dbstudio", PASSWORD)
        .await
        .expect("connect to pg16dbg (docker compose --profile debug up -d --wait pg16dbg)")
}

async fn sql(session: &dyn Session, text: &str) {
    execute_collect(session, QueryRequest::new(text))
        .await
        .unwrap_or_else(|e| panic!("{text}: {e}"));
}

async fn lease(session: &dyn Session) -> Arc<dyn Lease> {
    Arc::from(
        session
            .open_lease(LeaseOptions::default())
            .await
            .expect("open lease"),
    )
}

struct SessionOpener(Arc<dyn Session>);

#[async_trait]
impl ProxyOpener for SessionOpener {
    async fn open_proxy(&self, database: Option<String>) -> Result<Arc<dyn Lease>> {
        let l = self
            .0
            .open_lease(LeaseOptions {
                database,
                ..LeaseOptions::default()
            })
            .await?;
        Ok(Arc::from(l))
    }
}

// Routines with a known layout. Line 1 is the empty line after `$$`.
const FIXTURES: &str = "
CREATE OR REPLACE FUNCTION public.dbgt_inner(x integer) RETURNS integer LANGUAGE plpgsql AS $$
DECLARE
  r integer;
BEGIN
  r := x * 2;
  RETURN r;
END
$$;
CREATE OR REPLACE FUNCTION public.dbgt_outer(x integer) RETURNS integer LANGUAGE plpgsql AS $$
DECLARE
  total integer := 0;
  i integer;
BEGIN
  FOR i IN 1..x LOOP
    total := total + public.dbgt_inner(i);
  END LOOP;
  RETURN total;
END
$$;
CREATE OR REPLACE FUNCTION public.dbgt_types() RETURNS text LANGUAGE plpgsql AS $$
DECLARE
  t text := 'init';
  n numeric := 1;
  d date := DATE '2020-01-01';
  b boolean := false;
BEGIN
  t := t;
  RETURN t || '|' || coalesce(n::text, 'null') || '|' || d::text || '|' || b::text;
END
$$;
CREATE TABLE IF NOT EXISTS public.dbgt_t (id integer);
CREATE OR REPLACE FUNCTION public.dbgt_trg() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  NEW.id := NEW.id + 100;
  RETURN NEW;
END
$$;
DROP TRIGGER IF EXISTS dbgt_before_insert ON public.dbgt_t;
CREATE TRIGGER dbgt_before_insert BEFORE INSERT ON public.dbgt_t
  FOR EACH ROW EXECUTE FUNCTION public.dbgt_trg();
";

static FIXTURE_ONCE: OnceCell<()> = OnceCell::const_new();

async fn fixtures(session: &dyn Session) {
    FIXTURE_ONCE
        .get_or_init(|| async {
            sql(session, FIXTURES).await;
        })
        .await;
}

fn rref(sig: &str) -> RoutineRef {
    RoutineRef {
        signature: sig.into(),
        oid: None,
        path: None,
    }
}

fn user_bp(sig: &str, id: &str, line: u32) -> RoutineBreakpoint {
    RoutineBreakpoint {
        routine: rref(sig),
        source_hash: None,
        breakpoint: Breakpoint {
            id: id.into(),
            line,
            enabled: true,
            suspend: true,
            condition: None,
            log_message: None,
            hit_count: None,
        },
    }
}

const OUTER: &str = "public.dbgt_outer(integer)";
const INNER: &str = "public.dbgt_inner(integer)";

type Slot = Arc<Mutex<Option<Result<Vec<dbstudio_core::StatementResult>>>>>;
/// Resolves when the background statement ended, and keeps resolving (a plain `async` block
/// would panic when polled again after completion).
struct Done {
    rx: oneshot::Receiver<()>,
    fired: bool,
}

impl Future for Done {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<()> {
        if self.fired {
            return std::task::Poll::Ready(());
        }
        match Pin::new(&mut self.rx).poll(cx) {
            std::task::Poll::Ready(_) => {
                self.fired = true;
                std::task::Poll::Ready(())
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

/// A target lease running one statement in the background, plus the debugger.
struct Harness {
    session: Arc<dyn Session>,
    target: Arc<dyn Lease>,
    dbg: PgRoutineDebugger,
    slot: Slot,
    done: Option<Done>,
}

impl Harness {
    async fn new() -> Harness {
        let session = connect().await;
        fixtures(&*session).await;
        let target = lease(&*session).await;
        let dbg = PgRoutineDebugger::new(Arc::new(SessionOpener(session.clone())), None);
        Harness {
            session,
            target,
            dbg,
            slot: Arc::new(Mutex::new(None)),
            done: None,
        }
    }

    async fn support(&mut self) -> RoutineDebugSupport {
        timed(self.dbg.support(&*self.target))
            .await
            .expect("support")
    }

    /// Start `text` on the target in the background.
    fn run(&mut self, text: &str) {
        let (tx, rx) = oneshot::channel::<()>();
        let slot: Slot = Arc::new(Mutex::new(None));
        self.slot = slot.clone();
        let target = self.target.clone();
        let text = text.to_string();
        tokio::spawn(async move {
            let mut sink = CollectingSink::new();
            let r = target
                .execute(QueryRequest::new(text), &mut sink)
                .await
                .map(|_| sink.into_results());
            *slot.lock().unwrap() = Some(r);
            let _ = tx.send(());
        });
        self.done = Some(Done { rx, fired: false });
    }

    async fn arm_and_run(
        &mut self,
        statement: &str,
        entries: &[&str],
        bps: &[RoutineBreakpoint],
    ) -> RoutineStop {
        self.support().await;
        let targets: Vec<RoutineRef> = entries.iter().map(|s| rref(s)).collect();
        let statuses = timed(self.dbg.arm(&targets, bps)).await.expect("arm");
        assert_eq!(statuses.len(), bps.len());
        self.run(statement);
        self.wait().await
    }

    async fn wait(&mut self) -> RoutineStop {
        let done = self.done.as_mut().expect("run() first");
        timed(self.dbg.wait(done)).await.expect("wait")
    }

    async fn cmd(&mut self, c: RoutineCommand) -> RoutineStop {
        let done = self.done.as_mut().expect("run() first");
        timed(self.dbg.command(c, done)).await.expect("command")
    }

    /// Result of the background statement (waits for it).
    async fn result(&self) -> Result<Vec<dbstudio_core::StatementResult>> {
        let deadline = std::time::Instant::now() + LIMIT;
        loop {
            if let Some(r) = self.slot.lock().unwrap().take() {
                return r;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "target never finished"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn scalar_result(&self) -> Value {
        let results = self.result().await.expect("target statement");
        match &results[0].outcome {
            StatementOutcome::Rows(rs) => rs.rows[0][0].clone(),
            other => panic!("not rows: {other:?}"),
        }
    }
}

fn at(stop: &RoutineStop) -> (&str, u32, u32) {
    match stop {
        RoutineStop::At {
            routine,
            line,
            depth,
        } => (routine.signature.as_str(), *line, *depth),
        other => panic!("expected a stop inside a routine, got {other:?}"),
    }
}

fn step_of(text: &str, targets: &[&str]) -> Step {
    Step {
        index: 0,
        kind: StepKind::Statement,
        range: TextRange {
            start: 0,
            end: text.chars().count() as u32,
        },
        line_start: 1,
        line_end: 1,
        info: StatementInfo {
            index: 0,
            range: TextRange {
                start: 0,
                end: text.chars().count() as u32,
            },
            class: StatementClass::Read,
            verb: "SELECT".into(),
            destructive: None,
            implicit_commit: false,
            targets: targets.iter().map(|t| t.to_string()).collect(),
        },
        text: text.to_string(),
    }
}

macro_rules! it {
    ($name:ident, $body:expr) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $name() {
            if enabled() {
                $body.await
            }
        }
    };
}

// ------------------------------------------------------------------------------------------
// Detection
// ------------------------------------------------------------------------------------------

it!(pgdbg_support_available, async {
    let mut h = Harness::new().await;
    assert_eq!(h.support().await, RoutineDebugSupport::Available);
    let rows = execute_collect(&*h.session, QueryRequest::new("SELECT version()"))
        .await
        .unwrap();
    let StatementOutcome::Rows(rs) = &rows[0].outcome else {
        panic!()
    };
    let Value::Text(version) = &rs.rows[0][0] else {
        panic!()
    };
    assert_eq!(
        flavor_from_version(version),
        dbstudio_debug::routine::pg::PgFlavor::Postgres
    );
    assert!(h.dbg.target_pid().is_some());
});

it!(pgdbg_support_needs_extension_in_a_fresh_database, async {
    let admin = connect().await;
    let exists = execute_collect(
        &*admin,
        QueryRequest::new("SELECT 1 FROM pg_database WHERE datname = 'dbgt_noext'"),
    )
    .await
    .unwrap();
    let has_rows = matches!(&exists[0].outcome, StatementOutcome::Rows(rs) if !rs.rows.is_empty());
    if !has_rows {
        sql(&*admin, "CREATE DATABASE dbgt_noext").await;
    }
    let session = try_connect(pgdbg_port(), "dbgt_noext", "dbstudio", PASSWORD)
        .await
        .expect("connect to dbgt_noext");
    let target = lease(&*session).await;
    let mut dbg = PgRoutineDebugger::new(Arc::new(SessionOpener(session.clone())), None);
    let support = dbg.support(&*target).await.unwrap();
    assert_eq!(
        support,
        RoutineDebugSupport::NeedsExtension {
            statement: "CREATE EXTENSION pldbgapi;".into()
        }
    );
    // Arming a statement then fails with the instruction instead of a raw SQL error.
    let err = dbg.arm(&[rref(OUTER)], &[]).await.unwrap_err();
    assert!(
        matches!(&err, Error::Unsupported(m) if m.contains("CREATE EXTENSION pldbgapi")),
        "{err:?}"
    );
    target.close().await.unwrap();
    drop(session);
    let _ = execute_collect(&*admin, QueryRequest::new("DROP DATABASE dbgt_noext")).await;
});

it!(pgdbg_server_without_the_package_gives_guidance, async {
    // `pg16` is a plain postgres:16 image: no pldebugger files at all.
    let Ok(session) = try_connect(PG16, "dbstudio", "dbstudio", PASSWORD).await else {
        eprintln!("pg16 is not running: skipped");
        return;
    };
    let target = lease(&*session).await;
    let mut dbg = PgRoutineDebugger::new(Arc::new(SessionOpener(session.clone())), None);
    let support = dbg.support(&*target).await.unwrap();
    let RoutineDebugSupport::Unsupported { reason } = &support else {
        panic!("expected Unsupported, got {support:?}");
    };
    assert!(reason.contains("pldebugger") && reason.contains("shared_preload_libraries"));
    let err = dbg.arm(&[rref(OUTER)], &[]).await.unwrap_err();
    assert!(
        matches!(&err, Error::Unsupported(m) if m.contains("pldebugger")),
        "{err:?}"
    );
    // Even without a prior support() call the raw failure is turned into guidance.
    let mut cold = PgRoutineDebugger::new(Arc::new(SessionOpener(session.clone())), None);
    cold.set_target_pid(dbg.target_pid().unwrap());
    let err = cold.arm(&[rref(OUTER)], &[]).await.unwrap_err();
    assert!(
        matches!(&err, Error::Unsupported(m) if m.contains("CREATE EXTENSION") || m.contains("pldebugger")),
        "{err:?}"
    );
    target.close().await.unwrap();
});

it!(pgdbg_plain_role_gets_no_permission, async {
    let admin = connect().await;
    sql(
        &*admin,
        "DO $$ BEGIN IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'dbgt_plain') THEN \
         CREATE ROLE dbgt_plain LOGIN PASSWORD 'dbgt_plain'; END IF; END $$",
    )
    .await;
    let session = try_connect(pgdbg_port(), "dbstudio", "dbgt_plain", "dbgt_plain")
        .await
        .expect("connect as dbgt_plain");
    let target = lease(&*session).await;
    let mut dbg = PgRoutineDebugger::new(Arc::new(SessionOpener(session.clone())), None);
    let support = dbg.support(&*target).await.unwrap();
    assert!(
        matches!(&support, RoutineDebugSupport::NoPermission { hint } if hint.contains("superuser")),
        "{support:?}"
    );
    let err = dbg.arm(&[rref(OUTER)], &[]).await.unwrap_err();
    assert!(matches!(err, Error::Unsupported(_)), "{err:?}");
    target.close().await.unwrap();
});

it!(pgdbg_arm_without_a_pid_is_refused, async {
    let session = connect().await;
    let mut dbg = PgRoutineDebugger::new(Arc::new(SessionOpener(session.clone())), None);
    let err = dbg.arm(&[rref(OUTER)], &[]).await.unwrap_err();
    assert!(matches!(err, Error::Invalid(_)), "{err:?}");
});

// ------------------------------------------------------------------------------------------
// Stepping
// ------------------------------------------------------------------------------------------

it!(pgdbg_step_into_outer_then_inner, async {
    let mut h = Harness::new().await;
    h.support().await;
    // Candidates come from the statement text through the trait method.
    let text = "SELECT dbgt_outer(2)";
    let cands = timed(
        h.dbg
            .step_into_targets(&*h.target, &step_of(text, &[]), false),
    )
    .await
    .unwrap();
    assert_eq!(
        cands
            .iter()
            .map(|c| c.signature.as_str())
            .collect::<Vec<_>>(),
        [OUTER]
    );
    timed(h.dbg.arm(&cands, &[])).await.unwrap();
    h.run(text);

    // Step Into the statement: first line of f_outer (the FOR, line 6).
    let stop = h.wait().await;
    assert_eq!(at(&stop), (OUTER, 6, 1));
    let frames = timed(h.dbg.frames()).await.unwrap();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].line, 6);
    assert_eq!(frames[0].language.as_deref(), Some("plpgsql"));
    assert!(
        matches!(&frames[0].source, SourceRef::Routine { routine } if routine.signature == OUTER)
    );

    // Step Over the FOR header, then Step Into the nested call.
    assert_eq!(at(&h.cmd(RoutineCommand::StepOver).await), (OUTER, 7, 1));
    let stop = h.cmd(RoutineCommand::StepInto).await;
    assert_eq!(at(&stop), (INNER, 5, 2));

    // Two frames, innermost first.
    let frames = timed(h.dbg.frames()).await.unwrap();
    assert_eq!(frames.len(), 2);
    assert!(frames[0].name.contains("dbgt_inner") && frames[0].id == 0);
    assert!(frames[1].name.contains("dbgt_outer") && frames[1].id == 1);
    assert_eq!(frames[1].line, 7);
    assert_eq!(frames[0].args.as_deref(), Some("x=1"));

    // Variables of the selected frame, typed.
    let vars = timed(h.dbg.variables()).await.unwrap();
    let x = vars.iter().find(|v| v.name == "x").unwrap();
    assert_eq!(x.display, "1");
    assert_eq!(x.type_name.as_deref(), Some("integer"));
    assert_eq!(x.kind, dbstudio_core::debug::VariableKind::RoutineArgument);
    let r = vars.iter().find(|v| v.name == "r").unwrap();
    assert_eq!(r.display, "NULL");
    assert_eq!(r.kind, dbstudio_core::debug::VariableKind::RoutineLocal);
    // Selecting the outer frame shows its variables.
    timed(h.dbg.select_frame(1)).await.unwrap();
    let vars = timed(h.dbg.variables()).await.unwrap();
    assert!(vars.iter().any(|v| v.name == "total"));
    assert!(vars.iter().all(|v| v.reference.starts_with("1:")));
    timed(h.dbg.select_frame(0)).await.unwrap();

    // Step Over inside f_inner, a second Step Over returns to f_outer.
    assert_eq!(at(&h.cmd(RoutineCommand::StepOver).await), (INNER, 6, 2));
    let stop = h.cmd(RoutineCommand::StepOver).await;
    assert_eq!(at(&stop).2, 1);
    assert_eq!(at(&stop).0, OUTER);

    assert_eq!(
        h.cmd(RoutineCommand::Continue).await,
        RoutineStop::TargetDone
    );
    assert_eq!(h.scalar_result().await, Value::Int(6));
    timed(h.dbg.disarm()).await.unwrap();
});

it!(pgdbg_step_out_returns_to_the_caller, async {
    let mut h = Harness::new().await;
    let stop = h.arm_and_run("SELECT dbgt_outer(2)", &[OUTER], &[]).await;
    assert_eq!(at(&stop), (OUTER, 6, 1));
    h.cmd(RoutineCommand::StepOver).await;
    assert_eq!(at(&h.cmd(RoutineCommand::StepInto).await), (INNER, 5, 2));
    // Simulated Step Out: back in f_outer, one frame deep.
    let stop = timed(h.dbg.step_out()).await.unwrap();
    assert_eq!(at(&stop).0, OUTER);
    assert_eq!(at(&stop).2, 1);
    // Step Out of the outermost routine runs the statement to its end.
    assert_eq!(
        timed(h.dbg.step_out()).await.unwrap(),
        RoutineStop::TargetDone
    );
    assert_eq!(h.scalar_result().await, Value::Int(6));
    timed(h.dbg.disarm()).await.unwrap();
});

// ------------------------------------------------------------------------------------------
// Breakpoints
// ------------------------------------------------------------------------------------------

it!(
    pgdbg_user_breakpoint_hits_while_stepping_over_the_call,
    async {
        let mut h = Harness::new().await;
        // No entry breakpoint: the statement runs until the user breakpoint in f_inner (line 5).
        let bp = user_bp(INNER, "b1", 5);
        let stop = h.arm_and_run("SELECT dbgt_outer(3)", &[], &[bp]).await;
        assert_eq!(at(&stop), (INNER, 5, 2));
        let mut seen = Vec::new();
        loop {
            let vars = timed(h.dbg.variables()).await.unwrap();
            seen.push(vars.iter().find(|v| v.name == "x").unwrap().display.clone());
            match h.cmd(RoutineCommand::Continue).await {
                RoutineStop::At {
                    routine,
                    line,
                    depth,
                } => {
                    assert_eq!((routine.signature.as_str(), line, depth), (INNER, 5, 2));
                }
                RoutineStop::TargetDone => break,
                other => panic!("{other:?}"),
            }
        }
        // Hit once per loop iteration, with the right argument each time.
        assert_eq!(seen, ["1", "2", "3"]);
        assert_eq!(h.scalar_result().await, Value::Int(12));
        timed(h.dbg.disarm()).await.unwrap();
    }
);

it!(pgdbg_breakpoint_statuses, async {
    let mut h = Harness::new().await;
    h.support().await;
    let mut disabled = user_bp(INNER, "off", 5);
    disabled.breakpoint.enabled = false;
    let mut stale = user_bp(INNER, "stale", 5);
    stale.source_hash = Some("0".repeat(64));
    let missing = user_bp("public.dbgt_nope(integer)", "gone", 3);
    let good = user_bp(INNER, "ok", 5);
    let statuses = timed(h.dbg.arm(&[], &[disabled, stale, missing, good]))
        .await
        .unwrap();
    let by_id = |id: &str| statuses.iter().find(|s| s.id == id).unwrap();
    assert!(!by_id("off").verified);
    assert!(!by_id("stale").verified);
    assert!(
        by_id("stale")
            .message
            .as_deref()
            .unwrap()
            .contains("changed")
    );
    assert!(!by_id("gone").verified);
    assert!(by_id("ok").verified);
    assert_eq!(by_id("ok").line, 5);
    timed(h.dbg.disarm()).await.unwrap();
});

it!(pgdbg_force_step_into_stops_in_a_trigger_function, async {
    let mut h = Harness::new().await;
    h.support().await;
    let text = "INSERT INTO dbgt_t VALUES (1)";
    // Without force the trigger function is not a candidate; with force it is found.
    let plain =
        timed(
            h.dbg
                .candidates_for_text(&*h.target, text, &["public.dbgt_t".into()], false),
        )
        .await
        .unwrap();
    assert!(plain.is_empty());
    let forced =
        timed(
            h.dbg
                .candidates_for_text(&*h.target, text, &["public.dbgt_t".into()], true),
        )
        .await
        .unwrap();
    assert_eq!(forced.len(), 1);
    assert_eq!(forced[0].signature, "public.dbgt_trg()");
    timed(h.dbg.arm(&forced, &[])).await.unwrap();
    h.run(text);
    let stop = h.wait().await;
    assert_eq!(at(&stop).0, "public.dbgt_trg()");
    assert_eq!(at(&stop).2, 1);
    let vars = timed(h.dbg.variables()).await.unwrap();
    assert!(
        vars.iter()
            .any(|v| v.name == "new" || v.name == "NEW" || v.name == "tg_name")
    );
    assert_eq!(
        h.cmd(RoutineCommand::Continue).await,
        RoutineStop::TargetDone
    );
    h.result().await.expect("insert");
    timed(h.dbg.disarm()).await.unwrap();
    sql(&*h.session, "DELETE FROM dbgt_t").await;
});

// ------------------------------------------------------------------------------------------
// Variables
// ------------------------------------------------------------------------------------------

it!(pgdbg_set_variable_changes_the_result, async {
    let mut h = Harness::new().await;
    let stop = h.arm_and_run("SELECT dbgt_outer(1)", &[INNER], &[]).await;
    assert_eq!(at(&stop), (INNER, 5, 2));
    let vars = timed(h.dbg.variables()).await.unwrap();
    let x = vars.iter().find(|v| v.name == "x").unwrap();
    assert!(x.editable);
    assert_eq!(x.display, "1");
    let updated = timed(h.dbg.set_variable(&x.reference, "10")).await.unwrap();
    assert_eq!(updated.display, "10");
    assert!(updated.changed);
    // Invalid values are refused with a clear error, the session stays usable.
    let err = timed(h.dbg.set_variable(&x.reference, "'xx'"))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Invalid(_)), "{err:?}");
    // Run to the end: r := 10 * 2.
    assert_eq!(
        h.cmd(RoutineCommand::Continue).await,
        RoutineStop::TargetDone
    );
    assert_eq!(h.scalar_result().await, Value::Int(20));
    timed(h.dbg.disarm()).await.unwrap();
});

it!(pgdbg_deposit_accepts_expressions_text_and_null, async {
    let mut h = Harness::new().await;
    let stop = h
        .arm_and_run("SELECT dbgt_types()", &["public.dbgt_types()"], &[])
        .await;
    assert_eq!(at(&stop).0, "public.dbgt_types()");
    let find = |vars: &[dbstudio_core::debug::DebugVariable], n: &str| {
        vars.iter().find(|v| v.name == n).unwrap().clone()
    };
    let vars = timed(h.dbg.variables()).await.unwrap();
    assert_eq!(find(&vars, "t").display, "'init'");
    assert_eq!(find(&vars, "n").type_name.as_deref(), Some("numeric"));
    assert_eq!(find(&vars, "d").type_name.as_deref(), Some("date"));
    assert_eq!(find(&vars, "b").display, "false");

    // Plain text for a text variable, a literal for a number, a date in text form, true.
    let t = timed(h.dbg.set_variable(&find(&vars, "t").reference, "abc"))
        .await
        .unwrap();
    assert_eq!(t.display, "'abc'");
    // Text is stored as typed: `current_user` is not evaluated, `1+1` is not a number.
    let t = timed(
        h.dbg
            .set_variable(&find(&vars, "t").reference, "current_user"),
    )
    .await
    .unwrap();
    assert_eq!(t.display, "'current_user'");
    assert!(
        timed(h.dbg.set_variable(&find(&vars, "n").reference, "1+1"))
            .await
            .is_err()
    );
    let n = timed(h.dbg.set_variable(&find(&vars, "n").reference, "2.5"))
        .await
        .unwrap();
    assert_eq!(n.display, "2.5");
    let d = timed(
        h.dbg
            .set_variable(&find(&vars, "d").reference, "2024-02-03"),
    )
    .await
    .unwrap();
    assert_eq!(d.display, "'2024-02-03'");
    let b = timed(h.dbg.set_variable(&find(&vars, "b").reference, "true"))
        .await
        .unwrap();
    assert_eq!(b.display, "true");
    // NULL is a null, not the text "NULL".
    let n = timed(h.dbg.set_variable(&find(&vars, "n").reference, "NULL"))
        .await
        .unwrap();
    assert_eq!(n.value, Some(Value::Null));

    assert_eq!(
        h.cmd(RoutineCommand::Continue).await,
        RoutineStop::TargetDone
    );
    assert_eq!(
        h.scalar_result().await,
        Value::Text("current_user|null|2024-02-03|true".into())
    );
    timed(h.dbg.disarm()).await.unwrap();
});

it!(pgdbg_routine_source_matches_prosrc, async {
    let mut h = Harness::new().await;
    let stop = h.arm_and_run("SELECT dbgt_outer(1)", &[OUTER], &[]).await;
    let RoutineStop::At { routine, .. } = stop else {
        panic!()
    };
    let src = timed(h.dbg.source(&routine)).await.unwrap();
    assert!(src.text.starts_with("\nDECLARE"));
    assert_eq!(src.language, "plpgsql");
    assert_eq!(src.first_line, 1);
    assert_eq!(
        src.source_hash,
        dbstudio_debug::routine::pg::source_hash(&src.text)
    );
    // Line 6 of the source is the FOR header, where the target stopped.
    assert!(src.text.lines().nth(5).unwrap().contains("FOR i IN"));
    h.cmd(RoutineCommand::Continue).await;
    h.result().await.unwrap();
    timed(h.dbg.disarm()).await.unwrap();
});

// ------------------------------------------------------------------------------------------
// Stop, isolation, cleanup
// ------------------------------------------------------------------------------------------

it!(pgdbg_abort_in_the_middle_does_not_hang, async {
    let mut h = Harness::new().await;
    let stop = h.arm_and_run("SELECT dbgt_outer(3)", &[OUTER], &[]).await;
    assert_eq!(at(&stop).2, 1);
    h.cmd(RoutineCommand::StepOver).await;
    h.cmd(RoutineCommand::StepInto).await;
    assert_eq!(h.cmd(RoutineCommand::Abort).await, RoutineStop::Aborted);
    // The statement of the target fails with a cancel error ...
    let err = h.result().await.unwrap_err();
    assert!(
        matches!(&err, Error::Query { message, .. } if message.contains("cancel"))
            || matches!(err, Error::Cancelled),
        "{err:?}"
    );
    timed(h.dbg.disarm()).await.unwrap();
    // ... and the connection is usable again, also for the same routine without a debugger.
    let r = execute_collect_on(&*h.target, "SELECT dbgt_outer(2)").await;
    assert_eq!(r, Value::Int(6));
    assert!(!h.target.state().broken);
});

it!(pgdbg_other_backends_are_not_stopped, async {
    let mut h = Harness::new().await;
    let stop = h
        .arm_and_run(
            "SELECT dbgt_outer(2)",
            &[OUTER, INNER],
            &[user_bp(INNER, "b", 5)],
        )
        .await;
    assert_eq!(at(&stop).0, OUTER);
    // A third connection calls the very same routines while the target is stopped.
    let other = lease(&*h.session).await;
    let v = timed(execute_collect_on(&*other, "SELECT dbgt_outer(2)")).await;
    assert_eq!(v, Value::Int(6));
    // The target is still where it was.
    let frames = timed(h.dbg.frames()).await.unwrap();
    assert_eq!(frames.len(), 1);
    h.cmd(RoutineCommand::Abort).await;
    h.result().await.unwrap_err();
    timed(h.dbg.disarm()).await.unwrap();
    other.close().await.unwrap();
});

it!(pgdbg_statement_without_a_stop_leaves_nothing_armed, async {
    let mut h = Harness::new().await;
    // The statement never calls the armed routine: wait() ends when the statement ends.
    let stop = h.arm_and_run("SELECT 41 + 1", &[INNER], &[]).await;
    assert_eq!(stop, RoutineStop::TargetDone);
    assert_eq!(h.scalar_result().await, Value::Int(42));
    timed(h.dbg.disarm()).await.unwrap();
    // Closing the proxy cleared the global breakpoint: the routine now runs without stopping
    // (it would hang until the 30 s limit otherwise).
    let v = timed(execute_collect_on(&*h.target, "SELECT dbgt_inner(5)")).await;
    assert_eq!(v, Value::Int(10));
});

it!(pgdbg_proxy_loss_lets_the_target_run_on, async {
    let mut h = Harness::new().await;
    let stop = h.arm_and_run("SELECT dbgt_outer(2)", &[OUTER], &[]).await;
    assert_eq!(at(&stop).2, 1);
    // The proxy goes away while the target is stopped (app crash / network loss).
    timed(h.dbg.disarm()).await.unwrap();
    assert_eq!(h.scalar_result().await, Value::Int(6));
});

it!(pgdbg_sessions_can_be_repeated, async {
    let mut h = Harness::new().await;
    for round in 1..=3 {
        let stop = h.arm_and_run("SELECT dbgt_outer(1)", &[OUTER], &[]).await;
        assert_eq!(at(&stop), (OUTER, 6, 1), "round {round}");
        // The end of the outermost routine must not cost the ~9 s server timeout.
        let began = std::time::Instant::now();
        assert_eq!(
            h.cmd(RoutineCommand::Continue).await,
            RoutineStop::TargetDone
        );
        assert!(
            began.elapsed() < Duration::from_secs(3),
            "ending the routine took {:?}",
            began.elapsed()
        );
        assert_eq!(h.scalar_result().await, Value::Int(2));
        timed(h.dbg.disarm()).await.unwrap();
    }
});

it!(pgdbg_proxy_killed_mid_session_does_not_hang, async {
    let mut h = Harness::new().await;
    let stop = h.arm_and_run("SELECT dbgt_outer(2)", &[OUTER], &[]).await;
    assert_eq!(at(&stop).2, 1);
    let proxy_pid = h.dbg.proxy_pid().expect("proxy pid");
    let other = lease(&*h.session).await;
    execute_collect_on(
        &*other,
        &format!("SELECT pg_terminate_backend({proxy_pid})"),
    )
    .await;
    // The next command fails or ends the routine, but never hangs.
    let _ = timed(async {
        let done = h.done.as_mut().unwrap();
        h.dbg.command(RoutineCommand::StepOver, done).await
    })
    .await;
    timed(h.dbg.disarm()).await.unwrap();
    // The target is released and finishes (or fails) on its own.
    let _ = h.result().await;
    assert!(!h.target.state().broken);
    other.close().await.unwrap();
});

it!(pgdbg_third_session_before_attach_is_not_stopped, async {
    let mut h = Harness::new().await;
    h.support().await;
    let targets = vec![rref(OUTER), rref(INNER)];
    timed(h.dbg.arm(&targets, &[])).await.expect("arm");
    // Another backend runs the armed routines before the target statement starts.
    let other = lease(&*h.session).await;
    let v = timed(execute_collect_on(&*other, "SELECT dbgt_outer(2)")).await;
    assert_eq!(v, Value::Int(6));
    h.run("SELECT dbgt_outer(2)");
    let stop = h.wait().await;
    assert_eq!(at(&stop).0, OUTER);
    h.cmd(RoutineCommand::Abort).await;
    h.result().await.unwrap_err();
    timed(h.dbg.disarm()).await.unwrap();
    other.close().await.unwrap();
});

// ------------------------------------------------------------------------------------------
// Raw pldbgapi behaviour the module relies on (facts quoted in the module documentation)
// ------------------------------------------------------------------------------------------

async fn execute_collect_on(lease: &dyn Lease, text: &str) -> Value {
    let mut sink = CollectingSink::new();
    lease
        .execute(QueryRequest::new(text), &mut sink)
        .await
        .unwrap_or_else(|e| panic!("{text}: {e}"));
    match &sink.into_results()[0].outcome {
        StatementOutcome::Rows(rs) => rs.rows[0][0].clone(),
        other => panic!("not rows: {other:?}"),
    }
}

it!(pgdbg_raw_entry_line_minus_one_equals_null, async {
    let session = connect().await;
    fixtures(&*session).await;
    for line_sql in ["-1", "NULL"] {
        let target = lease(&*session).await;
        let proxy = lease(&*session).await;
        let pid = match execute_collect_on(&*target, "SELECT pg_backend_pid()").await {
            Value::Int(n) => n,
            other => panic!("{other:?}"),
        };
        let s = match execute_collect_on(&*proxy, "SELECT pldbg_create_listener()").await {
            Value::Int(n) => n,
            other => panic!("{other:?}"),
        };
        let oid = execute_collect_on(
            &*proxy,
            "SELECT 'public.dbgt_outer(integer)'::regprocedure::oid::int8",
        )
        .await;
        let Value::Int(oid) = oid else { panic!() };
        execute_collect_on(
            &*proxy,
            &format!("SELECT pldbg_set_global_breakpoint({s}, {oid}::oid, {line_sql}, {pid})"),
        )
        .await;
        let t2 = target.clone();
        let run = tokio::spawn(async move {
            let mut sink = CollectingSink::new();
            t2.execute(QueryRequest::new("SELECT dbgt_outer(1)"), &mut sink)
                .await
        });
        timed(execute_collect_on(
            &*proxy,
            &format!("SELECT pldbg_wait_for_target({s})"),
        ))
        .await;
        let line = execute_collect_on(
            &*proxy,
            &format!("SELECT linenumber FROM pldbg_get_stack({s}) LIMIT 1"),
        )
        .await;
        // The first executable statement, not the DECLARE: the stack reports line 6.
        assert_eq!(line, Value::Int(6), "line {line_sql}");
        // The tuples of pldbg_wait_for_breakpoint are one line ahead of the stack.
        let wfb = execute_collect_on(
            &*proxy,
            &format!("SELECT linenumber FROM pldbg_wait_for_breakpoint({s})"),
        )
        .await;
        assert_eq!(wfb, Value::Int(7));
        proxy.close().await.unwrap();
        run.await
            .unwrap()
            .expect("target runs on after the proxy closed");
        target.close().await.unwrap();
    }
});

it!(
    pgdbg_raw_listener_is_reusable_after_a_cancelled_wait,
    async {
        let session = connect().await;
        fixtures(&*session).await;
        let target = lease(&*session).await;
        let proxy = lease(&*session).await;
        let pid = match execute_collect_on(&*target, "SELECT pg_backend_pid()").await {
            Value::Int(n) => n,
            other => panic!("{other:?}"),
        };
        let Value::Int(proxy_pid) = execute_collect_on(&*proxy, "SELECT pg_backend_pid()").await
        else {
            panic!()
        };
        let Value::Int(s) = execute_collect_on(&*proxy, "SELECT pldbg_create_listener()").await
        else {
            panic!()
        };
        let Value::Int(oid) = execute_collect_on(
            &*proxy,
            "SELECT 'public.dbgt_inner(integer)'::regprocedure::oid::int8",
        )
        .await
        else {
            panic!()
        };
        let set = format!("SELECT pldbg_set_global_breakpoint({s}, {oid}::oid, -1, {pid})");
        execute_collect_on(&*proxy, &set).await;

        // First wait is cancelled (nothing runs on the target).
        let request = QueryRequest::new(format!("SELECT pldbg_wait_for_target({s})"));
        let qid = request.query_id;
        let p2 = proxy.clone();
        let waiter = tokio::spawn(async move {
            let mut sink = CollectingSink::new();
            p2.execute(request, &mut sink).await
        });
        // Wait until the call really sits in the server (no fixed sleep).
        let probe = format!(
            "SELECT count(*) FROM pg_stat_activity WHERE state = 'active' AND pid = {proxy_pid}"
        );
        timed(async {
            loop {
                let seen = execute_collect_on(&*target, &probe).await;
                if !matches!(seen, Value::Int(0) | Value::UInt(0)) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        proxy.cancel(qid).await.unwrap();
        let cancelled = timed(waiter).await.unwrap();
        assert!(cancelled.is_err(), "the cancelled wait must fail");

        // The breakpoint survived the cancelled wait: setting it again is an error, and the same
        // listener serves the next target run without being re-armed.
        let again = proxy
            .execute(QueryRequest::new(&set), &mut CollectingSink::new())
            .await
            .unwrap_err();
        assert!(
            matches!(&again, Error::Query { message, .. } if message.contains("already waiting")),
            "{again:?}"
        );
        let t2 = target.clone();
        let run = tokio::spawn(async move {
            let mut sink = CollectingSink::new();
            t2.execute(QueryRequest::new("SELECT dbgt_inner(4)"), &mut sink)
                .await
        });
        timed(execute_collect_on(
            &*proxy,
            &format!("SELECT pldbg_wait_for_target({s})"),
        ))
        .await;
        let line = execute_collect_on(
            &*proxy,
            &format!("SELECT linenumber FROM pldbg_get_stack({s}) LIMIT 1"),
        )
        .await;
        assert_eq!(line, Value::Int(5));
        execute_collect_on(&*proxy, &format!("SELECT pldbg_abort_target({s})::text")).await;
        assert!(run.await.unwrap().is_err());
        proxy.close().await.unwrap();
        target.close().await.unwrap();
    }
);

// ------------------------------------------------------------------------------------------
// End to end through DebugEngine (real actor + PgRoutineDebugger, no fakes besides the gate)
// ------------------------------------------------------------------------------------------

async fn engine_session(text: &str) -> (dbstudio_debug::DebugHandle, Arc<dyn Session>) {
    engine_session_with(text, vec![], false).await
}

async fn engine_session_with(
    text: &str,
    routine_breakpoints: Vec<RoutineBreakpoint>,
    muted: bool,
) -> (dbstudio_debug::DebugHandle, Arc<dyn Session>) {
    use dbstudio_core::Capabilities;
    use dbstudio_core::debug::{DebugEndAction, DebugOptions, DebugStartRequest};
    let session = connect().await;
    fixtures(&*session).await;
    let target = lease(&*session).await;
    let profile = dbstudio_debug::DialectProfile {
        kind: DbKind::Postgres,
        flavor: None,
        version: None,
        dialect: target.script_dialect().unwrap_or(session.script_dialect()),
        caps: Capabilities {
            manual_transactions: true,
            routine_debug: true,
            ..Capabilities::default()
        },
    };
    let request = DebugStartRequest {
        lease_id: uuid::Uuid::new_v4(),
        text: text.into(),
        range: None,
        params: vec![],
        breakpoints: vec![],
        routine_breakpoints,
        watches: vec![],
        options: DebugOptions {
            end_action: DebugEndAction::Rollback,
            ..Default::default()
        },
        muted,
    };
    let h = timed(dbstudio_debug::DebugEngine::start(
        profile,
        target,
        Arc::new(common::FakeGate::default()),
        Arc::new(common::RecSink::default()),
        Arc::new(SessionOpener(session.clone())),
        dbstudio_debug::DebugSettings::default(),
        request,
    ))
    .await
    .expect("start");
    (h, session)
}

async fn settle(h: &dbstudio_debug::DebugHandle) -> dbstudio_core::debug::DebugState {
    use dbstudio_core::debug::DebugStatus;
    timed(async {
        loop {
            let s = h.snapshot().await.expect("snapshot");
            if matches!(
                s.status,
                DebugStatus::Paused { .. } | DebugStatus::Terminated { .. }
            ) {
                return s;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
}

it!(pgdbg_engine_steps_into_a_routine_and_stops, async {
    use dbstudio_core::debug::{DebugAction, DebugStatus, PauseReason, SourceRef, TerminateReason};
    let (h, _session) = engine_session("SELECT dbgt_outer(2);").await;
    let s = settle(&h).await;
    assert!(matches!(
        s.status,
        DebugStatus::Paused {
            reason: PauseReason::Entry
        }
    ));
    timed(h.act(DebugAction::StepInto, None)).await.unwrap();
    let s = settle(&h).await;
    assert!(
        matches!(
            s.status,
            DebugStatus::Paused {
                reason: PauseReason::RoutineEntry
            }
        ),
        "{:?}",
        s.status
    );
    let pos = s.position.clone().expect("position");
    assert!(matches!(pos.source, SourceRef::Routine { .. }));
    assert!(s.frames.len() >= 2, "routine frame above the script frame");
    for _ in 0..3 {
        timed(h.act(DebugAction::StepOver, None)).await.unwrap();
        let s = settle(&h).await;
        assert!(
            matches!(s.status, DebugStatus::Paused { .. }),
            "{:?}",
            s.status
        );
    }
    let report = timed(h.stop(None)).await.expect("stop");
    assert!(report.rolled_back || !report.committed);
    let end = h.snapshot().await.unwrap();
    assert!(matches!(
        end.status,
        DebugStatus::Terminated {
            reason: TerminateReason::Stopped,
            ..
        }
    ));
});

it!(pgdbg_engine_resume_runs_the_routine_to_the_end, async {
    use dbstudio_core::debug::{DebugAction, DebugStatus, TerminateReason};
    let (h, _session) = engine_session("SELECT dbgt_outer(2);").await;
    settle(&h).await;
    timed(h.act(DebugAction::StepInto, None)).await.unwrap();
    settle(&h).await;
    timed(h.act(DebugAction::Resume, None)).await.unwrap();
    let s = settle(&h).await;
    assert!(
        matches!(
            s.status,
            DebugStatus::Terminated {
                reason: TerminateReason::Finished,
                ..
            }
        ),
        "{:?}",
        s.status
    );
});

// Line 7 of dbgt_outer is `total := total + public.dbgt_inner(i)` inside the loop.
const LOOP_LINE: u32 = 7;

fn routine_pause_line(s: &dbstudio_core::debug::DebugState) -> Option<(String, u32)> {
    match s.position.as_ref()? {
        dbstudio_core::debug::DebugPosition {
            source: dbstudio_core::debug::SourceRef::Routine { routine },
            line,
            ..
        } => Some((routine.signature.clone(), *line)),
        _ => None,
    }
}

it!(pgdbg_engine_resume_stops_at_a_routine_breakpoint, async {
    use dbstudio_core::debug::{DebugAction, DebugStatus, PauseReason};
    let (h, _s) = engine_session_with(
        "SELECT dbgt_outer(2);",
        vec![user_bp(OUTER, "rb", LOOP_LINE)],
        false,
    )
    .await;
    settle(&h).await;
    timed(h.act(DebugAction::Resume, None)).await.unwrap();
    let s = settle(&h).await;
    assert!(
        matches!(
            s.status,
            DebugStatus::Paused {
                reason: PauseReason::Breakpoint { .. }
            }
        ),
        "{:?}",
        s.status
    );
    assert_eq!(routine_pause_line(&s), Some((OUTER.into(), LOOP_LINE)));
    timed(h.stop(None)).await.unwrap();
});

it!(
    pgdbg_engine_step_over_the_call_stops_at_a_routine_breakpoint,
    async {
        use dbstudio_core::debug::{DebugAction, DebugStatus, PauseReason};
        let (h, _s) = engine_session_with(
            "SELECT dbgt_outer(2);",
            vec![user_bp(INNER, "rb", 5)],
            false,
        )
        .await;
        settle(&h).await;
        timed(h.act(DebugAction::StepOver, None)).await.unwrap();
        let s = settle(&h).await;
        assert!(
            matches!(
                s.status,
                DebugStatus::Paused {
                    reason: PauseReason::Breakpoint { .. }
                }
            ),
            "{:?}",
            s.status
        );
        assert_eq!(routine_pause_line(&s), Some((INNER.into(), 5)));
        // Resume goes on to the next hit (second call), then to the end.
        timed(h.act(DebugAction::Resume, None)).await.unwrap();
        let s = settle(&h).await;
        assert_eq!(routine_pause_line(&s), Some((INNER.into(), 5)));
        timed(h.act(DebugAction::Resume, None)).await.unwrap();
        let s = settle(&h).await;
        assert!(
            matches!(s.status, DebugStatus::Terminated { .. }),
            "{:?}",
            s.status
        );
    }
);

it!(pgdbg_engine_muted_routine_breakpoints_do_not_stop, async {
    use dbstudio_core::debug::{DebugAction, DebugStatus, TerminateReason};
    let (h, _s) = engine_session_with(
        "SELECT dbgt_outer(2);",
        vec![user_bp(OUTER, "rb", LOOP_LINE)],
        true,
    )
    .await;
    settle(&h).await;
    timed(h.act(DebugAction::Resume, None)).await.unwrap();
    let s = settle(&h).await;
    assert!(
        matches!(
            s.status,
            DebugStatus::Terminated {
                reason: TerminateReason::Finished,
                ..
            }
        ),
        "{:?}",
        s.status
    );
});

it!(
    pgdbg_engine_routine_breakpoint_condition_and_hit_count,
    async {
        use dbstudio_core::debug::{DebugAction, DebugStatus};
        // condition: only the call with i = 2 stops
        let mut bp = user_bp(OUTER, "rb", LOOP_LINE);
        bp.breakpoint.condition = Some("i = 2".into());
        let (h, _s) = engine_session_with("SELECT dbgt_outer(3);", vec![bp], false).await;
        settle(&h).await;
        timed(h.act(DebugAction::Resume, None)).await.unwrap();
        let s = settle(&h).await;
        assert_eq!(
            routine_pause_line(&s),
            Some((OUTER.into(), LOOP_LINE)),
            "{:?}",
            s.status
        );
        let vars = timed(h.variable_children("g:Routine")).await.unwrap();
        let i = vars.iter().rfind(|v| v.name == "i").expect("variable i");
        assert_eq!(i.display, "2");
        timed(h.act(DebugAction::Resume, None)).await.unwrap();
        let s = settle(&h).await;
        assert!(
            matches!(s.status, DebugStatus::Terminated { .. }),
            "{:?}",
            s.status
        );

        // hit count: the third pass
        let mut bp = user_bp(OUTER, "rb", LOOP_LINE);
        bp.breakpoint.hit_count = Some(3);
        let (h, _s) = engine_session_with("SELECT dbgt_outer(3);", vec![bp], false).await;
        settle(&h).await;
        timed(h.act(DebugAction::Resume, None)).await.unwrap();
        let s = settle(&h).await;
        assert_eq!(routine_pause_line(&s), Some((OUTER.into(), LOOP_LINE)));
        timed(h.stop(None)).await.unwrap();
    }
);

it!(pgdbg_engine_run_to_cursor_inside_a_routine, async {
    use dbstudio_core::debug::{DebugAction, DebugStatus, PauseReason, SourceRef};
    let (h, _s) = engine_session("SELECT dbgt_outer(2);").await;
    settle(&h).await;
    timed(h.act(DebugAction::StepInto, None)).await.unwrap();
    settle(&h).await;
    // RETURN total is line 9
    timed(h.act(
        DebugAction::RunToCursor {
            source: SourceRef::Routine {
                routine: rref(OUTER),
            },
            line: 9,
            force: false,
        },
        None,
    ))
    .await
    .unwrap();
    let s = settle(&h).await;
    assert!(
        matches!(
            s.status,
            DebugStatus::Paused {
                reason: PauseReason::RunToCursor
            }
        ),
        "{:?}",
        s.status
    );
    assert_eq!(routine_pause_line(&s), Some((OUTER.into(), 9)));
    // the caps of a stopped routine: no jump
    assert!(!s.caps.jump_to);
    timed(h.stop(None)).await.unwrap();
});
