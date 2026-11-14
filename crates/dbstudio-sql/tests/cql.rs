//! CQL (Cassandra / ScyllaDB): lexer, splitter, classifier, parameters, redaction, literals.

use dbstudio_core::{DestructiveReason as D, ScriptDialect, StatementClass as C, TxControl, Value};
use dbstudio_sql::lexer::{TokenKind as K, tokenize, tokenize_with_problems};
use dbstudio_sql::redact::redact;
use dbstudio_sql::{Dialect, StreamSplitter, analyze, find_params, split, vocabulary};

const Q: ScriptDialect = ScriptDialect::Cql;

fn texts(sql: &str) -> Vec<String> {
    let chars: Vec<char> = sql.chars().collect();
    split(Q, sql)
        .iter()
        .map(|r| chars[r.start as usize..r.end as usize].iter().collect())
        .collect()
}

fn kinds(sql: &str) -> Vec<(K, String)> {
    let chars: Vec<char> = sql.chars().collect();
    tokenize(Q, sql)
        .into_iter()
        .filter(|t| t.kind != K::Whitespace)
        .map(|t| {
            (
                t.kind,
                chars[t.range.start as usize..t.range.end as usize]
                    .iter()
                    .collect(),
            )
        })
        .collect()
}

fn cls(sql: &str) -> (C, Option<D>) {
    let a = analyze(Q, sql);
    assert_eq!(a.statements.len(), 1, "{sql}");
    (a.statements[0].class, a.statements[0].destructive)
}

