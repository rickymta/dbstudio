//! MySQL `ANSI_QUOTES`, TiDB executable comments and `BATCH ON`, CockroachDB keywords.

use dbstudio_core::{DestructiveReason as D, ScriptDialect, StatementClass as C};
use dbstudio_sql::lexer::{TokenKind as K, tokenize};
use dbstudio_sql::{analyze, mysql_dialect_from_sql_mode, split};

const MY: ScriptDialect = ScriptDialect::mysql(false, true);
const ANSI: ScriptDialect = ScriptDialect::MySql {
    mariadb: false,
    backslash_escapes: true,
    ansi_quotes: true,
};
const PG: ScriptDialect = ScriptDialect::Postgres;

fn kinds(d: ScriptDialect, sql: &str) -> Vec<K> {
    tokenize(d, sql)
        .into_iter()
        .filter(|t| t.kind != K::Whitespace)
        .map(|t| t.kind)
        .collect()
}

fn cls(d: ScriptDialect, sql: &str) -> (C, Option<D>) {
    let a = analyze(d, sql);
    assert_eq!(a.statements.len(), 1, "{sql}");
    (a.statements[0].class, a.statements[0].destructive)
}

#[test]
fn ansi_quotes_make_double_quotes_identifiers() {
    assert_eq!(kinds(MY, "\"x\""), [K::String]);
    assert_eq!(kinds(ANSI, "\"x\""), [K::QuotedIdent]);
    assert_eq!(kinds(ANSI, "\"a\"\"b\""), [K::QuotedIdent]);
    // single quotes stay strings, backticks stay identifiers
    assert_eq!(kinds(ANSI, "'x' `y`"), [K::String, K::QuotedIdent]);
    // a backslash is not an escape inside an ANSI identifier
    assert_eq!(kinds(ANSI, "\"a\\\" 'b'").len(), 2);
    assert_eq!(kinds(MY, "\"a\\\" 'b'\"").len(), 1);
    let (_, p) = dbstudio_sql::lexer::tokenize_with_problems(ANSI, "\"abc");
    assert_eq!(p[0].message, "unterminated quoted identifier");
}

#[test]
fn ansi_quotes_change_statement_boundaries_and_targets() {
    let sql = "SELECT \"a\\\"; DROP TABLE t; -- \"";
    assert_eq!(split(MY, sql).len(), 1);
    assert_eq!(split(ANSI, sql).len(), 2);
    let a = analyze(ANSI, "SELECT * FROM \"my table\" WHERE id = 1");
    assert_eq!(a.statements[0].targets, vec!["my table"]);
    let a = analyze(MY, "SELECT * FROM \"my table\" WHERE id = 1");
    assert_eq!(a.statements[0].class, C::Read);
    // DROP "t" is a table in ANSI mode, still destructive
    assert_eq!(cls(ANSI, "DROP TABLE \"t\""), (C::Ddl, Some(D::Drop)));
    assert_eq!(
        cls(ANSI, "DELETE FROM \"t\" WHERE 1"),
        (C::Write, Some(D::DeleteWithoutWhere))
    );
    // in ANSI mode `"x"` is not a parameter-looking or keyword-hiding string
    assert_eq!(cls(ANSI, "SELECT \"drop\" FROM t").1, None);
}

#[test]
fn sql_mode_to_ansi_dialect() {
    let d = |m| mysql_dialect_from_sql_mode(m, false);
    assert_eq!(d(""), MY);
    assert_eq!(d("ANSI_QUOTES"), ANSI);
    assert_eq!(
        d("REAL_AS_FLOAT,PIPES_AS_CONCAT,ANSI_QUOTES,IGNORE_SPACE"),
        ANSI
    );
    assert_eq!(d("ansi"), ANSI);
    assert_eq!(
        d("ANSI_QUOTES,NO_BACKSLASH_ESCAPES"),
        ScriptDialect::MySql {
            mariadb: false,
            backslash_escapes: false,
            ansi_quotes: true
        }
    );
    assert_eq!(d("STRICT_TRANS_TABLES"), MY);
    assert_eq!(d("NOT_ANSI_QUOTES"), MY);
}

