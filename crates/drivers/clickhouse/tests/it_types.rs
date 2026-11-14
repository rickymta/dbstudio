//! Every type of the design table (§3.2.2) round-trips through the driver, both read (SELECT)
//! and write (bulk insert / export -> import).

mod common;

use common::*;
use dbstudio_core::{
    BulkInsertRequest, EditValue, LeaseOptions, ObjectKind, ObjectPath, OnConflict,
    TableStreamRequest, Value, ValueKind,
};

struct Col {
    name: &'static str,
    ddl: &'static str,
    insert: &'static str,
    expect: Value,
    kind: ValueKind,
    /// Cannot be written through bulk insert.
    read_only: bool,
}

fn col(
    name: &'static str,
    ddl: &'static str,
    insert: &'static str,
    expect: Value,
    kind: ValueKind,
) -> Col {
    Col {
        name,
        ddl,
        insert,
        expect,
        kind,
        read_only: false,
    }
}

fn text(s: &str) -> Value {
    Value::Text(s.into())
}

fn columns() -> Vec<Col> {
    use ValueKind as K;
    let mut v = vec![
        col("i8", "Int8", "-8", Value::Int(-8), K::Int),
        col("i16", "Int16", "-16", Value::Int(-16), K::Int),
        col("i32", "Int32", "-32", Value::Int(-32), K::Int),
        col(
            "i64",
            "Int64",
            "-9223372036854775808",
            Value::Int(i64::MIN),
            K::Int,
        ),
        col(
            "i128",
            "Int128",
            "toInt128('-170141183460469231731687303715884105728')",
            Value::Decimal("-170141183460469231731687303715884105728".into()),
            K::Decimal,
        ),
        col(
            "i256",
            "Int256",
            "toInt256('-57896044618658097711785492504343953926634992332820282019728792003956564819968')",
            Value::Decimal(
                "-57896044618658097711785492504343953926634992332820282019728792003956564819968"
                    .into(),
            ),
            K::Decimal,
        ),
        col("u8", "UInt8", "255", Value::Int(255), K::Int),
        col("u16", "UInt16", "65535", Value::Int(65535), K::Int),
        col(
            "u32",
            "UInt32",
            "4294967295",
            Value::Int(4294967295),
            K::Int,
        ),
        col(
            "u64",
            "UInt64",
            "18446744073709551615",
            Value::UInt(u64::MAX),
            K::UInt,
        ),
        col(
            "u128",
            "UInt128",
            "toUInt128('340282366920938463463374607431768211455')",
            Value::Decimal("340282366920938463463374607431768211455".into()),
            K::Decimal,
        ),
        col(
            "u256",
            "UInt256",
            "toUInt256('115792089237316195423570985008687907853269984665640564039457584007913129639935')",
            Value::Decimal(
                "115792089237316195423570985008687907853269984665640564039457584007913129639935"
                    .into(),
            ),
            K::Decimal,
        ),
        col("f32", "Float32", "0.1", Value::Float(0.1), K::Float),
        col(
            "f64",
            "Float64",
            "3.141592653589793",
            Value::Float(std::f64::consts::PI),
            K::Float,
        ),
        col("b", "Bool", "true", Value::Bool(true), K::Bool),
        col("s", "String", "'héllo \\'q\\''", text("héllo 'q'"), K::Text),
        col(
            "sb",
            "String",
            "unhex('fffe00')",
            Value::Bytes(vec![0xff, 0xfe, 0]),
            K::Text,
        ),
        col(
            "fs",
            "FixedString(4)",
            "'ab'",
            Value::Bytes(b"ab\0\0".to_vec()),
            K::Text,
        ),
        col(
            "u",
            "UUID",
            "toUUID('61f0c404-5cb3-11e7-907b-a6006ad3dba0')",
            Value::Uuid("61f0c404-5cb3-11e7-907b-a6006ad3dba0".into()),
            K::Uuid,
        ),
        col(
            "d",
            "Date",
            "toDate('2024-02-29')",
            Value::Date("2024-02-29".into()),
            K::Date,
        ),
        col(
            "d32",
            "Date32",
            "toDate32('1900-01-01')",
            Value::Date("1900-01-01".into()),
            K::Date,
        ),
        col(
            "dt",
            "DateTime('UTC')",
            "toDateTime('2024-02-29 12:34:56', 'UTC')",
            Value::Timestamp("2024-02-29T12:34:56Z".into()),
            K::Timestamp,
        ),
        col(
            "dt64",
            "DateTime64(3, 'Europe/Paris')",
            "toDateTime64('2024-02-29 12:34:56.789', 3, 'Europe/Paris')",
            Value::Timestamp("2024-02-29T11:34:56.789Z".into()),
            K::Timestamp,
        ),
        col(
            "dec32",
            "Decimal(9, 2)",
            "toDecimal32('1234567.89', 2)",
            Value::Decimal("1234567.89".into()),
            K::Decimal,
        ),
        col(
            "dec64",
            "Decimal(18, 4)",
            "toDecimal64('-0.5', 4)",
            Value::Decimal("-0.5000".into()),
            K::Decimal,
        ),
        col(
            "dec128",
            "Decimal(38, 10)",
            "toDecimal128('12345678901234567890.0123456789', 10)",
            Value::Decimal("12345678901234567890.0123456789".into()),
            K::Decimal,
        ),
        col(
            "dec256",
            "Decimal(76, 20)",
            "toDecimal256('-1234567890123456789012345678901234567890.12345678901234567890', 20)",
            Value::Decimal("-1234567890123456789012345678901234567890.12345678901234567890".into()),
            K::Decimal,
        ),
        col("e8", "Enum8('a' = 1, 'b' = 2)", "'b'", text("b"), K::Text),
        col(
            "e16",
            "Enum16('x' = 1000, 'y' = 2000)",
            "'y'",
            text("y"),
            K::Text,
        ),
        col(
            "ip4",
            "IPv4",
            "toIPv4('192.168.0.1')",
            text("192.168.0.1"),
            K::Text,
        ),
        col(
            "ip6",
            "IPv6",
            "toIPv6('2001:db8::1')",
            text("2001:db8::1"),
            K::Text,
        ),
        col("ns", "Nullable(String)", "NULL", Value::Null, K::Text),
        col(
            "lc",
            "LowCardinality(String)",
            "'low'",
            text("low"),
            K::Text,
        ),
        col(
            "lcn",
            "LowCardinality(Nullable(String))",
            "NULL",
            Value::Null,
            K::Text,
        ),
        col(
            "arr",
            "Array(Nullable(Int32))",
            "[1, NULL, 3]",
            Value::Array(vec![Value::Int(1), Value::Null, Value::Int(3)]),
            K::Array,
        ),
        col(
            "tup",
            "Tuple(a UInt8, b String)",
            "CAST((1, 'x') AS Tuple(a UInt8, b String))",
            Value::Document(vec![("a".into(), Value::Int(1)), ("b".into(), text("x"))]),
            K::Document,
        ),
        col(
            "tup2",
            "Tuple(UInt8, String)",
            "(2, 'y')",
            Value::Array(vec![Value::Int(2), text("y")]),
            K::Array,
        ),
        col(
            "mp",
            "Map(String, UInt32)",
            "map('k', 7)",
            Value::Document(vec![("k".into(), Value::Int(7))]),
            K::Document,
        ),
        col(
            "mp2",
            "Map(UInt8, String)",
            "map(1, 'one')",
            Value::Array(vec![Value::Array(vec![Value::Int(1), text("one")])]),
            K::Array,
        ),
        col(
            "nested",
            "Array(Array(String))",
            "[['a'], ['b', 'c']]",
            Value::Array(vec![
                Value::Array(vec![text("a")]),
                Value::Array(vec![text("b"), text("c")]),
            ]),
            K::Array,
        ),
        col(
            "pt",
            "Point",
            "(1.5, 2.5)",
            Value::Array(vec![Value::Float(1.5), Value::Float(2.5)]),
            K::Array,
        ),
        col(
            "ring",
            "Ring",
            "[(0, 0), (1, 0), (1, 1)]",
            Value::Array(vec![
                Value::Array(vec![Value::Float(0.0), Value::Float(0.0)]),
                Value::Array(vec![Value::Float(1.0), Value::Float(0.0)]),
                Value::Array(vec![Value::Float(1.0), Value::Float(1.0)]),
            ]),
            K::Array,
        ),
    ];
    let mut json = col(
        "j",
        "JSON",
        "'{\"a\":1}'",
        Value::Json("{\"a\":1}".into()),
        K::Json,
    );
    json.read_only = false;
    v.push(json);
    let mut variant = col(
        "v",
        "Variant(String, UInt64)",
        "CAST(toUInt64(42) AS Variant(String, UInt64))",
        Value::UInt(42),
        K::Text,
    );
    variant.read_only = true;
    v.push(variant);
    let mut dynamic = col(
        "dy",
        "Dynamic",
        "CAST('txt' AS Dynamic)",
        text("txt"),
        K::Text,
    );
    dynamic.read_only = true;
    v.push(dynamic);
    v
}

