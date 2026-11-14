//! Findings of the package A review: executable comments, T-SQL statements without `;`,
//! MySQL/MariaDB/PostgreSQL specifics, Mongo chains, prepared statements, redaction.

use dbstudio_core::{DestructiveReason as R, Error, ScriptDialect, StatementClass as C};
use dbstudio_sql::redact::redact;
use dbstudio_sql::{Policy, Verdict, analyze, check, split, statement_databases};

const PG: ScriptDialect = ScriptDialect::Postgres;
const MY: ScriptDialect = ScriptDialect::mysql(false, true);
const MS: ScriptDialect = ScriptDialect::MsSql;
const ORA: ScriptDialect = ScriptDialect::Oracle;
const MONGO: ScriptDialect = ScriptDialect::MongoShell;

fn classes(d: ScriptDialect, sql: &str) -> Vec<(C, Option<R>)> {
    analyze(d, sql)
        .statements
        .iter()
        .map(|s| (s.class, s.destructive))
        .collect()
}

fn one(d: ScriptDialect, sql: &str) -> (C, Option<R>) {
    let v = classes(d, sql);
    assert_eq!(v.len(), 1, "{d:?} {sql:?} -> {v:?}");
    v[0]
}

fn ro_denied(d: ScriptDialect, sql: &str) {
    let policy = Policy {
        read_only: true,
        production: false,
        confirm_text: "db".into(),
    };
    match check(&policy, &analyze(d, sql)) {
        Verdict::Deny(Error::ReadOnlyViolation { .. }) => {}
        other => panic!("{d:?} {sql:?}: {other:?}"),
    }
}

fn ro_allowed(d: ScriptDialect, sql: &str) {
    let policy = Policy {
        read_only: true,
        production: false,
        confirm_text: "db".into(),
    };
    assert!(
        matches!(check(&policy, &analyze(d, sql)), Verdict::Allow),
        "{d:?} {sql:?}"
    );
}

fn prod_confirms(d: ScriptDialect, sql: &str) {
    let policy = Policy {
        read_only: false,
        production: true,
        confirm_text: "db".into(),
    };
    assert!(
        matches!(check(&policy, &analyze(d, sql)), Verdict::Confirm(_)),
        "{d:?} {sql:?}: {:?}",
        analyze(d, sql).statements
    );
}

// 1 ---------------------------------------------------------------------------------------

#[test]
fn mysql_executable_comments_are_code() {
    assert_eq!(one(MY, "/*!50000 DROP TABLE t */"), (C::Ddl, Some(R::Drop)));
    assert_eq!(one(MY, "/*! DROP TABLE t */"), (C::Ddl, Some(R::Drop)));
    assert_eq!(
        one(MY, "/*M!100000 TRUNCATE t */"),
        (C::Ddl, Some(R::Truncate))
    );
    assert_eq!(
        one(MY, "SELECT * FROM t /*!50000 INTO OUTFILE '/tmp/x' */").0,
        C::Write
    );
    assert_eq!(
        one(MY, "/*!50000 SET SESSION TRANSACTION READ WRITE */").0,
        C::Admin
    );
    ro_denied(MY, "/*!50000 DROP TABLE t */");
    ro_denied(MY, "/*!50000 SET SESSION TRANSACTION READ WRITE */");
    ro_denied(MY, "SELECT * FROM t /*!50000 INTO OUTFILE '/tmp/x' */");
    prod_confirms(MY, "/*!50000 DROP TABLE t */");
    // statements split inside executable comments like anywhere else
    assert_eq!(split(MY, "/*!40101 SET x = 1; */ select 2").len(), 2);
    // ordinary comments and other dialects keep ignoring the text
    assert_eq!(
        classes(MY, "/* DROP TABLE t */ select 1"),
        vec![(C::Read, None)]
    );
    assert_eq!(
        classes(PG, "/*! DROP TABLE t */ select 1"),
        vec![(C::Read, None)]
    );
    // mysqldump style header
    let dump = "/*!40101 SET @OLD_CHARACTER_SET_CLIENT=@@CHARACTER_SET_CLIENT */;\nDROP TABLE IF EXISTS `t`;";
    assert!(classes(MY, dump).iter().any(|c| c.1 == Some(R::Drop)));
}

