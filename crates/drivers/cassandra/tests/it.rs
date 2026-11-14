//! Integration tests against `scylla` (ScyllaDB 2026.1, plus its TLS port) and `cassandra5`
//! (Cassandra 5.0). They run only with `DBSTUDIO_IT=1` and a reachable container (one at a time:
//! `docker compose ... up -d --wait scylla` or `cassandra5`); a missing container skips the test
//! unless `DBSTUDIO_IT_STRICT=1`.

mod common;

use common::*;
use dbstudio_core::{
    BulkInsertRequest, CellEdit, ChangeSet, ChildrenPageRequest, CollectingSink, ColumnFilter,
    DdlOptions, DdlRequest, EditNote, EditValue, EndOutcome, Error, ExplainRequest, ExtValue,
    FilterOp, KeyChangeSet, LeaseContext, ModelDetail, NotEditableReason, ObjectKind, ObjectPath,
    ObjectSearchRequest, OnConflict, PlanFormat, QueryRequest, ResultSink, RowChange, RowChangeSet,
    SchemaModelRequest, Session, SortKey, StatementOutcome, TableCountRequest, TableDataRequest,
    TableStreamRequest, TlsMode, TxOptions, Value, execute_collect,
};
use uuid::Uuid;

fn names(nodes: &[dbstudio_core::ObjectNode]) -> Vec<String> {
    nodes.iter().map(|n| n.name.clone()).collect()
}

fn int(v: i64) -> EditValue {
    EditValue::Value(Value::Int(v))
}

fn cell(c: &str, v: EditValue) -> CellEdit {
    CellEdit {
        column: c.into(),
        value: v,
    }
}

// ---------------------------------------------------------------------------------------------
// connection
// ---------------------------------------------------------------------------------------------

async fn t_connect(c: &Ctx) {
    let s = c.session.as_ref();
    s.ping().await.unwrap();
    let info = s.server_info().await.unwrap();
    if c.server.scylla {
        assert_eq!(info.product, "ScyllaDB");
        assert_eq!(info.extra["flavor"], "scylladb");
    } else {
        assert_eq!(info.product, "Apache Cassandra");
        assert_eq!(info.extra["flavor"], "cassandra");
        assert!(info.version.starts_with('5'), "{}", info.version);
    }
    assert!(!info.extra["clusterName"].is_empty());
    let caps = s.capabilities().expect("flavour capabilities");
    assert_eq!(caps.triggers, !c.server.scylla);
    assert!(caps.sql && caps.leases && caps.explain_analyze && !caps.transactions);
    assert_eq!(s.script_dialect(), dbstudio_core::ScriptDialect::Cql);

    // Wrong password, unknown user.
    let mut cfg = config(c.server, "dbstudio");
    let err = match c.server.scylla {
        true => dbstudio_driver_cassandra::ScyllaDriver::new()
            .connect(&cfg, Some(&dbstudio_core::Secret::new("wrong")))
            .await
            .err(),
        false => dbstudio_driver_cassandra::CassandraDriver::new()
            .connect(&cfg, Some(&dbstudio_core::Secret::new("wrong")))
            .await
            .err(),
    }
    .expect("must fail");
    assert_eq!(err.kind(), "authentication", "{err}");
    // A profile keyspace that does not exist is a `notFound` when a lease or query uses it.
    cfg.database = Some("no_such_keyspace".into());
    let session = connect_cfg(c.server, &cfg, "dbstudio").await.unwrap();
    assert!(session.open_lease(Default::default()).await.is_err());
    session.close().await.unwrap();
    // Single node mode (what an SSH tunnel uses) works against the same server.
    let mut cfg = config(c.server, "dbstudio");
    cfg.options.insert("cql.singleNode".into(), "true".into());
    let single = connect_cfg(c.server, &cfg, "dbstudio").await.unwrap();
    single.ping().await.unwrap();
    assert!(single.server_info().await.unwrap().extra.contains_key("notice"));
    single.close().await.unwrap();
}