#[test]
fn classification_table() {
    let read = (C::Read, None);
    let write = (C::Write, None);
    let ddl = (C::Ddl, None);
    let ses = (C::Session, None);
    let adm = (C::Admin, None);
    let drop = |c| (c, Some(D::Drop));
    let rows: Vec<(&str, (C, Option<D>))> = vec![
        // reads
        ("SELECT * FROM ks.t", read),
        ("select a, b from t where id = 1", read),
        ("SELECT * FROM t LIMIT 10", read),
        ("SELECT JSON * FROM t", read),
        ("SELECT count(*) FROM t", read),
        ("SELECT * FROM t WHERE x = 1 ALLOW FILTERING", read),
        ("SELECT now() FROM system.local", read),
        ("SELECT * FROM \"Ks\".\"T\"", read),
        ("SELECT writetime(a), ttl(a) FROM t WHERE id = 1", read),
        ("SELECT * FROM t WHERE id IN (1, 2, 3)", read),
        ("SELECT * FROM t PER PARTITION LIMIT 1", read),
        ("SELECT token(id) FROM t", read),
        ("SELECT $$a;b$$ FROM t", read),
        ("SELECT * FROM t WHERE tags CONTAINS 'x'", read),
        ("SELECT * FROM t ORDER BY c DESC", read),
        ("SELECT DISTINCT id FROM t", read),
        ("-- note\nSELECT 1 FROM t", read),
        ("/* c */ SELECT 1 FROM t", read),
        ("// c\nSELECT 1 FROM t", read),
        ("DESCRIBE KEYSPACES", read),
        ("DESC TABLE ks.t", read),
        ("DESCRIBE CLUSTER", read),
        ("DESCRIBE KEYSPACE ks", read),
        ("LIST ROLES", read),
        ("LIST USERS", read),
        ("LIST ALL PERMISSIONS OF bob", read),
        // writes
        ("INSERT INTO t (a, b) VALUES (1, 'x')", write),
        ("INSERT INTO t (a) VALUES (1) IF NOT EXISTS", write),
        ("INSERT INTO t (a) VALUES (1) USING TTL 10", write),
        ("INSERT INTO t JSON '{\"a\": 1}'", write),
        ("UPDATE t SET a = 1 WHERE id = 1", write),
        ("UPDATE t USING TTL 5 SET a = 1 WHERE id = 1", write),
        ("UPDATE t SET a = 1 WHERE id = 1 IF EXISTS", write),
        ("UPDATE t SET a = a + 1 WHERE id = 1", write),
        ("DELETE FROM t WHERE id = 1", write),
        ("DELETE a FROM t WHERE id = 1", write),
        ("DELETE FROM t WHERE id = 1 IF EXISTS", write),
        (
            "BEGIN BATCH INSERT INTO t (a) VALUES (1); APPLY BATCH",
            write,
        ),
        (
            "BEGIN UNLOGGED BATCH INSERT INTO t (a) VALUES (1); INSERT INTO t (a) VALUES (2); APPLY BATCH",
            write,
        ),
        (
            "BEGIN COUNTER BATCH UPDATE c SET n = n + 1 WHERE id = 1; APPLY BATCH",
            write,
        ),
        (
            "begin batch using timestamp 1 insert into t (a) values (1); apply batch",
            write,
        ),
        (
            "BEGIN BATCH DELETE FROM t WHERE id = 1; INSERT INTO t (id) VALUES (2); APPLY BATCH",
            write,
        ),
        // DDL
        (
            "CREATE KEYSPACE ks WITH replication = {'class': 'SimpleStrategy', 'replication_factor': 1}",
            ddl,
        ),
        ("CREATE TABLE t (id int PRIMARY KEY, a text)", ddl),
        ("CREATE TABLE IF NOT EXISTS t (id int PRIMARY KEY)", ddl),
        ("CREATE INDEX ON t (a)", ddl),
        ("CREATE TYPE addr (street text, zip int)", ddl),
        (
            "CREATE MATERIALIZED VIEW v AS SELECT * FROM t WHERE a IS NOT NULL PRIMARY KEY (a, id)",
            ddl,
        ),
        (
            "CREATE FUNCTION f(a int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE java AS $$ return a; $$",
            ddl,
        ),
        ("ALTER TABLE t ADD c int", ddl),
        (
            "ALTER KEYSPACE ks WITH replication = {'class': 'SimpleStrategy', 'replication_factor': 2}",
            ddl,
        ),
        ("ALTER TYPE addr ADD city text", ddl),
        ("ALTER TABLE t WITH comment = 'x'", ddl),
        ("ALTER TABLE t RENAME a TO b", ddl),
        // destructive DDL
        ("DROP TABLE t", drop(C::Ddl)),
        ("DROP TABLE IF EXISTS ks.t", drop(C::Ddl)),
        ("DROP KEYSPACE ks", drop(C::Ddl)),
        ("DROP INDEX i", drop(C::Ddl)),
        ("DROP TYPE addr", drop(C::Ddl)),
        ("DROP MATERIALIZED VIEW v", drop(C::Ddl)),
        ("DROP FUNCTION f", drop(C::Ddl)),
        ("TRUNCATE t", (C::Ddl, Some(D::Truncate))),
        ("TRUNCATE TABLE ks.t", (C::Ddl, Some(D::Truncate))),
        ("ALTER TABLE t DROP c", (C::Ddl, Some(D::AlterDrop))),
        ("ALTER TABLE t DROP (a, b)", (C::Ddl, Some(D::AlterDrop))),
        // session
        ("USE ks", ses),
        ("USE \"Ks\"", ses),
        ("CONSISTENCY QUORUM", ses),
        ("CONSISTENCY", ses),
        ("SERIAL CONSISTENCY LOCAL_SERIAL", ses),
        ("TRACING ON", ses),
        ("TRACING OFF", ses),
        // admin
        ("GRANT SELECT ON ks.t TO bob", adm),
        ("REVOKE SELECT ON ks.t FROM bob", adm),
        ("CREATE ROLE r WITH PASSWORD = 'x' AND LOGIN = true", adm),
        ("ALTER ROLE r WITH LOGIN = false", adm),
        ("DROP ROLE r", drop(C::Admin)),
        ("CREATE USER u WITH PASSWORD 'p' NOSUPERUSER", adm),
        ("ALTER USER u WITH PASSWORD 'q'", adm),
        ("DROP USER u", drop(C::Admin)),
        ("GRANT ALL PERMISSIONS ON KEYSPACE ks TO r", adm),
        ("GRANT r2 TO r", adm),
    ];
    assert!(rows.len() >= 80, "{} cases", rows.len());
    for (sql, want) in rows {
        assert_eq!(cls(sql), want, "{sql}");
    }
}

#[test]
fn unknown_and_stray_verbs_keep_the_safety_net() {
    assert_eq!(cls("APPLY BATCH").0, C::Unknown);
    assert_eq!(cls("FOO bar").0, C::Unknown);
    assert_eq!(cls("FOO DROP TABLE t").1, Some(D::Drop));
    // a BEGIN that is not a batch is not a transaction in CQL
    let (c, _) = cls("BEGIN TRANSACTION");
    assert_ne!(
        c,
        C::Transaction {
            op: TxControl::Begin
        }
    );
}

