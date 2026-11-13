//! Integration tests against Oracle Database 23ai Free (`oraclefree`), run from the `oracle-it`
//! container. They only run when `DBSTUDIO_IT=1`; otherwise every test returns immediately.
//!
//! Every test owns its objects (names start with `IT_<TEST>_`) so tests can run in parallel in
//! the `DBSTUDIO` schema.

mod common;

use std::time::{Duration, Instant};

use common::*;
use dbstudio_core::{
    BulkInsertRequest, CellEdit, ChangeSet, ColumnFilter, DdlOptions, DdlRequest, DdlSection,
    DriverStatus, EditValue, EndOutcome, Error, ExplainRequest, FilterOp, LeaseContext,
    LeaseOptions, ModelDetail, NotEditableReason, ObjectKind, ObjectSearchRequest, OnConflict,
    PlanFormat, QueryRequest, RowChange, RowChangeSet, SchemaModelRequest, ScriptDialect, Session,
    SortKey, StatementOutcome, TableCountRequest, TableDataRequest, TableStreamRequest, TlsMode,
    TxOptions, TxStatus, Value, ValueKind, execute_collect,
};
use dbstudio_driver_oracle::OracleDriver;
use uuid::Uuid;

macro_rules! it {
    ($name:ident, $body:block) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $name() {
            if !enabled() {
                return;
            }
            init_client();
            assert_eq!(
                dbstudio_core::Driver::descriptor(&OracleDriver::new()).status,
                DriverStatus::Available,
                "the Oracle client must load in the integration environment"
            );
            $body
        }
    };
}

fn req(table: dbstudio_core::ObjectPath) -> TableDataRequest {
    TableDataRequest {
        query_id: Uuid::new_v4(),
        table,
        filters: vec![],
        raw_where: None,
        sort: vec![],
        offset: 0,
        limit: 100,
        batch_size: None,
    }
}

async fn fetch(s: &dyn Session, r: TableDataRequest) -> Rec {
    let mut rec = Rec::default();
    s.fetch_table(r, &mut rec).await.expect("fetch_table");
    rec
}

// ------------------------------------------------------------------------------------------
// Connection
// ------------------------------------------------------------------------------------------

it!(connect_and_server_info, {
    let s = app().await;
    s.ping().await.unwrap();
    let info = s.server_info().await.unwrap();
    assert_eq!(info.product, "Oracle Database");
    assert!(!info.version.is_empty(), "{info:?}");
    assert_eq!(
        info.extra.get("service").map(String::as_str),
        Some("FREEPDB1")
    );
    assert_eq!(info.extra.get("user").map(String::as_str), Some("DBSTUDIO"));
    assert_eq!(s.script_dialect(), ScriptDialect::Oracle);
    assert_eq!(scalar(&*s, "SELECT USER FROM dual").await, text("DBSTUDIO"));
    // The session is configured for lossless text: UTC and ISO formats.
    assert_eq!(
        scalar(&*s, "SELECT SESSIONTIMEZONE FROM dual").await,
        text("UTC")
    );
    s.close().await.unwrap();
});

it!(connection_errors_have_the_right_kind, {
    let cfg = config("dbstudio");
    let err = try_connect(&cfg, "wrong_password").await.err().unwrap();
    assert_eq!(err.kind(), "authentication", "{err}");
    assert!(!err.message().contains("wrong_password"));

    let mut bad_service = config("dbstudio");
    bad_service.database = Some("NO_SUCH_SERVICE".into());
    let err = try_connect(&bad_service, PASSWORD).await.err().unwrap();
    assert_eq!(err.kind(), "connection", "{err}");

    let mut closed_port = config("dbstudio");
    closed_port.port = 9;
    closed_port.connect_timeout_ms = Some(3000);
    let err = try_connect(&closed_port, PASSWORD).await.err().unwrap();
    assert_eq!(err.kind(), "connection", "{err}");

    // TCPS against the plain TCP listener must fail cleanly (and not hang).
    let mut tls = config("dbstudio");
    tls.tls.mode = TlsMode::Require;
    tls.connect_timeout_ms = Some(5000);
    let started = Instant::now();
    let err = try_connect(&tls, PASSWORD).await.err().unwrap();
    assert!(matches!(err.kind(), "tls" | "connection"), "{err}");
    assert!(started.elapsed() < Duration::from_secs(30));
});

it!(connection_variants, {
    // Easy Connect string in the database field.
    let mut easy = config("dbstudio");
    easy.database = Some(format!("{}:{}/{}", host(), port(), service()));
    easy.host = "ignored".into();
    let s = try_connect(&easy, PASSWORD).await.unwrap();
    assert_eq!(scalar(&*s, "SELECT USER FROM dual").await, text("DBSTUDIO"));

    // SYSDBA.
    let mut sys = config("SYS");
    sys.options.insert("connect_as".into(), "sysdba".into());
    let s = try_connect(&sys, PASSWORD).await.unwrap();
    assert_eq!(scalar(&*s, "SELECT USER FROM dual").await, text("SYS"));

    // SID (the container database root).
    let mut sid = config("SYSTEM");
    sid.database = None;
    sid.options.insert("sid".into(), "FREE".into());
    let s = try_connect(&sid, PASSWORD).await.unwrap();
    assert_eq!(
        scalar(&*s, "SELECT SYS_CONTEXT('USERENV','CON_NAME') FROM dual").await,
        text("CDB$ROOT")
    );
});

// ------------------------------------------------------------------------------------------
// Tree, details, search
// ------------------------------------------------------------------------------------------

const TREE_SCRIPT: &str = "\
CREATE TABLE IT_TREE_P (ID NUMBER(10) PRIMARY KEY, NAME VARCHAR2(50) NOT NULL);
CREATE TABLE IT_TREE_C (ID NUMBER(10) PRIMARY KEY, P_ID NUMBER(10) REFERENCES IT_TREE_P(ID), NOTE VARCHAR2(20));
CREATE INDEX IT_TREE_C_IX ON IT_TREE_C(NOTE);
CREATE VIEW IT_TREE_V AS SELECT ID, NAME FROM IT_TREE_P;
CREATE SEQUENCE IT_TREE_S;
CREATE SYNONYM IT_TREE_SYN FOR IT_TREE_P;
CREATE OR REPLACE PROCEDURE IT_TREE_PROC AS
BEGIN
  NULL;
END;
/
CREATE OR REPLACE FUNCTION IT_TREE_FN RETURN NUMBER AS
BEGIN
  RETURN 1;
END;
/
CREATE OR REPLACE PACKAGE IT_TREE_PK AS
  PROCEDURE x;
END IT_TREE_PK;
/
CREATE OR REPLACE PACKAGE BODY IT_TREE_PK AS
  PROCEDURE x IS BEGIN NULL; END;
END IT_TREE_PK;
/
CREATE OR REPLACE TRIGGER IT_TREE_TRG BEFORE INSERT ON IT_TREE_C FOR EACH ROW
BEGIN
  :new.NOTE := NVL(:new.NOTE, 'n');
END;
/
CREATE OR REPLACE TYPE IT_TREE_TY AS OBJECT (a NUMBER);
/
COMMENT ON TABLE IT_TREE_P IS 'parent table';
COMMENT ON COLUMN IT_TREE_P.NAME IS 'the name';
";

async fn tree_fixture(s: &dyn Session) {
    for (kind, name) in [
        ("TRIGGER", "IT_TREE_TRG"),
        ("TYPE", "IT_TREE_TY"),
        ("PACKAGE", "IT_TREE_PK"),
        ("FUNCTION", "IT_TREE_FN"),
        ("PROCEDURE", "IT_TREE_PROC"),
        ("SYNONYM", "IT_TREE_SYN"),
        ("SEQUENCE", "IT_TREE_S"),
        ("VIEW", "IT_TREE_V"),
        ("TABLE", "IT_TREE_C"),
        ("TABLE", "IT_TREE_P"),
    ] {
        drop_object(s, kind, name).await;
    }
    exec(s, TREE_SCRIPT).await;
}

it!(tree_levels_and_details, {
    let s = app().await;
    tree_fixture(&*s).await;
    let root = s
        .list_children(&dbstudio_core::ObjectPath::root())
        .await
        .unwrap();
    assert_eq!(
        root[0].name,
        "DBSTUDIO",
        "the connected user comes first: {:?}",
        names(&root)
    );
    assert_eq!(root[0].kind, ObjectKind::Schema);
    assert!(
        !names(&root).contains(&"SYS".to_string()),
        "system schemas are hidden"
    );
    assert!(
        !root.iter().any(|n| n.name == "roles"),
        "no DBA_ROLES access"
    );

    let folders = s.list_children(&root[0].path).await.unwrap();
    assert_eq!(
        names(&folders),
        [
            "tables",
            "views",
            "materializedViews",
            "sequences",
            "synonyms",
            "functions",
            "procedures",
            "packages",
            "types",
            "triggers"
        ]
    );
    let in_folder = |name: &str| {
        folders
            .iter()
            .find(|f| f.name == name)
            .unwrap_or_else(|| panic!("folder {name}"))
            .path
            .clone()
    };
    let tables = s.list_children(&in_folder("tables")).await.unwrap();
    let parent = tables
        .iter()
        .find(|n| n.name == "IT_TREE_P")
        .expect("IT_TREE_P");
    assert!(parent.has_children && parent.kind == ObjectKind::Table);
    assert_eq!(
        parent.path,
        table_path("DBSTUDIO", "IT_TREE_P"),
        "path = parent path + own segment"
    );
    let subfolders = s.list_children(&parent.path).await.unwrap();
    assert_eq!(names(&subfolders), ["columns", "indexes", "triggers"]);
    let cols = s.list_children(&subfolders[0].path).await.unwrap();
    assert_eq!(names(&cols), ["ID", "NAME"]);
    assert_eq!(cols[0].detail.as_deref(), Some("NUMBER(10) (PK)"));
    assert_eq!(cols[1].detail.as_deref(), Some("VARCHAR2(50)"));
    let child = tables.iter().find(|n| n.name == "IT_TREE_C").unwrap();
    let child_folders = s.list_children(&child.path).await.unwrap();
    let idx = s.list_children(&child_folders[1].path).await.unwrap();
    assert!(
        names(&idx).contains(&"IT_TREE_C_IX".to_string()),
        "{:?}",
        names(&idx)
    );
    let trg = s.list_children(&child_folders[2].path).await.unwrap();
    assert_eq!(names(&trg), ["IT_TREE_TRG"]);
    assert!(
        trg[0]
            .detail
            .as_deref()
            .unwrap()
            .contains("BEFORE EACH ROW")
    );

    let views = s.list_children(&in_folder("views")).await.unwrap();
    assert!(names(&views).contains(&"IT_TREE_V".to_string()));
    let seqs = s.list_children(&in_folder("sequences")).await.unwrap();
    assert!(names(&seqs).contains(&"IT_TREE_S".to_string()));
    let syns = s.list_children(&in_folder("synonyms")).await.unwrap();
    assert_eq!(
        syns.iter()
            .find(|n| n.name == "IT_TREE_SYN")
            .unwrap()
            .detail
            .as_deref(),
        Some("-> DBSTUDIO.IT_TREE_P")
    );
    let procs = s.list_children(&in_folder("procedures")).await.unwrap();
    assert!(names(&procs).contains(&"IT_TREE_PROC".to_string()));
    let fns = s.list_children(&in_folder("functions")).await.unwrap();
    assert!(names(&fns).contains(&"IT_TREE_FN".to_string()));
    let pks = s.list_children(&in_folder("packages")).await.unwrap();
    let pk = pks.iter().find(|n| n.name == "IT_TREE_PK").unwrap();
    assert_eq!(pk.kind, ObjectKind::Package);
    assert!(pk.detail.is_none(), "valid package has no detail");
    let types = s.list_children(&in_folder("types")).await.unwrap();
    assert!(names(&types).contains(&"IT_TREE_TY".to_string()));
    let all_trg = s.list_children(&in_folder("triggers")).await.unwrap();
    assert!(names(&all_trg).contains(&"IT_TREE_TRG".to_string()));

    // Paths without folder segments are accepted as well.
    let direct = schema_path("DBSTUDIO").child(ObjectKind::Table, "IT_TREE_P");
    assert_eq!(
        names(&s.list_children(&direct).await.unwrap()),
        ["columns", "indexes", "triggers"]
    );
    // The default page implementation filters with a glob and pages with a cursor.
    let page = s
        .list_children_page(dbstudio_core::ChildrenPageRequest {
            parent: in_folder("tables"),
            cursor: None,
            limit: 1,
            pattern: Some("it_tree_*".into()),
        })
        .await
        .unwrap();
    assert_eq!(page.nodes.len(), 1);
    assert!(page.next_cursor.is_some());

    // Details.
    let d = s
        .table_details(&table_path("DBSTUDIO", "IT_TREE_C"))
        .await
        .unwrap();
    assert_eq!(d.kind, ObjectKind::Table);
    assert_eq!(
        d.columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        ["ID", "P_ID", "NOTE"]
    );
    assert!(d.columns[0].primary_key && !d.columns[0].nullable);
    assert_eq!(d.columns[0].value_kind, ValueKind::Int);
    assert_eq!(d.columns[2].type_name, "VARCHAR2(20)");
    assert_eq!(d.foreign_keys.len(), 1);
    assert_eq!(d.foreign_keys[0].columns, ["P_ID"]);
    assert_eq!(d.foreign_keys[0].referenced_columns, ["ID"]);
    assert_eq!(
        d.foreign_keys[0].referenced_table,
        table_path("DBSTUDIO", "IT_TREE_P")
    );
    assert!(d.indexes.iter().any(|i| i.primary && i.columns == ["ID"]));
    assert!(
        d.indexes
            .iter()
            .any(|i| i.name == "IT_TREE_C_IX" && !i.unique && i.columns == ["NOTE"])
    );
    let p = s
        .table_details(&table_path("DBSTUDIO", "IT_TREE_P"))
        .await
        .unwrap();
    assert_eq!(p.comment.as_deref(), Some("parent table"));
    assert_eq!(p.columns[1].comment.as_deref(), Some("the name"));
    let v = s
        .table_details(&schema_path("DBSTUDIO").child(ObjectKind::View, "IT_TREE_V"))
        .await
        .unwrap();
    assert_eq!(v.kind, ObjectKind::View);
    assert!(v.indexes.is_empty());
    assert!(matches!(
        s.table_details(&table_path("DBSTUDIO", "NO_SUCH_TABLE"))
            .await,
        Err(Error::NotFound(_))
    ));
});

