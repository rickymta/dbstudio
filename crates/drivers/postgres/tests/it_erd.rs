//! Integration tests of `schema_model` and `generate_ddl` against `pg16` / `pg12`.
//! Only run with `DBSTUDIO_IT=1`. The Pagila-style fixture lives in database `erd_it`; the DDL
//! generated from it is replayed into the empty database `erd_it_copy` and both are compared.

mod common;

use common::*;
use dbstudio_core::{
    DdlItem, DdlOptions, DdlRequest, DdlSection, ModelDetail, ObjectKind, ObjectPath, QueryRequest,
    SchemaModel, SchemaModelRequest, Session, execute_collect,
};
use dbstudio_driver_postgres::model::count_catalog_queries;
use tokio::sync::OnceCell;

const FIXTURE: &str = include_str!("fixtures/erd_pagila.sql");

fn db_path(db: &str) -> ObjectPath {
    ObjectPath::root().child(ObjectKind::Database, db)
}

fn schema_scope(db: &str, schema: &str) -> ObjectPath {
    db_path(db)
        .child(ObjectKind::Folder, "schemas")
        .child(ObjectKind::Schema, schema)
}

fn table_in(db: &str, schema: &str, name: &str) -> ObjectPath {
    schema_scope(db, schema)
        .child(ObjectKind::Folder, "tables")
        .child(ObjectKind::Table, name)
}

async fn run_in(s: &dyn Session, db: &str, sql: &str) {
    let mut req = QueryRequest::new(sql);
    req.database = Some(db.to_string());
    if let Err(e) = execute_collect(s, req).await {
        panic!("sql failed in {db}: {e}\n{sql}");
    }
}

async fn recreate_db(admin: &dyn Session, db: &str) {
    exec(
        admin,
        &format!(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
             WHERE datname = '{db}' AND pid <> pg_backend_pid()"
        ),
    )
    .await;
    exec(admin, &format!("DROP DATABASE IF EXISTS {db}")).await;
    exec(admin, &format!("CREATE DATABASE {db}")).await;
}

async fn prepare(port: u16) -> Box<dyn Session> {
    let admin = connect(port).await;
    recreate_db(&*admin, "erd_it").await;
    run_in(&*admin, "erd_it", FIXTURE).await;
    admin
}

