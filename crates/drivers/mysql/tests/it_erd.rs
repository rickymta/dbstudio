//! Integration tests of `schema_model` and `generate_ddl` on MySQL / MariaDB (`DBSTUDIO_IT=1`).
//! A Sakila-style fixture lives in database `erd_it`; the DDL generated from it is replayed into
//! the empty database `erd_it_copy` and the two models are compared.

mod common;

use common::*;
use dbstudio_core::{
    DdlItem, DdlOptions, DdlRequest, DdlSection, ModelDetail, ObjectKind as K, ObjectPath,
    QueryRequest, SchemaModel, SchemaModelRequest, Session, execute_collect,
};
use dbstudio_driver_mysql::model::count_catalog_queries;

const FIXTURE: &str = include_str!("fixtures/erd_sakila.sql");

fn db_path(db: &str) -> ObjectPath {
    ObjectPath::root().child(K::Database, db)
}

fn table_in(db: &str, name: &str) -> ObjectPath {
    db_path(db).child(K::Folder, "tables").child(K::Table, name)
}

async fn run_in(s: &dyn Session, db: &str, sql: &str) {
    let mut req = QueryRequest::new(sql);
    req.database = Some(db.to_string());
    if let Err(e) = execute_collect(s, req).await {
        panic!("sql failed in {db}: {e}\n{sql}");
    }
}

async fn recreate_db(root: &dyn Session, db: &str) {
    run(
        root,
        &format!("DROP DATABASE IF EXISTS `{db}`; CREATE DATABASE `{db}` CHARACTER SET utf8mb4"),
    )
    .await;
}

/// Fixture database `erd_it`, recreated for each server and test (tests are sequential per
/// server but run in parallel across test functions, so every test owns its databases).
async fn fixture_db(root: &dyn Session, db: &str) {
    recreate_db(root, db).await;
    run_in(root, db, FIXTURE).await;
}

fn request(scope: ObjectPath) -> SchemaModelRequest {
    SchemaModelRequest {
        scope,
        tables: vec![],
        include_views: true,
        include_comments: true,
        detail: ModelDetail::Full,
        sample_size: None,
    }
}

/// Path without database and folder segments.
fn key(p: &ObjectPath) -> String {
    p.segments
        .iter()
        .filter(|s| !matches!(s.kind, K::Database | K::Folder))
        .map(|s| format!("{:?}:{}", s.kind, s.name))
        .collect::<Vec<_>>()
        .join("/")
}

fn summary(m: &SchemaModel) -> (Vec<String>, Vec<String>) {
    let tables = m
        .tables
        .iter()
        .map(|t| {
            let cols: Vec<String> = t
                .columns
                .iter()
                .map(|c| {
                    format!(
                        "{}|{}|{}|{}|{:?}|{:?}|{:?}|{}",
                        c.ordinal,
                        c.name,
                        c.type_name,
                        c.nullable,
                        c.default,
                        c.extra,
                        c.comment,
                        c.primary_key
                    )
                })
                .collect();
            format!(
                "{} {:?} {:?} pk={:?} uq={:?} [{}]",
                key(&t.path),
                t.kind,
                t.comment,
                t.primary_key,
                t.unique_keys,
                cols.join("; ")
            )
        })
        .collect();
    let rels = m
        .relations
        .iter()
        .map(|r| {
            format!(
                "{} {}{:?} -> {}{:?} {:?} {:?} opt={} 11={}",
                r.name,
                key(&r.from_table),
                r.from_columns,
                key(&r.to_table),
                r.to_columns,
                r.on_update,
                r.on_delete,
                r.from_optional,
                r.one_to_one
            )
        })
        .collect();
    (tables, rels)
}

