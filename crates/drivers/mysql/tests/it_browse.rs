//! Object tree, DDL and table details. Run with `DBSTUDIO_IT=1`.

mod common;

use common::*;
use dbstudio_core::{Error, ObjectKind as K, ObjectNode, ObjectPath, Session, ValueKind, folder};

fn db() -> ObjectPath {
    ObjectPath::root().child(K::Database, FIXTURE_DB)
}

async fn kids(s: &dyn Session, p: &ObjectPath) -> Vec<ObjectNode> {
    s.list_children(p)
        .await
        .unwrap_or_else(|e| panic!("list_children failed: {e}"))
}

fn names(v: &[ObjectNode]) -> Vec<&str> {
    v.iter().map(|n| n.name.as_str()).collect()
}

#[tokio::test]
async fn tree_levels() {
    if !it_enabled() {
        return;
    }
    for t in TARGETS {
        ensure_fixture(t).await;
        let s = user_session(t).await;
        let tag = t.name;

        // Root: user databases first, system ones last and marked.
        let root = kids(&*s, &ObjectPath::root()).await;
        let pos = |n: &str| root.iter().position(|x| x.name == n);
        let fx = pos(FIXTURE_DB)
            .unwrap_or_else(|| panic!("[{tag}] fixture db missing: {:?}", names(&root)));
        let info = pos("information_schema").unwrap();
        assert!(fx < info, "[{tag}] user db before system dbs");
        assert_eq!(root[info].detail.as_deref(), Some("system"));
        assert!(root[fx].detail.is_none());
        assert!(root.iter().all(|n| n.kind == K::Database && n.has_children));
        let first_system = root.iter().position(|n| n.detail.is_some()).unwrap();
        assert!(
            root[first_system..].iter().all(|n| n.detail.is_some()),
            "[{tag}] system dbs are last"
        );

        // Database folders.
        let folders = kids(&*s, &db()).await;
        let mut fnames = names(&folders);
        fnames.sort();
        let mut expected = vec![
            folder::FUNCTIONS,
            folder::PROCEDURES,
            folder::TABLES,
            folder::VIEWS,
        ];
        expected.sort();
        assert_eq!(fnames, expected, "[{tag}]");
        assert!(
            folders
                .iter()
                .all(|f| f.kind == K::Folder && f.has_children)
        );

        // Tables.
        let tables = kids(&*s, &db().child(K::Folder, folder::TABLES)).await;
        let tn = names(&tables);
        for want in [
            "all_types",
            "big",
            "child",
            "parent",
            "people",
            "We`ird Tbl",
            "MixedCase",
        ] {
            assert!(tn.contains(&want), "[{tag}] missing table {want}: {tn:?}");
        }
        assert!(!tn.contains(&"v_child"));
        assert!(tables.iter().all(|n| n.kind == K::Table && n.has_children));
        // Canonical table path has no folder segment.
        assert_eq!(tables[0].path, db().child(K::Table, &tables[0].name));

        // Views, functions, procedures.
        let views = kids(&*s, &db().child(K::Folder, folder::VIEWS)).await;
        assert_eq!(names(&views), ["v_child"]);
        assert_eq!(views[0].kind, K::View);
        let funcs = kids(&*s, &db().child(K::Folder, folder::FUNCTIONS)).await;
        assert_eq!(names(&funcs), ["fn_add"]);
        assert_eq!(funcs[0].kind, K::Function);
        assert!(funcs[0].detail.as_deref().unwrap_or("").contains("int"));
        let procs = kids(&*s, &db().child(K::Folder, folder::PROCEDURES)).await;
        assert_eq!(names(&procs), ["pr_noop"]);
        assert_eq!(procs[0].kind, K::Procedure);

        // Table folders and children.
        let child = db().child(K::Table, "child");
        let tf = kids(&*s, &child).await;
        assert_eq!(names(&tf), ["columns", "indexes", "triggers"], "[{tag}]");
        assert!(tf.iter().all(|f| f.has_children));
        let cols = kids(&*s, &child.child(K::Folder, folder::COLUMNS)).await;
        assert_eq!(names(&cols), ["id", "parent_id", "note", "gen_len"]);
        assert!(cols.iter().all(|c| c.kind == K::Column && !c.has_children));
        assert_eq!(cols[0].path, child.child(K::Column, "id"));
        let idx = kids(&*s, &child.child(K::Folder, folder::INDEXES)).await;
        let mut inames = names(&idx);
        inames.sort();
        assert_eq!(inames, ["PRIMARY", "idx_note", "uq_child"]);
        assert!(idx.iter().all(|i| i.kind == K::Index));
        assert_eq!(idx[0].name, "PRIMARY", "primary key first");
        let trg = kids(&*s, &child.child(K::Folder, folder::TRIGGERS)).await;
        assert_eq!(names(&trg), ["trg_child_bi"]);
        assert_eq!(trg[0].kind, K::Trigger);
        assert_eq!(trg[0].detail.as_deref(), Some("BEFORE INSERT"));

        // Column detail is the full type.
        let at = db().child(K::Table, "all_types");
        let cols = kids(&*s, &at.child(K::Folder, folder::COLUMNS)).await;
        let detail = |n: &str| {
            cols.iter()
                .find(|c| c.name == n)
                .unwrap()
                .detail
                .clone()
                .unwrap()
        };
        assert_eq!(detail("c_decimal"), "decimal(38,10)");
        assert!(
            detail("u_bigint").starts_with("bigint") && detail("u_bigint").ends_with("unsigned")
        );
        assert_eq!(detail("c_varchar"), "varchar(50)");
        assert_eq!(detail("c_set"), "set('x','y','z')");

        // View: only columns.
        let v = db().child(K::View, "v_child");
        let vf = kids(&*s, &v).await;
        assert_eq!(names(&vf), ["columns"]);
        let vc = kids(&*s, &v.child(K::Folder, folder::COLUMNS)).await;
        assert_eq!(names(&vc), ["id", "note"]);

        // A table without triggers reports an empty trigger folder.
        let tf = kids(&*s, &db().child(K::Table, "people")).await;
        let trig = tf.iter().find(|f| f.name == "triggers").unwrap();
        assert!(!trig.has_children);

        // Weird names: mixed case, spaces and backticks.
        let weird = db().child(K::Table, "We`ird Tbl");
        let wc = kids(&*s, &weird.child(K::Folder, folder::COLUMNS)).await;
        assert_eq!(names(&wc), ["Id Col", "col`tick", "Mixed Case"]);
        let wi = kids(&*s, &weird.child(K::Folder, folder::INDEXES)).await;
        assert_eq!(names(&wi), ["PRIMARY"]);
        let mc = kids(
            &*s,
            &db()
                .child(K::Table, "MixedCase")
                .child(K::Folder, folder::COLUMNS),
        )
        .await;
        assert_eq!(names(&mc), ["Id"]);

        // Missing objects.
        assert!(matches!(
            s.list_children(&ObjectPath::root().child(K::Database, "no_such_db"))
                .await,
            Err(Error::NotFound(_))
        ));
        assert!(matches!(
            s.list_children(&db().child(K::Table, "nope")).await,
            Err(Error::NotFound(_))
        ));
    }
}

