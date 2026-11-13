//! Data editing integration tests (`preview_changes`, `apply_changes`, `editability`).
//! Only run with `DBSTUDIO_IT=1`.

mod common;

use common::*;
use dbstudio_core::{
    CellEdit, ChangeSet, EditValue, Error, NotEditableReason, ObjectKind, RowChange, RowChangeSet,
    Session, Value,
};

fn txt(c: &str, v: &str) -> CellEdit {
    CellEdit {
        column: c.into(),
        value: EditValue::Text(v.into()),
    }
}

fn set(es: &str, table: &str, changes: Vec<RowChange>) -> ChangeSet {
    ChangeSet::Rows(RowChangeSet {
        table: table_path(es, ObjectKind::Table, table),
        key_columns: vec!["id".into()],
        changes,
    })
}

async fn make(s: &dyn Session, es: &str, name: &str, extra: &str) {
    exec(s, &format!("CREATE SCHEMA IF NOT EXISTS {es}")).await;
    exec(
        s,
        &format!(
            "DROP TABLE IF EXISTS {es}.{name}; \
             CREATE TABLE {es}.{name} (id int PRIMARY KEY, a text, b text NOT NULL DEFAULT 'd', \
             n int{extra}); \
             INSERT INTO {es}.{name} (id, a, b, n) VALUES (1,'x','x',1),(2,'y','y',2),(3,'z','z',3),(4,'w','w',4)"
        ),
    )
    .await;
}

async fn snapshot(s: &dyn Session, es: &str, name: &str) -> Vec<Vec<Value>> {
    rows(
        s,
        &format!("SELECT id, a, b, n FROM {es}.{name} ORDER BY id"),
    )
    .await
}

async fn edit_three_plus_insert_delete(port: u16) {
    let s = fixture(port).await;
    let es = "dbs_edit_ok";
    make(&*s, es, "edit_ok", "").await;
    let changes = set(
        es,
        "edit_ok",
        vec![
            RowChange::Update {
                key: vec![Value::Int(1)],
                set: vec![
                    txt("a", "new1"),
                    CellEdit {
                        column: "b".into(),
                        value: EditValue::Text(String::new()),
                    },
                ],
            },
            RowChange::Update {
                key: vec![Value::Int(2)],
                set: vec![CellEdit {
                    column: "a".into(),
                    value: EditValue::Value(Value::Null),
                }],
            },
            RowChange::Update {
                key: vec![Value::Int(3)],
                set: vec![txt("n", "33")],
            },
            RowChange::Insert {
                values: vec![
                    txt("id", "10"),
                    txt("a", "it's"),
                    CellEdit {
                        column: "b".into(),
                        value: EditValue::Default,
                    },
                ],
            },
            RowChange::Delete {
                key: vec![Value::Int(4)],
            },
        ],
    );
    let preview = s.preview_changes(&changes).await.unwrap();
    assert_eq!(preview.len(), 5);
    assert!(preview[0].starts_with("DELETE FROM"), "{preview:?}");
    assert!(
        preview[1..4].iter().all(|p| p.starts_with("UPDATE")),
        "{preview:?}"
    );
    assert!(preview[4].starts_with("INSERT INTO"), "{preview:?}");
    assert!(preview[4].contains("'it''s'"));
    let r = s.apply_changes(changes).await.unwrap();
    assert_eq!(r.applied, 5);
    assert_eq!(r.affected, vec![1; 5]);
    assert!(r.atomic);
    let rows = snapshot(&*s, es, "edit_ok").await;
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0][1], Value::Text("new1".into()));
    assert_eq!(
        rows[0][2],
        Value::Text(String::new()),
        "empty string stays empty"
    );
    assert_eq!(rows[1][1], Value::Null);
    assert_eq!(rows[2][3], Value::Int(33));
    assert_eq!(rows[3][0], Value::Int(10));
    assert_eq!(rows[3][2], Value::Text("d".into()), "DEFAULT applied");
}

