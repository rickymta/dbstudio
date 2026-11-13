//! Integration tests: explain, schema model, DDL generation.

mod common;

use std::sync::atomic::Ordering;

use common::*;
use dbstudio_core::{
    DdlItem, DdlOptions, DdlRequest, DdlSection, ExplainRequest, ModelDetail, ObjectKind,
    ObjectPath, PlanFormat, SchemaModelRequest, Value, folder,
};

macro_rules! it {
    () => {
        if !it_enabled() {
            return;
        }
    };
}

fn db(name: &str) -> ObjectPath {
    ObjectPath::root().child(ObjectKind::Database, name)
}
fn schema(db_name: &str, s: &str) -> ObjectPath {
    db(db_name)
        .child(ObjectKind::Folder, folder::SCHEMAS)
        .child(ObjectKind::Schema, s)
}

fn explain_req(text: &str, analyze: bool) -> ExplainRequest {
    ExplainRequest {
        query_id: uuid::Uuid::new_v4(),
        text: text.into(),
        database: None,
        analyze,
        options: Default::default(),
        timeout_ms: None,
    }
}

fn assert_well_formed(xml: &str) {
    let mut r = quick_xml::Reader::from_str(xml);
    loop {
        match r.read_event() {
            Ok(quick_xml::events::Event::Eof) => break,
            Ok(_) => {}
            Err(e) => panic!("plan is not well-formed XML: {e}"),
        }
    }
}

#[tokio::test]
async fn estimated_plan_is_showplan_xml() {
    it!();
    let s = fixture().await;
    let p = s
        .explain(explain_req("SELECT c.name, o.total FROM sales.customers c JOIN sales.orders o ON o.customer_id = c.id WHERE c.id = 1", false))
        .await
        .unwrap();
    assert_eq!(p.format, PlanFormat::MsSqlShowplanXml);
    assert!(
        p.text.contains("<ShowPlanXML"),
        "{}",
        &p.text[..p.text.len().min(200)]
    );
    assert!(p.text.contains("<RelOp"));
    assert!(!p.analyzed && !p.rolled_back);
    assert!(p.server_version.starts_with("16."), "{}", p.server_version);
    assert!(p.captured_at.ends_with('Z'));
    assert_well_formed(&p.text);
    // The connection is clean afterwards: ordinary queries return rows again.
    let r = run(&*s, "SELECT COUNT(*) FROM sales.customers").await;
    assert_eq!(rows_of(&r[0]).rows[0][0], Value::Int(4));
}

#[tokio::test]
async fn analyze_runs_and_always_rolls_back() {
    it!();
    let s = work().await;
    run(&*s, "DROP TABLE IF EXISTS dbo.explain_t;\nGO\nCREATE TABLE dbo.explain_t (id INT PRIMARY KEY, v INT);\nGO\nINSERT INTO dbo.explain_t VALUES (1, 1), (2, 2);").await;
    let p = s
        .explain(explain_req("UPDATE dbo.explain_t SET v = 100", true))
        .await
        .unwrap();
    assert!(p.analyzed && p.rolled_back);
    assert!(
        p.text.contains("<RunTimeInformation"),
        "actual statistics are present"
    );
    assert_well_formed(&p.text);
    let r = run(&*s, "SELECT SUM(v) FROM dbo.explain_t").await;
    assert_eq!(
        rows_of(&r[0]).rows[0][0],
        Value::Int(3),
        "the update was rolled back"
    );
    // Analyze of a SELECT is also wrapped and rolled back.
    let p = s
        .explain(explain_req("SELECT * FROM dbo.explain_t", true))
        .await
        .unwrap();
    assert!(p.rolled_back);
    // DDL cannot be rolled back reliably.
    assert_eq!(
        err_kind(
            s.explain(explain_req("DROP TABLE dbo.explain_t", true))
                .await
        ),
        "unsupported"
    );
    // Errors carry a position.
    let e = s
        .explain(explain_req("SELECT * FROM dbo.does_not_exist", false))
        .await
        .unwrap_err();
    assert!(matches!(e, dbstudio_core::Error::Query { .. }), "{e}");
}

