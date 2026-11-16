//! Opt-in, READ-ONLY smoke test against a real SQL Server (made for a 2008 R2 production box).
//!
//! The test reads the connection from the environment variable `MSSQL_CONNECTION`, an ADO.NET
//! connection string (parsed with the same parser as the "paste a connection string" import).
//! Without the variable every test prints `skipped` and passes. All tests are `#[ignore]`.
//!
//! Run it from the repository root (zsh):
//!
//! ```text
//! export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:$PATH"
//! export MSSQL_CONNECTION='Server=host,1433;Database=db;User Id=user;Password=secret;TrustServerCertificate=yes;Encrypt=false;'
//! cargo test -p dbstudio --lib mssql_smoke -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Safety: nothing is ever written. The profile is created `read_only` (the app's own guard
//! refuses writes), the only statements sent are `SELECT`s (`@@VERSION`, `sys.partitions` /
//! `sys.objects` catalog reads, `SELECT TOP (5)` of one table) plus the driver's own catalog
//! reads for the tree, table paging and DDL text. Nothing calls `apply_changes`, opens a lease
//! or transaction, creates temp objects or explains with execution. The profile lives in an
//! in-memory test state, and the password is only passed in memory as a `Secret` (no keychain,
//! no disk). The password is never printed.

use std::time::Instant;

use dbstudio_core::{
    CollectingSink, DdlOptions, DdlRequest, ObjectKind, ObjectPath, QueryRequest, Secret, Session,
    StatementOutcome, TableDataRequest, TlsMode, Value, execute_collect,
};
use dbstudio_store::SecretChange;
use uuid::Uuid;

use crate::commands::uri::parse_ado_net as ado;
use crate::commands::{connections, sessions};
use crate::state::build_registry;
use crate::state::testing::test_state;

/// Rows per page of the paging check (kept tiny on purpose).
const PAGE: u64 = 5;

fn connection_string() -> Option<String> {
    match std::env::var("MSSQL_CONNECTION") {
        Ok(v) if !v.trim().is_empty() => Some(v),
        _ => {
            eprintln!("mssql_smoke: skipped (MSSQL_CONNECTION is not set)");
            None
        }
    }
}

fn to_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Int(i) => Some(*i),
        Value::UInt(u) => i64::try_from(*u).ok(),
        Value::Decimal(s) | Value::Text(s) => s.trim().parse().ok(),
        Value::Float(f) => Some(*f as i64),
        _ => None,
    }
}

fn to_text(v: &Value) -> String {
    match v {
        Value::Text(s) => s.clone(),
        other => format!("{other:?}"),
    }
}

fn bracket(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}

