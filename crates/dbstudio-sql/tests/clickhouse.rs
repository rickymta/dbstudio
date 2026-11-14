//! ClickHouse: lexer, splitter, classifier, parameters, redaction, literals.

use dbstudio_core::{DestructiveReason as D, ScriptDialect, StatementClass as C, Value};
use dbstudio_sql::lexer::{TokenKind as K, tokenize, tokenize_with_problems};
use dbstudio_sql::redact::redact;
use dbstudio_sql::{Dialect, StreamSplitter, analyze, find_params, split, vocabulary};

const H: ScriptDialect = ScriptDialect::ClickHouse;

fn texts(sql: &str) -> Vec<String> {
    let chars: Vec<char> = sql.chars().collect();
    split(H, sql)
        .iter()
        .map(|r| chars[r.start as usize..r.end as usize].iter().collect())
        .collect()
}

fn kinds(sql: &str) -> Vec<(K, String)> {
    let chars: Vec<char> = sql.chars().collect();
    tokenize(H, sql)
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
    let a = analyze(H, sql);
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
    let explain = (
        C::Explain {
            analyze: false,
            writes: false,
        },
        None,
    );
    let rows: Vec<(&str, (C, Option<D>))> = vec![
        // reads
        ("SELECT 1", read),
        ("select * from db.t", read),
        (
            "SELECT a, count() FROM t GROUP BY a ORDER BY a LIMIT 10",
            read,
        ),
        ("SELECT * FROM t FINAL", read),
        ("SELECT * FROM t PREWHERE x = 1 WHERE y = 2", read),
        ("SELECT * FROM t FORMAT JSON", read),
        ("SELECT * FROM t SETTINGS max_threads = 2", read),
        ("SELECT * FROM t WHERE id = {id:UInt64}", read),
        ("SELECT `weird name` FROM `my-db`.`t`", read),
        ("SELECT * FROM numbers(10)", read),
        ("SELECT * FROM s3('https://x/y.csv', 'CSV')", adm),
        ("SELECT arrayJoin([1, 2, 3])", read),
        ("SELECT * FROM t ARRAY JOIN arr", read),
        ("SELECT * FROM t WHERE x IN (SELECT y FROM u)", read),
        ("WITH 1 AS a SELECT a", read),
        ("WITH x AS (SELECT 1) SELECT * FROM x", read),
        ("SELECT * FROM t SAMPLE 0.1", read),
        ("SELECT * FROM t LIMIT 5 BY a", read),
        ("# note\nSELECT 1", read),
        ("-- note\nSELECT 1", read),
        ("/* c */ SELECT 1", read),
        ("SHOW DATABASES", read),
        ("SHOW TABLES FROM db", read),
        ("SHOW CREATE TABLE t", read),
        ("SHOW PROCESSLIST", read),
        ("DESCRIBE TABLE t", read),
        ("DESC t", read),
        ("EXISTS TABLE t", read),
        ("EXISTS DATABASE d", read),
        ("EXISTS DICTIONARY d", read),
        ("EXPLAIN SELECT 1", explain),
        ("EXPLAIN AST SELECT 1", explain),
        ("EXPLAIN PIPELINE SELECT * FROM t", explain),
        ("EXPLAIN PLAN actions = 1 SELECT 1", explain),
        ("EXPLAIN SYNTAX SELECT 1", explain),
        ("EXPLAIN ESTIMATE SELECT * FROM t", explain),
        // writes
        ("INSERT INTO t VALUES (1, 'a')", write),
        ("INSERT INTO t (a, b) VALUES (1, 2), (3, 4)", write),
        ("INSERT INTO t SELECT * FROM u", write),
        ("INSERT INTO t FORMAT JSONEachRow {\"a\": 1}", write),
        ("INSERT INTO FUNCTION s3('u', 'CSV') SELECT 1", write),
        ("INSERT INTO t VALUES", write),
        ("DELETE FROM t WHERE id = 1", write),
        ("DELETE FROM t WHERE a = 1 AND b = 2", write),
        ("UPDATE t SET a = 1 WHERE id = 2", write),
        ("ALTER TABLE t UPDATE a = 1 WHERE id = 2", write),
        ("ALTER TABLE db.t DELETE WHERE id = 3", write),
        ("ALTER TABLE t ON CLUSTER c DELETE WHERE id = 3", write),
        ("ALTER TABLE t DELETE IN PARTITION 2020 WHERE id = 3", write),
        ("ALTER TABLE t UPDATE a = a + 1 WHERE b > 2", write),
        ("SELECT * FROM t INTO OUTFILE 'x.csv'", write),
        // DDL
        (
            "CREATE TABLE t (a Int32) ENGINE = MergeTree ORDER BY a",
            ddl,
        ),
        (
            "CREATE TABLE IF NOT EXISTS t (a Int32) ENGINE = Memory",
            ddl,
        ),
        ("CREATE DATABASE d", ddl),
        ("CREATE VIEW v AS SELECT 1", ddl),
        ("CREATE MATERIALIZED VIEW v TO t AS SELECT * FROM u", ddl),
        (
            "CREATE DICTIONARY d (id UInt64) PRIMARY KEY id SOURCE(NULL()) LAYOUT(FLAT()) LIFETIME(0)",
            ddl,
        ),
        ("CREATE FUNCTION f AS (x) -> x + 1", ddl),
        ("CREATE TABLE t AS SELECT 1", ddl),
        ("ALTER TABLE t ADD COLUMN c Int32", ddl),
        ("ALTER TABLE t MODIFY COLUMN c String", ddl),
        (
            "ALTER TABLE t MODIFY TTL d + INTERVAL 1 DAY DELETE",
            (C::Ddl, Some(D::AlterDrop)),
        ),
        (
            "ALTER TABLE t MODIFY TTL d + INTERVAL 1 DAY TO DISK 'cold'",
            ddl,
        ),
        ("ALTER TABLE t COMMENT COLUMN c 'x'", ddl),
        ("ALTER TABLE t MODIFY SETTING a = 1", ddl),
        ("ALTER TABLE t ADD INDEX i a TYPE minmax GRANULARITY 1", ddl),
        ("RENAME TABLE a TO b", ddl),
        ("RENAME DATABASE a TO b", ddl),
        ("EXCHANGE TABLES a AND b", ddl),
        ("OPTIMIZE TABLE t", ddl),
        (
            "OPTIMIZE TABLE t FINAL DEDUPLICATE",
            (C::Ddl, Some(D::DeleteWithoutWhere)),
        ),
        ("ATTACH TABLE t", ddl),
        ("ATTACH DATABASE d", ddl),
        ("UNDROP TABLE t", ddl),
        // destructive
        ("DROP TABLE t", (C::Ddl, Some(D::Drop))),
        ("DROP TABLE IF EXISTS d.t SYNC", (C::Ddl, Some(D::Drop))),
        ("DROP DATABASE d", (C::Ddl, Some(D::Drop))),
        ("DROP VIEW v", (C::Ddl, Some(D::Drop))),
        ("DROP DICTIONARY d", (C::Ddl, Some(D::Drop))),
        ("DROP FUNCTION f", (C::Ddl, Some(D::Drop))),
        ("DETACH TABLE t", (C::Ddl, Some(D::Drop))),
        ("DETACH TABLE t PERMANENTLY", (C::Ddl, Some(D::Drop))),
        ("DETACH DATABASE d", (C::Ddl, Some(D::Drop))),
        ("TRUNCATE TABLE t", (C::Ddl, Some(D::Truncate))),
        ("TRUNCATE t", (C::Ddl, Some(D::Truncate))),
        ("TRUNCATE ALL TABLES FROM d", (C::Ddl, Some(D::Truncate))),
        ("ALTER TABLE t DROP COLUMN c", (C::Ddl, Some(D::AlterDrop))),
        (
            "ALTER TABLE t DROP PARTITION 2020",
            (C::Ddl, Some(D::AlterDrop)),
        ),
        (
            "ALTER TABLE t DETACH PARTITION 2020",
            (C::Ddl, Some(D::AlterDrop)),
        ),
        ("ALTER TABLE t DROP INDEX i", (C::Ddl, Some(D::AlterDrop))),
        (
            "ALTER TABLE t DROP PROJECTION p",
            (C::Ddl, Some(D::AlterDrop)),
        ),
        (
            "ALTER TABLE t DELETE WHERE 1",
            (C::Write, Some(D::DeleteWithoutWhere)),
        ),
        (
            "ALTER TABLE t DELETE WHERE 1 = 1",
            (C::Write, Some(D::DeleteWithoutWhere)),
        ),
        (
            "ALTER TABLE t DELETE WHERE true",
            (C::Write, Some(D::DeleteWithoutWhere)),
        ),
        (
            "ALTER TABLE t UPDATE a = 1 WHERE 1",
            (C::Write, Some(D::UpdateWithoutWhere)),
        ),
        (
            "ALTER TABLE t UPDATE a = 1 WHERE (1)",
            (C::Write, Some(D::UpdateWithoutWhere)),
        ),
        (
            "DELETE FROM t WHERE 1",
            (C::Write, Some(D::DeleteWithoutWhere)),
        ),
        ("DELETE FROM t", (C::Write, Some(D::DeleteWithoutWhere))),
        (
            "UPDATE t SET a = 1 WHERE 1",
            (C::Write, Some(D::UpdateWithoutWhere)),
        ),
        // session
        ("USE db", ses),
        ("USE `my-db`", ses),
        ("SET max_threads = 4", ses),
        ("SET param_id = 5", ses),
        ("SET async_insert = 1", ses),
        // admin
        ("SYSTEM FLUSH LOGS", adm),
        ("SYSTEM RELOAD DICTIONARIES", adm),
        ("SYSTEM STOP MERGES", adm),
        ("SYSTEM DROP DNS CACHE", adm),
        ("KILL QUERY WHERE query_id = 'x'", adm),
        ("KILL MUTATION WHERE table = 't'", adm),
        ("GRANT SELECT ON db.* TO u", adm),
        ("REVOKE SELECT ON db.* FROM u", adm),
        ("CREATE USER u IDENTIFIED BY 'x'", adm),
        ("ALTER USER u IDENTIFIED BY 'y'", adm),
        ("CREATE ROLE r", adm),
        (
            "CREATE QUOTA q FOR INTERVAL 1 day MAX queries = 10 TO u",
            adm,
        ),
        ("CREATE ROW POLICY p ON t USING 1 TO u", adm),
        ("CREATE SETTINGS PROFILE p SETTINGS max_threads = 1", adm),
        ("BACKUP TABLE t TO Disk('b', 'x.zip')", adm),
        ("RESTORE TABLE t FROM Disk('b', 'x.zip')", adm),
        ("DROP USER u", (C::Admin, Some(D::Drop))),
        ("DROP ROLE r", (C::Admin, Some(D::Drop))),
        ("SET readonly = 0", adm),
    ];
    assert!(rows.len() >= 80, "{} cases", rows.len());
    for (sql, want) in rows {
        assert_eq!(cls(sql), want, "{sql}");
    }
}

