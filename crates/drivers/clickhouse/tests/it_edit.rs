//! Conservative data editing: MergeTree only, sorting key identifies rows, `count() = 1` guard.

mod common;

use common::*;
use dbstudio_core::{
    CellEdit, ChangeSet, EditNote, EditValue, Error, NotEditableReason, ObjectKind, ObjectPath,
    RowChange, RowChangeSet, Value,
};

fn table(db: &str, name: &str) -> ObjectPath {
    ObjectPath::root()
        .child(ObjectKind::Database, db)
        .child(ObjectKind::Table, name)
}

fn cell(c: &str, v: EditValue) -> CellEdit {
    CellEdit {
        column: c.into(),
        value: v,
    }
}

fn rows(table: ObjectPath, key: &[&str], changes: Vec<RowChange>) -> ChangeSet {
    ChangeSet::Rows(RowChangeSet {
        table,
        key_columns: key.iter().map(|k| k.to_string()).collect(),
        changes,
    })
}

#[tokio::test]
async fn merge_tree_is_editable_by_its_sorting_key() {
    let Some(s) = session().await else { return };
    let db = TempDb::create(s.as_ref(), http_port(), "edit").await;
    run(
        s.as_ref(),
        &format!(
            "CREATE TABLE {} (a UInt32, b String, n Nullable(String), v Decimal(10, 2), \
             t DateTime, g UInt32 MATERIALIZED a * 2, p UInt8) \
             ENGINE = MergeTree PARTITION BY p ORDER BY (a, b)",
            db.t("t")
        ),
    )
    .await;
    run(
        s.as_ref(),
        &format!(
            "INSERT INTO {} (a, b, n, v, t, p) VALUES (1,'x','one',1.50,'2024-01-01 00:00:00',1), \
             (2,'y',NULL,2.50,'2024-01-02 00:00:00',1), (3,'z','three',3.50,'2024-01-03 00:00:00',2)",
            db.t("t")
        ),
    )
    .await;
    let path = table(&db.name, "t");
    let ed = s.editability(&path).await.unwrap();
    assert!(ed.editable);
    assert_eq!(ed.key_columns, vec!["a", "b"]);
    assert!(ed.read_only_columns.contains(&"a".to_string()));
    assert!(
        ed.read_only_columns.contains(&"p".to_string()),
        "partition key is read-only"
    );
    assert!(
        ed.read_only_columns.contains(&"g".to_string()),
        "materialized column is read-only"
    );
    assert!(!ed.read_only_columns.contains(&"n".to_string()));
    assert!(!ed.atomic);
    assert_eq!(ed.notes, vec![EditNote::AsyncMutation]);

    let changes = rows(
        path.clone(),
        &["a", "b"],
        vec![
            RowChange::Update {
                key: vec![Value::Int(1), Value::Text("x".into())],
                set: vec![
                    cell("n", EditValue::Value(Value::Null)),
                    cell("v", EditValue::Text("9.99".into())),
                    cell(
                        "t",
                        EditValue::Value(Value::Timestamp("2025-06-07T08:09:10Z".into())),
                    ),
                ],
            },
            RowChange::Delete {
                key: vec![Value::Int(3), Value::Text("z".into())],
            },
            RowChange::Insert {
                values: vec![
                    cell("a", EditValue::Value(Value::Int(4))),
                    cell("b", EditValue::Text("it's".into())),
                    cell("n", EditValue::Default),
                    cell("v", EditValue::Text("4.25".into())),
                    cell(
                        "t",
                        EditValue::Value(Value::Timestamp("2024-04-04T04:04:04Z".into())),
                    ),
                    cell("p", EditValue::Value(Value::Int(3))),
                ],
            },
        ],
    );
    let preview = s.preview_changes(&changes).await.unwrap();
    assert_eq!(preview.len(), 3);
    assert!(preview[0].starts_with("DELETE FROM"), "{preview:?}");
    assert!(
        preview[1].starts_with("ALTER TABLE") && preview[1].contains(" UPDATE "),
        "{preview:?}"
    );
    assert!(preview[2].starts_with("INSERT INTO"));
    let result = s.apply_changes(changes).await.unwrap();
    assert_eq!(result.applied, 3);
    assert!(!result.atomic);
    let r = query(
        s.as_ref(),
        &format!("SELECT a, b, n, v, t FROM {} ORDER BY a", db.t("t")),
    )
    .await;
    assert_eq!(r.len(), 3, "row 3 deleted, row 4 inserted: {r:?}");
    assert_eq!(r[0][2], Value::Null);
    assert_eq!(r[0][3], Value::Decimal("9.99".into()));
    assert_eq!(r[0][4], Value::Timestamp("2025-06-07T08:09:10Z".into()));
    assert_eq!(r[2][1], Value::Text("it's".into()));
    assert_eq!(r[2][3], Value::Decimal("4.25".into()));
}

