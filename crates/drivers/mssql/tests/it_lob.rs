//! Integration tests: LOB columns. Interactive reads (a row limit is set) are cut at
//! `max_lob_bytes` by the PLP cap of the tiberius fork, so a huge `varchar(max)` value neither
//! fills memory nor breaks the connection; export reads (`stream_table`, a query without row
//! limit) get every byte.

mod common;

use std::sync::Arc;

use common::*;
use dbstudio_core::{
    CollectingSink, LeaseOptions, QueryRequest, Session, StatementOutcome, TableStreamRequest,
    TlsMode, Value, execute_collect,
};

macro_rules! it {
    () => {
        if !it_enabled() {
            return;
        }
    };
}

const LIMIT: usize = 1 << 20;

async fn limited_session(db: &str) -> Arc<dyn Session> {
    let mut cfg = config(Some(db), TlsMode::Require);
    cfg.options
        .insert("max_lob_bytes".into(), LIMIT.to_string());
    Arc::from(connect_cfg(&cfg).await.unwrap())
}

/// A request with a row limit: what the editor sends.
fn capped(sql: &str) -> QueryRequest {
    let mut r = QueryRequest::new(sql);
    r.max_rows = Some(1000);
    r
}

async fn run_capped(s: &dyn Session, sql: &str) -> Vec<dbstudio_core::StatementResult> {
    execute_collect(s, capped(sql)).await.unwrap()
}

/// Resident set size of this process in KiB (`ps`, works on macOS and Linux).
fn rss_kib() -> u64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0)
}

#[tokio::test]
async fn huge_varchar_max_is_cut_without_filling_memory() {
    it!();
    let s = limited_session("master").await;
    // Warm up so connection and buffer allocations are not counted.
    run_capped(&*s, "SELECT 1").await;
    let before = rss_kib();
    let r = run_capped(
        &*s,
        "SELECT REPLICATE(CAST('x' AS varchar(max)), 50000000) AS big, 7 AS n",
    )
    .await;
    let after = rss_kib();
    // RSS is process-wide and other tests of this binary run in parallel, so the bound only has
    // to tell "capped" apart from "buffered the whole 50 MB value" (~50-100 MB).
    assert!(
        after.saturating_sub(before) < 40 * 1024,
        "RSS grew by {} KiB",
        after.saturating_sub(before)
    );
    let StatementOutcome::Rows(rs) = &r[0].outcome else {
        panic!("expected rows");
    };
    match &rs.rows[0][0] {
        Value::Ext(e) => {
            assert_eq!(e.ty, "truncatedText");
            assert_eq!(e.text.len(), LIMIT);
        }
        other => panic!("expected a truncated cell, got {other:?}"),
    }
    assert_eq!(rs.rows[0][1], Value::Int(7));
    assert_eq!(
        r[0].messages,
        vec![format!("column big: 1 value(s) truncated at {LIMIT} bytes")]
    );
    // The connection stays usable.
    let again = run_capped(&*s, "SELECT 42").await;
    assert_eq!(rows_of(&again[0]).rows[0][0], Value::Int(42));
}

#[tokio::test]
async fn nvarchar_binary_and_xml_columns_are_cut_too() {
    it!();
    let s = limited_session("master").await;
    let r = run_capped(
        &*s,
        "SELECT REPLICATE(CAST(N'é€' AS nvarchar(max)), 2000000) AS t, \
                CAST(REPLICATE(CAST('A' AS varchar(max)), 3000000) AS varbinary(max)) AS b, \
                CAST('<r>' + REPLICATE(CAST('<i>0123456789</i>' AS varchar(max)), 200000) + '</r>' AS xml) AS x, \
                CAST(N'short' AS nvarchar(max)) AS s",
    )
    .await;
    let row = &rows_of(&r[0]).rows[0];
    assert!(matches!(&row[0], Value::Ext(e) if e.ty == "truncatedText" && e.text.len() <= LIMIT));
    assert!(matches!(&row[1], Value::Ext(e) if e.ty == "truncatedBytes"));
    assert!(
        matches!(&row[2], Value::Ext(e) if e.ty == "truncatedText" && e.text.len() <= LIMIT),
        "xml cell: {:?}",
        row[2]
    );
    assert_eq!(row[3], Value::Text("short".into()));
    // One message per column, not per cell.
    assert_eq!(r[0].messages.len(), 3, "{:?}", r[0].messages);
}