#[test]
fn unknown_verbs_keep_the_safety_net() {
    assert_eq!(cls("FOO bar").0, C::Unknown);
    assert_eq!(cls("FOO DROP TABLE t").1, Some(D::Drop));
    assert_eq!(cls("FOO TRUNCATE t").1, Some(D::Truncate));
}

#[test]
fn verb_and_targets() {
    let a = analyze(H, "SELECT a FROM db.t WHERE id = 1");
    assert_eq!(a.statements[0].verb, "SELECT");
    assert_eq!(a.statements[0].targets, vec!["db.t"]);
    let a = analyze(H, "SELECT * FROM `my-db`.`t 1`");
    assert_eq!(a.statements[0].targets, vec!["my-db.t 1"]);
    let a = analyze(H, "INSERT INTO db.t VALUES (1)");
    assert_eq!(a.statements[0].targets, vec!["db.t"]);
    assert!(!a.statements[0].implicit_commit);
}

#[test]
fn splitting() {
    assert_eq!(texts("SELECT 1; SELECT 2;"), ["SELECT 1", "SELECT 2"]);
    assert_eq!(texts("SELECT 1; SELECT 2"), ["SELECT 1", "SELECT 2"]);
    assert_eq!(texts("SELECT ';'; SELECT 2"), ["SELECT ';'", "SELECT 2"]);
    assert_eq!(texts("SELECT 'a\\'; b'; SELECT 2").len(), 2);
    assert_eq!(texts("SELECT \"a;b\" FROM t; SELECT 2").len(), 2);
    assert_eq!(texts("SELECT `a;b` FROM t; SELECT 2").len(), 2);
    assert_eq!(texts("SELECT 1 -- ;\n; SELECT 2").len(), 2);
    assert_eq!(texts("SELECT 1 # ;\n; SELECT 2").len(), 2);
    assert_eq!(texts("SELECT 1 /* ; */; SELECT 2").len(), 2);
    assert_eq!(texts("SELECT $x$a;b$x$; SELECT 2").len(), 2);
    assert_eq!(texts("SELECT $$a;b$$; SELECT 2").len(), 2);
    assert_eq!(texts(";;; SELECT 1;;").len(), 1);
    assert!(texts("  ").is_empty());
    assert!(texts("-- only a comment").is_empty());
    assert_eq!(texts("SELECT 'it''s; ok'; SELECT 2").len(), 2);
    // BEGIN ... END is not special in ClickHouse: `;` always ends
    assert_eq!(texts("SELECT 1; SELECT 2; SELECT 3").len(), 3);
}

