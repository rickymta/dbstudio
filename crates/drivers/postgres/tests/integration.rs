//! Integration tests against the docker servers (`pg16` on 54316 with TLS, `pg12` on 54312).
//! They only run when `DBSTUDIO_IT=1`; otherwise every test returns immediately.

mod common;

use std::time::{Duration, Instant};

use common::*;
use dbstudio_core::{
    ColumnFilter, EndOutcome, Error, FilterOp, ObjectKind, QueryRequest, Result, ResultSink,
    Session, SortKey, StatementEnd, StatementOutcome, TableCountRequest, TableDataRequest, TlsMode,
    Value, ValueKind, execute_collect,
};
use uuid::Uuid;

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

fn int(v: i64) -> Value {
    Value::Int(v)
}
fn text(s: &str) -> Value {
    Value::Text(s.to_string())
}

// ------------------------------------------------------------------------------------------
// Connection, TLS, errors
// ------------------------------------------------------------------------------------------

async fn ssl_in_use(s: &dyn Session) -> bool {
    scalar(
        s,
        "SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()",
    )
    .await
        == Value::Bool(true)
}

#[tokio::test]
async fn tls_modes_on_pg16() {
    if !enabled() {
        return;
    }
    // verify-full with the right CA, by IP and by name.
    let s = try_connect(&config(PG16, TlsMode::VerifyFull), PASSWORD)
        .await
        .unwrap();
    assert!(ssl_in_use(&*s).await);
    let mut cfg = config(PG16, TlsMode::VerifyFull);
    cfg.host = "localhost".into();
    let s = try_connect(&cfg, PASSWORD).await.unwrap();
    assert!(ssl_in_use(&*s).await);

    // verify-ca, require, prefer all encrypt.
    for mode in [TlsMode::VerifyCa, TlsMode::Require, TlsMode::Prefer] {
        let s = try_connect(&config(PG16, mode), PASSWORD).await.unwrap();
        assert!(ssl_in_use(&*s).await, "{mode:?}");
    }
    // disable stays plain.
    let s = try_connect(&config(PG16, TlsMode::Disable), PASSWORD)
        .await
        .unwrap();
    assert!(!ssl_in_use(&*s).await);

    // An unrelated CA is rejected by both verifying modes.
    for mode in [TlsMode::VerifyFull, TlsMode::VerifyCa] {
        let mut cfg = config(PG16, mode);
        cfg.tls.ca_cert_path = Some(other_ca_path());
        let err = try_connect(&cfg, PASSWORD).await.err().expect("must fail");
        assert!(matches!(err, Error::Tls(_)), "{mode:?}: {err:?}");
        assert!(!err.to_string().contains(PASSWORD));
    }
    // Without a CA file the system roots are used, which do not know the test CA.
    let mut cfg = config(PG16, TlsMode::VerifyFull);
    cfg.tls.ca_cert_path = None;
    let err = try_connect(&cfg, PASSWORD).await.err().expect("must fail");
    assert!(matches!(err, Error::Tls(_)), "{err:?}");
    // Missing CA file.
    let mut cfg = config(PG16, TlsMode::VerifyFull);
    cfg.tls.ca_cert_path = Some("/nonexistent/ca.crt".into());
    assert!(matches!(
        try_connect(&cfg, PASSWORD).await.err().expect("must fail"),
        Error::Tls(_)
    ));
}

#[tokio::test]
async fn tls_modes_on_pg12() {
    if !enabled() {
        return;
    }
    // Prefer falls back to plain text, Disable works, Require and verify modes fail with Tls.
    let s = try_connect(&config(PG12, TlsMode::Prefer), PASSWORD)
        .await
        .unwrap();
    assert!(!ssl_in_use(&*s).await);
    let s = try_connect(&config(PG12, TlsMode::Disable), PASSWORD)
        .await
        .unwrap();
    assert!(!ssl_in_use(&*s).await);
    for mode in [TlsMode::Require, TlsMode::VerifyCa, TlsMode::VerifyFull] {
        let err = try_connect(&config(PG12, mode), PASSWORD)
            .await
            .err()
            .expect("must fail");
        assert!(matches!(err, Error::Tls(_)), "{mode:?}: {err:?}");
    }
}

async fn bad_password(port: u16) {
    let err = try_connect(&config(port, TlsMode::Prefer), "wrong_pw_value")
        .await
        .err()
        .expect("must fail");
    assert!(matches!(err, Error::Authentication(_)), "{err:?}");
    assert!(!err.to_string().contains("wrong_pw_value"));
    // Missing password where one is required is also an authentication error.
    let err = PostgresDriverAlias::new()
        .connect(&config(port, TlsMode::Prefer), None)
        .await
        .err()
        .expect("must fail");
    assert!(matches!(err, Error::Authentication(_)), "{err:?}");
}
use dbstudio_core::Driver;
use dbstudio_driver_postgres::PostgresDriver as PostgresDriverAlias;
on_both!(bad_password, bad_password_pg16, bad_password_pg12);

#[tokio::test]
async fn connection_errors() {
    if !enabled() {
        return;
    }
    let mut cfg = config(PG16, TlsMode::Prefer);
    cfg.port = 54999;
    let err = try_connect(&cfg, PASSWORD).await.err().expect("must fail");
    assert!(matches!(err, Error::Connection(_)), "{err:?}");

    // Unknown database.
    let mut cfg = config(PG16, TlsMode::Prefer);
    cfg.database = Some("no_such_db_xyz".into());
    let err = try_connect(&cfg, PASSWORD).await.err().expect("must fail");
    assert!(matches!(err, Error::Connection(_)), "{err:?}");

    // Black-holed address: either the connect times out or the network refuses it outright.
    let mut cfg = config(PG16, TlsMode::Prefer);
    cfg.host = "10.255.255.1".into();
    cfg.connect_timeout_ms = Some(400);
    let started = Instant::now();
    let err = try_connect(&cfg, PASSWORD).await.err().expect("must fail");
    assert!(
        matches!(err, Error::Timeout | Error::Connection(_)),
        "{err:?} after {:?}",
        started.elapsed()
    );
    assert!(started.elapsed() < Duration::from_secs(3));
}

async fn info_and_ping(port: u16) {
    let s = connect(port).await;
    s.ping().await.unwrap();
    let info = s.server_info().await.unwrap();
    assert_eq!(info.product, "PostgreSQL");
    let major = if port == PG16 { "16" } else { "12" };
    assert!(info.version.starts_with(major), "{}", info.version);
    assert!(info.extra["fullVersion"].starts_with("PostgreSQL"));
    assert_eq!(info.extra["encoding"], "UTF8");
    assert_eq!(info.extra["database"], "dbstudio");
    assert_eq!(info.extra["timezone"], "Etc/UTC");
    s.close().await.unwrap();
    assert!(s.ping().await.is_err());
}
on_both!(info_and_ping, info_and_ping_pg16, info_and_ping_pg12);

async fn connection_options(port: u16) {
    let mut cfg = config(port, TlsMode::Prefer);
    cfg.options
        .insert("application_name".into(), "my app".into());
    cfg.options
        .insert("statement_timeout".into(), "12345".into());
    cfg.read_only = true;
    let s = try_connect(&cfg, PASSWORD).await.unwrap();
    assert_eq!(scalar(&*s, "SHOW application_name").await, text("my app"));
    assert_eq!(scalar(&*s, "SHOW statement_timeout").await, text("12345ms"));
    assert_eq!(
        scalar(&*s, "SHOW default_transaction_read_only").await,
        text("on")
    );
    let err = exec_err(&*s, "CREATE TABLE public.ro_test (a int)").await;
    assert!(matches!(err, Error::Query { .. }));
}
on_both!(
    connection_options,
    connection_options_pg16,
    connection_options_pg12
);

// ------------------------------------------------------------------------------------------
// Object tree
// ------------------------------------------------------------------------------------------