/// Fixture ready once per server and test process.
async fn fixture(port: u16) -> Box<dyn Session> {
    static PG16_READY: OnceCell<()> = OnceCell::const_new();
    static PG12_READY: OnceCell<()> = OnceCell::const_new();
    let cell = if port == PG16 {
        &PG16_READY
    } else {
        &PG12_READY
    };
    cell.get_or_init(|| async {
        let s = prepare(port).await;
        s.close().await.unwrap();
    })
    .await;
    connect(port).await
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

/// Path without the database segment, so that two databases can be compared.
fn key(p: &ObjectPath) -> String {
    p.segments
        .iter()
        .filter(|s| !matches!(s.kind, ObjectKind::Database | ObjectKind::Folder))
        .map(|s| format!("{:?}:{}", s.kind, s.name))
        .collect::<Vec<_>>()
        .join("/")
}

/// Comparable summary of a model (database name removed).
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

async fn model_pagila(port: u16) {
    let s = fixture(port).await;
    let m = s.schema_model(request(db_path("erd_it"))).await.unwrap();
    assert!(!m.inferred && !m.truncated);
    let tables = m
        .tables
        .iter()
        .filter(|t| t.kind == ObjectKind::Table)
        .count();
    assert_eq!(
        tables,
        25,
        "tables: {:?}",
        m.tables.iter().map(|t| &t.name).collect::<Vec<_>>()
    );
    assert_eq!(m.tables.len(), 29, "tables + 3 views + 1 matview");
    assert_eq!(m.relations.len(), 32);

    let film = m.tables.iter().find(|t| t.name == "film").unwrap();
    assert_eq!(film.schema.as_deref(), Some("public"));
    assert_eq!(film.primary_key, vec!["film_id"]);
    assert_eq!(film.comment.as_deref(), Some("Films; one row per title"));
    assert_eq!(film.columns.len(), 14);
    assert!(film.columns[0].primary_key);
    assert!(!film.columns[0].nullable);
    assert!(
        film.columns[0]
            .default
            .as_deref()
            .unwrap()
            .starts_with("nextval(")
    );
    let rating = film.columns.iter().find(|c| c.name == "rating").unwrap();
    assert_eq!(rating.comment.as_deref(), Some("MPAA rating"));
    let fa = m.tables.iter().find(|t| t.name == "film_actor").unwrap();
    assert_eq!(fa.primary_key, vec!["actor_id", "film_id"]);
    let actor = m.tables.iter().find(|t| t.name == "actor").unwrap();
    assert!(
        actor
            .unique_keys
            .contains(&vec!["first_name".to_string(), "last_name".to_string()])
    );
    let customer = m.tables.iter().find(|t| t.name == "customer").unwrap();
    assert!(
        customer.unique_keys.is_empty(),
        "expression unique indexes are not unique keys"
    );
    let note = m
        .tables
        .iter()
        .find(|t| t.name == "rental_item_note")
        .unwrap();
    let id = note.columns.iter().find(|c| c.name == "note_id").unwrap();
    assert_eq!(id.extra.as_deref(), Some("identity always"));

    // Optional / mandatory / 1-1 / composite / self / cross-schema.
    let r = find_rel(&m, "film", &["original_language_id"], "language");
    assert!(r.from_optional && !r.one_to_one);
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
    let r = find_rel(&m, "staff", &["reports_to"], "staff");
    assert!(r.from_optional);
    let r = find_rel(
        &m,
        "rental_item_note",
        &["rental_id", "line_no"],
        "rental_item",
    );
    assert_eq!(r.to_columns, vec!["rental_id", "line_no"]);
    assert!(!r.from_optional && !r.one_to_one);
    let r = find_rel(&m, "payment", &["rental_id"], "rental");
    assert!(r.from_optional);
    let r = find_rel(&m, "audit_log", &["customer_id"], "customer");
    assert!(r.from_optional);
    assert_eq!(key(&r.from_table), "Schema:erd_aux/Table:audit_log");

    // Views / materialized views are model entries without keys.
    let v = m.tables.iter().find(|t| t.name == "film_list").unwrap();
    assert_eq!(v.kind, ObjectKind::View);
    assert!(v.primary_key.is_empty());
    let mv = m
        .tables
        .iter()
        .find(|t| t.name == "sales_by_store")
        .unwrap();
    assert_eq!(mv.kind, ObjectKind::MaterializedView);

    // Schema scope drops the other schema and the cross-schema relation.
    let m2 = s
        .schema_model(request(schema_scope("erd_it", "public")))
        .await
        .unwrap();
    assert_eq!(m2.tables.len(), 28);
    assert_eq!(m2.relations.len(), 31);
    // Without views.
    let mut req = request(db_path("erd_it"));
    req.include_views = false;
    req.include_comments = false;
    let m3 = s.schema_model(req).await.unwrap();
    assert_eq!(m3.tables.len(), 25);
    assert!(m3.tables.iter().all(|t| t.comment.is_none()));
    assert!(
        m3.tables
            .iter()
            .flat_map(|t| &t.columns)
            .all(|c| c.comment.is_none())
    );
    // Keys detail keeps key columns only.
    let mut req = request(schema_scope("erd_it", "public"));
    req.detail = ModelDetail::Keys;
    let m4 = s.schema_model(req).await.unwrap();
    let film = m4.tables.iter().find(|t| t.name == "film").unwrap();
    let names: Vec<&str> = film.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["film_id", "language_id", "original_language_id"]
    );
    // Explicit table list.
    let mut req = request(db_path("erd_it"));
    req.tables = vec![
        table_in("erd_it", "public", "film"),
        table_in("erd_it", "public", "language"),
    ];
    let m5 = s.schema_model(req).await.unwrap();
    assert_eq!(m5.tables.len(), 2);
    assert_eq!(m5.relations.len(), 2);
    s.close().await.unwrap();
}