#[test]
fn stream_splitter_matches_split() {
    let sql = "SELECT 1; INSERT INTO t VALUES ('a;b'); SELECT `x;y`; USE db";
    let want = split(H, sql).len();
    let mut s = StreamSplitter::new(H);
    let mut got = 0;
    for ch in sql.chars() {
        got += s.push(&ch.to_string()).len();
    }
    got += s.finish().len();
    assert_eq!(got, want);
    assert_eq!(want, 4);
}

#[test]
fn lexer_tokens() {
    assert_eq!(kinds("'it''s'"), [(K::String, "'it''s'".into())]);
    // backslash escapes in strings and identifiers
    assert_eq!(kinds("'a\\'b'"), [(K::String, "'a\\'b'".into())]);
    assert_eq!(kinds("\"a\\\"b\""), [(K::QuotedIdent, "\"a\\\"b\"".into())]);
    assert_eq!(kinds("`a\\`b`"), [(K::QuotedIdent, "`a\\`b`".into())]);
    assert_eq!(kinds("`a``b`").len(), 1);
    assert_eq!(kinds("\"a\"\"b\"").len(), 1);
    assert_eq!(
        kinds("$tag$a'b$tag$"),
        [(K::String, "$tag$a'b$tag$".into())]
    );
    assert_eq!(kinds("$$a$$"), [(K::String, "$$a$$".into())]);
    assert_eq!(kinds("-- c"), [(K::LineComment, "-- c".into())]);
    assert_eq!(kinds("# c"), [(K::LineComment, "# c".into())]);
    assert_eq!(kinds("#! c"), [(K::LineComment, "#! c".into())]);
    assert_eq!(kinds("/* c */"), [(K::BlockComment, "/* c */".into())]);
    // `//` is an operator-ish punctuation, not a comment
    assert_eq!(kinds("//")[0].0, K::Punct);
    // block comments nest
    let k = kinds("/* /* */ x */");
    assert_eq!(k, [(K::BlockComment, "/* /* */ x */".into())]);
    // double quotes are identifiers
    assert_eq!(kinds("\"x\"")[0].0, K::QuotedIdent);
    // query parameter: braces stay punctuation
    let k = kinds("{id:UInt64}");
    assert_eq!(k[0].1, "{");
    assert_eq!(k.last().unwrap().1, "}");
    let src = "SELECT 'a'; -- x\n`b` \"c\" $t$d$t$ # e";
    let tiles: usize = tokenize(H, src)
        .iter()
        .map(|t| (t.range.end - t.range.start) as usize)
        .sum();
    assert_eq!(tiles, src.chars().count());
}