// 2 ---------------------------------------------------------------------------------------

#[test]
fn tsql_statements_without_semicolons_are_split() {
    assert_eq!(
        classes(MS, "select 1 drop table t"),
        vec![(C::Read, None), (C::Ddl, Some(R::Drop))]
    );
    assert_eq!(
        classes(MS, "select 1 delete from t"),
        vec![(C::Read, None), (C::Write, Some(R::DeleteWithoutWhere))]
    );
    let v = classes(MS, "select 1 exec('drop table t')");
    assert_eq!(v[0], (C::Read, None));
    assert_eq!(v[1], (C::Procedural, Some(R::Drop)));
    let v = classes(
        MS,
        "with x as (select 1 a) select * from x update t set a = 1",
    );
    assert_eq!(v.len(), 2);
    assert_eq!(v[0], (C::Read, None));
    assert_eq!(v[1], (C::Write, Some(R::UpdateWithoutWhere)));
    for sql in [
        "select 1 drop table t",
        "select 1 delete from t",
        "select 1 truncate table t",
        "select 1 exec('drop table t')",
    ] {
        ro_denied(MS, sql);
        prod_confirms(MS, sql);
    }
}

#[test]
fn tsql_go_followed_by_a_comment_is_a_separator() {
    let sql = "select 1\nGO /* c */\ndrop table t";
    assert_eq!(split(MS, sql).len(), 2);
    assert_eq!(
        classes(MS, sql),
        vec![(C::Read, None), (C::Ddl, Some(R::Drop))]
    );
    prod_confirms(MS, sql);
    assert_eq!(split(MS, "select 1\nGO -- c\ndrop table t").len(), 2);
}

#[test]
fn tsql_splitting_keeps_legitimate_constructs_together() {
    // CTE, set operations, INSERT ... SELECT pieces, hints, definitions
    assert_eq!(
        classes(MS, "with a as (select 1 x) select * from a").len(),
        1
    );
    assert_eq!(classes(MS, "select 1 union all select 2").len(), 1);
    assert_eq!(
        classes(MS, "select * from t with (nolock) where a = 1").len(),
        1
    );
    assert_eq!(classes(MS, "grant select on t to u").len(), 1);
    assert_eq!(classes(MS, "deny delete on t to u").len(), 1);
    assert_eq!(classes(MS, "alter table t drop column c").len(), 1);
    assert_eq!(classes(MS, "drop table if exists t").len(), 1);
    assert_eq!(classes(MS, "update t set a = 1 where b = 2").len(), 1);
    assert_eq!(
        classes(
            MS,
            "merge t using s on t.id = s.id when matched then delete;"
        )
        .len(),
        1
    );
    assert_eq!(
        one(
            MS,
            "merge t using s on t.id = s.id when matched then delete;"
        ),
        (C::Write, Some(R::DeleteWithoutWhere))
    );
    assert_eq!(
        classes(MS, "create procedure p as select 1 delete from t").len(),
        1
    );
    assert_eq!(
        classes(MS, "select case when 1=1 then 1 else 2 end").len(),
        1
    );
    // keywords inside parentheses and blocks do not split
    assert_eq!(classes(MS, "select * from (select 1 a) x").len(), 1);
    assert_eq!(
        classes(MS, "if 1=1 begin delete from t select 1 end").len(),
        2
    );
    let v = classes(MS, "begin delete from t end");
    assert_eq!(v, vec![(C::Procedural, Some(R::DeleteWithoutWhere))]);
}

// 3, 4, 5 ---------------------------------------------------------------------------------

#[test]
fn mysql_describe_analyze_executes_the_statement() {
    let e = |analyze, writes| C::Explain { analyze, writes };
    assert_eq!(one(MY, "describe analyze delete from t").0, e(true, true));
    assert_eq!(
        one(MY, "desc analyze delete from t"),
        (e(true, true), Some(R::DeleteWithoutWhere))
    );
    assert_eq!(one(MY, "describe analyze select 1").0, e(true, false));
    assert_eq!(one(MY, "describe select 1").0, e(false, false));
    assert_eq!(one(MY, "describe t").0, C::Read);
    assert_eq!(one(MY, "desc t").0, C::Read);
    ro_denied(MY, "describe analyze delete from t");
    ro_allowed(MY, "describe analyze select 1");
}