async fn failure_rolls_back(port: u16) {
    let s = fixture(port).await;
    let es = "dbs_edit_fail";
    make(&*s, es, "edit_fail", "").await;
    let before = snapshot(&*s, es, "edit_fail").await;
    let changes = set(
        es,
        "edit_fail",
        vec![
            RowChange::Update {
                key: vec![Value::Int(1)],
                set: vec![txt("a", "changed")],
            },
            // NOT NULL violation.
            RowChange::Update {
                key: vec![Value::Int(2)],
                set: vec![CellEdit {
                    column: "b".into(),
                    value: EditValue::Value(Value::Null),
                }],
            },
            RowChange::Insert {
                values: vec![txt("id", "11")],
            },
        ],
    );
    match s.apply_changes(changes).await {
        Err(Error::ChangeFailed { index, applied, .. }) => {
            assert_eq!(index, 1);
            assert_eq!(applied, 0);
        }
        other => panic!("expected ChangeFailed, got {other:?}"),
    }
    assert_eq!(snapshot(&*s, es, "edit_fail").await, before);
}

async fn zero_rows_rolls_back(port: u16) {
    let s = fixture(port).await;
    let es = "dbs_edit_zero";
    make(&*s, es, "edit_zero", "").await;
    let before = snapshot(&*s, es, "edit_zero").await;
    let changes = set(
        es,
        "edit_zero",
        vec![
            RowChange::Update {
                key: vec![Value::Int(1)],
                set: vec![txt("a", "changed")],
            },
            RowChange::Update {
                key: vec![Value::Int(999)],
                set: vec![txt("a", "nope")],
            },
        ],
    );
    match s.apply_changes(changes).await {
        Err(Error::ChangeFailed { index, .. }) => assert_eq!(index, 1),
        other => panic!("expected ChangeFailed, got {other:?}"),
    }
    assert_eq!(snapshot(&*s, es, "edit_zero").await, before);
    // A delete of a missing row is rejected the same way.
    let changes = set(
        es,
        "edit_zero",
        vec![RowChange::Delete {
            key: vec![Value::Int(999)],
        }],
    );
    assert!(matches!(
        s.apply_changes(changes).await,
        Err(Error::ChangeFailed { index: 0, .. })
    ));
}

async fn editability_cases(port: u16) {
    let s = fixture(port).await;
    let es = "dbs_edit_cases";
    make(
        &*s,
        es,
        "edit_pk",
        ", g int GENERATED ALWAYS AS (n * 2) STORED",
    )
    .await;
    let e = s
        .editability(&table_path(es, ObjectKind::Table, "edit_pk"))
        .await
        .unwrap();
    assert!(e.editable && e.atomic);
    assert_eq!(e.key_columns, vec!["id"]);
    assert!(e.read_only_columns.contains(&"g".to_string()), "{e:?}");
    // Writing a generated column is rejected before anything runs.
    let changes = set(
        es,
        "edit_pk",
        vec![RowChange::Update {
            key: vec![Value::Int(1)],
            set: vec![txt("g", "5")],
        }],
    );
    assert!(matches!(
        s.apply_changes(changes).await,
        Err(Error::ChangeFailed { index: 0, .. })
    ));

    exec(
        &*s,
        &format!(
            "DROP TABLE IF EXISTS {es}.edit_nokey; CREATE TABLE {es}.edit_nokey (a int, b text)"
        ),
    )
    .await;
    let e = s
        .editability(&table_path(es, ObjectKind::Table, "edit_nokey"))
        .await
        .unwrap();
    assert!(!e.editable);
    assert_eq!(e.reason, Some(NotEditableReason::NoUniqueKey));
    assert!(
        s.apply_changes(set(es, "edit_nokey", vec![]))
            .await
            .is_err()
    );

    exec(
        &*s,
        &format!(
            "DROP VIEW IF EXISTS {es}.edit_v; CREATE VIEW {es}.edit_v AS SELECT id FROM {es}.edit_pk"
        ),
    )
    .await;
    let e = s
        .editability(&table_path(es, ObjectKind::View, "edit_v"))
        .await
        .unwrap();
    assert_eq!(e.reason, Some(NotEditableReason::NotATable));
}

macro_rules! both {
    ($f:ident, $a:ident, $b:ident) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $a() {
            if enabled() {
                $f(PG16).await
            }
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $b() {
            if enabled() {
                $f(PG12).await
            }
        }
    };
}

both!(edit_three_plus_insert_delete, edit_ok_16, edit_ok_12);
both!(failure_rolls_back, edit_fail_16, edit_fail_12);
both!(zero_rows_rolls_back, edit_zero_16, edit_zero_12);
both!(editability_cases, edit_cases_16, edit_cases_12);
