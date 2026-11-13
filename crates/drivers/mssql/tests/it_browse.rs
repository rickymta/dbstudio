//! Integration tests: object tree, details, DDL, search, table reads.

mod common;

use common::*;
use dbstudio_core::{
    ColumnFilter, FilterOp, ObjectKind, ObjectPath, ObjectSearchRequest, SortKey,
    TableCountRequest, TableDataRequest, Value, folder,
};

macro_rules! it {
    () => {
        if !it_enabled() {
            return;
        }
    };
}

fn db() -> ObjectPath {
    ObjectPath::root().child(ObjectKind::Database, IT_DB)
}
fn schema(name: &str) -> ObjectPath {
    db().child(ObjectKind::Folder, folder::SCHEMAS)
        .child(ObjectKind::Schema, name)
}
fn table(schema_name: &str, name: &str) -> ObjectPath {
    schema(schema_name)
        .child(ObjectKind::Folder, folder::TABLES)
        .child(ObjectKind::Table, name)
}

#[tokio::test]
async fn tree_levels() {
    it!();
    let s = fixture().await;
    let root = s.list_children(&ObjectPath::root()).await.unwrap();
    let names: Vec<&str> = root.iter().map(|n| n.name.as_str()).collect();
    assert!(names.contains(&"master") && names.contains(&IT_DB));
    let master = root.iter().find(|n| n.name == "master").unwrap();
    assert_eq!(master.detail.as_deref(), Some("system"));
    assert_eq!(master.kind, ObjectKind::Database);
    assert!(
        root.iter()
            .any(|n| n.kind == ObjectKind::Folder && n.name == folder::ROLES)
    );
    let roles = s
        .list_children(&ObjectPath::root().child(ObjectKind::Folder, folder::ROLES))
        .await
        .unwrap();
    assert!(
        roles
            .iter()
            .any(|r| r.name == "sa" && r.kind == ObjectKind::Role)
    );

    let d = s.list_children(&db()).await.unwrap();
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].path, db().child(ObjectKind::Folder, folder::SCHEMAS));
    let schemas = s.list_children(&d[0].path).await.unwrap();
    let sn: Vec<&str> = schemas.iter().map(|n| n.name.as_str()).collect();
    assert!(sn.contains(&"dbo") && sn.contains(&"sales") && sn.contains(&"sys"));
    assert!(!sn.contains(&"db_owner"));

    let folders = s.list_children(&schema("sales")).await.unwrap();
    let keys: Vec<&str> = folders.iter().map(|n| n.name.as_str()).collect();
    assert_eq!(
        keys,
        vec![
            "tables",
            "views",
            "functions",
            "procedures",
            "sequences",
            "synonyms",
            "types"
        ]
    );
    let count = |k: &str| {
        folders
            .iter()
            .find(|n| n.name == k)
            .and_then(|n| n.detail.clone())
            .unwrap()
    };
    assert_eq!(count("tables"), "2");
    assert_eq!(count("views"), "1");
    assert_eq!(count("functions"), "2");
    assert_eq!(count("procedures"), "1");
    assert_eq!(count("sequences"), "1");
    assert_eq!(count("types"), "1");

    let tables = s.list_children(&folders[0].path).await.unwrap();
    assert_eq!(
        tables.iter().map(|n| n.name.as_str()).collect::<Vec<_>>(),
        vec!["customers", "orders"]
    );
    assert_eq!(tables[0].path, table("sales", "customers"));
    assert!(tables[0].has_children);

    let fns = s.list_children(&folders[2].path).await.unwrap();
    assert_eq!(fns[0].detail.as_deref(), Some("scalar"));
    assert_eq!(fns[1].detail.as_deref(), Some("inline table"));
    let seq = s.list_children(&folders[4].path).await.unwrap();
    assert_eq!(seq[0].detail.as_deref(), Some("bigint"));
    let syn = s
        .list_children(&schema("dbo").child(ObjectKind::Folder, folder::SYNONYMS))
        .await
        .unwrap();
    assert_eq!(syn[0].name, "cust");
    assert!(syn[0].detail.as_deref().unwrap().contains("customers"));
    let types = s.list_children(&folders[6].path).await.unwrap();
    assert_eq!(
        (types[0].name.as_str(), types[0].detail.as_deref()),
        ("LineList", Some("table type"))
    );
}