it!(roles_folder_with_dictionary_access, {
    let s = system().await;
    let root = s
        .list_children(&dbstudio_core::ObjectPath::root())
        .await
        .unwrap();
    let roles = root
        .iter()
        .find(|n| n.name == "roles")
        .expect("roles folder for SYSTEM");
    assert_eq!(roles.kind, ObjectKind::Folder);
    let list = s.list_children(&roles.path).await.unwrap();
    assert!(
        names(&list).contains(&"DBA".to_string()),
        "{:?}",
        names(&list)
    );
    assert!(
        list.iter()
            .all(|n| n.kind == ObjectKind::Role && !n.has_children)
    );
    // `show_all_schemas` lists the Oracle-maintained schemas too.
    let mut cfg = config("SYSTEM");
    cfg.options.insert("show_all_schemas".into(), "true".into());
    let all = try_connect(&cfg, PASSWORD).await.unwrap();
    let schemas = all
        .list_children(&dbstudio_core::ObjectPath::root())
        .await
        .unwrap();
    assert!(names(&schemas).contains(&"SYS".to_string()));
});

it!(search_objects_orders_and_scopes, {
    let s = app().await;
    tree_fixture(&*s).await;
    let search = |q: &str, scope: dbstudio_core::ObjectPath, kinds: Vec<ObjectKind>, limit| {
        ObjectSearchRequest {
            query: q.into(),
            scope,
            kinds,
            limit,
        }
    };
    let found = s
        .search_objects(search(
            "it_tree_p",
            dbstudio_core::ObjectPath::root(),
            vec![],
            50,
        ))
        .await
        .unwrap();
    let list = names(&found);
    // Exact match first, then prefix matches by name.
    assert_eq!(list[0], "IT_TREE_P", "{list:?}");
    assert!(list.contains(&"IT_TREE_PK".to_string()) && list.contains(&"IT_TREE_PROC".to_string()));
    let pk = found.iter().find(|n| n.name == "IT_TREE_PK").unwrap();
    assert_eq!(pk.kind, ObjectKind::Package);
    assert_eq!(pk.detail.as_deref(), Some("DBSTUDIO"));
    assert_eq!(
        pk.path,
        schema_path("DBSTUDIO")
            .child(ObjectKind::Folder, "packages")
            .child(ObjectKind::Package, "IT_TREE_PK")
    );
    // `_` and `%` are literal.
    assert!(
        s.search_objects(search(
            "IT_TREE%",
            dbstudio_core::ObjectPath::root(),
            vec![],
            50
        ))
        .await
        .unwrap()
        .is_empty()
    );
    assert!(
        s.search_objects(search("  ", dbstudio_core::ObjectPath::root(), vec![], 50))
            .await
            .unwrap()
            .is_empty()
    );
    let only_views = s
        .search_objects(search(
            "it_tree",
            schema_path("DBSTUDIO"),
            vec![ObjectKind::View],
            50,
        ))
        .await
        .unwrap();
    assert_eq!(names(&only_views), ["IT_TREE_V"]);
    let limited = s
        .search_objects(search(
            "it_tree",
            dbstudio_core::ObjectPath::root(),
            vec![],
            3,
        ))
        .await
        .unwrap();
    assert_eq!(limited.len(), 3);
    // A table node is a table once (the materialized view container is not listed twice).
    let tables = s
        .search_objects(search(
            "it_tree_c",
            dbstudio_core::ObjectPath::root(),
            vec![ObjectKind::Table],
            50,
        ))
        .await
        .unwrap();
    assert_eq!(names(&tables), ["IT_TREE_C"]);
    // Another schema, explicitly: dictionary tables of SYSTEM are visible to SYSTEM.
    let sys = system().await;
    let in_sys = sys
        .search_objects(search(
            "it_tree_p",
            schema_path("DBSTUDIO"),
            vec![ObjectKind::Table],
            10,
        ))
        .await
        .unwrap();
    assert_eq!(names(&in_sys), ["IT_TREE_P"]);
});

// ------------------------------------------------------------------------------------------
// Scripts, PL/SQL, DBMS_OUTPUT
// ------------------------------------------------------------------------------------------

it!(script_statements_results_and_affected, {
    let s = app().await;
    drop_object(&*s, "TABLE", "IT_SCRIPT_T").await;
    let mut rec = Rec::default();
    let summary = s
        .execute(
            QueryRequest::new(
                "CREATE TABLE IT_SCRIPT_T (ID NUMBER(10) PRIMARY KEY, V VARCHAR2(20));\n\
                 INSERT INTO IT_SCRIPT_T VALUES (1, 'a');\n\
                 INSERT INTO IT_SCRIPT_T SELECT LEVEL + 1, 'x' FROM dual CONNECT BY LEVEL <= 4;\n\
                 SELECT ID, V FROM IT_SCRIPT_T ORDER BY ID;\n\
                 UPDATE IT_SCRIPT_T SET V = 'u' WHERE ID > 2;\n\
                 DELETE FROM IT_SCRIPT_T WHERE ID = 1;\n\
                 SELECT COUNT(*) AS N FROM IT_SCRIPT_T",
            ),
            &mut rec,
        )
        .await
        .unwrap();
    assert_eq!(summary.statements, 7);
    assert_eq!(rec.starts.len(), 7);
    assert_eq!(rec.starts[0].index, 0);
    assert_eq!(
        rec.starts[1].range.start, 62,
        "statement 2 starts after the first terminator"
    );
    let outcomes: Vec<_> = rec.ends.iter().map(|e| e.outcome.clone()).collect();
    assert_eq!(outcomes[0], EndOutcome::Done);
    assert_eq!(outcomes[1], EndOutcome::Affected { rows: 1 });
    assert_eq!(outcomes[2], EndOutcome::Affected { rows: 4 });
    assert_eq!(
        outcomes[3],
        EndOutcome::Rows {
            row_count: 5,
            truncated: false
        }
    );
    assert_eq!(outcomes[4], EndOutcome::Affected { rows: 3 });
    assert_eq!(outcomes[5], EndOutcome::Affected { rows: 1 });
    assert_eq!(
        outcomes[6],
        EndOutcome::Rows {
            row_count: 1,
            truncated: false
        }
    );
    // Statements are committed one by one: another session sees the data.
    let other = app().await;
    assert_eq!(
        scalar(&*other, "SELECT COUNT(*) FROM IT_SCRIPT_T").await,
        dec("4")
    );

    // A trailing semicolon, comments and blank lines.
    let r = exec(
        &*s,
        "-- first\nSELECT 1 AS A FROM dual;\n\n/* second */\nSELECT 2 AS B FROM dual;\n",
    )
    .await;
    assert_eq!(r.len(), 2);

    // max_rows truncates the last result and says so.
    let mut q = QueryRequest::new("SELECT LEVEL AS N FROM dual CONNECT BY LEVEL <= 100");
    q.max_rows = Some(10);
    q.batch_size = Some(4);
    let mut rec = Rec::default();
    s.execute(q, &mut rec).await.unwrap();
    assert_eq!(rec.rows.len(), 10);
    assert_eq!(rec.batches, vec![4, 4, 2]);
    assert_eq!(
        rec.ends[0].outcome,
        EndOutcome::Rows {
            row_count: 10,
            truncated: true
        }
    );
    let mut q = QueryRequest::new("SELECT LEVEL AS N FROM dual CONNECT BY LEVEL <= 10");
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
});

it!(script_errors_carry_char_positions, {
    let s = app().await;
    // The error is in the second statement, after a multi-byte character: the position is an
    // absolute char offset into the whole text.
    let text = "SELECT 'é€😀' AS A FROM dual;\nSELECT x FROM it_no_such_table_zz";
    let err = exec_err(&*s, text).await;
    match err {
        Error::Query { message, position } => {
            assert!(message.starts_with("ORA-00942"), "{message}");
            let chars: Vec<char> = text.chars().collect();
            let pos = position.expect("position") as usize;
            let from_there: String = chars[pos..].iter().collect();
            assert!(
                from_there.starts_with("it_no_such_table_zz"),
                "{from_there:?}"
            );
        }
        other => panic!("{other:?}"),
    }
    // Statements before the failing one still ran and were reported.
    let mut rec = Rec::default();
    let res = s
        .execute(
            QueryRequest::new("SELECT 1 AS A FROM dual; SELECT * FROM it_no_such_table_zz"),
            &mut rec,
        )
        .await;
    assert!(res.is_err());
    assert_eq!(rec.ends.len(), 1);
    let err = exec_err(&*s, "SELEKT 1 FROM dual").await;
    assert!(
        matches!(
            &err,
            Error::Query {
                position: Some(0),
                ..
            }
        ),
        "{err:?}"
    );
    let err = exec_err(&*s, "SELECT 1/0 FROM dual").await;
    assert!(
        matches!(&err, Error::Query { message, .. } if message.starts_with("ORA-01476")),
        "{err:?}"
    );
});

it!(plsql_blocks_procedures_and_dbms_output, {
    let s = app().await;
    drop_object(&*s, "PROCEDURE", "IT_PLS_HELLO").await;
    drop_object(&*s, "TABLE", "IT_PLS_T").await;
    let script = "\
SET SERVEROUTPUT ON
CREATE TABLE IT_PLS_T (ID NUMBER, V VARCHAR2(30));
CREATE OR REPLACE PROCEDURE IT_PLS_HELLO(p_name IN VARCHAR2) AS
BEGIN
  DBMS_OUTPUT.PUT_LINE('Hello, ' || p_name || '!');
  INSERT INTO IT_PLS_T VALUES (1, p_name);
  IF p_name IS NULL THEN
    DBMS_OUTPUT.PUT_LINE('no name');
  END IF;
END IT_PLS_HELLO;
/
BEGIN
  IT_PLS_HELLO('Oracle');
  FOR i IN 1..3 LOOP
    DBMS_OUTPUT.PUT_LINE('row ' || i);
    INSERT INTO IT_PLS_T VALUES (i + 1, 'loop');
  END LOOP;
END;
/
EXEC IT_PLS_HELLO('exec')
SELECT COUNT(*) AS N FROM IT_PLS_T;
";
    let mut rec = Rec::default();
    let summary = s
        .execute(QueryRequest::new(script), &mut rec)
        .await
        .unwrap();
    // SET, CREATE TABLE, CREATE PROCEDURE, BEGIN block, EXEC, SELECT.
    assert_eq!(summary.statements, 6, "{:?}", rec.ends);
    assert_eq!(rec.starts.len(), 6);
    assert!(
        rec.messages
            .iter()
            .any(|m| m.contains("SET SERVEROUTPUT") && m.contains("Ignored")),
        "{:?}",
        rec.messages
    );
    for expected in ["Hello, Oracle!", "row 1", "row 2", "row 3", "Hello, exec!"] {
        assert!(
            rec.messages.iter().any(|m| m == expected),
            "{expected}: {:?}",
            rec.messages
        );
    }
    assert_eq!(rec.rows, vec![vec![dec("5")]]);
    // PL/SQL blocks are not split at their inner semicolons, and the `/` is not part of a statement.
    assert_eq!(
        rec.starts[3].range.start as usize,
        script.find("BEGIN\n  IT_PLS_HELLO").unwrap()
    );

    // The output belongs to the statement that produced it.
    let results = exec(&*s, "BEGIN DBMS_OUTPUT.PUT_LINE('one'); END;\nBEGIN DBMS_OUTPUT.PUT_LINE('two'); DBMS_OUTPUT.PUT_LINE(''); DBMS_OUTPUT.PUT_LINE('three'); END;").await;
    assert_eq!(results[0].messages, vec!["one"]);
    assert_eq!(results[1].messages, vec!["two", "", "three"]);
    // A procedure with a compilation error reports the errors as messages.
    let results = exec(
        &*s,
        "CREATE OR REPLACE PROCEDURE IT_PLS_BAD AS\nBEGIN\n  undefined_call_zz();\nEND;\n/",
    )
    .await;
    assert!(
        results[0]
            .messages
            .iter()
            .any(|m| m.contains("compilation errors")),
        "{:?}",
        results[0].messages
    );
    assert!(
        results[0].messages.iter().any(|m| m.contains("PLS-00201")),
        "{:?}",
        results[0].messages
    );
    drop_object(&*s, "PROCEDURE", "IT_PLS_BAD").await;
    // A runtime error inside a block surfaces as a query error.
    let err = exec_err(&*s, "BEGIN RAISE_APPLICATION_ERROR(-20001, 'boom'); END;").await;
    assert!(
        matches!(&err, Error::Query { message, .. } if message.contains("ORA-20001") && message.contains("boom")),
        "{err:?}"
    );
    // Implicit results.
    let results = exec(&*s, "DECLARE c SYS_REFCURSOR; BEGIN OPEN c FOR SELECT 1 AS X FROM dual; DBMS_SQL.RETURN_RESULT(c); END;").await;
    assert_eq!(results.len(), 2, "{results:?}");
    assert!(
        matches!(&results[0].outcome, StatementOutcome::Rows(rs) if rs.rows == vec![vec![dec("1")]])
    );
    drop_object(&*s, "PROCEDURE", "IT_PLS_HELLO").await;
    drop_object(&*s, "TABLE", "IT_PLS_T").await;
});

it!(package_and_trigger_scripts_split_correctly, {
    let s = app().await;
    drop_object(&*s, "PACKAGE", "IT_PK2").await;
    drop_object(&*s, "TABLE", "IT_PK2_T").await;
    let script = "\
CREATE TABLE IT_PK2_T (ID NUMBER, N NUMBER);
CREATE OR REPLACE PACKAGE IT_PK2 AS
  FUNCTION twice(x NUMBER) RETURN NUMBER;
  PROCEDURE log(msg VARCHAR2);
END IT_PK2;
/
CREATE OR REPLACE PACKAGE BODY IT_PK2 AS
  FUNCTION twice(x NUMBER) RETURN NUMBER IS
  BEGIN
    RETURN CASE WHEN x IS NULL THEN 0 ELSE x * 2 END;
  END twice;
  PROCEDURE log(msg VARCHAR2) IS
  BEGIN
    DBMS_OUTPUT.PUT_LINE('log: ' || msg);
  END log;
END IT_PK2;
/
CREATE OR REPLACE TRIGGER IT_PK2_TRG BEFORE INSERT ON IT_PK2_T FOR EACH ROW
BEGIN
  :new.N := IT_PK2.twice(:new.ID);
END;
/
INSERT INTO IT_PK2_T (ID) VALUES (21);
BEGIN IT_PK2.log('done'); END;
SELECT N FROM IT_PK2_T;
";
    let mut rec = Rec::default();
    let summary = s
        .execute(QueryRequest::new(script), &mut rec)
        .await
        .unwrap();
    assert_eq!(summary.statements, 7, "{:?}", rec.ends);
    assert_eq!(rec.rows, vec![vec![dec("42")]]);
    assert!(
        rec.messages.contains(&"log: done".to_string()),
        "{:?}",
        rec.messages
    );
    drop_object(&*s, "PACKAGE", "IT_PK2").await;
    drop_object(&*s, "TABLE", "IT_PK2_T").await;
});