#[tokio::test]
async fn every_type_reads_back_exactly() {
    let Some(s) = session().await else { return };
    let db = TempDb::create(s.as_ref(), http_port(), "types").await;
    let cols = columns();
    let ddl: Vec<String> = cols
        .iter()
        .map(|c| format!("`{}` {}", c.name, c.ddl))
        .collect();
    run(
        s.as_ref(),
        &format!(
            "CREATE TABLE {} (id UInt32, {}) ENGINE = MergeTree ORDER BY id",
            db.t("t"),
            ddl.join(", ")
        ),
    )
    .await;
    let exprs: Vec<String> = cols.iter().map(|c| c.insert.to_string()).collect();
    run(
        s.as_ref(),
        &format!("INSERT INTO {} SELECT 1, {}", db.t("t"), exprs.join(", ")),
    )
    .await;
    run(
        s.as_ref(),
        &format!("INSERT INTO {} (id) VALUES (2)", db.t("t")),
    )
    .await;

    let r = run(
        s.as_ref(),
        &format!("SELECT * FROM {} ORDER BY id", db.t("t")),
    )
    .await;
    let rs = rows_of(&r[0]);
    assert_eq!(rs.rows.len(), 2);
    for c in &cols {
        let i = rs.columns.iter().position(|m| m.name == c.name).unwrap();
        let got = &rs.rows[0][i];
        match (&c.expect, got) {
            (Value::Json(want), Value::Json(have)) => {
                let a: serde_json::Value = serde_json::from_str(want).unwrap();
                let b: serde_json::Value = serde_json::from_str(have).unwrap();
                assert_eq!(a, b, "column {}", c.name);
            }
            (want, have) => assert_eq!(want, have, "column {} ({})", c.name, c.ddl),
        }
        if !matches!(c.name, "v" | "dy") {
            assert_eq!(rs.columns[i].value_kind, c.kind, "kind of {}", c.name);
        }
        if matches!(c.name, "ns" | "lcn") {
            assert_eq!(rs.columns[i].nullable, Some(true), "nullable {}", c.name);
        }
        if c.name == "i8" {
            assert_eq!(rs.columns[i].type_name, "Int8");
            assert_eq!(rs.columns[i].nullable, Some(false));
        }
    }
    // The defaults row (all zero / empty / NULL) decodes without error as well.
    assert_eq!(rs.rows[1][0], Value::Int(2));
}