#[test]
fn lexer_problems_make_statements_unknown() {
    for bad in [
        "SELECT 'abc",
        "SELECT \"abc",
        "SELECT `abc",
        "SELECT $t$abc",
        "SELECT 1 /* x",
        "SELECT 'a\\",
    ] {
        let (_, p) = tokenize_with_problems(H, bad);
        assert_eq!(p.len(), 1, "{bad}");
        let a = analyze(H, bad);
        assert!(!a.problems.is_empty(), "{bad}");
        assert_eq!(a.statements[0].class, C::Unknown, "{bad}");
    }
    let a = analyze(H, "TRUNCATE t; SELECT 'x");
    assert_eq!(a.statements[0].class, C::Unknown);
    assert_eq!(a.statements[0].destructive, Some(D::Truncate));
}

#[test]
fn parameters() {
    let p = find_params(H, "SELECT * FROM t WHERE a = :a AND b = :b AND c = :a");
    assert_eq!(p.len(), 2);
    assert_eq!(p[0].ranges.len(), 2);
    // `?` is the ternary operator in ClickHouse, never a parameter
    assert!(find_params(H, "SELECT * FROM t WHERE a = ? AND b = ?").is_empty());
    assert!(find_params(H, "SELECT x ? 1 : 2 FROM t").is_empty());
    // ClickHouse typed parameters are kept as they are
    assert!(find_params(H, "SELECT * FROM t WHERE id = {id:UInt64}").is_empty());
    assert!(find_params(H, "SELECT {a:String}, {b:Array(Int32)}").is_empty());
    assert!(find_params(H, "SELECT ':a', `x:y`, \"p:q\" -- :c\n").is_empty());
    let p = find_params(H, "SELECT {a:String}, :b");
    assert_eq!(p.len(), 1);
    assert_eq!(p[0].name, ":b");
    let b = dbstudio_sql::bind(
        H,
        "SELECT {a:String}, :b",
        &[dbstudio_core::ParamValue {
            name: ":b".into(),
            value: dbstudio_core::ParamInput::Text("it's \\".into()),
        }],
    )
    .unwrap();
    assert_eq!(b.text, "SELECT {a:String}, 'it''s \\\\'");
}

