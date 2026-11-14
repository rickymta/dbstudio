//! Object tree, details, search, DDL generation, schema model and explain.

mod common;

use common::*;
use dbstudio_core::{
    DdlRequest, DdlSection, Error, ExplainRequest, ObjectKind, ObjectPath, ObjectSearchRequest,
    PlanFormat, SchemaModelRequest, Value, folder,
};
use uuid::Uuid;

fn db_path(db: &str) -> ObjectPath {
    ObjectPath::root().child(ObjectKind::Database, db)
}

/// Fixture: table with a skipping index, a view, a materialized view (with target table) and
/// a dictionary over a source table.
async fn fixture(s: &dyn dbstudio_core::Session, db: &TempDb) {
    for sql in [
        format!(
            "CREATE TABLE {} (id UInt32, ts DateTime, kind String COMMENT 'the kind', n Nullable(Int32), \
             g UInt32 MATERIALIZED id * 2, INDEX idx_kind kind TYPE bloom_filter GRANULARITY 2) \
             ENGINE = MergeTree PARTITION BY toYYYYMM(ts) ORDER BY (id, ts) COMMENT 'events table'",
            db.t("events")
        ),
        format!(
            "CREATE TABLE {} (day Date, n UInt64) ENGINE = SummingMergeTree ORDER BY day",
            db.t("daily")
        ),
        format!(
            "CREATE MATERIALIZED VIEW {} TO {} AS SELECT toDate(ts) AS day, count() AS n FROM {} GROUP BY day",
            db.t("mv_daily"),
            db.t("daily"),
            db.t("events")
        ),
        format!(
            "CREATE VIEW {} AS SELECT id, kind FROM {}",
            db.t("v_events"),
            db.t("events")
        ),
        format!(
            "CREATE TABLE {} (k UInt64, v String) ENGINE = MergeTree ORDER BY k",
            db.t("dict_src")
        ),
        format!(
            "CREATE DICTIONARY {} (k UInt64, v String) PRIMARY KEY k \
             SOURCE(CLICKHOUSE(TABLE 'dict_src' DB '{}' USER 'dbstudio' PASSWORD 'dbstudio_test')) \
             LAYOUT(FLAT()) LIFETIME(0)",
            db.t("dict"),
            db.name
        ),
        format!(
            "INSERT INTO {} VALUES (1, '2024-01-01 00:00:00', 'a', 5), (2, '2024-02-01 00:00:00', 'b', NULL)",
            db.t("events")
        ),
    ] {
        run(s, &sql).await;
    }
}

fn names(nodes: &[dbstudio_core::ObjectNode]) -> Vec<String> {
    nodes.iter().map(|n| n.name.clone()).collect()
}