#[test]
fn verb_and_targets() {
    let a = analyze(Q, "SELECT a FROM ks.t WHERE id = 1");
    assert_eq!(a.statements[0].verb, "SELECT");
    assert_eq!(a.statements[0].targets, vec!["ks.t"]);
    let a = analyze(Q, "DROP TABLE ks.t");
    assert_eq!(a.statements[0].verb, "DROP TABLE");
    let a = analyze(Q, "INSERT INTO \"Ks\".\"T\" (a) VALUES (1)");
    assert_eq!(a.statements[0].targets, vec!["Ks.T"]);
    assert!(!a.statements[0].implicit_commit);
}

#[test]
fn splitting() {
    assert_eq!(texts("SELECT 1; SELECT 2;"), ["SELECT 1", "SELECT 2"]);
    assert_eq!(texts("SELECT 1; SELECT 2"), ["SELECT 1", "SELECT 2"]);
    assert_eq!(texts("SELECT ';'; SELECT 2"), ["SELECT ';'", "SELECT 2"]);
    assert_eq!(texts("SELECT $$a;b$$; SELECT 2").len(), 2);
    assert_eq!(texts("SELECT \"a;b\" FROM t; SELECT 2").len(), 2);
    assert_eq!(texts("SELECT 1 -- ;\n; SELECT 2").len(), 2);
    assert_eq!(texts("SELECT 1 // ;\n; SELECT 2").len(), 2);
    assert_eq!(texts("SELECT 1 /* ; */; SELECT 2").len(), 2);
    assert_eq!(texts(";;; SELECT 1;;").len(), 1);
    assert!(texts("   ").is_empty());
    assert!(texts("-- only a comment").is_empty());
    assert_eq!(texts("SELECT 'it''s; ok'; SELECT 2").len(), 2);
    // a backslash does not escape in CQL strings
    assert_eq!(
        texts("SELECT 'a\\'; SELECT 2"),
        ["SELECT 'a\\'", "SELECT 2"]
    );
}

#[test]
fn cqlsh_commands_end_at_the_end_of_the_line() {
    for cmd in [
        "DESCRIBE TABLES",
        "CONSISTENCY ALL",
        "TRACING ON",
        "USE ks",
        "PAGING OFF",
        "SERIAL CONSISTENCY SERIAL",
        "desc keyspaces",
    ] {
        for nl in ["\n", "\r\n", "\r"] {
            let sql = format!("{cmd}{nl}DROP TABLE t;");
            assert_eq!(
                texts(&sql),
                [cmd.to_string(), "DROP TABLE t".to_string()],
                "{sql:?}"
            );
            // The guard sees both statements too.
            assert_eq!(analyze(Q, &sql).statements.len(), 2, "{sql:?}");
        }
    }
    assert_eq!(
        texts("CONSISTENCY ALL\nTRUNCATE t;"),
        ["CONSISTENCY ALL", "TRUNCATE t"]
    );
    // A `;` still ends them, and an ordinary statement spans lines.
    assert_eq!(texts("USE ks; SELECT 1"), ["USE ks", "SELECT 1"]);
    assert_eq!(texts("SELECT *\nFROM t;"), ["SELECT *\nFROM t"]);
    // Inside a batch nothing is cut.
    assert_eq!(texts("BEGIN BATCH\nUSE x\nAPPLY BATCH;").len(), 1);
}

#[test]
fn line_comments_end_at_a_carriage_return() {
    let sql = "// x\r/* \nDROP TABLE ks.t; */ SELECT 1";
    let st = analyze(Q, sql).statements;
    assert_eq!(texts(sql).len(), st.len());
    assert!(
        st.iter().all(|s| s.class != C::Ddl) || st.len() == 2,
        "{st:?}"
    );
}

#[test]
fn batch_is_one_statement() {
    let b = "BEGIN BATCH INSERT INTO t (a) VALUES (1); INSERT INTO t (a) VALUES (2); APPLY BATCH";
    assert_eq!(texts(&format!("{b};")), [b]);
    assert_eq!(texts(b), [b]);
    let two = format!("{b}; {b};");
    assert_eq!(texts(&two), [b, b]);
    assert_eq!(
        texts(&format!("SELECT 1; {b}; SELECT 2")),
        ["SELECT 1", b, "SELECT 2"]
    );
    let lower = b.to_lowercase();
    assert_eq!(texts(&lower), [lower.as_str()]);
    // unterminated batch swallows the rest
    assert_eq!(
        texts("BEGIN BATCH INSERT INTO t (a) VALUES (1); SELECT 1;").len(),
        1
    );
    // a string that mentions BEGIN BATCH is not a batch
    assert_eq!(texts("SELECT 'BEGIN BATCH'; SELECT 2").len(), 2);
    // a column called batch is not a batch
    assert_eq!(texts("SELECT batch FROM t; SELECT 2").len(), 2);
    assert_eq!(analyze(Q, &format!("{b}; SELECT 1")).statements.len(), 2);
}