// ------------------------------------------------------------------------------------------
// Types
// ------------------------------------------------------------------------------------------

it!(value_mapping_of_boundary_types, {
    let s = app().await;
    drop_object(&*s, "TABLE", "IT_TYPES").await;
    exec(
        &*s,
        "CREATE TABLE IT_TYPES (\
           ID NUMBER(10) PRIMARY KEY, SMALL NUMBER(18,0), BIG NUMBER(38,0), DEC NUMBER(20,6), ANY_NUM NUMBER, \
           F FLOAT(126), BF BINARY_FLOAT, BD BINARY_DOUBLE, D DATE, TS TIMESTAMP(3), TS9 TIMESTAMP(9), \
           TSTZ TIMESTAMP(6) WITH TIME ZONE, TSLTZ TIMESTAMP(6) WITH LOCAL TIME ZONE, \
           IDS INTERVAL DAY(2) TO SECOND(6), IYM INTERVAL YEAR(2) TO MONTH, \
           C CHAR(5), V VARCHAR2(20 CHAR), NV NVARCHAR2(20), NC NCHAR(3), R RAW(8), CL CLOB, NCL NCLOB, BL BLOB, \
           LNG_FREE VARCHAR2(10))",
    )
    .await;
    exec(
        &*s,
        "INSERT INTO IT_TYPES (ID, SMALL, BIG, DEC, ANY_NUM, F, BF, BD, D, TS, TS9, TSTZ, TSLTZ, IDS, IYM, C, V, NV, NC, R, CL, NCL, BL) VALUES (\
           1, 123456789012345678, 12345678901234567890123456789012345678, 12345678901234.123456, 0.5, \
           1.5, 1.5, 2.25, TO_DATE('2024-05-31 13:45:07', 'YYYY-MM-DD HH24:MI:SS'), \
           TO_TIMESTAMP('2024-05-31 13:45:07.123', 'YYYY-MM-DD HH24:MI:SS.FF'), \
           TO_TIMESTAMP('2024-05-31 13:45:07.123456789', 'YYYY-MM-DD HH24:MI:SS.FF'), \
           TO_TIMESTAMP_TZ('2024-05-31 13:45:07.5 +07:00', 'YYYY-MM-DD HH24:MI:SS.FF TZH:TZM'), \
           TO_TIMESTAMP_TZ('2024-05-31 13:45:07 +02:00', 'YYYY-MM-DD HH24:MI:SS TZH:TZM'), \
           INTERVAL '2 03:04:05.5' DAY TO SECOND, INTERVAL '1-02' YEAR TO MONTH, \
           'ab', 'Xin chào Việt Nam', N'Ünïcödé 日本', N'日本', HEXTORAW('DEADBEEF'), \
           'clob text', N'nclob text', UTL_RAW.CAST_TO_RAW('blob'))",
    )
    .await;
    exec(&*s, "INSERT INTO IT_TYPES (ID) VALUES (2)").await;

    let mut rec = Rec::default();
    s.execute(
        QueryRequest::new("SELECT * FROM IT_TYPES ORDER BY ID"),
        &mut rec,
    )
    .await
    .unwrap();
    let kinds = |c: &str| rec.cols[rec.col(c)].value_kind;
    assert_eq!(kinds("ID"), ValueKind::Int);
    assert_eq!(kinds("SMALL"), ValueKind::Int);
    assert_eq!(kinds("BIG"), ValueKind::Decimal);
    assert_eq!(kinds("DEC"), ValueKind::Decimal);
    assert_eq!(kinds("ANY_NUM"), ValueKind::Decimal);
    assert_eq!(kinds("BF"), ValueKind::Float);
    assert_eq!(kinds("BD"), ValueKind::Float);
    assert_eq!(kinds("D"), ValueKind::DateTime);
    assert_eq!(kinds("TSTZ"), ValueKind::Timestamp);
    assert_eq!(kinds("IDS"), ValueKind::Ext);
    assert_eq!(kinds("R"), ValueKind::Bytes);
    assert_eq!(kinds("BL"), ValueKind::Bytes);
    assert_eq!(rec.cols[rec.col("DEC")].type_name, "NUMBER(20,6)");
    assert_eq!(rec.cols[rec.col("ID")].nullable, Some(false));

    assert_eq!(rec.get(0, "ID"), &int(1));
    assert_eq!(rec.get(0, "SMALL"), &int(123_456_789_012_345_678));
    // 38 digits survive exactly.
    assert_eq!(
        rec.get(0, "BIG"),
        &dec("12345678901234567890123456789012345678")
    );
    assert_eq!(rec.get(0, "DEC"), &dec("12345678901234.123456"));
    assert_eq!(rec.get(0, "ANY_NUM"), &dec("0.5"));
    assert_eq!(rec.get(0, "F"), &dec("1.5"));
    assert_eq!(rec.get(0, "BF"), &Value::Float(1.5));
    assert_eq!(rec.get(0, "BD"), &Value::Float(2.25));
    assert_eq!(
        rec.get(0, "D"),
        &Value::DateTime("2024-05-31T13:45:07".into())
    );
    assert_eq!(
        rec.get(0, "TS"),
        &Value::DateTime("2024-05-31T13:45:07.123".into())
    );
    assert_eq!(
        rec.get(0, "TS9"),
        &Value::DateTime("2024-05-31T13:45:07.123456789".into())
    );
    assert_eq!(
        rec.get(0, "TSTZ"),
        &Value::Timestamp("2024-05-31T13:45:07.500000+07:00".into())
    );
    assert!(
        matches!(rec.get(0, "TSLTZ"), Value::Timestamp(t) if t.starts_with("2024-05-31T11:45:07") && (t.ends_with('Z') || t.ends_with("+00:00"))),
        "{:?}",
        rec.get(0, "TSLTZ")
    );
    assert!(
        matches!(rec.get(0, "IDS"), Value::Ext(e) if e.ty == "interval" && e.text.starts_with("+02 03:04:05.5")),
        "{:?}",
        rec.get(0, "IDS")
    );
    assert!(
        matches!(rec.get(0, "IYM"), Value::Ext(e) if e.ty == "interval" && e.text.contains("01-02")),
        "{:?}",
        rec.get(0, "IYM")
    );
    assert_eq!(rec.get(0, "C"), &text("ab   "));
    assert_eq!(rec.get(0, "V"), &text("Xin chào Việt Nam"));
    assert_eq!(rec.get(0, "NV"), &text("Ünïcödé 日本"));
    assert_eq!(rec.get(0, "NC"), &text("日本 "));
    assert_eq!(rec.get(0, "R"), &Value::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF]));
    assert_eq!(rec.get(0, "CL"), &text("clob text"));
    assert_eq!(rec.get(0, "NCL"), &text("nclob text"));
    assert_eq!(rec.get(0, "BL"), &Value::Bytes(b"blob".to_vec()));
    // NULLs, including the empty string.
    assert!(
        rec.rows[1][1..].iter().all(Value::is_null),
        "{:?}",
        rec.rows[1]
    );
    assert_eq!(scalar(&*s, "SELECT '' FROM dual").await, Value::Null);

    // Special floating point values.
    let r = rows(&*s, "SELECT BINARY_DOUBLE_NAN, BINARY_DOUBLE_INFINITY, -BINARY_DOUBLE_INFINITY, 1.5e300d FROM dual").await;
    assert!(matches!(r[0][0], Value::Float(f) if f.is_nan()));
    assert_eq!(r[0][1], Value::Float(f64::INFINITY));
    assert_eq!(r[0][2], Value::Float(f64::NEG_INFINITY));
    assert_eq!(r[0][3], Value::Float(1.5e300));
    // ROWID and UROWID.
    let r = rows(&*s, "SELECT ROWID FROM IT_TYPES WHERE ID = 1").await;
    assert!(
        matches!(&r[0][0], Value::Ext(e) if e.ty == "rowid" && e.text.len() == 18),
        "{:?}",
        r[0][0]
    );
    // Numbers: negative, small fractions and large exponents.
    let r = rows(&*s, "SELECT -0.000123, 1E-10, 1E30, 100 FROM dual").await;
    assert_eq!(r[0][0], dec("-0.000123"));
    assert!(
        matches!(&r[0][1], Value::Decimal(d) if d.parse::<f64>().unwrap() == 1e-10),
        "{:?}",
        r[0][1]
    );
    assert!(
        matches!(&r[0][2], Value::Decimal(d) if d.parse::<f64>().unwrap() == 1e30),
        "{:?}",
        r[0][2]
    );
    assert_eq!(r[0][3], dec("100"));
    // BOOLEAN (23ai).
    let r = rows(&*s, "SELECT TRUE AS T, FALSE AS F FROM dual").await;
    assert_eq!(r[0], vec![Value::Bool(true), Value::Bool(false)]);

    // `fetch_table` reads the same table (explicit column list not needed here).
    let rec = fetch(&*s, req(table_path("DBSTUDIO", "IT_TYPES"))).await;
    assert_eq!(rec.rows.len(), 2);
    assert_eq!(
        rec.get(0, "BIG"),
        &dec("12345678901234567890123456789012345678")
    );
    drop_object(&*s, "TABLE", "IT_TYPES").await;
});

it!(lobs_are_cut_at_the_limit, {
    let mut cfg = config("dbstudio");
    cfg.options.insert("max_lob_bytes".into(), "1000".into());
    let s = try_connect(&cfg, PASSWORD).await.unwrap();
    let r = rows(&*s, "SELECT TO_CLOB(RPAD('x', 500, 'x')) AS SHORT_CLOB, TO_CLOB(RPAD('y', 20000, 'y')) || TO_CLOB(RPAD('z', 20000, 'z')) AS LONG_CLOB, UTL_RAW.CAST_TO_RAW(RPAD('b', 2000, 'b')) AS RAW_VAL, TO_BLOB(UTL_RAW.CAST_TO_RAW(RPAD('c', 5000, 'c'))) AS LONG_BLOB FROM dual").await;
    assert_eq!(r[0][0], Value::Text("x".repeat(500)));
    match &r[0][1] {
        Value::Ext(e) => {
            assert_eq!(e.ty, "truncatedText");
            assert_eq!(e.text.len(), 1000);
            assert!(e.text.chars().all(|c| c == 'y'));
        }
        other => panic!("{other:?}"),
    }
    assert!(
        matches!(&r[0][2], Value::Ext(e) if e.ty == "truncatedBytes"),
        "{:?}",
        r[0][2]
    );
    assert!(
        matches!(&r[0][3], Value::Ext(e) if e.ty == "truncatedBytes"),
        "{:?}",
        r[0][3]
    );
});

// ------------------------------------------------------------------------------------------
// Table data
// ------------------------------------------------------------------------------------------

async fn paging_fixture(s: &dyn Session) {
    drop_object(s, "TABLE", "IT_PAGE").await;
    drop_object(s, "TABLE", "IT_PAGE_NOKEY").await;
    exec(
        s,
        "CREATE TABLE IT_PAGE (ID NUMBER(10) PRIMARY KEY, GRP NUMBER(2), NAME VARCHAR2(30), NOTE VARCHAR2(30));\n\
         INSERT INTO IT_PAGE SELECT LEVEL, MOD(LEVEL, 3), 'name ' || LEVEL, CASE WHEN MOD(LEVEL, 5) = 0 THEN NULL ELSE 'n%' || LEVEL END FROM dual CONNECT BY LEVEL <= 95;\n\
         CREATE TABLE IT_PAGE_NOKEY (GRP NUMBER(2), V VARCHAR2(10));\n\
         INSERT INTO IT_PAGE_NOKEY SELECT MOD(LEVEL, 2), 'same' FROM dual CONNECT BY LEVEL <= 30;\n\
         COMMIT",
    )
    .await;
}

