//! Origin of result columns (`ColumnMeta::origin`) from the protocol's column definitions.
//! Run with `DBSTUDIO_IT=1`.

mod common;

use common::*;
use dbstudio_core::ColumnMeta;

fn pair(c: &ColumnMeta) -> Option<(String, String, String)> {
    c.origin.as_ref().map(|o| {
        (
            o.database.clone().unwrap_or_default(),
            o.table.clone(),
            o.column.clone(),
        )
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn origins_of_aliases_expressions_and_joins() {
    if !it_enabled() || !selected(MYSQL8) {
        return;
    }
    ensure_fixture(MYSQL8).await;
    let db = scratch_db(MYSQL8, "origin").await;
    let s = user_session(MYSQL8).await;
    run(
        &*s,
        &format!(
            "CREATE TABLE `{db}`.pa (id INT PRIMARY KEY, name VARCHAR(20), price DECIMAL(8,2)); \
             CREATE TABLE `{db}`.pb (id INT PRIMARY KEY, a_id INT, note VARCHAR(20)); \
             INSERT INTO `{db}`.pa VALUES (1,'x',1.5); INSERT INTO `{db}`.pb VALUES (1,1,'n'); \
             CREATE VIEW `{db}`.pv AS SELECT id, name FROM `{db}`.pa"
        ),
    )
    .await;
    let want = |t: &str, c: &str| Some((db.to_string(), t.to_string(), c.to_string()));

    // Table and column aliases: the origin is still the base column.
    let rs = query(
        &*s,
        &format!("SELECT p.id, p.name AS label, price FROM `{db}`.pa p"),
    )
    .await;
    assert_eq!(pair(&rs.columns[0]), want("pa", "id"));
    assert_eq!(rs.columns[1].name, "label");
    assert_eq!(pair(&rs.columns[1]), want("pa", "name"));
    assert_eq!(pair(&rs.columns[2]), want("pa", "price"));

    // Expressions, functions and constants have none.
    let rs = query(
        &*s,
        &format!("SELECT id, price * 2 AS twice, UPPER(name), 1 AS one FROM `{db}`.pa"),
    )
    .await;
    assert_eq!(pair(&rs.columns[0]), want("pa", "id"));
    assert!(rs.columns[1..].iter().all(|c| c.origin.is_none()));

    // A JOIN: each column keeps its own table.
    let rs = query(
        &*s,
        &format!("SELECT a.id, a.name, b.note FROM `{db}`.pa a JOIN `{db}`.pb b ON b.a_id = a.id"),
    )
    .await;
    assert_eq!(pair(&rs.columns[1]), want("pa", "name"));
    assert_eq!(pair(&rs.columns[2]), want("pb", "note"));

    // A derived table hides the base table.
    let rs = query(
        &*s,
        &format!("SELECT x.id FROM (SELECT id FROM `{db}`.pa) x"),
    )
    .await;
    assert!(rs.columns[0].origin.is_none() || pair(&rs.columns[0]) == want("pa", "id"));
}