#[test]
fn redaction() {
    let r = |s: &str| redact(H, s);
    assert_eq!(
        r("CREATE USER u IDENTIFIED BY 'secret'"),
        "CREATE USER u IDENTIFIED BY '***'"
    );
    assert_eq!(
        r("CREATE USER u IDENTIFIED WITH sha256_password BY 'secret'"),
        "CREATE USER u IDENTIFIED WITH sha256_password BY '***'"
    );
    assert_eq!(
        r("ALTER USER u IDENTIFIED WITH plaintext_password BY 'p' SETTINGS x = 1"),
        "ALTER USER u IDENTIFIED WITH plaintext_password BY '***' SETTINGS x = 1"
    );
    assert_eq!(
        r("CREATE USER u IDENTIFIED WITH sha256_hash BY 'abcdef' HOST ANY"),
        "CREATE USER u IDENTIFIED WITH sha256_hash BY '***' HOST ANY"
    );
    assert_eq!(
        r("SELECT * FROM mysql('h:3306', 'db', 't', 'user', 'pw')"),
        "SELECT * FROM mysql('h:3306', 'db', 't', 'user', '***')"
    );
    assert_eq!(
        r("CREATE TABLE t (a Int32) ENGINE = PostgreSQL('h:5432', 'db', 't', 'user', 'pw')"),
        "CREATE TABLE t (a Int32) ENGINE = PostgreSQL('h:5432', 'db', 't', 'user', '***')"
    );
    assert_eq!(
        r("SELECT * FROM remote('h', 'db', 't', 'user', 'pw')"),
        "SELECT * FROM remote('h', 'db', 't', 'user', '***')"
    );
    assert_eq!(r("SELECT * FROM t"), "SELECT * FROM t");
    assert_eq!(r("SELECT 'identified by x'"), "SELECT 'identified by x'");
    assert_eq!(
        r("SELECT 1; CREATE USER u IDENTIFIED BY 'x'; SELECT 2"),
        "SELECT 1; CREATE USER u IDENTIFIED BY '***'; SELECT 2"
    );
}