it!(paging_is_deterministic_and_filters_work, {
    let s = app().await;
    paging_fixture(&*s).await;
    let table = table_path("DBSTUDIO", "IT_PAGE");
    // Ten pages of ten, no duplicate and no gap although there is no ORDER BY from the user.
    let mut seen = Vec::new();
    for page in 0..10u64 {
        let mut r = req(table.clone());
        r.offset = page * 10;
        r.limit = 10;
        let rec = fetch(&*s, r).await;
        assert_eq!(
            rec.rows.len(),
            if page == 9 { 5 } else { 10 },
            "page {page}"
        );
        assert_eq!(
            rec.ends[0].outcome,
            EndOutcome::Rows {
                row_count: rec.rows.len() as u64,
                truncated: false
            }
        );
        seen.extend(rec.rows.iter().map(|r| r[0].clone()));
    }
    assert_eq!(seen, (1..=95).map(int).collect::<Vec<_>>());

    // Sorting plus the key as tie break.
    let mut r = req(table.clone());
    r.sort = vec![SortKey {
        column: "GRP".into(),
        descending: true,
    }];
    r.limit = 5;
    let rec = fetch(&*s, r).await;
    let ids: Vec<Value> = rec.rows.iter().map(|r| r[0].clone()).collect();
    assert_eq!(ids, vec![int(2), int(5), int(8), int(11), int(14)]);

    // Filters.
    let f = |column: &str, op: FilterOp, values: &[&str]| ColumnFilter {
        column: column.into(),
        op,
        values: values.iter().map(|s| s.to_string()).collect(),
    };
    let count = |filters: Vec<ColumnFilter>, raw: Option<&str>| {
        let s = &s;
        let raw = raw.map(str::to_string);
        let table = table.clone();
        async move {
            s.count_table(TableCountRequest {
                table,
                filters,
                raw_where: raw,
                query_id: None,
            })
            .await
            .unwrap()
        }
    };
    assert_eq!(count(vec![], None).await, 95);
    assert_eq!(count(vec![f("GRP", FilterOp::Eq, &["1"])], None).await, 32);
    assert_eq!(count(vec![f("GRP", FilterOp::Ne, &["1"])], None).await, 63);
    assert_eq!(count(vec![f("ID", FilterOp::Gt, &["90"])], None).await, 5);
    assert_eq!(count(vec![f("ID", FilterOp::Le, &["3"])], None).await, 3);
    assert_eq!(
        count(vec![f("NAME", FilterOp::Contains, &["NAME 9"])], None).await,
        6,
        "case-insensitive"
    );
    assert_eq!(
        count(vec![f("NAME", FilterOp::StartsWith, &["name 1"])], None).await,
        11
    );
    assert_eq!(
        count(vec![f("NAME", FilterOp::EndsWith, &["5"])], None).await,
        10
    );
    assert_eq!(
        count(vec![f("NAME", FilterOp::NotContains, &["1"])], None).await,
        95 - 29
    );
    // `%` and `_` are literal.
    assert_eq!(
        count(vec![f("NOTE", FilterOp::Contains, &["n%"])], None).await,
        76
    );
    assert_eq!(
        count(vec![f("NOTE", FilterOp::Contains, &["%"])], None).await,
        76
    );
    assert_eq!(
        count(vec![f("NOTE", FilterOp::IsNull, &[])], None).await,
        19
    );
    assert_eq!(
        count(vec![f("NOTE", FilterOp::IsNotNull, &[])], None).await,
        76
    );
    assert_eq!(
        count(vec![f("NOTE", FilterOp::Eq, &[""])], None).await,
        19,
        "'' is NULL in Oracle"
    );
    assert_eq!(
        count(vec![f("ID", FilterOp::In, &["1", "2", "3", "500"])], None).await,
        3
    );
    assert_eq!(count(vec![f("ID", FilterOp::In, &[])], None).await, 0);
    assert_eq!(
        count(
            vec![
                f("GRP", FilterOp::Eq, &["0"]),
                f("ID", FilterOp::Gt, &["50"])
            ],
            None
        )
        .await,
        15
    );
    assert_eq!(count(vec![], Some("MOD(ID, 10) = 0 -- comment")).await, 9);
    // Values are escaped, never executed.
    assert_eq!(
        count(vec![f("NAME", FilterOp::Eq, &["x' OR '1'='1"])], None).await,
        0
    );
    // The raw clause rides the same page request.
    let mut r = req(table.clone());
    r.raw_where = Some("GRP = 2 AND ID < 12".into());
    let rec = fetch(&*s, r).await;
    assert_eq!(
        rec.rows.iter().map(|r| r[0].clone()).collect::<Vec<_>>(),
        vec![int(2), int(5), int(8), int(11)]
    );
    // A raw clause cannot smuggle a second statement.
    let mut r = req(table.clone());
    r.raw_where = Some("1 = 1; DROP TABLE IT_PAGE".into());
    let mut rec = Rec::default();
    assert!(s.fetch_table(r, &mut rec).await.is_err());
    assert_eq!(count(vec![], None).await, 95, "the table is still there");
    // Unknown column / table.
    let mut r = req(table.clone());
    r.sort = vec![SortKey {
        column: "NOPE".into(),
        descending: false,
    }];
    let mut rec = Rec::default();
    assert!(matches!(
        s.fetch_table(r, &mut rec).await,
        Err(Error::Query { .. })
    ));
    let mut rec = Rec::default();
    assert!(matches!(
        s.fetch_table(req(table_path("DBSTUDIO", "IT_NOPE")), &mut rec)
            .await,
        Err(Error::NotFound(_))
    ));
    // The batch size of the request applies.
    let mut r = req(table.clone());
    r.limit = 25;
    r.batch_size = Some(10);
    let rec = fetch(&*s, r).await;
    assert_eq!(rec.batches, vec![10, 10, 5]);

    // Tables without a key page deterministically by ROWID, and show it as a column.
    let nokey = table_path("DBSTUDIO", "IT_PAGE_NOKEY");
    let mut all = Vec::new();
    for page in 0..3u64 {
        let mut r = req(nokey.clone());
        r.offset = page * 10;
        r.limit = 10;
        let rec = fetch(&*s, r).await;
        assert_eq!(rec.cols.last().unwrap().name, "ROWID");
        all.extend(rec.rows.iter().map(|r| r.last().unwrap().clone()));
    }
    let unique: std::collections::HashSet<String> = all
        .iter()
        .map(|v| match v {
            Value::Ext(e) => e.text.clone(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(unique.len(), 30);
    drop_object(&*s, "TABLE", "IT_PAGE").await;
    drop_object(&*s, "TABLE", "IT_PAGE_NOKEY").await;
});

it!(json_and_special_columns_are_readable_through_fetch_table, {
    let s = app().await;
    drop_object(&*s, "TABLE", "IT_SPECIAL").await;
    exec(&*s, "CREATE TABLE IT_SPECIAL (ID NUMBER(5) PRIMARY KEY, DOC JSON, XML XMLTYPE, VEC VECTOR(3, FLOAT32))").await;
    exec(
        &*s,
        "INSERT INTO IT_SPECIAL VALUES (1, '{\"a\": [1, 2, {\"b\": null}]}', XMLTYPE('<r><c>1</c></r>'), '[1.5, 2, 3]')",
    )
    .await;
    let rec = fetch(&*s, req(table_path("DBSTUDIO", "IT_SPECIAL"))).await;
    assert_eq!(rec.rows.len(), 1);
    let json = match rec.get(0, "DOC") {
        Value::Text(t) | Value::Json(t) => t.clone(),
        other => panic!("{other:?}"),
    };
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed["a"][2]["b"], serde_json::Value::Null);
    assert!(
        matches!(rec.get(0, "XML"), Value::Text(t) if t.contains("<c>1</c>")),
        "{:?}",
        rec.get(0, "XML")
    );
    assert!(
        matches!(rec.get(0, "VEC"), Value::Text(t) if t.contains("1.5")),
        "{:?}",
        rec.get(0, "VEC")
    );
    drop_object(&*s, "TABLE", "IT_SPECIAL").await;
});

// ------------------------------------------------------------------------------------------
// Cancellation and timeouts
// ------------------------------------------------------------------------------------------

it!(cancel_and_timeout, {
    let s = app().await;
    // Cancel a running statement: it ends quickly, the session keeps working.
    let q = QueryRequest::new("BEGIN DBMS_SESSION.SLEEP(60); END;");
    let id = q.query_id;
    let mut rec = Rec::default();
    let started = Instant::now();
    let (res, cancelled) = tokio::join!(s.execute(q, &mut rec), async {
        tokio::time::sleep(Duration::from_millis(800)).await;
        s.cancel(id).await
    });
    cancelled.unwrap();
    assert!(matches!(res, Err(Error::Cancelled)), "{res:?}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(scalar(&*s, "SELECT 1 FROM dual").await, dec("1"));

    // Cancelling an unknown or finished id is a no-op.
    s.cancel(Uuid::new_v4()).await.unwrap();
    s.cancel(id).await.unwrap();

    // A long query that returns no row for a while.
    let q = QueryRequest::new(
        "SELECT COUNT(*) FROM all_objects a, all_objects b, all_objects c, all_objects d",
    );
    let id = q.query_id;
    let mut rec = Rec::default();
    let started = Instant::now();
    let res = cancel_until_done(&*s, id, s.execute(q, &mut rec)).await;
    assert!(matches!(res, Err(Error::Cancelled)), "{res:?}");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(scalar(&*s, "SELECT 2 FROM dual").await, dec("2"));

    // The timeout takes the same road and reports Timeout.
    let mut q = QueryRequest::new("BEGIN DBMS_SESSION.SLEEP(60); END;");
    q.timeout_ms = Some(1000);
    let mut rec = Rec::default();
    let started = Instant::now();
    let res = s.execute(q, &mut rec).await;
    assert!(matches!(res, Err(Error::Timeout)), "{res:?}");
    assert!(
        started.elapsed() < Duration::from_secs(6),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(scalar(&*s, "SELECT 3 FROM dual").await, dec("3"));

    // A slow count and a slow page fetch can be cancelled too.
    let big = table_path("DBSTUDIO", "IT_CANCEL_BIG");
    drop_object(&*s, "TABLE", "IT_CANCEL_BIG").await;
    exec(&*s, "CREATE TABLE IT_CANCEL_BIG (ID NUMBER); INSERT INTO IT_CANCEL_BIG SELECT LEVEL FROM dual CONNECT BY LEVEL <= 2000; COMMIT").await;
    let id = Uuid::new_v4();
    let count = TableCountRequest {
        table: big.clone(),
        filters: vec![],
        raw_where: Some(
            "(SELECT COUNT(*) FROM all_objects a, all_objects b, all_objects c) > ID".into(),
        ),
        query_id: Some(id),
    };
    let res = cancel_until_done(&*s, id, s.count_table(count)).await;
    assert!(matches!(res, Err(Error::Cancelled)), "{res:?}");
    let mut r = req(big);
    r.raw_where =
        Some("(SELECT COUNT(*) FROM all_objects a, all_objects b, all_objects c) > ID".into());
    let id = r.query_id;
    let mut rec = Rec::default();
    let res = cancel_until_done(&*s, id, s.fetch_table(r, &mut rec)).await;
    assert!(matches!(res, Err(Error::Cancelled)), "{res:?}");
    assert_eq!(scalar(&*s, "SELECT 4 FROM dual").await, dec("4"));
    drop_object(&*s, "TABLE", "IT_CANCEL_BIG").await;

    // A sink that stops the stream stops the query and leaves the connection usable.
    struct StopAfter(usize);
    impl dbstudio_core::ResultSink for StopAfter {
        fn on_columns(&mut self, _: Vec<dbstudio_core::ColumnMeta>) -> dbstudio_core::Result<()> {
            Ok(())
        }
        fn on_rows(&mut self, _: Vec<Vec<Value>>) -> dbstudio_core::Result<()> {
            self.0 += 1;
            Err(Error::Cancelled)
        }
        fn on_message(&mut self, _: String) -> dbstudio_core::Result<()> {
            Ok(())
        }
        fn on_statement_end(
            &mut self,
            _: dbstudio_core::StatementEnd,
        ) -> dbstudio_core::Result<()> {
            Ok(())
        }
    }
    let mut q = QueryRequest::new("SELECT LEVEL FROM dual CONNECT BY LEVEL <= 1000000");
    q.batch_size = Some(100);
    let mut sink = StopAfter(0);
    let res = s.execute(q, &mut sink).await;
    assert!(matches!(res, Err(Error::Cancelled)));
    assert_eq!(sink.0, 1);
    assert_eq!(scalar(&*s, "SELECT 5 FROM dual").await, dec("5"));
});

// ------------------------------------------------------------------------------------------
// Leases and transactions
// ------------------------------------------------------------------------------------------

it!(lease_manual_transactions, {
    let s = app().await;
    drop_object(&*s, "TABLE", "IT_LEASE_T").await;
    exec(
        &*s,
        "CREATE TABLE IT_LEASE_T (ID NUMBER PRIMARY KEY, V VARCHAR2(20))",
    )
    .await;
    let other = app().await;
    let count = |s: &dyn Session| {
        let _ = s;
    };
    let _ = count;

    let lease = s
        .open_lease(LeaseOptions {
            auto_commit: false,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(!lease.state().auto_commit);
    assert_eq!(lease.state().transaction, TxStatus::Idle);
    let mut rec = Rec::default();
    lease
        .execute(
            QueryRequest::new("INSERT INTO IT_LEASE_T VALUES (1, 'a')"),
            &mut rec,
        )
        .await
        .unwrap();
    assert_eq!(lease.state().transaction, TxStatus::Active);
    // Another connection does not see the uncommitted row; the lease does.
    assert_eq!(
        scalar(&*other, "SELECT COUNT(*) FROM IT_LEASE_T").await,
        dec("0")
    );
    let mut rec = Rec::default();
    lease
        .execute(
            QueryRequest::new("SELECT COUNT(*) AS N FROM IT_LEASE_T"),
            &mut rec,
        )
        .await
        .unwrap();
    assert_eq!(rec.rows, vec![vec![dec("1")]]);
    lease.rollback().await.unwrap();
    assert_eq!(lease.state().transaction, TxStatus::Idle);
    let mut rec = Rec::default();
    lease
        .execute(
            QueryRequest::new("SELECT COUNT(*) AS N FROM IT_LEASE_T"),
            &mut rec,
        )
        .await
        .unwrap();
    assert_eq!(rec.rows, vec![vec![dec("0")]]);

    let mut rec = Rec::default();
    lease
        .execute(
            QueryRequest::new("INSERT INTO IT_LEASE_T VALUES (2, 'b')"),
            &mut rec,
        )
        .await
        .unwrap();
    lease.commit().await.unwrap();
    assert_eq!(
        scalar(&*other, "SELECT COUNT(*) FROM IT_LEASE_T").await,
        dec("1")
    );
    assert_eq!(lease.state().transaction, TxStatus::Idle);

    // COMMIT typed in the editor ends the transaction as well.
    let mut rec = Rec::default();
    lease
        .execute(
            QueryRequest::new("INSERT INTO IT_LEASE_T VALUES (3, 'c'); COMMIT"),
            &mut rec,
        )
        .await
        .unwrap();
    assert_eq!(lease.state().transaction, TxStatus::Idle);
    assert_eq!(
        scalar(&*other, "SELECT COUNT(*) FROM IT_LEASE_T").await,
        dec("2")
    );

    // DDL commits implicitly.
    let mut rec = Rec::default();
    lease
        .execute(
            QueryRequest::new(
                "INSERT INTO IT_LEASE_T VALUES (4, 'd'); CREATE TABLE IT_LEASE_DDL (A NUMBER)",
            ),
            &mut rec,
        )
        .await
        .unwrap();
    assert_eq!(lease.state().transaction, TxStatus::Idle);
    assert_eq!(
        scalar(&*other, "SELECT COUNT(*) FROM IT_LEASE_T").await,
        dec("3")
    );
    drop_object(&*s, "TABLE", "IT_LEASE_DDL").await;

    // Closing with an open transaction rolls it back and says so.
    let mut rec = Rec::default();
    lease
        .execute(
            QueryRequest::new("INSERT INTO IT_LEASE_T VALUES (5, 'e')"),
            &mut rec,
        )
        .await
        .unwrap();
    assert_eq!(lease.state().transaction, TxStatus::Active);
    let report = lease.close().await.unwrap();
    assert!(report.rolled_back && !report.forced, "{report:?}");
    assert_eq!(
        scalar(&*other, "SELECT COUNT(*) FROM IT_LEASE_T").await,
        dec("3")
    );
    // Closing twice is fine; a closed lease refuses work.
    let again = lease.close().await.unwrap();
    assert!(!again.rolled_back);
    let mut rec = Rec::default();
    assert!(
        lease
            .execute(QueryRequest::new("SELECT 1 FROM dual"), &mut rec)
            .await
            .is_err()
    );
    drop_object(&*s, "TABLE", "IT_LEASE_T").await;
});

it!(lease_auto_commit_begin_and_context, {
    let s = app().await;
    drop_object(&*s, "TABLE", "IT_LEASE2_T").await;
    exec(&*s, "CREATE TABLE IT_LEASE2_T (ID NUMBER)").await;
    let other = app().await;
    let lease = s.open_lease(LeaseOptions::default()).await.unwrap();
    assert!(lease.state().auto_commit);
    let mut rec = Rec::default();
    lease
        .execute(
            QueryRequest::new("INSERT INTO IT_LEASE2_T VALUES (1)"),
            &mut rec,
        )
        .await
        .unwrap();
    assert_eq!(
        scalar(&*other, "SELECT COUNT(*) FROM IT_LEASE2_T").await,
        dec("1"),
        "auto commit"
    );
    assert_eq!(lease.state().transaction, TxStatus::Idle);

    // begin() suspends the commits until commit/rollback.
    lease.begin(TxOptions::default()).await.unwrap();
    assert_eq!(lease.state().transaction, TxStatus::Active);
    let mut rec = Rec::default();
    lease
        .execute(
            QueryRequest::new("INSERT INTO IT_LEASE2_T VALUES (2)"),
            &mut rec,
        )
        .await
        .unwrap();
    assert_eq!(
        scalar(&*other, "SELECT COUNT(*) FROM IT_LEASE2_T").await,
        dec("1")
    );
    // Switching to auto commit with an open transaction asks first.
    assert!(matches!(
        lease.set_auto_commit(true).await,
        Err(Error::Busy(_))
    ));
    lease.rollback().await.unwrap();
    assert_eq!(
        scalar(&*other, "SELECT COUNT(*) FROM IT_LEASE2_T").await,
        dec("1")
    );
    lease.set_auto_commit(false).await.unwrap();
    assert!(!lease.state().auto_commit);
    lease.set_auto_commit(true).await.unwrap();

    // Read only transaction.
    lease
        .begin(TxOptions {
            read_only: true,
            isolation: None,
        })
        .await
        .unwrap();
    let mut rec = Rec::default();
    let err = lease
        .execute(
            QueryRequest::new("INSERT INTO IT_LEASE2_T VALUES (3)"),
            &mut rec,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Query { message, .. } if message.contains("ORA-01456")),
        "{err:?}"
    );
    lease.rollback().await.unwrap();

    // Serializable.
    lease
        .begin(TxOptions {
            read_only: false,
            isolation: Some(dbstudio_core::IsolationLevel::Serializable),
        })
        .await
        .unwrap();
    let mut rec = Rec::default();
    lease
        .execute(
            QueryRequest::new("SELECT COUNT(*) AS N FROM IT_LEASE2_T"),
            &mut rec,
        )
        .await
        .unwrap();
    exec(&*other, "INSERT INTO IT_LEASE2_T VALUES (9); COMMIT").await;
    let mut rec = Rec::default();
    lease
        .execute(
            QueryRequest::new("SELECT COUNT(*) AS N FROM IT_LEASE2_T"),
            &mut rec,
        )
        .await
        .unwrap();
    assert_eq!(rec.rows, vec![vec![dec("1")]], "snapshot isolation");
    lease.rollback().await.unwrap();

    // Context: the current schema.
    lease
        .set_context(LeaseContext {
            database: None,
            schema: Some("SYSTEM".into()),
        })
        .await
        .unwrap();
    assert_eq!(lease.state().schema.as_deref(), Some("SYSTEM"));
    let mut rec = Rec::default();
    lease
        .execute(
            QueryRequest::new("SELECT SYS_CONTEXT('USERENV','CURRENT_SCHEMA') AS S FROM dual"),
            &mut rec,
        )
        .await
        .unwrap();
    assert_eq!(rec.rows, vec![vec![text("SYSTEM")]]);
    lease.close().await.unwrap();
    // A lease can start in another schema.
    let l2 = s
        .open_lease(LeaseOptions {
            schema: Some("SYSTEM".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    let mut rec = Rec::default();
    l2.execute(
        QueryRequest::new("SELECT SYS_CONTEXT('USERENV','CURRENT_SCHEMA') AS S FROM dual"),
        &mut rec,
    )
    .await
    .unwrap();
    assert_eq!(rec.rows, vec![vec![text("SYSTEM")]]);
    l2.close().await.unwrap();
    drop_object(&*s, "TABLE", "IT_LEASE2_T").await;
});

it!(lease_busy_cancel_and_fetch_more, {
    let s = app().await;
    let lease = std::sync::Arc::new(s.open_lease(LeaseOptions::default()).await.unwrap());
    // Cancel only hits the statement it names.
    let q = QueryRequest::new("BEGIN DBMS_SESSION.SLEEP(60); END;");
    let id = q.query_id;
    let started = Instant::now();
    let l = lease.clone();
    let runner = tokio::spawn(async move {
        let mut rec = Rec::default();
        l.execute(q, &mut rec).await
    });
    // Wait for it to run.
    for _ in 0..100 {
        if lease.state().running == Some(id) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(lease.state().running, Some(id));
    // A second statement is refused while the first runs.
    let mut rec = Rec::default();
    assert!(matches!(
        lease
            .execute(QueryRequest::new("SELECT 1 FROM dual"), &mut rec)
            .await,
        Err(Error::Busy(_))
    ));
    lease.cancel(Uuid::new_v4()).await.unwrap();
    assert_eq!(
        lease.state().running,
        Some(id),
        "a late/foreign cancel does nothing"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    lease.cancel(id).await.unwrap();
    let res = runner.await.unwrap();
    assert!(matches!(res, Err(Error::Cancelled)), "{res:?}");
    assert!(started.elapsed() < Duration::from_secs(8));
    assert_eq!(lease.state().running, None);

    // Timeout.
    let mut q = QueryRequest::new("BEGIN DBMS_SESSION.SLEEP(60); END;");
    q.timeout_ms = Some(800);
    let mut rec = Rec::default();
    assert!(matches!(
        lease.execute(q, &mut rec).await,
        Err(Error::Timeout)
    ));

    // fetch_more: the last result stays open when it hits max_rows.
    let mut q = QueryRequest::new("SELECT LEVEL AS N FROM dual CONNECT BY LEVEL <= 25");
    q.max_rows = Some(10);
    let qid = q.query_id;
    let mut rec = Rec::default();
    lease.execute(q, &mut rec).await.unwrap();
    assert_eq!(rec.rows.len(), 10);
    assert_eq!(
        rec.ends[0].outcome,
        EndOutcome::Rows {
            row_count: 10,
            truncated: true
        }
    );
    assert_eq!(lease.state().pending_result, Some(qid));
    let mut all = rec.rows.clone();
    for expected_truncated in [true, false] {
        let mut more = Rec::default();
        let before = if expected_truncated { 10 } else { 5 };
        lease.fetch_more(qid, 10, None, &mut more).await.unwrap();
        assert_eq!(more.rows.len(), before);
        assert!(more.cols.is_empty(), "fetch_more sends no columns");
        assert_eq!(
            more.ends[0].outcome,
            EndOutcome::Rows {
                row_count: before as u64,
                truncated: expected_truncated
            }
        );
        all.extend(more.rows);
        assert_eq!(lease.state().pending_result.is_some(), expected_truncated);
    }
    assert_eq!(
        all,
        (1..=25)
            .map(|i| vec![dec(&i.to_string())])
            .collect::<Vec<_>>()
    );
    assert!(matches!(
        lease.fetch_more(qid, 10, None, &mut Rec::default()).await,
        Err(Error::NotFound(_))
    ));
    // Wrong id.
    let mut q = QueryRequest::new("SELECT LEVEL AS N FROM dual CONNECT BY LEVEL <= 25");
    q.max_rows = Some(5);
    lease.execute(q, &mut Rec::default()).await.unwrap();
    assert!(matches!(
        lease
            .fetch_more(Uuid::new_v4(), 10, None, &mut Rec::default())
            .await,
        Err(Error::NotFound(_))
    ));
    // The next execute drops the open result; discard_result does too.
    lease
        .execute(QueryRequest::new("SELECT 1 FROM dual"), &mut Rec::default())
        .await
        .unwrap();
    assert_eq!(lease.state().pending_result, None);
    let mut q = QueryRequest::new("SELECT LEVEL AS N FROM dual CONNECT BY LEVEL <= 25");
    q.max_rows = Some(5);
    lease.execute(q, &mut Rec::default()).await.unwrap();
    assert!(lease.state().pending_result.is_some());
    lease.discard_result().await.unwrap();
    assert_eq!(lease.state().pending_result, None);
    lease.ping().await.unwrap();
    lease.close().await.unwrap();
});

it!(lease_error_in_manual_mode_keeps_the_transaction, {
    let s = app().await;
    drop_object(&*s, "TABLE", "IT_LEASE3_T").await;
    exec(&*s, "CREATE TABLE IT_LEASE3_T (ID NUMBER PRIMARY KEY)").await;
    let lease = s
        .open_lease(LeaseOptions {
            auto_commit: false,
            ..Default::default()
        })
        .await
        .unwrap();
    lease
        .execute(
            QueryRequest::new("INSERT INTO IT_LEASE3_T VALUES (1)"),
            &mut Rec::default(),
        )
        .await
        .unwrap();
    let err = lease
        .execute(
            QueryRequest::new("INSERT INTO IT_LEASE3_T VALUES (1)"),
            &mut Rec::default(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Query { message, .. } if message.contains("ORA-00001")),
        "{err:?}"
    );
    assert_eq!(
        lease.state().transaction,
        TxStatus::Active,
        "the first insert is still pending"
    );
    lease.commit().await.unwrap();
    assert_eq!(
        scalar(&*s, "SELECT COUNT(*) FROM IT_LEASE3_T").await,
        dec("1")
    );
    lease.close().await.unwrap();
    drop_object(&*s, "TABLE", "IT_LEASE3_T").await;
});

// ------------------------------------------------------------------------------------------
// Data editing
// ------------------------------------------------------------------------------------------

fn edit(column: &str, value: EditValue) -> CellEdit {
    CellEdit {
        column: column.into(),
        value,
    }
}

it!(editability_rules, {
    let s = app().await;
    for t in [
        "IT_ED_PK",
        "IT_ED_UQ",
        "IT_ED_NOKEY",
        "IT_ED_NULLUQ",
        "IT_ED_GEN",
    ] {
        drop_object(&*s, "TABLE", t).await;
    }
    drop_object(&*s, "VIEW", "IT_ED_V").await;
    exec(
        &*s,
        "CREATE TABLE IT_ED_PK (A NUMBER, B NUMBER, V VARCHAR2(10), PRIMARY KEY (A, B));\n\
         CREATE TABLE IT_ED_UQ (A NUMBER NOT NULL, V VARCHAR2(10), CONSTRAINT IT_ED_UQ_U UNIQUE (A));\n\
         CREATE TABLE IT_ED_NOKEY (A NUMBER, V VARCHAR2(10));\n\
         CREATE TABLE IT_ED_NULLUQ (A NUMBER, V VARCHAR2(10), CONSTRAINT IT_ED_NULLUQ_U UNIQUE (A));\n\
         CREATE TABLE IT_ED_GEN (ID NUMBER GENERATED ALWAYS AS IDENTITY PRIMARY KEY, N NUMBER, VIRT NUMBER GENERATED ALWAYS AS (N * 2) VIRTUAL);\n\
         CREATE VIEW IT_ED_V AS SELECT A, V FROM IT_ED_PK",
    )
    .await;
    let e = s
        .editability(&table_path("DBSTUDIO", "IT_ED_PK"))
        .await
        .unwrap();
    assert!(e.editable && e.atomic);
    assert_eq!(e.key_columns, ["A", "B"]);
    let e = s
        .editability(&table_path("DBSTUDIO", "IT_ED_UQ"))
        .await
        .unwrap();
    assert_eq!(e.key_columns, ["A"], "unique NOT NULL key");
    let e = s
        .editability(&table_path("DBSTUDIO", "IT_ED_NULLUQ"))
        .await
        .unwrap();
    assert_eq!(
        e.key_columns,
        ["ROWID"],
        "a nullable unique key is no key; ROWID is"
    );
    let e = s
        .editability(&table_path("DBSTUDIO", "IT_ED_NOKEY"))
        .await
        .unwrap();
    assert!(e.editable);
    assert_eq!(e.key_columns, ["ROWID"]);
    let e = s
        .editability(&table_path("DBSTUDIO", "IT_ED_GEN"))
        .await
        .unwrap();
    assert_eq!(
        e.read_only_columns,
        ["VIRT", "ID"]
            .map(String::from)
            .to_vec()
            .into_iter()
            .filter(|c| e.read_only_columns.contains(c))
            .collect::<Vec<_>>()
    );
    assert!(
        e.read_only_columns.contains(&"ID".to_string())
            && e.read_only_columns.contains(&"VIRT".to_string())
    );
    let e = s
        .editability(&schema_path("DBSTUDIO").child(ObjectKind::View, "IT_ED_V"))
        .await
        .unwrap();
    assert!(!e.editable);
    assert_eq!(e.reason, Some(NotEditableReason::NotATable));
    // Read only connection.
    let mut cfg = config("dbstudio");
    cfg.read_only = true;
    let ro = try_connect(&cfg, PASSWORD).await.unwrap();
    let e = ro
        .editability(&table_path("DBSTUDIO", "IT_ED_PK"))
        .await
        .unwrap();
    assert_eq!(e.reason, Some(NotEditableReason::ReadOnlyConnection));
    let err = ro
        .apply_changes(ChangeSet::Rows(RowChangeSet {
            table: table_path("DBSTUDIO", "IT_ED_PK"),
            key_columns: vec!["A".into(), "B".into()],
            changes: vec![RowChange::Delete {
                key: vec![int(1), int(1)],
            }],
        }))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), "readOnly");
    for t in [
        "IT_ED_PK",
        "IT_ED_UQ",
        "IT_ED_NOKEY",
        "IT_ED_NULLUQ",
        "IT_ED_GEN",
    ] {
        drop_object(&*s, "TABLE", t).await;
    }
    drop_object(&*s, "VIEW", "IT_ED_V").await;
});

it!(preview_and_apply_changes, {
    let s = app().await;
    drop_object(&*s, "TABLE", "IT_APPLY").await;
    exec(
        &*s,
        "CREATE TABLE IT_APPLY (ID NUMBER(10) PRIMARY KEY, NAME VARCHAR2(20), SALARY NUMBER(10,2), HIRED DATE, SEEN TIMESTAMP(3), TZ TIMESTAMP WITH TIME ZONE, DATA RAW(10), NOTE VARCHAR2(10) DEFAULT 'dflt');\n\
         INSERT INTO IT_APPLY (ID, NAME, SALARY) SELECT LEVEL, 'n' || LEVEL, LEVEL * 10 FROM dual CONNECT BY LEVEL <= 5;\n\
         COMMIT",
    )
    .await;
    let table = table_path("DBSTUDIO", "IT_APPLY");
    let set = RowChangeSet {
        table: table.clone(),
        key_columns: vec!["ID".into()],
        changes: vec![
            RowChange::Update {
                key: vec![int(1)],
                set: vec![
                    edit("NAME", EditValue::Text("O'Neil".into())),
                    edit("SALARY", EditValue::Value(dec("1234.50"))),
                    edit("HIRED", EditValue::Value(Value::Date("2024-05-31".into()))),
                    edit(
                        "SEEN",
                        EditValue::Value(Value::DateTime("2024-05-31T13:45:07.123".into())),
                    ),
                    edit(
                        "TZ",
                        EditValue::Value(Value::Timestamp("2024-05-31T13:45:07+07:00".into())),
                    ),
                    edit("DATA", EditValue::Value(Value::Bytes(vec![1, 2, 3]))),
                ],
            },
            RowChange::Update {
                key: vec![int(2)],
                set: vec![
                    edit("NAME", EditValue::Value(Value::Null)),
                    edit("NOTE", EditValue::Default),
                ],
            },
            RowChange::Insert {
                values: vec![
                    edit("ID", EditValue::Value(int(10))),
                    edit("NAME", EditValue::Text("new".into())),
                    edit("NOTE", EditValue::Default),
                ],
            },
            RowChange::Delete { key: vec![int(5)] },
        ],
    };
    let cs = ChangeSet::Rows(set.clone());
    let preview = s.preview_changes(&cs).await.unwrap();
    assert_eq!(preview.len(), 4);
    assert!(
        preview[0].starts_with("UPDATE \"DBSTUDIO\".\"IT_APPLY\" SET \"NAME\" = 'O''Neil'"),
        "{}",
        preview[0]
    );
    assert!(
        preview[0].contains("TO_DATE('2024-05-31', 'YYYY-MM-DD')")
            && preview[0].contains("HEXTORAW('010203')"),
        "{}",
        preview[0]
    );
    assert!(
        preview[1].contains("\"NOTE\" = DEFAULT") && preview[1].contains("\"NAME\" = NULL"),
        "{}",
        preview[1]
    );
    assert!(preview[2].starts_with("INSERT INTO \"DBSTUDIO\".\"IT_APPLY\" (\"ID\", \"NAME\", \"NOTE\") VALUES (10, 'new', DEFAULT)"), "{}", preview[2]);
    assert_eq!(
        preview[3],
        "DELETE FROM \"DBSTUDIO\".\"IT_APPLY\" WHERE \"ID\" = 5"
    );
    // The preview changes nothing.
    assert_eq!(scalar(&*s, "SELECT COUNT(*) FROM IT_APPLY").await, dec("5"));

    let result = s.apply_changes(cs).await.unwrap();
    assert_eq!(result.applied, 4);
    assert_eq!(result.affected, vec![1, 1, 1, 1]);
    assert!(result.atomic);
    let r = rows(&*s, "SELECT NAME, SALARY, TO_CHAR(HIRED, 'YYYY-MM-DD'), TO_CHAR(SEEN, 'YYYY-MM-DD HH24:MI:SS.FF3'), TO_CHAR(TZ, 'YYYY-MM-DD HH24:MI:SS TZH:TZM'), RAWTOHEX(DATA) FROM IT_APPLY WHERE ID = 1").await;
    assert_eq!(r[0][0], text("O'Neil"));
    assert_eq!(r[0][1], dec("1234.5"));
    assert_eq!(r[0][2], text("2024-05-31"));
    assert_eq!(r[0][3], text("2024-05-31 13:45:07.123"));
    assert_eq!(r[0][4], text("2024-05-31 13:45:07 +07:00"));
    assert_eq!(r[0][5], text("010203"));
    let r = rows(&*s, "SELECT NAME, NOTE FROM IT_APPLY WHERE ID = 2").await;
    assert_eq!(r[0], vec![Value::Null, text("dflt")]);
    assert_eq!(scalar(&*s, "SELECT COUNT(*) FROM IT_APPLY").await, dec("5"));
    assert_eq!(
        scalar(&*s, "SELECT COUNT(*) FROM IT_APPLY WHERE ID = 5").await,
        dec("0")
    );
    // Committed: visible to another session.
    let other = app().await;
    assert_eq!(
        scalar(&*other, "SELECT COUNT(*) FROM IT_APPLY WHERE ID = 10").await,
        dec("1")
    );

    // The second change fails: nothing is applied, the error says which change.
    let bad = ChangeSet::Rows(RowChangeSet {
        table: table.clone(),
        key_columns: vec!["ID".into()],
        changes: vec![
            RowChange::Update {
                key: vec![int(3)],
                set: vec![edit("NAME", EditValue::Text("changed".into()))],
            },
            RowChange::Insert {
                values: vec![edit("ID", EditValue::Value(int(1)))],
            },
        ],
    });
    match s.apply_changes(bad).await.unwrap_err() {
        Error::ChangeFailed {
            index,
            message,
            applied,
        } => {
            assert_eq!(index, 1);
            assert_eq!(applied, 0);
            assert!(message.contains("ORA-00001"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        scalar(&*s, "SELECT NAME FROM IT_APPLY WHERE ID = 3").await,
        text("n3")
    );

    // An update that matches no row rolls everything back.
    let none = ChangeSet::Rows(RowChangeSet {
        table: table.clone(),
        key_columns: vec!["ID".into()],
        changes: vec![
            RowChange::Update {
                key: vec![int(3)],
                set: vec![edit("NAME", EditValue::Text("changed".into()))],
            },
            RowChange::Update {
                key: vec![int(999)],
                set: vec![edit("NAME", EditValue::Text("x".into()))],
            },
        ],
    });
    match s.apply_changes(none).await.unwrap_err() {
        Error::ChangeFailed { index, .. } => assert_eq!(index, 1),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        scalar(&*s, "SELECT NAME FROM IT_APPLY WHERE ID = 3").await,
        text("n3")
    );

    // ROWID keyed edits (table without a key).
    drop_object(&*s, "TABLE", "IT_APPLY_NOKEY").await;
    exec(&*s, "CREATE TABLE IT_APPLY_NOKEY (A NUMBER, V VARCHAR2(10)); INSERT INTO IT_APPLY_NOKEY VALUES (1, 'x'); INSERT INTO IT_APPLY_NOKEY VALUES (1, 'x'); COMMIT").await;
    let nokey = table_path("DBSTUDIO", "IT_APPLY_NOKEY");
    let e = s.editability(&nokey).await.unwrap();
    assert_eq!(e.key_columns, ["ROWID"]);
    let rec = fetch(&*s, req(nokey.clone())).await;
    let rowid = rec.get(0, "ROWID").clone();
    let r = s
        .apply_changes(ChangeSet::Rows(RowChangeSet {
            table: nokey.clone(),
            key_columns: vec!["ROWID".into()],
            changes: vec![RowChange::Update {
                key: vec![rowid],
                set: vec![edit("V", EditValue::Text("y".into()))],
            }],
        }))
        .await
        .unwrap();
    assert_eq!(r.affected, vec![1]);
    assert_eq!(
        scalar(&*s, "SELECT COUNT(*) FROM IT_APPLY_NOKEY WHERE V = 'y'").await,
        dec("1")
    );
    // A non-unique key touching two rows is refused.
    let err = s
        .apply_changes(ChangeSet::Rows(RowChangeSet {
            table: nokey,
            key_columns: vec!["A".into()],
            changes: vec![RowChange::Delete { key: vec![int(1)] }],
        }))
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::ChangeFailed { index: 0, .. }),
        "{err:?}"
    );
    assert_eq!(
        scalar(&*s, "SELECT COUNT(*) FROM IT_APPLY_NOKEY").await,
        dec("2")
    );
    // Not a row change set.
    assert!(matches!(
        s.preview_changes(&ChangeSet::Keys(dbstudio_core::KeyChangeSet {
            database: dbstudio_core::ObjectPath::root(),
            changes: vec![]
        }))
        .await,
        Err(Error::Unsupported(_))
    ));
    drop_object(&*s, "TABLE", "IT_APPLY").await;
    drop_object(&*s, "TABLE", "IT_APPLY_NOKEY").await;
});

// ------------------------------------------------------------------------------------------
// Explain
// ------------------------------------------------------------------------------------------

fn explain_req(text: &str, analyze: bool) -> ExplainRequest {
    ExplainRequest {
        query_id: Uuid::new_v4(),
        text: text.into(),
        database: None,
        analyze,
        options: Default::default(),
        timeout_ms: None,
    }
}

it!(explain_estimated_plan, {
    let s = app().await;
    drop_object(&*s, "TABLE", "IT_EXPL").await;
    exec(&*s, "CREATE TABLE IT_EXPL (ID NUMBER PRIMARY KEY, V VARCHAR2(10)); INSERT INTO IT_EXPL SELECT LEVEL, 'v' FROM dual CONNECT BY LEVEL <= 100; COMMIT").await;
    let plan = s
        .explain(explain_req("SELECT * FROM IT_EXPL WHERE V = 'v'", false))
        .await
        .unwrap();
    assert_eq!(plan.format, PlanFormat::OraclePlanRows);
    assert!(!plan.analyzed && !plan.rolled_back);
    let json: serde_json::Value = serde_json::from_str(&plan.text).unwrap();
    let lines = json.as_array().unwrap();
    assert!(lines.len() >= 2);
    assert_eq!(lines[0]["ID"], 0);
    assert_eq!(lines[0]["OPERATION"], "SELECT STATEMENT");
    assert!(
        lines
            .iter()
            .any(|l| l["OPERATION"] == "TABLE ACCESS" && l["OBJECT_NAME"] == "IT_EXPL"),
        "{json}"
    );
    assert!(
        lines[1]["PARENT_ID"] == 0 && lines[1]["CARDINALITY"].is_number(),
        "{json}"
    );
    assert!(
        plan.extra["xplan"].contains("Plan hash value"),
        "{}",
        plan.extra["xplan"]
    );
    assert!(!plan.server_version.is_empty() && plan.captured_at.ends_with('Z'));
    assert_eq!(plan.statement, "SELECT * FROM IT_EXPL WHERE V = 'v'");
    // A trailing terminator is accepted; several statements are not.
    s.explain(explain_req("SELECT 1 FROM dual;", false))
        .await
        .unwrap();
    assert!(
        s.explain(explain_req("SELECT 1 FROM dual; SELECT 2 FROM dual", false))
            .await
            .is_err()
    );
    // Errors have positions.
    let err = s
        .explain(explain_req("SELECT * FROM it_no_such_zz", false))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Query { .. }), "{err:?}");
    // The plan table stays empty afterwards.
    assert_eq!(
        scalar(&*s, "SELECT COUNT(*) FROM plan_table").await,
        dec("0")
    );
    // Explain on a lease keeps the lease's transaction intact.
    let lease = s
        .open_lease(LeaseOptions {
            auto_commit: false,
            ..Default::default()
        })
        .await
        .unwrap();
    lease
        .execute(
            QueryRequest::new("INSERT INTO IT_EXPL VALUES (1000, 'pending')"),
            &mut Rec::default(),
        )
        .await
        .unwrap();
    lease
        .explain(explain_req("SELECT * FROM IT_EXPL", false))
        .await
        .unwrap();
    assert_eq!(lease.state().transaction, TxStatus::Active);
    lease.rollback().await.unwrap();
    lease.close().await.unwrap();
    drop_object(&*s, "TABLE", "IT_EXPL").await;
});

it!(explain_analyze_needs_privileges_and_never_changes_data, {
    let s = app().await;
    drop_object(&*s, "TABLE", "IT_EXPL2").await;
    exec(&*s, "CREATE TABLE IT_EXPL2 (ID NUMBER PRIMARY KEY, V VARCHAR2(10)); INSERT INTO IT_EXPL2 SELECT LEVEL, 'v' FROM dual CONNECT BY LEVEL <= 100; COMMIT").await;
    // The application user cannot read V$SQL.
    let err = s
        .explain(explain_req("SELECT * FROM IT_EXPL2", true))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Permission(_)), "{err:?}");
    assert_eq!(
        scalar(&*s, "SELECT COUNT(*) FROM IT_EXPL2").await,
        dec("100")
    );

    let sys = system().await;
    let plan = sys
        .explain(explain_req(
            "SELECT * FROM DBSTUDIO.IT_EXPL2 WHERE ID > 90",
            true,
        ))
        .await
        .unwrap();
    assert!(plan.analyzed);
    assert!(!plan.rolled_back, "a SELECT has nothing to roll back");
    let json: serde_json::Value = serde_json::from_str(&plan.text).unwrap();
    let lines = json.as_array().unwrap();
    assert!(
        lines[0].get("LAST_OUTPUT_ROWS").is_some() && lines[0].get("LAST_ELAPSED_TIME").is_some(),
        "{json}"
    );
    assert!(lines.iter().any(|l| l["LAST_OUTPUT_ROWS"] == 10), "{json}");
    assert!(
        plan.extra["xplan"].contains("A-Rows"),
        "{}",
        plan.extra["xplan"]
    );
    assert!(plan.extra.contains_key("sqlId"));

    // A write is analysed inside a transaction that is rolled back.
    let plan = sys
        .explain(explain_req(
            "UPDATE DBSTUDIO.IT_EXPL2 SET V = 'changed' WHERE ID <= 50",
            true,
        ))
        .await
        .unwrap();
    assert!(plan.analyzed && plan.rolled_back);
    assert_eq!(
        scalar(&*s, "SELECT COUNT(*) FROM IT_EXPL2 WHERE V = 'changed'").await,
        dec("0")
    );
    let plan = sys
        .explain(explain_req("DELETE FROM DBSTUDIO.IT_EXPL2", true))
        .await
        .unwrap();
    assert!(plan.rolled_back);
    assert_eq!(
        scalar(&*s, "SELECT COUNT(*) FROM IT_EXPL2").await,
        dec("100")
    );
    // DDL and PL/SQL cannot be analysed.
    let err = sys
        .explain(explain_req("CREATE TABLE IT_EXPL_NO (A NUMBER)", true))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Unsupported(c) if c == "explain_analyze_ddl"),
        "{err:?}"
    );
    let err = sys
        .explain(explain_req("BEGIN NULL; END;", true))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Unsupported(_)), "{err:?}");
    // A read only connection refuses to analyse a write.
    let mut cfg = config("SYSTEM");
    cfg.read_only = true;
    let ro = try_connect(&cfg, PASSWORD).await.unwrap();
    let err = ro
        .explain(explain_req("DELETE FROM DBSTUDIO.IT_EXPL2", true))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), "readOnly");
    // The statement is analysed inside a lease transaction without touching pending work.
    let lease = sys
        .open_lease(LeaseOptions {
            auto_commit: false,
            ..Default::default()
        })
        .await
        .unwrap();
    lease
        .execute(
            QueryRequest::new("UPDATE DBSTUDIO.IT_EXPL2 SET V = 'mine' WHERE ID = 1"),
            &mut Rec::default(),
        )
        .await
        .unwrap();
    let plan = lease
        .explain(explain_req(
            "UPDATE DBSTUDIO.IT_EXPL2 SET V = 'theirs' WHERE ID = 2",
            true,
        ))
        .await
        .unwrap();
    assert!(plan.rolled_back);
    let mut rec = Rec::default();
    lease
        .execute(
            QueryRequest::new("SELECT V FROM DBSTUDIO.IT_EXPL2 WHERE ID IN (1, 2) ORDER BY ID"),
            &mut rec,
        )
        .await
        .unwrap();
    assert_eq!(rec.rows, vec![vec![text("mine")], vec![text("v")]]);
    lease.rollback().await.unwrap();
    lease.close().await.unwrap();
    drop_object(&*s, "TABLE", "IT_EXPL2").await;
});