async fn tree(port: u16) {
    let s = fixture(port).await;
    let root = s
        .list_children(&dbstudio_core::ObjectPath::root())
        .await
        .unwrap();
    let dbs: Vec<_> = root
        .iter()
        .filter(|n| n.kind == ObjectKind::Database)
        .collect();
    let db_names = names(&root);
    assert!(db_names.contains(&"dbstudio".to_string()));
    assert!(db_names.contains(&"dbs_it_other".to_string()));
    assert!(!db_names.contains(&"template0".to_string()));
    assert!(!db_names.contains(&"template1".to_string()));
    assert!(dbs.iter().all(|n| n.has_children));
    let roles_folder = root.iter().find(|n| n.kind == ObjectKind::Folder).unwrap();
    assert_eq!(roles_folder.name, "roles");
    let roles = s.list_children(&roles_folder.path).await.unwrap();
    let dbstudio_role = roles.iter().find(|r| r.name == "dbstudio").unwrap();
    assert_eq!(dbstudio_role.kind, ObjectKind::Role);
    assert!(
        dbstudio_role
            .detail
            .as_deref()
            .unwrap()
            .contains("superuser")
    );
    assert!(!dbstudio_role.has_children);

    let db = dbs.iter().find(|n| n.name == "dbstudio").unwrap();
    let db_children = s.list_children(&db.path).await.unwrap();
    assert_eq!(names(&db_children), ["schemas"]);
    let schemas = s.list_children(&db_children[0].path).await.unwrap();
    let schema_names = names(&schemas);
    let pos = |n: &str| schema_names.iter().position(|x| x == n).unwrap();
    assert!(pos("public") < pos("pg_catalog"));
    assert!(pos(SCHEMA) < pos("information_schema"));
    assert!(pos("Dbs It \"S\"") < pos("pg_catalog"));
    assert!(!schema_names.iter().any(|n| n.starts_with("pg_temp")));
    let pg_catalog = &schemas[pos("pg_catalog")];
    assert_eq!(pg_catalog.detail.as_deref(), Some("system"));
    assert_eq!(schemas[pos(SCHEMA)].detail, None);
    assert!(schemas.iter().all(|n| n.kind == ObjectKind::Schema));

    // Schema folders.
    let folders = s.list_children(&schemas[pos(SCHEMA)].path).await.unwrap();
    assert_eq!(
        names(&folders),
        [
            "tables",
            "views",
            "materializedViews",
            "functions",
            "procedures",
            "sequences"
        ]
    );
    assert!(
        folders
            .iter()
            .all(|f| f.kind == ObjectKind::Folder && f.has_children)
    );
    // An empty schema only offers the tables folder, flagged as empty.
    exec(&*s, "CREATE SCHEMA IF NOT EXISTS dbs_it_empty").await;
    let empty = s.list_children(&schema_path("dbs_it_empty")).await.unwrap();
    assert_eq!(names(&empty), ["tables"]);
    assert!(!empty[0].has_children);
    // System schema also lists folders.
    let sys = s.list_children(&pg_catalog.path).await.unwrap();
    assert!(names(&sys).contains(&"tables".to_string()));

    let kids = |i: usize| folders[i].path.clone();
    let tables = s.list_children(&kids(0)).await.unwrap();
    let tnames = names(&tables);
    for expect in [
        "all_types",
        "big",
        "children",
        "parents",
        "part",
        "part_2024",
        "people",
        "Weird \"Table\" Name",
    ] {
        assert!(
            tnames.contains(&expect.to_string()),
            "{expect} in {tnames:?}"
        );
    }
    assert!(
        tables
            .iter()
            .all(|t| t.kind == ObjectKind::Table && t.has_children)
    );
    let detail = |n: &str| tables.iter().find(|t| t.name == n).unwrap().detail.clone();
    assert_eq!(detail("part").as_deref(), Some("partitioned"));
    assert_eq!(detail("part_2024").as_deref(), Some("partition"));
    assert_eq!(detail("people"), None);
    assert!(!tnames.contains(&"v_children".to_string()));

    let views = s.list_children(&kids(1)).await.unwrap();
    assert_eq!(names(&views), ["v_children"]);
    assert_eq!(views[0].kind, ObjectKind::View);
    let mviews = s.list_children(&kids(2)).await.unwrap();
    assert_eq!(names(&mviews), ["mv_parents"]);
    assert_eq!(mviews[0].kind, ObjectKind::MaterializedView);
    let funcs = s.list_children(&kids(3)).await.unwrap();
    let fnames = names(&funcs);
    assert!(
        fnames.contains(&"add(a integer, b integer)".to_string()),
        "{fnames:?}"
    );
    assert!(fnames.contains(&"add(a integer, b integer, c integer)".to_string()));
    assert!(fnames.contains(&"trg_fn()".to_string()));
    assert!(
        funcs
            .iter()
            .all(|f| f.kind == ObjectKind::Function && !f.has_children)
    );
    let procs = s.list_children(&kids(4)).await.unwrap();
    assert_eq!(names(&procs), ["do_nothing()"]);
    assert_eq!(procs[0].kind, ObjectKind::Procedure);
    let seqs = s.list_children(&kids(5)).await.unwrap();
    let snames = names(&seqs);
    assert!(snames.contains(&"seq1".to_string()));
    assert!(snames.contains(&"parents_id_seq".to_string()));
    assert!(
        !snames.contains(&"children_id_seq".to_string()),
        "identity sequence is internal"
    );
    assert_eq!(
        seqs.iter()
            .find(|n| n.name == "seq1")
            .unwrap()
            .detail
            .as_deref(),
        Some("bigint")
    );

    // Table: columns / indexes / triggers.
    let children = tables.iter().find(|t| t.name == "children").unwrap();
    let tf = s.list_children(&children.path).await.unwrap();
    assert_eq!(names(&tf), ["columns", "indexes", "triggers"]);
    assert!(tf.iter().all(|f| f.has_children));
    let cols = s.list_children(&tf[0].path).await.unwrap();
    assert_eq!(names(&cols), ["id", "parent_id", "note", "price", "total"]);
    assert!(
        cols.iter()
            .all(|c| c.kind == ObjectKind::Column && !c.has_children)
    );
    assert_eq!(cols[3].detail.as_deref(), Some("numeric(10,2)"));
    assert_eq!(cols[2].detail.as_deref(), Some("text"));
    let idx = s.list_children(&tf[1].path).await.unwrap();
    let inames = names(&idx);
    assert!(inames.contains(&"children_pkey".to_string()));
    assert!(inames.contains(&"children_note_idx".to_string()));
    assert!(inames.contains(&"children_parent_id_note_key".to_string()));
    assert!(idx.iter().all(|i| i.kind == ObjectKind::Index));
    assert_eq!(idx[0].name, "children_pkey");
    assert!(idx[0].detail.as_deref().unwrap().starts_with("PRIMARY KEY"));
    let trg = s.list_children(&tf[2].path).await.unwrap();
    assert_eq!(names(&trg), ["children_before_ins"]);
    assert_eq!(trg[0].kind, ObjectKind::Trigger);
    assert_eq!(
        trg[0].detail.as_deref(),
        Some("BEFORE INSERT OR UPDATE, row")
    );

    // varchar(20) and friends.
    let all_types = tables.iter().find(|t| t.name == "all_types").unwrap();
    let atf = s.list_children(&all_types.path).await.unwrap();
    let atc = s.list_children(&atf[0].path).await.unwrap();
    let d = |n: &str| {
        atc.iter()
            .find(|c| c.name == n)
            .unwrap()
            .detail
            .clone()
            .unwrap()
    };
    assert_eq!(d("c_num"), "numeric(38,10)");
    assert_eq!(d("c_vc"), "character varying(20)");
    assert_eq!(d("c_ch"), "character(5)");
    assert_eq!(d("c_tstz"), "timestamp with time zone");
    assert_eq!(d("a_i4"), "integer[]");

    // Views: columns only. Partitioned table keeps its folders.
    let vf = s.list_children(&views[0].path).await.unwrap();
    assert_eq!(names(&vf), ["columns"]);
    let vc = s.list_children(&vf[0].path).await.unwrap();
    assert_eq!(names(&vc), ["id", "note"]);
    let mf = s.list_children(&mviews[0].path).await.unwrap();
    assert_eq!(names(&mf), ["columns"]);
    let part = tables.iter().find(|t| t.name == "part").unwrap();
    let pf = s.list_children(&part.path).await.unwrap();
    assert_eq!(names(&pf), ["columns", "indexes", "triggers"]);
    assert!(!pf[1].has_children);

    // Odd names, in tables and schemas.
    let weird = tables
        .iter()
        .find(|t| t.name == "Weird \"Table\" Name")
        .unwrap();
    let wf = s.list_children(&weird.path).await.unwrap();
    let wc = s.list_children(&wf[0].path).await.unwrap();
    assert_eq!(names(&wc), ["id", "Col A", "we\"ird"]);
    let ws = s.list_children(&schema_path("Dbs It \"S\"")).await.unwrap();
    assert_eq!(names(&ws), ["tables"]);
    let wt = s.list_children(&ws[0].path).await.unwrap();
    assert_eq!(names(&wt), ["T x"]);

    // Unknown things are NotFound; leaves have no children.
    let missing = schema_path("no_such_schema");
    assert!(matches!(
        s.list_children(&missing).await,
        Err(Error::NotFound(_))
    ));
    assert!(s.list_children(&cols[0].path).await.unwrap().is_empty());
}
on_both!(tree, tree_pg16, tree_pg12);

async fn second_database(port: u16) {
    let s = fixture(port).await;
    let db = dbstudio_core::ObjectPath::root().child(ObjectKind::Database, "dbs_it_other");
    let folders = s.list_children(&db).await.unwrap();
    let schemas = s.list_children(&folders[0].path).await.unwrap();
    let public = schemas.iter().find(|n| n.name == "public").unwrap();
    let f = s.list_children(&public.path).await.unwrap();
    let t = s.list_children(&f[0].path).await.unwrap();
    assert_eq!(names(&t), ["other_t"]);
    assert!(!names(&s.list_children(&f[0].path).await.unwrap()).contains(&"all_types".to_string()));

    // Data and metadata come from that database.
    let mut rec = Rec::default();
    let mut req = TableDataRequest {
        query_id: Uuid::new_v4(),
        table: t[0].path.clone(),
        filters: vec![],
        raw_where: None,
        sort: vec![],
        offset: 0,
        limit: 10,
        batch_size: None,
    };
    s.fetch_table(req.clone(), &mut rec).await.unwrap();
    assert_eq!(rec.rows, vec![vec![int(1), text("x")]]);
    let details = s.table_details(&t[0].path).await.unwrap();
    assert_eq!(details.columns.len(), 2);
    assert!(details.columns[0].primary_key);
    let ddl = s.object_ddl(&t[0].path).await.unwrap();
    assert!(ddl.contains("CREATE TABLE \"public\".\"other_t\""), "{ddl}");
    req.table = t[0].path.clone();
    assert_eq!(
        s.count_table(TableCountRequest {
            table: t[0].path.clone(),
            filters: vec![],
            raw_where: None
        })
        .await
        .unwrap(),
        1
    );
    let mut q = QueryRequest::new("SELECT current_database()");
    q.database = Some("dbs_it_other".into());
    let r = execute_collect(&*s, q).await.unwrap();
    match &r[0].outcome {
        StatementOutcome::Rows(rs) => assert_eq!(rs.rows[0][0], text("dbs_it_other")),
        o => panic!("{o:?}"),
    }
    // The login database is unaffected.
    assert_eq!(
        scalar(&*s, "SELECT current_database()").await,
        text("dbstudio")
    );
}
on_both!(second_database, second_database_pg16, second_database_pg12);

