//! Data access: value decoding, filters, streaming, execute, cancel, concurrency.
//! Run with `DBSTUDIO_IT=1`.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use dbstudio_core::{
    ColumnFilter as CF, Error, FilterOp as Op, ObjectKind as K, ObjectPath, QueryRequest, Session,
    SortKey, StatementOutcome, TableCountRequest, TlsMode, Value, ValueKind, execute_collect,
};
use uuid::Uuid;

fn f(col: &str, op: Op, vals: &[&str]) -> CF {
    CF {
        column: col.into(),
        op,
        values: vals.iter().map(|s| s.to_string()).collect(),
    }
}

fn people_ids(rs: &dbstudio_core::ResultSet) -> Vec<i64> {
    let mut v = ints(rs, 0);
    v.sort();
    v
}

#[tokio::test]
async fn value_decoding_is_lossless() {
    if !it_enabled() {
        return;
    }
    for t in targets() {
        ensure_fixture(t).await;
        let s = user_session(t).await;
        let tag = t.name;
        let mut req = table_req(FIXTURE_DB, "all_types");
        req.sort = vec![SortKey {
            column: "id".into(),
            descending: false,
        }];
        let rs = fetch(&*s, req).await.unwrap();
        assert_eq!(rs.rows.len(), 3);
        let idx = |n: &str| rs.columns.iter().position(|c| c.name == n).unwrap();
        let cell = |row: usize, n: &str| rs.rows[row][idx(n)].clone();
        let kind = |n: &str| rs.columns[idx(n)].value_kind;
        let tname = |n: &str| rs.columns[idx(n)].type_name.clone();

        // Row 1: extreme values.
        assert_eq!(cell(0, "c_tinyint"), Value::Int(-128));
        assert_eq!(cell(0, "c_smallint"), Value::Int(-32768));
        assert_eq!(cell(0, "c_mediumint"), Value::Int(-8388608));
        assert_eq!(cell(0, "c_int"), Value::Int(-2147483648));
        assert_eq!(cell(0, "c_bigint"), Value::Int(i64::MIN));
        assert_eq!(cell(0, "u_tinyint"), Value::UInt(255));
        assert_eq!(cell(0, "u_smallint"), Value::UInt(65535));
        assert_eq!(cell(0, "u_mediumint"), Value::UInt(16777215));
        assert_eq!(cell(0, "u_int"), Value::UInt(4294967295));
        assert_eq!(
            cell(0, "u_bigint"),
            Value::UInt(18446744073709551615),
            "[{tag}]"
        );
        assert_eq!(
            cell(0, "c_decimal"),
            Value::Decimal("-12345678901234567890123456.1234567890".into())
        );
        assert_eq!(cell(0, "c_float"), Value::Float(1.5));
        assert_eq!(cell(0, "c_double"), Value::Float(1.2345678901234567));
        assert_eq!(cell(0, "c_date"), Value::Date("2024-02-29".into()));
        assert_eq!(
            cell(0, "c_datetime"),
            Value::DateTime("2024-02-29T13:45:01.123456".into())
        );
        assert_eq!(
            cell(0, "c_timestamp"),
            Value::DateTime("2024-02-29T13:45:01".into())
        );
        assert_eq!(cell(0, "c_time"), Value::Time("-838:59:59.000".into()));
        assert_eq!(cell(0, "c_year"), Value::Int(2024));
        assert_eq!(cell(0, "c_char"), Value::Text("ab".into()));
        assert_eq!(cell(0, "c_varchar"), Value::Text("héllo wörld 日本".into()));
        assert_eq!(cell(0, "c_text"), Value::Text("long\ntext".into()));
        assert_eq!(cell(0, "c_binary"), Value::Bytes(vec![b'a', b'b', 0, 0]));
        assert_eq!(cell(0, "c_varbinary"), Value::Bytes(vec![0, 0xff, 0x10]));
        assert_eq!(
            cell(0, "c_blob"),
            Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef])
        );
        let expected_json = serde_json::json!({"a": [1, 2, {"b": null}]});
        match cell(0, "c_json") {
            Value::Json(txt) => {
                assert!(!t.mariadb, "MariaDB json is longtext");
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&txt).unwrap(),
                    expected_json
                );
            }
            Value::Text(txt) => {
                assert!(t.mariadb, "MySQL json must decode to Json");
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&txt).unwrap(),
                    expected_json
                );
            }
            other => panic!("[{tag}] json decoded as {other:?}"),
        }
        assert_eq!(cell(0, "c_enum"), Value::Text("b".into()));
        assert_eq!(cell(0, "c_set"), Value::Text("x,z".into()));
        assert_eq!(cell(0, "c_bit"), Value::UInt(0b1010_1010_1010));
        match cell(0, "c_geom") {
            Value::Bytes(b) => assert_eq!(b.len(), 25, "SRID + WKB point"),
            other => panic!("[{tag}] geometry decoded as {other:?}"),
        }
        // Zero datetime does not break the row.
        assert_eq!(
            cell(0, "c_zero_dt"),
            Value::Text("0000-00-00 00:00:00".into())
        );

        // Row 2: all NULL.
        for c in &rs.columns {
            if c.name != "id" {
                assert_eq!(rs.rows[1][idx(&c.name)], Value::Null, "[{tag}] {}", c.name);
            }
        }
        // Row 3: zero date, long time, tiny decimal.
        assert_eq!(cell(2, "c_date"), Value::Text("0000-00-00".into()));
        assert_eq!(cell(2, "c_time"), Value::Time("100:00:00.000".into()));
        assert_eq!(cell(2, "c_decimal"), Value::Decimal("0.0000000001".into()));
        assert_eq!(cell(2, "u_bigint"), Value::UInt(0));

        // Column metadata.
        assert_eq!(kind("c_bigint"), ValueKind::Int);
        assert_eq!(kind("u_bigint"), ValueKind::UInt);
        assert_eq!(kind("c_decimal"), ValueKind::Decimal);
        assert_eq!(kind("c_datetime"), ValueKind::DateTime);
        assert_eq!(kind("c_timestamp"), ValueKind::DateTime);
        assert_eq!(kind("c_time"), ValueKind::Time);
        assert_eq!(kind("c_blob"), ValueKind::Bytes);
        assert_eq!(kind("c_varchar"), ValueKind::Text);
        assert_eq!(kind("c_bit"), ValueKind::UInt);
        assert_eq!(tname("u_bigint"), "bigint unsigned");
        assert_eq!(tname("c_decimal"), "decimal");
        assert_eq!(tname("c_blob"), "blob");
        assert_eq!(tname("c_varchar"), "varchar");
        assert_eq!(rs.columns[idx("id")].nullable, Some(false));
        assert_eq!(rs.columns[idx("c_int")].nullable, Some(true));

        // The same decoding applies to ad-hoc queries.
        let q = query(&*s, "SELECT 18446744073709551615 AS big, -1 AS neg, 1.10 AS d, 1e0 AS f, 'x' AS s, NULL AS n, 0x00ff AS b").await;
        assert_eq!(q.rows[0][0], Value::UInt(u64::MAX));
        assert_eq!(q.rows[0][1], Value::Int(-1));
        assert_eq!(q.rows[0][2], Value::Decimal("1.10".into()));
        assert_eq!(q.rows[0][3], Value::Float(1.0));
        assert_eq!(q.rows[0][4], Value::Text("x".into()));
        assert_eq!(q.rows[0][5], Value::Null);
        assert_eq!(q.rows[0][6], Value::Bytes(vec![0, 0xff]));
        // Invalid UTF-8 text must not panic.
        let q = query(&*s, "SELECT CONVERT(UNHEX('61FF62') USING latin1) AS l, CAST(UNHEX('61FF62') AS CHAR CHARACTER SET utf8mb4) AS bad").await;
        assert!(matches!(q.rows[0][0], Value::Text(_)));
        assert!(matches!(
            q.rows[0][1],
            Value::Text(_) | Value::Bytes(_) | Value::Null
        ));
    }
}