#[tokio::test]
async fn tree_lists_databases_folders_objects_and_columns() {
    let Some(s) = session().await else { return };
    let db = TempDb::create(s.as_ref(), http_port(), "tree").await;
    fixture(s.as_ref(), &db).await;
    let root = s.list_children(&ObjectPath::root()).await.unwrap();
    let dbs = names(&root);
    assert!(dbs.contains(&db.name));
    assert!(
        !dbs.iter()
            .any(|d| d == "system" || d == "INFORMATION_SCHEMA" || d == "information_schema")
    );
    assert!(root.iter().all(|n| n.kind == ObjectKind::Database));

    let folders = s.list_children(&db_path(&db.name)).await.unwrap();
    assert_eq!(
        names(&folders),
        vec![
            folder::TABLES,
            folder::VIEWS,
            folder::MATERIALIZED_VIEWS,
            folder::DICTIONARIES
        ]
    );
    // Counts: tables events, daily, dict_src (the MV inner table is hidden; this MV has TO).
    assert_eq!(folders[0].detail.as_deref(), Some("3"));

    let tables = s
        .list_children(&db_path(&db.name).child(ObjectKind::Folder, folder::TABLES))
        .await
        .unwrap();
    assert_eq!(names(&tables), vec!["daily", "dict_src", "events"]);
    assert_eq!(tables[2].detail.as_deref(), Some("MergeTree"));
    assert!(
        tables
            .iter()
            .all(|n| n.kind == ObjectKind::Table && n.has_children)
    );
    let views = s
        .list_children(&db_path(&db.name).child(ObjectKind::Folder, folder::VIEWS))
        .await
        .unwrap();
    assert_eq!(names(&views), vec!["v_events"]);
    assert_eq!(views[0].kind, ObjectKind::View);
    let mvs = s
        .list_children(&db_path(&db.name).child(ObjectKind::Folder, folder::MATERIALIZED_VIEWS))
        .await
        .unwrap();
    assert_eq!(names(&mvs), vec!["mv_daily"]);
    let dicts = s
        .list_children(&db_path(&db.name).child(ObjectKind::Folder, folder::DICTIONARIES))
        .await
        .unwrap();
    assert_eq!(names(&dicts), vec!["dict"]);
    assert_eq!(dicts[0].kind, ObjectKind::Dictionary);

    let events = tables[2].path.clone();
    let tf = s.list_children(&events).await.unwrap();
    assert_eq!(names(&tf), vec![folder::COLUMNS, folder::INDEXES]);
    assert_eq!(tf[1].detail.as_deref(), Some("1"));
    let cols = s
        .list_children(&events.child(ObjectKind::Folder, folder::COLUMNS))
        .await
        .unwrap();
    assert_eq!(names(&cols), vec!["id", "ts", "kind", "n", "g"]);
    assert_eq!(cols[3].detail.as_deref(), Some("Nullable(Int32)"));
    let idx = s
        .list_children(&events.child(ObjectKind::Folder, folder::INDEXES))
        .await
        .unwrap();
    assert_eq!(names(&idx), vec!["idx_kind"]);
    assert_eq!(idx[0].kind, ObjectKind::Index);
    // A view only has columns.
    let vf = s.list_children(&views[0].path).await.unwrap();
    assert_eq!(names(&vf), vec![folder::COLUMNS]);
    // Pagination through the default implementation.
    let page = s
        .list_children_page(dbstudio_core::ChildrenPageRequest {
            parent: db_path(&db.name).child(ObjectKind::Folder, folder::TABLES),
            cursor: None,
            limit: 2,
            pattern: None,
        })
        .await
        .unwrap();
    assert_eq!(page.nodes.len(), 2);
    assert!(page.next_cursor.is_some());
    // Unknown objects.
    assert!(matches!(
        s.list_children(&db_path("no_such_db_it")).await,
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        s.list_children(&db_path(&db.name).child(ObjectKind::Table, "nope"))
            .await,
        Err(Error::NotFound(_))
    ));
}