#[tokio::test]
async fn key_matching_several_rows_or_none_fails_without_changing_anything() {
    let Some(s) = session().await else { return };
    let db = TempDb::create(s.as_ref(), http_port(), "dup").await;
    run(
        s.as_ref(),
        &format!(
            "CREATE TABLE {} (a UInt32, b String) ENGINE = MergeTree ORDER BY a",
            db.t("t")
        ),
    )
    .await;
    run(
        s.as_ref(),
        &format!("INSERT INTO {} VALUES (1,'x'),(1,'y'),(2,'z')", db.t("t")),
    )
    .await;
    let path = table(&db.name, "t");
    // Two rows share the key.
    let err = s
        .apply_changes(rows(
            path.clone(),
            &["a"],
            vec![RowChange::Update {
                key: vec![Value::Int(1)],
                set: vec![cell("b", EditValue::Text("changed".into()))],
            }],
        ))
        .await
        .unwrap_err();
    match err {
        Error::ChangeFailed {
            index,
            message,
            applied,
        } => {
            assert_eq!((index, applied), (0, 0));
            assert_eq!(message, "key matches 2 rows");
        }
        other => panic!("{other:?}"),
    }
    // No row at all.
    let err = s
        .apply_changes(rows(
            path.clone(),
            &["a"],
            vec![RowChange::Delete {
                key: vec![Value::Int(99)],
            }],
        ))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::ChangeFailed { message, .. } if message == "key matches 0 rows"),
        "{err:?}"
    );
    assert_eq!(
        scalar(
            s.as_ref(),
            &format!("SELECT countIf(b = 'changed') FROM {}", db.t("t"))
        )
        .await,
        Value::UInt(0)
    );
    assert_eq!(
        scalar(s.as_ref(), &format!("SELECT count() FROM {}", db.t("t"))).await,
        Value::UInt(3)
    );
    // A failure in the middle reports how many changes were applied before it.
    let err = s
        .apply_changes(rows(
            path.clone(),
            &["a"],
            vec![
                RowChange::Delete {
                    key: vec![Value::Int(2)],
                },
                RowChange::Update {
                    key: vec![Value::Int(1)],
                    set: vec![cell("b", EditValue::Text("c".into()))],
                },
            ],
        ))
        .await
        .unwrap_err();
    assert!(
        matches!(
            &err,
            Error::ChangeFailed {
                index: 1,
                applied: 1,
                ..
            }
        ),
        "{err:?}"
    );
    assert_eq!(
        scalar(s.as_ref(), &format!("SELECT count() FROM {}", db.t("t"))).await,
        Value::UInt(2)
    );
}