#[test]
fn mariadb_set_statement_is_judged_by_the_statement() {
    let m = ScriptDialect::mysql(true, true);
    assert_eq!(
        one(m, "SET STATEMENT max_statement_time=10 FOR DELETE FROM t"),
        (C::Write, Some(R::DeleteWithoutWhere))
    );
    assert_eq!(
        one(m, "SET STATEMENT max_statement_time=10 FOR SELECT 1"),
        (C::Read, None)
    );
    assert_eq!(one(m, "SET STATEMENT sql_mode='' FOR SELECT 1").0, C::Admin);
    assert_eq!(
        one(m, "SET STATEMENT max_statement_time=1 FOR DROP TABLE t"),
        (C::Ddl, Some(R::Drop))
    );
    ro_denied(m, "SET STATEMENT max_statement_time=10 FOR DELETE FROM t");
    ro_allowed(m, "SET STATEMENT max_statement_time=10 FOR SELECT 1");
    prod_confirms(m, "SET STATEMENT max_statement_time=1 FOR DROP TABLE t");
}

#[test]
fn mysql_reset_is_administrative() {
    for sql in [
        "RESET MASTER",
        "RESET REPLICA",
        "RESET SLAVE",
        "RESET PERSIST",
        "RESET BINARY LOGS AND GTIDS",
        "reset query cache",
    ] {
        assert_eq!(one(MY, sql).0, C::Admin, "{sql}");
        ro_denied(MY, sql);
    }
}

// 6 ---------------------------------------------------------------------------------------

#[test]
fn quoted_names_are_matched_like_bare_ones() {
    for sql in [
        "SET \"default_transaction_read_only\" = off",
        "SET SESSION \"default_transaction_read_only\" TO off",
        "SET \"transaction_read_only\" = off",
        "SELECT \"set_config\"('default_transaction_read_only','off',false)",
        "SELECT \"nextval\"('s')",
        "SELECT \"pg_terminate_backend\"(1)",
        "SET \"standard_conforming_strings\" = off",
        "SET \"ROLE\" admin",
    ] {
        ro_denied(PG, sql);
    }
    // a quoted identifier that is not called is just a name
    ro_allowed(PG, "SELECT \"nextval\" FROM t");
}

#[test]
fn postgres_values_and_dblink_and_lexing_settings() {
    ro_denied(PG, "VALUES (nextval('s'))");
    ro_denied(PG, "VALUES (pg_terminate_backend(1))");
    ro_allowed(PG, "VALUES (1), (2)");
    ro_denied(
        PG,
        "SELECT * FROM dblink('dbname=x', 'delete from t') AS t(a int)",
    );
    ro_denied(PG, "SELECT dblink_exec('dbname=x', 'drop table t')");
    ro_denied(PG, "SELECT pg_advisory_lock(1)");
    ro_denied(PG, "SELECT pg_try_advisory_xact_lock(1)");
    ro_denied(MY, "SELECT get_lock('a', 10)");
    for sql in [
        "SET standard_conforming_strings = off",
        "SET LOCAL backslash_quote = on",
        "SET escape_string_warning = off",
        "RESET standard_conforming_strings",
        "RESET backslash_quote",
        "RESET ALL",
        "DISCARD ALL",
        "SET ROLE admin",
        "SET LOCAL ROLE admin",
        "SET SESSION AUTHORIZATION admin",
    ] {
        assert_eq!(one(PG, sql).0, C::Admin, "{sql}");
        ro_denied(PG, sql);
    }
    assert_eq!(one(MY, "SET DEFAULT ROLE ALL TO u").0, C::Admin);
    assert_eq!(one(MY, "SET ROLE r").0, C::Admin);
    ro_allowed(PG, "SET search_path = a");
    ro_allowed(PG, "RESET work_mem");
}

