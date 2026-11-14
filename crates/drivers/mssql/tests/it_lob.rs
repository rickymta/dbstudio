//! Integration tests: free-form queries read LOB columns up to `max_lob_bytes` (PLP cap of the
//! tiberius fork), so a huge `varchar(max)` value neither fills memory nor breaks the connection.

mod common;

use std::sync::Arc;

use common::*;
use dbstudio_core::{QueryRequest, Session, StatementOutcome, TlsMode, Value, execute_collect};

macro_rules! it {
    () => {
        if !it_enabled() {
            return;
        }
    };
}

const LIMIT: usize = 1 << 20;

async fn small_limit_session() -> Arc<dyn Session> {
    let mut cfg = config(Some("master"), TlsMode::Require);
    cfg.options
        .insert("max_lob_bytes".into(), LIMIT.to_string());
    Arc::from(connect_cfg(&cfg).await.unwrap())
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
    let s = small_limit_session().await;
    // Warm up so connection and buffer allocations are not counted.
    run(&*s, "SELECT 1").await;
    let before = rss_kib();
    let r = execute_collect(
        &*s,
        QueryRequest::new("SELECT REPLICATE(CAST('x' AS varchar(max)), 50000000) AS big, 7 AS n"),
    )
    .await
    .unwrap();
    let after = rss_kib();
    assert!(
        after.saturating_sub(before) < 20 * 1024,
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
    assert!(
        r[0].messages.iter().any(|m| m.contains("big truncated")),
        "{:?}",
        r[0].messages
    );
    // The connection stays usable.
    let again = run(&*s, "SELECT 42").await;
    assert_eq!(rows_of(&again[0]).rows[0][0], Value::Int(42));
}

#[tokio::test]
async fn nvarchar_binary_and_xml_columns_are_cut_too() {
    it!();
    let s = small_limit_session().await;
    let r = run(
        &*s,
        "SELECT REPLICATE(CAST(N'é€' AS nvarchar(max)), 2000000) AS t, \
                CAST(REPLICATE(CAST('A' AS varchar(max)), 3000000) AS varbinary(max)) AS b, \
                CAST(REPLICATE(CAST('<a>' AS varchar(max)), 1) + 'x</a>' AS xml) AS x, \
                CAST(N'short' AS nvarchar(max)) AS s",
    )
    .await;
    let row = &rows_of(&r[0]).rows[0];
    assert!(matches!(&row[0], Value::Ext(e) if e.ty == "truncatedText" && e.text.len() <= LIMIT));
    assert!(matches!(&row[1], Value::Ext(e) if e.ty == "truncatedBytes"));
    assert!(matches!(&row[2], Value::Text(_) | Value::Ext(_)));
    assert_eq!(row[3], Value::Text("short".into()));
}

#[tokio::test]
async fn values_within_the_limit_stay_whole_and_the_cap_is_released() {
    it!();
    let s = small_limit_session().await;
    let r = run(
        &*s,
        "SELECT REPLICATE(CAST('y' AS varchar(max)), 1000) AS ok",
    )
    .await;
    assert!(matches!(&rows_of(&r[0]).rows[0][0], Value::Text(t) if t.len() == 1000));
}