#[tokio::test]
async fn export_then_import_round_trips_through_rowbinary() {
    let Some(s) = session().await else { return };
    let db = TempDb::create(s.as_ref(), http_port(), "copy").await;
    let cols: Vec<Col> = columns().into_iter().filter(|c| !c.read_only).collect();
    let ddl: Vec<String> = cols
        .iter()
        .map(|c| format!("`{}` {}", c.name, c.ddl))
        .collect();
    for t in ["src", "dst"] {
        run(
            s.as_ref(),
            &format!(
                "CREATE TABLE {} (id UInt32, {}) ENGINE = MergeTree ORDER BY id",
                db.t(t),
                ddl.join(", ")
            ),
        )
        .await;
    }
    let exprs: Vec<String> = cols.iter().map(|c| c.insert.to_string()).collect();
    run(
        s.as_ref(),
        &format!("INSERT INTO {} SELECT 1, {}", db.t("src"), exprs.join(", ")),
    )
    .await;
    run(
        s.as_ref(),
        &format!("INSERT INTO {} (id) VALUES (2)", db.t("src")),
    )
    .await;

    let table = |name: &str| {
        ObjectPath::root()
            .child(ObjectKind::Database, db.name.clone())
            .child(ObjectKind::Table, name)
    };
    let lease = s.open_lease(LeaseOptions::default()).await.unwrap();
    let mut sink = dbstudio_core::CollectingSink::new();
    lease
        .stream_table(
            TableStreamRequest {
                query_id: uuid::Uuid::new_v4(),
                table: table("src"),
                columns: vec![],
                filters: vec![],
                raw_where: None,
                order_by_key: true,
                batch_size: None,
            },
            &mut sink,
        )
        .await
        .unwrap();
    let results = sink.into_results();
    let rs = rows_of(&results[0]);
    assert_eq!(rs.rows.len(), 2);
    let names: Vec<String> = rs.columns.iter().map(|c| c.name.clone()).collect();
    let n = lease
        .bulk_insert(BulkInsertRequest {
            table: table("dst"),
            columns: names.clone(),
            rows: rs
                .rows
                .iter()
                .map(|r| r.iter().cloned().map(EditValue::Value).collect())
                .collect(),
            on_conflict: OnConflict::Error,
            conflict_columns: vec![],
        })
        .await
        .unwrap();
    assert_eq!(n.inserted, 2);
    lease.close().await.unwrap();

    let a = query(
        s.as_ref(),
        &format!("SELECT * FROM {} ORDER BY id", db.t("src")),
    )
    .await;
    let b = query(
        s.as_ref(),
        &format!("SELECT * FROM {} ORDER BY id", db.t("dst")),
    )
    .await;
    assert_eq!(a.len(), b.len());
    for (ra, rb) in a.iter().zip(&b) {
        for (i, (x, y)) in ra.iter().zip(rb).enumerate() {
            match (x, y) {
                (Value::Json(p), Value::Json(q)) => assert_eq!(
                    serde_json::from_str::<serde_json::Value>(p).unwrap(),
                    serde_json::from_str::<serde_json::Value>(q).unwrap()
                ),
                _ => assert_eq!(x, y, "column #{i} {}", names[i]),
            }
        }
    }
    // The server agrees byte for byte on a few exact types.
    let same = scalar(
        s.as_ref(),
        &format!(
            "SELECT count() FROM {} s INNER JOIN {} d ON s.id = d.id WHERE s.u256 = d.u256 AND s.dec256 = d.dec256 AND s.sb = d.sb AND s.u = d.u AND s.dt64 = d.dt64",
            db.t("src"),
            db.t("dst")
        ),
    )
    .await;
    assert_eq!(same, Value::UInt(2));
}

