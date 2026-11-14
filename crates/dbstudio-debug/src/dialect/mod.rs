//! `DialectKit` implementations, one per dialect family (package D).

use dbstudio_core::{DbKind, Error, Result, ScriptDialect};

use crate::{DialectKit, DialectProfile, EvalFrame};

pub mod clickhouse;
pub mod cql;
pub mod mongo;
pub mod mssql;
pub mod mysql;
pub mod oracle;
pub mod pg;
pub mod redis;

/// The kit of the dialect and server flavour of `profile`.
pub fn kit_for(profile: &DialectProfile) -> Box<dyn DialectKit> {
    match profile.dialect {
        ScriptDialect::Postgres => Box::new(pg::PgKit {
            cockroach: profile.kind == DbKind::CockroachDb
                || profile.flavor.as_deref() == Some("cockroachdb"),
        }),
        ScriptDialect::MySql { mariadb, .. } => Box::new(mysql::MySqlKit {
            tidb: profile.kind == DbKind::TiDb || profile.flavor.as_deref() == Some("tidb"),
            old_isolation_var: mariadb || mysql::before_8(profile.version.as_deref()),
        }),
        ScriptDialect::MsSql => Box::new(mssql::MsSqlKit),
        ScriptDialect::Oracle => Box::new(oracle::OracleKit),
        ScriptDialect::MongoShell => Box::new(mongo::MongoKit),
        ScriptDialect::RedisCli => Box::new(redis::RedisKit),
        ScriptDialect::Cql => Box::new(cql::CqlKit),
        ScriptDialect::ClickHouse => Box::new(clickhouse::ClickHouseKit),
        ScriptDialect::Opaque => Box::new(mongo::MongoKit),
    }
}

/// Frame that does nothing (databases without a read-only transaction).
pub(crate) fn no_frame() -> EvalFrame {
    EvalFrame {
        before: vec![],
        after: vec![],
        skip: None,
    }
}

/// Frame of a failed transaction: no evaluation is possible.
pub(crate) fn failed_frame() -> EvalFrame {
    EvalFrame {
        before: vec![],
        after: vec![],
        skip: Some("transaction aborted: only ROLLBACK works".into()),
    }
}

pub(crate) fn frame(before: &[&str], after: &[&str]) -> EvalFrame {
    EvalFrame {
        before: before.iter().map(|s| s.to_string()).collect(),
        after: after.iter().map(|s| s.to_string()).collect(),
        skip: None,
    }
}

/// `SELECT CASE WHEN (<cond>) THEN 1 ELSE 0 END<suffix>`.
pub(crate) fn case_condition(cond: &str, suffix: &str) -> String {
    format!(
        "SELECT CASE WHEN ({}) THEN 1 ELSE 0 END{suffix}",
        cond.trim().trim_end_matches(';')
    )
}

/// Text of an Evaluate / Watch expression for `d`: SQL dialects wrap a bare expression in
/// `SELECT`, command dialects (Mongo, Redis, CQL) take the text as a command.
pub fn eval_text(d: ScriptDialect, expr: &str) -> String {
    let e = expr.trim().trim_end_matches(';').trim();
    match d {
        ScriptDialect::MongoShell | ScriptDialect::RedisCli | ScriptDialect::Opaque => {
            e.to_string()
        }
        ScriptDialect::Cql => e.to_string(),
        _ => {
            let first = e.split_whitespace().next().unwrap_or("").to_uppercase();
            // statements are passed as they are, so that the guard refuses a write by name
            // instead of the server meeting `SELECT DELETE ...`
            if matches!(
                first.as_str(),
                "SELECT"
                    | "WITH"
                    | "SHOW"
                    | "EXPLAIN"
                    | "VALUES"
                    | "TABLE"
                    | "DESCRIBE"
                    | "DESC"
                    | "INSERT"
                    | "UPDATE"
                    | "DELETE"
                    | "DROP"
                    | "TRUNCATE"
                    | "ALTER"
                    | "CREATE"
                    | "GRANT"
                    | "REVOKE"
                    | "MERGE"
                    | "CALL"
                    | "EXEC"
                    | "EXECUTE"
                    | "SET"
                    | "USE"
                    | "COPY"
                    | "REPLACE"
                    | "BEGIN"
                    | "COMMIT"
                    | "ROLLBACK"
            ) {
                e.to_string()
            } else if d == ScriptDialect::Oracle && !e.to_uppercase().contains(" FROM ") {
                format!("SELECT {e} FROM dual")
            } else {
                format!("SELECT {e}")
            }
        }
    }
}