#[tokio::test]
async fn details_ddl_and_search() {
    let Some(s) = session().await else { return };
    let db = TempDb::create(s.as_ref(), http_port(), "meta").await;
    fixture(s.as_ref(), &db).await;
    let events = db_path(&db.name)
        .child(ObjectKind::Folder, folder::TABLES)
        .child(ObjectKind::Table, "events");
    let d = s.table_details(&events).await.unwrap();
    assert_eq!(d.kind, ObjectKind::Table);
    assert_eq!(d.comment.as_deref(), Some("events table"));
    assert_eq!(d.columns.len(), 5);
    let kind = d.columns.iter().find(|c| c.name == "kind").unwrap();
    assert_eq!(kind.comment.as_deref(), Some("the kind"));
    assert!(d.columns.iter().find(|c| c.name == "n").unwrap().nullable);
    let g = d.columns.iter().find(|c| c.name == "g").unwrap();
    assert!(
        g.extra
            .as_deref()
            .unwrap()
            .starts_with("MATERIALIZED id * 2"),
        "{:?}",
        g.extra
    );
    let id = d.columns.iter().find(|c| c.name == "id").unwrap();
    assert!(id.extra.as_deref().unwrap().contains("sorting key"));
    assert_eq!(d.estimated_rows, Some(2));
    assert!(d.size_bytes.unwrap_or(0) > 0);
    let names: Vec<&str> = d.indexes.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(names, vec!["PARTITION BY", "ORDER BY", "idx_kind"]);
    assert_eq!(d.indexes[1].columns, vec!["id", "ts"]);
    // Views and materialized views have details too.
    let v = s
        .table_details(&db_path(&db.name).child(ObjectKind::View, "v_events"))
        .await
        .unwrap();
    assert_eq!(v.kind, ObjectKind::View);
    assert_eq!(v.columns.len(), 2);
    assert!(matches!(
        s.table_details(&db_path(&db.name).child(ObjectKind::Table, "nope"))
            .await,
        Err(Error::NotFound(_))
    ));

    // DDL text.
    let ddl = s.object_ddl(&events).await.unwrap();
    assert!(ddl.starts_with("CREATE TABLE"), "{ddl}");
    assert!(ddl.contains("MergeTree"));
    assert!(
        s.object_ddl(&db_path(&db.name))
            .await
            .unwrap()
            .starts_with("CREATE DATABASE")
    );
    let dict = db_path(&db.name).child(ObjectKind::Dictionary, "dict");
    assert!(
        s.object_ddl(&dict)
            .await
            .unwrap()
            .starts_with("CREATE DICTIONARY")
    );
    assert!(matches!(
        s.object_ddl(&events.child(ObjectKind::Folder, folder::COLUMNS))
            .await,
        Err(Error::Unsupported(_))
    ));

    // Search.
    let hits = s
        .search_objects(ObjectSearchRequest {
            query: "EVENT".into(),
            scope: ObjectPath::root(),
            kinds: vec![],
            limit: 50,
        })
        .await
        .unwrap();
    let hit_names: Vec<String> = hits
        .iter()
        .filter(|n| n.detail.as_deref() == Some(db.name.as_str()))
        .map(|n| format!("{:?}:{}", n.kind, n.name))
        .collect();
    assert!(
        hit_names.contains(&"Table:events".to_string()),
        "{hit_names:?}"
    );
    assert!(
        hit_names.contains(&"View:v_events".to_string()),
        "{hit_names:?}"
    );
    let cols = s
        .search_objects(ObjectSearchRequest {
            query: "kind".into(),
            scope: db_path(&db.name),
            kinds: vec![ObjectKind::Column],
            limit: 10,
        })
        .await
        .unwrap();
    assert!(
        cols.iter()
            .any(|n| n.kind == ObjectKind::Column && n.name == "kind")
    );
    assert!(
        cols.iter()
            .all(|n| n.path.segments.last().unwrap().kind == ObjectKind::Column)
    );
    assert!(
        s.search_objects(ObjectSearchRequest {
            query: "%".into(),
            scope: db_path(&db.name),
            kinds: vec![ObjectKind::Table],
            limit: 10,
        })
        .await
        .unwrap()
        .is_empty(),
        "LIKE wildcards in the query are literal"
    );
}