#[test]
fn quoting_and_literals() {
    let d = Dialect::new(H);
    assert_eq!(d.quote_ident("a`b"), "`a\\`b`");
    assert_eq!(d.quote_ident("a\\b"), "`a\\\\b`");
    assert_eq!(d.literal_text("it's"), "'it''s'");
    assert_eq!(d.literal_text("a\\b"), "'a\\\\b'");
    assert_eq!(d.literal(&Value::Null), "NULL");
    assert_eq!(d.literal(&Value::Bool(false)), "false");
    assert_eq!(d.literal(&Value::Int(-5)), "-5");
    assert_eq!(d.literal(&Value::UInt(7)), "7");
    assert_eq!(d.literal(&Value::Float(1.5)), "1.5");
    assert_eq!(d.literal(&Value::Float(f64::NAN)), "nan");
    assert_eq!(d.literal(&Value::Float(f64::INFINITY)), "inf");
    assert_eq!(d.literal(&Value::Decimal("12.50".into())), "12.50");
    assert_eq!(d.literal(&Value::Bytes(vec![0xde, 0xad])), "unhex('dead')");
    assert_eq!(d.literal(&Value::Date("2024-05-31".into())), "'2024-05-31'");
    assert_eq!(
        d.literal(&Value::DateTime("2024-05-31T10:00:00".into())),
        "'2024-05-31 10:00:00'"
    );
    assert_eq!(
        d.literal(&Value::Timestamp("2024-05-31T10:00:00.123Z".into())),
        "'2024-05-31 10:00:00.123'"
    );
    assert_eq!(
        d.literal(&Value::Uuid("123e4567-e89b-12d3-a456-426614174000".into())),
        "'123e4567-e89b-12d3-a456-426614174000'"
    );
    let arr = Value::Array(vec![Value::Int(1), Value::Int(2)]);
    assert_eq!(d.literal(&arr), "[1, 2]");
    assert_eq!(d.literal_for_type(&arr, Some("Array(Int32)")), "[1, 2]");
    assert_eq!(
        d.literal_for_type(&arr, Some("Tuple(Int32, Int32)")),
        "(1, 2)"
    );
    assert_eq!(
        d.literal_for_type(&arr, Some("Nullable(Array(Int32))")),
        "[1, 2]"
    );
    assert_eq!(d.literal(&Value::Array(vec![])), "[]");
    let strs = Value::Array(vec![Value::Text("a".into()), Value::Text("b'".into())]);
    assert_eq!(d.literal(&strs), "['a', 'b''']");
    let dt = Value::Array(vec![Value::DateTime("2024-05-31T10:00:00".into())]);
    assert_eq!(
        d.literal_for_type(&dt, Some("Array(DateTime64(3, 'UTC'))")),
        "['2024-05-31 10:00:00']"
    );
    let doc = Value::Document(vec![
        ("k".into(), Value::Int(1)),
        ("j".into(), Value::Int(2)),
    ]);
    assert_eq!(d.literal(&doc), "map('k', 1, 'j', 2)");
    assert_eq!(
        d.literal_for_type(&doc, Some("Map(String, Int32)")),
        "map('k', 1, 'j', 2)"
    );
    let nested = Value::Document(vec![("a".into(), Value::Array(vec![Value::Int(1)]))]);
    assert_eq!(d.literal(&nested), "map('a', [1])");
    assert_eq!(d.literal(&Value::Document(vec![])), "map()");
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
        "`q`",
        "\\",
        "a\\'b",
    ] {
        let lit = Dialect::new(H).literal_text(s);
        let toks = tokenize(H, &lit);
        assert_eq!(toks.len(), 1, "{s:?} -> {lit}");
        assert_eq!(toks[0].kind, K::String);
        assert_eq!(split(H, &lit).len(), 1);
        let q = Dialect::new(H).quote_ident(s);
        let toks = tokenize(H, &q);
        assert_eq!(toks.len(), 1, "{s:?} -> {q}");
        assert_eq!(toks[0].kind, K::QuotedIdent);
        assert!(tokenize_with_problems(H, &q).1.is_empty(), "{q}");
    }
}

#[test]
fn column_types() {
    use dbstudio_core::ValueKind as V;
    let d = Dialect::new(H);
    assert_eq!(d.type_for(V::Int), "Int64");
    assert_eq!(d.type_for(V::UInt), "UInt64");
    assert_eq!(d.type_for(V::Bool), "Bool");
    assert_eq!(d.type_for(V::Float), "Float64");
    assert_eq!(d.type_for(V::Uuid), "UUID");
    assert_eq!(d.type_for(V::Timestamp), "DateTime64(6, 'UTC')");
    assert_eq!(d.type_for(V::Text), "String");
    assert_eq!(d.type_for(V::Bytes), "String");
}

#[test]
fn vocabulary_has_clickhouse_words() {
    let v = vocabulary(H);
    for k in [
        "SELECT", "FINAL", "PREWHERE", "ENGINE", "SETTINGS", "OPTIMIZE",
    ] {
        assert!(v.keywords.iter().any(|x| x == k), "{k}");
    }
    for f in [
        "toDate",
        "arrayJoin",
        "uniq",
        "groupArray",
        "JSONExtractString",
    ] {
        assert!(v.functions.iter().any(|x| x == f), "{f}");
    }
    assert!(v.commands.is_empty());
    assert!(v.keywords.len() > 150 && v.functions.len() > 100);
    let mut up: Vec<String> = v.keywords.iter().map(|k| k.to_uppercase()).collect();
    let n = up.len();
    up.sort();
    up.dedup();
    assert_eq!(up.len(), n);
}

#[test]
fn use_statement_tracks_the_database() {
    let sql = "SELECT 1 FROM a; USE `db 2`; SELECT 1 FROM b";
    let a = analyze(H, sql);
    let dbs = dbstudio_sql::statement_databases(H, sql, &a);
    assert_eq!(dbs, [None, Some("db 2".into()), Some("db 2".into())]);
}