#[test]
fn lock_table_with_write_modes_is_not_read_only() {
    ro_denied(PG, "LOCK TABLE t IN ACCESS EXCLUSIVE MODE");
    ro_denied(PG, "LOCK TABLE t IN EXCLUSIVE MODE");
    ro_denied(MY, "LOCK TABLES t WRITE");
    ro_denied(ORA, "LOCK TABLE t IN EXCLUSIVE MODE");
    ro_allowed(MY, "LOCK TABLES t READ");
    ro_allowed(MY, "UNLOCK TABLES");
}

// 7 ---------------------------------------------------------------------------------------

#[test]
fn mongo_chains_are_whitelisted_and_siblings_resolved() {
    for q in [
        "db.c.find().drop()",
        "db.c.find().deleteMany({})",
        "db.stats().dropDatabase()",
        "db.c.aggregate([]).drop()",
        "db.c.countDocuments({}).drop()",
        "db.c.drop().find()",
    ] {
        assert!(dbstudio_sql::mongo_shell::parse(q).is_err(), "{q}");
    }
    for q in [
        "db.c.find({}).sort({a: 1}).limit(5).skip(1).project({a: 1}).toArray()",
        "db.c.find().hint({a: 1}).collation({locale: 'en'}).maxTimeMS(10).batchSize(5).comment('x').pretty()",
        "db.c.aggregate([{$match: {}}]).toArray()",
        "db.c.find().count()",
    ] {
        assert!(dbstudio_sql::mongo_shell::parse(q).is_ok(), "{q}");
    }
    // sibling databases
    assert_eq!(
        one(MONGO, "db.getSiblingDB('x').dropDatabase()"),
        (C::Ddl, Some(R::DropDatabase))
    );
    assert_eq!(
        one(MONGO, "db.getMongo().getDB('x').dropDatabase()"),
        (C::Ddl, Some(R::DropDatabase))
    );
    assert_eq!(
        one(MONGO, "db.getSiblingDB('x').c.deleteMany({})"),
        (C::Write, Some(R::DeleteManyAll))
    );
    assert_eq!(
        one(MONGO, "db.getSiblingDB('x').getCollection('c').drop()"),
        (C::Ddl, Some(R::DropCollection))
    );
    ro_denied(MONGO, "db.getSiblingDB('x').dropDatabase()");
    prod_confirms(MONGO, "db.getSiblingDB('x').dropDatabase()");
    ro_allowed(MONGO, "db.getSiblingDB('x').c.find({})");
    let text = "use a\ndb.getSiblingDB('b').c.drop()\ndb.c.drop()";
    let an = analyze(MONGO, text);
    assert_eq!(
        statement_databases(MONGO, text, &an),
        vec![
            Some("a".to_string()),
            Some("b".to_string()),
            Some("a".to_string())
        ]
    );
}

#[test]
fn mongo_aggregate_stages_as_arguments() {
    assert_eq!(one(MONGO, "db.c.aggregate({$out: 'b'})").0, C::Write);
    assert_eq!(
        one(MONGO, "db.c.aggregate({$match: {}}, {$merge: {into: 'b'}})").0,
        C::Write
    );
    assert_eq!(one(MONGO, "db.c.aggregate({$match: {}})").0, C::Read);
    ro_denied(MONGO, "db.c.aggregate({$out: 'b'})");
}

// 8 ---------------------------------------------------------------------------------------

#[test]
fn mysql_nbsp_is_not_a_dash_comment_separator() {
    // `--\u{a0}` is `- -` followed by an identifier char: the DROP is real code.
    let sql = "SELECT 1 --\u{a0}1; DROP TABLE t";
    assert!(
        classes(MY, sql).iter().any(|c| c.1 == Some(R::Drop)),
        "{sql:?}"
    );
    let sql = "SELECT 1 -- 1; DROP TABLE t";
    assert!(classes(MY, sql).iter().all(|c| c.1.is_none()));
}

// 9 ---------------------------------------------------------------------------------------

