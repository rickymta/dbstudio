//! End to end: analyse + `guard::check` for the statements that can lift read-only mode, hide a
//! write in a SELECT, or change how the text is lexed.

use dbstudio_core::{Error, ScriptDialect};
use dbstudio_sql::{Policy, Verdict, analyze, check, split};

const PG: ScriptDialect = ScriptDialect::Postgres;
const MY: ScriptDialect = ScriptDialect::MySql {
    mariadb: false,
    backslash_escapes: true,
};
const MY_NB: ScriptDialect = ScriptDialect::MySql {
    mariadb: false,
    backslash_escapes: false,
};

fn ro(d: ScriptDialect, sql: &str) -> Verdict {
    let policy = Policy {
        read_only: true,
        production: false,
        confirm_text: "db".into(),
    };
    check(&policy, &analyze(d, sql))
}

fn denied(d: ScriptDialect, sql: &str) {
    match ro(d, sql) {
        Verdict::Deny(Error::ReadOnlyViolation { .. }) => {}
        other => panic!("{d:?} {sql:?} should be denied in read-only mode, got {other:?}"),
    }
}

fn allowed(d: ScriptDialect, sql: &str) {
    match ro(d, sql) {
        Verdict::Allow => {}
        other => panic!("{d:?} {sql:?} should be allowed in read-only mode, got {other:?}"),
    }
}

#[test]
fn postgres_statements_that_lift_read_only_are_denied() {
    for sql in [
        "SET default_transaction_read_only = off",
        "set session default_transaction_read_only to false",
        "SET SESSION default_transaction_read_only = 0",
        "SET transaction_read_only = off",
        "SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE",
        "BEGIN READ WRITE",
        "BEGIN TRANSACTION ISOLATION LEVEL SERIALIZABLE, READ WRITE",
        "START TRANSACTION READ WRITE",
        "SET TRANSACTION READ WRITE",
        "SELECT set_config('default_transaction_read_only', 'off', false)",
        "select set_config('transaction_read_only','off',true)",
        "SET LOCAL default_transaction_read_only = off",
    ] {
        denied(PG, sql);
    }
}

#[test]
fn harmless_session_and_transaction_statements_stay_allowed() {
    for sql in [
        "SET search_path = a",
        "SET statement_timeout = 1000",
        "BEGIN",
        "BEGIN READ ONLY",
        "START TRANSACTION READ ONLY",
        "SET TRANSACTION READ ONLY",
        "COMMIT",
        "ROLLBACK",
        "SAVEPOINT a",
        "SHOW default_transaction_read_only",
        "SELECT 1",
        "SELECT * FROM t WHERE a = 1",
    ] {
        allowed(PG, sql);
    }
    allowed(MY, "START TRANSACTION READ ONLY");
    allowed(MY, "SET autocommit = 0");
    allowed(MY, "SET NAMES utf8mb4");
}

#[test]
fn mysql_statements_that_lift_read_only_are_denied() {
    for sql in [
        "SET transaction_read_only = 0",
        "SET SESSION transaction_read_only = OFF",
        "SET @@session.transaction_read_only = 0",
        "SET @@tx_read_only = 0",
        "SET SESSION tx_read_only = 0",
        "SET SESSION TRANSACTION READ WRITE",
        "SET TRANSACTION READ WRITE",
        "START TRANSACTION READ WRITE",
        "SET sql_mode = ''",
        "SET SESSION sql_mode = 'NO_BACKSLASH_ESCAPES'",
        "SET @@sql_mode = 'ANSI_QUOTES'",
        "SET GLOBAL read_only = 0",
    ] {
        denied(MY, sql);
    }
}