#[tokio::test]
async fn filters_sort_paging_raw_where() {
    if !it_enabled() {
        return;
    }
    for t in targets() {
        ensure_fixture(t).await;
        let s = user_session(t).await;
        let tag = t.name;
        let ids = |filters: Vec<CF>| {
            let s = s.clone();
            async move {
                let mut r = table_req(FIXTURE_DB, "people");
                r.filters = filters;
                let rs = fetch(&*s, r).await.unwrap();
                people_ids(&rs)
            }
        };
        assert_eq!(ids(vec![]).await, vec![1, 2, 3, 4, 5, 6, 7], "[{tag}]");
        assert_eq!(ids(vec![f("name", Op::Eq, &["Alice"])]).await, vec![1]);
        assert_eq!(ids(vec![f("age", Op::Eq, &["25"])]).await, vec![2, 4]);
        assert_eq!(ids(vec![f("age", Op::Ne, &["25"])]).await, vec![1, 3, 5, 7]);
        assert_eq!(ids(vec![f("age", Op::Lt, &["30"])]).await, vec![2, 4, 7]);
        assert_eq!(ids(vec![f("age", Op::Le, &["30"])]).await, vec![1, 2, 4, 7]);
        assert_eq!(ids(vec![f("age", Op::Gt, &["35"])]).await, vec![5]);
        assert_eq!(ids(vec![f("age", Op::Ge, &["35"])]).await, vec![3, 5]);
        assert_eq!(
            ids(vec![f("score", Op::Gt, &["40.5"])]).await,
            vec![5, 6, 7]
        );
        // LIKE wildcards are matched literally.
        assert_eq!(ids(vec![f("note", Op::Contains, &["a%b"])]).await, vec![2]);
        assert_eq!(ids(vec![f("note", Op::Contains, &["a_b"])]).await, vec![3]);
        assert_eq!(ids(vec![f("note", Op::Contains, &["%"])]).await, vec![2]);
        assert_eq!(ids(vec![f("note", Op::Contains, &["_"])]).await, vec![3]);
        assert_eq!(
            ids(vec![f("note", Op::Contains, &["\\"])]).await,
            vec![4],
            "[{tag}]"
        );
        assert_eq!(ids(vec![f("note", Op::Contains, &["'"])]).await, vec![5]);
        assert_eq!(
            ids(vec![f("name", Op::StartsWith, &["Al"])]).await,
            vec![1, 7]
        );
        assert_eq!(ids(vec![f("note", Op::StartsWith, &["a_"])]).await, vec![3]);
        assert_eq!(ids(vec![f("note", Op::EndsWith, &["b"])]).await, vec![2, 3]);
        assert_eq!(
            ids(vec![f("name", Op::NotContains, &["a"])]).await,
            vec![2, 5]
        );
        // Contains works on the text form of numeric columns.
        assert_eq!(ids(vec![f("score", Op::Contains, &[".25"])]).await, vec![3]);
        assert_eq!(
            ids(vec![f("note", Op::IsNull, &["ignored"])]).await,
            vec![1]
        );
        assert_eq!(
            ids(vec![f("note", Op::IsNotNull, &[])]).await,
            vec![2, 3, 4, 5, 6, 7]
        );
        assert_eq!(ids(vec![f("age", Op::IsNull, &[])]).await, vec![6]);
        assert_eq!(
            ids(vec![f("id", Op::In, &["1", "3", "99"])]).await,
            vec![1, 3]
        );
        assert_eq!(
            ids(vec![f("note", Op::In, &["it's", "x"])]).await,
            vec![5, 6]
        );
        assert_eq!(ids(vec![f("id", Op::In, &[])]).await, Vec::<i64>::new());
        // AND combination.
        assert_eq!(
            ids(vec![
                f("age", Op::Eq, &["25"]),
                f("name", Op::Contains, &["ob"])
            ])
            .await,
            vec![2]
        );

        // Hostile values are plain data.
        for evil in [
            "'; DROP TABLE people; --",
            "\\'; DROP TABLE people; --",
            "x' OR '1'='1",
            "` OR 1=1 --",
        ] {
            assert_eq!(
                ids(vec![f("name", Op::Eq, &[evil])]).await,
                Vec::<i64>::new(),
                "[{tag}] {evil}"
            );
            assert_eq!(
                ids(vec![f("name", Op::Contains, &[evil])]).await,
                Vec::<i64>::new()
            );
            assert_eq!(ids(vec![f("name", Op::In, &[evil, "Bob"])]).await, vec![2]);
        }
        // Hostile column names are plain identifiers (unknown column -> query error).
        let mut r = table_req(FIXTURE_DB, "people");
        r.filters = vec![f("id` = 1 OR `1", Op::Eq, &["1"])];
        assert!(matches!(fetch(&*s, r).await, Err(Error::Query { .. })));
        assert_eq!(ids(vec![]).await.len(), 7, "table still intact");

        // Sorting and paging.
        let mut r = table_req(FIXTURE_DB, "people");
        r.sort = vec![
            SortKey {
                column: "age".into(),
                descending: false,
            },
            SortKey {
                column: "name".into(),
                descending: true,
            },
        ];
        let rs = fetch(&*s, r.clone()).await.unwrap();
        assert_eq!(
            ints(&rs, 0),
            vec![6, 7, 4, 2, 1, 3, 5],
            "[{tag}] NULL age sorts first, ties by name desc"
        );
        r.offset = 2;
        r.limit = 3;
        assert_eq!(
            ints(&fetch(&*s, r.clone()).await.unwrap(), 0),
            vec![4, 2, 1]
        );
        r.offset = 100;
        assert!(fetch(&*s, r.clone()).await.unwrap().rows.is_empty());
        r.offset = 0;
        r.limit = 0;
        assert!(fetch(&*s, r).await.unwrap().rows.is_empty());

        // raw_where is appended verbatim and combined with AND.
        let mut r = table_req(FIXTURE_DB, "people");
        r.raw_where = Some("age > 30 OR name = 'Bob'".into());
        assert_eq!(
            people_ids(&fetch(&*s, r.clone()).await.unwrap()),
            vec![2, 3, 5]
        );
        r.filters = vec![f("score", Op::Lt, &["45"])];
        assert_eq!(
            people_ids(&fetch(&*s, r.clone()).await.unwrap()),
            vec![2, 3]
        );
        r.raw_where = Some("id = 1 -- trailing comment".into());
        r.filters = vec![];
        assert_eq!(people_ids(&fetch(&*s, r.clone()).await.unwrap()), vec![1]);
        r.raw_where = Some("   ".into());
        assert_eq!(fetch(&*s, r.clone()).await.unwrap().rows.len(), 7);
        r.raw_where = Some("id = no_such_col".into());
        assert!(matches!(fetch(&*s, r).await, Err(Error::Query { .. })));

        // count_table uses the same conditions.
        let count = |filters: Vec<CF>, raw: Option<&str>| {
            let s = s.clone();
            let raw = raw.map(String::from);
            async move {
                s.count_table(TableCountRequest {
                    query_id: None,
                    table: ObjectPath::root()
                        .child(K::Database, FIXTURE_DB)
                        .child(K::Table, "people"),
                    filters,
                    raw_where: raw,
                })
                .await
                .unwrap()
            }
        };
        assert_eq!(count(vec![], None).await, 7);
        assert_eq!(count(vec![f("age", Op::Eq, &["25"])], None).await, 2);
        assert_eq!(
            count(vec![f("note", Op::Contains, &["a%b"])], None).await,
            1
        );
        assert_eq!(
            count(vec![f("age", Op::Gt, &["20"])], Some("name <> 'Bob'")).await,
            5
        );
        // A view can be browsed too.
        let mut r = table_req(FIXTURE_DB, "v_child");
        r.table = ObjectPath::root()
            .child(K::Database, FIXTURE_DB)
            .child(K::View, "v_child");
        assert!(fetch(&*s, r).await.unwrap().rows.is_empty());
        // Weird identifiers.
        let mut r = table_req(FIXTURE_DB, "We`ird Tbl");
        r.filters = vec![f("col`tick", Op::Eq, &["c"])];
        r.sort = vec![SortKey {
            column: "Mixed Case".into(),
            descending: true,
        }];
        let rs = fetch(&*s, r).await.unwrap();
        assert_eq!(
            rs.rows,
            vec![vec![
                Value::Int(2),
                Value::Text("c".into()),
                Value::Text("d".into())
            ]]
        );
        assert_eq!(rs.columns[1].name, "col`tick");
        let r = table_req(FIXTURE_DB, "MixedCase");
        assert!(fetch(&*s, r).await.unwrap().rows.is_empty());
        // Missing table.
        assert!(matches!(
            fetch(&*s, table_req(FIXTURE_DB, "nope")).await,
            Err(Error::Query { .. })
        ));
    }
}