#[tokio::test]
async fn schema_model_is_four_queries() {
    it!();
    let s = fixture().await;
    let (n, _guard) = count_queries();
    let m = s
        .schema_model(SchemaModelRequest {
            scope: schema(IT_DB, "sales"),
            tables: vec![],
            include_views: false,
            include_comments: true,
            detail: ModelDetail::Full,
            sample_size: None,
        })
        .await
        .unwrap();
    assert!(
        n.load(Ordering::SeqCst) <= 4,
        "queries: {}",
        n.load(Ordering::SeqCst)
    );
    assert_eq!(
        m.tables.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
        vec!["customers", "orders"]
    );
    assert!(!m.truncated && !m.inferred);
    let c = &m.tables[0];
    assert_eq!(c.comment.as_deref(), Some("Customer master"));
    assert_eq!(c.primary_key, vec!["id"]);
    assert_eq!(c.unique_keys, vec![vec!["name".to_string()]]);
    assert_eq!(c.estimated_rows, Some(4));
    assert_eq!(c.columns.len(), 6);
    assert_eq!(c.columns[1].comment.as_deref(), Some("Display name"));
    assert_eq!(c.schema.as_deref(), Some("sales"));
    assert_eq!(m.relations.len(), 1);
    let r = &m.relations[0];
    assert_eq!(r.name, "FK_orders_customer");
    assert_eq!(r.from_table.last().unwrap().name, "orders");
    assert_eq!(r.to_table.last().unwrap().name, "customers");
    assert!(!r.from_optional && !r.one_to_one && !r.inferred);
    assert_eq!(r.on_delete.as_deref(), Some("CASCADE"));

    // Keys detail keeps key and foreign key columns only; comments can be left out.
    let m = s
        .schema_model(SchemaModelRequest {
            scope: db(IT_DB),
            tables: vec![],
            include_views: true,
            include_comments: false,
            detail: ModelDetail::Keys,
            sample_size: None,
        })
        .await
        .unwrap();
    let orders = m.tables.iter().find(|t| t.name == "orders").unwrap();
    let names: Vec<_> = orders.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["id", "line", "customer_id"]);
    assert!(orders.comment.is_none());
    assert!(
        m.tables
            .iter()
            .any(|t| t.kind == ObjectKind::View && t.name == "big_orders")
    );
    // Restricting to chosen tables.
    let only = s
        .schema_model(SchemaModelRequest {
            scope: db(IT_DB),
            tables: vec![
                schema(IT_DB, "sales")
                    .child(ObjectKind::Folder, folder::TABLES)
                    .child(ObjectKind::Table, "orders"),
            ],
            include_views: false,
            include_comments: false,
            detail: ModelDetail::Full,
            sample_size: None,
        })
        .await
        .unwrap();
    assert_eq!(only.tables.len(), 1);
    assert!(
        only.relations.is_empty(),
        "the referenced table is outside the model"
    );
}

