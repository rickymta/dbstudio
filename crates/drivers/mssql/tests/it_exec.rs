//! Integration tests: connection, query execution, multiple result sets, PRINT, `GO`, cancel.

mod common;

use std::time::{Duration, Instant};

use common::*;
use dbstudio_core::{
    CollectingSink, EndOutcome, Error, QueryRequest, ResultSink, StatementEnd, StatementOutcome,
    TlsMode, Value,
};

macro_rules! it {
    () => {
        if !it_enabled() {
            return;
        }
    };
}

#[tokio::test]
async fn smoke_select() {
    it!();
    let s = fixture().await;
    let r = run(&*s, "SELECT id, name FROM dbo.simple ORDER BY id").await;
    assert_eq!(r.len(), 1);
    let rs = rows_of(&r[0]);
    assert_eq!(rs.rows.len(), 3);
    assert_eq!(rs.rows[0], vec![Value::Int(1), Value::Text("one".into())]);
    assert_eq!(rs.rows[2][1], Value::Null);
    assert_eq!(rs.columns[0].type_name, "int");
    assert_eq!(rs.columns[1].type_name, "nvarchar");
}

#[tokio::test]
async fn three_statements_three_results() {
    it!();
    let s = fixture().await;
    let r = run(
        &*s,
        "CREATE TABLE #t (a INT); INSERT INTO #t VALUES (1),(2),(3); UPDATE #t SET a = a + 1; SELECT * FROM #t ORDER BY a; PRINT N'hello é'; DROP TABLE #t",
    )
    .await;
    for (i, x) in r.iter().enumerate() {
        eprintln!("{i}: {:?} msgs={:?}", x.outcome, x.messages);
    }
}

#[tokio::test]
async fn raw_tokens_debug() {
    it!();
    use futures_util::TryStreamExt;
    use tokio_util::compat::TokioAsyncWriteCompatExt;
    let mut cfg = tiberius::Config::new();
    cfg.host("127.0.0.1");
    cfg.port(port());
    cfg.authentication(tiberius::AuthMethod::sql_server("sa", PASSWORD));
    cfg.trust_cert();
    let tcp = tokio::net::TcpStream::connect(cfg.get_addr()).await.unwrap();
    let mut c = tiberius::Client::connect(cfg, tcp.compat_write()).await.unwrap();
    for sql in [
        "SELECT @@OPTIONS & 512 AS nocount",
        "CREATE TABLE #w (a INT); INSERT INTO #w VALUES (1),(2); UPDATE #w SET a = 5; DELETE FROM #w",
        "CREATE TABLE #t (a INT); INSERT INTO #t VALUES (1),(2),(3); UPDATE #t SET a = a + 1; SELECT * FROM #t ORDER BY a; PRINT N'hello'; DROP TABLE #t",
        "SELECT 1 AS x; SELECT 2 AS y",
        "PRINT 'x'",
        "CREATE TABLE #u (a INT); CREATE TABLE #v (a INT)",
    ] {
        eprintln!("--- {sql}");
        let mut st = c.simple_query(sql).await.unwrap();
        while let Some(i) = st.try_next().await.unwrap() {
            eprintln!("{i:?}");
        }
    }
}