#[tokio::test]
async fn no_backslash_escapes_mode() {
    if !it_enabled() {
        return;
    }
    for t in targets() {
        ensure_fixture(t).await;
        let mut c = config(t, "dbstudio", TlsMode::Disable);
        c.options.insert(
            "init_command".into(),
            "SET SESSION sql_mode = CONCAT(@@sql_mode, ',NO_BACKSLASH_ESCAPES')".into(),
        );
        let s = connect_cfg(&c).await.unwrap();
        let mode = query(&*s, "SELECT @@sql_mode").await;
        assert!(matches!(&mode.rows[0][0], Value::Text(m) if m.contains("NO_BACKSLASH_ESCAPES")));
        let ids = |filters: Vec<CF>| {
            let s = &s;
            async move {
                let mut r = table_req(FIXTURE_DB, "people");
                r.filters = filters;
                people_ids(&fetch(&**s, r).await.unwrap())
            }
        };
        assert_eq!(
            ids(vec![f("note", Op::Eq, &["back\\slash"])]).await,
            vec![4],
            "[{}]",
            t.name
        );
        assert_eq!(ids(vec![f("note", Op::Contains, &["\\"])]).await, vec![4]);
        assert_eq!(ids(vec![f("note", Op::Contains, &["a%b"])]).await, vec![2]);
        assert_eq!(
            ids(vec![f("note", Op::Eq, &["\\' OR 1=1 -- "])]).await,
            Vec::<i64>::new()
        );
        let n = s
            .count_table(TableCountRequest {
                query_id: None,
                table: ObjectPath::root()
                    .child(K::Database, FIXTURE_DB)
                    .child(K::Table, "people"),
                filters: vec![f("note", Op::Eq, &["back\\slash"])],
                raw_where: None,
            })
            .await
            .unwrap();
        assert_eq!(n, 1);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn streaming_big_table() {
    if !it_enabled() {
        return;
    }
    for t in targets() {
        ensure_fixture(t).await;
        let s = user_session(t).await;
        let tag = t.name;
        let mut req = table_req(FIXTURE_DB, "big");
        req.limit = 1_000_000;
        req.batch_size = Some(1000);
        let mut sink = CountingSink::default();
        let started = Instant::now();
        let sum = s.fetch_table(req.clone(), &mut sink).await.unwrap();
        assert_eq!(sink.rows, 200_000, "[{tag}]");
        assert_eq!(sink.batches, 200, "[{tag}] 200k rows in batches of 1000");
        assert_eq!((sink.columns, sink.ends, sum.statements), (3, 1, 1));
        eprintln!("[{tag}] streamed 200k rows in {:?}", started.elapsed());

        // Default batch size is around 1000 as well.
        req.batch_size = None;
        let mut sink = CountingSink::default();
        s.fetch_table(req.clone(), &mut sink).await.unwrap();
        assert_eq!(sink.rows, 200_000);
        assert!((100..=400).contains(&sink.batches), "{}", sink.batches);

        // Uneven batches: the last one is partial.
        req.batch_size = Some(75_000);
        let mut sink = CountingSink::default();
        s.fetch_table(req.clone(), &mut sink).await.unwrap();
        assert_eq!((sink.batches, sink.rows), (3, 200_000));

        // Paging deep into the table, with sort and filter.
        let mut r = table_req(FIXTURE_DB, "big");
        r.offset = 199_990;
        r.limit = 100;
        r.sort = vec![SortKey {
            column: "id".into(),
            descending: false,
        }];
        let rs = fetch(&*s, r).await.unwrap();
        assert_eq!(ints(&rs, 0), (199_991..=200_000).collect::<Vec<i64>>());
        let mut r = table_req(FIXTURE_DB, "big");
        r.sort = vec![SortKey {
            column: "id".into(),
            descending: true,
        }];
        r.limit = 3;
        assert_eq!(
            ints(&fetch(&*s, r).await.unwrap(), 0),
            vec![200_000, 199_999, 199_998]
        );
        let mut r = table_req(FIXTURE_DB, "big");
        r.filters = vec![f("grp", Op::Eq, &["7"])];
        r.limit = 100_000;
        assert_eq!(fetch(&*s, r).await.unwrap().rows.len(), 2000);
        let n = s
            .count_table(TableCountRequest {
                query_id: None,
                table: ObjectPath::root()
                    .child(K::Database, FIXTURE_DB)
                    .child(K::Table, "big"),
                filters: vec![f("payload", Op::StartsWith, &["row-19"])],
                raw_where: None,
            })
            .await
            .unwrap();
        // row-19, row-190..199, row-1900..1999, row-19000..19999, row-190000..199999
        assert_eq!(n, 1 + 10 + 100 + 1000 + 10000, "[{tag}]");

        // Same through execute with max_rows.
        let mut q = QueryRequest::new(format!("SELECT * FROM `{FIXTURE_DB}`.big"));
        q.max_rows = Some(5000);
        q.batch_size = Some(2000);
        let mut sink = CountingSink::default();
        s.execute(q, &mut sink).await.unwrap();
        assert_eq!((sink.rows, sink.batches), (5000, 3));
        let mut q = QueryRequest::new(format!("SELECT * FROM `{FIXTURE_DB}`.big LIMIT 10"));
        q.max_rows = Some(10);
        let r = execute_collect(&*s, q).await.unwrap();
        match &r[0].outcome {
            StatementOutcome::Rows(rs) => assert!(!rs.truncated && rs.rows.len() == 10),
            o => panic!("{o:?}"),
        }
        let mut q = QueryRequest::new(format!("SELECT * FROM `{FIXTURE_DB}`.big LIMIT 11"));
        q.max_rows = Some(10);
        match &execute_collect(&*s, q).await.unwrap()[0].outcome {
            StatementOutcome::Rows(rs) => assert!(rs.truncated && rs.rows.len() == 10),
            o => panic!("{o:?}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sink_stop_keeps_session_usable() {
    if !it_enabled() {
        return;
    }
    for t in targets() {
        ensure_fixture(t).await;
        // A single pooled connection proves it is not returned half-read.
        let mut c = config(t, "dbstudio", TlsMode::Disable);
        c.options.insert("pool_max".into(), "1".into());
        let s: Arc<dyn Session> = Arc::from(connect_cfg(&c).await.unwrap());
        let tag = t.name;
        let scratch = scratch_db(t, "sinkstop").await;
        run(&*s, &format!("CREATE TABLE `{scratch}`.t (a INT)")).await;
        for _ in 0..3 {
            let mut req = table_req(FIXTURE_DB, "big");
            req.limit = 1_000_000;
            req.batch_size = Some(500);
            let mut sink = CountingSink {
                fail_after: Some((2, None)),
                ..Default::default()
            };
            let started = Instant::now();
            let r = s.fetch_table(req, &mut sink).await;
            assert!(matches!(r, Err(Error::Cancelled)), "[{tag}] {r:?}");
            assert_eq!(sink.batches, 2);
            assert!(started.elapsed() < Duration::from_secs(5));
            // The same (only) connection answers the next queries correctly.
            let rs = query(&*s, "SELECT 1 + 1").await;
            assert_eq!(rs.rows[0][0], Value::Int(2));
            // No READ ONLY transaction is left over on the connection.
            run(&*s, &format!("INSERT INTO `{scratch}`.t VALUES (1)")).await;
        }
        // A custom sink error is propagated unchanged.
        let mut req = table_req(FIXTURE_DB, "big");
        req.limit = 1_000_000;
        let mut sink = CountingSink {
            fail_after: Some((1, Some("sink says stop".into()))),
            ..Default::default()
        };
        match s.fetch_table(req, &mut sink).await {
            Err(Error::Other(m)) => assert_eq!(m, "sink says stop"),
            other => panic!("[{tag}] {other:?}"),
        }
        // Same for execute.
        let mut sink = CountingSink {
            fail_after: Some((1, None)),
            ..Default::default()
        };
        let mut q = QueryRequest::new(format!("SELECT * FROM `{FIXTURE_DB}`.big"));
        q.batch_size = Some(100);
        assert!(matches!(
            s.execute(q, &mut sink).await,
            Err(Error::Cancelled)
        ));
        s.ping().await.unwrap();
        assert_eq!(
            query(&*s, &format!("SELECT COUNT(*) FROM {FIXTURE_DB}.people"))
                .await
                .rows
                .len(),
            1
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_kills_the_statement() {
    if !it_enabled() {
        return;
    }
    for t in targets() {
        let s = user_session(t).await;
        let tag = t.name;
        let mk = marker();
        let q = QueryRequest::new(format!("SELECT SLEEP(60) /* {mk} */"));
        let id = q.query_id;
        let s2 = s.clone();
        let task = tokio::spawn(async move {
            let mut sink = CountingSink::default();
            let started = Instant::now();
            let r = s2.execute(q, &mut sink).await;
            (r, started.elapsed())
        });
        wait_running(t, &mk).await;
        let c0 = Instant::now();
        s.cancel(id).await.unwrap();
        let (r, took) = task.await.unwrap();
        assert!(matches!(r, Err(Error::Cancelled)), "[{tag}] {r:?}");
        assert!(
            c0.elapsed() < Duration::from_secs(2),
            "[{tag}] cancel took {:?}",
            c0.elapsed()
        );
        assert!(took < Duration::from_secs(3), "[{tag}] {took:?}");
        // Unknown ids are a harmless no-op.
        s.cancel(Uuid::new_v4()).await.unwrap();
        s.cancel(id).await.unwrap();
        s.ping().await.unwrap();

        // Cancel a long multi-statement script between statements.
        let mk = marker();
        let q = QueryRequest::new(format!("SELECT 1; SELECT SLEEP(60) /* {mk} */; SELECT 2"));
        let id = q.query_id;
        let s2 = s.clone();
        let task = tokio::spawn(async move {
            let mut sink = dbstudio_core::CollectingSink::new();
            let r = s2.execute(q, &mut sink).await;
            (r, sink.into_results().len())
        });
        wait_running(t, &mk).await;
        s.cancel(id).await.unwrap();
        let (r, delivered) = task.await.unwrap();
        assert!(matches!(r, Err(Error::Cancelled)), "[{tag}] {r:?}");
        assert_eq!(
            delivered, 1,
            "[{tag}] the first statement's result was delivered"
        );

        // Cancel a fetch_table that is streaming slowly.
        ensure_fixture(t).await;
        let mut req = table_req(FIXTURE_DB, "big");
        req.limit = 1_000_000;
        req.batch_size = Some(1000);
        let progress = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let prog = progress.clone();
        let id = req.query_id;
        let s2 = s.clone();
        let task = tokio::spawn(async move {
            let mut sink = CountingSink {
                delay_ms: 40,
                progress: Some(prog),
                ..Default::default()
            };
            let started = Instant::now();
            let r = s2.fetch_table(req, &mut sink).await;
            (r, sink.batches, started.elapsed())
        });
        wait_progress(&progress, 2).await;
        s.cancel(id).await.unwrap();
        let (r, batches, took) = task.await.unwrap();
        assert!(matches!(r, Err(Error::Cancelled)), "[{tag}] {r:?}");
        assert!(
            batches < 150,
            "[{tag}] stopped early after {batches} batches"
        );
        assert!(took < Duration::from_secs(4), "[{tag}] {took:?}");
        s.ping().await.unwrap();
        assert_eq!(query(&*s, "SELECT 5").await.rows[0][0], Value::Int(5));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn timeout_ms_interrupts() {
    if !it_enabled() {
        return;
    }
    for t in targets() {
        let mut c = config(t, "dbstudio", TlsMode::Disable);
        c.options.insert("pool_max".into(), "1".into());
        let s = connect_cfg(&c).await.unwrap();
        let mut q = QueryRequest::new("SELECT SLEEP(30)");
        q.timeout_ms = Some(600);
        let started = Instant::now();
        let r = s.execute(q, &mut CountingSink::default()).await;
        assert!(matches!(r, Err(Error::Timeout)), "[{}] {r:?}", t.name);
        assert!(started.elapsed() < Duration::from_secs(4));
        // A fast query with a generous timeout is untouched, and a later query on the same
        // connection is not hit by a stray KILL.
        let mut q = QueryRequest::new("SELECT 1");
        q.timeout_ms = Some(300);
        execute_collect(&*s, q).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        let mut q = QueryRequest::new("SELECT SLEEP(0.8)");
        q.timeout_ms = Some(5000);
        execute_collect(&*s, q).await.unwrap();
    }
}

#[tokio::test]
async fn execute_scripts() {
    if !it_enabled() {
        return;
    }
    for t in targets() {
        let tag = t.name;
        let db = scratch_db(t, "exec").await;
        let s = user_session(t).await;
        let mut q = QueryRequest::new(
            "CREATE TABLE a (id INT AUTO_INCREMENT PRIMARY KEY, v VARCHAR(10));
             INSERT INTO a (v) VALUES ('x'),('y'),('z');
             UPDATE a SET v = 'q' WHERE id > 1;
             SELECT * FROM a ORDER BY id;
             SELECT COUNT(*) AS n FROM a;
             DELETE FROM a WHERE id = 99;
             DROP TABLE a",
        );
        q.database = Some(db.clone());
        let mut sink = dbstudio_core::CollectingSink::new();
        let sum = s.execute(q, &mut sink).await.unwrap();
        let r = sink.into_results();
        assert_eq!(sum.statements, 7, "[{tag}]");
        assert_eq!(r.len(), 7);
        assert_eq!(r[0].outcome, StatementOutcome::Done);
        assert_eq!(r[1].outcome, StatementOutcome::Affected { rows: 3 });
        assert_eq!(r[2].outcome, StatementOutcome::Affected { rows: 2 });
        match &r[3].outcome {
            StatementOutcome::Rows(rs) => {
                assert_eq!(rs.columns.len(), 2);
                assert_eq!(rs.rows[1], vec![Value::Int(2), Value::Text("q".into())]);
            }
            o => panic!("[{tag}] {o:?}"),
        }
        match &r[4].outcome {
            StatementOutcome::Rows(rs) => assert_eq!(rs.rows[0][0], Value::Int(3)),
            o => panic!("{o:?}"),
        }
        assert_eq!(r[6].outcome, StatementOutcome::Done);

        // An error in the middle: earlier results are delivered, the error has the MySQL code.
        let mut q = QueryRequest::new("SELECT 1 AS one; SELECT * FROM no_such_table_zz; SELECT 3");
        q.database = Some(db.clone());
        let mut sink = dbstudio_core::CollectingSink::new();
        let e = s.execute(q, &mut sink).await.unwrap_err();
        match &e {
            Error::Query { message, .. } => {
                assert!(message.contains("1146"), "[{tag}] {message}");
                assert!(message.contains("no_such_table_zz"));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            sink.into_results().len(),
            1,
            "[{tag}] first statement result kept"
        );
        // Syntax error in the first statement.
        let e = execute_collect(&*s, QueryRequest::new("SELEC 1"))
            .await
            .unwrap_err();
        assert!(
            matches!(&e, Error::Query { message, .. } if message.contains("1064")),
            "{e:?}"
        );

        // `database` switches the default database, and the pool does not leak it.
        let mut q = QueryRequest::new("SELECT DATABASE()");
        q.database = Some(db.clone());
        let r = execute_collect(&*s, q).await.unwrap();
        match &r[0].outcome {
            StatementOutcome::Rows(rs) => assert_eq!(rs.rows[0][0], Value::Text(db.clone())),
            o => panic!("{o:?}"),
        }
        let r = query(&*s, "SELECT DATABASE()").await;
        assert_eq!(
            r.rows[0][0],
            Value::Null,
            "[{tag}] default database must not leak between requests"
        );
        let mut q = QueryRequest::new("SELECT 1");
        q.database = Some("dbstudio_it_does_not_exist".into());
        assert!(matches!(
            execute_collect(&*s, q).await,
            Err(Error::Query { .. })
        ));

        // Warnings become messages.
        let mut q = QueryRequest::new(
            "SET SESSION sql_mode = ''; CREATE TABLE w (a TINYINT); INSERT INTO w VALUES (1000)",
        );
        q.database = Some(db.clone());
        let r = execute_collect(&*s, q).await.unwrap();
        assert_eq!(r.len(), 3);
        assert_eq!(r[2].outcome, StatementOutcome::Affected { rows: 1 });
        assert!(
            r[2].messages.iter().any(|m| m.contains("warning")),
            "[{tag}] {:?}",
            r[2].messages
        );
        // Empty result set keeps its columns.
        let rs = query(&*s, "SELECT 1 AS a, 'x' AS b FROM DUAL WHERE 1 = 0").await;
        assert!(rs.rows.is_empty() && rs.columns.len() == 2 && !rs.truncated);
        // Calling a procedure returns its result set followed by a status result.
        ensure_fixture(t).await;
        let mut q = QueryRequest::new("CALL pr_noop()");
        q.database = Some(FIXTURE_DB.into());
        let r = execute_collect(&*s, q).await.unwrap();
        assert!(matches!(r[0].outcome, StatementOutcome::Rows(_)));
        assert_eq!(r.len(), 2);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_use_of_one_session() {
    if !it_enabled() {
        return;
    }
    for t in targets() {
        ensure_fixture(t).await;
        let s = user_session(t).await;
        let tag = t.name;
        let mut handles = Vec::new();
        for i in 0..24u32 {
            let s = s.clone();
            handles.push(tokio::spawn(async move {
                match i % 6 {
                    0 => s.ping().await.map(|_| ()),
                    1 => s.server_info().await.map(|_| ()),
                    2 => s
                        .list_children(&ObjectPath::root().child(K::Database, FIXTURE_DB))
                        .await
                        .map(|v| assert!(!v.is_empty())),
                    3 => {
                        let mut r = table_req(FIXTURE_DB, "people");
                        r.filters = vec![f("age", Op::Eq, &["25"])];
                        fetch(&*s, r).await.map(|rs| assert_eq!(rs.rows.len(), 2))
                    }
                    4 => {
                        let mut r = table_req(FIXTURE_DB, "big");
                        r.limit = 20_000;
                        let mut sink = CountingSink::default();
                        s.fetch_table(r, &mut sink)
                            .await
                            .map(|_| assert_eq!(sink.rows, 20_000))
                    }
                    _ => execute_collect(
                        &*s,
                        QueryRequest::new("SELECT SLEEP(0.2), CONNECTION_ID()"),
                    )
                    .await
                    .map(|_| ()),
                }
            }));
        }
        for h in handles {
            h.await
                .unwrap()
                .unwrap_or_else(|e| panic!("[{tag}] concurrent call failed: {e}"));
        }
        // table_details concurrently as well.
        let p = ObjectPath::root()
            .child(K::Database, FIXTURE_DB)
            .child(K::Table, "child");
        let (a, b, c) = tokio::join!(s.table_details(&p), s.object_ddl(&p), s.list_children(&p));
        a.unwrap();
        b.unwrap();
        c.unwrap();
    }
}

const RAW_DB_TABLE: &str = "t";

async fn raw_where_fixture(t: Target) -> String {
    let db = scratch_db(t, "rawwhere").await;
    let root = root_session(t).await;
    run(
        &*root,
        &format!(
            "CREATE TABLE `{db}`.t (id INT PRIMARY KEY, name VARCHAR(20));
             INSERT INTO `{db}`.t VALUES (1,'a;b'),(2,'two'),(3,'three'),(4,'four'),(5,'five')"
        ),
    )
    .await;
    root.close().await.unwrap();
    db
}

async fn raw_where_intact(s: &dyn Session, db: &str, tag: &str, evil: &str) {
    let n = query(s, &format!("SELECT COUNT(*) FROM `{db}`.t")).await;
    assert_eq!(
        n.rows[0][0],
        Value::Int(5),
        "[{tag}] data changed by `{evil}`"
    );
    let tables = query(
        s,
        &format!("SELECT COUNT(*) FROM information_schema.TABLES WHERE TABLE_SCHEMA = '{db}'"),
    )
    .await;
    assert_eq!(
        tables.rows[0][0],
        Value::Int(1),
        "[{tag}] schema changed by `{evil}`"
    );
}

#[tokio::test]
async fn raw_where_cannot_run_extra_statements() {
    if !it_enabled() {
        return;
    }
    for t in targets() {
        let tag = t.name;
        let db = raw_where_fixture(t).await;
        let s = user_session(t).await;
        // A profile marked read_only must stay read only too.
        let mut c = config(t, "dbstudio", TlsMode::Disable);
        c.read_only = true;
        let ro: Arc<dyn Session> = Arc::from(connect_cfg(&c).await.unwrap());
        let table = ObjectPath::root()
            .child(K::Database, db.as_str())
            .child(K::Table, RAW_DB_TABLE);
        let payloads = [
            format!("1=1); COMMIT; DELETE FROM `{db}`.t; SELECT (1"),
            format!("1=1); SET SESSION TRANSACTION READ WRITE; DELETE FROM `{db}`.t; SELECT (1"),
            format!("1=1); CREATE TABLE `{db}`.evil (a INT); SELECT (1"),
            format!("1=1); DROP TABLE `{db}`.t; SELECT (1"),
            format!("1=1; DELETE FROM `{db}`.t"),
            format!("1=1 /*! ); DELETE FROM `{db}`.t; SELECT ( */"),
            "1=1) /*".to_string(),
            "(1=1".to_string(),
            "1=1)".to_string(),
            "name = 'unterminated".to_string(),
            format!("1=1 -- \n); DELETE FROM `{db}`.t; SELECT (1"),
            format!("1=1 # \n); DELETE FROM `{db}`.t; SELECT (1"),
        ];
        for evil in &payloads {
            for sess in [&s, &ro] {
                let mut r = table_req(&db, RAW_DB_TABLE);
                r.raw_where = Some(evil.clone());
                let e = fetch(&**sess, r).await.unwrap_err();
                assert!(
                    matches!(e, Error::Query { position: None, .. }),
                    "[{tag}] fetch `{evil}`: {e:?}"
                );
                let e = sess
                    .count_table(TableCountRequest {
                        query_id: None,
                        table: table.clone(),
                        filters: vec![],
                        raw_where: Some(evil.clone()),
                    })
                    .await
                    .unwrap_err();
                assert!(
                    matches!(e, Error::Query { position: None, .. }),
                    "[{tag}] count `{evil}`: {e:?}"
                );
                raw_where_intact(&**sess, &db, tag, evil).await;
            }
        }
        // Legitimate fragments, including `;` and comment markers inside literals, still work.
        for (w, expect) in [
            ("name = 'a;b'", vec![1]),
            ("name = \"a;b\"", vec![1]),
            ("name = 'two' -- trailing comment ; (", vec![2]),
            ("name = 'two' # another ) comment", vec![2]),
            ("(id > 3) AND /* ; ( */ id < 100", vec![4, 5]),
            ("id = /*! 3 */", vec![3]),
            ("name LIKE '%;%'", vec![1]),
        ] {
            let mut r = table_req(&db, RAW_DB_TABLE);
            r.raw_where = Some(w.to_string());
            assert_eq!(
                people_ids(&fetch(&*s, r).await.unwrap()),
                expect,
                "[{tag}] {w}"
            );
            let n = s
                .count_table(TableCountRequest {
                    query_id: None,
                    table: table.clone(),
                    filters: vec![],
                    raw_where: Some(w.to_string()),
                })
                .await
                .unwrap();
            assert_eq!(n as usize, expect.len(), "[{tag}] count {w}");
        }
        raw_where_intact(&*s, &db, tag, "legit fragments").await;
    }
}

#[tokio::test]
async fn paging_is_deterministic() {
    if !it_enabled() {
        return;
    }
    for t in targets() {
        ensure_fixture(t).await;
        let s = user_session(t).await;
        let tag = t.name;
        // `grp` has 2000 rows per value: without a tie-break the pages would overlap.
        for sort in [
            vec![SortKey {
                column: "grp".into(),
                descending: false,
            }],
            vec![SortKey {
                column: "grp".into(),
                descending: true,
            }],
            vec![],
        ] {
            let mut seen = std::collections::HashSet::new();
            let mut total = 0;
            for page in 0..8u64 {
                let mut r = table_req(FIXTURE_DB, "big");
                r.sort = sort.clone();
                r.offset = page * 25_000;
                r.limit = 25_000;
                let rs = fetch(&*s, r).await.unwrap();
                total += rs.rows.len();
                for id in ints(&rs, 0) {
                    assert!(seen.insert(id), "[{tag}] row {id} appeared on two pages");
                }
            }
            assert_eq!((total, seen.len()), (200_000, 200_000), "[{tag}] {sort:?}");
        }
        // Unique NOT NULL index is used when there is no primary key; keyless tables are fine.
        let db = scratch_db(t, "paging").await;
        let root = root_session(t).await;
        run(
            &*root,
            &format!(
                "CREATE TABLE `{db}`.u (a INT NOT NULL, b INT, UNIQUE KEY ua (a));
                 INSERT INTO `{db}`.u VALUES (3,1),(1,1),(2,1);
                 CREATE TABLE `{db}`.k (a INT);
                 INSERT INTO `{db}`.k VALUES (3),(1),(2)"
            ),
        )
        .await;
        root.close().await.unwrap();
        let rs = fetch(&*s, table_req(&db, "u")).await.unwrap();
        assert_eq!(
            ints(&rs, 0),
            vec![1, 2, 3],
            "[{tag}] unique index tie-break"
        );
        assert_eq!(fetch(&*s, table_req(&db, "k")).await.unwrap().rows.len(), 3);
        let mut r = table_req(&db, "u");
        r.sort = vec![SortKey {
            column: "B".into(),
            descending: false,
        }];
        assert_eq!(ints(&fetch(&*s, r).await.unwrap(), 0), vec![1, 2, 3]);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn count_table_can_be_cancelled() {
    if !it_enabled() {
        return;
    }
    for t in targets() {
        ensure_fixture(t).await;
        let s = user_session(t).await;
        let mk = marker();
        let id = Uuid::new_v4();
        let s2 = s.clone();
        let table = ObjectPath::root()
            .child(K::Database, FIXTURE_DB)
            .child(K::Table, "big");
        let raw = format!("SLEEP(1) = 0 /* {mk} */");
        let task = tokio::spawn(async move {
            s2.count_table(TableCountRequest {
                query_id: Some(id),
                table,
                filters: vec![],
                raw_where: Some(raw),
            })
            .await
        });
        wait_running(t, &mk).await;
        let c0 = Instant::now();
        s.cancel(id).await.unwrap();
        let r = task.await.unwrap();
        assert!(matches!(r, Err(Error::Cancelled)), "[{}] {r:?}", t.name);
        assert!(c0.elapsed() < Duration::from_secs(3));
        s.ping().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_cancels_running_statements() {
    if !it_enabled() {
        return;
    }
    for t in targets() {
        ensure_fixture(t).await;
        let s = user_session(t).await;
        let mk1 = marker();
        let mk2 = marker();
        let s1 = s.clone();
        let q = QueryRequest::new(format!("SELECT SLEEP(60) /* {mk1} */"));
        let exec = tokio::spawn(async move { s1.execute(q, &mut CountingSink::default()).await });
        let s2 = s.clone();
        let mut req = table_req(FIXTURE_DB, "big");
        req.raw_where = Some(format!("SLEEP(1) = 0 /* {mk2} */"));
        let fetch_task =
            tokio::spawn(async move { s2.fetch_table(req, &mut CountingSink::default()).await });
        wait_running(t, &mk1).await;
        wait_running(t, &mk2).await;
        let c0 = Instant::now();
        s.close().await.unwrap();
        assert!(
            c0.elapsed() < Duration::from_secs(5),
            "[{}] close took {:?}",
            t.name,
            c0.elapsed()
        );
        let r = exec.await.unwrap();
        assert!(matches!(r, Err(Error::Cancelled)), "[{}] {r:?}", t.name);
        let r = fetch_task.await.unwrap();
        assert!(matches!(r, Err(Error::Cancelled)), "[{}] {r:?}", t.name);
        // The server really stopped them.
        let root = root_session(t).await;
        let rs = query(
            &*root,
            &format!(
                "SELECT COUNT(*) FROM information_schema.PROCESSLIST WHERE INFO LIKE '%{mk1}%' OR INFO LIKE '%{mk2}%'"
            ),
        )
        .await;
        assert_eq!(
            rs.rows[0][0],
            Value::Int(1),
            "only the probe itself matches"
        );
        assert!(matches!(s.ping().await, Err(Error::Connection(_))));
    }
}