// ------------------------------------------------------------------------------------------
// DDL and table details
// ------------------------------------------------------------------------------------------

async fn ddl(port: u16) {
    let s = fixture(port).await;
    let children = s
        .object_ddl(&table_path(SCHEMA, ObjectKind::Table, "children"))
        .await
        .unwrap();
    for needle in [
        "CREATE TABLE \"dbs_it\".\"children\" (",
        "\"id\" integer GENERATED ALWAYS AS IDENTITY NOT NULL",
        "\"parent_id\" integer NOT NULL",
        "\"note\" text DEFAULT 'n/a'::text",
        "\"price\" numeric(10,2)",
        "\"total\" numeric GENERATED ALWAYS AS ((price * (2)::numeric)) STORED",
        "CONSTRAINT \"children_pkey\" PRIMARY KEY (id)",
        "CONSTRAINT \"children_parent_id_note_key\" UNIQUE (parent_id, note)",
        "CONSTRAINT \"children_price_check\" CHECK (price >= 0::numeric)",
        "FOREIGN KEY (parent_id) REFERENCES dbs_it.parents(id) ON UPDATE RESTRICT ON DELETE CASCADE",
        "CREATE INDEX children_note_idx ON dbs_it.children USING btree (lower(note));",
        "COMMENT ON COLUMN \"dbs_it\".\"children\".\"note\" IS 'free text';",
    ] {
        assert!(
            children.contains(needle),
            "missing `{needle}` in:\n{children}"
        );
    }
    assert!(
        !children.contains("children_pkey ON"),
        "pk index must not be repeated"
    );

    let parents = s
        .object_ddl(&table_path(SCHEMA, ObjectKind::Table, "parents"))
        .await
        .unwrap();
    assert!(
        parents
            .contains("\"id\" integer DEFAULT nextval('dbs_it.parents_id_seq'::regclass) NOT NULL"),
        "{parents}"
    );
    assert!(parents.contains("COMMENT ON TABLE \"dbs_it\".\"parents\" IS 'Parent rows';"));

    let part = s
        .object_ddl(&table_path(SCHEMA, ObjectKind::Table, "part"))
        .await
        .unwrap();
    assert!(part.contains("PARTITION BY RANGE (d)"), "{part}");
    let child = s
        .object_ddl(&table_path(SCHEMA, ObjectKind::Table, "part_2024"))
        .await
        .unwrap();
    assert!(
        child.contains("PARTITION OF \"dbs_it\".\"part\""),
        "{child}"
    );
    assert!(child.contains("FOR VALUES FROM ('2024-01-01') TO ('2025-01-01')"));

    let weird = s
        .object_ddl(&table_path(
            SCHEMA,
            ObjectKind::Table,
            "Weird \"Table\" Name",
        ))
        .await
        .unwrap();
    assert!(
        weird.contains("CREATE TABLE \"dbs_it\".\"Weird \"\"Table\"\" Name\" ("),
        "{weird}"
    );
    assert!(weird.contains("\"we\"\"ird\" text"));

    let view = s
        .object_ddl(&table_path(SCHEMA, ObjectKind::View, "v_children"))
        .await
        .unwrap();
    assert!(
        view.starts_with("CREATE OR REPLACE VIEW \"dbs_it\".\"v_children\" AS"),
        "{view}"
    );
    assert!(view.contains("FROM dbs_it.children"));
    let mview = s
        .object_ddl(&table_path(
            SCHEMA,
            ObjectKind::MaterializedView,
            "mv_parents",
        ))
        .await
        .unwrap();
    assert!(
        mview.starts_with("CREATE MATERIALIZED VIEW \"dbs_it\".\"mv_parents\" AS"),
        "{mview}"
    );
    assert!(mview.contains("CREATE INDEX mv_parents_name_idx"));

    let f = s
        .object_ddl(&table_path(
            SCHEMA,
            ObjectKind::Function,
            "add(a integer, b integer, c integer)",
        ))
        .await
        .unwrap();
    assert!(
        f.contains("CREATE OR REPLACE FUNCTION dbs_it.add(a integer, b integer, c integer)"),
        "{f}"
    );
    assert!(f.contains("select a + b + c"));
    let f2 = s
        .object_ddl(&table_path(
            SCHEMA,
            ObjectKind::Function,
            "add(a integer, b integer)",
        ))
        .await
        .unwrap();
    assert!(
        f2.contains("select a + b\n") || f2.contains("select a + b$"),
        "{f2}"
    );
    assert!(!f2.contains("select a + b + c"));
    let p = s
        .object_ddl(&table_path(SCHEMA, ObjectKind::Procedure, "do_nothing()"))
        .await
        .unwrap();
    assert!(
        p.contains("CREATE OR REPLACE PROCEDURE dbs_it.do_nothing()"),
        "{p}"
    );

    let seq = s
        .object_ddl(&table_path(SCHEMA, ObjectKind::Sequence, "seq1"))
        .await
        .unwrap();
    for needle in [
        "CREATE SEQUENCE \"dbs_it\".\"seq1\"",
        "AS bigint",
        "START WITH 5",
        "INCREMENT BY 2",
        "MINVALUE 1",
        "MAXVALUE 1000",
        "CACHE 3",
        "CYCLE;",
    ] {
        assert!(seq.contains(needle), "missing `{needle}` in:\n{seq}");
    }
    assert!(!seq.contains("NO CYCLE"));
    let owned = s
        .object_ddl(&table_path(SCHEMA, ObjectKind::Sequence, "parents_id_seq"))
        .await
        .unwrap();
    assert!(owned.contains("OWNED BY dbs_it.parents.id"), "{owned}");

    let tpath = table_path(SCHEMA, ObjectKind::Table, "children");
    let idx = s
        .object_ddl(
            &tpath
                .child(ObjectKind::Folder, "indexes")
                .child(ObjectKind::Index, "children_note_idx"),
        )
        .await
        .unwrap();
    assert_eq!(
        idx.trim(),
        "CREATE INDEX children_note_idx ON dbs_it.children USING btree (lower(note));"
    );
    let trg = s
        .object_ddl(
            &tpath
                .child(ObjectKind::Folder, "triggers")
                .child(ObjectKind::Trigger, "children_before_ins"),
        )
        .await
        .unwrap();
    assert!(
        trg.contains(
            "CREATE TRIGGER children_before_ins BEFORE INSERT OR UPDATE ON dbs_it.children"
        ),
        "{trg}"
    );
    assert!(trg.contains("EXECUTE FUNCTION dbs_it.trg_fn()"));

    // Unsupported and missing.
    let col = tpath
        .child(ObjectKind::Folder, "columns")
        .child(ObjectKind::Column, "id");
    assert!(matches!(
        s.object_ddl(&col).await,
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        s.object_ddl(&schema_path(SCHEMA)).await,
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        s.object_ddl(&table_path(SCHEMA, ObjectKind::Table, "nope"))
            .await,
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        s.object_ddl(&table_path(SCHEMA, ObjectKind::View, "nope"))
            .await,
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        s.object_ddl(&table_path(SCHEMA, ObjectKind::Function, "nope()"))
            .await,
        Err(Error::NotFound(_))
    ));

    // The generated DDL is valid: replay parents and children into another schema.
    exec(&*s, "DROP SCHEMA IF EXISTS dbs_it_rt CASCADE; CREATE SCHEMA dbs_it_rt; CREATE SEQUENCE dbs_it_rt.parents_id_seq").await;
    for t in ["parents", "children", "all_types"] {
        let d = s
            .object_ddl(&table_path(SCHEMA, ObjectKind::Table, t))
            .await
            .unwrap();
        let d = d
            .replace("\"dbs_it\".", "\"dbs_it_rt\".")
            .replace("dbs_it.", "dbs_it_rt.");
        let d = d
            .replace("dbs_it_rt.mood", "dbs_it.mood")
            .replace("dbs_it_rt.pair", "dbs_it.pair")
            .replace("dbs_it_rt.posint", "dbs_it.posint");
        let d = d.replace("\"dbs_it_rt\".\"mood\"", "dbs_it.mood");
        if let Err(e) = execute_collect(&*s, QueryRequest::new(d.clone())).await {
            panic!("replay of {t} failed: {e}\n{d}");
        }
    }
    exec(&*s, "DROP SCHEMA dbs_it_rt CASCADE").await;
}
on_both!(ddl, ddl_pg16, ddl_pg12);