#[test]
fn stream_splitter_matches_split() {
    let sql =
        "SELECT 1; BEGIN BATCH INSERT INTO t (a) VALUES (1); APPLY BATCH; SELECT 'x;y'; USE ks";
    let want = split(Q, sql).len();
    let mut s = StreamSplitter::new(Q);
    let mut got = 0;
    let mut seen = String::new();
    for ch in sql.chars() {
        seen.push(ch);
        got += s.push(&ch.to_string()).len();
    }
    got += s.finish().len();
    assert_eq!(got, want);
    assert_eq!(want, 4);
}

#[test]
fn lexer_tokens() {
    assert_eq!(kinds("'it''s'"), [(K::String, "'it''s'".into())]);
    assert_eq!(kinds("$$a'b$$"), [(K::String, "$$a'b$$".into())]);
    assert_eq!(kinds("\"a\"\"b\""), [(K::QuotedIdent, "\"a\"\"b\"".into())]);
    assert_eq!(kinds("-- c"), [(K::LineComment, "-- c".into())]);
    assert_eq!(kinds("// c"), [(K::LineComment, "// c".into())]);
    assert_eq!(kinds("/* c */"), [(K::BlockComment, "/* c */".into())]);
    assert_eq!(kinds("0xCAFE"), [(K::Number, "0xCAFE".into())]);
    assert_eq!(kinds("1.5e3"), [(K::Number, "1.5e3".into())]);
    // backslash is not an escape
    assert_eq!(kinds("'a\\'"), [(K::String, "'a\\'".into())]);
    // comments do not nest
    let k = kinds("/* /* */ x */");
    assert_eq!(k[0], (K::BlockComment, "/* /* */".into()));
    assert_eq!(k[1].1, "x");
    // `#` is not a comment in CQL
    assert_eq!(kinds("#")[0].0, K::Punct);
    // double quotes are identifiers, never strings
    assert_eq!(kinds("\"x\"")[0].0, K::QuotedIdent);
    // dollar signs elsewhere stay punctuation / word characters
    assert_eq!(kinds("$")[0].0, K::Punct);
    let tiles: usize = tokenize(Q, "SELECT 'a'; -- x\n$$b$$ \"c\"")
        .iter()
        .map(|t| (t.range.end - t.range.start) as usize)
        .sum();
    assert_eq!(tiles, "SELECT 'a'; -- x\n$$b$$ \"c\"".chars().count());
}

#[test]
fn lexer_problems_make_statements_unknown() {
    for bad in [
        "SELECT 'abc",
        "SELECT $$abc",
        "SELECT \"abc",
        "SELECT 1 /* x",
    ] {
        let (_, p) = tokenize_with_problems(Q, bad);
        assert_eq!(p.len(), 1, "{bad}");
        let a = analyze(Q, bad);
        assert!(!a.problems.is_empty());
        assert_eq!(a.statements[0].class, C::Unknown, "{bad}");
    }
    let a = analyze(Q, "DROP TABLE t; SELECT 'x");
    assert_eq!(a.statements[0].class, C::Unknown);
    assert_eq!(a.statements[0].destructive, Some(D::Drop));
}

#[test]
fn parameters() {
    let p = find_params(Q, "SELECT * FROM t WHERE a = :a AND b = :b AND c = :a");
    assert_eq!(p.len(), 2);
    assert_eq!(p[0].name, ":a");
    assert_eq!(p[0].ranges.len(), 2);
    let p = find_params(Q, "SELECT * FROM t WHERE a = ? AND b = ?");
    assert_eq!(p.len(), 2);
    assert_eq!(p[1].name, "?2");
    // literals, comments, strings and map / UDT literals are not parameters
    assert!(find_params(Q, "SELECT ':a', \"x:y\", $$:b$$ -- :c\n").is_empty());
    assert!(find_params(Q, "INSERT INTO t (m) VALUES ({'k':1, 'j':2})").is_empty());
    assert!(find_params(Q, "INSERT INTO t (u) VALUES ({street:'x', zip:1})").is_empty());
    let b = dbstudio_sql::bind(
        Q,
        "SELECT * FROM t WHERE a = :a AND b = ?",
        &[
            dbstudio_core::ParamValue {
                name: ":a".into(),
                value: dbstudio_core::ParamInput::Text("it's".into()),
            },
            dbstudio_core::ParamValue {
                name: "?1".into(),
                value: dbstudio_core::ParamInput::Number("5".into()),
            },
        ],
    )
    .unwrap();
    assert_eq!(b.text, "SELECT * FROM t WHERE a = 'it''s' AND b = 5");
}