async fn model_100_tables_in_four_queries(port: u16) {
    let s = connect(port).await;
    recreate_db(&*s, "erd_it_100").await;
    let mut script = String::from("CREATE SCHEMA gen100;\n");
    for i in 1..=100 {
        let fk = if i > 1 {
            format!(", parent_id integer REFERENCES gen100.t{:03}(id)", i / 2)
        } else {
            String::new()
        };
        script.push_str(&format!(
            "CREATE TABLE gen100.t{i:03} (id serial PRIMARY KEY, name text NOT NULL UNIQUE{fk});\n"
        ));
    }
    run_in(&*s, "erd_it_100", &script).await;
    let (m, queries) =
        count_catalog_queries(s.schema_model(request(schema_scope("erd_it_100", "gen100")))).await;
    let m = m.unwrap();
    assert_eq!(m.tables.len(), 100);
    assert_eq!(m.relations.len(), 99);
    assert!(queries <= 4, "{queries} catalog queries");
    let t = m.tables.iter().find(|t| t.name == "t007").unwrap();
    assert_eq!(t.unique_keys, vec![vec!["name".to_string()]]);
    s.close().await.unwrap();
}

fn positions<'a>(items: &'a [DdlItem], pred: impl Fn(&DdlItem) -> bool + 'a) -> Vec<usize> {
    items
        .iter()
        .enumerate()
        .filter(|(_, i)| pred(i))
        .map(|(n, _)| n)
        .collect()
}