#[tokio::test]
async fn ddl_for_every_kind() {
    if !it_enabled() {
        return;
    }
    for t in TARGETS {
        ensure_fixture(t).await;
        let s = user_session(t).await;
        let tag = t.name;
        let ddl = |p: ObjectPath| {
            let s = s.clone();
            async move { s.object_ddl(&p).await }
        };
        let d = ddl(db().child(K::Table, "child")).await.unwrap();
        assert!(d.starts_with("CREATE TABLE `child`"), "[{tag}] {d}");
        assert!(d.contains("fk_child_parent"));
        let d = ddl(db().child(K::Table, "We`ird Tbl")).await.unwrap();
        assert!(
            d.contains("`We``ird Tbl`") && d.contains("`col``tick`"),
            "[{tag}] {d}"
        );
        let d = ddl(db().child(K::View, "v_child")).await.unwrap();
        assert!(d.contains("VIEW") && d.contains("v_child"), "[{tag}] {d}");
        let d = ddl(db().child(K::Function, "fn_add")).await.unwrap();
        assert!(
            d.contains("FUNCTION") && d.contains("fn_add"),
            "[{tag}] {d}"
        );
        let d = ddl(db().child(K::Procedure, "pr_noop")).await.unwrap();
        assert!(
            d.contains("PROCEDURE") && d.contains("pr_noop"),
            "[{tag}] {d}"
        );
        let child = db().child(K::Table, "child");
        let d = ddl(child.child(K::Trigger, "trg_child_bi")).await.unwrap();
        assert!(
            d.contains("TRIGGER") && d.contains("trg_child_bi"),
            "[{tag}] {d}"
        );
        assert_eq!(
            ddl(child.child(K::Index, "PRIMARY")).await.unwrap(),
            format!("ALTER TABLE `{FIXTURE_DB}`.`child` ADD PRIMARY KEY (`id`);")
        );
        assert_eq!(
            ddl(child.child(K::Index, "uq_child")).await.unwrap(),
            format!(
                "CREATE UNIQUE INDEX `uq_child` ON `{FIXTURE_DB}`.`child` (`parent_id`, `note`);"
            )
        );
        assert_eq!(
            ddl(child.child(K::Index, "idx_note")).await.unwrap(),
            format!("CREATE INDEX `idx_note` ON `{FIXTURE_DB}`.`child` (`note`);")
        );
        // Not found / unsupported.
        for p in [
            db().child(K::Table, "nope"),
            db().child(K::View, "nope"),
            db().child(K::Function, "nope"),
            db().child(K::Procedure, "nope"),
            child.child(K::Trigger, "nope"),
            child.child(K::Index, "nope"),
            ObjectPath::root()
                .child(K::Database, "dbstudio_it_none")
                .child(K::Table, "t"),
        ] {
            assert!(
                matches!(s.object_ddl(&p).await, Err(Error::NotFound(_))),
                "[{tag}] {p:?}"
            );
        }
        for p in [
            ObjectPath::root(),
            db(),
            db().child(K::Folder, folder::TABLES),
            child.child(K::Column, "id"),
        ] {
            assert!(
                matches!(s.object_ddl(&p).await, Err(Error::Unsupported(_))),
                "[{tag}] {p:?}"
            );
        }
    }
}

