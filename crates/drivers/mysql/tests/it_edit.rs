//! Data editing: preview, apply (one transaction), editability. Run with `DBSTUDIO_IT=1`.

mod common;

use common::*;
use dbstudio_core::{
    CellEdit, ChangeSet, EditValue, Error, NotEditableReason, ObjectKind, ObjectPath, RowChange,
    RowChangeSet, Session, Value,
};

fn txt(c: &str, v: &str) -> CellEdit {
    CellEdit {
        column: c.into(),
        value: EditValue::Text(v.into()),
    }
}

fn path(db: &str, table: &str) -> ObjectPath {
    ObjectPath::root()
        .child(ObjectKind::Database, db)
        .child(ObjectKind::Table, table)
}

fn set(db: &str, table: &str, changes: Vec<RowChange>) -> ChangeSet {
    ChangeSet::Rows(RowChangeSet {
        table: path(db, table),
        key_columns: vec!["id".into()],
        changes,
    })
}

/// Fresh scratch database with table `t` (4 rows) and a session on it.
async fn setup(t: Target, name: &str, extra: &str) -> (std::sync::Arc<dyn Session>, ScratchDb) {
    ensure_fixture(t).await;
    let db = scratch_db(t, name).await;
    let s = user_session(t).await;
    run(
        &*s,
        &format!(
            "CREATE TABLE `{db}`.t (id INT PRIMARY KEY, a VARCHAR(20), b VARCHAR(20) NOT NULL DEFAULT 'd', \
             n INT, ts DATETIME NULL DEFAULT CURRENT_TIMESTAMP{extra}); \
             INSERT INTO `{db}`.t (id, a, b, n) VALUES (1,'x','x',1),(2,'y','y',2),(3,'z','z',3),(4,'w','w',4)"
        ),
    )
    .await;
    (s, db)
}

async fn snapshot(s: &dyn Session, db: &str) -> Vec<Vec<Value>> {
    query(s, &format!("SELECT id, a, b, n FROM `{db}`.t ORDER BY id"))
        .await
        .rows
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_updates_insert_delete_in_order() {
    if !it_enabled() {
        return;
    }
    for t in targets() {
        let (s, db) = setup(t, "edit_ok", "").await;
        let changes = set(
            &db,
            "t",
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
                // Same value as stored: MySQL reports 0 changed rows, but 1 matched.
                RowChange::Update {
                    key: vec![Value::Int(3)],
                    set: vec![txt("n", "3")],
                },
                RowChange::Insert {
                    values: vec![
                        txt("id", "10"),
                        txt("a", "it's \\ here"),
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
        assert_eq!(preview.len(), 5, "[{}]", t.name);
        assert!(preview[0].starts_with("DELETE FROM"), "{preview:?}");
        assert!(
            preview[1..4].iter().all(|p| p.starts_with("UPDATE")),
            "{preview:?}"
        );
        assert!(preview[4].starts_with("INSERT INTO"), "{preview:?}");
        let r = s
            .apply_changes(changes)
            .await
            .unwrap_or_else(|e| panic!("[{}] {e}", t.name));
        assert_eq!(r.applied, 5);
        assert_eq!(r.affected, vec![1; 5], "[{}]", t.name);
        let rows = snapshot(&*s, &db).await;
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0][1], Value::Text("new1".into()));
        assert_eq!(rows[0][2], Value::Text(String::new()));
        assert_eq!(rows[1][1], Value::Null);
        assert_eq!(rows[3][0], Value::Int(10));
        assert_eq!(rows[3][1], Value::Text("it's \\ here".into()));
        assert_eq!(rows[3][2], Value::Text("d".into()));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failure_and_zero_rows_roll_back() {
    if !it_enabled() {
        return;
    }
    for t in targets() {
        let (s, db) = setup(t, "edit_fail", "").await;
        let before = snapshot(&*s, &db).await;
        // Failure at change 1 (NOT NULL violation): everything is undone.
        let changes = set(
            &db,
            "t",
            vec![
                RowChange::Update {
                    key: vec![Value::Int(1)],
                    set: vec![txt("a", "changed")],
                },
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
                assert_eq!((index, applied), (1, 0), "[{}]", t.name)
            }
            other => panic!("[{}] expected ChangeFailed, got {other:?}", t.name),
        }
        assert_eq!(snapshot(&*s, &db).await, before, "[{}]", t.name);
        // An UPDATE that matches no row rolls back too.
        let changes = set(
            &db,
            "t",
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
            Err(Error::ChangeFailed { index, .. }) => assert_eq!(index, 1, "[{}]", t.name),
            other => panic!("[{}] expected ChangeFailed, got {other:?}", t.name),
        }
        assert_eq!(snapshot(&*s, &db).await, before, "[{}]", t.name);
        let changes = set(
            &db,
            "t",
            vec![RowChange::Delete {
                key: vec![Value::Int(999)],
            }],
        );
        assert!(matches!(
            s.apply_changes(changes).await,
            Err(Error::ChangeFailed { index: 0, .. })
        ));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn editability_cases() {
    if !it_enabled() {
        return;
    }
    for t in targets() {
        let (s, db) = setup(
            t,
            "edit_cases",
            ", g INT GENERATED ALWAYS AS (n * 2) VIRTUAL",
        )
        .await;
        let e = s.editability(&path(&db, "t")).await.unwrap();
        assert!(e.editable && e.atomic, "[{}] {e:?}", t.name);
        assert_eq!(e.key_columns, vec!["id"]);
        assert!(
            e.read_only_columns.contains(&"g".to_string()),
            "[{}] {e:?}",
            t.name
        );
        let changes = set(
            &db,
            "t",
            vec![RowChange::Update {
                key: vec![Value::Int(1)],
                set: vec![txt("g", "5")],
            }],
        );
        assert!(matches!(
            s.apply_changes(changes).await,
            Err(Error::ChangeFailed { index: 0, .. })
        ));

        run(
            &*s,
            &format!("CREATE TABLE `{db}`.nokey (a INT, b VARCHAR(5))"),
        )
        .await;
        let e = s.editability(&path(&db, "nokey")).await.unwrap();
        assert!(!e.editable);
        assert_eq!(
            e.reason,
            Some(NotEditableReason::NoUniqueKey),
            "[{}]",
            t.name
        );
        assert!(s.apply_changes(set(&db, "nokey", vec![])).await.is_err());

        run(
            &*s,
            &format!("CREATE VIEW `{db}`.v AS SELECT id FROM `{db}`.t"),
        )
        .await;
        let vp = ObjectPath::root()
            .child(ObjectKind::Database, db.as_str())
            .child(ObjectKind::View, "v");
        let e = s.editability(&vp).await.unwrap();
        assert_eq!(e.reason, Some(NotEditableReason::NotATable), "[{}]", t.name);
    }
}