async fn ddl_replays_into_empty_database(port: u16) {
    let s = fixture(port).await;
    let req = DdlRequest {
        scope: db_path("erd_it"),
        objects: vec![],
        options: DdlOptions::default(),
    };
    let (items, queries) = count_catalog_queries(s.generate_ddl(req.clone())).await;
    let items = items.unwrap();
    assert!(queries <= 16, "generate_ddl is bulk: {queries} queries");

    // PreData strictly before PostData; foreign keys only in PostData.
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
            !i.sql.contains("FOREIGN KEY") || i.kind != ObjectKind::Table,
            "FK in PreData: {}",
            i.sql
        );
        assert!(!i.sql.contains("FOREIGN KEY"), "FK in PreData: {}", i.sql);
    }
    let fks = positions(&items, |i| {
        i.sql.contains(" ADD CONSTRAINT ") && i.sql.contains("FOREIGN KEY")
    });
    assert_eq!(fks.len(), 32);
    assert!(
        fks.iter()
            .all(|&p| items[p].section == DdlSection::PostData)
    );
    // Types and sequences before tables, indexes before FKs, triggers after FKs.
    let pos_of = |needle: &str| {
        items
            .iter()
            .position(|i| i.sql.contains(needle))
            .unwrap_or_else(|| panic!("no item with {needle}"))
    };
    assert!(
        pos_of("CREATE TYPE \"public\".\"mpaa_rating\"")
            < pos_of("CREATE TABLE \"public\".\"film\" (")
    );
    assert!(
        pos_of("CREATE SEQUENCE \"public\".\"payment_group_seq\"")
            < pos_of("CREATE TABLE \"public\".\"payment\" (")
    );
    assert!(pos_of("CREATE SCHEMA \"erd_aux\"") < pos_of("CREATE TABLE \"erd_aux\".\"audit_log\""));
    assert!(
        pos_of("CREATE TABLE \"public\".\"film\" (")
            < pos_of("CREATE OR REPLACE VIEW \"public\".\"film_list\" ")
    );
    assert!(
        pos_of("CREATE OR REPLACE VIEW \"public\".\"film_list\" ")
            < pos_of("CREATE OR REPLACE VIEW \"public\".\"film_list_long\"")
    );
    assert!(pos_of("CREATE INDEX idx_film_title") < fks[0]);
    assert!(pos_of("CREATE TRIGGER film_last_updated") > *fks.last().unwrap());
    assert!(
        pos_of("CREATE OR REPLACE FUNCTION public.last_updated")
            > pos_of("CREATE TABLE \"public\".\"film\" (")
    );
    // The table paths of items are tree paths.
    let film = items
        .iter()
        .find(|i| i.sql.starts_with("CREATE TABLE \"public\".\"film\" ("))
        .unwrap();
    assert_eq!(film.path, table_in("erd_it", "public", "film").clone());
    assert_eq!(film.kind, ObjectKind::Table);
    // Partitions follow their parent and carry no columns.
    assert!(
        pos_of("CREATE TABLE \"public\".\"rental_archive\" (")
            < pos_of("CREATE TABLE \"public\".\"rental_archive_2023\" PARTITION OF")
    );
    // Comments and setval are present.
    assert!(
        items
            .iter()
            .any(|i| i.sql.starts_with("COMMENT ON TABLE \"public\".\"film\" IS"))
    );
    assert!(
        items
            .iter()
            .any(|i| i.sql.contains("setval") && i.sql.contains("payment_group_seq"))
    );
    assert!(
        items
            .iter()
            .any(|i| i.sql.contains("pg_get_serial_sequence") && i.sql.contains("film_review"))
    );
    assert!(items.iter().any(|i| i.sql.starts_with("ALTER SEQUENCE \"public\".\"country_country_id_seq\" OWNED BY \"public\".\"country\".\"country_id\"")));

    // Replay into an empty database.
    let admin = connect(port).await;
    recreate_db(&*admin, "erd_it_copy").await;
    for i in &items {
        run_in(&*admin, "erd_it_copy", &i.sql).await;
    }
    let a = s.schema_model(request(db_path("erd_it"))).await.unwrap();
    let b = s
        .schema_model(request(db_path("erd_it_copy")))
        .await
        .unwrap();
    let (ta, ra) = summary(&a);
    let (tb, rb) = summary(&b);
    assert_eq!(ta, tb);
    assert_eq!(ra, rb);
    assert_eq!(a.tables.len(), 29);

    // The copy yields the same DDL again.
    let again = s
        .generate_ddl(DdlRequest {
            scope: db_path("erd_it_copy"),
            ..req
        })
        .await
        .unwrap();
    let mut x: Vec<String> = items.iter().map(|i| i.sql.clone()).collect();
    let mut y: Vec<String> = again.iter().map(|i| i.sql.clone()).collect();
    x.sort();
    y.sort();
    assert_eq!(x, y);

    // Sequence state survived: the next payment group number continues after setval(115).
    run_in(
        &*admin,
        "erd_it_copy",
        "INSERT INTO language (name) VALUES ('Klingon')",
    )
    .await;
    let mut q = QueryRequest::new("SELECT currval('language_language_id_seq')");
    q.database = Some("erd_it_copy".into());
    let r = execute_collect(&*admin, q).await.unwrap();
    assert_eq!(r.len(), 1);
    admin.close().await.unwrap();
    s.close().await.unwrap();
}