#[tokio::test]
async fn table_children_and_paths_without_folders() {
    it!();
    let s = fixture().await;
    let t = table("sales", "customers");
    let folders = s.list_children(&t).await.unwrap();
    assert_eq!(
        folders
            .iter()
            .map(|n| (n.name.as_str(), n.detail.as_deref()))
            .collect::<Vec<_>>(),
        vec![
            ("columns", Some("6")),
            ("indexes", Some("2")),
            ("triggers", Some("1"))
        ]
    );
    let cols = s.list_children(&folders[0].path).await.unwrap();
    assert_eq!(cols[0].name, "id");
    assert_eq!(cols[0].detail.as_deref(), Some("int identity"));
    assert_eq!(cols[2].detail.as_deref(), Some("dbo.Email"));
    assert_eq!(
        cols[0].path,
        t.child(ObjectKind::Folder, folder::COLUMNS)
            .child(ObjectKind::Column, "id")
    );
    let idx = s.list_children(&folders[1].path).await.unwrap();
    assert_eq!(idx.len(), 2);
    assert!(idx.iter().any(
        |i| i.name == "PK_customers" && i.detail.as_deref().unwrap().starts_with("PRIMARY KEY")
    ));
    let trg = s.list_children(&folders[2].path).await.unwrap();
    assert_eq!(trg[0].name, "trg_customers_ins");
    assert_eq!(trg[0].detail.as_deref(), Some("AFTER INSERT"));

    // The same table addressed without Folder segments.
    let bare = ObjectPath::root()
        .child(ObjectKind::Database, IT_DB)
        .child(ObjectKind::Schema, "sales")
        .child(ObjectKind::Table, "customers");
    assert_eq!(s.list_children(&bare).await.unwrap().len(), 3);
    let d = s.table_details(&bare).await.unwrap();
    assert_eq!(d.path, t);
    assert_eq!(
        err_kind(
            s.list_children(&table("sales", "nope").child(ObjectKind::Folder, folder::COLUMNS))
                .await
        ),
        "notFound"
    );
}

#[tokio::test]
async fn details_of_a_table() {
    it!();
    let s = fixture().await;
    let d = s.table_details(&table("sales", "customers")).await.unwrap();
    assert_eq!(d.kind, ObjectKind::Table);
    assert_eq!(d.comment.as_deref(), Some("Customer master"));
    assert_eq!(d.estimated_rows, Some(4));
    assert!(d.size_bytes.unwrap() > 0);
    let id = &d.columns[0];
    assert!(id.primary_key && !id.nullable);
    assert_eq!(id.extra.as_deref(), Some("identity(1,1)"));
    let name = &d.columns[1];
    assert_eq!(name.type_name, "nvarchar(100)");
    assert_eq!(name.comment.as_deref(), Some("Display name"));
    let email = &d.columns[2];
    assert_eq!(email.type_name, "dbo.Email");
    assert_eq!(email.value_kind, dbstudio_core::ValueKind::Text);
    assert_eq!(d.columns[3].type_name, "decimal(12,2)");
    assert_eq!(d.columns[3].default.as_deref(), Some("((0))"));
    assert_eq!(d.columns[4].type_name, "datetime2(3)");
    assert_eq!(d.columns[5].type_name, "nvarchar(max)");
    assert!(
        d.indexes
            .iter()
            .any(|i| i.primary && i.columns == vec!["id"])
    );
    assert!(
        d.indexes
            .iter()
            .any(|i| i.name == "UQ_customers_name" && i.unique && !i.primary)
    );

    let o = s.table_details(&table("sales", "orders")).await.unwrap();
    assert_eq!(o.foreign_keys.len(), 1);
    let fk = &o.foreign_keys[0];
    assert_eq!(fk.name, "FK_orders_customer");
    assert_eq!(fk.referenced_table, table("sales", "customers"));
    assert_eq!(fk.on_delete.as_deref(), Some("CASCADE"));
    assert_eq!(fk.on_update.as_deref(), Some("NO ACTION"));
    let total = o.columns.iter().find(|c| c.name == "total").unwrap();
    assert_eq!(total.extra.as_deref(), Some("computed persisted"));
    let ver = o.columns.iter().find(|c| c.name == "ver").unwrap();
    assert_eq!(ver.extra.as_deref(), Some("rowversion"));
    let ix = o
        .indexes
        .iter()
        .find(|i| i.name == "IX_orders_customer")
        .unwrap();
    assert_eq!(ix.columns, vec!["customer_id DESC"]);
    assert!(
        ix.definition
            .as_deref()
            .unwrap()
            .contains("INCLUDE ([qty])")
    );
    assert!(
        ix.definition
            .as_deref()
            .unwrap()
            .contains("WHERE ([qty]>(0))")
    );
    assert_eq!(
        err_kind(s.table_details(&table("sales", "nope")).await),
        "notFound"
    );

    let v = db()
        .child(ObjectKind::Folder, folder::SCHEMAS)
        .child(ObjectKind::Schema, "sales")
        .child(ObjectKind::Folder, folder::VIEWS)
        .child(ObjectKind::View, "big_orders");
    let vd = s.table_details(&v).await.unwrap();
    assert_eq!(vd.kind, ObjectKind::View);
    assert_eq!(vd.columns.len(), 3);
}

