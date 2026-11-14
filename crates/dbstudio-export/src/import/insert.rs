//! `INSERT` statements for the import, built in the dialect of the target.

use dbstudio_core::{EditValue, Error, OnConflict, Result, ScriptDialect};
use dbstudio_sql::Dialect;

use crate::writers::sql_literal;

/// SQL Server takes at most 1000 rows in one `VALUES` list; other drivers follow the same cap so
/// that one statement stays a reasonable size.
pub(crate) const MAX_STATEMENT_ROWS: usize = 1000;

/// Statements marking a savepoint inside the open transaction.
#[derive(Debug, Clone)]
pub(crate) struct Savepoint {
    pub create: String,
    pub release: Option<String>,
    pub rollback: String,
}

/// Savepoint syntax of the dialect (`None` where the import cannot nest rollbacks).
pub(crate) fn savepoint(dialect: ScriptDialect, name: &str) -> Option<Savepoint> {
    match dialect {
        ScriptDialect::Postgres | ScriptDialect::MySql { .. } => Some(Savepoint {
            create: format!("SAVEPOINT {name}"),
            release: Some(format!("RELEASE SAVEPOINT {name}")),
            rollback: format!("ROLLBACK TO SAVEPOINT {name}"),
        }),
        ScriptDialect::Oracle => Some(Savepoint {
            create: format!("SAVEPOINT {name}"),
            release: None,
            rollback: format!("ROLLBACK TO SAVEPOINT {name}"),
        }),
        ScriptDialect::MsSql => Some(Savepoint {
            create: format!("SAVE TRANSACTION {name}"),
            release: None,
            rollback: format!("ROLLBACK TRANSACTION {name}"),
        }),
        _ => None,
    }
}

/// Target table of an insert.
#[derive(Debug, Clone)]
pub(crate) struct Target {
    /// Quoted, qualified table name.
    pub table: String,
    pub columns: Vec<String>,
    /// Key columns used by `ON CONFLICT (...)`.
    pub conflict_columns: Vec<String>,
}

fn value_sql(q: Dialect, dialect: ScriptDialect, v: &EditValue) -> String {
    match v {
        EditValue::Text(s) => q.literal_text(s),
        EditValue::Value(v) => sql_literal(q, dialect, v),
        EditValue::Default => "DEFAULT".into(),
    }
}

