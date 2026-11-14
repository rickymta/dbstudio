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
    for t in targets() {
        ensure_fixture(t).await;
        let s = user_session(t).await;
        let tag = t.name;

        // Root: user databases first, system ones last and marked.
        let mut root = kids(&*s, &ObjectPath::root()).await;
        if t.tidb {
            // TiDB lets this login read `mysql.user` when it connects through the published
            // port, so the `roles` folder is offered; the tree test looks at databases only.
            root.retain(|n| n.kind == K::Database);
        }
        // (TiDB spells its system databases in upper case.)
        let pos = |n: &str| root.iter().position(|x| x.name.eq_ignore_ascii_case(n));
        let fx = pos(FIXTURE_DB)
            .unwrap_or_else(|| panic!("[{tag}] fixture db missing: {:?}", names(&root)));
        let info = pos("information_schema").unwrap();
        assert!(fx < info, "[{tag}] user db before system dbs");
        assert_eq!(root[info].detail.as_deref(), Some("system"));
        assert!(root[fx].detail.is_none());
        assert!(
            root.iter()
                .all(|n| n.kind == K::Database && n.has_children),
            "[{tag}] {:?}",
            names(&root)
        );
        let first_system = root.iter().position(|n| n.detail.is_some()).unwrap();
        assert!(
            root[first_system..].iter().all(|n| n.detail.is_some()),
            "[{tag}] system dbs are last"
        );

        // Database folders.
        let folders = kids(&*s, &db()).await;
        let mut fnames = names(&folders);
        fnames.sort();
        // TiDB has no stored routines: those folders are hidden.
        let mut expected = if t.tidb {
            vec![folder::TABLES, folder::VIEWS]
        } else {
            vec![
                folder::FUNCTIONS,
                folder::PROCEDURES,
                folder::TABLES,
                folder::VIEWS,
            ]
        };
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
        // A node's path is its parent's path plus its own segment.
        assert_eq!(
            tables[0].path,
            db().child(K::Folder, folder::TABLES)
                .child(K::Table, &tables[0].name)
        );

        // Views, functions, procedures.
        let views = kids(&*s, &db().child(K::Folder, folder::VIEWS)).await;
        assert_eq!(names(&views), ["v_child"]);
        assert_eq!(views[0].kind, K::View);
        if !t.tidb {
            let funcs = kids(&*s, &db().child(K::Folder, folder::FUNCTIONS)).await;
            assert_eq!(names(&funcs), ["fn_add"]);
            assert_eq!(funcs[0].kind, K::Function);
            assert_eq!(
                funcs[0].path,
                db().child(K::Folder, folder::FUNCTIONS)
                    .child(K::Function, "fn_add")
            );
            assert_eq!(
                views[0].path,
                db().child(K::Folder, folder::VIEWS)
                    .child(K::View, "v_child")
            );
            assert!(funcs[0].detail.as_deref().unwrap_or("").contains("int"));
            let procs = kids(&*s, &db().child(K::Folder, folder::PROCEDURES)).await;
            assert_eq!(names(&procs), ["pr_noop"]);
            assert_eq!(procs[0].kind, K::Procedure);
        }

        // Table folders and children.
        let child = db()
            .child(K::Folder, folder::TABLES)
            .child(K::Table, "child");
        let tf = kids(&*s, &child).await;
        if t.tidb {
            assert_eq!(names(&tf), ["columns", "indexes"], "[{tag}]");
        } else {
            assert_eq!(names(&tf), ["columns", "indexes", "triggers"], "[{tag}]");
        }
        assert!(tf.iter().all(|f| f.has_children));
        let cols = kids(&*s, &child.child(K::Folder, folder::COLUMNS)).await;
        assert_eq!(names(&cols), ["id", "parent_id", "note", "gen_len"]);
        assert!(cols.iter().all(|c| c.kind == K::Column && !c.has_children));
        assert_eq!(
            cols[0].path,
            child
                .child(K::Folder, folder::COLUMNS)
                .child(K::Column, "id")
        );
        let idx = kids(&*s, &child.child(K::Folder, folder::INDEXES)).await;
        let mut inames = names(&idx);
        inames.sort();
        assert_eq!(inames, ["PRIMARY", "idx_note", "uq_child"]);
        assert!(idx.iter().all(|i| i.kind == K::Index));
        assert_eq!(idx[0].name, "PRIMARY", "primary key first");
        if !t.tidb {
            let trg = kids(&*s, &child.child(K::Folder, folder::TRIGGERS)).await;
            assert_eq!(names(&trg), ["trg_child_bi"]);
            assert_eq!(trg[0].kind, K::Trigger);
            assert_eq!(trg[0].detail.as_deref(), Some("BEFORE INSERT"));
        }

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
        if !t.tidb {
            let trig = tf.iter().find(|f| f.name == "triggers").unwrap();
            assert!(!trig.has_children);
        }

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
    for t in targets() {
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
        let child = db().child(K::Table, "child");
        if !t.tidb {
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
            let d = ddl(child.child(K::Trigger, "trg_child_bi")).await.unwrap();
            assert!(
                d.contains("TRIGGER") && d.contains("trg_child_bi"),
                "[{tag}] {d}"
            );
        }
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
    for t in targets() {
        ensure_fixture(t).await;
        let s = user_session(t).await;
        let tag = t.name;

        let d = s
            .table_details(&db().child(K::Table, "child"))
            .await
            .unwrap();
        assert_eq!(d.kind, K::Table);
        assert_eq!(d.path, db().child(K::Table, "child"));
        let folded = db()
            .child(K::Folder, folder::TABLES)
            .child(K::Table, "child");
        assert_eq!(s.table_details(&folded).await.unwrap().path, folded);
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
        assert!(pk.unique && pk.columns == ["id"]);
        let uq = d.indexes.iter().find(|i| i.name == "uq_child").unwrap();
        assert!(uq.unique && !uq.primary && uq.columns == ["parent_id", "note"]);
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
        assert_eq!(
            fk.referenced_table,
            db().child(K::Folder, folder::TABLES)
                .child(K::Table, "parent")
        );
        assert_eq!(fk.on_delete.as_deref(), Some("CASCADE"));
        assert_eq!(fk.on_update.as_deref(), Some("RESTRICT"));
        // TiDB reports table sizes only from statistics, which a fresh fixture lacks.
        if !t.tidb {
            assert!(d.size_bytes.unwrap() > 0);
        }
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

#[tokio::test]
async fn roles_folder_depends_on_privileges() {
    if !it_enabled() {
        return;
    }
    for t in targets() {
        let tag = t.name;
        let root = root_session(t).await;
        let top = kids(&*root, &ObjectPath::root()).await;
        let folder_node = top
            .iter()
            .find(|n| n.kind == K::Folder && n.name == folder::ROLES)
            .unwrap_or_else(|| panic!("[{tag}] roles folder missing"));
        assert_eq!(
            folder_node.path,
            ObjectPath::root().child(K::Folder, folder::ROLES)
        );
        assert!(
            top.last().unwrap().kind == K::Folder,
            "databases first, folder last"
        );
        let roles = kids(&*root, &folder_node.path).await;
        let r = roles
            .iter()
            .find(|n| n.name == "'root'@'%'")
            .unwrap_or_else(|| panic!("[{tag}] {:?}", names(&roles)));
        assert_eq!(r.kind, K::Role);
        assert_eq!(r.path, folder_node.path.child(K::Role, "'root'@'%'"));
        assert!(!r.has_children);
        assert!(roles.iter().any(|n| n.name == "'dbstudio'@'%'"));
        // Without privilege the folder is silently absent.
        if t.tidb {
            // See `tree_levels`: TiDB does not hide `mysql.user` from this login.
            continue;
        }
        let s = user_session(t).await;
        let top = kids(&*s, &ObjectPath::root()).await;
        assert!(!top.iter().any(|n| n.name == folder::ROLES), "[{tag}]");
    }
}

fn search_req(query: &str, scope: ObjectPath) -> dbstudio_core::ObjectSearchRequest {
    dbstudio_core::ObjectSearchRequest {
        query: query.into(),
        scope,
        kinds: vec![],
        limit: 100,
    }
}

#[tokio::test]
async fn server_side_object_search() {
    if !it_enabled() {
        return;
    }
    for t in targets() {
        let tag = t.name;
        ensure_fixture(t).await;
        let db1 = scratch_db(t, "search1").await;
        let db2 = scratch_db(t, "search2").await;
        let root = root_session(t).await;
        run(
            &*root,
            &format!(
                "CREATE TABLE `{db1}`.`user` (a INT); CREATE TABLE `{db1}`.`users` (a INT);
                 CREATE TABLE `{db1}`.`super_user` (a INT); CREATE TABLE `{db1}`.`xuserx` (a INT);
                 CREATE TABLE `{db1}`.`a_b` (a INT); CREATE TABLE `{db1}`.`axb` (a INT);
                 CREATE TABLE `{db1}`.`100%` (a INT); CREATE TABLE `{db1}`.`it's` (a INT);
                 CREATE TABLE `{db1}`.`back\\slash` (a INT);
                 CREATE VIEW `{db1}`.`user_view` AS SELECT 1 AS a;
                 CREATE TABLE `{db2}`.`UserTwo` (a INT);"
            ),
        )
        .await;
        // TiDB has no stored routines.
        if !t.tidb {
            for sql in [
                format!(
                    "CREATE DEFINER='dbstudio'@'%' FUNCTION `{db1}`.user_fn() RETURNS INT DETERMINISTIC RETURN 1"
                ),
                format!("CREATE DEFINER='dbstudio'@'%' PROCEDURE `{db1}`.user_proc() SELECT 1"),
            ] {
                run(&*root, &sql).await;
            }
        }
        root.close().await.unwrap();
        let s = user_session(t).await;
        let search = |q: &str, scope: ObjectPath| {
            let s = s.clone();
            let req = search_req(q, scope);
            async move { s.search_objects(req).await.unwrap() }
        };
        let in_db1 = ObjectPath::root().child(K::Database, db1.as_str());

        // Ranking: exact, prefix, substring; then by name.
        let r = search("user", in_db1.clone()).await;
        assert_eq!(
            names(&r),
            if t.tidb {
                vec!["user", "user_view", "users", "super_user", "xuserx"]
            } else {
                vec![
                    "user",
                    "user_fn",
                    "user_proc",
                    "user_view",
                    "users",
                    "super_user",
                    "xuserx",
                ]
            },
            "[{tag}]"
        );
        // Case-insensitive, and the scope may be written with a folder segment.
        let upper = search("USER", in_db1.child(K::Folder, folder::TABLES)).await;
        assert_eq!(names(&upper), names(&r), "[{tag}]");
        // Whole-server search covers both scratch databases and hides system databases.
        let all = search("user", ObjectPath::root()).await;
        assert!(
            all.iter()
                .any(|n| n.name == "UserTwo" && n.detail.as_deref() == Some(db2.as_str()))
        );
        assert!(
            all.iter()
                .filter(|n| n.detail.as_deref() == Some(db1.as_str()))
                .count()
                >= if t.tidb { 5 } else { 7 }
        );
        for n in &all {
            let d = n.detail.as_deref().unwrap();
            assert!(
                !["information_schema", "mysql", "performance_schema", "sys"].contains(&d),
                "[{tag}] {d}"
            );
        }
        let only2 = search("user", ObjectPath::root().child(K::Database, db2.as_str())).await;
        assert_eq!(names(&only2), ["UserTwo"]);

        // %, _, \ and ' are ordinary characters.
        assert_eq!(
            names(&search("_", in_db1.clone()).await),
            if t.tidb {
                vec!["a_b", "super_user", "user_view"]
            } else {
                vec!["a_b", "super_user", "user_fn", "user_proc", "user_view"]
            }
        );
        assert_eq!(names(&search("%", in_db1.clone()).await), ["100%"]);
        assert_eq!(names(&search("'", in_db1.clone()).await), ["it's"]);
        assert_eq!(names(&search("\\", in_db1.clone()).await), ["back\\slash"]);
        assert!(
            search("x'; DROP TABLE x; --", in_db1.clone())
                .await
                .is_empty()
        );
        assert!(search("", in_db1.clone()).await.is_empty());

        // Kinds, limit.
        let mut req = search_req("user", in_db1.clone());
        req.kinds = vec![K::View];
        assert_eq!(
            names(&s.search_objects(req.clone()).await.unwrap()),
            ["user_view"]
        );
        req.kinds = vec![K::Function, K::Procedure];
        assert_eq!(
            names(&s.search_objects(req.clone()).await.unwrap()),
            if t.tidb {
                vec![]
            } else {
                vec!["user_fn", "user_proc"]
            }
        );
        req.kinds = vec![K::MaterializedView, K::Sequence];
        assert!(s.search_objects(req.clone()).await.unwrap().is_empty());
        req.kinds = vec![];
        req.limit = 3;
        assert_eq!(
            names(&s.search_objects(req.clone()).await.unwrap()),
            if t.tidb {
                vec!["user", "user_view", "users"]
            } else {
                vec!["user", "user_fn", "user_proc"]
            }
        );
        req.limit = 0;
        assert_eq!(
            names(&s.search_objects(req.clone()).await.unwrap()),
            ["user"],
            "limit clamps to 1"
        );
        req.limit = 1_000_000;
        assert_eq!(
            s.search_objects(req).await.unwrap().len(),
            if t.tidb { 5 } else { 7 }
        );
        let mut req = search_req("e", ObjectPath::root().child(K::Database, FIXTURE_DB));
        req.limit = 1000;
        assert!(s.search_objects(req).await.unwrap().len() >= 5);

        // Paths follow the tree convention, match list_children, and work with the other calls.
        for n in search("user", in_db1.clone())
            .await
            .iter()
            .chain(all.iter())
        {
            let mut parent = n.path.clone();
            parent.segments.pop();
            let siblings = kids(&*s, &parent).await;
            let same = siblings.iter().find(|x| x.path == n.path);
            let same =
                same.unwrap_or_else(|| panic!("[{tag}] {:?} not listed under its parent", n.path));
            assert_eq!(
                (same.kind, &same.name, same.has_children),
                (n.kind, &n.name, n.has_children)
            );
            s.object_ddl(&n.path)
                .await
                .unwrap_or_else(|e| panic!("[{tag}] ddl {:?}: {e}", n.path));
            if matches!(n.kind, K::Table | K::View) {
                let d = s.table_details(&n.path).await.unwrap();
                assert_eq!(d.path, n.path);
            }
        }
        let tables = search("people", ObjectPath::root().child(K::Database, FIXTURE_DB)).await;
        assert_eq!(
            tables[0].path,
            db().child(K::Folder, folder::TABLES)
                .child(K::Table, "people")
        );
    }
}

#[tokio::test]
async fn search_limit_applies_after_kind_filter_and_edge_cases() {
    if !it_enabled() {
        return;
    }
    for t in targets() {
        let tag = t.name;
        ensure_fixture(t).await;
        let dbn = scratch_db(t, "search3").await;
        let root = root_session(t).await;
        let mut sql = String::new();
        for i in 0..12 {
            sql.push_str(&format!("CREATE TABLE `{dbn}`.`zz_t{i:02}` (a INT);"));
        }
        sql.push_str(&format!("CREATE VIEW `{dbn}`.`zz_view` AS SELECT 1 AS a"));
        run(&*root, &sql).await;
        root.close().await.unwrap();
        let s = user_session(t).await;
        let scope = ObjectPath::root().child(K::Database, dbn.as_str());
        let mut req = search_req("zz", scope.clone());
        req.limit = 10;
        // More than `limit` tables rank before the view; the kind filter must come first.
        req.kinds = vec![K::View];
        assert_eq!(
            names(&s.search_objects(req.clone()).await.unwrap()),
            ["zz_view"],
            "[{tag}]"
        );
        req.kinds = vec![K::Table];
        let tables = s.search_objects(req.clone()).await.unwrap();
        assert_eq!(tables.len(), 10);
        assert!(tables.iter().all(|n| n.kind == K::Table));
        req.kinds = vec![];
        assert_eq!(s.search_objects(req.clone()).await.unwrap().len(), 10);
        // Only inapplicable kinds: nothing, and the filter is not dropped.
        req.kinds = vec![K::Sequence];
        assert!(s.search_objects(req.clone()).await.unwrap().is_empty());
        // Whitespace-only queries are empty; surrounding whitespace is ignored.
        assert!(
            s.search_objects(search_req("   ", scope.clone()))
                .await
                .unwrap()
                .is_empty()
        );
        let r = s
            .search_objects(search_req("  zz_view ", scope.clone()))
            .await
            .unwrap();
        assert_eq!(names(&r), ["zz_view"]);
        // A table (or deeper) is not a valid scope.
        let bad = scope.child(K::Table, "zz_t00");
        assert!(matches!(
            s.search_objects(search_req("zz", bad.clone())).await,
            Err(Error::NotFound(_))
        ));
        assert!(matches!(
            s.search_objects(search_req("zz", bad.child(K::Folder, folder::COLUMNS)))
                .await,
            Err(Error::NotFound(_))
        ));
    }
}

#[tokio::test]
async fn search_has_a_time_limit() {
    if !it_enabled() {
        return;
    }
    for t in targets() {
        let mut c = config(t, "dbstudio", dbstudio_core::TlsMode::Disable);
        c.options.insert("search_timeout_ms".into(), "400".into());
        c.options
            .insert("debug_search_sleep_ms".into(), "20000".into());
        c.options.insert("pool_max".into(), "1".into());
        let s = connect_cfg(&c).await.unwrap();
        let started = std::time::Instant::now();
        let r = s
            .search_objects(search_req("user", ObjectPath::root()))
            .await;
        assert!(matches!(r, Err(Error::Timeout)), "[{}] {r:?}", t.name);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "[{}]",
            t.name
        );
        // The only pooled connection is healthy again.
        s.ping().await.unwrap();
        assert_eq!(query(&*s, "SELECT 1").await.rows.len(), 1);
    }
}