#[tokio::test]
async fn object_ddl_per_kind() {
    it!();
    let s = fixture().await;
    let t = s.object_ddl(&table("sales", "customers")).await.unwrap();
    assert!(t.contains("CREATE TABLE [sales].[customers] ("));
    assert!(t.contains("[id] INT IDENTITY(1,1) NOT NULL"));
    assert!(t.contains("[email] [dbo].[Email] NULL"));
    assert!(t.contains("CONSTRAINT [DF_customers_credit] DEFAULT ((0))"));
    assert!(t.contains("CONSTRAINT [PK_customers] PRIMARY KEY CLUSTERED ([id] ASC)"));
    assert!(t.contains("CONSTRAINT [UQ_customers_name] UNIQUE NONCLUSTERED ([name] ASC)"));
    assert!(t.contains("CHECK ([credit]>=(0))"));
    assert!(
        t.contains("CREATE TRIGGER [sales].[trg_customers_ins]")
            || t.contains("CREATE TRIGGER sales.trg_customers_ins")
    );
    assert!(t.contains("sp_addextendedproperty"));
    let o = s.object_ddl(&table("sales", "orders")).await.unwrap();
    assert!(o.contains("[total] AS ([qty]*[price]) PERSISTED"));
    assert!(o.contains("[ver] ROWVERSION"));
    assert!(o.contains("CREATE NONCLUSTERED INDEX [IX_orders_customer] ON [sales].[orders] ([customer_id] DESC) INCLUDE ([qty])"));
    assert!(o.contains("ADD CONSTRAINT [FK_orders_customer] FOREIGN KEY ([customer_id]) REFERENCES [sales].[customers] ([id]) ON DELETE CASCADE ON UPDATE NO ACTION"));

    let sc = schema("sales");
    let view = sc
        .child(ObjectKind::Folder, folder::VIEWS)
        .child(ObjectKind::View, "big_orders");
    assert!(
        s.object_ddl(&view)
            .await
            .unwrap()
            .contains("CREATE VIEW sales.big_orders")
    );
    let func = sc
        .child(ObjectKind::Folder, folder::FUNCTIONS)
        .child(ObjectKind::Function, "fn_double");
    assert!(s.object_ddl(&func).await.unwrap().contains("RETURN @x * 2"));
    let proc_ = sc
        .child(ObjectKind::Folder, folder::PROCEDURES)
        .child(ObjectKind::Procedure, "sp_customer_count");
    assert!(s.object_ddl(&proc_).await.unwrap().contains("COUNT(*)"));
    let seq = sc
        .child(ObjectKind::Folder, folder::SEQUENCES)
        .child(ObjectKind::Sequence, "order_seq");
    assert_eq!(
        s.object_ddl(&seq).await.unwrap(),
        "CREATE SEQUENCE [sales].[order_seq] AS bigint START WITH 1000 INCREMENT BY 5 MINVALUE -9223372036854775808 MAXVALUE 9223372036854775807 NO CYCLE CACHE;"
    );
    let syn = schema("dbo")
        .child(ObjectKind::Folder, folder::SYNONYMS)
        .child(ObjectKind::Synonym, "cust");
    assert_eq!(
        s.object_ddl(&syn).await.unwrap(),
        "CREATE SYNONYM [dbo].[cust] FOR [sales].[customers];"
    );
    let ty = sc
        .child(ObjectKind::Folder, folder::TYPES)
        .child(ObjectKind::Type, "LineList");
    assert!(
        s.object_ddl(&ty)
            .await
            .unwrap()
            .starts_with("CREATE TYPE [sales].[LineList] AS TABLE (")
    );
    let alias = schema("dbo")
        .child(ObjectKind::Folder, folder::TYPES)
        .child(ObjectKind::Type, "Email");
    assert_eq!(
        s.object_ddl(&alias).await.unwrap(),
        "CREATE TYPE [dbo].[Email] FROM NVARCHAR(100) NOT NULL;"
    );
    let idx = table("sales", "orders")
        .child(ObjectKind::Folder, folder::INDEXES)
        .child(ObjectKind::Index, "IX_orders_customer");
    assert!(
        s.object_ddl(&idx)
            .await
            .unwrap()
            .starts_with("CREATE NONCLUSTERED INDEX")
    );
    assert!(
        s.object_ddl(&sc)
            .await
            .unwrap()
            .starts_with("CREATE SCHEMA [sales] AUTHORIZATION [dbo]")
    );
    assert!(
        s.object_ddl(&db())
            .await
            .unwrap()
            .starts_with("CREATE DATABASE [dbstudio_it]")
    );
    assert_eq!(
        err_kind(s.object_ddl(&table("sales", "nope")).await),
        "notFound"
    );
}