async fn details(port: u16) {
    let s = fixture(port).await;
    let path = table_path(SCHEMA, ObjectKind::Table, "children");
    let d = s.table_details(&path).await.unwrap();
    assert_eq!(d.kind, ObjectKind::Table);
    assert_eq!(d.path, path);
    assert_eq!(d.comment, None);
    assert_eq!(d.columns.len(), 5);
    let c = &d.columns;
    assert_eq!(
        (c[0].name.as_str(), c[0].ordinal, c[0].type_name.as_str()),
        ("id", 1, "integer")
    );
    assert!(c[0].primary_key && !c[0].nullable);
    assert_eq!(c[0].value_kind, ValueKind::Int);
    assert_eq!(c[0].extra.as_deref(), Some("identity always"));
    assert!(!c[1].primary_key && !c[1].nullable);
    assert_eq!(c[2].default.as_deref(), Some("'n/a'::text"));
    assert_eq!(c[2].comment.as_deref(), Some("free text"));
    assert!(c[2].nullable);
    assert_eq!(c[3].type_name, "numeric(10,2)");
    assert_eq!(c[3].value_kind, ValueKind::Decimal);
    assert_eq!(c[4].extra.as_deref(), Some("generated stored"));
    let idx = |n: &str| d.indexes.iter().find(|i| i.name == n).unwrap();
    assert!(idx("children_pkey").primary && idx("children_pkey").unique);
    assert_eq!(idx("children_pkey").columns, ["id"]);
    assert_eq!(idx("children_pkey").method.as_deref(), Some("btree"));
    assert_eq!(
        idx("children_parent_id_note_key").columns,
        ["parent_id", "note"]
    );
    assert!(
        idx("children_parent_id_note_key").unique && !idx("children_parent_id_note_key").primary
    );
    assert_eq!(idx("children_note_idx").columns, ["lower(note)"]);
    assert!(
        idx("children_note_idx")
            .definition
            .as_deref()
            .unwrap()
            .starts_with("CREATE INDEX")
    );
    assert_eq!(d.indexes[0].name, "children_pkey");
    assert_eq!(d.foreign_keys.len(), 1);
    let fk = &d.foreign_keys[0];
    assert_eq!(fk.columns, ["parent_id"]);
    assert_eq!(fk.referenced_columns, ["id"]);
    assert_eq!(
        fk.referenced_table,
        table_path(SCHEMA, ObjectKind::Table, "parents")
    );
    assert_eq!(fk.on_delete.as_deref(), Some("CASCADE"));
    assert_eq!(fk.on_update.as_deref(), Some("RESTRICT"));
    assert!(d.size_bytes.unwrap() > 0);

    let p = s
        .table_details(&table_path(SCHEMA, ObjectKind::Table, "parents"))
        .await
        .unwrap();
    assert_eq!(p.comment.as_deref(), Some("Parent rows"));
    assert_eq!(
        p.columns[0].default.as_deref(),
        Some("nextval('dbs_it.parents_id_seq'::regclass)")
    );

    let big = s
        .table_details(&table_path(SCHEMA, ObjectKind::Table, "big"))
        .await
        .unwrap();
    let est = big.estimated_rows.unwrap();
    assert!((190_000..=210_000).contains(&est), "{est}");
    assert!(big.size_bytes.unwrap() > 1_000_000);

    let v = s
        .table_details(&table_path(SCHEMA, ObjectKind::View, "v_children"))
        .await
        .unwrap();
    assert_eq!(v.kind, ObjectKind::View);
    assert_eq!(v.columns.len(), 2);
    assert_eq!(v.size_bytes, None);
    let m = s
        .table_details(&table_path(
            SCHEMA,
            ObjectKind::MaterializedView,
            "mv_parents",
        ))
        .await
        .unwrap();
    assert_eq!(m.kind, ObjectKind::MaterializedView);
    assert_eq!(m.indexes.len(), 1);
    let at = s
        .table_details(&table_path(SCHEMA, ObjectKind::Table, "all_types"))
        .await
        .unwrap();
    let kind = |n: &str| at.columns.iter().find(|c| c.name == n).unwrap().value_kind;
    assert_eq!(kind("c_i8"), ValueKind::Int);
    assert_eq!(kind("c_tstz"), ValueKind::Timestamp);
    assert_eq!(kind("a_i4"), ValueKind::Array);
    assert_eq!(kind("c_enum"), ValueKind::Text);
    assert_eq!(kind("c_dom"), ValueKind::Int);
    assert_eq!(kind("a_enum"), ValueKind::Text);
    assert!(matches!(
        s.table_details(&table_path(SCHEMA, ObjectKind::Table, "nope"))
            .await,
        Err(Error::NotFound(_))
    ));
}
on_both!(details, details_pg16, details_pg12);

// ------------------------------------------------------------------------------------------
// Values
// ------------------------------------------------------------------------------------------

fn check_values(rec: &Rec) {
    let row = |id: i64| {
        rec.rows
            .iter()
            .position(|r| r[0] == Value::Int(id))
            .unwrap()
    };
    let r1 = row(1);
    let g = |r: usize, n: &str| rec.get(r, n).clone();
    assert_eq!(g(r1, "c_i2"), int(12));
    assert_eq!(g(r1, "c_i4"), int(123_456));
    assert_eq!(g(r1, "c_i8"), int(9_007_199_254_740_993));
    assert_eq!(
        g(r1, "c_num"),
        Value::Decimal("1234567890123456789012345678.1234567890".into())
    );
    assert_eq!(g(r1, "c_f4"), Value::Float(1.5));
    assert_eq!(g(r1, "c_f8"), Value::Float(2.25));
    assert_eq!(g(r1, "c_bool"), Value::Bool(true));
    assert_eq!(g(r1, "c_text"), text("héllo wörld"));
    assert_eq!(g(r1, "c_vc"), text("varchar"));
    assert_eq!(g(r1, "c_ch"), text("ab   "));
    assert_eq!(g(r1, "c_name"), text("nm"));
    assert_eq!(g(r1, "c_bytea"), Value::Bytes(vec![0, 255, 16]));
    assert_eq!(g(r1, "c_date"), Value::Date("2024-05-31".into()));
    assert_eq!(g(r1, "c_time"), Value::Time("13:45:00.5".into()));
    assert_eq!(g(r1, "c_timetz"), Value::Time("13:45:00+07:00".into()));
    assert_eq!(
        g(r1, "c_ts"),
        Value::DateTime("2024-05-31T13:45:00.123456".into())
    );
    assert_eq!(
        g(r1, "c_tstz"),
        Value::Timestamp("2024-05-31T06:45:00.5Z".into())
    );
    assert_eq!(
        g(r1, "c_uuid"),
        Value::Uuid("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11".into())
    );
    assert_eq!(
        g(r1, "c_json"),
        Value::Json(serde_json::json!({"a": [1, 2]}))
    );
    assert_eq!(
        g(r1, "c_jsonb"),
        Value::Json(serde_json::json!({"b": {"c": null}}))
    );
    assert_eq!(
        g(r1, "a_i4"),
        Value::Array(vec![int(1), int(2), Value::Null])
    );
    assert_eq!(
        g(r1, "a_text"),
        Value::Array(vec![text("a"), text("b c"), Value::Null])
    );
    assert_eq!(
        g(r1, "a_num"),
        Value::Array(vec![
            Value::Decimal("1.50".into()),
            Value::Decimal("2".into())
        ])
    );
    assert_eq!(
        g(r1, "a_tstz"),
        Value::Array(vec![Value::Timestamp("2024-01-01T00:00:00Z".into())])
    );
    assert_eq!(
        g(r1, "a_bool"),
        Value::Array(vec![Value::Bool(true), Value::Bool(false)])
    );
    // Everything else comes back as the server's text form.
    assert_eq!(g(r1, "c_enum"), text("happy"));
    assert_eq!(g(r1, "c_inet"), text("10.0.0.1/8"));
    assert_eq!(g(r1, "c_cidr"), text("10.1.0.0/16"));
    assert_eq!(g(r1, "c_interval"), text("1 day 02:03:04"));
    assert_eq!(g(r1, "c_range"), text("[1,10)"));
    assert_eq!(g(r1, "c_point"), text("(1,2)"));
    assert_eq!(g(r1, "c_comp"), text("(1,x)"));
    assert_eq!(g(r1, "c_dom"), int(5));
    assert_eq!(g(r1, "c_money"), text("$12.34"));
    assert_eq!(g(r1, "c_xml"), text("<a>1</a>"));
    assert_eq!(g(r1, "c_bit"), text("101"));
    assert_eq!(g(r1, "c_mac"), text("08:00:2b:01:02:03"));
    assert_eq!(g(r1, "a_enum"), text("{sad,happy}"));

    let r2 = row(2);
    assert_eq!(g(r2, "c_i2"), int(-32768));
    assert_eq!(g(r2, "c_i4"), int(-2_147_483_648));
    assert_eq!(g(r2, "c_i8"), int(i64::MAX));
    assert_eq!(
        g(r2, "c_num"),
        Value::Decimal("9999999999999999999999999999.9999999999".into())
    );
    assert!(matches!(g(r2, "c_f4"), Value::Float(f) if f.is_nan()));
    assert_eq!(g(r2, "c_f8"), Value::Float(f64::NEG_INFINITY));
    assert_eq!(g(r2, "c_text"), text(""));
    assert_eq!(g(r2, "c_bytea"), Value::Bytes(vec![]));
    assert_eq!(g(r2, "c_date"), text("infinity"));
    assert_eq!(g(r2, "c_ts"), Value::DateTime("0001-01-01T00:00:00".into()));
    assert_eq!(g(r2, "c_tstz"), text("infinity"));
    assert_eq!(g(r2, "c_json"), Value::Json(serde_json::json!([])));
    assert_eq!(g(r2, "c_jsonb"), Value::Json(serde_json::Value::Null));
    assert_eq!(g(r2, "a_i4"), Value::Array(vec![]));
    assert_eq!(g(r2, "a_text"), Value::Array(vec![text(""), text("NULL")]));

    let r3 = row(3);
    for (i, c) in rec.cols.iter().enumerate() {
        if c.name != "id" {
            assert_eq!(rec.rows[r3][i], Value::Null, "{}", c.name);
        }
    }

    let r4 = row(4);
    assert_eq!(g(r4, "c_i8"), int(i64::MIN));
    assert_eq!(g(r4, "c_num"), Value::Decimal("NaN".into()));
    assert_eq!(g(r4, "c_f4"), Value::Float(f64::INFINITY));
    assert!(matches!(g(r4, "c_f8"), Value::Float(f) if f.is_nan()));

    // Column metadata.
    let meta = |n: &str| rec.cols[rec.col(n)].clone();
    assert_eq!(meta("c_i8").type_name, "int8");
    assert_eq!(meta("c_i8").value_kind, ValueKind::Int);
    assert_eq!(meta("c_num").type_name, "numeric");
    assert_eq!(meta("c_num").value_kind, ValueKind::Decimal);
    assert_eq!(meta("c_tstz").type_name, "timestamptz");
    assert_eq!(meta("c_tstz").value_kind, ValueKind::Timestamp);
    assert_eq!(meta("a_i4").type_name, "int4[]");
    assert_eq!(meta("a_i4").value_kind, ValueKind::Array);
    assert_eq!(meta("c_enum").type_name, "mood");
    assert_eq!(meta("c_enum").value_kind, ValueKind::Text);
    assert_eq!(meta("c_inet").type_name, "inet");
    assert_eq!(meta("a_enum").value_kind, ValueKind::Text);
}