#[test]
fn selects_with_side_effects_are_not_reads() {
    for sql in [
        "SELECT nextval('s')",
        "SELECT setval('s', 10)",
        "SELECT pg_terminate_backend(123)",
        "SELECT pg_cancel_backend(123)",
        "SELECT lo_import('/etc/passwd')",
        "SELECT lo_export(1, '/tmp/x')",
        "SELECT dblink_exec('dbname=x', 'drop table t')",
        "SELECT pg_reload_conf()",
        "SELECT * FROM t FOR UPDATE",
        "SELECT * FROM t FOR SHARE",
        "SELECT * FROM t FOR NO KEY UPDATE",
        "SELECT * FROM t FOR KEY SHARE",
        "SELECT * INTO newt FROM t",
        "WITH a AS (SELECT 1) SELECT nextval('s') FROM a",
        "SELECT * FROM (SELECT nextval('s')) x",
    ] {
        denied(PG, sql);
    }
    for sql in [
        "SELECT * FROM t INTO OUTFILE '/tmp/x'",
        "SELECT * FROM t INTO DUMPFILE '/tmp/x'",
        "SELECT a INTO @v FROM t",
        "SELECT * FROM t LOCK IN SHARE MODE",
        "SELECT * FROM t FOR UPDATE",
        "SELECT * FROM t FOR SHARE",
    ] {
        denied(MY, sql);
    }
    denied(ScriptDialect::MsSql, "SELECT * INTO #t FROM t");
    denied(ScriptDialect::Oracle, "SELECT * FROM t FOR UPDATE");
    // Plain reads keep working, including words that look similar.
    allowed(PG, "SELECT 'nextval(1)', \"setval\" FROM t -- nextval()");
    allowed(PG, "SELECT * FROM t WHERE a = 'for update'");
    allowed(PG, "SELECT max(a) FROM t");
}

#[test]
fn unknown_and_procedural_statements_count_as_writes() {
    for sql in [
        "FROBNICATE x",
        "CALL p()",
        "DO $$ BEGIN NULL; END $$",
        "EXECUTE stmt",
    ] {
        denied(PG, sql);
    }
    denied(MY, "CALL p()");
    denied(ScriptDialect::MsSql, "EXEC sp_who");
    denied(ScriptDialect::Oracle, "BEGIN NULL; END;");
    denied(ScriptDialect::Opaque, "anything");
}

fn prod(d: ScriptDialect, sql: &str) -> Verdict {
    let policy = Policy {
        read_only: false,
        production: true,
        confirm_text: "db".into(),
    };
    check(&policy, &analyze(d, sql))
}

#[test]
fn production_flags_drop_and_truncate_hidden_in_unknown_or_procedural_statements() {
    for (d, sql) in [
        (PG, "DO $$ BEGIN DROP TABLE x; END $$"),
        (PG, "DO $$ BEGIN EXECUTE 'truncate table x'; END $$"),
        (PG, "FROBNICATE DROP TABLE x"),
        (MY, "CALL p(); "),
        (ScriptDialect::MsSql, "EXEC('DROP TABLE x')"),
        (
            ScriptDialect::Oracle,
            "BEGIN EXECUTE IMMEDIATE 'DROP TABLE x'; END;",
        ),
    ] {
        let confirm = matches!(prod(d, sql), Verdict::Confirm(_));
        // `CALL p()` has no keyword: it must run, everything else needs confirmation.
        assert_eq!(confirm, !sql.starts_with("CALL"), "{d:?} {sql:?}");
    }
    // Keywords inside comments never count.
    assert!(matches!(
        prod(PG, "SELECT 1 -- drop table x\n"),
        Verdict::Allow
    ));
}