// ------------------------------------------------------------------------------------------
// Schema model, DDL
// ------------------------------------------------------------------------------------------

const MODEL_SCRIPT: &str = "\
CREATE TABLE IT_M_CUSTOMER (ID NUMBER(10) PRIMARY KEY, NAME VARCHAR2(50) NOT NULL, EMAIL VARCHAR2(80) UNIQUE);
CREATE TABLE IT_M_ORDER (ID NUMBER(10) PRIMARY KEY, CUSTOMER_ID NUMBER(10) NOT NULL REFERENCES IT_M_CUSTOMER(ID) ON DELETE CASCADE, NOTE VARCHAR2(40));
CREATE TABLE IT_M_PROFILE (CUSTOMER_ID NUMBER(10) PRIMARY KEY REFERENCES IT_M_CUSTOMER(ID), BIO VARCHAR2(100));
CREATE TABLE IT_M_REF (ID NUMBER(10) PRIMARY KEY, CUSTOMER_ID NUMBER(10), CONSTRAINT IT_M_REF_FK FOREIGN KEY (CUSTOMER_ID) REFERENCES IT_M_CUSTOMER(ID));
CREATE INDEX IT_M_ORDER_NOTE ON IT_M_ORDER(NOTE);
COMMENT ON TABLE IT_M_ORDER IS 'orders';
COMMENT ON COLUMN IT_M_ORDER.NOTE IS 'free text';
CREATE VIEW IT_M_V AS SELECT ID, NAME FROM IT_M_CUSTOMER;
";

