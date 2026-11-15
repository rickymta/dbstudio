//! Origin of result columns (`ColumnMeta::origin`). Only runs with `DBSTUDIO_IT=1`.

mod common;

use common::*;
use dbstudio_core::{ColumnMeta, StatementOutcome};

async fn columns_of(s: &dyn dbstudio_core::Session, sql: &str) -> Vec<ColumnMeta> {
    match exec(s, sql).await.remove(0).outcome {
        StatementOutcome::Rows(rs) => rs.columns,
        other => panic!("not rows: {other:?}"),
    }
}

fn pair(c: &ColumnMeta) -> Option<(String, String, String)> {
    c.origin.as_ref().map(|o| {
        (
            o.schema.clone().unwrap_or_default(),
            o.table.clone(),
            o.column.clone(),
        )
    })
}

async fn origins(port: u16) {
    let s = fixture(port).await;
    exec(
        &*s,
        "CREATE SCHEMA IF NOT EXISTS dbs_origin; \
         DROP TABLE IF EXISTS dbs_origin.pa CASCADE; DROP TABLE IF EXISTS dbs_origin.pb CASCADE; \
         CREATE TABLE dbs_origin.pa (id int PRIMARY KEY, name text, price numeric); \
         CREATE TABLE dbs_origin.pb (id int PRIMARY KEY, a_id int, note text); \
         INSERT INTO dbs_origin.pa VALUES (1,'x',1.5); INSERT INTO dbs_origin.pb VALUES (1,1,'n')",
    )
    .await;
    let want = |t: &str, c: &str| Some(("dbs_origin".to_string(), t.to_string(), c.to_string()));

    // Plain columns, with table and column aliases: the origin is the base column.
    let cols = columns_of(
        &*s,
        "SELECT p.id, p.name AS label, price FROM dbs_origin.pa p",
    )
    .await;
    assert_eq!(pair(&cols[0]), want("pa", "id"));
    assert_eq!(cols[1].name, "label");
    assert_eq!(pair(&cols[1]), want("pa", "name"));
    assert_eq!(pair(&cols[2]), want("pa", "price"));
    assert_eq!(
        cols[0].origin.as_ref().unwrap().database.as_deref(),
        Some("dbstudio")
    );

    // Expressions, functions and constants have none.
    let cols = columns_of(
        &*s,
        "SELECT id, price * 2 AS twice, upper(name), 1 AS one FROM dbs_origin.pa",
    )
    .await;
    assert_eq!(pair(&cols[0]), want("pa", "id"));
    assert!(cols[1].origin.is_none() && cols[2].origin.is_none() && cols[3].origin.is_none());

    // A JOIN: each column keeps its own table (the editor refuses the mix, not the driver).
    let cols = columns_of(
        &*s,
        "SELECT a.id, a.name, b.note FROM dbs_origin.pa a JOIN dbs_origin.pb b ON b.a_id = a.id",
    )
    .await;
    assert_eq!(pair(&cols[1]), want("pa", "name"));
    assert_eq!(pair(&cols[2]), want("pb", "note"));

    // A view column is reported against the view, not the table behind it.
    exec(
        &*s,
        "CREATE OR REPLACE VIEW dbs_origin.pv AS SELECT id, name FROM dbs_origin.pa",
    )
    .await;
    let cols = columns_of(&*s, "SELECT id FROM dbs_origin.pv").await;
    assert_eq!(pair(&cols[0]), want("pv", "id"));

    // A subquery in FROM loses nothing the server knows, but a computed column has none.
    let cols = columns_of(
        &*s,
        "SELECT x.id, x.n FROM (SELECT id, length(name) AS n FROM dbs_origin.pa) x",
    )
    .await;
    assert!(cols[1].origin.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn origin_pg16() {
    if enabled() {
        origins(PG16).await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn origin_pg12() {
    if enabled() {
        origins(PG12).await
    }
}