#[test]
fn redaction() {
    let r = |s: &str| redact(Q, s);
    assert_eq!(
        r("CREATE ROLE r WITH PASSWORD = 'secret' AND LOGIN = true"),
        "CREATE ROLE r WITH PASSWORD = '***' AND LOGIN = true"
    );
    assert_eq!(
        r("CREATE USER u WITH PASSWORD 'secret' SUPERUSER"),
        "CREATE USER u WITH PASSWORD '***' SUPERUSER"
    );
    assert_eq!(
        r("ALTER ROLE r WITH PASSWORD = 'secret'"),
        "ALTER ROLE r WITH PASSWORD = '***'"
    );
    assert_eq!(
        r("alter user u with password 'p'"),
        "alter user u with password '***'"
    );
    assert_eq!(r("SELECT * FROM t"), "SELECT * FROM t");
    assert_eq!(r("SELECT 'password'"), "SELECT 'password'");
    // only the secret changes; statements around it stay
    assert_eq!(
        r("USE ks; CREATE ROLE r WITH PASSWORD = 'x'; SELECT 1"),
        "USE ks; CREATE ROLE r WITH PASSWORD = '***'; SELECT 1"
    );
    assert_eq!(
        r("CREATE ROLE r WITH HASHED PASSWORD = '$2a$10$abc'"),
        "CREATE ROLE r WITH HASHED PASSWORD = '***'"
    );
}

#[test]
fn quoting_and_literals() {
    let d = Dialect::new(Q);
    assert_eq!(d.quote_ident("a\"b"), "\"a\"\"b\"");
    assert_eq!(d.literal_text("it's"), "'it''s'");
    assert_eq!(d.literal_text("a\\b"), "'a\\b'");
    assert_eq!(d.literal(&Value::Null), "NULL");
    assert_eq!(d.literal(&Value::Bool(true)), "true");
    assert_eq!(d.literal(&Value::Int(-5)), "-5");
    assert_eq!(d.literal(&Value::Float(1.5)), "1.5");
    assert_eq!(d.literal(&Value::Float(f64::NAN)), "NaN");
    assert_eq!(d.literal(&Value::Float(f64::NEG_INFINITY)), "-Infinity");
    assert_eq!(d.literal(&Value::Decimal("12.50".into())), "12.50");
    assert_eq!(d.literal(&Value::Bytes(vec![0xde, 0xad])), "0xdead");
    assert_eq!(d.literal(&Value::Date("2024-05-31".into())), "'2024-05-31'");
    assert_eq!(d.literal(&Value::Time("13:45:00".into())), "'13:45:00'");
    let u = "123e4567-e89b-12d3-a456-426614174000";
    assert_eq!(d.literal(&Value::Uuid(u.into())), u);
    assert_eq!(d.literal_for_type(&Value::Text(u.into()), Some("uuid")), u);
    assert_eq!(
        d.literal_for_type(&Value::Text(u.into()), Some("text")),
        format!("'{u}'")
    );
    assert_eq!(
        d.literal(&Value::DateTime("2024-05-31T10:00:00".into())),
        "'2024-05-31T10:00:00Z'"
    );
    assert_eq!(
        d.literal(&Value::Timestamp("2024-05-31T10:00:00Z".into())),
        "'2024-05-31T10:00:00Z'"
    );
    assert_eq!(
        d.literal(&Value::DateTime("2024-05-31T10:00:00+02:00".into())),
        "'2024-05-31T10:00:00+02:00'"
    );
    let arr = Value::Array(vec![Value::Int(1), Value::Int(2)]);
    assert_eq!(d.literal_for_type(&arr, Some("list<int>")), "[1, 2]");
    assert_eq!(
        d.literal_for_type(&arr, Some("frozen<list<int>>")),
        "[1, 2]"
    );
    assert_eq!(d.literal_for_type(&arr, Some("set<int>")), "{1, 2}");
    assert_eq!(d.literal_for_type(&arr, Some("tuple<int, int>")), "(1, 2)");
    assert_eq!(d.literal_for_type(&arr, None), "[1, 2]");
    assert_eq!(d.literal(&Value::Array(vec![])), "[]");
    let strs = Value::Array(vec![Value::Text("a".into()), Value::Text("b'".into())]);
    assert_eq!(d.literal_for_type(&strs, Some("set<text>")), "{'a', 'b'''}");
    let tup = Value::Array(vec![Value::Text(u.into()), Value::Int(1)]);
    assert_eq!(
        d.literal_for_type(&tup, Some("tuple<uuid, int>")),
        format!("({u}, 1)")
    );
    let doc = Value::Document(vec![
        ("k".into(), Value::Int(1)),
        ("j".into(), Value::Int(2)),
    ]);
    assert_eq!(
        d.literal_for_type(&doc, Some("map<text, int>")),
        "{'k': 1, 'j': 2}"
    );
    assert_eq!(d.literal_for_type(&doc, None), "{'k': 1, 'j': 2}");
    assert_eq!(
        d.literal_for_type(&doc, Some("frozen<address>")),
        "{k: 1, j: 2}"
    );
    let odd = Value::Document(vec![("we ird".into(), Value::Int(1))]);
    assert_eq!(d.literal_for_type(&odd, Some("address")), "{\"we ird\": 1}");
    let nested = Value::Document(vec![("a".into(), Value::Array(vec![Value::Int(1)]))]);
    assert_eq!(
        d.literal_for_type(&nested, Some("map<text, set<int>>")),
        "{'a': {1}}"
    );
    assert_eq!(
        d.literal_for_type(&Value::Null, Some("map<text,int>")),
        "NULL"
    );
}