#[tokio::test]
async fn schema_model_lists_tables_columns_and_comments() {
    let Some(s) = session().await else { return };
    let db = TempDb::create(s.as_ref(), http_port(), "model").await;
    fixture(s.as_ref(), &db).await;
    let m = s
        .schema_model(SchemaModelRequest {
            scope: db_path(&db.name),
            tables: vec![],
            include_views: false,
            include_comments: true,
            detail: Default::default(),
            sample_size: None,
        })
        .await
        .unwrap();
    let tables: Vec<&str> = m.tables.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(tables, vec!["daily", "dict_src", "events"]);
    assert!(m.relations.is_empty() && !m.inferred && !m.truncated);
    let ev = m.tables.iter().find(|t| t.name == "events").unwrap();
    assert_eq!(ev.comment.as_deref(), Some("events table"));
    assert_eq!(ev.columns.len(), 5);
    assert!(ev.primary_key.is_empty(), "ClickHouse keys are not unique");
    assert_eq!(ev.columns[2].comment.as_deref(), Some("the kind"));
    let with_views = s
        .schema_model(SchemaModelRequest {
            scope: db_path(&db.name),
            tables: vec![],
            include_views: true,
            include_comments: false,
            detail: Default::default(),
            sample_size: None,
        })
        .await
        .unwrap();
    assert_eq!(with_views.tables.len(), 5);
    assert!(with_views.tables.iter().any(|t| t.kind == ObjectKind::View));
    assert!(with_views.tables.iter().all(|t| t.comment.is_none()));
}

#[tokio::test]
async fn generated_ddl_runs_again_and_rebuilds_the_database() {
    let Some(s) = session().await else { return };
    let db = TempDb::create(s.as_ref(), http_port(), "ddl").await;
    fixture(s.as_ref(), &db).await;
    let items = s
        .generate_ddl(DdlRequest {
            scope: db_path(&db.name),
            objects: vec![],
            options: dbstudio_core::DdlOptions {
                if_not_exists: true,
                ..Default::default()
            },
        })
        .await
        .unwrap();
    let kinds: Vec<ObjectKind> = items.iter().map(|i| i.kind).collect();
    assert_eq!(kinds[0], ObjectKind::Database);
    // Tables before dictionaries, views and materialized views last (PostData).
    let pos = |k: ObjectKind| kinds.iter().position(|x| *x == k).unwrap();
    assert!(pos(ObjectKind::Table) < pos(ObjectKind::Dictionary));
    assert!(pos(ObjectKind::Dictionary) < pos(ObjectKind::View));
    assert!(pos(ObjectKind::View) < pos(ObjectKind::MaterializedView));
    for i in &items {
        let post = matches!(i.kind, ObjectKind::View | ObjectKind::MaterializedView);
        assert_eq!(i.section == DdlSection::PostData, post, "{:?}", i.kind);
        assert!(i.sql.trim_end().ends_with(';'));
    }
    // Rebuild into a fresh database: drop the original, then replay the statements.
    run(s.as_ref(), &format!("DROP DATABASE `{}` SYNC", db.name)).await;
    for i in &items {
        let sql = i.sql.trim_end().trim_end_matches(';').to_string();
        run(s.as_ref(), &sql).await;
    }
    let tables = s
        .list_children(&db_path(&db.name).child(ObjectKind::Folder, folder::TABLES))
        .await
        .unwrap();
    assert_eq!(names(&tables), vec!["daily", "dict_src", "events"]);
    assert_eq!(
        s.list_children(&db_path(&db.name).child(ObjectKind::Folder, folder::MATERIALIZED_VIEWS))
            .await
            .unwrap()
            .len(),
        1
    );
    // The recreated materialized view still feeds its target table.
    run(
        s.as_ref(),
        &format!(
            "INSERT INTO {} VALUES (9, '2024-05-05 00:00:00', 'z', 1)",
            db.t("events")
        ),
    )
    .await;
    assert_eq!(
        scalar(s.as_ref(), &format!("SELECT sum(n) FROM {}", db.t("daily"))).await,
        Value::UInt(1)
    );
    // Options: drop statements first, no schema qualification, a single object.
    let one = s
        .generate_ddl(DdlRequest {
            scope: db_path(&db.name),
            objects: vec![db_path(&db.name).child(ObjectKind::Table, "events")],
            options: dbstudio_core::DdlOptions {
                include_drop: true,
                qualify_schema: false,
                ..Default::default()
            },
        })
        .await
        .unwrap();
    assert_eq!(one.len(), 1);
    assert!(
        one[0].sql.starts_with("DROP TABLE IF EXISTS"),
        "{}",
        one[0].sql
    );
    assert!(one[0].sql.contains("CREATE TABLE events"), "{}", one[0].sql);
    // No scope and no objects is refused.
    assert!(
        s.generate_ddl(DdlRequest {
            scope: ObjectPath::root(),
            objects: vec![],
            options: Default::default(),
        })
        .await
        .is_err()
    );
}