fn req(t: ObjectPath, limit: u64) -> TableDataRequest {
    TableDataRequest {
        query_id: uuid::Uuid::new_v4(),
        table: t,
        filters: vec![],
        raw_where: None,
        sort: vec![],
        offset: 0,
        limit,
        batch_size: None,
    }
}

async fn fetch(
    s: &dyn dbstudio_core::Session,
    r: TableDataRequest,
) -> dbstudio_core::Result<dbstudio_core::ResultSet> {
    let mut sink = dbstudio_core::CollectingSink::new();
    s.fetch_table(r, &mut sink).await?;
    let res = sink.into_results();
    assert_eq!(
        res.len(),
        1,
        "fetch_table must produce exactly one statement"
    );
    Ok(rows_of(&res[0]).clone())
}

#[tokio::test]
async fn paging_is_deterministic_and_complete() {
    it!();
    let s = fixture().await;
    let t = table("dbo", "paging");
    let mut seen = Vec::new();
    for page in 0..5 {
        let mut r = req(t.clone(), 500);
        r.offset = page * 500;
        r.sort = vec![SortKey {
            column: "grp".into(),
            descending: false,
        }];
        let rs = fetch(&*s, r).await.unwrap();
        assert_eq!(rs.rows.len(), 500);
        seen.extend(rs.rows.iter().map(|r| r[0].clone()));
    }
    let mut ids: Vec<i64> = seen
        .iter()
        .map(|v| if let Value::Int(i) = v { *i } else { 0 })
        .collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 2500, "every row exactly once across pages");
    let n = s
        .count_table(TableCountRequest {
            table: t.clone(),
            filters: vec![],
            raw_where: None,
            query_id: None,
        })
        .await
        .unwrap();
    assert_eq!(n, 2500);
    // Keyless table pages without error.
    let rs = fetch(&*s, req(table("dbo", "nokey"), 10)).await.unwrap();
    assert_eq!(rs.rows.len(), 2);
    // Limit 0 yields the columns only.
    let rs = fetch(&*s, req(t, 0)).await.unwrap();
    assert!(rs.rows.is_empty() && rs.columns.len() == 3);
}