async fn values(port: u16) {
    let s = fixture(port).await;
    // Through the SQL editor path...
    let mut rec = Rec::default();
    s.execute(
        QueryRequest::new("SELECT * FROM dbs_it.all_types ORDER BY id"),
        &mut rec,
    )
    .await
    .unwrap();
    check_values(&rec);
    // ...and through the table browser path.
    let mut rec = Rec::default();
    let req = TableDataRequest {
        query_id: Uuid::new_v4(),
        table: table_path(SCHEMA, ObjectKind::Table, "all_types"),
        filters: vec![],
        raw_where: None,
        sort: vec![SortKey {
            column: "id".into(),
            descending: false,
        }],
        offset: 0,
        limit: 100,
        batch_size: None,
    };
    s.fetch_table(req, &mut rec).await.unwrap();
    check_values(&rec);
    assert_eq!(
        rec.ends[0].outcome,
        EndOutcome::Rows {
            row_count: 4,
            truncated: false
        }
    );
}
on_both!(values, values_pg16, values_pg12);

async fn unusual_types(port: u16) {
    let s = fixture(port).await;
    let r = rows(
        &*s,
        "SELECT '1 year'::interval, int4range(1,5), ARRAY['10.0.0.1'::inet], \
                (1,'x')::dbs_it.pair, 'ok'::dbs_it.mood, NULL::dbs_it.mood, '1 2'::int2vector, \
                'x'::\"char\", 42::oid, to_tsvector('simple', 'a b'), point(1,2), '(1,2),(3,4)'::box",
    )
    .await;
    let row = &r[0];
    assert_eq!(row[0], text("1 year"));
    assert_eq!(row[1], text("[1,5)"));
    assert_eq!(row[2], text("{10.0.0.1}"));
    assert_eq!(row[3], text("(1,x)"));
    assert_eq!(row[4], text("ok"));
    assert_eq!(row[5], Value::Null);
    assert_eq!(row[6], text("1 2"));
    assert_eq!(row[7], text("x"));
    assert_eq!(row[8], int(42));
    assert_eq!(row[9], text("'a':1 'b':2"));
    assert_eq!(row[10], text("(1,2)"));
    assert_eq!(row[11], text("(3,4),(1,2)"));
    // A timestamptz is normalised to UTC even when the column was written with an offset.
    assert_eq!(
        scalar(&*s, "SELECT '2024-05-31 23:30:00-05'::timestamptz").await,
        Value::Timestamp("2024-06-01T04:30:00Z".into())
    );
    // Unknown or weird composite types and functions returning records.
    let r = rows(&*s, "SELECT * FROM (VALUES ((1,'a')::dbs_it.pair)) v(x)").await;
    assert_eq!(r[0][0], text("(1,a)"));
}
on_both!(unusual_types, unusual_types_pg16, unusual_types_pg12);

// ------------------------------------------------------------------------------------------
// fetch_table
// ------------------------------------------------------------------------------------------

fn req(table: &str) -> TableDataRequest {
    TableDataRequest {
        query_id: Uuid::new_v4(),
        table: table_path(SCHEMA, ObjectKind::Table, table),
        filters: vec![],
        raw_where: None,
        sort: vec![SortKey {
            column: "id".into(),
            descending: false,
        }],
        offset: 0,
        limit: 1000,
        batch_size: None,
    }
}

fn filt(column: &str, op: FilterOp, values: &[&str]) -> ColumnFilter {
    ColumnFilter {
        column: column.into(),
        op,
        values: values.iter().map(|s| s.to_string()).collect(),
    }
}

async fn ids(s: &dyn Session, mut r: TableDataRequest, filters: Vec<ColumnFilter>) -> Vec<i64> {
    r.filters = filters;
    let mut rec = Rec::default();
    let count_req = TableCountRequest {
        table: r.table.clone(),
        filters: r.filters.clone(),
        raw_where: r.raw_where.clone(),
    };
    let whole_table = r.offset == 0 && r.limit >= 1000;
    s.fetch_table(r, &mut rec)
        .await
        .unwrap_or_else(|e| panic!("fetch failed: {e}"));
    let id = rec.col("id");
    let out: Vec<i64> = rec
        .rows
        .iter()
        .map(|row| match row[id] {
            Value::Int(i) => i,
            ref v => panic!("{v:?}"),
        })
        .collect();
    if whole_table {
        assert_eq!(s.count_table(count_req).await.unwrap(), out.len() as u64);
    }
    out
}

async fn filters(port: u16) {
    let s = fixture(port).await;
    let s = &*s;
    let r = || req("people");
    use FilterOp::*;
    assert_eq!(ids(s, r(), vec![]).await, [1, 2, 3, 4, 5, 6, 7]);
    assert_eq!(ids(s, r(), vec![filt("name", Eq, &["bob"])]).await, [2]);
    assert_eq!(ids(s, r(), vec![filt("id", Eq, &["3"])]).await, [3]);
    assert_eq!(
        ids(s, r(), vec![filt("age", Ne, &["30"])]).await,
        [2, 4, 5, 6, 7]
    );
    assert_eq!(ids(s, r(), vec![filt("age", Lt, &["30"])]).await, [2, 6]);
    assert_eq!(ids(s, r(), vec![filt("age", Le, &["30"])]).await, [1, 2, 6]);
    assert_eq!(ids(s, r(), vec![filt("age", Gt, &["35"])]).await, [4, 7]);
    assert_eq!(ids(s, r(), vec![filt("age", Ge, &["35"])]).await, [4, 5, 7]);
    assert_eq!(
        ids(s, r(), vec![filt("name", Contains, &["ALI"])]).await,
        [1, 6]
    );
    assert_eq!(
        ids(s, r(), vec![filt("name", Contains, &["ali"])]).await,
        [1, 6]
    );
    // Wildcards in the user's text are literal.
    assert_eq!(
        ids(s, r(), vec![filt("note", Contains, &["100%"])]).await,
        [1]
    );
    assert_eq!(ids(s, r(), vec![filt("note", Contains, &["_"])]).await, [2]);
    assert_eq!(
        ids(s, r(), vec![filt("name", Contains, &["f_g%"])]).await,
        [7]
    );
    assert_eq!(
        ids(s, r(), vec![filt("note", Contains, &["\\"])]).await,
        [5]
    );
    assert_eq!(
        ids(s, r(), vec![filt("name", NotContains, &["a"])]).await,
        [2, 5, 7]
    );
    assert_eq!(
        ids(s, r(), vec![filt("name", StartsWith, &["al"])]).await,
        [1, 6]
    );
    assert_eq!(
        ids(s, r(), vec![filt("name", StartsWith, &["_"])]).await,
        Vec::<i64>::new()
    );
    assert_eq!(
        ids(s, r(), vec![filt("name", EndsWith, &["E"])]).await,
        [1, 4, 5]
    );
    assert_eq!(ids(s, r(), vec![filt("name", EndsWith, &["%"])]).await, [7]);
    assert_eq!(
        ids(s, r(), vec![filt("age", IsNull, &["ignored"])]).await,
        [3]
    );
    assert_eq!(
        ids(s, r(), vec![filt("note", IsNotNull, &[])]).await,
        [1, 2, 4, 5, 6, 7]
    );
    assert_eq!(
        ids(s, r(), vec![filt("id", In, &["1", "3", "99"])]).await,
        [1, 3]
    );
    assert_eq!(
        ids(s, r(), vec![filt("id", In, &[])]).await,
        Vec::<i64>::new()
    );
    assert_eq!(ids(s, r(), vec![filt("note", Eq, &["O'Brien"])]).await, [4]);
    assert_eq!(
        ids(s, r(), vec![filt("note", Eq, &["back\\slash"])]).await,
        [5]
    );
    // Contains also works on non-text columns (compared as text).
    assert_eq!(
        ids(s, r(), vec![filt("age", Contains, &["5"])]).await,
        [2, 5, 7]
    );
    // Filters combine with AND.
    assert_eq!(
        ids(
            s,
            r(),
            vec![filt("age", Ge, &["30"]), filt("name", Contains, &["e"])]
        )
        .await,
        [1, 4, 5]
    );
    // Attack strings are just data.
    for evil in [
        "'; DROP TABLE dbs_it.people; --",
        "\\'; DROP TABLE dbs_it.people; --",
        "x' OR '1'='1",
    ] {
        assert_eq!(
            ids(s, r(), vec![filt("name", Eq, &[evil])]).await,
            Vec::<i64>::new()
        );
        assert_eq!(
            ids(s, r(), vec![filt("name", Contains, &[evil])]).await,
            Vec::<i64>::new()
        );
        assert_eq!(
            ids(s, r(), vec![filt("name", In, &[evil, "bob"])]).await,
            [2]
        );
    }
    assert_eq!(
        scalar(s, "SELECT count(*) FROM dbs_it.people").await,
        int(7)
    );
    // A type mismatch is a query error, not a crash.
    let mut rec = Rec::default();
    let mut bad = r();
    bad.filters = vec![filt("age", Eq, &["abc"])];
    assert!(matches!(
        s.fetch_table(bad, &mut rec).await,
        Err(Error::Query { .. })
    ));
    let mut bad = r();
    bad.filters = vec![filt("no_such_column", Eq, &["1"])];
    assert!(matches!(
        s.fetch_table(bad, &mut rec).await,
        Err(Error::Query { .. })
    ));
    // The pool keeps working afterwards.
    assert_eq!(ids(s, r(), vec![filt("id", Eq, &["1"])]).await, [1]);
}
on_both!(filters, filters_pg16, filters_pg12);