async fn t_read_only(c: &Ctx) {
    let ks = c.keyspace("ro").await;
    run(c.session.as_ref(), Some(&ks.name), "CREATE TABLE t (id int PRIMARY KEY, v text)").await;
    let mut cfg = config(c.server, "dbstudio");
    cfg.read_only = true;
    let ro = connect_cfg(c.server, &cfg, "dbstudio").await.unwrap();
    let err = try_run(ro.as_ref(), Some(&ks.name), "INSERT INTO t (id, v) VALUES (1, 'x')")
        .await
        .unwrap_err();
    assert_eq!(err.kind(), "readOnly", "{err}");
    run(ro.as_ref(), Some(&ks.name), "SELECT * FROM t").await;
    run(ro.as_ref(), Some(&ks.name), "CONSISTENCY ONE").await;
    let ed = ro.editability(&ks.table("t")).await.unwrap();
    assert!(!ed.editable && ed.reason == Some(NotEditableReason::ReadOnlyConnection));
    let change = ChangeSet::Rows(RowChangeSet {
        table: ks.table("t"),
        key_columns: vec!["id".into()],
        changes: vec![RowChange::Insert {
            values: vec![cell("id", int(1))],
        }],
    });
    assert_eq!(ro.apply_changes(change).await.unwrap_err().kind(), "readOnly");
    let lease = ro
        .open_lease(dbstudio_core::LeaseOptions {
            database: Some(ks.name.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        lease_run(lease.as_ref(), "DELETE FROM t WHERE id = 1")
            .await
            .unwrap_err()
            .kind(),
        "readOnly"
    );
    assert_eq!(
        lease
            .bulk_insert(BulkInsertRequest {
                table: ks.table("t"),
                columns: vec!["id".into()],
                rows: vec![vec![int(1)]],
                on_conflict: OnConflict::Update,
                conflict_columns: vec![],
            })
            .await
            .unwrap_err()
            .kind(),
        "readOnly"
    );
    lease.close().await.unwrap();
    ro.close().await.unwrap();
}

async fn t_tls(c: &Ctx) {
    if !c.server.scylla {
        return;
    }
    let port = std::env::var(TLS_PORT_ENV)
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(SCYLLA_TLS_PORT);
    let certs = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../docker/certs");
    let ca = certs.join("ca.crt").to_string_lossy().to_string();
    let other_ca = certs.join("other-ca.crt").to_string_lossy().to_string();
    let mut cfg = config(c.server, "dbstudio");
    cfg.port = port;
    let connect = |cfg: ConnectionConfig| async move { connect_cfg(SCYLLA, &cfg, "dbstudio").await };

    cfg.tls.mode = TlsMode::Require;
    connect(cfg.clone()).await.unwrap().ping().await.unwrap();
    cfg.tls.mode = TlsMode::VerifyCa;
    cfg.tls.ca_cert_path = Some(ca.clone());
    connect(cfg.clone()).await.unwrap().ping().await.unwrap();
    // The driver passes the node IP as the name: verification uses `tls.server_name`/host.
    cfg.tls.mode = TlsMode::VerifyFull;
    cfg.host = "localhost".into();
    connect(cfg.clone()).await.unwrap().ping().await.unwrap();
    cfg.host = "127.0.0.1".into();
    connect(cfg.clone()).await.unwrap().ping().await.unwrap();
    // As through an SSH tunnel: connect to 127.0.0.1, verify against the original host name.
    cfg.tls.server_name = Some("scylla".into());
    connect(cfg.clone()).await.unwrap().ping().await.unwrap();
    // Wrong name: rejected by verifyFull, accepted by verifyCa.
    cfg.tls.server_name = Some("wrong.example.com".into());
    let err = connect(cfg.clone()).await.err().expect("wrong name must fail");
    assert_eq!(err.kind(), "tls", "{err}");
    cfg.tls.mode = TlsMode::VerifyCa;
    connect(cfg.clone()).await.unwrap().ping().await.unwrap();
    // Wrong CA.
    cfg.tls.server_name = None;
    cfg.tls.ca_cert_path = Some(other_ca);
    let err = connect(cfg.clone()).await.err().expect("wrong CA must fail");
    assert_eq!(err.kind(), "tls", "{err}");
    // Plain text to the TLS port fails.
    cfg.tls.mode = TlsMode::Disable;
    assert!(connect(cfg).await.is_err());
}

// ---------------------------------------------------------------------------------------------
// tree, details, search, model, ddl
// ---------------------------------------------------------------------------------------------

async fn t_tree(c: &Ctx) {
    let s = c.session.as_ref();
    let ks = c.keyspace("tree").await;
    let k = Some(ks.name.as_str());
    run(s, k, "CREATE TYPE address (street text, zip int)").await;
    run(
        s,
        k,
        "CREATE TABLE users (id uuid PRIMARY KEY, name text, email text, tags set<text>, home frozen<address>) WITH comment = 'people'",
    )
    .await;
    run(
        s,
        k,
        "CREATE TABLE events (day date, ts timeuuid, kind text, PRIMARY KEY (day, ts)) WITH CLUSTERING ORDER BY (ts DESC)",
    )
    .await;
    run(s, k, "CREATE TABLE hits (page text PRIMARY KEY, n counter)").await;
    run(s, k, "CREATE INDEX users_name_idx ON users (name)").await;
    run(
        s,
        k,
        "CREATE MATERIALIZED VIEW users_by_email AS SELECT * FROM users WHERE email IS NOT NULL AND id IS NOT NULL PRIMARY KEY (email, id)",
    )
    .await;

    // Root: user keyspaces only by default.
    let root = s.list_children(&ObjectPath::root()).await.unwrap();
    assert!(names(&root).contains(&ks.name));
    assert!(!names(&root).iter().any(|n| n.starts_with("system")), "{:?}", names(&root));
    assert!(root.iter().all(|n| n.kind == ObjectKind::Database));

    // Folders of the keyspace (only non-empty ones).
    let folders = s.list_children(&ks.path()).await.unwrap();
    assert_eq!(names(&folders), vec!["tables", "materializedViews", "types"]);
    assert!(folders.iter().all(|n| n.kind == ObjectKind::Folder && n.has_children));
    let tables = s.list_children(&folders[0].path).await.unwrap();
    assert_eq!(names(&tables), vec!["events", "hits", "users"]);
    assert!(tables.iter().all(|n| n.kind == ObjectKind::Table));
    let views = s.list_children(&folders[1].path).await.unwrap();
    assert_eq!(names(&views), vec!["users_by_email"]);
    assert_eq!(views[0].kind, ObjectKind::MaterializedView);
    let types = s.list_children(&folders[2].path).await.unwrap();
    assert_eq!(names(&types), vec!["address"]);
    assert_eq!(types[0].kind, ObjectKind::Type);

    // Table children: columns and indexes folders; paths mirror the tree.
    let users = ks.table("users");
    let sub = s.list_children(&users).await.unwrap();
    assert_eq!(names(&sub), vec!["columns", "indexes"]);
    let cols = s.list_children(&sub[0].path).await.unwrap();
    assert_eq!(names(&cols), vec!["id", "email", "home", "name", "tags"]);
    assert_eq!(cols[0].kind, ObjectKind::Column);
    assert!(cols[0].detail.as_deref().unwrap().contains("partition key"));
    let idx = s.list_children(&sub[1].path).await.unwrap();
    assert_eq!(names(&idx), vec!["users_name_idx"]);
    assert_eq!(idx[0].path, users.child(ObjectKind::Folder, "indexes").child(ObjectKind::Index, "users_name_idx"));
    // Paths without Folder segments are accepted too.
    let short = ks.path().child(ObjectKind::Table, "users");
    assert_eq!(s.list_children(&short).await.unwrap().len(), 2);
    // A page of children (default implementation) with a glob.
    let page = s
        .list_children_page(ChildrenPageRequest {
            parent: folders[0].path.clone(),
            cursor: None,
            limit: 2,
            pattern: None,
        })
        .await
        .unwrap();
    assert_eq!(page.nodes.len(), 2);
    assert!(page.next_cursor.is_some());

    // Details.
    let d = s.table_details(&users).await.unwrap();
    assert_eq!(d.kind, ObjectKind::Table);
    assert_eq!(d.comment.as_deref(), Some("people"));
    assert_eq!(d.columns.len(), 5);
    assert!(d.columns[0].primary_key && !d.columns[0].nullable && d.columns[0].type_name == "uuid");
    assert_eq!(d.columns.iter().find(|c| c.name == "tags").unwrap().type_name, "set<text>");
    assert_eq!(d.columns.iter().find(|c| c.name == "home").unwrap().type_name, "frozen<address>");
    assert!(d.indexes[0].primary && d.indexes[0].columns == vec!["id"]);
    assert!(d.indexes.iter().any(|i| i.name == "users_name_idx" && i.columns == vec!["name"]));
    let ev = s.table_details(&ks.table("events")).await.unwrap();
    assert_eq!(ev.indexes[0].columns, vec!["day", "ts"]);
    assert!(ev.columns[1].extra.as_deref().unwrap().contains("desc"));
    let mv = s.table_details(&ks.view("users_by_email")).await.unwrap();
    assert_eq!(mv.kind, ObjectKind::MaterializedView);

    // Editability.
    let e = s.editability(&users).await.unwrap();
    assert!(e.editable && !e.atomic && e.key_columns == vec!["id"]);
    assert_eq!(e.notes, vec![EditNote::ConditionalWrites]);
    let e = s.editability(&ks.table("hits")).await.unwrap();
    assert!(!e.editable && e.reason == Some(NotEditableReason::Unsupported));
    let e = s.editability(&ks.view("users_by_email")).await.unwrap();
    assert!(!e.editable && e.reason == Some(NotEditableReason::NotATable));

    // Search.
    let found = s
        .search_objects(ObjectSearchRequest {
            query: "USER".into(),
            scope: ObjectPath::root(),
            kinds: vec![],
            limit: 10,
        })
        .await
        .unwrap();
    assert_eq!(names(&found), vec!["users", "users_by_email"]);
    assert_eq!(found[0].path, users);
    assert_eq!(found[0].detail.as_deref(), Some(ks.name.as_str()));
    let found = s
        .search_objects(ObjectSearchRequest {
            query: "address".into(),
            scope: ks.path(),
            kinds: vec![ObjectKind::Type],
            limit: 10,
        })
        .await
        .unwrap();
    assert_eq!(names(&found), vec!["address"]);
    assert!(
        s.search_objects(ObjectSearchRequest {
            query: "  ".into(),
            scope: ObjectPath::root(),
            kinds: vec![],
            limit: 10,
        })
        .await
        .unwrap()
        .is_empty()
    );

    // Schema model.
    let model = s
        .schema_model(SchemaModelRequest {
            scope: ks.path(),
            tables: vec![],
            include_views: true,
            include_comments: true,
            detail: ModelDetail::Full,
            sample_size: None,
        })
        .await
        .unwrap();
    assert_eq!(model.tables.len(), 4);
    assert!(model.relations.is_empty() && !model.inferred);
    let u = model.tables.iter().find(|t| t.name == "users").unwrap();
    assert_eq!(u.primary_key, vec!["id"]);
    assert_eq!(u.comment.as_deref(), Some("people"));
    let keys_only = s
        .schema_model(SchemaModelRequest {
            scope: ks.path(),
            tables: vec![users.clone()],
            include_views: false,
            include_comments: false,
            detail: ModelDetail::Keys,
            sample_size: None,
        })
        .await
        .unwrap();
    assert_eq!(keys_only.tables.len(), 1);
    assert_eq!(keys_only.tables[0].columns.len(), 1);

    // DDL text.
    let ddl = s.object_ddl(&users).await.unwrap();
    assert!(ddl.contains(&format!("CREATE TABLE {}.users", ks.name)), "{ddl}");
    assert!(ddl.contains("PRIMARY KEY (id)") && ddl.contains("CREATE INDEX users_name_idx"), "{ddl}");
    let ks_ddl = s.object_ddl(&ks.path()).await.unwrap();
    assert!(ks_ddl.starts_with(&format!("CREATE KEYSPACE {}", ks.name)), "{ks_ddl}");
    assert!(ks_ddl.contains("CREATE TYPE") && ks_ddl.contains("CREATE MATERIALIZED VIEW"));
    let type_ddl = s
        .object_ddl(&ks.path().child(ObjectKind::Folder, "types").child(ObjectKind::Type, "address"))
        .await
        .unwrap();
    assert!(type_ddl.contains("street text"), "{type_ddl}");
    assert!(s.object_ddl(&cols[0].path).await.is_err());

    // The schema cache is dropped by DDL run through the driver.
    run(s, k, "CREATE TABLE late (id int PRIMARY KEY)").await;
    let tables = s.list_children(&folders[0].path).await.unwrap();
    assert!(names(&tables).contains(&"late".to_string()));
    run(s, k, "DROP TABLE late").await;
    assert!(!names(&s.list_children(&folders[0].path).await.unwrap()).contains(&"late".to_string()));

    // System keyspaces show up in a `system` folder with `cql.showSystem`.
    let mut cfg = config(c.server, "dbstudio");
    cfg.options.insert("cql.showSystem".into(), "true".into());
    let sys = connect_cfg(c.server, &cfg, "dbstudio").await.unwrap();
    let root = sys.list_children(&ObjectPath::root()).await.unwrap();
    let folder = root.iter().find(|n| n.kind == ObjectKind::Folder).expect("system folder");
    assert_eq!(folder.name, "system");
    let sysks = sys.list_children(&folder.path).await.unwrap();
    assert!(names(&sysks).contains(&"system_schema".to_string()));
    let inner = sys.list_children(&sysks.iter().find(|n| n.name == "system_schema").unwrap().path).await.unwrap();
    assert!(!inner.is_empty());
    sys.close().await.unwrap();

    // Bad paths.
    assert_eq!(
        s.list_children(&ObjectPath::root().child(ObjectKind::Table, "x")).await.unwrap_err().kind(),
        "invalid"
    );
    assert_eq!(
        s.list_children(&ObjectPath::root().child(ObjectKind::Database, "nope")).await.unwrap_err().kind(),
        "notFound"
    );
}

async fn t_ddl_roundtrip(c: &Ctx) {
    let s = c.session.as_ref();
    let src = c.keyspace("ddl_src").await;
    let dst = c.keyspace("ddl_dst").await;
    let k = Some(src.name.as_str());
    run(s, k, "CREATE TYPE geo (lat double, lon double)").await;
    run(s, k, "CREATE TYPE place (name text, loc frozen<geo>, tags list<text>)").await;
    run(
        s,
        k,
        "CREATE TABLE trips (owner text, day date, seq int, note text STATIC, stop frozen<place>, extra map<text, frozen<list<int>>>, \
         PRIMARY KEY ((owner, day), seq)) WITH CLUSTERING ORDER BY (seq DESC) AND comment = 'trips' AND default_time_to_live = 3600 AND gc_grace_seconds = 7200",
    )
    .await;
    run(s, k, "CREATE TABLE plain (id int PRIMARY KEY, v text)").await;
    run(s, k, "CREATE INDEX plain_v ON plain (v)").await;
    run(
        s,
        k,
        "CREATE MATERIALIZED VIEW plain_by_v AS SELECT * FROM plain WHERE v IS NOT NULL AND id IS NOT NULL PRIMARY KEY (v, id)",
    )
    .await;

    let items = s
        .generate_ddl(DdlRequest {
            scope: src.path(),
            objects: vec![],
            options: DdlOptions {
                qualify_schema: false,
                ..DdlOptions::default()
            },
        })
        .await
        .unwrap();
    assert!(items.iter().all(|i| i.language == dbstudio_core::DdlLanguage::Cql));
    let position = |kind: ObjectKind| items.iter().position(|i| i.kind == kind).unwrap();
    assert_eq!(items[0].kind, ObjectKind::Database);
    assert!(position(ObjectKind::Type) < position(ObjectKind::Table));
    assert!(position(ObjectKind::Table) < position(ObjectKind::Index));
    assert!(items.iter().any(|i| i.kind == ObjectKind::MaterializedView));
    // Run everything but the keyspace into the empty keyspace.
    for item in items.iter().filter(|i| i.kind != ObjectKind::Database) {
        try_run(s, Some(&dst.name), &item.sql)
            .await
            .unwrap_or_else(|e| panic!("{}: {e}", item.sql));
    }
    let shape = |ks: &Keyspace| {
        let path = ks.path();
        async move {
            let m = s
                .schema_model(SchemaModelRequest {
                    scope: path,
                    tables: vec![],
                    include_views: true,
                    include_comments: true,
                    detail: ModelDetail::Full,
                    sample_size: None,
                })
                .await
                .unwrap();
            m.tables
                .iter()
                .map(|t| {
                    (
                        t.name.clone(),
                        t.comment.clone(),
                        t.primary_key.clone(),
                        t.columns
                            .iter()
                            .map(|c| (c.name.clone(), c.type_name.clone(), c.extra.clone()))
                            .collect::<Vec<_>>(),
                    )
                })
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(shape(&src).await, shape(&dst).await);
    let t_src = s.table_details(&src.table("plain")).await.unwrap();
    let t_dst = s.table_details(&dst.table("plain")).await.unwrap();
    assert_eq!(t_src.indexes.len(), t_dst.indexes.len());
    // Options survive (TTL and gc_grace_seconds appear in the generated text).
    let ddl = s.object_ddl(&dst.table("trips")).await.unwrap();
    assert!(ddl.contains("default_time_to_live = 3600") && ddl.contains("gc_grace_seconds = 7200"), "{ddl}");
    assert!(ddl.contains("CLUSTERING ORDER BY (seq DESC)") && ddl.contains("note text STATIC"), "{ddl}");

    // Restricted to a table: no keyspace, the table plus the types it needs.
    let only = s
        .generate_ddl(DdlRequest {
            scope: src.path(),
            objects: vec![src.table("trips")],
            options: DdlOptions::default(),
        })
        .await
        .unwrap();
    assert!(!only.iter().any(|i| i.kind == ObjectKind::Database));
    assert_eq!(only.iter().filter(|i| i.kind == ObjectKind::Type).count(), 2);
    assert!(only.iter().all(|i| i.path.last().unwrap().name != "plain"));
    // With drop and IF NOT EXISTS the script can run twice.
    let again = s
        .generate_ddl(DdlRequest {
            scope: src.path(),
            objects: vec![],
            options: DdlOptions {
                qualify_schema: false,
                if_not_exists: true,
                ..DdlOptions::default()
            },
        })
        .await
        .unwrap();
    for item in again.iter().filter(|i| i.kind != ObjectKind::Database) {
        try_run(s, Some(&dst.name), &item.sql)
            .await
            .unwrap_or_else(|e| panic!("{}: {e}", item.sql));
    }
    assert_eq!(
        s.generate_ddl(DdlRequest {
            scope: ObjectPath::root().child(ObjectKind::Database, "nope"),
            objects: vec![],
            options: DdlOptions::default(),
        })
        .await
        .unwrap_err()
        .kind(),
        "notFound"
    );
}

// ---------------------------------------------------------------------------------------------
// types
// ---------------------------------------------------------------------------------------------

const TYPES_COLUMNS: &str = "id int PRIMARY KEY, a_ascii ascii, a_bigint bigint, a_blob blob, a_bool boolean, a_date date, \
    a_decimal decimal, a_double double, a_duration duration, a_float float, a_inet inet, a_int int, a_small smallint, \
    a_text text, a_time time, a_ts timestamp, a_timeuuid timeuuid, a_tiny tinyint, a_uuid uuid, a_varint varint, \
    l list<int>, s set<text>, m_text map<text, int>, m_int map<int, text>, t tuple<int, text>, \
    u frozen<address>, lu list<frozen<address>>";

async fn t_types(c: &Ctx) {
    let s = c.session.as_ref();
    let ks = c.keyspace("types").await;
    let k = Some(ks.name.as_str());
    run(s, k, "CREATE TYPE geo (lat double, lon double)").await;
    run(s, k, "CREATE TYPE address (street text, zip int, loc frozen<geo>, tags list<text>)").await;
    // Vectors need Cassandra 5 / a recent Scylla.
    let vector = try_run(
        s,
        k,
        &format!("CREATE TABLE all_types ({TYPES_COLUMNS}, v vector<float, 3>)"),
    )
    .await
    .is_ok();
    if !vector {
        run(s, k, &format!("CREATE TABLE all_types ({TYPES_COLUMNS})")).await;
    }
    let vec_col = if vector { ", v" } else { "" };
    let vec_val = if vector { ", [1.0, 2.0, 3.0]" } else { "" };
    run(
        s,
        k,
        &format!(
            "INSERT INTO all_types (id, a_ascii, a_bigint, a_blob, a_bool, a_date, a_decimal, a_double, a_duration, a_float, \
             a_inet, a_int, a_small, a_text, a_time, a_ts, a_timeuuid, a_tiny, a_uuid, a_varint, l, s, m_text, m_int, t, u, lu{vec_col}) \
             VALUES (1, 'abc', 9007199254740993, 0xdeadbeef, true, '2024-05-31', 12345.6789, 1.5, 1mo2d3ns, 1.5, '192.168.1.1', 7, 300, \
             'héllo', '13:45:00.123456789', '2024-05-31T13:30:45.123+0000', 50554d6e-29bb-11e5-b345-feff819cdc9f, 5, \
             123e4567-e89b-12d3-a456-426614174000, 123456789012345678901234567890, [1, 2, 3], {{'x', 'y'}}, {{'a': 1, 'b': 2}}, \
             {{1: 'one', 2: 'two'}}, (1, 'x'), {{street: 'Main', zip: 12345, loc: {{lat: 1.5, lon: 2.5}}, tags: ['a', 'b']}}, \
             [{{street: 'A', zip: 1, loc: {{lat: 0.5, lon: 0.5}}, tags: []}}]{vec_val})"
        ),
    )
    .await;
    run(s, k, "INSERT INTO all_types (id) VALUES (2)").await;

    let results = run(s, k, "SELECT * FROM all_types").await;
    let rs = result_set(&results[0]);
    let col = |name: &str| rs.columns.iter().position(|c| c.name == name).unwrap();
    let row1 = rs.rows.iter().find(|r| r[col("id")] == Value::Int(1)).unwrap();
    let row2 = rs.rows.iter().find(|r| r[col("id")] == Value::Int(2)).unwrap();
    let get = |name: &str| row1[col(name)].clone();
    assert_eq!(get("a_ascii"), text("abc"));
    assert_eq!(get("a_bigint"), Value::Int(9_007_199_254_740_993));
    assert_eq!(get("a_blob"), Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef]));
    assert_eq!(get("a_bool"), Value::Bool(true));
    assert_eq!(get("a_date"), Value::Date("2024-05-31".into()));
    assert_eq!(get("a_decimal"), Value::Decimal("12345.6789".into()));
    assert_eq!(get("a_double"), Value::Float(1.5));
    assert_eq!(get("a_duration"), Value::Ext(ExtValue::new("duration", "1mo2d3ns")));
    assert_eq!(get("a_float"), Value::Float(1.5));
    assert_eq!(get("a_inet"), text("192.168.1.1"));
    assert_eq!(get("a_int"), Value::Int(7));
    assert_eq!(get("a_small"), Value::Int(300));
    assert_eq!(get("a_text"), text("héllo"));
    assert_eq!(get("a_time"), Value::Time("13:45:00.123456789".into()));
    assert_eq!(get("a_ts"), Value::Timestamp("2024-05-31T13:30:45.123Z".into()));
    assert_eq!(get("a_timeuuid"), Value::Uuid("50554d6e-29bb-11e5-b345-feff819cdc9f".into()));
    assert_eq!(get("a_tiny"), Value::Int(5));
    assert_eq!(get("a_uuid"), Value::Uuid("123e4567-e89b-12d3-a456-426614174000".into()));
    assert_eq!(get("a_varint"), Value::Decimal("123456789012345678901234567890".into()));
    assert_eq!(get("l"), Value::Array(vec![Value::Int(1), Value::Int(2), Value::Int(3)]));
    assert_eq!(get("s"), Value::Array(vec![text("x"), text("y")]));
    assert_eq!(
        get("m_text"),
        Value::Document(vec![("a".into(), Value::Int(1)), ("b".into(), Value::Int(2))])
    );
    assert_eq!(
        get("m_int"),
        Value::Array(vec![
            Value::Array(vec![Value::Int(1), text("one")]),
            Value::Array(vec![Value::Int(2), text("two")]),
        ])
    );
    assert_eq!(get("t"), Value::Array(vec![Value::Int(1), text("x")]));
    let address = Value::Document(vec![
        ("street".into(), text("Main")),
        ("zip".into(), Value::Int(12345)),
        (
            "loc".into(),
            Value::Document(vec![("lat".into(), Value::Float(1.5)), ("lon".into(), Value::Float(2.5))]),
        ),
        ("tags".into(), Value::Array(vec![text("a"), text("b")])),
    ]);
    assert_eq!(get("u"), address);
    match get("lu") {
        Value::Array(items) => assert_eq!(items.len(), 1),
        other => panic!("{other:?}"),
    }
    if vector {
        assert_eq!(
            get("v"),
            Value::Array(vec![Value::Float(1.0), Value::Float(2.0), Value::Float(3.0)])
        );
    }
    for (i, v) in row2.iter().enumerate() {
        if i != col("id") {
            assert_eq!(v, &Value::Null, "{}", rs.columns[i].name);
        }
    }
    assert_eq!(rs.columns[col("m_int")].type_name, "map<int, text>");
    assert_eq!(rs.columns[col("m_int")].value_kind, dbstudio_core::ValueKind::Array);
    assert_eq!(rs.columns[col("m_text")].value_kind, dbstudio_core::ValueKind::Document);
    assert_eq!(rs.columns[col("u")].value_kind, dbstudio_core::ValueKind::Document);

    // Round trip: rows read back are written into a copy with `bulk_insert`.
    let create = if vector {
        format!("CREATE TABLE copy_types ({TYPES_COLUMNS}, v vector<float, 3>)")
    } else {
        format!("CREATE TABLE copy_types ({TYPES_COLUMNS})")
    };
    run(s, k, &create).await;
    let lease = c.lease(k).await;
    let columns: Vec<String> = rs.columns.iter().map(|c| c.name.clone()).collect();
    let result = lease
        .bulk_insert(BulkInsertRequest {
            table: ks.table("copy_types"),
            columns,
            rows: rs
                .rows
                .iter()
                .map(|r| r.iter().cloned().map(EditValue::Value).collect())
                .collect(),
            on_conflict: OnConflict::Update,
            conflict_columns: vec![],
        })
        .await
        .unwrap();
    assert_eq!(result.inserted, 2);
    let copy = run(s, k, "SELECT * FROM copy_types").await;
    let mut a = rs.rows.clone();
    let mut b = result_set(&copy[0]).rows.clone();
    a.sort_by_key(|r| format!("{:?}", r[0]));
    b.sort_by_key(|r| format!("{:?}", r[0]));
    assert_eq!(a, b);
    lease.close().await.unwrap();

    // Editing a collection and UDT cell with typed text.
    let change = ChangeSet::Rows(RowChangeSet {
        table: ks.table("all_types"),
        key_columns: vec!["id".into()],
        changes: vec![RowChange::Update {
            key: vec![Value::Int(2)],
            set: vec![
                cell("l", EditValue::Text("[9, 8]".into())),
                cell("u", EditValue::Value(address.clone())),
                cell("a_varint", EditValue::Text("-42".into())),
                cell("a_bool", EditValue::Text("false".into())),
            ],
        }],
    });
    // Row 2 exists (it only has its key), so the conditional update applies.
    s.apply_changes(change).await.unwrap();
    let rows = select(s, &ks.name, "SELECT l, u, a_varint, a_bool FROM all_types WHERE id = 2").await;
    assert_eq!(rows[0][0], Value::Array(vec![Value::Int(9), Value::Int(8)]));
    assert_eq!(rows[0][1], address);
    assert_eq!(rows[0][2], Value::Decimal("-42".into()));
    assert_eq!(rows[0][3], Value::Bool(false));
}

// ---------------------------------------------------------------------------------------------
// console, paging, leases
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct Rec {
    columns: usize,
    rows: usize,
    messages: Vec<String>,
    ends: Vec<EndOutcome>,
}

impl ResultSink for Rec {
    fn on_columns(&mut self, columns: Vec<dbstudio_core::ColumnMeta>) -> dbstudio_core::Result<()> {
        self.columns += columns.len();
        Ok(())
    }
    fn on_rows(&mut self, rows: Vec<Vec<Value>>) -> dbstudio_core::Result<()> {
        self.rows += rows.len();
        Ok(())
    }
    fn on_message(&mut self, message: String) -> dbstudio_core::Result<()> {
        self.messages.push(message);
        Ok(())
    }
    fn on_statement_end(&mut self, end: dbstudio_core::StatementEnd) -> dbstudio_core::Result<()> {
        self.ends.push(end.outcome);
        Ok(())
    }
}

async fn fill_nums(c: &Ctx, ks: &Keyspace, n: i64) {
    run(
        c.session.as_ref(),
        Some(&ks.name),
        "CREATE TABLE nums (p int, c int, v text, PRIMARY KEY (p, c))",
    )
    .await;
    let lease = c.lease(Some(&ks.name)).await;
    let rows = (0..n)
        .map(|i| vec![int(i % 5), int(i), EditValue::Text(format!("x{i}"))])
        .collect();
    let r = lease
        .bulk_insert(BulkInsertRequest {
            table: ks.table("nums"),
            columns: vec!["p".into(), "c".into(), "v".into()],
            rows,
            on_conflict: OnConflict::Update,
            conflict_columns: vec![],
        })
        .await
        .unwrap();
    assert_eq!(r.inserted, n as u64);
    lease.close().await.unwrap();
}

async fn t_lease(c: &Ctx) {
    let s = c.session.as_ref();
    let ks = c.keyspace("lease").await;
    fill_nums(c, &ks, 2500).await;
    let other = c.keyspace("lease_other").await;
    run(s, Some(&other.name), "CREATE TABLE only_here (id int PRIMARY KEY)").await;
    let lease = c.lease(Some(&ks.name)).await;

    // Pending result: 1000 + 1000 + 500 rows.
    let mut req = QueryRequest::new("SELECT * FROM nums");
    req.max_rows = Some(1000);
    req.batch_size = Some(300);
    let id = req.query_id;
    let mut rec = Rec::default();
    lease.execute(req, &mut rec).await.unwrap();
    assert_eq!(rec.rows, 1000);
    assert_eq!(rec.ends, vec![EndOutcome::Rows { row_count: 1000, truncated: true }]);
    assert_eq!(lease.state().pending_result, Some(id));
    assert_eq!(lease.state().database.as_deref(), Some(ks.name.as_str()));
    let mut rec = Rec::default();
    lease.fetch_more(id, 1000, None, &mut rec).await.unwrap();
    assert_eq!(rec.rows, 1000);
    assert_eq!(rec.columns, 0, "no columns on fetch_more");
    assert_eq!(rec.ends, vec![EndOutcome::Rows { row_count: 1000, truncated: true }]);
    let mut rec = Rec::default();
    lease.fetch_more(id, 1000, None, &mut rec).await.unwrap();
    assert_eq!(rec.rows, 500);
    assert_eq!(rec.ends, vec![EndOutcome::Rows { row_count: 500, truncated: false }]);
    assert_eq!(lease.state().pending_result, None);
    assert_eq!(
        lease.fetch_more(id, 10, None, &mut Rec::default()).await.unwrap_err().kind(),
        "notFound"
    );
    // discard_result.
    let mut req = QueryRequest::new("SELECT * FROM nums");
    req.max_rows = Some(10);
    let id = req.query_id;
    lease.execute(req, &mut Rec::default()).await.unwrap();
    assert!(lease.state().pending_result.is_some());
    lease.discard_result().await.unwrap();
    assert_eq!(lease.state().pending_result, None);
    assert_eq!(
        lease.fetch_more(id, 10, None, &mut Rec::default()).await.unwrap_err().kind(),
        "notFound"
    );

    // cqlsh commands and state.
    let r = lease_run(lease.as_ref(), "CONSISTENCY QUORUM\nCONSISTENCY\nSERIAL CONSISTENCY LOCAL_SERIAL\nTRACING ON").await.unwrap();
    assert_eq!(r.len(), 4);
    assert!(r[0].messages[0].contains("set to QUORUM"));
    assert!(r[1].messages[0].contains("QUORUM"));
    assert!(r[2].messages[0].contains("LOCAL_SERIAL"));
    assert!(r[3].messages[0].contains("enabled"));
    let r = lease_run(lease.as_ref(), "SELECT * FROM nums WHERE p = 1 AND c = 1").await.unwrap();
    assert!(
        r[0].messages.iter().any(|m| m.contains("Tracing session")),
        "{:?}",
        r[0].messages
    );
    lease_run(lease.as_ref(), "TRACING OFF\nCONSISTENCY LOCAL_ONE").await.unwrap();
    assert_eq!(
        lease_run(lease.as_ref(), "CONSISTENCY FOO").await.unwrap_err().kind(),
        "invalid"
    );
    assert_eq!(
        lease_run(lease.as_ref(), "COPY nums TO '/tmp/x.csv'").await.unwrap_err().kind(),
        "unsupported"
    );

    // USE and set_context.
    lease_run(lease.as_ref(), &format!("USE {}", other.name)).await.unwrap();
    assert_eq!(lease.state().database.as_deref(), Some(other.name.as_str()));
    lease_run(lease.as_ref(), "SELECT * FROM only_here").await.unwrap();
    assert!(lease_run(lease.as_ref(), "SELECT * FROM nums").await.is_err());
    lease
        .set_context(LeaseContext {
            database: Some(ks.name.clone()),
            schema: None,
        })
        .await
        .unwrap();
    lease_run(lease.as_ref(), "SELECT * FROM nums LIMIT 1").await.unwrap();
    assert!(
        lease
            .set_context(LeaseContext {
                database: Some("no_such_ks".into()),
                schema: None
            })
            .await
            .is_err()
    );
    assert!(lease_run(lease.as_ref(), "USE no_such_ks").await.is_err());

    // Several statements, batch, LWT.
    let r = lease_run(
        lease.as_ref(),
        "INSERT INTO nums (p, c, v) VALUES (9, 1, 'a'); \
         BEGIN UNLOGGED BATCH INSERT INTO nums (p, c, v) VALUES (9, 2, 'b'); INSERT INTO nums (p, c, v) VALUES (9, 3, 'c'); APPLY BATCH; \
         INSERT INTO nums (p, c, v) VALUES (8, 1, 'lwt') IF NOT EXISTS; \
         INSERT INTO nums (p, c, v) VALUES (8, 1, 'lwt2') IF NOT EXISTS; \
         SELECT count(*) FROM nums WHERE p = 9",
    )
    .await
    .unwrap();
    assert_eq!(r.len(), 5);
    assert!(matches!(r[0].outcome, StatementOutcome::Done));
    assert!(matches!(r[1].outcome, StatementOutcome::Done));
    assert_eq!(result_set(&r[2]).columns[0].name, "[applied]");
    assert_eq!(result_set(&r[2]).rows[0][0], Value::Bool(true));
    assert_eq!(result_set(&r[3]).rows[0][0], Value::Bool(false));
    assert_eq!(result_set(&r[4]).rows[0][0], Value::Int(3));

    // Errors: the position is a character offset of the whole script.
    let err = lease_run(lease.as_ref(), "SELECT 1 FROM nums LIMIT 1;\nSELECT * FRM nums").await.unwrap_err();
    match err {
        Error::Query { position: Some(p), .. } => assert!(p >= 28, "position {p}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        lease_run(lease.as_ref(), "SELECT * FROM missing_table").await.unwrap_err().kind(),
        "query"
    );

    // No transactions.
    assert_eq!(lease.begin(TxOptions::default()).await.unwrap_err().kind(), "unsupported");
    assert_eq!(lease.set_auto_commit(false).await.unwrap_err().kind(), "unsupported");
    lease.set_auto_commit(true).await.unwrap();
    lease.ping().await.unwrap();

    // DESCRIBE through the console (server side or built from the schema).
    let r = lease_run(lease.as_ref(), "DESCRIBE KEYSPACES").await.unwrap();
    let rs = result_set(&r[0]);
    assert!(rs.rows.iter().any(|row| row.iter().any(|v| v == &text(&ks.name))));
    let r = lease_run(lease.as_ref(), &format!("DESCRIBE KEYSPACE {}", ks.name)).await.unwrap();
    let all: String = result_set(&r[0])
        .rows
        .iter()
        .flat_map(|row| row.iter())
        .filter_map(|v| if let Value::Text(t) = v { Some(t.as_str()) } else { None })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(all.contains("CREATE TABLE") && all.contains("nums"), "{all}");
    let r = lease_run(lease.as_ref(), &format!("DESCRIBE TABLE {}.nums", ks.name)).await.unwrap();
    assert_eq!(result_set(&r[0]).rows.len(), 1);
    assert_eq!(
        lease_run(lease.as_ref(), "DESCRIBE FOOBAR").await.unwrap_err().kind(),
        "unsupported"
    );
    lease.close().await.unwrap();
    // A closed lease is gone.
    assert!(lease_run(lease.as_ref(), "SELECT 1 FROM nums").await.is_err());

    // The session runs scripts too (keyspace from `database`).
    let r = run(s, Some(&ks.name), "SELECT count(*) FROM nums WHERE p = 0").await;
    assert_eq!(result_set(&r[0]).rows[0][0], Value::Int(500));
}

async fn t_cancel_timeout(c: &Ctx) {
    let ks = c.keyspace("cancel").await;
    fill_nums(c, &ks, 600).await;
    let lease = c.lease(Some(&ks.name)).await;
    // A long script is cancelled between statements.
    let script = "SELECT count(*) FROM nums;".repeat(600);
    let req = QueryRequest::new(script);
    let id = req.query_id;
    let mut sink = CollectingSink::new();
    let (res, cancelled) = tokio::join!(lease.execute(req, &mut sink), async {
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        lease.cancel(id).await
    });
    cancelled.unwrap();
    assert!(matches!(res, Err(Error::Cancelled)), "{res:?}");
    // The lease is usable afterwards, and cancelling an unknown id is fine.
    lease.cancel(Uuid::new_v4()).await.unwrap();
    lease_run(lease.as_ref(), "SELECT * FROM nums LIMIT 1").await.unwrap();
    // The session cancels by id as well.
    let req = QueryRequest::new("SELECT count(*) FROM nums;".repeat(600));
    let id = req.query_id;
    let mut sink = CollectingSink::new();
    let (res, _) = tokio::join!(c.session.execute(req, &mut sink), async {
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        c.session.cancel(id).await
    });
    assert!(matches!(res, Err(Error::Cancelled)), "{res:?}");
    c.session.cancel(Uuid::new_v4()).await.unwrap();

    // Client side timeout.
    let mut req = QueryRequest::new("SELECT * FROM nums WHERE v = 'x599' ALLOW FILTERING");
    req.timeout_ms = Some(1);
    req.database = Some(ks.name.clone());
    let err = execute_collect(c.session.as_ref(), req).await.unwrap_err();
    assert_eq!(err.kind(), "timeout", "{err}");
    lease.close().await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// data grid
// ---------------------------------------------------------------------------------------------

fn grid_req(table: &ObjectPath, offset: u64, limit: u64) -> TableDataRequest {
    TableDataRequest {
        query_id: Uuid::new_v4(),
        table: table.clone(),
        filters: vec![],
        raw_where: None,
        sort: vec![],
        offset,
        limit,
        batch_size: Some(40),
    }
}

async fn grid(s: &dyn Session, req: TableDataRequest) -> (Vec<Vec<Value>>, Rec) {
    struct Keep(Rec, Vec<Vec<Value>>);
    impl ResultSink for Keep {
        fn on_columns(&mut self, c: Vec<dbstudio_core::ColumnMeta>) -> dbstudio_core::Result<()> {
            self.0.on_columns(c)
        }
        fn on_rows(&mut self, rows: Vec<Vec<Value>>) -> dbstudio_core::Result<()> {
            self.0.rows += rows.len();
            self.1.extend(rows);
            Ok(())
        }
        fn on_message(&mut self, m: String) -> dbstudio_core::Result<()> {
            self.0.on_message(m)
        }
        fn on_statement_end(&mut self, e: dbstudio_core::StatementEnd) -> dbstudio_core::Result<()> {
            self.0.on_statement_end(e)
        }
    }
    let mut sink = Keep(Rec::default(), vec![]);
    s.fetch_table(req, &mut sink).await.unwrap();
    (sink.1, sink.0)
}

fn column_c(rows: &[Vec<Value>], pos: usize) -> Vec<i64> {
    rows.iter()
        .map(|r| match &r[pos] {
            Value::Int(i) => *i,
            other => panic!("{other:?}"),
        })
        .collect()
}

async fn t_grid(c: &Ctx) {
    let s = c.session.as_ref();
    let ks = c.keyspace("grid").await;
    run(s, Some(&ks.name), "CREATE TABLE g (p int, c int, v text, PRIMARY KEY (p, c))").await;
    let lease = c.lease(Some(&ks.name)).await;
    lease
        .bulk_insert(BulkInsertRequest {
            table: ks.table("g"),
            columns: vec!["p".into(), "c".into(), "v".into()],
            rows: (0..250)
                .map(|i| vec![int(1), int(i), EditValue::Text(format!("x{i}"))])
                .collect(),
            on_conflict: OnConflict::Update,
            conflict_columns: vec![],
        })
        .await
        .unwrap();
    lease.close().await.unwrap();
    let t = ks.table("g");
    // Sequential pages use the cached paging state.
    let (rows, rec) = grid(s, grid_req(&t, 0, 100)).await;
    assert_eq!(column_c(&rows, 1), (0..100).collect::<Vec<_>>());
    assert_eq!(rec.ends, vec![EndOutcome::Rows { row_count: 100, truncated: true }]);
    assert_eq!(rec.columns, 3);
    let (rows, _) = grid(s, grid_req(&t, 100, 100)).await;
    assert_eq!(column_c(&rows, 1), (100..200).collect::<Vec<_>>());
    let (rows, rec) = grid(s, grid_req(&t, 200, 100)).await;
    assert_eq!(column_c(&rows, 1), (200..250).collect::<Vec<_>>());
    assert_eq!(rec.ends, vec![EndOutcome::Rows { row_count: 50, truncated: false }]);
    // A jump: closest cached state (100) then skip 50.
    let (rows, _) = grid(s, grid_req(&t, 150, 20)).await;
    assert_eq!(column_c(&rows, 1), (150..170).collect::<Vec<_>>());
    // A query never seen before: read and drop up to the offset.
    let mut req = grid_req(&t, 120, 10);
    req.filters = vec![ColumnFilter {
        column: "p".into(),
        op: FilterOp::Eq,
        values: vec!["1".into()],
    }];
    req.raw_where = Some("c >= 10".into());
    let (rows, _) = grid(s, req).await;
    assert_eq!(column_c(&rows, 1), (130..140).collect::<Vec<_>>());
    // Beyond the end: no rows, columns still sent.
    let (rows, rec) = grid(s, grid_req(&t, 1000, 10)).await;
    assert!(rows.is_empty() && rec.columns == 3);
    assert_eq!(rec.ends, vec![EndOutcome::Rows { row_count: 0, truncated: false }]);

    // Filters: a non-key column needs ALLOW FILTERING, added with a warning.
    let mut req = grid_req(&t, 0, 50);
    req.filters = vec![ColumnFilter {
        column: "v".into(),
        op: FilterOp::Eq,
        values: vec!["x77".into()],
    }];
    let (rows, rec) = grid(s, req).await;
    assert_eq!(rows.len(), 1);
    assert!(rec.messages.iter().any(|m| m.contains("ALLOW FILTERING")), "{:?}", rec.messages);
    // A raw WHERE that needs it is retried.
    let mut req = grid_req(&t, 0, 50);
    req.raw_where = Some("v = 'x78'".into());
    let (rows, rec) = grid(s, req).await;
    assert_eq!(rows.len(), 1);
    assert!(rec.messages.iter().any(|m| m.contains("ALLOW FILTERING")));
    // IN with values, IN without values.
    let mut req = grid_req(&t, 0, 50);
    req.filters = vec![
        ColumnFilter { column: "p".into(), op: FilterOp::Eq, values: vec!["1".into()] },
        ColumnFilter { column: "c".into(), op: FilterOp::In, values: vec!["3".into(), "5".into()] },
    ];
    assert_eq!(grid(s, req).await.0.len(), 2);
    let mut req = grid_req(&t, 0, 50);
    req.filters = vec![ColumnFilter { column: "c".into(), op: FilterOp::In, values: vec![] }];
    let (rows, rec) = grid(s, req).await;
    assert!(rows.is_empty() && rec.columns == 3);

    // Not available in CQL.
    let mut req = grid_req(&t, 0, 10);
    req.sort = vec![SortKey { column: "c".into(), descending: true }];
    let mut sink = CollectingSink::new();
    assert_eq!(
        s.fetch_table(req, &mut sink).await.unwrap_err().to_string(),
        "unsupported: table_sort"
    );
    let mut req = grid_req(&t, 0, 10);
    req.filters = vec![ColumnFilter { column: "v".into(), op: FilterOp::Ne, values: vec!["x".into()] }];
    assert_eq!(s.fetch_table(req, &mut sink).await.unwrap_err().kind(), "unsupported");
    let mut req = grid_req(&t, 0, 10);
    req.filters = vec![ColumnFilter { column: "nope".into(), op: FilterOp::Eq, values: vec!["x".into()] }];
    assert_eq!(s.fetch_table(req, &mut sink).await.unwrap_err().kind(), "invalid");
    assert_eq!(
        s.count_table(TableCountRequest { table: t.clone(), filters: vec![], raw_where: None, query_id: None })
            .await
            .unwrap_err()
            .to_string(),
        "unsupported: exact_count"
    );
    // A huge offset without a cached state is refused.
    let mut req = grid_req(&t, 500_000, 10);
    req.raw_where = Some("p = 1".into());
    assert_eq!(s.fetch_table(req, &mut sink).await.unwrap_err().to_string(), "unsupported: offset");

    // Views are readable.
    run(
        s,
        Some(&ks.name),
        "CREATE MATERIALIZED VIEW g_by_v AS SELECT * FROM g WHERE v IS NOT NULL AND p IS NOT NULL AND c IS NOT NULL PRIMARY KEY (v, p, c)",
    )
    .await;
    // Views fill asynchronously: poll.
    for _ in 0..50 {
        let (rows, _) = grid(s, grid_req(&ks.view("g_by_v"), 0, 300)).await;
        if rows.len() == 250 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    panic!("materialized view was not populated");
}

// ---------------------------------------------------------------------------------------------
// editing
// ---------------------------------------------------------------------------------------------

async fn t_edit(c: &Ctx) {
    let s = c.session.as_ref();
    let ks = c.keyspace("edit").await;
    let k = Some(ks.name.as_str());
    run(s, k, "CREATE TABLE people (id int, ck text, name text, age int, PRIMARY KEY (id, ck))").await;
    let t = ks.table("people");
    let keys = vec!["id".to_string(), "ck".to_string()];
    let insert = |id: i64, name: &str| RowChange::Insert {
        values: vec![
            cell("id", int(id)),
            cell("ck", EditValue::Text("a".into())),
            cell("name", EditValue::Text(name.into())),
        ],
    };
    let changes = |ch: Vec<RowChange>| ChangeSet::Rows(RowChangeSet {
        table: t.clone(),
        key_columns: keys.clone(),
        changes: ch,
    });
    let ed = s.editability(&t).await.unwrap();
    assert!(ed.editable && ed.key_columns == keys && ed.read_only_columns == keys && !ed.atomic);

    // Preview shows the CQL.
    let preview = s
        .preview_changes(&changes(vec![
            insert(1, "Ann"),
            RowChange::Update {
                key: vec![Value::Int(1), text("a")],
                set: vec![cell("age", int(30))],
            },
            RowChange::Delete {
                key: vec![Value::Int(1), text("a")],
            },
        ]))
        .await
        .unwrap();
    assert_eq!(preview.len(), 3);
    assert!(preview[0].contains("IF NOT EXISTS") && preview[1].contains("IF EXISTS") && preview[2].starts_with("DELETE FROM"));
    assert!(preview[0].contains(&format!("{}.people", ks.name)));

    // Apply: insert, update, delete.
    let r = s.apply_changes(changes(vec![insert(1, "Ann"), insert(2, "Bob")])).await.unwrap();
    assert_eq!((r.applied, r.affected.clone(), r.atomic), (2, vec![1, 1], false));
    let r = s
        .apply_changes(changes(vec![RowChange::Update {
            key: vec![Value::Int(1), text("a")],
            set: vec![cell("name", EditValue::Text("Anna".into())), cell("age", int(31))],
        }]))
        .await
        .unwrap();
    assert_eq!(r.applied, 1);
    let rows = select(s, &ks.name, "SELECT name, age FROM people WHERE id = 1 AND ck = 'a'").await;
    assert_eq!(rows[0], vec![text("Anna"), Value::Int(31)]);
    s.apply_changes(changes(vec![RowChange::Delete {
        key: vec![Value::Int(2), text("a")],
    }]))
    .await
    .unwrap();
    assert!(select(s, &ks.name, "SELECT * FROM people WHERE id = 2").await.is_empty());

    // Conflicts: insert of an existing row, update/delete of a missing one.
    match s.apply_changes(changes(vec![insert(5, "Eve"), insert(1, "Dup")])).await.unwrap_err() {
        Error::ChangeFailed { index, applied, message } => {
            assert_eq!((index, applied), (1, 1));
            assert!(message.contains("already exists"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(select(s, &ks.name, "SELECT * FROM people WHERE id = 5").await.len(), 1, "no rollback");
    match s
        .apply_changes(changes(vec![RowChange::Update {
            key: vec![Value::Int(99), text("a")],
            set: vec![cell("age", int(1))],
        }]))
        .await
        .unwrap_err()
    {
        Error::ChangeFailed { index: 0, applied: 0, message } => assert!(message.contains("no longer exists")),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        s.apply_changes(changes(vec![RowChange::Delete { key: vec![Value::Int(99), text("a")] }])).await,
        Err(Error::ChangeFailed { .. })
    ));
    // Invalid changes are rejected before they reach the server.
    for bad in [
        RowChange::Update { key: vec![Value::Int(1), text("a")], set: vec![cell("id", int(7))] },
        RowChange::Insert { values: vec![cell("name", EditValue::Text("x".into()))] },
        RowChange::Update { key: vec![Value::Int(1)], set: vec![cell("age", int(1))] },
        RowChange::Update { key: vec![Value::Int(1), text("a")], set: vec![cell("age", EditValue::Text("old".into()))] },
    ] {
        assert_eq!(s.apply_changes(changes(vec![bad])).await.unwrap_err().kind(), "invalid");
    }
    assert_eq!(
        s.preview_changes(&ChangeSet::Keys(KeyChangeSet { database: ks.path(), changes: vec![] }))
            .await
            .unwrap_err()
            .kind(),
        "unsupported"
    );

    // Without lightweight transactions: plain upserts.
    let mut cfg = config(c.server, "dbstudio");
    cfg.options.insert("cql.lwtEdits".into(), "false".into());
    let upsert = connect_cfg(c.server, &cfg, "dbstudio").await.unwrap();
    let ed = upsert.editability(&t).await.unwrap();
    assert_eq!(ed.notes, vec![EditNote::UpsertSemantics]);
    let p = upsert.preview_changes(&changes(vec![insert(1, "Over")])).await.unwrap();
    assert!(!p[0].contains("IF NOT EXISTS"));
    upsert.apply_changes(changes(vec![insert(1, "Over")])).await.unwrap();
    upsert.apply_changes(changes(vec![insert(40, "New")])).await.unwrap();
    assert_eq!(
        select(s, &ks.name, "SELECT name FROM people WHERE id = 1 AND ck = 'a'").await[0][0],
        text("Over")
    );
    upsert.close().await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// export / import
// ---------------------------------------------------------------------------------------------

async fn stream_rows(l: &dyn dbstudio_core::Lease, req: TableStreamRequest) -> dbstudio_core::Result<Vec<Vec<Value>>> {
    struct Keep(Vec<Vec<Value>>);
    impl ResultSink for Keep {
        fn on_columns(&mut self, _: Vec<dbstudio_core::ColumnMeta>) -> dbstudio_core::Result<()> {
            Ok(())
        }
        fn on_rows(&mut self, rows: Vec<Vec<Value>>) -> dbstudio_core::Result<()> {
            self.0.extend(rows);
            Ok(())
        }
        fn on_message(&mut self, _: String) -> dbstudio_core::Result<()> {
            Ok(())
        }
        fn on_statement_end(&mut self, _: dbstudio_core::StatementEnd) -> dbstudio_core::Result<()> {
            Ok(())
        }
    }
    let mut sink = Keep(vec![]);
    l.stream_table(req, &mut sink).await?;
    Ok(sink.0)
}

fn stream_req(table: &ObjectPath) -> TableStreamRequest {
    TableStreamRequest {
        query_id: Uuid::new_v4(),
        table: table.clone(),
        columns: vec![],
        filters: vec![],
        raw_where: None,
        order_by_key: true,
        batch_size: Some(200),
    }
}

async fn t_export_import(c: &Ctx) {
    let s = c.session.as_ref();
    let ks = c.keyspace("export").await;
    fill_nums(c, &ks, 1500).await;
    let k = Some(ks.name.as_str());
    run(s, k, "CREATE TABLE nums2 (p int, c int, v text, PRIMARY KEY (p, c))").await;
    let lease = c.lease(k).await;
    let all = stream_rows(lease.as_ref(), stream_req(&ks.table("nums"))).await.unwrap();
    assert_eq!(all.len(), 1500);
    let mut cols = stream_req(&ks.table("nums"));
    cols.columns = vec!["c".into(), "v".into()];
    cols.filters = vec![ColumnFilter { column: "p".into(), op: FilterOp::Eq, values: vec!["3".into()] }];
    let part = stream_rows(lease.as_ref(), cols).await.unwrap();
    assert_eq!(part.len(), 300);
    assert_eq!(part[0].len(), 2);
    let mut filtered = stream_req(&ks.table("nums"));
    filtered.raw_where = Some("v = 'x42'".into());
    assert_eq!(stream_rows(lease.as_ref(), filtered).await.unwrap().len(), 1);
    let mut empty = stream_req(&ks.table("nums"));
    empty.filters = vec![ColumnFilter { column: "p".into(), op: FilterOp::In, values: vec![] }];
    assert!(stream_rows(lease.as_ref(), empty).await.unwrap().is_empty());
    let mut bad = stream_req(&ks.table("nums"));
    bad.columns = vec!["zz".into()];
    assert_eq!(stream_rows(lease.as_ref(), bad).await.unwrap_err().kind(), "invalid");

    // Import into another table.
    let import = |rows: Vec<Vec<Value>>, on_conflict| BulkInsertRequest {
        table: ks.table("nums2"),
        columns: vec!["p".into(), "c".into(), "v".into()],
        rows: rows.into_iter().map(|r| r.into_iter().map(EditValue::Value).collect()).collect(),
        on_conflict,
        conflict_columns: vec![],
    };
    let r = lease.bulk_insert(import(all.clone(), OnConflict::Error)).await.unwrap();
    assert_eq!((r.inserted, r.skipped), (1500, 0));
    let copy = stream_rows(lease.as_ref(), stream_req(&ks.table("nums2"))).await.unwrap();
    let key = |r: &Vec<Value>| format!("{:?}", (&r[0], &r[1]));
    let (mut a, mut b) = (all.clone(), copy);
    a.sort_by_key(key);
    b.sort_by_key(key);
    assert_eq!(a, b);
    // Conflict policies on existing keys.
    let changed: Vec<Vec<Value>> = all
        .iter()
        .take(10)
        .map(|r| vec![r[0].clone(), r[1].clone(), text("changed")])
        .collect();
    let r = lease.bulk_insert(import(changed.clone(), OnConflict::Skip)).await.unwrap();
    assert_eq!((r.inserted, r.skipped), (0, 10));
    assert!(select(s, &ks.name, "SELECT v FROM nums2 WHERE p = 0 AND c = 0").await[0][0] != text("changed"));
    let err = lease.bulk_insert(import(changed.clone(), OnConflict::Error)).await.unwrap_err();
    assert_eq!(err.kind(), "query", "{err}");
    let r = lease.bulk_insert(import(changed, OnConflict::Update)).await.unwrap();
    assert_eq!(r.inserted, 10);
    assert_eq!(select(s, &ks.name, "SELECT v FROM nums2 WHERE p = 0 AND c = 0").await[0][0], text("changed"));
    // Errors.
    let missing_key = BulkInsertRequest {
        table: ks.table("nums2"),
        columns: vec!["p".into(), "v".into()],
        rows: vec![vec![int(1), EditValue::Text("x".into())]],
        on_conflict: OnConflict::Update,
        conflict_columns: vec![],
    };
    assert_eq!(lease.bulk_insert(missing_key).await.unwrap_err().kind(), "invalid");
    let bad_type = BulkInsertRequest {
        table: ks.table("nums2"),
        columns: vec!["p".into(), "c".into(), "v".into()],
        rows: vec![vec![EditValue::Text("abc".into()), int(1), EditValue::Text("x".into())]],
        on_conflict: OnConflict::Update,
        conflict_columns: vec![],
    };
    assert_eq!(lease.bulk_insert(bad_type).await.unwrap_err().kind(), "invalid");
    run(s, k, "CREATE TABLE cnt (id int PRIMARY KEY, n counter)").await;
    let counter = BulkInsertRequest {
        table: ks.table("cnt"),
        columns: vec!["id".into()],
        rows: vec![vec![int(1)]],
        on_conflict: OnConflict::Update,
        conflict_columns: vec![],
    };
    assert_eq!(lease.bulk_insert(counter).await.unwrap_err().kind(), "unsupported");
    // Counter tables read fine.
    run(s, k, "UPDATE cnt SET n = n + 5 WHERE id = 1").await;
    let (rows, _) = grid(s, grid_req(&ks.table("cnt"), 0, 10)).await;
    assert_eq!(rows[0][1], Value::Int(5));
    lease.close().await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// explain analyze
// ---------------------------------------------------------------------------------------------

fn capture(name: &str, plan: &dbstudio_core::RawPlan) {
    if let Ok(dir) = std::env::var("DBSTUDIO_CAPTURE_DIR") {
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(format!("{dir}/{name}.json"), &plan.text).unwrap();
    }
}

async fn t_explain(c: &Ctx) {
    let s = c.session.as_ref();
    let ks = c.keyspace("explain").await;
    fill_nums(c, &ks, 400).await;
    let k = Some(ks.name.clone());
    let req = |text: &str, analyze: bool| ExplainRequest {
        query_id: Uuid::new_v4(),
        text: text.into(),
        database: k.clone(),
        analyze,
        options: Default::default(),
        timeout_ms: None,
    };
    let by_pk = s.explain(req("SELECT * FROM nums WHERE p = 1 AND c = 1", true)).await.unwrap();
    assert_eq!(by_pk.format, PlanFormat::CqlTrace);
    assert!(by_pk.analyzed && !by_pk.rolled_back);
    assert!(!by_pk.server_version.is_empty() && by_pk.captured_at.ends_with('Z'));
    let tree = dbstudio_plan::parse_and_analyze(&by_pk, &Default::default()).unwrap();
    assert_eq!(tree.root.op, "CQL request");
    assert!(tree.node_count > 3, "{} events", tree.node_count);
    assert!(tree.totals.execution_time_ms.unwrap() > 0.0);
    assert_eq!(tree.root.actual_rows, Some(1.0));
    capture(&format!("{}_01_pk_lookup", c.server.name), &by_pk);

    let scan = s
        .explain(req("SELECT * FROM nums WHERE v = 'x5' ALLOW FILTERING", true))
        .await
        .unwrap();
    let tree = dbstudio_plan::parse(&scan).unwrap();
    assert_eq!(tree.root.object.as_ref().unwrap().name, "nums");
    capture(&format!("{}_02_filtering_scan", c.server.name), &scan);

    // Tombstones: delete a block of rows, then read the partition.
    run(s, k.as_deref(), "DELETE FROM nums WHERE p = 2 AND c = 7").await;
    let lease = c.lease(k.as_deref()).await;
    let r = lease.bulk_insert(BulkInsertRequest {
        table: ks.table("nums"),
        columns: vec!["p".into(), "c".into(), "v".into()],
        rows: (0..20).map(|i| vec![int(7), int(i), EditValue::Value(Value::Null)]).collect(),
        on_conflict: OnConflict::Update,
        conflict_columns: vec![],
    })
    .await
    .unwrap();
    assert_eq!(r.inserted, 20);
    let tomb = lease.explain(req("SELECT * FROM nums WHERE p = 7", true)).await.unwrap();
    capture(&format!("{}_03_tombstones_lease", c.server.name), &tomb);
    assert_eq!(tomb.format, PlanFormat::CqlTrace);
    let full = s.explain(req("SELECT * FROM nums", true)).await.unwrap();
    capture(&format!("{}_04_full_table", c.server.name), &full);
    let cnt = s.explain(req("SELECT count(*) FROM nums WHERE p = 1", true)).await.unwrap();
    capture(&format!("{}_05_count_partition", c.server.name), &cnt);
    for p in [&tomb, &full, &cnt] {
        dbstudio_plan::parse(p).unwrap();
    }

    // Only SELECT is traced.
    assert_eq!(
        s.explain(req("INSERT INTO nums (p, c, v) VALUES (1, 1000, 'x')", true)).await.unwrap_err().to_string(),
        "unsupported: explain_analyze_writes"
    );
    assert_eq!(
        lease.explain(req("DELETE FROM nums WHERE p = 1", true)).await.unwrap_err().kind(),
        "unsupported"
    );
    assert_eq!(
        s.explain(req("SELECT * FROM nums", false)).await.unwrap_err().to_string(),
        "unsupported: explain"
    );
    assert!(select(s, &ks.name, "SELECT * FROM nums WHERE p = 1 AND c = 1000").await.is_empty());
    lease.close().await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// the suite for each server
// ---------------------------------------------------------------------------------------------

macro_rules! suite {
    ($module:ident, $server:expr) => {
        mod $module {
            use super::*;

            macro_rules! it {
                ($name:ident, $body:path) => {
                    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                    async fn $name() {
                        if let Some(c) = ctx($server).await {
                            $body(&c).await;
                            c.session.close().await.unwrap();
                        }
                    }
                };
            }

            it!(connect, t_connect);
            it!(read_only, t_read_only);
            it!(tls, t_tls);
            it!(tree, t_tree);
            it!(ddl_roundtrip, t_ddl_roundtrip);
            it!(types, t_types);
            it!(lease, t_lease);
            it!(cancel_and_timeout, t_cancel_timeout);
            it!(grid, t_grid);
            it!(edit, t_edit);
            it!(export_import, t_export_import);
            it!(explain, t_explain);
        }
    };
}

suite!(scylla, SCYLLA);
suite!(cassandra5, CASSANDRA5);