#[test]
fn prepared_statements_are_judged_by_their_text() {
    for (d, sql) in [
        (MY, "PREPARE s FROM 'drop table t'"),
        (MY, "PREPARE s FROM \"truncate table t\""),
        (PG, "PREPARE p AS DELETE FROM t"),
        (PG, "PREPARE p (int) AS UPDATE t SET a = $1"),
        (ORA, "EXECUTE IMMEDIATE 'drop table t'"),
        (MS, "EXEC('drop table t')"),
        (MS, "EXEC sp_executesql N'drop table t'"),
        (MS, "[sp_executesql] N'drop table t'"),
        (MS, "[dbo].[sp_executesql] N'delete from t'"),
    ] {
        prod_confirms(d, sql);
    }
    assert_eq!(one(MY, "PREPARE s FROM 'select 1'").0, C::Session);
    assert_eq!(one(PG, "PREPARE p AS SELECT 1").0, C::Session);
    ro_denied(MY, "PREPARE s FROM 'delete from t where a = 1'");
    ro_denied(MY, "PREPARE s FROM @v");
    assert_eq!(one(MY, "EXECUTE s").0, C::Procedural);
}

#[test]
fn procedural_scan_catches_dml_alter_and_merge() {
    for (d, sql) in [
        (MS, "IF 1=1 DELETE FROM t"),
        (MS, "BEGIN DELETE FROM t END"),
        (MS, "BEGIN UPDATE t SET a = 1 END"),
        (MS, "EXEC('DELETE FROM t')"),
        (PG, "DO $$ BEGIN DELETE FROM t; END $$"),
        (PG, "DO $$ BEGIN UPDATE t SET a = 1; END $$"),
        (ORA, "BEGIN DELETE FROM t; END;"),
        (ORA, "BEGIN EXECUTE IMMEDIATE 'delete from t'; END;"),
        (PG, "DO $$ BEGIN ALTER TABLE t DROP COLUMN c; END $$"),
    ] {
        prod_confirms(d, sql);
    }
    // bounded DML inside a block is fine
    let policy = Policy {
        read_only: false,
        production: true,
        confirm_text: "db".into(),
    };
    for (d, sql) in [
        (PG, "DO $$ BEGIN DELETE FROM t WHERE a = 1; END $$"),
        (ORA, "BEGIN DELETE FROM t WHERE a = 1; END;"),
    ] {
        assert!(
            matches!(check(&policy, &analyze(d, sql)), Verdict::Allow),
            "{sql}"
        );
    }
}

#[test]
fn merge_delete_and_partition_ddl_are_destructive() {
    for d in [PG, MS, ORA] {
        assert_eq!(
            one(
                d,
                "MERGE INTO t USING s ON (t.id = s.id) WHEN MATCHED THEN DELETE"
            ),
            (C::Write, Some(R::DeleteWithoutWhere)),
            "{d:?}"
        );
        assert_eq!(
            one(
                d,
                "MERGE INTO t USING s ON (t.id = s.id) WHEN MATCHED THEN UPDATE SET a = 1"
            ),
            (C::Write, None),
            "{d:?}"
        );
    }
    assert_eq!(
        one(MY, "ALTER TABLE t TRUNCATE PARTITION p0"),
        (C::Ddl, Some(R::Truncate))
    );
    assert_eq!(
        one(MY, "ALTER TABLE t DROP PARTITION p0"),
        (C::Ddl, Some(R::AlterDrop))
    );
    assert_eq!(
        one(ORA, "ALTER TABLE t TRUNCATE PARTITION p0"),
        (C::Ddl, Some(R::Truncate))
    );
    prod_confirms(MY, "ALTER TABLE t TRUNCATE PARTITION p0");
}

// 11 --------------------------------------------------------------------------------------