async fn sort_paging_raw(port: u16) {
    let s = fixture(port).await;
    let s = &*s;
    let mut r = req("people");
    r.sort = vec![
        SortKey {
            column: "age".into(),
            descending: true,
        },
        SortKey {
            column: "id".into(),
            descending: false,
        },
    ];
    // NULLs sort first in descending order in PostgreSQL.
    assert_eq!(ids(s, r.clone(), vec![]).await, [3, 7, 4, 5, 1, 2, 6]);
    let mut p = req("people");
    p.offset = 2;
    p.limit = 3;
    assert_eq!(ids(s, p.clone(), vec![]).await, [3, 4, 5]);
    p.offset = 100;
    assert_eq!(ids(s, p, vec![]).await, Vec::<i64>::new());
    let mut z = req("people");
    z.limit = 0;
    assert_eq!(ids(s, z, vec![]).await, Vec::<i64>::new());

    let mut w = req("people");
    w.raw_where = Some("age > 20 AND name ILIKE 'a%'".into());
    assert_eq!(ids(s, w.clone(), vec![]).await, [1, 6]);
    assert_eq!(
        ids(s, w.clone(), vec![filt("age", FilterOp::Lt, &["25"])]).await,
        [6]
    );
    // Trailing comments and OR do not leak out of the parenthesised clause.
    w.raw_where = Some("age = 30 OR age = 25 -- trailing".into());
    assert_eq!(
        ids(s, w.clone(), vec![filt("id", FilterOp::Ne, &["1"])]).await,
        [2]
    );
    // Syntax errors are query errors.
    w.raw_where = Some("age >".into());
    let mut rec = Rec::default();
    assert!(matches!(
        s.fetch_table(w.clone(), &mut rec).await,
        Err(Error::Query { .. })
    ));
    // A second statement is not accepted, and writes are impossible (READ ONLY transaction).
    w.raw_where = Some("true; DELETE FROM dbs_it.people".into());
    assert!(matches!(
        s.fetch_table(w.clone(), &mut rec).await,
        Err(Error::Query { .. })
    ));
    w.raw_where = Some("id = (SELECT count(*) FROM (SELECT nextval('dbs_it.seq1')) q)".into());
    let err = s.fetch_table(w, &mut rec).await.unwrap_err();
    assert!(err.to_string().contains("read-only"), "{err}");
    assert_eq!(
        scalar(s, "SELECT count(*) FROM dbs_it.people").await,
        int(7)
    );

    // Weird identifiers.
    let mut wr = req("Weird \"Table\" Name");
    wr.sort = vec![SortKey {
        column: "Col A".into(),
        descending: false,
    }];
    let mut rec = Rec::default();
    wr.filters = vec![filt("we\"ird", FilterOp::Ne, &["one"])];
    s.fetch_table(wr, &mut rec).await.unwrap();
    assert_eq!(
        rec.cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        ["id", "Col A", "we\"ird"]
    );
    assert_eq!(
        rec.rows,
        vec![
            vec![int(2), text("a"), text("two")],
            vec![int(3), text("m"), text("three")]
        ]
    );
    // Views work as well.
    let mut v = req("v_children");
    v.table = table_path(SCHEMA, ObjectKind::View, "v_children");
    assert_eq!(
        ids(s, v, vec![filt("note", FilterOp::In, &["a", "c"])]).await,
        [1, 3]
    );
    // And a table in the odd schema.
    let mut o = req("T x");
    o.table = table_path("Dbs It \"S\"", ObjectKind::Table, "T x");
    o.sort = vec![SortKey {
        column: "y z".into(),
        descending: true,
    }];
    let mut rec = Rec::default();
    s.fetch_table(o, &mut rec).await.unwrap();
    assert_eq!(rec.rows, vec![vec![int(2), int(20)], vec![int(1), int(10)]]);
    // A missing table is a query error.
    let mut rec = Rec::default();
    assert!(matches!(
        s.fetch_table(req("nope"), &mut rec).await,
        Err(Error::Query { .. })
    ));
    // Path that is not a table.
    let mut bad = req("people");
    bad.table = schema_path(SCHEMA);
    assert!(matches!(
        s.fetch_table(bad, &mut rec).await,
        Err(Error::NotFound(_))
    ));
}
on_both!(sort_paging_raw, sort_paging_raw_pg16, sort_paging_raw_pg12);

// ------------------------------------------------------------------------------------------
// Streaming, cancellation, concurrency
// ------------------------------------------------------------------------------------------

async fn stream_big(port: u16) {
    let s = fixture(port).await;
    let mut r = req("big");
    r.limit = 1_000_000;
    r.batch_size = Some(1000);
    let started = Instant::now();
    let mut rec = Rec::default();
    s.fetch_table(r, &mut rec).await.unwrap();
    assert_eq!(rec.rows.len(), 200_000);
    assert_eq!(rec.batches.len(), 200);
    assert!(rec.batches.iter().all(|b| *b == 1000));
    assert!(rec.first_rows_at.unwrap() < rec.ended_at.unwrap());
    assert!(rec.first_rows_at.unwrap() - started < Duration::from_secs(2));
    assert_eq!(rec.rows[0][0], int(1));
    assert_eq!(rec.rows[199_999][0], int(200_000));
    assert_eq!(
        rec.ends[0].outcome,
        EndOutcome::Rows {
            row_count: 200_000,
            truncated: false
        }
    );
    assert_eq!(rec.cols.len(), 3);

    // Default batch size, with an offset in the middle.
    let mut r = req("big");
    r.offset = 150_000;
    r.limit = 5_500;
    let mut rec = Rec::default();
    s.fetch_table(r, &mut rec).await.unwrap();
    assert_eq!(rec.batches, [1000, 1000, 1000, 1000, 1000, 500]);
    assert_eq!(rec.rows[0][0], int(150_001));
    // Filters + count on the big table.
    let n = s
        .count_table(TableCountRequest {
            table: table_path(SCHEMA, ObjectKind::Table, "big"),
            filters: vec![filt("grp", FilterOp::Eq, &["3"])],
            raw_where: Some("id <= 100000".into()),
        })
        .await
        .unwrap();
    assert_eq!(n, 10_000);
    assert_eq!(
        s.count_table(TableCountRequest {
            table: table_path(SCHEMA, ObjectKind::Table, "big"),
            filters: vec![],
            raw_where: None
        })
        .await
        .unwrap(),
        200_000
    );

    // The SQL editor path streams too.
    let mut q = QueryRequest::new("SELECT * FROM dbs_it.big");
    q.batch_size = Some(5000);
    let mut rec = Rec::default();
    s.execute(q, &mut rec).await.unwrap();
    assert_eq!(rec.batches.len(), 40);
    assert_eq!(rec.rows.len(), 200_000);
}
on_both!(stream_big, stream_big_pg16, stream_big_pg12);

/// Sink that refuses the stream after `limit` batches.
struct StopAfter {
    limit: usize,
    seen: usize,
    err: fn() -> Error,
}