/// Leading number of a version such as `10.50.6000.34`.
fn major_of(version: &str) -> Option<u32> {
    let digits: String = version
        .trim()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

macro_rules! step {
    ($n:expr, $title:expr, $body:expr) => {{
        let started = Instant::now();
        println!("[step {}] {} ...", $n, $title);
        match $body.await {
            Ok(v) => {
                println!("[step {}] ok in {} ms", $n, started.elapsed().as_millis());
                v
            }
            Err(e) => {
                return Err(format!(
                    "step {} ({}) failed after {} ms:\n{e}\n{e:?}",
                    $n,
                    $title,
                    started.elapsed().as_millis()
                ));
            }
        }
    }};
}

/// Runs a catalog `SELECT` in `db` and returns its rows.
async fn select(
    session: &dyn Session,
    db: &str,
    sql: &str,
) -> dbstudio_core::Result<Vec<Vec<Value>>> {
    let mut req = QueryRequest::new(sql);
    req.database = Some(db.to_string());
    let results = execute_collect(session, req).await?;
    match results.into_iter().next().map(|r| r.outcome) {
        Some(StatementOutcome::Rows(set)) => Ok(set.rows),
        other => Err(dbstudio_core::Error::Other(format!(
            "expected rows, got {other:?}"
        ))),
    }
}

async fn names(
    session: &dyn Session,
    path: &ObjectPath,
) -> dbstudio_core::Result<Vec<(String, ObjectPath)>> {
    Ok(session
        .list_children(path)
        .await?
        .into_iter()
        .map(|n| (n.name, n.path))
        .collect())
}

async fn run(session: &dyn Session, db: &str) -> Result<(), String> {
    // 2. server info
    let info = step!(2, "server info", session.server_info());
    println!(
        "  product={} version={} extra={:?}",
        info.product, info.version, info.extra
    );
    let major = major_of(&info.version);
    println!("  major version: {major:?}");

    // 3. object tree
    let root = ObjectPath::root();
    let dbs = step!(3, "databases", names(session, &root));
    println!(
        "  databases: {} -> {:?}",
        dbs.len(),
        dbs.iter().map(|d| &d.0).collect::<Vec<_>>()
    );
    let db_path = dbs
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(db))
        .map(|(_, p)| p.clone())
        .ok_or_else(|| format!("database `{db}` is not in the tree"))?;
    let db_children = step!(3, "database children", names(session, &db_path));
    println!(
        "  database children: {:?}",
        db_children.iter().map(|d| &d.0).collect::<Vec<_>>()
    );
    let schemas_folder = db_children
        .iter()
        .find(|(n, _)| n == "schemas")
        .map(|(_, p)| p.clone())
        .unwrap_or_else(|| db_path.child(ObjectKind::Folder, "schemas"));
    let schemas = step!(3, "schemas", names(session, &schemas_folder));
    println!(
        "  schemas: {} -> {:?}",
        schemas.len(),
        schemas.iter().map(|d| &d.0).collect::<Vec<_>>()
    );
    let dbo = schemas
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("dbo"))
        .map(|(_, p)| p.clone())
        .ok_or("schema dbo not found")?;
    let folders = step!(3, "dbo folders", names(session, &dbo));
    let mut first_table = None;
    for (name, path) in &folders {
        let items = step!(3, format!("dbo folder `{name}`"), names(session, path));
        println!("  dbo/{name}: {} item(s)", items.len());
        if name == "tables" {
            first_table = items.first().map(|(_, p)| p.clone());
        }
        if name == "sequences" && major.is_some_and(|m| m < 11) && !items.is_empty() {
            return Err(format!(
                "SQL Server {} has no sequences but the `sequences` folder lists {} item(s): \
                 version-aware browsing (other agent's work) is not merged yet",
                info.version,
                items.len()
            ));
        }
    }
    if major.is_some_and(|m| m < 11) {
        println!(
            "  sequences folder: {}",
            if folders.iter().any(|(n, _)| n == "sequences") {
                "present and empty"
            } else {
                "absent"
            }
        );
    }
    if let Some(table) = first_table {
        let tfolders = step!(3, "table sub-folders", names(session, &table));
        for (name, path) in &tfolders {
            let items = step!(3, format!("table folder `{name}`"), names(session, path));
            println!(
                "  {}/{name}: {} item(s)",
                table.last().map_or("?", |s| s.name.as_str()),
                items.len()
            );
        }
    } else {
        println!("  dbo has no table; table sub-folders skipped");
    }

    // 4. paging on the smallest table with at least 2 rows
    let smallest = step!(
        4,
        "pick the smallest table with >= 2 rows (SELECT on sys.partitions)",
        select(
            session,
            db,
            "SELECT TOP (1) s.name, o.name, SUM(p.rows) AS n \
             FROM sys.objects o \
             JOIN sys.schemas s ON s.schema_id = o.schema_id \
             JOIN sys.partitions p ON p.object_id = o.object_id AND p.index_id IN (0, 1) \
             WHERE o.type = 'U' AND o.is_ms_shipped = 0 \
             GROUP BY s.name, o.name HAVING SUM(p.rows) >= 2 \
             ORDER BY SUM(p.rows), s.name, o.name"
        )
    );
    let row = smallest
        .first()
        .ok_or("no user table with at least 2 rows")?;
    let (schema, table) = (to_text(&row[0]), to_text(&row[1]));
    let total = to_i64(&row[2]).unwrap_or(2).max(2) as u64;
    println!("  table {schema}.{table}, ~{total} row(s)");
    let table_path = db_path
        .child(ObjectKind::Schema, schema.clone())
        .child(ObjectKind::Table, table.clone());
    let page = PAGE.min(total / 2).max(1);
    let fetch = |offset: u64| {
        let table_path = table_path.clone();
        async move {
            let mut sink = CollectingSink::new();
            session
                .fetch_table(
                    TableDataRequest {
                        query_id: Uuid::new_v4(),
                        table: table_path,
                        filters: vec![],
                        raw_where: None,
                        sort: vec![],
                        offset,
                        limit: page,
                        batch_size: None,
                    },
                    &mut sink,
                )
                .await?;
            match sink.into_results().into_iter().next().map(|r| r.outcome) {
                Some(StatementOutcome::Rows(set)) => Ok(set),
                other => Err(dbstudio_core::Error::Other(format!(
                    "expected rows, got {other:?}"
                ))),
            }
        }
    };
    let p1 = step!(4, format!("page 1 (offset 0, limit {page})"), fetch(0));
    let p2 = step!(
        4,
        format!("page 2 (offset {page}, limit {page})"),
        fetch(page)
    );
    println!(
        "  page 1: {} row(s), page 2: {} row(s), {} column(s)",
        p1.rows.len(),
        p2.rows.len(),
        p1.columns.len()
    );
    if p1.rows.is_empty() || p2.rows.is_empty() {
        return Err("a page came back empty although the table has at least 2 rows".into());
    }
    let cols =
        |s: &dbstudio_core::ResultSet| s.columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>();
    if cols(&p1) != cols(&p2) {
        return Err(format!(
            "column lists differ: {:?} vs {:?}",
            cols(&p1),
            cols(&p2)
        ));
    }
    if p1.rows == p2.rows {
        return Err("page 2 returned the same rows as page 1".into());
    }

    // 5. plain queries through the normal execution path
    let qualified = format!("{}.{}", bracket(&schema), bracket(&table));
    let top = step!(
        5,
        "SELECT TOP (5)",
        select(session, db, &format!("SELECT TOP (5) * FROM {qualified}"))
    );
    println!("  rows: {}", top.len());
    if top.is_empty() {
        return Err("SELECT TOP (5) returned no rows".into());
    }
    let ver = step!(
        5,
        "SELECT @@VERSION",
        select(session, db, "SELECT @@VERSION")
    );
    let banner = ver
        .first()
        .and_then(|r| r.first())
        .map(to_text)
        .unwrap_or_default();
    println!("  @@VERSION: {}", banner.lines().next().unwrap_or(""));
    if banner.is_empty() {
        return Err("@@VERSION returned nothing".into());
    }

    // 6. DDL generation (text only)
    let mut texts: Vec<(String, String)> = Vec::new();
    let ddl = step!(
        6,
        "object_ddl of the table",
        session.object_ddl(&table_path)
    );
    texts.push((format!("table {qualified}"), ddl));
    let generated = step!(
        6,
        "generate_ddl of the table",
        session.generate_ddl(DdlRequest {
            scope: db_path.child(ObjectKind::Schema, schema.clone()),
            objects: vec![table_path.clone()],
            options: DdlOptions::default(),
        })
    );
    texts.push((
        format!("generate_ddl {qualified}"),
        generated
            .iter()
            .map(|i| i.sql.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
    ));
    for (label, kind, code) in [
        ("view", ObjectKind::View, "V"),
        ("procedure", ObjectKind::Procedure, "P"),
    ] {
        let rows = step!(
            6,
            format!("pick one {label} (SELECT on sys.objects)"),
            select(
                session,
                db,
                &format!(
                    "SELECT TOP (1) SCHEMA_NAME(schema_id), name FROM sys.objects \
                     WHERE type = '{code}' AND is_ms_shipped = 0 ORDER BY name"
                )
            )
        );
        match rows.first() {
            Some(r) => {
                let path = db_path
                    .child(ObjectKind::Schema, to_text(&r[0]))
                    .child(kind, to_text(&r[1]));
                let text = step!(
                    6,
                    format!("object_ddl of the {label}"),
                    session.object_ddl(&path)
                );
                texts.push((
                    format!("{label} {}.{}", to_text(&r[0]), to_text(&r[1])),
                    text,
                ));
            }
            None => println!("  no user {label} in this database; skipped"),
        }
    }
    for (label, text) in &texts {
        println!("  DDL {label}: {} chars", text.len());
        if text.trim().is_empty() {
            return Err(format!("DDL of {label} is empty"));
        }
        if major.is_some_and(|m| m < 13) {
            let lower = text.to_ascii_lowercase();
            for bad in ["create or alter", "if exists"] {
                if lower.contains(bad) {
                    return Err(format!(
                        "DDL of {label} contains `{bad}`, which SQL Server {} does not support \
                         (expected until the version-aware DDL work is merged)",
                        info.version
                    ));
                }
            }
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs MSSQL_CONNECTION (read-only smoke test)"]
async fn mssql_smoke_readonly() {
    let Some(text) = connection_string() else {
        return;
    };
    // Parse with the paste-import parser, so it is exercised for real.
    let parsed = ado(&text).unwrap_or_else(|e| panic!("MSSQL_CONNECTION does not parse: {e}"));
    let mut profile = parsed.profile;
    for note in &parsed.notes {
        println!("parser note: {note}");
    }
    let db = profile
        .config
        .database
        .clone()
        .unwrap_or_else(|| panic!("MSSQL_CONNECTION must name a Database / Initial Catalog"));
    profile.config.read_only = true;
    profile.config.connect_timeout_ms.get_or_insert(30_000);
    profile.save_password = false;
    println!(
        "target: {}:{} database={} user={:?} tls mode={:?} options={:?}",
        profile.config.host,
        profile.config.port,
        db,
        profile.config.username,
        profile.config.tls.mode,
        profile.config.options
    );
    if profile.config.tls.mode == TlsMode::Disable {
        println!("TlsMode::Disable: data is not encrypted, the driver still encrypts the login");
    }

    let (state, _secrets) = test_state(build_registry());
    let saved = connections::save_impl(&state, profile, SecretChange::Keep)
        .await
        .expect("save the in-memory profile");
    let password = parsed.password.map(Secret::new);

    // 1. connect
    let started = Instant::now();
    println!("[step 1] connect ...");
    let info = match sessions::open_impl(&state, saved.config.id, password).await {
        Ok(i) => i,
        Err(e) => panic!(
            "step 1 (connect) failed after {} ms:\n{e}\n{e:?}",
            started.elapsed().as_millis()
        ),
    };
    println!(
        "[step 1] ok in {} ms (session {})",
        started.elapsed().as_millis(),
        info.session_id
    );
    let session = state.sessions.get(info.session_id).expect("session");

    let outcome = run(&*session, &db).await;

    // 7. disconnect (also after a failure)
    let started = Instant::now();
    println!("[step 7] disconnect ...");
    match sessions::close_impl(&state, info.session_id).await {
        Ok(()) => println!("[step 7] ok in {} ms", started.elapsed().as_millis()),
        Err(e) => println!("[step 7] close failed: {e}"),
    }
    if let Err(msg) = outcome {
        panic!("{msg}");
    }
    println!("mssql_smoke: all steps passed");
}