#[test]
fn literals_are_single_tokens_and_split_once() {
    for s in [
        "",
        "plain",
        "it's",
        "a\\b",
        "'; drop table x; --",
        "\\'",
        "é😀",
        "a\nb",
        "$$",
        "\"q\"",
    ] {
        let lit = Dialect::new(Q).literal_text(s);
        let toks: Vec<_> = tokenize(Q, &lit);
        assert_eq!(toks.len(), 1, "{s:?} -> {lit}");
        assert_eq!(toks[0].kind, K::String);
        assert_eq!(split(Q, &lit).len(), 1);
        let q = Dialect::new(Q).quote_ident(s);
        let toks = tokenize(Q, &q);
        assert_eq!(toks.len(), 1, "{s:?} -> {q}");
        assert_eq!(toks[0].kind, K::QuotedIdent);
    }
}

#[test]
fn column_types() {
    use dbstudio_core::ValueKind as V;
    let d = Dialect::new(Q);
    assert_eq!(d.type_for(V::Int), "bigint");
    assert_eq!(d.type_for(V::Bool), "boolean");
    assert_eq!(d.type_for(V::Bytes), "blob");
    assert_eq!(d.type_for(V::Uuid), "uuid");
    assert_eq!(d.type_for(V::Timestamp), "timestamp");
    assert_eq!(d.type_for(V::Text), "text");
    assert_eq!(d.type_for(V::Json), "text");
}

#[test]
fn vocabulary_has_cql_words() {
    let v = vocabulary(Q);
    for k in ["SELECT", "KEYSPACE", "CONSISTENCY", "APPLY", "BATCH", "TTL"] {
        assert!(v.keywords.iter().any(|x| x == k), "{k}");
    }
    for f in ["now", "uuid", "toTimestamp", "token", "writetime"] {
        assert!(v.functions.iter().any(|x| x == f), "{f}");
    }
    assert!(v.commands.is_empty());
    assert!(v.keywords.len() > 80 && v.functions.len() > 30);
    let mut up: Vec<String> = v.keywords.iter().map(|k| k.to_uppercase()).collect();
    let n = up.len();
    up.sort();
    up.dedup();
    assert_eq!(up.len(), n);
}

#[test]
fn use_statement_tracks_the_keyspace() {
    let sql = "SELECT 1 FROM a; USE ks2; SELECT 1 FROM b";
    let a = analyze(Q, sql);
    let dbs = dbstudio_sql::statement_databases(Q, sql, &a);
    assert_eq!(dbs, [None, Some("ks2".into()), Some("ks2".into())]);
}