/// SQL literal for what the user typed in "Set Value": numbers, `NULL`, booleans and quoted text
/// pass through, anything else becomes a string literal.
pub fn literal(value_text: &str) -> String {
    let t = value_text.trim();
    let numeric = !t.is_empty()
        && t.parse::<f64>().is_ok()
        && t.chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '-' | '+' | '.' | 'e' | 'E'));
    if numeric
        || t.eq_ignore_ascii_case("null")
        || t.eq_ignore_ascii_case("true")
        || t.eq_ignore_ascii_case("false")
        || is_single_quoted_literal(t)
    {
        t.to_string()
    } else {
        format!("'{}'", t.replace('\'', "''"))
    }
}

/// `'...'` that is one string literal: inside, a quote only appears doubled and there is no
/// backslash (MySQL would read it as an escape). `'a'; DELETE ...; SELECT 'b'` is not one.
fn is_single_quoted_literal(t: &str) -> bool {
    if t.len() < 2 || !t.starts_with('\'') || !t.ends_with('\'') {
        return false;
    }
    let inner: Vec<char> = t[1..t.len() - 1].chars().collect();
    let mut i = 0;
    while i < inner.len() {
        match inner[i] {
            '\\' => return false,
            '\'' => {
                if inner.get(i + 1) == Some(&'\'') {
                    i += 1;
                } else {
                    return false;
                }
            }
            _ => {}
        }
        i += 1;
    }
    true
}

/// `name` is a safe setting / variable name (`work_mem`, `app.x`).
pub(crate) fn safe_name(name: &str) -> Result<&str> {
    if !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
    {
        Ok(name)
    } else {
        Err(Error::Invalid(format!("not a settable name: {name}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals_and_eval_text() {
        assert_eq!(literal("5"), "5");
        assert_eq!(literal(" -1.5e3 "), "-1.5e3");
        assert_eq!(literal("NULL"), "NULL");
        assert_eq!(literal("it's"), "'it''s'");
        assert_eq!(literal("'x'"), "'x'");
        assert_eq!(literal("'it''s'"), "'it''s'");
        // Not one literal: escaped as a whole, the statements inside never run.
        assert_eq!(
            literal("'a'; DELETE FROM t; SELECT 'b'"),
            "'''a''; DELETE FROM t; SELECT ''b'''"
        );
        assert_eq!(literal("'a\\'"), "'''a\\'''");
        assert_eq!(literal("1abc"), "'1abc'");
        assert_eq!(eval_text(ScriptDialect::Postgres, "a + 1;"), "SELECT a + 1");
        assert_eq!(eval_text(ScriptDialect::Postgres, "select 1"), "select 1");
        assert_eq!(
            eval_text(ScriptDialect::Postgres, "DELETE FROM t"),
            "DELETE FROM t"
        );
        assert_eq!(
            eval_text(ScriptDialect::Oracle, "sysdate"),
            "SELECT sysdate FROM dual"
        );
        assert_eq!(eval_text(ScriptDialect::RedisCli, "GET a"), "GET a");
        assert_eq!(
            case_condition("i = 3;", ""),
            "SELECT CASE WHEN (i = 3) THEN 1 ELSE 0 END"
        );
    }
}