#[tokio::test]
async fn other_engines_are_insert_only_or_not_editable() {
    let Some(s) = session().await else { return };
    let db = TempDb::create(s.as_ref(), http_port(), "engines").await;
    for (name, ddl) in [
        (
            "rep",
            "(k UInt32, v String) ENGINE = ReplacingMergeTree ORDER BY k",
        ),
        (
            "sum",
            "(k UInt32, c UInt64) ENGINE = SummingMergeTree ORDER BY k",
        ),
        ("mem", "(k UInt32) ENGINE = Memory"),
        ("log", "(k UInt32) ENGINE = Log"),
        ("nokey", "(k UInt32) ENGINE = MergeTree ORDER BY tuple()"),
    ] {
        run(s.as_ref(), &format!("CREATE TABLE {} {ddl}", db.t(name))).await;
    }
    run(
        s.as_ref(),
        &format!("CREATE VIEW {} AS SELECT 1 AS k", db.t("vw")),
    )
    .await;

    for name in ["rep", "sum", "nokey"] {
        let ed = s.editability(&table(&db.name, name)).await.unwrap();
        assert!(ed.editable, "{name}");
        assert!(ed.key_columns.is_empty(), "{name} is insert only");
    }
    for name in ["mem", "log"] {
        let ed = s.editability(&table(&db.name, name)).await.unwrap();
        assert!(!ed.editable, "{name}");
        assert_eq!(ed.reason, Some(NotEditableReason::Unsupported));
    }
    let ed = s
        .editability(
            &ObjectPath::root()
                .child(ObjectKind::Database, db.name.clone())
                .child(ObjectKind::View, "vw"),
        )
        .await
        .unwrap();
    assert_eq!(
        (ed.editable, ed.reason),
        (false, Some(NotEditableReason::NotATable))
    );

    // Inserts work on an insert-only table, updates and deletes are refused.
    let rep = table(&db.name, "rep");
    s.apply_changes(rows(
        rep.clone(),
        &[],
        vec![RowChange::Insert {
            values: vec![
                cell("k", EditValue::Value(Value::Int(1))),
                cell("v", EditValue::Text("a".into())),
            ],
        }],
    ))
    .await
    .unwrap();
    assert_eq!(
        scalar(s.as_ref(), &format!("SELECT count() FROM {}", db.t("rep"))).await,
        Value::UInt(1)
    );
    let err = s
        .apply_changes(rows(
            rep,
            &["k"],
            vec![RowChange::Delete {
                key: vec![Value::Int(1)],
            }],
        ))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Unsupported(_)), "{err:?}");
    // Memory tables are refused entirely.
    let err = s
        .apply_changes(rows(
            table(&db.name, "mem"),
            &[],
            vec![RowChange::Insert {
                values: vec![cell("k", EditValue::Value(Value::Int(1)))],
            }],
        ))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Unsupported(_)), "{err:?}");
}

#[tokio::test]
async fn insert_only_option_disables_updates_and_deletes() {
    let Some(s) = session().await else { return };
    let db = TempDb::create(s.as_ref(), http_port(), "insonly").await;
    run(
        s.as_ref(),
        &format!(
            "CREATE TABLE {} (a UInt32) ENGINE = MergeTree ORDER BY a",
            db.t("t")
        ),
    )
    .await;
    let mut cfg = config(http_port());
    cfg.options
        .insert("clickhouse.editing".into(), "insertOnly".into());
    let s2 = connect_cfg(&cfg).await.unwrap();
    let ed = s2.editability(&table(&db.name, "t")).await.unwrap();
    assert!(ed.editable && ed.key_columns.is_empty());
    cfg.options
        .insert("clickhouse.editing".into(), "sometimes".into());
    assert!(matches!(connect_err(&cfg).await, Error::Invalid(_)));
}

#[tokio::test]
async fn bad_values_are_reported_per_change() {
    let Some(s) = session().await else { return };
    let db = TempDb::create(s.as_ref(), http_port(), "badval").await;
    run(s.as_ref(), &format!("CREATE TABLE {} (a UInt32, b UInt8, g UInt32 MATERIALIZED a) ENGINE = MergeTree ORDER BY a", db.t("t"))).await;
    run(
        s.as_ref(),
        &format!("INSERT INTO {} (a, b) VALUES (1, 1)", db.t("t")),
    )
    .await;
    let path = table(&db.name, "t");
    for (change, why) in [
        (
            RowChange::Update {
                key: vec![Value::Int(1)],
                set: vec![cell("b", EditValue::Text("not a number".into()))],
            },
            "text in a number column",
        ),
        (
            RowChange::Update {
                key: vec![Value::Int(1)],
                set: vec![cell("a", EditValue::Value(Value::Int(5)))],
            },
            "sorting key column",
        ),
        (
            RowChange::Update {
                key: vec![Value::Int(1)],
                set: vec![cell("g", EditValue::Value(Value::Int(5)))],
            },
            "generated column",
        ),
        (
            RowChange::Insert {
                values: vec![cell("b", EditValue::Value(Value::Int(1000)))],
            },
            "out of range by the server",
        ),
    ] {
        let err = s
            .apply_changes(rows(path.clone(), &["a"], vec![change]))
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                Error::ChangeFailed {
                    index: 0,
                    applied: 0,
                    ..
                }
            ),
            "{why}: {err:?}"
        );
    }
    // Wrong key columns are invalid input.
    let err = s
        .apply_changes(rows(
            path,
            &["b"],
            vec![RowChange::Delete {
                key: vec![Value::Int(1)],
            }],
        ))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Invalid(_)), "{err:?}");
}