#[tokio::test]
async fn explain_returns_a_parseable_plan_with_estimate_and_pipeline() {
    let Some(s) = session().await else { return };
    let db = TempDb::create(s.as_ref(), http_port(), "plan").await;
    run(
        s.as_ref(),
        &format!(
            "CREATE TABLE {} (id UInt64, g UInt32) ENGINE = MergeTree ORDER BY id SETTINGS index_granularity = 1024",
            db.t("t")
        ),
    )
    .await;
    run(
        s.as_ref(),
        &format!(
            "INSERT INTO {} SELECT number, number % 10 FROM numbers(200000)",
            db.t("t")
        ),
    )
    .await;
    let req = |text: &str| ExplainRequest {
        query_id: Uuid::new_v4(),
        text: text.into(),
        database: None,
        analyze: false,
        options: Default::default(),
        timeout_ms: None,
    };
    let raw = s
        .explain(req(&format!(
            "SELECT count() FROM {} WHERE id = 5;",
            db.t("t")
        )))
        .await
        .unwrap();
    assert_eq!(raw.format, PlanFormat::ClickHouseJson);
    assert!(!raw.analyzed && !raw.rolled_back);
    assert!(raw.server_version.starts_with("26."));
    assert!(raw.captured_at.ends_with('Z'));
    let est: serde_json::Value = serde_json::from_str(&raw.extra["estimate"]).unwrap();
    assert_eq!(est[0]["table"], "t");
    assert!(est[0]["rows"].as_u64().unwrap() > 0);
    assert!(raw.extra["pipeline"].contains("MergeTree"));
    let tree =
        dbstudio_plan::parse_and_analyze(&raw, &dbstudio_plan::PlanRules::default()).unwrap();
    let read = {
        fn find(n: &dbstudio_plan::PlanNode) -> Option<&dbstudio_plan::PlanNode> {
            if n.op == "ReadFromMergeTree" {
                return Some(n);
            }
            n.children.iter().find_map(find)
        }
        find(&tree.root).unwrap()
    };
    assert_eq!(read.kind, dbstudio_plan::PlanNodeKind::IndexScan);
    // A scan without a usable key has no narrowing.
    let raw = s
        .explain(req(&format!(
            "SELECT count() FROM {} WHERE g = 3",
            db.t("t")
        )))
        .await
        .unwrap();
    let tree = dbstudio_plan::parse(&raw).unwrap();
    assert_eq!(tree.node_count as usize, {
        fn count(n: &dbstudio_plan::PlanNode) -> usize {
            1 + n.children.iter().map(count).sum::<usize>()
        }
        count(&tree.root)
    });
    // EXPLAIN ANALYZE does not exist; multiple statements and bad SQL are refused.
    let mut a = req("SELECT 1");
    a.analyze = true;
    assert!(matches!(s.explain(a).await, Err(Error::Unsupported(c)) if c == "explain_analyze"));
    assert!(s.explain(req("SELEC 1")).await.is_err());
    assert!(s.explain(req("")).await.is_err());
}