fn find_rel<'a>(
    m: &'a SchemaModel,
    from: &str,
    cols: &[&str],
    to: &str,
) -> &'a dbstudio_core::Relation {
    m.relations
        .iter()
        .find(|r| {
            r.from_table.last().unwrap().name == from
                && r.to_table.last().unwrap().name == to
                && r.from_columns == cols
        })
        .unwrap_or_else(|| panic!("relation {from}{cols:?} -> {to} not found"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn model_sakila() {
    if !it_enabled() {
        return;
    }
    for t in targets() {
        let tag = t.name;
        let root = root_session(t).await;
        fixture_db(&*root, "erd_it_model").await;
        let m = root
            .schema_model(request(db_path("erd_it_model")))
            .await
            .unwrap();
        assert!(!m.inferred && !m.truncated);
        let tables = m.tables.iter().filter(|x| x.kind == K::Table).count();
        assert_eq!(tables, 23, "[{tag}]");
        assert_eq!(m.tables.len(), 26, "[{tag}] tables + 3 views");
        assert_eq!(m.relations.len(), 32, "[{tag}]");

        let film = m.tables.iter().find(|x| x.name == "film").unwrap();
        assert_eq!(film.schema, None);
        assert_eq!(film.primary_key, vec!["film_id"], "[{tag}]");
        assert_eq!(film.comment.as_deref(), Some("Films; one row per title"));
        assert_eq!(film.columns.len(), 13, "[{tag}]");
        assert!(film.columns[0].primary_key && !film.columns[0].nullable);
        assert_eq!(film.columns[0].extra.as_deref(), Some("auto_increment"));
        let rating = film.columns.iter().find(|c| c.name == "rating").unwrap();
        assert_eq!(rating.comment.as_deref(), Some("MPAA rating"));
        let fa = m.tables.iter().find(|x| x.name == "film_actor").unwrap();
        assert_eq!(fa.primary_key, vec!["actor_id", "film_id"], "[{tag}]");
        let actor = m.tables.iter().find(|x| x.name == "actor").unwrap();
        assert!(
            actor
                .unique_keys
                .contains(&vec!["first_name".to_string(), "last_name".to_string()])
        );

        let r = find_rel(&m, "film", &["original_language_id"], "language");
        assert!(r.from_optional && !r.one_to_one, "[{tag}]");
        assert_eq!(r.on_delete.as_deref(), Some("RESTRICT"));
        assert_eq!(r.on_update.as_deref(), Some("CASCADE"));
        let r = find_rel(&m, "film", &["language_id"], "language");
        assert!(!r.from_optional && !r.one_to_one);
        let r = find_rel(&m, "film_text", &["film_id"], "film");
        assert!(!r.from_optional && r.one_to_one);
        assert_eq!(r.on_delete.as_deref(), Some("CASCADE"));
        assert!(find_rel(&m, "staff_credentials", &["staff_id"], "staff").one_to_one);
        assert!(find_rel(&m, "store", &["manager_staff_id"], "staff").one_to_one);
        assert!(find_rel(&m, "customer_profile", &["customer_id"], "customer").one_to_one);
        let r = find_rel(&m, "category", &["parent_category_id"], "category");
        assert!(r.from_optional && !r.one_to_one);
        assert_eq!(r.on_delete.as_deref(), Some("SET NULL"));
        assert!(find_rel(&m, "staff", &["reports_to"], "staff").from_optional);
        let r = find_rel(
            &m,
            "rental_item_note",
            &["rental_id", "line_no"],
            "rental_item",
        );
        assert_eq!(r.to_columns, vec!["rental_id", "line_no"]);
        assert!(!r.from_optional && !r.one_to_one);
        assert!(find_rel(&m, "payment", &["rental_id"], "rental").from_optional);
        assert!(find_rel(&m, "audit_log", &["customer_id"], "customer").from_optional);

        let v = m.tables.iter().find(|x| x.name == "film_list").unwrap();
        assert_eq!(v.kind, K::View);
        assert!(v.primary_key.is_empty());
        assert_eq!(v.path.segments[1].name, "views");

        // Without views / comments.
        let mut req = request(db_path("erd_it_model"));
        req.include_views = false;
        req.include_comments = false;
        let m3 = root.schema_model(req).await.unwrap();
        assert_eq!(m3.tables.len(), 23);
        assert!(m3.tables.iter().all(|x| x.comment.is_none()));
        assert!(
            m3.tables
                .iter()
                .flat_map(|x| &x.columns)
                .all(|c| c.comment.is_none())
        );
        // Keys detail.
        let mut req = request(db_path("erd_it_model"));
        req.detail = ModelDetail::Keys;
        let m4 = root.schema_model(req).await.unwrap();
        let film = m4.tables.iter().find(|x| x.name == "film").unwrap();
        let names: Vec<&str> = film.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["film_id", "language_id", "original_language_id"]
        );
        // Explicit table list.
        let mut req = request(db_path("erd_it_model"));
        req.tables = vec![
            table_in("erd_it_model", "film"),
            table_in("erd_it_model", "language"),
        ];
        let m5 = root.schema_model(req).await.unwrap();
        assert_eq!(m5.tables.len(), 2);
        assert_eq!(m5.relations.len(), 2);
        // Unknown database: empty model.
        let m6 = root
            .schema_model(request(db_path("erd_it_does_not_exist")))
            .await
            .unwrap();
        assert!(m6.tables.is_empty());
        run(&*root, "DROP DATABASE erd_it_model").await;
        root.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn model_100_tables_in_four_queries() {
    if !it_enabled() {
        return;
    }
    for t in targets() {
        let tag = t.name;
        let root = root_session(t).await;
        recreate_db(&*root, "erd_it_100").await;
        let mut script = String::new();
        for i in 1..=100 {
            let fk = if i > 1 {
                format!(
                    ", parent_id int NULL, CONSTRAINT fk_t{i:03} FOREIGN KEY (parent_id) REFERENCES t{:03}(id)",
                    i / 2
                )
            } else {
                String::new()
            };
            script.push_str(&format!(
                "CREATE TABLE t{i:03} (id int NOT NULL AUTO_INCREMENT PRIMARY KEY, name varchar(50) NOT NULL, UNIQUE KEY uq_name (name){fk});\n"
            ));
        }
        run_in(&*root, "erd_it_100", &script).await;
        let (m, queries) =
            count_catalog_queries(root.schema_model(request(db_path("erd_it_100")))).await;
        let m = m.unwrap();
        assert_eq!(m.tables.len(), 100, "[{tag}]");
        assert_eq!(m.relations.len(), 99, "[{tag}]");
        assert!(queries <= 4, "[{tag}] {queries} catalog queries");
        let t7 = m.tables.iter().find(|x| x.name == "t007").unwrap();
        assert_eq!(t7.unique_keys, vec![vec!["name".to_string()]]);
        run(&*root, "DROP DATABASE erd_it_100").await;
        root.close().await.unwrap();
    }
}

fn sqls(items: &[DdlItem]) -> Vec<&str> {
    items.iter().map(|i| i.sql.as_str()).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ddl_replays_into_empty_database() {
    if !it_enabled() {
        return;
    }
    for t in targets() {
        let tag = t.name;
        let root = root_session(t).await;
        fixture_db(&*root, "erd_it").await;
        let opts = DdlOptions {
            qualify_schema: false,
            ..DdlOptions::default()
        };
        let req = DdlRequest {
            scope: db_path("erd_it"),
            objects: vec![],
            options: opts,
        };
        let items = root.generate_ddl(req.clone()).await.unwrap();

        // PreData before PostData; no foreign keys inside CREATE TABLE.
        let first_post = items
            .iter()
            .position(|i| i.section == DdlSection::PostData)
            .unwrap();
        assert!(
            items[first_post..]
                .iter()
                .all(|i| i.section == DdlSection::PostData)
        );
        for i in &items[..first_post] {
            assert!(
                !i.sql.contains("FOREIGN KEY"),
                "[{tag}] FK in PreData: {}",
                i.sql
            );
            assert!(
                !i.sql.contains("DEFINER="),
                "[{tag}] definer left: {}",
                i.sql
            );
        }
        let fks: Vec<usize> = items
            .iter()
            .enumerate()
            .filter(|(_, i)| i.sql.contains(" ADD CONSTRAINT ") && i.sql.contains("FOREIGN KEY"))
            .map(|(n, _)| n)
            .collect();
        assert_eq!(fks.len(), 32, "[{tag}]");
        assert!(
            fks.iter()
                .all(|&p| items[p].section == DdlSection::PostData)
        );
        let pos = |needle: &str| {
            items
                .iter()
                .position(|i| i.sql.contains(needle))
                .unwrap_or_else(|| panic!("[{tag}] no item with {needle}"))
        };
        assert!(pos("CREATE TABLE `film` (") < pos("CREATE OR REPLACE ALGORITHM=UNDEFINED"));
        assert!(pos("VIEW `film_list` AS") < pos("VIEW `film_list_long` AS"));
        assert!(
            pos("ADD KEY `idx_title`") < fks[0],
            "[{tag}] indexes before FKs"
        );
        assert!(pos("FULLTEXT KEY `ft_description`") < fks[0]);
        assert!(pos("ins_film") > *fks.last().unwrap());
        assert!(pos("film_stock") > pos("CREATE TABLE `film` ("));
        // Plain indexes were moved out of the CREATE TABLE, unique keys stay.
        let film = items
            .iter()
            .find(|i| i.sql.starts_with("CREATE TABLE `film` ("))
            .unwrap();
        assert!(!film.sql.contains("KEY `idx_title`"));
        assert!(film.sql.contains("PRIMARY KEY (`film_id`)"));
        assert_eq!(film.path, table_in("erd_it", "film"));
        let actor = items
            .iter()
            .find(|i| i.sql.starts_with("CREATE TABLE `actor` ("))
            .unwrap();
        assert!(actor.sql.contains("UNIQUE KEY `uq_actor_name`"));
        // Partition clauses are kept.
        let archive = items
            .iter()
            .find(|i| i.sql.starts_with("CREATE TABLE `rental_archive` ("))
            .unwrap();
        assert!(archive.sql.contains("PARTITION"), "[{tag}] {}", archive.sql);
        assert!(film.sql.contains("COMMENT"), "[{tag}] comments kept");
        assert!(film.sql.contains("AUTO_INCREMENT=3") || film.sql.contains("ENGINE=InnoDB"));

        // Replay into an empty database and compare.
        recreate_db(&*root, "erd_it_copy").await;
        for i in &items {
            run_in(&*root, "erd_it_copy", &i.sql).await;
        }
        let a = root.schema_model(request(db_path("erd_it"))).await.unwrap();
        let b = root
            .schema_model(request(db_path("erd_it_copy")))
            .await
            .unwrap();
        let (ta, ra) = summary(&a);
        let (tb, rb) = summary(&b);
        assert_eq!(ta, tb, "[{tag}]");
        assert_eq!(ra, rb, "[{tag}]");
        assert_eq!(a.tables.len(), 26);

        // The copy yields the same DDL (modulo order).
        let again = root
            .generate_ddl(DdlRequest {
                scope: db_path("erd_it_copy"),
                ..req
            })
            .await
            .unwrap();
        let mut x: Vec<&str> = sqls(&items);
        let mut y: Vec<&str> = sqls(&again);
        x.sort_unstable();
        y.sort_unstable();
        assert_eq!(x, y, "[{tag}]");

        // Routines and triggers work in the copy.
        let rs = query(&*root, "SELECT erd_it_copy.film_stock(1)").await;
        assert_eq!(rs.rows.len(), 1);
        run(&*root, "DROP DATABASE erd_it").await;
        run(&*root, "DROP DATABASE erd_it_copy").await;
        root.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ddl_options_and_selection() {
    if !it_enabled() {
        return;
    }
    for t in targets() {
        let tag = t.name;
        let root = root_session(t).await;
        fixture_db(&*root, "erd_it_opts").await;
        let db = "erd_it_opts";

        // One table: its indexes, FKs and triggers only.
        let items = root
            .generate_ddl(DdlRequest {
                scope: table_in(db, "film"),
                objects: vec![],
                options: DdlOptions {
                    qualify_schema: false,
                    ..DdlOptions::default()
                },
            })
            .await
            .unwrap();
        let all = sqls(&items);
        assert!(
            all.iter().any(|q| q.starts_with("CREATE TABLE `film` (")),
            "[{tag}]"
        );
        assert!(all.iter().any(|q| q.contains("ADD KEY `idx_title`")));
        assert!(
            all.iter()
                .any(|q| q.contains("FOREIGN KEY (`language_id`)"))
        );
        assert!(
            all.iter()
                .any(|q| q.to_uppercase().contains("TRIGGER") && q.contains("ins_film"))
        );
        assert!(!all.iter().any(|q| q.contains("CREATE TABLE `language`")));
        assert!(!all.iter().any(|q| q.contains("VIEW")));

        // Selection in a database scope: FKs to unselected tables are left out.
        let items = root
            .generate_ddl(DdlRequest {
                scope: db_path(db),
                objects: vec![
                    table_in(db, "film"),
                    table_in(db, "language"),
                    table_in(db, "film_text"),
                ],
                options: DdlOptions {
                    qualify_schema: false,
                    include_routines: false,
                    ..DdlOptions::default()
                },
            })
            .await
            .unwrap();
        let tables = items
            .iter()
            .filter(|i| i.sql.starts_with("CREATE TABLE "))
            .count();
        assert_eq!(tables, 3, "[{tag}]");
        let fk_count = items
            .iter()
            .filter(|i| i.sql.contains("FOREIGN KEY"))
            .count();
        assert_eq!(
            fk_count, 3,
            "[{tag}] film x2 -> language, film_text -> film"
        );
        assert!(!items.iter().any(|i| i.sql.contains("customer")));

        // Without indexes, FKs, comments, views, routines and triggers.
        let items = root
            .generate_ddl(DdlRequest {
                scope: db_path(db),
                objects: vec![],
                options: DdlOptions {
                    qualify_schema: false,
                    include_indexes: false,
                    include_foreign_keys: false,
                    include_comments: false,
                    include_views: false,
                    include_routines: false,
                    include_triggers: false,
                    ..DdlOptions::default()
                },
            })
            .await
            .unwrap();
        assert_eq!(items.len(), 23, "[{tag}] only CREATE TABLE items");
        assert!(items.iter().all(|i| i.section == DdlSection::PreData));
        assert!(
            !items
                .iter()
                .any(|i| i.sql.contains(" COMMENT ") || i.sql.contains("COMMENT="))
        );
        assert!(!items.iter().any(|i| i.sql.contains("\n  KEY `")));

        // Qualified names + drop + if-not-exists: the script restores over itself.
        let before = root.schema_model(request(db_path(db))).await.unwrap();
        let items = root
            .generate_ddl(DdlRequest {
                scope: db_path(db),
                objects: vec![],
                options: DdlOptions {
                    include_drop: true,
                    if_not_exists: true,
                    ..DdlOptions::default()
                },
            })
            .await
            .unwrap();
        assert!(items[0].sql.starts_with("SET FOREIGN_KEY_CHECKS = 0"));
        assert!(
            items[1]
                .sql
                .starts_with("CREATE DATABASE IF NOT EXISTS `erd_it_opts`"),
            "[{tag}] {}",
            items[1].sql
        );
        assert!(items.iter().any(|i| {
            i.sql
                .starts_with("CREATE TABLE IF NOT EXISTS `erd_it_opts`.`film` (")
        }));
        assert!(items.iter().any(|i| {
            i.sql
                .starts_with("DROP TABLE IF EXISTS `erd_it_opts`.`film`")
        }));
        // One connection runs the whole script (FOREIGN_KEY_CHECKS is a session variable).
        let script: String = items.iter().map(|i| format!("{}\n", i.sql)).collect();
        run_in(&*root, db, &script).await;
        let after = root.schema_model(request(db_path(db))).await.unwrap();
        assert_eq!(summary(&before), summary(&after), "[{tag}]");

        // Unsupported scope.
        let err = root
            .generate_ddl(DdlRequest {
                scope: ObjectPath::root(),
                objects: vec![],
                options: DdlOptions::default(),
            })
            .await;
        assert!(err.is_err());
        run(&*root, "DROP DATABASE erd_it_opts").await;
        root.close().await.unwrap();
    }
}