#[tokio::test]
async fn filters_and_raw_where() {
    it!();
    let s = fixture().await;
    let t = table("sales", "customers");
    let f = |op, v: &[&str]| ColumnFilter {
        column: "name".into(),
        op,
        values: v.iter().map(|x| x.to_string()).collect(),
    };
    let names = |rs: &dbstudio_core::ResultSet| -> Vec<String> {
        rs.rows.iter().map(|r| text(&r[1])).collect()
    };
    let mut r = req(t.clone(), 100);
    r.filters = vec![f(FilterOp::Contains, &["100% _"])];
    assert_eq!(names(&fetch(&*s, r).await.unwrap()), vec!["Linus 100% _x_"]);
    let mut r = req(t.clone(), 100);
    r.filters = vec![f(FilterOp::Contains, &["ADA"])];
    assert_eq!(
        names(&fetch(&*s, r).await.unwrap()),
        vec!["Ada Lovelace"],
        "case-insensitive"
    );
    let mut r = req(t.clone(), 100);
    r.filters = vec![f(FilterOp::StartsWith, &["gr"])];
    assert_eq!(names(&fetch(&*s, r).await.unwrap()), vec!["Grace Hopper"]);
    let mut r = req(t.clone(), 100);
    r.filters = vec![f(FilterOp::EndsWith, &["😀"])];
    assert_eq!(names(&fetch(&*s, r).await.unwrap()), vec!["Zoë 😀"]);
    let mut r = req(t.clone(), 100);
    r.filters = vec![f(FilterOp::In, &["Ada Lovelace", "Grace Hopper"])];
    assert_eq!(fetch(&*s, r).await.unwrap().rows.len(), 2);
    let mut r = req(t.clone(), 100);
    r.filters = vec![ColumnFilter {
        column: "email".into(),
        op: FilterOp::IsNull,
        values: vec![],
    }];
    assert_eq!(fetch(&*s, r).await.unwrap().rows.len(), 2);
    let mut r = req(t.clone(), 100);
    r.filters = vec![ColumnFilter {
        column: "email".into(),
        op: FilterOp::NotContains,
        values: vec!["example".into()],
    }];
    assert_eq!(
        fetch(&*s, r).await.unwrap().rows.len(),
        1,
        "NULL emails are excluded by NOT LIKE"
    );
    let mut r = req(t.clone(), 100);
    r.filters = vec![ColumnFilter {
        column: "id".into(),
        op: FilterOp::Ge,
        values: vec!["3".into()],
    }];
    assert_eq!(fetch(&*s, r).await.unwrap().rows.len(), 2);
    let mut r = req(t.clone(), 100);
    r.raw_where = Some("credit > 5 AND name LIKE '%a%' -- trailing".into());
    assert_eq!(fetch(&*s, r).await.unwrap().rows.len(), 1);
    // Raw WHERE: syntax errors and smuggled statements are rejected before running.
    for bad in [
        "credit >",
        "1 = 1; DROP TABLE sales.customers",
        "1 = 1) UNION SELECT 1 --",
        "(credit > 1",
    ] {
        let mut r = req(t.clone(), 100);
        r.raw_where = Some(bad.into());
        let e = fetch(&*s, r).await.unwrap_err();
        assert!(is_query_err(&e), "{bad}: {e}");
        if let dbstudio_core::Error::Query { position, .. } = e {
            assert!(position.is_none());
        }
    }
    let n = s
        .count_table(TableCountRequest {
            table: t,
            filters: vec![],
            raw_where: Some("credit > 5".into()),
            query_id: None,
        })
        .await
        .unwrap();
    assert_eq!(n, 2);
    // The table survived.
    assert_eq!(
        rows_of(&run(&*s, "SELECT COUNT(*) FROM sales.customers").await[0]).rows[0][0],
        Value::Int(4)
    );
}

#[tokio::test]
async fn search_objects_ranking_and_scope() {
    it!();
    let s = fixture().await;
    let q = |query: &str, scope: ObjectPath, kinds: Vec<ObjectKind>| ObjectSearchRequest {
        query: query.into(),
        scope,
        kinds,
        limit: 50,
    };
    // Root scope searches the session's database; `cust` finds the synonym only when asked.
    let r = s
        .search_objects(q("CUSTOM", ObjectPath::root(), vec![]))
        .await
        .unwrap();
    assert_eq!(
        r.iter().map(|n| n.name.as_str()).collect::<Vec<_>>(),
        vec!["customers", "sp_customer_count"]
    );
    assert_eq!(r[0].path, table("sales", "customers"));
    assert_eq!(
        r[0].detail.as_deref(),
        Some("dbstudio_it.sales"),
        "root scope shows database.schema"
    );
    let r = s
        .search_objects(q("o", schema("sales"), vec![]))
        .await
        .unwrap();
    let names: Vec<_> = r.iter().map(|n| n.name.as_str()).collect();
    // prefix matches (orders, order_seq) before substring matches, each by name.
    assert_eq!(&names[..2], &["order_seq", "orders"]);
    assert!(names.contains(&"sp_customer_count"));
    let r = s
        .search_objects(q("cust", db(), vec![ObjectKind::Synonym]))
        .await
        .unwrap();
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].kind, ObjectKind::Synonym);
    assert_eq!(
        r[0].path,
        schema("dbo")
            .child(ObjectKind::Folder, folder::SYNONYMS)
            .child(ObjectKind::Synonym, "cust")
    );
    assert!(
        s.search_objects(q("  ", db(), vec![]))
            .await
            .unwrap()
            .is_empty()
    );
    // `%` and `_` are literal.
    assert!(
        s.search_objects(q("%", db(), vec![]))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        s.search_objects(q("order_s", db(), vec![]))
            .await
            .unwrap()
            .len()
            == 1
    );
    let mut lim = q("o", db(), vec![]);
    lim.limit = 2;
    assert_eq!(s.search_objects(lim).await.unwrap().len(), 2);
}