#[test]
fn mysql_backslash_escapes_follow_the_dialect_flag() {
    let sql = "SELECT 'a\\'; DROP TABLE t; -- '";
    // With backslash escapes the whole text is one SELECT.
    assert_eq!(split(MY, sql).len(), 1);
    let a = analyze(MY, sql);
    assert_eq!(a.statements.len(), 1);
    assert_eq!(a.statements[0].destructive, None);
    assert!(a.problems.is_empty());
    // NO_BACKSLASH_ESCAPES: the quote after the backslash closes the string, DROP is real.
    assert_eq!(split(MY_NB, sql).len(), 2);
    let a = analyze(MY_NB, sql);
    assert_eq!(a.statements.len(), 2);
    assert_eq!(
        a.statements[1].destructive,
        Some(dbstudio_core::DestructiveReason::Drop)
    );
    let policy = Policy {
        read_only: false,
        production: true,
        confirm_text: "db".into(),
    };
    assert!(matches!(check(&policy, &analyze(MY, sql)), Verdict::Allow));
    assert!(matches!(
        check(&policy, &analyze(MY_NB, sql)),
        Verdict::Confirm(_)
    ));
    // mariadb flag does not change the lexing, only backslash_escapes does
    let mariadb = ScriptDialect::MySql {
        mariadb: true,
        backslash_escapes: true,
    };
    assert_eq!(split(mariadb, sql).len(), 1);
}

// ---- Mongo classifier hardening and effective database -------------------------------------

use dbstudio_core::{DestructiveReason as R, StatementClass as C};
use dbstudio_sql::{confirm_text_for, statement_databases};

const MONGO: ScriptDialect = ScriptDialect::MongoShell;

fn mclass(sql: &str) -> (C, Option<R>) {
    let a = analyze(MONGO, sql);
    assert_eq!(a.statements.len(), 1, "{sql}");
    (a.statements[0].class, a.statements[0].destructive)
}

#[test]
fn explain_in_the_chain_does_not_hide_writes() {
    assert_eq!(
        mclass("db.a.deleteMany({}).explain()"),
        (C::Write, Some(R::DeleteManyAll))
    );
    // A chain after a database method is rejected by the parser; the statement stays unknown,
    // keeps the destructive reason of the keyword scan and read-only refuses it.
    let unparsed = analyze(MONGO, "db.dropDatabase().explain()");
    assert!(!unparsed.problems.is_empty());
    assert_eq!(unparsed.statements[0].class, C::Unknown);
    assert_eq!(unparsed.statements[0].destructive, Some(R::DropDatabase));
    denied(MONGO, "db.dropDatabase().explain()");
    assert_eq!(
        mclass("db.a.aggregate([{$out: 'b'}]).explain()").0,
        C::Write
    );
    assert_eq!(
        mclass("db.a.aggregate([{$merge: {into: 'b'}}]).explain()").0,
        C::Write
    );
    assert_eq!(
        mclass("db.a.drop().explain()"),
        (C::Ddl, Some(R::DropCollection))
    );
    assert_eq!(mclass("db.a.insertOne({}).explain()").0, C::Write);
    // reads still become Explain
    for q in [
        "db.a.find({}).explain()",
        "db.a.aggregate([{$match: {}}]).explain('executionStats')",
        "db.a.countDocuments({}).explain()",
        "db.a.distinct('x').explain()",
    ] {
        assert!(
            matches!(mclass(q).0, C::Explain { writes: false, .. }),
            "{q}"
        );
    }
    // and the guard refuses the hidden writes in read-only mode
    denied(MONGO, "db.a.deleteMany({}).explain()");
    denied(MONGO, "db.a.aggregate([{$out:'b'}]).explain()");
    allowed(MONGO, "db.a.find({}).explain()");
}

#[test]
fn empty_filters_of_every_shape_are_destructive() {
    for q in [
        "db.a.deleteMany()",
        "db.a.deleteMany(null)",
        "db.a.deleteMany(undefined)",
        "db.a.deleteMany({})",
        "db.a.remove()",
        "db.a.remove(null)",
        "db.a.remove({})",
    ] {
        assert_eq!(mclass(q), (C::Write, Some(R::DeleteManyAll)), "{q}");
    }
    for q in [
        "db.a.updateMany(null, {$set: {x: 1}})",
        "db.a.updateMany(undefined, {$set: {x: 1}})",
        "db.a.updateMany({}, {$set: {x: 1}})",
        "db.a.updateMany()",
    ] {
        assert_eq!(mclass(q), (C::Write, Some(R::UpdateManyAll)), "{q}");
    }
    assert_eq!(mclass("db.a.deleteMany({x: 1})").1, None);
    assert_eq!(mclass("db.a.remove({x: 1})").1, None);
    assert_eq!(mclass("db.a.updateMany({x: 1}, {$set: {y: 1}})").1, None);
}