#[tokio::test]
async fn bulk_insert_defaults_and_errors() {
    let Some(s) = session().await else { return };
    let db = TempDb::create(s.as_ref(), http_port(), "bulk").await;
    run(
        s.as_ref(),
        &format!(
            "CREATE TABLE {} (id UInt32, n String DEFAULT 'dflt', m MATERIALIZED id * 2) ENGINE = MergeTree ORDER BY id",
            db.t("t")
        ),
    )
    .await;
    let path = ObjectPath::root()
        .child(ObjectKind::Database, db.name.clone())
        .child(ObjectKind::Table, "t");
    let lease = s.open_lease(LeaseOptions::default()).await.unwrap();
    let req = |columns: &[&str], rows: Vec<Vec<EditValue>>, on_conflict| BulkInsertRequest {
        table: path.clone(),
        columns: columns.iter().map(|c| c.to_string()).collect(),
        rows,
        on_conflict,
        conflict_columns: vec![],
    };
    lease
        .bulk_insert(req(
            &["id", "n"],
            vec![
                vec![EditValue::Value(Value::Int(1)), EditValue::Default],
                vec![EditValue::Text("2".into()), EditValue::Text("given".into())],
            ],
            OnConflict::Error,
        ))
        .await
        .unwrap();
    let rows = query(
        s.as_ref(),
        &format!("SELECT id, n, m FROM {} ORDER BY id", db.t("t")),
    )
    .await;
    assert_eq!(rows[0], vec![Value::Int(1), text("dflt"), Value::UInt(2)]);
    assert_eq!(rows[1], vec![Value::Int(2), text("given"), Value::UInt(4)]);
    // Unsupported conflict handling, unknown and generated columns, bad values.
    for (cols, rows, mode) in [
        (
            vec!["id"],
            vec![vec![EditValue::Text("3".into())]],
            OnConflict::Skip,
        ),
        (
            vec!["nope"],
            vec![vec![EditValue::Text("3".into())]],
            OnConflict::Error,
        ),
        (
            vec!["m"],
            vec![vec![EditValue::Text("3".into())]],
            OnConflict::Error,
        ),
        (
            vec!["id"],
            vec![vec![EditValue::Text("abc".into())]],
            OnConflict::Error,
        ),
        (vec!["id"], vec![vec![]], OnConflict::Error),
    ] {
        let err = lease.bulk_insert(req(&cols, rows, mode)).await.unwrap_err();
        assert!(matches!(err.kind(), "unsupported" | "invalid"), "{err:?}");
    }
    lease.close().await.unwrap();
}