async fn model_fixture(s: &dyn Session) {
    drop_object(s, "VIEW", "IT_M_V").await;
    for t in ["IT_M_REF", "IT_M_PROFILE", "IT_M_ORDER", "IT_M_CUSTOMER"] {
        drop_object(s, "TABLE", t).await;
    }
    exec(s, MODEL_SCRIPT).await;
}

it!(schema_model_has_tables_keys_and_relations, {
    let s = app().await;
    model_fixture(&*s).await;
    let scope = schema_path("DBSTUDIO");
    let model = s
        .schema_model(SchemaModelRequest {
            scope: scope.clone(),
            tables: vec![
                table_path("DBSTUDIO", "IT_M_CUSTOMER"),
                table_path("DBSTUDIO", "IT_M_ORDER"),
                table_path("DBSTUDIO", "IT_M_PROFILE"),
                table_path("DBSTUDIO", "IT_M_REF"),
            ],
            include_views: false,
            include_comments: true,
            detail: ModelDetail::Full,
            sample_size: None,
        })
        .await
        .unwrap();
    assert!(!model.inferred && !model.truncated);
    assert_eq!(model.tables.len(), 4);
    let customer = model
        .tables
        .iter()
        .find(|t| t.name == "IT_M_CUSTOMER")
        .unwrap();
    assert_eq!(customer.schema.as_deref(), Some("DBSTUDIO"));
    assert_eq!(customer.primary_key, ["ID"]);
    assert_eq!(customer.unique_keys, vec![vec!["EMAIL".to_string()]]);
    assert_eq!(customer.columns.len(), 3);
    assert!(customer.columns[0].primary_key);
    let order = model
        .tables
        .iter()
        .find(|t| t.name == "IT_M_ORDER")
        .unwrap();
    assert_eq!(order.comment.as_deref(), Some("orders"));
    assert_eq!(order.columns[2].comment.as_deref(), Some("free text"));
    assert_eq!(model.relations.len(), 3);
    let rel = |name_part: &str| {
        model
            .relations
            .iter()
            .find(|r| r.from_table.last().unwrap().name == name_part)
            .unwrap_or_else(|| panic!("relation from {name_part}"))
    };
    let r = rel("IT_M_ORDER");
    assert_eq!(r.from_columns, ["CUSTOMER_ID"]);
    assert_eq!(r.to_columns, ["ID"]);
    assert_eq!(r.to_table, table_path("DBSTUDIO", "IT_M_CUSTOMER"));
    assert_eq!(r.on_delete.as_deref(), Some("CASCADE"));
    assert!(!r.from_optional && !r.one_to_one);
    let r = rel("IT_M_PROFILE");
    assert!(r.one_to_one, "the FK is the primary key");
    assert!(!r.from_optional);
    let r = rel("IT_M_REF");
    assert!(r.from_optional && !r.one_to_one);
    assert_eq!(r.name, "IT_M_REF_FK");

    // Views on request; keys-only detail; comments off.
    let m2 = s
        .schema_model(SchemaModelRequest {
            scope: scope.clone(),
            tables: vec![
                table_path("DBSTUDIO", "IT_M_CUSTOMER"),
                schema_path("DBSTUDIO").child(ObjectKind::View, "IT_M_V"),
            ],
            include_views: true,
            include_comments: false,
            detail: ModelDetail::Keys,
            sample_size: None,
        })
        .await
        .unwrap();
    assert_eq!(m2.tables.len(), 2);
    assert!(
        m2.tables
            .iter()
            .any(|t| t.kind == ObjectKind::View && t.name == "IT_M_V")
    );
    let c = m2
        .tables
        .iter()
        .find(|t| t.name == "IT_M_CUSTOMER")
        .unwrap();
    assert_eq!(
        c.columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        ["ID", "EMAIL"]
    );
    assert!(
        m2.relations.is_empty(),
        "relations to tables outside the model are dropped"
    );
    // Whole schema without a table list.
    let all = s
        .schema_model(SchemaModelRequest {
            scope,
            tables: vec![],
            include_views: false,
            include_comments: false,
            detail: ModelDetail::Full,
            sample_size: None,
        })
        .await
        .unwrap();
    assert!(all.tables.len() >= 4);
    assert!(all.relations.len() >= 3);
    for t in ["IT_M_REF", "IT_M_PROFILE", "IT_M_ORDER", "IT_M_CUSTOMER"] {
        drop_object(&*s, "TABLE", t).await;
    }
    drop_object(&*s, "VIEW", "IT_M_V").await;
});