impl ResultSink for StopAfter {
    fn on_columns(&mut self, _: Vec<dbstudio_core::ColumnMeta>) -> Result<()> {
        Ok(())
    }
    fn on_rows(&mut self, _: Vec<Vec<Value>>) -> Result<()> {
        self.seen += 1;
        if self.seen >= self.limit {
            Err((self.err)())
        } else {
            Ok(())
        }
    }
    fn on_message(&mut self, _: String) -> Result<()> {
        Ok(())
    }
    fn on_statement_end(&mut self, _: StatementEnd) -> Result<()> {
        Ok(())
    }
}

async fn assert_connection_clean(s: &dyn Session) {
    // VACUUM refuses to run inside a transaction block, so it fails on any connection that
    // was returned to the pool with an open (or aborted) transaction. Run it several times so
    // that every pooled connection gets checked.
    let probes = (0..4).map(|_| async { exec(s, "VACUUM dbs_it.people").await });
    futures_join_all(probes).await;
    assert_eq!(scalar(s, "SELECT 1").await, int(1));
}

async fn sink_stops_stream(port: u16) {
    let s = fixture(port).await;
    for _ in 0..3 {
        let mut r = req("big");
        r.limit = 1_000_000;
        r.batch_size = Some(500);
        let mut sink = StopAfter {
            limit: 3,
            seen: 0,
            err: || Error::Cancelled,
        };
        let started = Instant::now();
        let err = s.fetch_table(r, &mut sink).await.unwrap_err();
        assert!(matches!(err, Error::Cancelled), "{err:?}");
        assert_eq!(sink.seen, 3);
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_connection_clean(&*s).await;
    }
    // Any other sink error is propagated untouched.
    let mut r = req("big");
    r.limit = 1_000_000;
    let mut sink = StopAfter {
        limit: 1,
        seen: 0,
        err: || Error::Other("sink failed".into()),
    };
    let err = s.fetch_table(r, &mut sink).await.unwrap_err();
    assert!(
        matches!(err, Error::Other(ref m) if m == "sink failed"),
        "{err:?}"
    );
    assert_connection_clean(&*s).await;
    // Same for execute.
    let mut sink = StopAfter {
        limit: 2,
        seen: 0,
        err: || Error::Cancelled,
    };
    let mut q = QueryRequest::new("SELECT * FROM dbs_it.big");
    q.batch_size = Some(100);
    assert!(matches!(
        s.execute(q, &mut sink).await,
        Err(Error::Cancelled)
    ));
    assert_connection_clean(&*s).await;
}
on_both!(
    sink_stops_stream,
    sink_stops_stream_pg16,
    sink_stops_stream_pg12
);

async fn cancel_running(port: u16) {
    let s = fixture(port).await;
    let q = QueryRequest::new("SELECT pg_sleep(60)");
    let id = q.query_id;
    let mut rec = Rec::default();
    let started = Instant::now();
    let (res, cancelled) = tokio::join!(s.execute(q, &mut rec), async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        s.cancel(id).await
    });
    cancelled.unwrap();
    assert!(matches!(res, Err(Error::Cancelled)), "{res:?}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert_connection_clean(&*s).await;

    // Cancelling an id that is not running is NotFound.
    assert!(matches!(
        s.cancel(Uuid::new_v4()).await,
        Err(Error::NotFound(_))
    ));

    // A long-running table fetch can be cancelled as well.
    let mut r = req("big");
    r.limit = 1_000_000;
    r.raw_where = Some("pg_sleep(0.01) IS NOT NULL".into());
    let id = r.query_id;
    let mut rec = Rec::default();
    let started = Instant::now();
    let (res, _) = tokio::join!(s.fetch_table(r, &mut rec), async {
        tokio::time::sleep(Duration::from_millis(400)).await;
        s.cancel(id).await
    });
    assert!(matches!(res, Err(Error::Cancelled)), "{res:?}");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    assert_connection_clean(&*s).await;

    // Cancel in the middle of a multi-statement script: the remaining statements do not run.
    let q = QueryRequest::new(
        "SELECT pg_sleep(30); CREATE TABLE public.dbs_it_should_not_exist(a int)",
    );
    let id = q.query_id;
    let mut rec = Rec::default();
    let (res, _) = tokio::join!(s.execute(q, &mut rec), async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        s.cancel(id).await
    });
    assert!(matches!(res, Err(Error::Cancelled)));
    assert_eq!(
        scalar(
            &*s,
            "SELECT count(*) FROM pg_tables WHERE tablename = 'dbs_it_should_not_exist'"
        )
        .await,
        int(0)
    );
}
on_both!(cancel_running, cancel_running_pg16, cancel_running_pg12);

async fn concurrency(port: u16) {
    let s = fixture(port).await;
    let s = &*s;
    let root = dbstudio_core::ObjectPath::root();
    let big = table_path(SCHEMA, ObjectKind::Table, "big");
    let children_path = table_path(SCHEMA, ObjectKind::Table, "children");
    let started = Instant::now();
    let (a, b, c, d, e, f, g) = tokio::join!(
        s.ping(),
        s.server_info(),
        s.list_children(&root),
        s.table_details(&big),
        s.object_ddl(&children_path),
        async {
            let mut rec = Rec::default();
            let mut r = req("big");
            r.limit = 50_000;
            s.fetch_table(r, &mut rec).await.map(|_| rec.rows.len())
        },
        s.count_table(TableCountRequest {
            table: big.clone(),
            filters: vec![],
            raw_where: None
        }),
    );
    a.unwrap();
    assert_eq!(b.unwrap().product, "PostgreSQL");
    assert!(!c.unwrap().is_empty());
    assert_eq!(d.unwrap().columns.len(), 3);
    assert!(e.unwrap().contains("CREATE TABLE"));
    assert_eq!(f.unwrap(), 50_000);
    assert_eq!(g.unwrap(), 200_000);

    // More concurrent statements than the pool allows (4) queue up instead of failing.
    let sleeps = (0..10).map(|_| async { scalar(s, "SELECT pg_sleep(0.3)::text").await });
    let results = futures_join_all(sleeps).await;
    assert_eq!(results.len(), 10);
    let took = started.elapsed();
    assert!(took < Duration::from_secs(10), "{took:?}");
    // Concurrent lazily-created pools of other databases.
    let other = dbstudio_core::ObjectPath::root().child(ObjectKind::Database, "dbs_it_other");
    let (x, y) = tokio::join!(
        async {
            let f = s.list_children(&other).await.unwrap();
            s.list_children(&f[0].path).await.unwrap().len()
        },
        s.list_children(&root)
    );
    assert!(x >= 2);
    y.unwrap();
}
on_both!(concurrency, concurrency_pg16, concurrency_pg12);

async fn futures_join_all<F: std::future::Future>(
    it: impl IntoIterator<Item = F>,
) -> Vec<F::Output> {
    let mut set = Vec::new();
    for f in it {
        set.push(f);
    }
    // Poll all futures concurrently on this task.
    let mut pinned: Vec<std::pin::Pin<Box<F>>> = set.into_iter().map(Box::pin).collect();
    let mut out: Vec<Option<F::Output>> = (0..pinned.len()).map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut pending = false;
        for (i, f) in pinned.iter_mut().enumerate() {
            if out[i].is_none() {
                match f.as_mut().poll(cx) {
                    std::task::Poll::Ready(v) => out[i] = Some(v),
                    std::task::Poll::Pending => pending = true,
                }
            }
        }
        if pending {
            std::task::Poll::Pending
        } else {
            std::task::Poll::Ready(())
        }
    })
    .await;
    out.into_iter().map(|o| o.unwrap()).collect()
}

// ------------------------------------------------------------------------------------------
// execute()
// ------------------------------------------------------------------------------------------

