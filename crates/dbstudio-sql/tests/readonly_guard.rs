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