it!(object_ddl_of_every_kind, {
    let s = app().await;
    tree_fixture(&*s).await;
    let ddl = |kind: ObjectKind, name: &str| {
        let s = &s;
        let path = if kind == ObjectKind::Table {
            table_path("DBSTUDIO", name)
        } else {
            let folder = match kind {
                ObjectKind::View => "views",
                ObjectKind::Sequence => "sequences",
                ObjectKind::Synonym => "synonyms",
                ObjectKind::Function => "functions",
                ObjectKind::Procedure => "procedures",
                ObjectKind::Package => "packages",
                ObjectKind::Type => "types",
                ObjectKind::Trigger => "triggers",
                other => panic!("{other:?}"),
            };
            schema_path("DBSTUDIO")
                .child(ObjectKind::Folder, folder)
                .child(kind, name)
        };
        let name = name.to_string();
        async move {
            s.object_ddl(&path)
                .await
                .unwrap_or_else(|e| panic!("ddl of {name}: {e}"))
        }
    };
    let t = ddl(ObjectKind::Table, "IT_TREE_C").await;
    assert!(t.contains("CREATE TABLE \"DBSTUDIO\".\"IT_TREE_C\""), "{t}");
    assert!(t.contains("PRIMARY KEY"), "{t}");
    assert!(
        t.contains("CREATE INDEX \"DBSTUDIO\".\"IT_TREE_C_IX\""),
        "indexes are included: {t}"
    );
    assert!(t.contains("FOREIGN KEY"), "foreign keys are included: {t}");
    assert!(
        !t.contains("SEGMENT CREATION") && !t.contains("STORAGE("),
        "no physical attributes: {t}"
    );
    assert!(t.trim_end().ends_with(';'), "terminator: {t}");
    let p = ddl(ObjectKind::Table, "IT_TREE_P").await;
    assert!(
        p.contains("COMMENT ON TABLE") && p.contains("parent table"),
        "{p}"
    );
    assert!(p.contains("COMMENT ON COLUMN"), "{p}");
    assert!(
        ddl(ObjectKind::View, "IT_TREE_V")
            .await
            .contains("CREATE OR REPLACE")
    );
    assert!(
        ddl(ObjectKind::Sequence, "IT_TREE_S")
            .await
            .contains("CREATE SEQUENCE")
    );
    assert!(
        ddl(ObjectKind::Synonym, "IT_TREE_SYN")
            .await
            .contains("SYNONYM")
    );
    assert!(
        ddl(ObjectKind::Procedure, "IT_TREE_PROC")
            .await
            .contains("PROCEDURE")
    );
    assert!(
        ddl(ObjectKind::Function, "IT_TREE_FN")
            .await
            .contains("FUNCTION")
    );
    let pk = ddl(ObjectKind::Package, "IT_TREE_PK").await;
    assert!(
        pk.contains("PACKAGE \"DBSTUDIO\".\"IT_TREE_PK\"") && pk.contains("PACKAGE BODY"),
        "spec and body: {pk}"
    );
    assert!(ddl(ObjectKind::Type, "IT_TREE_TY").await.contains("OBJECT"));
    assert!(
        ddl(ObjectKind::Trigger, "IT_TREE_TRG")
            .await
            .contains("TRIGGER")
    );
    // An index under a table.
    let idx = table_path("DBSTUDIO", "IT_TREE_C")
        .child(ObjectKind::Folder, "indexes")
        .child(ObjectKind::Index, "IT_TREE_C_IX");
    assert!(s.object_ddl(&idx).await.unwrap().contains("CREATE INDEX"));
    // Missing objects and unsupported kinds.
    assert!(matches!(
        s.object_ddl(&table_path("DBSTUDIO", "IT_NOPE")).await,
        Err(Error::NotFound(_))
    ));
    let col = table_path("DBSTUDIO", "IT_TREE_C")
        .child(ObjectKind::Folder, "columns")
        .child(ObjectKind::Column, "ID");
    assert!(s.object_ddl(&col).await.is_err());
});