#[test]
fn more_secrets_are_redacted() {
    let cases: Vec<(ScriptDialect, &str, &str)> = vec![
        (
            MY,
            "CHANGE REPLICATION SOURCE TO SOURCE_PASSWORD='s3cret', SOURCE_HOST='h'",
            "CHANGE REPLICATION SOURCE TO SOURCE_PASSWORD='***', SOURCE_HOST='h'",
        ),
        (
            MY,
            "CHANGE MASTER TO MASTER_PASSWORD = 's3cret'",
            "CHANGE MASTER TO MASTER_PASSWORD = '***'",
        ),
        (
            MY,
            "SET PASSWORD = 'new' REPLACE 'old'",
            "SET PASSWORD = '***' REPLACE '***'",
        ),
        (
            MY,
            "ALTER USER u IDENTIFIED BY 'new' REPLACE 'old'",
            "ALTER USER u IDENTIFIED BY '***' REPLACE '***'",
        ),
        (
            MS,
            "CREATE CREDENTIAL c WITH IDENTITY = 'u', SECRET = 'topsecret'",
            "CREATE CREDENTIAL c WITH IDENTITY = 'u', SECRET = '***'",
        ),
        (
            MS,
            "EXEC sp_addlinkedsrvlogin @rmtsrvname = 's', @rmtuser = 'u', @rmtpassword = 'pw'",
            "EXEC sp_addlinkedsrvlogin @rmtsrvname = 's', @rmtuser = 'u', @rmtpassword = '***'",
        ),
        (
            MS,
            "EXEC sp_password @old = 'a', @new = 'b'",
            "EXEC sp_password @old = '***', @new = '***'",
        ),
        (
            MS,
            "EXEC sp_addlogin 'u', 'pw'",
            "EXEC sp_addlogin 'u', '***'",
        ),
        (
            MS,
            "EXEC sp_addlogin 'u', 'pw', 'db'",
            "EXEC sp_addlogin 'u', '***', 'db'",
        ),
        (
            MS,
            "EXEC sp_password 'old', 'new'",
            "EXEC sp_password '***', '***'",
        ),
        (
            MS,
            "EXEC sp_setapprole 'r', @password = 'p'",
            "EXEC sp_setapprole 'r', @password = '***'",
        ),
        (
            ORA,
            "BEGIN dbms_scheduler.create_credential(username => 'u', password => 'pw'); END;",
            "BEGIN dbms_scheduler.create_credential(username => 'u', password => '***'); END;",
        ),
        (ORA, "connect scott/tiger@db", "connect scott/'***'@db"),
        (ORA, "CONNECT scott/tiger", "CONNECT scott/'***'"),
        (
            ScriptDialect::RedisCli,
            "SENTINEL SET mymaster auth-pass hunter2",
            "SENTINEL SET mymaster auth-pass '***'",
        ),
        (
            ScriptDialect::RedisCli,
            "sentinel set m auth-user bob auth-pass pw",
            "sentinel set m auth-user bob auth-pass '***'",
        ),
        (
            MONGO,
            "db.runCommand({createUser: 'u', pwd: 'x', ldapQueryPassword: 'y'})",
            "db.runCommand({createUser: 'u', pwd: '***', ldapQueryPassword: '***'})",
        ),
        (MONGO, "db.c.find({passwd: 1})", "db.c.find({passwd: 1})"),
        (
            PG,
            "SELECT dblink_connect('postgres://u:p@ss@h/db')",
            "SELECT dblink_connect('postgres://u:***@h/db')",
        ),
        (
            PG,
            "SELECT 'mongodb://u:p%40ss:w@h:1/x?authSource=a@b'",
            "SELECT 'mongodb://u:***@h:1/x?authSource=a@b'",
        ),
    ];
    for (d, input, want) in cases {
        assert_eq!(redact(d, input), want, "{d:?} {input}");
    }
}

#[test]
fn redis_secret_forms_still_covered() {
    let r = ScriptDialect::RedisCli;
    assert_eq!(redact(r, "AUTH pw"), "AUTH '***'");
    assert_eq!(
        redact(r, "CONFIG SET requirepass x"),
        "CONFIG SET requirepass '***'"
    );
    assert_eq!(
        redact(r, "CONFIG SET masterauth x"),
        "CONFIG SET masterauth '***'"
    );
    assert_eq!(
        redact(r, "ACL SETUSER u >p #h"),
        "ACL SETUSER u '***' '***'"
    );
    assert_eq!(
        redact(r, "MIGRATE h 1 k 0 1 AUTH2 u p"),
        "MIGRATE h 1 k 0 1 AUTH2 '***' '***'"
    );
    assert_eq!(
        redact(r, "MIGRATE h 1 k 0 1 AUTH p"),
        "MIGRATE h 1 k 0 1 AUTH '***'"
    );
}