#[tokio::test]
async fn hundred_tables_in_four_queries() {
    it!();
    let m = master().await;
    run(&*m, "IF DB_ID(N'dbstudio_it_wide') IS NOT NULL BEGIN ALTER DATABASE [dbstudio_it_wide] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [dbstudio_it_wide]; END\nGO\nCREATE DATABASE [dbstudio_it_wide]").await;
    let s = session_on("dbstudio_it_wide").await;
    let mut sql = String::from("CREATE SCHEMA wide;\nGO\n");
    sql.push_str("DECLARE @i INT = 1, @s NVARCHAR(MAX) = N''; WHILE @i <= 100 BEGIN SET @s += N'CREATE TABLE wide.t' + CAST(@i AS NVARCHAR(5)) + N' (id INT PRIMARY KEY, parent INT NULL' + CASE WHEN @i > 1 THEN N' REFERENCES wide.t' + CAST(@i - 1 AS NVARCHAR(5)) + N'(id)' ELSE N'' END + N', name NVARCHAR(30));'; SET @i += 1; END; EXEC(@s);");
    run(&*s, &sql).await;
    let (n, _guard) = count_queries();
    let model = s
        .schema_model(SchemaModelRequest {
            scope: schema("dbstudio_it_wide", "wide"),
            tables: vec![],
            include_views: false,
            include_comments: false,
            detail: ModelDetail::Full,
            sample_size: None,
        })
        .await
        .unwrap();
    assert_eq!(model.tables.len(), 100);
    assert_eq!(model.relations.len(), 99);
    assert!(
        n.load(Ordering::SeqCst) <= 4,
        "queries: {}",
        n.load(Ordering::SeqCst)
    );
    drop(_guard);
    s.close().await.ok();
    run(&*m, "ALTER DATABASE [dbstudio_it_wide] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [dbstudio_it_wide]").await;
}

async fn counts(s: &dyn dbstudio_core::Session, what: &str) -> Vec<String> {
    let r = run(
        s,
        &format!(
            "SELECT CONCAT(type, ':', COUNT(*)) FROM sys.objects WHERE is_ms_shipped = 0 AND type IN ('U','V','FN','IF','TF','P','SO','SN','TR','PK','UQ','F','C','D') GROUP BY type ORDER BY type -- {what}"
        ),
    )
    .await;
    rows_of(&r[0]).rows.iter().map(|r| text(&r[0])).collect()
}

async fn run_items(s: &dyn dbstudio_core::Session, items: &[DdlItem]) {
    for it in items {
        run(s, &it.sql).await;
    }
}