it!(generated_ddl_recreates_the_objects, {
    let s = app().await;
    model_fixture(&*s).await;
    let scope = schema_path("DBSTUDIO");
    let objects: Vec<_> = ["IT_M_CUSTOMER", "IT_M_ORDER", "IT_M_PROFILE", "IT_M_REF"]
        .iter()
        .map(|t| table_path("DBSTUDIO", t))
        .chain([scope
            .child(ObjectKind::Folder, "views")
            .child(ObjectKind::View, "IT_M_V")])
        .collect();
    let items = s
        .generate_ddl(DdlRequest {
            scope: scope.clone(),
            objects: objects.clone(),
            options: DdlOptions {
                include_drop: true,
                ..DdlOptions::default()
            },
        })
        .await
        .unwrap();
    // Pre-data first: tables before the view that reads them; foreign keys and indexes after.
    let first_post = items
        .iter()
        .position(|i| i.section == DdlSection::PostData)
        .expect("post data");
    assert!(
        items[..first_post]
            .iter()
            .all(|i| i.section == DdlSection::PreData)
    );
    assert!(
        items[first_post..]
            .iter()
            .all(|i| i.section == DdlSection::PostData)
    );
    let pos = |name: &str, kind: ObjectKind| {
        items
            .iter()
            .position(|i| {
                i.kind == kind
                    && i.path.last().unwrap().name == name
                    && i.section == DdlSection::PreData
            })
            .unwrap_or_else(|| panic!("{name}"))
    };
    assert!(pos("IT_M_CUSTOMER", ObjectKind::Table) < pos("IT_M_V", ObjectKind::View));
    assert!(
        items
            .iter()
            .any(|i| i.sql.contains("ALTER TABLE") && i.sql.contains("FOREIGN KEY")),
        "foreign keys in the post data section"
    );
    assert!(
        items.iter().any(|i| i.sql.contains("IT_M_ORDER_NOTE")),
        "index"
    );
    assert!(
        items.iter().any(|i| i.sql.contains("COMMENT ON")),
        "comments"
    );
    let view = &items[pos("IT_M_V", ObjectKind::View)];
    assert!(
        view.depends_on
            .contains(&table_path("DBSTUDIO", "IT_M_CUSTOMER")),
        "{:?}",
        view.depends_on
    );
    assert!(
        items
            .iter()
            .all(|i| i.language == dbstudio_core::DdlLanguage::Sql)
    );

    // Capture what exists, drop everything and run the generated script again.
    let before = s
        .schema_model(SchemaModelRequest {
            scope: scope.clone(),
            tables: objects[..4].to_vec(),
            include_views: false,
            include_comments: true,
            detail: ModelDetail::Full,
            sample_size: None,
        })
        .await
        .unwrap();
    drop_object(&*s, "VIEW", "IT_M_V").await;
    for t in ["IT_M_REF", "IT_M_PROFILE", "IT_M_ORDER", "IT_M_CUSTOMER"] {
        drop_object(&*s, "TABLE", t).await;
    }
    for item in &items {
        let mut rec = Rec::default();
        // The drops of objects that are gone fail with ORA-00942 / ORA-04043; ignore those.
        for stmt in dbstudio_split(&item.sql) {
            match s.execute(QueryRequest::new(stmt.clone()), &mut rec).await {
                Ok(_) => {}
                Err(Error::Query { message, .. })
                    if stmt.trim_start().to_uppercase().starts_with("DROP")
                        && (message.contains("ORA-00942") || message.contains("ORA-04043")) => {}
                Err(e) => panic!("running {stmt}: {e}"),
            }
        }
    }
    let after = s
        .schema_model(SchemaModelRequest {
            scope: scope.clone(),
            tables: objects[..4].to_vec(),
            include_views: false,
            include_comments: true,
            detail: ModelDetail::Full,
            sample_size: None,
        })
        .await
        .unwrap();
    assert_eq!(before.tables.len(), after.tables.len());
    for (b, a) in before.tables.iter().zip(&after.tables) {
        assert_eq!(b.name, a.name);
        assert_eq!(b.primary_key, a.primary_key);
        assert_eq!(b.unique_keys, a.unique_keys);
        assert_eq!(b.comment, a.comment);
        let cols = |t: &dbstudio_core::TableModel| {
            t.columns
                .iter()
                .map(|c| {
                    (
                        c.name.clone(),
                        c.type_name.clone(),
                        c.nullable,
                        c.comment.clone(),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(cols(b), cols(a), "{}", b.name);
    }
    assert_eq!(before.relations.len(), after.relations.len());
    for (b, a) in before.relations.iter().zip(&after.relations) {
        assert_eq!(
            (&b.from_columns, &b.to_columns, &b.on_delete),
            (&a.from_columns, &a.to_columns, &a.on_delete)
        );
    }
    // The view and the index exist again.
    assert_eq!(scalar(&*s, "SELECT COUNT(*) FROM IT_M_V").await, dec("0"));
    assert_eq!(
        scalar(
            &*s,
            "SELECT COUNT(*) FROM user_indexes WHERE index_name = 'IT_M_ORDER_NOTE'"
        )
        .await,
        dec("1")
    );

    // Whole-scope generation without an object list, and options.
    let all = s
        .generate_ddl(DdlRequest {
            scope: scope.clone(),
            objects: vec![],
            options: DdlOptions {
                include_views: false,
                include_indexes: false,
                include_foreign_keys: false,
                include_comments: false,
                qualify_schema: false,
                ..DdlOptions::default()
            },
        })
        .await
        .unwrap();
    assert!(
        all.iter()
            .any(|i| i.path.last().unwrap().name == "IT_M_CUSTOMER")
    );
    assert!(all.iter().all(|i| i.kind != ObjectKind::View));
    assert!(
        all.iter()
            .all(|i| i.section == DdlSection::PreData || i.kind == ObjectKind::Trigger),
        "no indexes / fks / comments requested"
    );
    let customer = all
        .iter()
        .find(|i| i.path.last().unwrap().name == "IT_M_CUSTOMER")
        .unwrap();
    assert!(
        !customer.sql.contains("\"DBSTUDIO\"."),
        "unqualified: {}",
        customer.sql
    );
    // Objects of another schema than the scope are refused.
    assert!(
        s.generate_ddl(DdlRequest {
            scope,
            objects: vec![table_path("SYSTEM", "X")],
            options: DdlOptions::default()
        })
        .await
        .is_err()
    );
    drop_object(&*s, "VIEW", "IT_M_V").await;
    for t in ["IT_M_REF", "IT_M_PROFILE", "IT_M_ORDER", "IT_M_CUSTOMER"] {
        drop_object(&*s, "TABLE", t).await;
    }
});

/// Split generated DDL the way a user script would be (the driver's own splitter runs inside
/// `execute`, so a whole item can simply be sent as one script).
fn dbstudio_split(sql: &str) -> Vec<String> {
    vec![sql.to_string()]
}

// ------------------------------------------------------------------------------------------
// Streaming, bulk, read only
// ------------------------------------------------------------------------------------------

it!(stream_table_and_bulk_insert_on_a_lease, {
    let s = app().await;
    for t in ["IT_BULK", "IT_BULK_DST"] {
        drop_object(&*s, "TABLE", t).await;
    }
    exec(
        &*s,
        "CREATE TABLE IT_BULK (ID NUMBER(10) PRIMARY KEY, NAME VARCHAR2(30) NOT NULL, D DATE, J JSON);\n\
         INSERT INTO IT_BULK SELECT LEVEL, 'row ' || LEVEL, DATE '2024-01-01' + LEVEL, '{\"n\": 1}' FROM dual CONNECT BY LEVEL <= 2500;\n\
         COMMIT;\n\
         CREATE TABLE IT_BULK_DST (ID NUMBER(10) PRIMARY KEY, NAME VARCHAR2(30), NOTE VARCHAR2(10) DEFAULT 'dflt')",
    )
    .await;
    let lease = s
        .open_lease(LeaseOptions {
            auto_commit: false,
            ..Default::default()
        })
        .await
        .unwrap();
    lease
        .begin(TxOptions {
            read_only: true,
            isolation: None,
        })
        .await
        .unwrap();
    // Whole table, key ordered, only some columns, in small batches.
    let mut rec = Rec::default();
    let summary = lease
        .stream_table(
            TableStreamRequest {
                query_id: Uuid::new_v4(),
                table: table_path("DBSTUDIO", "IT_BULK"),
                columns: vec!["ID".into(), "NAME".into()],
                filters: vec![],
                raw_where: Some("ID > 100".into()),
                order_by_key: true,
                batch_size: Some(500),
            },
            &mut rec,
        )
        .await
        .unwrap();
    assert_eq!(summary.statements, 1);
    assert_eq!(
        rec.cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        ["ID", "NAME"]
    );
    assert_eq!(rec.rows.len(), 2400);
    assert_eq!(rec.batches, vec![500, 500, 500, 500, 400]);
    assert_eq!(rec.rows[0], vec![int(101), text("row 101")]);
    assert_eq!(rec.rows[2399][0], int(2500));
    // All columns, JSON included.
    let mut rec = Rec::default();
    lease
        .stream_table(
            TableStreamRequest {
                query_id: Uuid::new_v4(),
                table: table_path("DBSTUDIO", "IT_BULK"),
                columns: vec![],
                filters: vec![ColumnFilter {
                    column: "ID".into(),
                    op: FilterOp::Le,
                    values: vec!["3".into()],
                }],
                raw_where: None,
                order_by_key: true,
                batch_size: None,
            },
            &mut rec,
        )
        .await
        .unwrap();
    assert_eq!(rec.rows.len(), 3);
    assert!(
        matches!(rec.get(0, "J"), Value::Text(t) | Value::Json(t) if t.contains("\"n\"")),
        "{:?}",
        rec.get(0, "J")
    );
    // The snapshot does not move while the lease reads.
    exec(
        &*s,
        "INSERT INTO IT_BULK VALUES (9999, 'late', NULL, NULL); COMMIT",
    )
    .await;
    let mut rec = Rec::default();
    lease
        .stream_table(
            TableStreamRequest {
                query_id: Uuid::new_v4(),
                table: table_path("DBSTUDIO", "IT_BULK"),
                columns: vec!["ID".into()],
                filters: vec![],
                raw_where: None,
                order_by_key: false,
                batch_size: None,
            },
            &mut rec,
        )
        .await
        .unwrap();
    assert_eq!(rec.rows.len(), 2500);
    lease.rollback().await.unwrap();
    // Unknown column.
    assert!(
        lease
            .stream_table(
                TableStreamRequest {
                    query_id: Uuid::new_v4(),
                    table: table_path("DBSTUDIO", "IT_BULK"),
                    columns: vec!["NOPE".into()],
                    filters: vec![],
                    raw_where: None,
                    order_by_key: false,
                    batch_size: None
                },
                &mut Rec::default(),
            )
            .await
            .is_err()
    );
    lease.close().await.unwrap();

    // Bulk insert: array DML in the lease's transaction.
    let lease = s
        .open_lease(LeaseOptions {
            auto_commit: false,
            ..Default::default()
        })
        .await
        .unwrap();
    let rows: Vec<Vec<EditValue>> = (1..=1500)
        .map(|i| {
            vec![
                EditValue::Text(i.to_string()),
                EditValue::Text(format!("n{i}")),
                if i % 2 == 0 {
                    EditValue::Default
                } else {
                    EditValue::Value(Value::Text("odd".into()))
                },
            ]
        })
        .collect();
    let result = lease
        .bulk_insert(BulkInsertRequest {
            table: table_path("DBSTUDIO", "IT_BULK_DST"),
            columns: vec!["ID".into(), "NAME".into(), "NOTE".into()],
            rows,
            on_conflict: OnConflict::Error,
            conflict_columns: vec![],
        })
        .await
        .unwrap();
    assert_eq!(result.inserted, 1500);
    assert_eq!(
        lease.state().transaction,
        TxStatus::Active,
        "nothing is committed by the bulk insert"
    );
    assert_eq!(
        scalar(&*s, "SELECT COUNT(*) FROM IT_BULK_DST").await,
        dec("0")
    );
    lease.commit().await.unwrap();
    assert_eq!(
        scalar(&*s, "SELECT COUNT(*) FROM IT_BULK_DST").await,
        dec("1500")
    );
    assert_eq!(
        scalar(&*s, "SELECT NOTE FROM IT_BULK_DST WHERE ID = 2").await,
        text("dflt")
    );
    // A duplicate key fails the whole batch.
    let err = lease
        .bulk_insert(BulkInsertRequest {
            table: table_path("DBSTUDIO", "IT_BULK_DST"),
            columns: vec!["ID".into(), "NAME".into()],
            rows: vec![
                vec![EditValue::Text("5000".into()), EditValue::Text("a".into())],
                vec![EditValue::Text("1".into()), EditValue::Text("dup".into())],
            ],
            on_conflict: OnConflict::Error,
            conflict_columns: vec![],
        })
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Query { message, .. } if message.contains("ORA-00001")),
        "{err:?}"
    );
    lease.rollback().await.unwrap();
    assert_eq!(
        scalar(&*s, "SELECT COUNT(*) FROM IT_BULK_DST WHERE ID = 5000").await,
        dec("0")
    );
    // Skip and update on conflict.
    let skipped = lease
        .bulk_insert(BulkInsertRequest {
            table: table_path("DBSTUDIO", "IT_BULK_DST"),
            columns: vec!["ID".into(), "NAME".into()],
            rows: vec![
                vec![
                    EditValue::Text("5001".into()),
                    EditValue::Text("new".into()),
                ],
                vec![EditValue::Text("1".into()), EditValue::Text("dup".into())],
            ],
            on_conflict: OnConflict::Skip,
            conflict_columns: vec!["ID".into()],
        })
        .await
        .unwrap();
    assert_eq!((skipped.inserted, skipped.skipped), (1, 1));
    let updated = lease
        .bulk_insert(BulkInsertRequest {
            table: table_path("DBSTUDIO", "IT_BULK_DST"),
            columns: vec!["ID".into(), "NAME".into()],
            rows: vec![
                vec![
                    EditValue::Text("1".into()),
                    EditValue::Text("changed".into()),
                ],
                vec![
                    EditValue::Text("6000".into()),
                    EditValue::Text("fresh".into()),
                ],
            ],
            on_conflict: OnConflict::Update,
            conflict_columns: vec!["ID".into()],
        })
        .await
        .unwrap();
    assert_eq!(updated.inserted + updated.updated, 2, "{updated:?}");
    lease.commit().await.unwrap();
    assert_eq!(
        scalar(&*s, "SELECT NAME FROM IT_BULK_DST WHERE ID = 1").await,
        text("changed")
    );
    assert_eq!(
        scalar(&*s, "SELECT NAME FROM IT_BULK_DST WHERE ID = 6000").await,
        text("fresh")
    );
    lease.close().await.unwrap();
    for t in ["IT_BULK", "IT_BULK_DST"] {
        drop_object(&*s, "TABLE", t).await;
    }
});

it!(read_only_connections_reject_writes, {
    let mut cfg = config("dbstudio");
    cfg.read_only = true;
    let s = try_connect(&cfg, PASSWORD).await.unwrap();
    assert_eq!(scalar(&*s, "SELECT 1 FROM dual").await, dec("1"));
    for sql in [
        "CREATE TABLE IT_RO (A NUMBER)",
        "INSERT INTO IT_RO VALUES (1)",
        "BEGIN NULL; END;",
        "DROP TABLE IT_RO",
    ] {
        let err = exec_err(&*s, sql).await;
        assert_eq!(err.kind(), "readOnly", "{sql}: {err:?}");
    }
    // The statements before the forbidden one still run, the forbidden one is never sent.
    let mut rec = Rec::default();
    let err = s
        .execute(
            QueryRequest::new("SELECT 1 AS A FROM dual; DELETE FROM it_nope"),
            &mut rec,
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), "readOnly");
    assert_eq!(rec.ends.len(), 1);
    let lease = s.open_lease(LeaseOptions::default()).await.unwrap();
    let err = lease
        .execute(
            QueryRequest::new("CREATE TABLE IT_RO (A NUMBER)"),
            &mut Rec::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), "readOnly");
    lease.close().await.unwrap();
    let results = exec(
        &*s,
        "SET TRANSACTION READ ONLY; SELECT 1 AS A FROM dual; COMMIT",
    )
    .await;
    assert_eq!(results.len(), 3);
});

it!(sessions_close_cleanly_and_report_closed, {
    let s = app().await;
    let lease = s.open_lease(LeaseOptions::default()).await.unwrap();
    s.close().await.unwrap();
    assert!(s.ping().await.is_err());
    // The lease owns its own connection and keeps working until it is closed.
    lease.ping().await.unwrap();
    lease.close().await.unwrap();
    // Many parallel calls share the pool of four connections.
    let s = std::sync::Arc::new(app().await);
    let mut tasks = Vec::new();
    for i in 0..12 {
        let s = s.clone();
        tasks.push(tokio::spawn(async move {
            let r = execute_collect(
                &**s,
                QueryRequest::new(format!("SELECT {i} AS N FROM dual")),
            )
            .await
            .unwrap();
            match &r[0].outcome {
                StatementOutcome::Rows(rs) => rs.rows[0][0].clone(),
                other => panic!("{other:?}"),
            }
        }));
    }
    let mut got = Vec::new();
    for t in tasks {
        got.push(t.await.unwrap());
    }
    assert_eq!(
        got,
        (0..12).map(|i| dec(&i.to_string())).collect::<Vec<_>>()
    );
});