#[tokio::test]
async fn details_of_tables_and_views() {
    if !it_enabled() {
        return;
    }
    for t in TARGETS {
        ensure_fixture(t).await;
        let s = user_session(t).await;
        let tag = t.name;

        let d = s
            .table_details(&db().child(K::Table, "child"))
            .await
            .unwrap();
        assert_eq!(d.kind, K::Table);
        assert_eq!(d.path, db().child(K::Table, "child"));
        let id = &d.columns[0];
        assert_eq!(
            (id.name.as_str(), id.ordinal, id.primary_key, id.nullable),
            ("id", 1, true, false)
        );
        assert!(id.extra.as_deref().unwrap().contains("auto_increment"));
        assert_eq!(id.value_kind, ValueKind::Int);
        let note = d.columns.iter().find(|c| c.name == "note").unwrap();
        assert!(
            note.nullable && note.default.is_none() && !note.primary_key,
            "[{tag}] {note:?}"
        );
        assert_eq!(note.type_name, "varchar(20)");
        let gen_col = d.columns.iter().find(|c| c.name == "gen_len").unwrap();
        assert!(
            gen_col
                .extra
                .as_deref()
                .unwrap()
                .to_uppercase()
                .contains("GENERATED"),
            "[{tag}] {gen_col:?}"
        );
        let idx: Vec<_> = d.indexes.iter().map(|i| i.name.as_str()).collect();
        assert!(idx.contains(&"PRIMARY") && idx.contains(&"idx_note") && idx.contains(&"uq_child"));
        let pk = d.indexes.iter().find(|i| i.primary).unwrap();
        assert!(pk.unique && pk.columns == ["`id`"]);
        let uq = d.indexes.iter().find(|i| i.name == "uq_child").unwrap();
        assert!(uq.unique && !uq.primary && uq.columns == ["`parent_id`", "`note`"]);
        assert!(
            uq.definition
                .as_deref()
                .unwrap()
                .starts_with("CREATE UNIQUE INDEX")
        );
        assert_eq!(uq.method.as_deref(), Some("BTREE"));
        assert_eq!(d.foreign_keys.len(), 1);
        let fk = &d.foreign_keys[0];
        assert_eq!(fk.name, "fk_child_parent");
        assert_eq!(fk.columns, ["parent_id"]);
        assert_eq!(fk.referenced_columns, ["id"]);
        assert_eq!(fk.referenced_table, db().child(K::Table, "parent"));
        assert_eq!(fk.on_delete.as_deref(), Some("CASCADE"));
        assert_eq!(fk.on_update.as_deref(), Some("RESTRICT"));
        assert!(d.size_bytes.unwrap() > 0);
        assert!(d.estimated_rows.is_some());

        // Defaults and comments.
        let d = s
            .table_details(&db().child(K::Table, "parent"))
            .await
            .unwrap();
        let name = d.columns.iter().find(|c| c.name == "name").unwrap();
        assert_eq!(name.comment.as_deref(), Some("the name"));
        assert!(!name.nullable);
        assert!(
            name.default.as_deref().unwrap().contains("none"),
            "[{tag}] {:?}",
            name.default
        );

        // Table comment and the wide type table.
        let d = s
            .table_details(&db().child(K::Table, "all_types"))
            .await
            .unwrap();
        assert_eq!(d.comment.as_deref(), Some("every type"));
        let col = |n: &str| d.columns.iter().find(|c| c.name == n).unwrap();
        assert_eq!(col("u_bigint").value_kind, ValueKind::UInt);
        assert_eq!(col("c_decimal").value_kind, ValueKind::Decimal);
        assert_eq!(col("c_decimal").type_name, "decimal(38,10)");
        assert_eq!(col("c_blob").value_kind, ValueKind::Bytes);
        assert_eq!(col("c_datetime").type_name, "datetime(6)");

        // Weird names.
        let d = s
            .table_details(&db().child(K::Table, "We`ird Tbl"))
            .await
            .unwrap();
        assert_eq!(d.columns.len(), 3);
        assert_eq!(d.columns[1].name, "col`tick");

        // View.
        let d = s
            .table_details(&db().child(K::View, "v_child"))
            .await
            .unwrap();
        assert_eq!(d.kind, K::View);
        assert!(d.indexes.is_empty() && d.foreign_keys.is_empty());
        assert_eq!(d.columns.len(), 2);
        assert!(d.comment.is_none() && d.estimated_rows.is_none());

        // Errors.
        assert!(matches!(
            s.table_details(&db().child(K::Table, "nope")).await,
            Err(Error::NotFound(_))
        ));
        assert!(s.table_details(&ObjectPath::root()).await.is_err());
    }
}