#[test]
fn block_comments_nest() {
    // The inner `*/` does not end the comment, so the quote below is code and opens a string
    // that hides nothing: the DROP is a real statement start only after the comment.
    let sql = "/* /* */ SELECT ' */ DROP TABLE t -- '";
    let a = analyze(H, sql);
    assert!(
        !a.problems.is_empty() || a.statements[0].destructive.is_some(),
        "{a:?}"
    );
    let sql = "/* /* */ DROP TABLE t */ SELECT 1";
    assert_eq!(cls(sql), (C::Read, None));
    assert_eq!(
        kinds("/* /* */ x */"),
        [(K::BlockComment, "/* /* */ x */".into())]
    );
    let unterminated = analyze(H, "/* /* */ SELECT 1");
    assert!(!unterminated.problems.is_empty());
    // the attack from the review: the server sees `DROP TABLE t` after the nested comment
    let attack = "/* /* */ DROP TABLE t */ DROP TABLE u";
    assert_eq!(cls(attack), (C::Ddl, Some(D::Drop)));
    assert_eq!(
        cls("/* /* */ TRUNCATE t */ TRUNCATE u"),
        (C::Ddl, Some(D::Truncate))
    );
}

#[test]
fn alter_commands_are_judged_one_by_one() {
    let wr = |d| (C::Write, Some(d));
    for (sql, want) in [
        (
            "ALTER TABLE t DELETE WHERE 1 SETTINGS mutations_sync = 2",
            wr(D::DeleteWithoutWhere),
        ),
        (
            "DELETE FROM t WHERE 1 SETTINGS lightweight_deletes_sync = 2",
            wr(D::DeleteWithoutWhere),
        ),
        (
            "ALTER TABLE t DELETE WHERE 1 FORMAT Null",
            wr(D::DeleteWithoutWhere),
        ),
        (
            "ALTER TABLE t DELETE WHERE 1, MODIFY SETTING x = 1",
            wr(D::DeleteWithoutWhere),
        ),
        (
            "ALTER TABLE t UPDATE a = 1 WHERE 1, UPDATE b = 2 WHERE id = 3",
            wr(D::UpdateWithoutWhere),
        ),
        (
            "ALTER TABLE t UPDATE a = 1 WHERE id = 3, UPDATE b = 2 WHERE 1",
            wr(D::UpdateWithoutWhere),
        ),
        (
            "ALTER TABLE t UPDATE a = 1, b = 2 WHERE 1",
            wr(D::UpdateWithoutWhere),
        ),
        (
            "ALTER TABLE t UPDATE a = 1, b = 2 WHERE id = 5",
            (C::Write, None),
        ),
        (
            "ALTER TABLE t DELETE WHERE id = 1 SETTINGS mutations_sync = 2",
            (C::Write, None),
        ),
        (
            "ALTER TABLE t DELETE WHERE id = 1, DELETE WHERE 1",
            wr(D::DeleteWithoutWhere),
        ),
        // a TTL action must not hide a later real DELETE
        (
            "ALTER TABLE t MODIFY TTL d DELETE, DELETE WHERE 1",
            (C::Write, Some(D::AlterDrop)),
        ),
        (
            "ALTER TABLE t MODIFY TTL d DELETE, DELETE WHERE id = 1",
            (C::Write, Some(D::AlterDrop)),
        ),
        (
            "ALTER TABLE t DELETE WHERE 1, MODIFY TTL d DELETE",
            (C::Write, Some(D::AlterDrop)),
        ),
        (
            "ALTER TABLE IF EXISTS db.t ON CLUSTER c DELETE WHERE 1",
            wr(D::DeleteWithoutWhere),
        ),
        (
            "ALTER TABLE `my db`.`t` DELETE WHERE 1",
            wr(D::DeleteWithoutWhere),
        ),
        (
            "ALTER TABLE t DROP COLUMN a, DELETE WHERE id = 1",
            (C::Write, Some(D::AlterDrop)),
        ),
    ] {
        assert_eq!(cls(sql), want, "{sql}");
    }
}