#[tokio::test]
async fn generated_ddl_recreates_the_database() {
    it!();
    let src = fixture().await;
    let items = src
        .generate_ddl(DdlRequest {
            scope: db(IT_DB),
            objects: vec![],
            options: DdlOptions::default(),
        })
        .await
        .unwrap();
    assert!(!items.is_empty());
    // Sections: every PostData item comes after every PreData item.
    let first_post = items
        .iter()
        .position(|i| i.section == DdlSection::PostData)
        .unwrap();
    assert!(
        items[first_post..]
            .iter()
            .all(|i| i.section == DdlSection::PostData)
    );
    assert!(
        items[..first_post]
            .iter()
            .all(|i| i.section == DdlSection::PreData)
    );
    // Dependencies: a table precedes the view on it, schemas and types come first.
    let pos = |kind: ObjectKind, name: &str| {
        items
            .iter()
            .position(|i| i.kind == kind && i.path.last().unwrap().name == name)
            .unwrap_or_else(|| panic!("{kind:?} {name} missing"))
    };
    assert!(pos(ObjectKind::Schema, "sales") < pos(ObjectKind::Type, "LineList"));
    assert!(pos(ObjectKind::Type, "Email") < pos(ObjectKind::Table, "customers"));
    assert!(pos(ObjectKind::Table, "orders") < pos(ObjectKind::View, "big_orders"));
    assert!(pos(ObjectKind::Function, "fn_double") < first_post);
    assert!(
        items
            .iter()
            .all(|i| !i.sql.contains("\nGO\n") && !i.sql.trim_start().starts_with("GO"))
    );

    // Re-run in an empty database.
    let master = master().await;
    run(&*master, "IF DB_ID(N'dbstudio_it_copy') IS NOT NULL BEGIN ALTER DATABASE [dbstudio_it_copy] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [dbstudio_it_copy]; END\nGO\nCREATE DATABASE [dbstudio_it_copy] COLLATE Latin1_General_100_CI_AS").await;
    let dst = session_on("dbstudio_it_copy").await;
    run_items(&*dst, &items).await;
    assert_eq!(counts(&*src, "src").await, counts(&*dst, "dst").await);

    // Same model on both sides (columns, keys, relations).
    let req = |d: &str| SchemaModelRequest {
        scope: db(d),
        tables: vec![],
        include_views: true,
        include_comments: true,
        detail: ModelDetail::Full,
        sample_size: None,
    };
    let strip = |mut m: dbstudio_core::SchemaModel| {
        let db_of = |p: &ObjectPath| p.segments[0].name.clone();
        let _ = db_of;
        for t in &mut m.tables {
            t.estimated_rows = None;
            t.path = ObjectPath::root();
        }
        for r in &mut m.relations {
            r.from_table = ObjectPath::root()
                .child(ObjectKind::Table, r.from_table.last().unwrap().name.clone());
            r.to_table = ObjectPath::root()
                .child(ObjectKind::Table, r.to_table.last().unwrap().name.clone());
        }
        m.scope = ObjectPath::root();
        m
    };
    let a = strip(src.schema_model(req(IT_DB)).await.unwrap());
    let b = strip(dst.schema_model(req("dbstudio_it_copy")).await.unwrap());
    assert_eq!(a.tables.len(), b.tables.len());
    for (x, y) in a.tables.iter().zip(&b.tables) {
        assert_eq!(x, y, "table {}", x.name);
    }
    assert_eq!(a.relations, b.relations);
    // A second run is harmless with if_not_exists; with drops it rebuilds everything.
    let again = src
        .generate_ddl(DdlRequest {
            scope: db(IT_DB),
            objects: vec![],
            options: DdlOptions {
                if_not_exists: true,
                ..Default::default()
            },
        })
        .await
        .unwrap();
    run_items(&*dst, &again).await;
    let rebuilt = src
        .generate_ddl(DdlRequest {
            scope: db(IT_DB),
            objects: vec![],
            options: DdlOptions {
                include_drop: true,
                ..Default::default()
            },
        })
        .await
        .unwrap();
    run_items(&*dst, &rebuilt).await;
    assert_eq!(counts(&*src, "src").await, counts(&*dst, "dst").await);
    dst.close().await.ok();
    run(&*master, "ALTER DATABASE [dbstudio_it_copy] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [dbstudio_it_copy]").await;
}

#[tokio::test]
async fn ddl_of_chosen_objects_and_options() {
    it!();
    let s = fixture().await;
    let orders = schema(IT_DB, "sales")
        .child(ObjectKind::Folder, folder::TABLES)
        .child(ObjectKind::Table, "orders");
    let items = s
        .generate_ddl(DdlRequest {
            scope: db(IT_DB),
            objects: vec![orders.clone()],
            options: DdlOptions {
                include_foreign_keys: true,
                qualify_schema: false,
                ..Default::default()
            },
        })
        .await
        .unwrap();
    // Only the chosen table (its FK target is not selected, so the key is left out).
    assert!(
        items
            .iter()
            .any(|i| i.kind == ObjectKind::Table && i.sql.starts_with("CREATE TABLE [orders]"))
    );
    assert!(!items.iter().any(|i| i.sql.contains("FOREIGN KEY")));
    assert!(items.iter().any(|i| i.kind == ObjectKind::Index));
    let none = s
        .generate_ddl(DdlRequest {
            scope: db(IT_DB),
            objects: vec![],
            options: DdlOptions {
                include_indexes: false,
                include_foreign_keys: false,
                include_triggers: false,
                include_views: false,
                include_routines: false,
                include_sequences: false,
                include_comments: false,
                ..Default::default()
            },
        })
        .await
        .unwrap();
    assert!(none.iter().all(|i| matches!(
        i.kind,
        ObjectKind::Table | ObjectKind::Schema | ObjectKind::Type | ObjectKind::Synonym
    )));
    assert!(none.iter().all(|i| i.section == DdlSection::PreData));
}