async fn ddl_options_and_selection(port: u16) {
    let s = fixture(port).await;
    // Single table: its own indexes/triggers/FKs/comments, and its sequence.
    let film = table_in("erd_it", "public", "film");
    let items = s
        .generate_ddl(DdlRequest {
            scope: film.clone(),
            objects: vec![],
            options: DdlOptions::default(),
        })
        .await
        .unwrap();
    let sqls: Vec<&str> = items.iter().map(|i| i.sql.as_str()).collect();
    assert!(
        sqls.iter()
            .any(|q| q.starts_with("CREATE TABLE \"public\".\"film\" ("))
    );
    assert!(
        sqls.iter()
            .any(|q| q.starts_with("CREATE INDEX idx_film_title"))
    );
    assert!(
        sqls.iter()
            .any(|q| q.contains("CREATE TRIGGER film_last_updated"))
    );
    assert!(sqls.iter().any(|q| q.contains("FOREIGN KEY (language_id)")));
    assert!(
        sqls.iter()
            .any(|q| q.starts_with("CREATE SEQUENCE \"public\".\"film_film_id_seq\""))
    );
    assert!(
        !sqls
            .iter()
            .any(|q| q.contains("CREATE TABLE \"public\".\"language\""))
    );
    assert!(!sqls.iter().any(|q| q.contains("CREATE VIEW")));

    // Explicit selection in a database scope: foreign keys to unselected tables are left out.
    let items = s
        .generate_ddl(DdlRequest {
            scope: db_path("erd_it"),
            objects: vec![
                film.clone(),
                table_in("erd_it", "public", "language"),
                table_in("erd_it", "public", "film_text"),
            ],
            options: DdlOptions::default(),
        })
        .await
        .unwrap();
    let tables = positions(&items, |i| i.sql.starts_with("CREATE TABLE "));
    assert_eq!(tables.len(), 3);
    let fk_sql: Vec<&str> = items
        .iter()
        .filter(|i| i.sql.contains("FOREIGN KEY"))
        .map(|i| i.sql.as_str())
        .collect();
    assert_eq!(
        fk_sql.len(),
        3,
        "film x2 -> language, film_text -> film: {fk_sql:?}"
    );
    assert!(
        items
            .iter()
            .any(|i| i.sql.starts_with("CREATE TYPE \"public\".\"mpaa_rating\""))
    );
    assert!(!items.iter().any(|i| i.sql.contains("customer")));

    // Replaying twice with drop + if-not-exists works.
    let opts = DdlOptions {
        include_drop: true,
        if_not_exists: true,
        include_foreign_keys: false,
        include_triggers: false,
        include_views: false,
        include_routines: false,
        include_grants: true,
        qualify_schema: false,
        ..DdlOptions::default()
    };
    let items = s
        .generate_ddl(DdlRequest {
            scope: schema_scope("erd_it", "erd_aux"),
            objects: vec![],
            options: opts,
        })
        .await
        .unwrap();
    assert!(
        items
            .iter()
            .any(|i| i.sql.starts_with("DROP TABLE IF EXISTS \"audit_log\""))
    );
    assert!(
        items
            .iter()
            .any(|i| i.sql.contains("CREATE TABLE IF NOT EXISTS \"audit_log\""))
    );
    assert!(!items.iter().any(|i| i.sql.contains("FOREIGN KEY")));
    let admin = connect(port).await;
    recreate_db(&*admin, "erd_it_opts").await;
    run_in(
        &*admin,
        "erd_it_opts",
        "CREATE SCHEMA erd_aux; SET search_path TO erd_aux",
    )
    .await;
    for _ in 0..2 {
        for i in &items {
            let sql = format!("SET search_path TO erd_aux; {}", i.sql);
            run_in(&*admin, "erd_it_opts", &sql).await;
        }
    }
    admin.close().await.unwrap();

    // Not an object kind: unsupported.
    let err = s
        .generate_ddl(DdlRequest {
            scope: film
                .child(ObjectKind::Folder, "columns")
                .child(ObjectKind::Column, "title"),
            objects: vec![],
            options: DdlOptions::default(),
        })
        .await;
    assert!(err.is_err());
    s.close().await.unwrap();
}

macro_rules! on_both {
    ($f:ident, $n16:ident, $n12:ident) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $n16() {
            if enabled() {
                $f(PG16).await
            }
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $n12() {
            if enabled() {
                $f(PG12).await
            }
        }
    };
}

on_both!(model_pagila, model_pagila_pg16, model_pagila_pg12);
on_both!(
    model_100_tables_in_four_queries,
    model_100_tables_pg16,
    model_100_tables_pg12
);
on_both!(
    ddl_replays_into_empty_database,
    ddl_replay_pg16,
    ddl_replay_pg12
);
on_both!(
    ddl_options_and_selection,
    ddl_options_pg16,
    ddl_options_pg12
);