#[tokio::test]
async fn fetch_and_count_use_filters_sorting_and_the_sorting_key() {
    let Some(s) = session().await else { return };
    use dbstudio_core::{
        CollectingSink, ColumnFilter, FilterOp, SortKey, TableCountRequest, TableDataRequest,
    };
    let db = TempDb::create(s.as_ref(), http_port(), "grid").await;
    run(
        s.as_ref(),
        &format!(
            "CREATE TABLE {} (id UInt32, name String, tag Nullable(String)) ENGINE = MergeTree ORDER BY id",
            db.t("t")
        ),
    )
    .await;
    run(
        s.as_ref(),
        &format!(
            "INSERT INTO {} VALUES (1,'alpha',NULL),(2,'beta','x'),(3,'Gamma_50%','y'),(4,'delta',NULL),(5,'epsilon','x')",
            db.t("t")
        ),
    )
    .await;
    let table = db_path(&db.name).child(ObjectKind::Table, "t");
    let fetch =
        |filters: Vec<ColumnFilter>, raw: Option<&str>, sort: Vec<SortKey>, offset, limit| {
            let table = table.clone();
            let s = s.clone();
            let raw = raw.map(str::to_string);
            async move {
                let mut sink = CollectingSink::new();
                s.fetch_table(
                    TableDataRequest {
                        query_id: Uuid::new_v4(),
                        table,
                        filters,
                        raw_where: raw,
                        sort,
                        offset,
                        limit,
                        batch_size: None,
                    },
                    &mut sink,
                )
                .await
                .map(|_| sink.into_results())
            }
        };
    let ids = |r: &[dbstudio_core::StatementResult]| -> Vec<Value> {
        rows_of(&r[0])
            .rows
            .iter()
            .map(|row| row[0].clone())
            .collect()
    };
    let f = |c: &str, op, v: &[&str]| ColumnFilter {
        column: c.into(),
        op,
        values: v.iter().map(|s| s.to_string()).collect(),
    };
    let r = fetch(vec![], None, vec![], 1, 2).await.unwrap();
    assert_eq!(
        ids(&r),
        vec![Value::Int(2), Value::Int(3)],
        "paged in sorting key order"
    );
    let r = fetch(
        vec![],
        None,
        vec![SortKey {
            column: "name".into(),
            descending: true,
        }],
        0,
        2,
    )
    .await
    .unwrap();
    assert_eq!(ids(&r), vec![Value::Int(5), Value::Int(4)]);
    let r = fetch(
        vec![f("name", FilterOp::Contains, &["50%"])],
        None,
        vec![],
        0,
        10,
    )
    .await
    .unwrap();
    assert_eq!(ids(&r), vec![Value::Int(3)]);
    let r = fetch(
        vec![f("name", FilterOp::StartsWith, &["ALP"])],
        None,
        vec![],
        0,
        10,
    )
    .await
    .unwrap();
    assert_eq!(ids(&r), vec![Value::Int(1)]);
    let r = fetch(vec![f("tag", FilterOp::IsNull, &[])], None, vec![], 0, 10)
        .await
        .unwrap();
    assert_eq!(ids(&r), vec![Value::Int(1), Value::Int(4)]);
    let r = fetch(
        vec![f("id", FilterOp::In, &["2", "5"])],
        None,
        vec![],
        0,
        10,
    )
    .await
    .unwrap();
    assert_eq!(ids(&r), vec![Value::Int(2), Value::Int(5)]);
    let r = fetch(
        vec![f("id", FilterOp::Ge, &["4"])],
        Some("tag = 'x' -- comment"),
        vec![],
        0,
        10,
    )
    .await
    .unwrap();
    assert_eq!(ids(&r), vec![Value::Int(5)]);
    assert!(
        fetch(vec![], Some("1 = 1; DROP TABLE t"), vec![], 0, 10)
            .await
            .is_err()
    );
    let count = |filters, raw: Option<&str>| {
        let table = table.clone();
        let s = s.clone();
        let raw = raw.map(str::to_string);
        async move {
            s.count_table(TableCountRequest {
                table,
                filters,
                raw_where: raw,
                query_id: Some(Uuid::new_v4()),
            })
            .await
            .unwrap()
        }
    };
    assert_eq!(count(vec![], None).await, 5);
    assert_eq!(count(vec![f("tag", FilterOp::Eq, &["x"])], None).await, 2);
    assert_eq!(count(vec![], Some("id > 2")).await, 3);
}