/// One statement inserting `rows` (at most [`MAX_STATEMENT_ROWS`]).
pub(crate) fn insert_statement(
    dialect: ScriptDialect,
    target: &Target,
    rows: &[Vec<EditValue>],
    on_conflict: OnConflict,
    returning_inserted: bool,
) -> Result<String> {
    let q = Dialect::new(dialect);
    let cols = target
        .columns
        .iter()
        .map(|c| q.quote_ident(c))
        .collect::<Vec<_>>()
        .join(", ");
    let tuple = |row: &Vec<EditValue>| {
        let vals: Vec<String> = row.iter().map(|v| value_sql(q, dialect, v)).collect();
        format!("({})", vals.join(", "))
    };
    let unsupported = |what: &str| Error::Unsupported(format!("on_conflict_{what}"));
    match dialect {
        ScriptDialect::Oracle => {
            if on_conflict != OnConflict::Error {
                return Err(unsupported("oracle"));
            }
            let mut s = String::from("INSERT ALL");
            for row in rows {
                s.push_str(&format!(
                    " INTO {} ({cols}) VALUES {}",
                    target.table,
                    tuple(row)
                ));
            }
            s.push_str(" SELECT 1 FROM DUAL");
            Ok(s)
        }
        ScriptDialect::MySql { .. } => {
            let values = rows.iter().map(tuple).collect::<Vec<_>>().join(",\n");
            let mut s = format!("INSERT INTO {} ({cols}) VALUES\n{values}", target.table);
            // No `INSERT IGNORE`: it would turn bad values into silent warnings. A duplicate
            // is skipped by "updating" a column to itself.
            let noop = target
                .columns
                .first()
                .map(|c| format!("{0} = {0}", q.quote_ident(c)));
            match on_conflict {
                OnConflict::Error => {}
                OnConflict::Skip => {
                    if let Some(noop) = noop {
                        s.push_str(&format!("\nON DUPLICATE KEY UPDATE {noop}"));
                    }
                }
                OnConflict::Update => {
                    let sets: Vec<String> = target
                        .columns
                        .iter()
                        .filter(|c| !target.conflict_columns.contains(c))
                        .map(|c| format!("{0} = VALUES({0})", q.quote_ident(c)))
                        .collect();
                    if sets.is_empty() {
                        if let Some(noop) = noop {
                            s.push_str(&format!("\nON DUPLICATE KEY UPDATE {noop}"));
                        }
                    } else {
                        s.push_str(&format!("\nON DUPLICATE KEY UPDATE {}", sets.join(", ")));
                    }
                }
            }
            Ok(s)
        }
        ScriptDialect::Postgres => {
            let values = rows.iter().map(tuple).collect::<Vec<_>>().join(",\n");
            let mut s = format!("INSERT INTO {} ({cols}) VALUES\n{values}", target.table);
            match on_conflict {
                OnConflict::Error => {}
                OnConflict::Skip => s.push_str("\nON CONFLICT DO NOTHING"),
                OnConflict::Update => {
                    let keys: Vec<String> = target
                        .conflict_columns
                        .iter()
                        .map(|c| q.quote_ident(c))
                        .collect();
                    if keys.is_empty() {
                        return Err(Error::Other(
                            "updating on conflict needs a primary key or conflict columns".into(),
                        ));
                    }
                    let sets: Vec<String> = target
                        .columns
                        .iter()
                        .filter(|c| !target.conflict_columns.contains(c))
                        .map(|c| format!("{0} = EXCLUDED.{0}", q.quote_ident(c)))
                        .collect();
                    if sets.is_empty() {
                        s.push_str(&format!("\nON CONFLICT ({}) DO NOTHING", keys.join(", ")));
                    } else {
                        s.push_str(&format!(
                            "\nON CONFLICT ({}) DO UPDATE SET {}",
                            keys.join(", "),
                            sets.join(", ")
                        ));
                    }
                    if returning_inserted {
                        // `xmax = 0` tells an inserted row from an updated one.
                        s.push_str("\nRETURNING (xmax = 0)");
                    }
                }
            }
            Ok(s)
        }
        ScriptDialect::Cql => {
            // One row per statement. `INSERT` is an upsert in CQL: `Error` and `Update` both
            // overwrite; `Skip` only inserts rows whose key is free (lightweight transaction).
            if rows.len() != 1 {
                return Err(Error::Invalid("CQL inserts one row per statement".into()));
            }
            let mut s = format!(
                "INSERT INTO {} ({cols}) VALUES {}",
                target.table,
                tuple(&rows[0])
            );
            if on_conflict == OnConflict::Skip {
                s.push_str(" IF NOT EXISTS");
            }
            Ok(s)
        }
        _ => {
            if on_conflict != OnConflict::Error {
                return Err(unsupported("dialect"));
            }
            let values = rows.iter().map(tuple).collect::<Vec<_>>().join(",\n");
            Ok(format!(
                "INSERT INTO {} ({cols}) VALUES\n{values}",
                target.table
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_core::Value;

    fn target() -> Target {
        Target {
            table: "\"s\".\"t\"".into(),
            columns: vec!["id".into(), "name".into()],
            conflict_columns: vec!["id".into()],
        }
    }

    fn rows() -> Vec<Vec<EditValue>> {
        vec![
            vec![EditValue::Text("1".into()), EditValue::Text("it's".into())],
            vec![
                EditValue::Value(Value::Int(2)),
                EditValue::Value(Value::Null),
            ],
            vec![EditValue::Default, EditValue::Text("x".into())],
        ]
    }

    fn stmt(
        d: ScriptDialect,
        t: &Target,
        rows: &[Vec<EditValue>],
        c: OnConflict,
    ) -> Result<String> {
        insert_statement(d, t, rows, c, false)
    }

    #[test]
    fn cql_inserts_one_row_and_skips_with_if_not_exists() {
        let t = Target {
            table: "ks.t".into(),
            columns: vec!["id".into(), "name".into()],
            conflict_columns: vec!["id".into()],
        };
        let one = vec![vec![
            EditValue::Value(Value::Int(1)),
            EditValue::Text("a".into()),
        ]];
        let sql = insert_statement(ScriptDialect::Cql, &t, &one, OnConflict::Error, false).unwrap();
        assert!(sql.starts_with("INSERT INTO ks.t ("), "{sql}");
        assert!(!sql.contains("IF NOT EXISTS"));
        let skip = insert_statement(ScriptDialect::Cql, &t, &one, OnConflict::Skip, false).unwrap();
        assert!(skip.ends_with(" IF NOT EXISTS"), "{skip}");
        let two = vec![one[0].clone(), one[0].clone()];
        assert!(insert_statement(ScriptDialect::Cql, &t, &two, OnConflict::Error, false).is_err());
        // ClickHouse: multi-row insert, no conflict handling.
        let ch = insert_statement(
            ScriptDialect::ClickHouse,
            &t,
            &two,
            OnConflict::Error,
            false,
        )
        .unwrap();
        assert!(ch.contains("VALUES\n("), "{ch}");
        assert!(
            insert_statement(ScriptDialect::ClickHouse, &t, &two, OnConflict::Skip, false).is_err()
        );
    }

    #[test]
    fn postgres_conflict_modes() {
        let pg = ScriptDialect::Postgres;
        let sql = stmt(pg, &target(), &rows(), OnConflict::Error).unwrap();
        assert_eq!(
            sql,
            "INSERT INTO \"s\".\"t\" (\"id\", \"name\") VALUES\n('1', 'it''s'),\n(2, NULL),\n(DEFAULT, 'x')"
        );
        let skip = stmt(pg, &target(), &rows(), OnConflict::Skip).unwrap();
        assert!(skip.ends_with("ON CONFLICT DO NOTHING"));
        let upd = stmt(pg, &target(), &rows(), OnConflict::Update).unwrap();
        assert!(upd.ends_with("ON CONFLICT (\"id\") DO UPDATE SET \"name\" = EXCLUDED.\"name\""));
        let with_returning =
            insert_statement(pg, &target(), &rows(), OnConflict::Update, true).unwrap();
        assert!(with_returning.ends_with("EXCLUDED.\"name\"\nRETURNING (xmax = 0)"));
        let mut no_key = target();
        no_key.conflict_columns.clear();
        assert!(stmt(pg, &no_key, &rows(), OnConflict::Update).is_err());
    }

    #[test]
    fn mysql_conflict_modes() {
        let my = ScriptDialect::mysql(false, true);
        let mut t = target();
        t.table = "`t`".into();
        // Skipping a duplicate is a no-op update, not `INSERT IGNORE` (which hides bad values).
        let skip = stmt(my, &t, &rows(), OnConflict::Skip).unwrap();
        assert!(skip.starts_with("INSERT INTO `t` (`id`, `name`) VALUES"));
        assert!(skip.ends_with("ON DUPLICATE KEY UPDATE `id` = `id`"));
        assert!(!skip.contains("IGNORE"));
        let upd = stmt(my, &t, &rows(), OnConflict::Update).unwrap();
        assert!(upd.ends_with("ON DUPLICATE KEY UPDATE `name` = VALUES(`name`)"));
    }

    #[test]
    fn oracle_and_savepoints() {
        let sql = stmt(
            ScriptDialect::Oracle,
            &target(),
            &rows()[..2],
            OnConflict::Error,
        )
        .unwrap();
        assert!(sql.starts_with("INSERT ALL INTO"));
        assert!(sql.ends_with("SELECT 1 FROM DUAL"));
        assert!(stmt(ScriptDialect::Oracle, &target(), &rows(), OnConflict::Skip).is_err());
        let sp = savepoint(ScriptDialect::Postgres, "s1").unwrap();
        assert_eq!(sp.rollback, "ROLLBACK TO SAVEPOINT s1");
        assert!(
            savepoint(ScriptDialect::MsSql, "s1")
                .unwrap()
                .release
                .is_none()
        );
        assert!(savepoint(ScriptDialect::Opaque, "s1").is_none());
    }
}