async fn execute_scripts(port: u16) {
    let s = fixture(port).await;
    let s = &*s;
    exec(
        s,
        "DROP SCHEMA IF EXISTS dbs_it_exec CASCADE; CREATE SCHEMA dbs_it_exec",
    )
    .await;
    let results = exec(
        s,
        "CREATE TABLE dbs_it_exec.t (id serial PRIMARY KEY, v text);\n\
         INSERT INTO dbs_it_exec.t (v) VALUES ('a'), ('b'), ('c');\n\
         SELECT id, v FROM dbs_it_exec.t ORDER BY id;\n\
         UPDATE dbs_it_exec.t SET v = 'z' WHERE id > 1;\n\
         DELETE FROM dbs_it_exec.t WHERE id = 1;\n\
         INSERT INTO dbs_it_exec.t (v) VALUES ('r') RETURNING id, v;\n\
         WITH x AS (INSERT INTO dbs_it_exec.t (v) VALUES ('w') RETURNING id) SELECT id FROM x;\n\
         EXPLAIN SELECT * FROM dbs_it_exec.t;\n\
         SET search_path = public;\n\
         SHOW search_path;\n\
         DROP TABLE IF EXISTS dbs_it_exec.nothing;\n",
    )
    .await;
    assert_eq!(results.len(), 11);
    assert!(matches!(results[0].outcome, StatementOutcome::Done));
    assert!(matches!(
        results[1].outcome,
        StatementOutcome::Affected { rows: 3 }
    ));
    match &results[2].outcome {
        StatementOutcome::Rows(rs) => {
            assert_eq!(
                rs.columns
                    .iter()
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>(),
                ["id", "v"]
            );
            assert_eq!(rs.columns[0].type_name, "int4");
            assert_eq!(rs.columns[0].value_kind, ValueKind::Int);
            assert_eq!(
                rs.rows,
                vec![
                    vec![int(1), text("a")],
                    vec![int(2), text("b")],
                    vec![int(3), text("c")]
                ]
            );
            assert!(!rs.truncated);
        }
        o => panic!("{o:?}"),
    }
    assert!(matches!(
        results[3].outcome,
        StatementOutcome::Affected { rows: 2 }
    ));
    assert!(matches!(
        results[4].outcome,
        StatementOutcome::Affected { rows: 1 }
    ));
    match &results[5].outcome {
        StatementOutcome::Rows(rs) => assert_eq!(rs.rows, vec![vec![int(4), text("r")]]),
        o => panic!("{o:?}"),
    }
    match &results[6].outcome {
        StatementOutcome::Rows(rs) => assert_eq!(rs.rows, vec![vec![int(5)]]),
        o => panic!("{o:?}"),
    }
    match &results[7].outcome {
        StatementOutcome::Rows(rs) => {
            assert!(!rs.rows.is_empty());
            assert!(matches!(&rs.rows[0][0], Value::Text(t) if t.contains("Seq Scan")));
        }
        o => panic!("{o:?}"),
    }
    assert!(matches!(results[8].outcome, StatementOutcome::Done));
    match &results[9].outcome {
        StatementOutcome::Rows(rs) => assert_eq!(rs.rows, vec![vec![text("public")]]),
        o => panic!("{o:?}"),
    }
    // DROP ... IF EXISTS produces a NOTICE for the missing table.
    assert!(matches!(results[10].outcome, StatementOutcome::Done));
    assert!(
        results[10]
            .messages
            .iter()
            .any(|m| m.starts_with("NOTICE:") && m.contains("does not exist")),
        "{:?}",
        results[10].messages
    );
    let summary = s
        .execute(
            QueryRequest::new("SELECT 1; SELECT 2;; ;"),
            &mut Rec::default(),
        )
        .await
        .unwrap();
    assert_eq!(summary.statements, 2);
    let summary = s
        .execute(
            QueryRequest::new("  -- nothing here\n"),
            &mut Rec::default(),
        )
        .await
        .unwrap();
    assert_eq!(summary.statements, 0);

    // RAISE NOTICE from a DO block.
    let r = exec(
        s,
        "DO $$ BEGIN RAISE NOTICE 'hello %', 42; RAISE WARNING 'careful'; END $$",
    )
    .await;
    assert_eq!(r[0].messages, ["NOTICE: hello 42", "WARNING: careful"]);

    // Quirky text: semicolons inside literals, comments and dollar quotes do not split.
    let r = exec(s, "SELECT 'a;b', $q$c;d$q$ /* e;f */, \"x;y\" FROM (SELECT 1 AS \"x;y\") t -- g;h\n; SELECT 2").await;
    assert_eq!(r.len(), 2);
    match &r[0].outcome {
        StatementOutcome::Rows(rs) => assert_eq!(rs.rows[0][..2], [text("a;b"), text("c;d")]),
        o => panic!("{o:?}"),
    }
}
on_both!(execute_scripts, execute_scripts_pg16, execute_scripts_pg12);

async fn execute_options_and_errors(port: u16) {
    let s = fixture(port).await;
    let s = &*s;
    // Syntax error with a position relative to the whole script.
    let err = exec_err(s, "SELECT 1; SELEC 2").await;
    match err {
        Error::Query { message, position } => {
            assert!(message.contains("syntax error"), "{message}");
            assert_eq!(position, Some(10));
        }
        e => panic!("{e:?}"),
    }
    // Earlier statements were already delivered before the failure.
    let mut rec = Rec::default();
    let e = s
        .execute(
            QueryRequest::new("SELECT 1; SELECT 1/0; SELECT 3"),
            &mut rec,
        )
        .await
        .unwrap_err();
    assert!(matches!(&e, Error::Query { message, .. } if message.contains("division by zero")));
    assert_eq!(rec.ends.len(), 1);
    assert_connection_clean(s).await;

    // max_rows (cursor path and buffered path).
    let mut q = QueryRequest::new("SELECT g FROM generate_series(1, 50) g");
    q.max_rows = Some(10);
    q.batch_size = Some(4);
    let mut rec = Rec::default();
    s.execute(q, &mut rec).await.unwrap();
    assert_eq!(rec.rows.len(), 10);
    assert_eq!(rec.batches, [4, 4, 2]);
    assert_eq!(
        rec.ends[0].outcome,
        EndOutcome::Rows {
            row_count: 10,
            truncated: true
        }
    );
    let mut q = QueryRequest::new("SELECT g FROM generate_series(1, 10) g");
    q.max_rows = Some(10);
    let mut rec = Rec::default();
    s.execute(q, &mut rec).await.unwrap();
    assert_eq!(
        rec.ends[0].outcome,
        EndOutcome::Rows {
            row_count: 10,
            truncated: false
        }
    );
    let mut q = QueryRequest::new("SHOW ALL");
    q.max_rows = Some(5);
    let mut rec = Rec::default();
    s.execute(q, &mut rec).await.unwrap();
    assert_eq!(rec.rows.len(), 5);
    assert_eq!(
        rec.ends[0].outcome,
        EndOutcome::Rows {
            row_count: 5,
            truncated: true
        }
    );
    let mut q = QueryRequest::new("SELECT * FROM dbs_it.big");
    q.max_rows = Some(0);
    let mut rec = Rec::default();
    s.execute(q, &mut rec).await.unwrap();
    assert_eq!(
        rec.ends[0].outcome,
        EndOutcome::Rows {
            row_count: 0,
            truncated: true
        }
    );
    assert_connection_clean(s).await;

    // timeout_ms.
    let mut q = QueryRequest::new("SELECT pg_sleep(30)");
    q.timeout_ms = Some(300);
    let started = Instant::now();
    let err = s.execute(q, &mut Rec::default()).await.unwrap_err();
    assert!(matches!(err, Error::Timeout), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_connection_clean(s).await;
    let mut q = QueryRequest::new("SELECT 1");
    q.timeout_ms = Some(5000);
    s.execute(q, &mut Rec::default()).await.unwrap();

    // A server-side statement_timeout is an ordinary query error, not a user cancel.
    let err = exec_err(s, "SET statement_timeout = 200; SELECT pg_sleep(5)").await;
    assert!(
        matches!(&err, Error::Query { message, .. } if message.contains("statement timeout")),
        "{err:?}"
    );
    exec(s, "SET statement_timeout = 0").await;

    // database override.
    let mut q = QueryRequest::new("SELECT current_database()");
    q.database = Some("dbs_it_other".into());
    assert!(matches!(
        &execute_collect(s, q).await.unwrap()[0].outcome,
        StatementOutcome::Rows(rs) if rs.rows[0][0] == text("dbs_it_other")
    ));
    let mut q = QueryRequest::new("SELECT 1");
    q.database = Some("no_such_db_zzz".into());
    assert!(matches!(
        execute_collect(s, q).await.unwrap_err(),
        Error::Connection(_)
    ));
}
on_both!(
    execute_options_and_errors,
    execute_options_pg16,
    execute_options_pg12
);

async fn execute_transactions(port: u16) {
    let s = fixture(port).await;
    let s = &*s;
    exec(
        s,
        "DROP TABLE IF EXISTS dbs_it.tx_t; CREATE TABLE dbs_it.tx_t (a int)",
    )
    .await;
    // An explicit transaction inside one script, with cursor-streamed SELECTs inside it.
    let r = exec(
        s,
        "BEGIN; INSERT INTO dbs_it.tx_t VALUES (1), (2); SELECT count(*) FROM dbs_it.tx_t; COMMIT",
    )
    .await;
    assert!(matches!(&r[2].outcome, StatementOutcome::Rows(rs) if rs.rows[0][0] == int(2)));
    assert_eq!(scalar(s, "SELECT count(*) FROM dbs_it.tx_t").await, int(2));
    // A failed script that left a transaction open is rolled back when the connection returns.
    let e = exec_err(
        s,
        "BEGIN; INSERT INTO dbs_it.tx_t VALUES (3); INSERT INTO nope VALUES (1)",
    )
    .await;
    assert!(matches!(e, Error::Query { .. }));
    assert_eq!(scalar(s, "SELECT count(*) FROM dbs_it.tx_t").await, int(2));
    assert_connection_clean(s).await;
    // A script that opens a transaction and forgets it does not leak into the next call.
    exec(s, "BEGIN; INSERT INTO dbs_it.tx_t VALUES (4)").await;
    assert_eq!(scalar(s, "SELECT count(*) FROM dbs_it.tx_t").await, int(2));
    assert_connection_clean(s).await;
    // SELECTs with side effects are committed.
    exec(s, "SELECT setval('dbs_it.seq1', 7)").await;
    assert_eq!(
        scalar(s, "SELECT last_value FROM dbs_it.seq1").await,
        int(7)
    );
    exec(s, "DROP TABLE dbs_it.tx_t").await;
}
on_both!(
    execute_transactions,
    execute_transactions_pg16,
    execute_transactions_pg12
);

#[tokio::test]
async fn close_releases_everything() {
    if !enabled() {
        return;
    }
    let s = connect(PG16).await;
    s.ping().await.unwrap();
    s.close().await.unwrap();
    assert!(matches!(s.ping().await, Err(Error::Connection(_))));
    assert!(matches!(
        s.list_children(&dbstudio_core::ObjectPath::root()).await,
        Err(Error::Connection(_))
    ));
}