#[test]
fn string_form_of_run_command_is_classified_like_the_object_form() {
    for cmd in ["runCommand", "adminCommand"] {
        assert_eq!(
            mclass(&format!("db.{cmd}('dropDatabase')")),
            (C::Ddl, Some(R::DropDatabase))
        );
        assert_eq!(
            mclass(&format!("db.{cmd}(\"drop\")")),
            (C::Ddl, Some(R::DropCollection))
        );
        assert_eq!(
            mclass(&format!("db.{cmd}('dropIndexes')")),
            (C::Ddl, Some(R::AlterDrop))
        );
        assert_eq!(
            mclass(&format!("db.{cmd}('delete')")).1,
            Some(R::DeleteManyAll)
        );
        assert_eq!(
            mclass(&format!("db.{cmd}('update')")).1,
            Some(R::UpdateManyAll)
        );
        assert_eq!(mclass(&format!("db.{cmd}('ping')")), (C::Read, None));
        assert_eq!(
            mclass(&format!("db.{cmd}('shutdown')")),
            (C::Procedural, None)
        );
    }
    assert_eq!(
        mclass("db.runCommand({dropIndexes: 'a', index: '*'})"),
        (C::Ddl, Some(R::AlterDrop))
    );
}

#[test]
fn effective_database_follows_use() {
    let text = "db.a.drop()\nuse shop\ndb.b.drop()\nuse other\ndb.c.deleteMany({})";
    let a = analyze(MONGO, text);
    let dbs = statement_databases(MONGO, text, &a);
    assert_eq!(
        dbs,
        vec![
            None,
            Some("shop".to_string()),
            Some("shop".to_string()),
            Some("other".to_string()),
            Some("other".to_string())
        ]
    );
    let policy = Policy {
        read_only: false,
        production: true,
        confirm_text: "initial".into(),
    };
    let Verdict::Confirm(list) = check(&policy, &a) else {
        panic!()
    };
    assert_eq!(list.len(), 3);
    assert_eq!(
        confirm_text_for("initial", &dbs, &list),
        "initial, shop, other"
    );
    assert_eq!(confirm_text_for("initial", &dbs, &list[1..2]), "shop");
    assert_eq!(confirm_text_for("initial", &dbs, &[]), "initial");
    // `use` with a quoted name
    let t2 = "use 'my-db'\ndb.a.drop()";
    let a2 = analyze(MONGO, t2);
    assert_eq!(
        statement_databases(MONGO, t2, &a2)[1].as_deref(),
        Some("my-db")
    );
    // unparsable scripts: nothing is known
    let t3 = "use x\ndb.a.find({";
    let a3 = analyze(MONGO, t3);
    assert!(
        statement_databases(MONGO, t3, &a3)
            .iter()
            .all(Option::is_none)
    );
}

#[test]
fn effective_database_for_mysql_and_tsql() {
    for (d, text, quoted) in [
        (
            MY,
            "DROP TABLE a; USE `shop`; DROP TABLE b; use other;",
            "`shop`",
        ),
        (
            ScriptDialect::MsSql,
            "DROP TABLE a;\nUSE [shop];\nDROP TABLE b;\nGO\nUSE other",
            "[shop]",
        ),
    ] {
        let a = analyze(d, text);
        let dbs = statement_databases(d, text, &a);
        assert_eq!(dbs.len(), a.statements.len(), "{quoted}");
        assert_eq!(dbs[0], None);
        assert_eq!(dbs[1].as_deref(), Some("shop"));
        assert_eq!(dbs[2].as_deref(), Some("shop"));
        assert_eq!(dbs.last().unwrap().as_deref(), Some("other"));
    }
    // other dialects have no notion of it
    let a = analyze(PG, "select 1");
    assert_eq!(statement_databases(PG, "select 1", &a), vec![None]);
}