#[tokio::test]
async fn values_within_the_limit_stay_whole() {
    it!();
    let s = limited_session("master").await;
    let r = run_capped(
        &*s,
        "SELECT REPLICATE(CAST('y' AS varchar(max)), 1000) AS ok",
    )
    .await;
    assert!(matches!(&rows_of(&r[0]).rows[0][0], Value::Text(t) if t.len() == 1000));
    assert!(r[0].messages.is_empty());
}

/// Create a table with a 20 MB `varbinary(max)` and a 5 M-character `nvarchar(max)` cell.
async fn prepare_lob_table(s: &dyn Session, name: &str) {
    let table = format!("dbo.{name}");
    run(
        s,
        &format!(
            "DROP TABLE IF EXISTS {table};\nGO\n\
             CREATE TABLE {table} (id INT PRIMARY KEY, b VARBINARY(max), t NVARCHAR(max));\n\
             INSERT INTO {table} VALUES (1, \
               CAST(REPLICATE(CAST('0123456789abcdef' AS varchar(max)), 1250000) AS varbinary(max)), \
               REPLICATE(CAST(N'é€😀x' AS nvarchar(max)), 500000)), (2, NULL, N'small');"
        ),
    )
    .await;
}

fn expected_bytes() -> Vec<u8> {
    b"0123456789abcdef".repeat(1_250_000)
}

fn expected_text() -> String {
    "é€😀x".repeat(500_000)
}

fn assert_full(rows: &[Vec<Value>]) {
    assert_eq!(rows.len(), 2);
    // Column order of the table: id, b, t.
    assert!(
        matches!(&rows[0][1], Value::Bytes(b) if *b == expected_bytes()),
        "varbinary(max) cell of 20 MB must arrive whole"
    );
    assert!(
        matches!(&rows[0][2], Value::Text(t) if *t == expected_text()),
        "nvarchar(max) cell of 5 MB chars must arrive whole"
    );
    assert_eq!(rows[1][1], Value::Null);
    assert_eq!(rows[1][2], Value::Text("small".into()));
}

#[tokio::test]
async fn stream_table_returns_whole_values_despite_a_small_limit() {
    it!();
    ensure_fixture().await;
    let s = limited_session(WORK_DB).await;
    prepare_lob_table(&*s, "lob_stream_t").await;
    let l = s.open_lease(LeaseOptions::default()).await.unwrap();
    let mut sink = CollectingSink::new();
    l.stream_table(
        TableStreamRequest {
            query_id: uuid::Uuid::new_v4(),
            table: dbstudio_core::ObjectPath::root()
                .child(dbstudio_core::ObjectKind::Database, WORK_DB)
                .child(
                    dbstudio_core::ObjectKind::Folder,
                    dbstudio_core::folder::SCHEMAS,
                )
                .child(dbstudio_core::ObjectKind::Schema, "dbo")
                .child(
                    dbstudio_core::ObjectKind::Folder,
                    dbstudio_core::folder::TABLES,
                )
                .child(dbstudio_core::ObjectKind::Table, "lob_stream_t"),
            columns: vec![],
            filters: vec![],
            raw_where: None,
            order_by_key: true,
            batch_size: Some(1),
        },
        &mut sink,
    )
    .await
    .unwrap();
    let res = sink.into_results();
    assert_full(&rows_of(&res[0]).rows);
    assert!(res[0].messages.is_empty(), "{:?}", res[0].messages);
    l.close().await.unwrap();
}

#[tokio::test]
async fn cap_is_released_on_the_same_connection() {
    it!();
    ensure_fixture().await;
    let s = limited_session(WORK_DB).await;
    prepare_lob_table(&*s, "lob_cap_t").await;
    let l = s.open_lease(LeaseOptions::default()).await.unwrap();
    let sql = "SELECT id, b, t FROM dbo.lob_cap_t ORDER BY id".to_string();
    // An interactive read on the lease's connection is cut ...
    let mut sink = CollectingSink::new();
    l.execute(capped(&sql), &mut sink).await.unwrap();
    let res = sink.into_results();
    let rs = rows_of(&res[0]);
    assert!(matches!(&rs.rows[0][1], Value::Ext(e) if e.ty == "truncatedBytes"));
    assert!(matches!(&rs.rows[0][2], Value::Ext(e) if e.ty == "truncatedText"));
    // ... and the next export-style read (no row limit) on the same connection is whole.
    let mut sink = CollectingSink::new();
    l.execute(QueryRequest::new(&sql), &mut sink).await.unwrap();
    assert_full(&rows_of(&sink.into_results()[0]).rows);
    l.close().await.unwrap();
}