#[test]
fn destructive_alter_variants() {
    let ad = (C::Ddl, Some(D::AlterDrop));
    for sql in [
        "ALTER TABLE t CLEAR COLUMN c IN PARTITION 1",
        "ALTER TABLE t CLEAR INDEX i",
        "ALTER TABLE t CLEAR PROJECTION p",
        "ALTER TABLE t REPLACE PARTITION 1 FROM u",
        "ALTER TABLE t MOVE PARTITION 1 TO TABLE u",
        "ALTER TABLE t MATERIALIZE TTL",
    ] {
        let want = if sql.contains("MATERIALIZE") {
            (C::Ddl, None)
        } else {
            ad
        };
        assert_eq!(cls(sql), want, "{sql}");
    }
    assert_eq!(
        cls("ALTER TABLE t MOVE PARTITION 1 TO DISK 'cold'"),
        (C::Ddl, None)
    );
    assert_eq!(
        cls("ALTER TABLE t MOVE PART 'x' TO VOLUME 'v'"),
        (C::Ddl, None)
    );
    assert_eq!(cls("ALTER TABLE t ADD COLUMN c Int32"), (C::Ddl, None));
}

#[test]
fn replace_table_drops_the_old_one() {
    let dr = (C::Ddl, Some(D::Drop));
    assert_eq!(
        cls("CREATE OR REPLACE TABLE t (a Int32) ENGINE = Memory"),
        dr
    );
    assert_eq!(cls("create or replace table t as select 1"), dr);
    assert_eq!(cls("REPLACE TABLE t ENGINE = Memory AS SELECT 1"), dr);
    assert_eq!(
        cls(
            "CREATE OR REPLACE DICTIONARY d (id UInt64) PRIMARY KEY id SOURCE(NULL()) LAYOUT(FLAT()) LIFETIME(0)"
        ),
        dr
    );
    assert_eq!(cls("CREATE OR REPLACE VIEW v AS SELECT 1"), (C::Ddl, None));
    assert_eq!(
        cls("CREATE TABLE t (a Int32) ENGINE = Memory"),
        (C::Ddl, None)
    );
}

#[test]
fn external_resource_functions_are_not_plain_reads() {
    for f in [
        "file('x.csv')",
        "url('http://x')",
        "urlCluster('c', 'http://x')",
        "s3('u')",
        "s3Cluster('c', 'u')",
        "gcs('u')",
        "azureBlobStorage('a')",
        "hdfs('u')",
        "remote('h', 'd', 't')",
        "remoteSecure('h', 'd', 't')",
        "cluster('c', 'd', 't')",
        "mysql('h', 'd', 't', 'u', 'p')",
        "postgresql('h', 'd', 't', 'u', 'p')",
        "mongodb('h', 'd', 'c', 'u', 'p', 's')",
        "redis('h', 'k', 's')",
        "jdbc('u', 't')",
        "odbc('d', 't')",
        "sqlite('p', 't')",
        "executable('s', 'TSV', 'a UInt8')",
        "input('a UInt8')",
    ] {
        let sql = format!("SELECT * FROM {f}");
        assert_eq!(cls(&sql), (C::Admin, None), "{sql}");
        let lower = sql.to_lowercase();
        assert_eq!(cls(&lower).0, C::Admin, "{lower}");
    }
    // writing keeps its own class; the name alone is not enough
    assert_eq!(cls("INSERT INTO FUNCTION s3('u') SELECT 1").0, C::Write);
    assert_eq!(cls("SELECT file FROM t").0, C::Read);
    assert_eq!(cls("SELECT url, s3 FROM t").0, C::Read);
    assert_eq!(cls("SELECT * FROM numbers(3)").0, C::Read);
    assert_eq!(
        cls("SELECT 1 FROM t WHERE x IN (SELECT a FROM url('http://x'))").0,
        C::Admin
    );
}

#[test]
fn clickhouse_admin_and_session_extras() {
    assert_eq!(cls("MOVE USER u TO local_directory"), (C::Admin, None));
    assert_eq!(cls("SET allow_ddl = 1"), (C::Admin, None));
    assert_eq!(
        cls("SET allow_introspection_functions = 1"),
        (C::Admin, None)
    );
    assert_eq!(cls("SET max_threads = 1"), (C::Session, None));
}

#[test]
fn heredoc_tags_are_ascii() {
    // `$é$` is not a heredoc opener in ClickHouse
    let k = kinds("$é$ x $é$");
    assert!(k.iter().all(|(kind, _)| *kind != K::String), "{k:?}");
    assert_eq!(kinds("$abc_1$ x $abc_1$").len(), 1);
}