#[test]
fn tidb_executable_comments_are_code() {
    for sql in [
        "SELECT /*T! 1 */ 2",
        "SELECT /*T![clustered_index] 1 */ 2",
        "SELECT /*T!50100 1 */ 2",
    ] {
        let k = kinds(MY, sql);
        assert!(!k.contains(&K::BlockComment), "{sql}: {k:?}");
    }
    assert_eq!(
        cls(MY, "/*T![clustered_index] DROP TABLE t */"),
        (C::Ddl, Some(D::Drop))
    );
    assert_eq!(
        cls(MY, "/*T!40000 DROP TABLE t */"),
        (C::Ddl, Some(D::Drop))
    );
    assert_eq!(cls(MY, "/*!40000 DROP TABLE t */"), (C::Ddl, Some(D::Drop)));
    assert_eq!(
        cls(
            MY,
            "CREATE TABLE t (a int primary key /*T![clustered_index] CLUSTERED */)"
        )
        .0,
        C::Ddl
    );
    // a plain comment is still a comment
    assert!(kinds(MY, "/* drop table t */ select 1").contains(&K::BlockComment));
    assert_eq!(cls(MY, "/* drop table t */ select 1"), (C::Read, None));
    // an unclosed feature bracket keeps `[` as code instead of eating text
    assert_eq!(cls(MY, "SELECT /*T![abc 1 */ 1").0, C::Read);
}

#[test]
fn tidb_batch_dml_and_admin_statements() {
    assert_eq!(
        cls(MY, "BATCH ON id LIMIT 1000 DELETE FROM t WHERE x = 1"),
        (C::Write, None)
    );
    assert_eq!(
        cls(MY, "BATCH ON id LIMIT 1000 DELETE FROM t WHERE 1"),
        (C::Write, Some(D::DeleteWithoutWhere))
    );
    assert_eq!(
        cls(MY, "BATCH ON id LIMIT 1000 DELETE FROM t"),
        (C::Write, Some(D::DeleteWithoutWhere))
    );
    assert_eq!(
        cls(MY, "BATCH ON id LIMIT 10 UPDATE t SET a = 1 WHERE b = 2"),
        (C::Write, None)
    );
    assert_eq!(
        cls(MY, "BATCH LIMIT 10 INSERT INTO t SELECT * FROM u"),
        (C::Write, None)
    );
    assert_eq!(cls(MY, "ADMIN SHOW DDL JOBS"), (C::Admin, None));
    assert_eq!(cls(MY, "ADMIN CANCEL DDL JOBS 1"), (C::Admin, None));
    assert_eq!(cls(MY, "CANCEL IMPORT JOB 1"), (C::Admin, None));
    assert_eq!(cls(MY, "BACKUP DATABASE * TO 's3://b'"), (C::Admin, None));
    assert_eq!(
        cls(MY, "RESTORE DATABASE * FROM 's3://b'"),
        (C::Admin, None)
    );
    assert_eq!(
        cls(MY, "IMPORT INTO t FROM 's3://b/x.csv'"),
        (C::Admin, None)
    );
    assert_eq!(cls(MY, "SET CONFIG tikv `x` = 1"), (C::Admin, None));
    assert_eq!(cls(MY, "SET @a = 1"), (C::Session, None));
    assert_eq!(cls(MY, "FLASHBACK TABLE t TO t2").0, C::Ddl,);
}

#[test]
fn cockroach_keywords() {
    assert_eq!(
        cls(PG, "UPSERT INTO t (a, b) VALUES (1, 2)"),
        (C::Write, None)
    );
    assert_eq!(cls(PG, "SHOW RANGES FROM TABLE t"), (C::Admin, None));
    assert_eq!(
        cls(PG, "SHOW RANGE FROM TABLE t FOR ROW (1)"),
        (C::Admin, None)
    );
    assert_eq!(cls(PG, "SHOW TABLES"), (C::Read, None));
    assert_eq!(cls(PG, "BACKUP INTO 's3://b'"), (C::Admin, None));
    assert_eq!(cls(PG, "RESTORE FROM LATEST IN 's3://b'"), (C::Admin, None));
    assert_eq!(
        cls(PG, "IMPORT INTO t CSV DATA ('s3://b/x')"),
        (C::Admin, None)
    );
    assert_eq!(cls(PG, "CANCEL QUERY '123'"), (C::Admin, None));
    assert_eq!(cls(PG, "CANCEL JOB 1"), (C::Admin, None));
    assert_eq!(cls(PG, "CANCEL SESSION '1'"), (C::Admin, None));
    assert_eq!(cls(PG, "PAUSE JOB 1"), (C::Admin, None));
    assert_eq!(cls(PG, "RESUME JOBS SELECT 1"), (C::Admin, None));
    assert_eq!(
        cls(PG, "SET CLUSTER SETTING kv.rangefeed.enabled = true"),
        (C::Admin, None)
    );
    assert_eq!(cls(PG, "SET application_name = 'x'"), (C::Session, None));
    assert_eq!(cls(PG, "SHOW CLUSTER SETTING version"), (C::Read, None));
    // `UPSERT ... ` inside ordinary text does not matter; destructive keywords still do
    assert_eq!(cls(PG, "DROP TABLE t"), (C::Ddl, Some(D::Drop)));
}
