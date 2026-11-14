//! Survey of the PostgreSQL driver against CockroachDB (and YugabyteDB with `heavy`).
#![allow(dead_code)]
mod common;
use common::*;
use dbstudio_core::*;
use dbstudio_driver_postgres::PostgresFlavorDriver;

fn port() -> u16 {
    std::env::var("DBSTUDIO_IT_COCKROACH_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(26258)
}

async fn crdb() -> Box<dyn Session> {
    let mut cfg = ConnectionConfig::new(DbKind::CockroachDb, "it", "127.0.0.1");
    cfg.port = port();
    cfg.database = Some("dbstudio".into());
    cfg.username = Some("dbstudio".into());
    cfg.tls.mode = TlsMode::Prefer;
    cfg.connect_timeout_ms = Some(5000);
    PostgresFlavorDriver::new(DbKind::CockroachDb)
        .connect(&cfg, Some(&Secret::new(PASSWORD)))
        .await
        .expect("connect")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn survey() {
    if !enabled() {
        return;
    }
    let s = crdb().await;
    eprintln!("INFO {:?}", s.server_info().await);
    eprintln!("CAPS {:?}", s.capabilities().map(|c| c.triggers));
    let setup = "DROP SCHEMA IF EXISTS sv CASCADE; CREATE SCHEMA sv; \
      CREATE TABLE sv.parent (id INT PRIMARY KEY, name TEXT NOT NULL); \
      CREATE TABLE sv.child (id INT PRIMARY KEY, pid INT REFERENCES sv.parent(id), v NUMERIC(10,2), j JSONB, ts TIMESTAMPTZ DEFAULT now()); \
      CREATE TABLE sv.norowid (a INT, b TEXT); \
      CREATE INDEX ON sv.child (v); CREATE VIEW sv.vw AS SELECT * FROM sv.parent; \
      CREATE SEQUENCE sv.sq; \
      INSERT INTO sv.parent VALUES (1,'a'),(2,'b'); INSERT INTO sv.child VALUES (1,1,1.5,'{\"k\":1}',now()); \
      INSERT INTO sv.norowid VALUES (1,'x'),(2,'y');";
    match execute_collect(&*s, QueryRequest::new(setup)).await {
        Ok(_) => eprintln!("SETUP ok"),
        Err(e) => eprintln!("SETUP ERR {e}"),
    }
    let root = ObjectPath::root();
    let dbs = s.list_children(&root).await;
    eprintln!("DBS {:?}", dbs.as_ref().map(|d| names(d)));
    let db = root.child(ObjectKind::Database, "dbstudio");
    let kids = s.list_children(&db).await;
    eprintln!("DBKIDS {:?}", kids.as_ref().map(|d| names(d)));
    let sch = db.child(ObjectKind::Folder, "schemas");
    eprintln!(
        "SCHEMAS {:?}",
        s.list_children(&sch).await.as_ref().map(|d| names(d))
    );
    let sv = sch.child(ObjectKind::Schema, "sv");
    let svk = s.list_children(&sv).await;
    eprintln!("SVKIDS {:?}", svk.as_ref().map(|d| names(d)));
    for f in [
        "tables",
        "views",
        "sequences",
        "functions",
        "procedures",
        "materializedViews",
    ] {
        let p = sv.child(ObjectKind::Folder, f);
        eprintln!(
            "FOLDER {f} {:?}",
            s.list_children(&p).await.as_ref().map(|d| names(d))
        );
    }
    let tp = |n: &str| {
        sv.child(ObjectKind::Folder, "tables")
            .child(ObjectKind::Table, n)
    };
    for t in ["parent", "child", "norowid"] {
        let p = tp(t);
        eprintln!(
            "COLS {t} {:?}",
            s.list_children(&p).await.as_ref().map(|d| names(d))
        );
        match s.table_details(&p).await {
            Ok(d) => eprintln!(
                "DETAILS {t} cols={:?} idx={:?} fks={:?}",
                d.columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>(),
                d.indexes.iter().map(|i| i.name.clone()).collect::<Vec<_>>(),
                d.foreign_keys.len()
            ),
            Err(e) => eprintln!("DETAILS {t} ERR {e}"),
        }
        eprintln!("DDL {t} {:?}", s.object_ddl(&p).await);
        eprintln!("EDIT {t} {:?}", s.editability(&p).await);
        let mut sink = CollectingSink::new();
        let r = s
            .fetch_table(
                TableDataRequest {
                    query_id: uuid::Uuid::new_v4(),
                    table: p.clone(),
                    filters: vec![],
                    raw_where: None,
                    sort: vec![],
                    offset: 0,
                    limit: 10,
                    batch_size: None,
                },
                &mut sink,
            )
            .await;
        eprintln!(
            "FETCH {t} {:?} {:?}",
            r.map(|_| ()),
            sink.into_results().first().map(|r| match &r.outcome {
                StatementOutcome::Rows(rs) => format!("{:?}", rs.rows),
                o => format!("{o:?}"),
            })
        );
        eprintln!(
            "COUNT {t} {:?}",
            s.count_table(TableCountRequest {
                table: p.clone(),
                filters: vec![],
                raw_where: None,
                query_id: None
            })
            .await
        );
    }
    eprintln!(
        "VIEWDDL {:?}",
        s.object_ddl(
            &sv.child(ObjectKind::Folder, "views")
                .child(ObjectKind::View, "vw")
        )
        .await
    );
    eprintln!(
        "SEQDDL {:?}",
        s.object_ddl(
            &sv.child(ObjectKind::Folder, "sequences")
                .child(ObjectKind::Sequence, "sq")
        )
        .await
    );
    let mreq = SchemaModelRequest {
        scope: sv.clone(),
        tables: vec![],
        include_views: true,
        include_comments: true,
        detail: ModelDetail::default(),
        sample_size: None,
    };
    match s.schema_model(mreq).await {
        Ok(m) => eprintln!(
            "MODEL ok tables={} rel={}",
            m.tables.len(),
            m.relations.len()
        ),
        Err(e) => eprintln!("MODEL ERR {e}"),
    }
    eprintln!(
        "SEARCH {:?}",
        s.search_objects(ObjectSearchRequest {
            query: "par".into(),
            scope: db.clone(),
            kinds: vec![],
            limit: 20
        })
        .await
        .as_ref()
        .map(|d| names(d))
    );
    for q in [
        ("SELECT * FROM sv.parent WHERE id = 1", false),
        (
            "SELECT * FROM sv.child c JOIN sv.parent p ON p.id=c.pid",
            false,
        ),
        ("SELECT * FROM sv.norowid", true),
    ] {
        let mut r = ExplainRequest {
            query_id: uuid::Uuid::new_v4(),
            text: q.0.into(),
            database: None,
            analyze: q.1,
            options: Default::default(),
            timeout_ms: None,
        };
        match s.explain(r.clone()).await {
            Ok(p) => eprintln!("EXPLAIN {} {:?}\n{}", q.0, p.format, p.text),
            Err(e) => eprintln!("EXPLAIN ERR {e}"),
        }
        r.options.verbose = true;
    }
    let lease = s
        .open_lease(LeaseOptions {
            database: None,
            schema: None,
            auto_commit: true,
        })
        .await;
    eprintln!("LEASE {:?}", lease.as_ref().map(|_| ()));
    if let Ok(l) = lease {
        eprintln!(
            "CTX {:?}",
            l.set_context(LeaseContext {
                database: Some("defaultdb".into()),
                schema: None
            })
            .await
        );
        let mut sink = CollectingSink::new();
        eprintln!(
            "CURDB {:?}",
            l.execute(QueryRequest::new("SELECT current_database()"), &mut sink)
                .await
                .map(|_| ())
        );
        eprintln!(
            "{:?}",
            sink.into_results()
                .first()
                .map(|r| format!("{:?}", r.outcome))
        );
        let _ = l.close().await;
    }
    // cancel
    let id = uuid::Uuid::new_v4();
    let s2: std::sync::Arc<Box<dyn Session>> = std::sync::Arc::new(s);
    let s3 = s2.clone();
    let h = tokio::spawn(async move {
        let mut q = QueryRequest::new("SELECT pg_sleep(20)");
        q.query_id = id;
        execute_collect(&**s3, q).await
    });
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    let t = std::time::Instant::now();
    eprintln!("CANCEL {:?}", s2.cancel(id).await);
    eprintln!(
        "CANCELRES {:?} in {:?}",
        h.await.unwrap().map(|_| ()),
        t.elapsed()
    );
}
