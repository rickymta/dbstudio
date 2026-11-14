//! Script debugger kit for PostgreSQL, YugabyteDB and CockroachDB (package D).

use dbstudio_core::{Result, TxStatus};

use super::{case_condition, failed_frame, frame, literal, safe_name};
use crate::program::{DebugProgram, wrap_single};
use crate::{DialectKit, EvalFrame, Probe, ScriptVar, Step};
use dbstudio_core::ScriptDialect;

/// PostgreSQL family kit.
#[derive(Debug, Clone, Copy)]
pub struct PgKit {
    /// CockroachDB lacks `pg_my_temp_schema()`, `txid_current_if_assigned()` and the session
    /// part of `pg_settings`.
    pub cockroach: bool,
}

impl DialectKit for PgKit {
    fn wrap_single(&self, step_text: &str) -> String {
        wrap_single(ScriptDialect::Postgres, step_text)
    }

    fn eval_frame(&self, tx: TxStatus) -> EvalFrame {
        match tx {
            TxStatus::Failed => failed_frame(),
            TxStatus::Active => frame(
                &["SAVEPOINT __dbg"],
                &["ROLLBACK TO SAVEPOINT __dbg", "RELEASE SAVEPOINT __dbg"],
            ),
            _ => frame(&["BEGIN READ ONLY"], &["ROLLBACK"]),
        }
    }

    fn condition_query(&self, cond: &str) -> String {
        case_condition(cond, "")
    }

    fn probes(&self) -> Vec<Probe> {
        let session = if self.cockroach {
            "SELECT current_database() AS database, current_schema() AS schema, current_user AS \"user\", current_setting('transaction_isolation') AS isolation"
        } else {
            "SELECT current_database() AS database, current_schema() AS schema, current_user AS \"user\", pg_backend_pid() AS pid, current_setting('transaction_isolation') AS isolation, txid_current_if_assigned() AS txid"
        };
        let mut probes = vec![Probe {
            group: "Session".into(),
            sql: session.into(),
            always: true,
        }];
        if !self.cockroach {
            probes.push(Probe {
                group: "Settings".into(),
                sql: "SELECT name, setting, unit FROM pg_settings WHERE source = 'session' ORDER BY name"
                    .into(),
                always: false,
            });
            probes.push(Probe {
                group: "Temporary tables".into(),
                sql: "SELECT c.relname, c.reltuples::bigint AS rows FROM pg_class c WHERE c.relnamespace = pg_my_temp_schema() AND c.relkind IN ('r','p') ORDER BY 1"
                    .into(),
                always: false,
            });
        }
        probes
    }

    fn script_variables(&self, _program: &DebugProgram) -> Vec<ScriptVar> {
        // psql meta commands (`\set`) are not supported by the editor: no script variables.
        vec![]
    }

    fn set_variable_sql(&self, var: &ScriptVar, value_text: &str) -> Result<String> {
        Ok(format!(
            "SET {} = {}",
            safe_name(&var.name)?,
            literal(value_text)
        ))
    }

    fn preserves_counters(&self, _next_step: &Step) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_follow_the_transaction_state() {
        let k = PgKit { cockroach: false };
        assert_eq!(k.eval_frame(TxStatus::Idle).before, ["BEGIN READ ONLY"]);
        assert_eq!(k.eval_frame(TxStatus::Idle).after, ["ROLLBACK"]);
        let a = k.eval_frame(TxStatus::Active);
        assert_eq!(a.before, ["SAVEPOINT __dbg"]);
        assert_eq!(
            a.after,
            ["ROLLBACK TO SAVEPOINT __dbg", "RELEASE SAVEPOINT __dbg"]
        );
        assert!(k.eval_frame(TxStatus::Failed).skip.is_some());
    }

    #[test]
    fn settings_use_set() {
        let k = PgKit { cockroach: false };
        let v = ScriptVar {
            name: "work_mem".into(),
            declared_type: None,
            line: 0,
        };
        assert_eq!(
            k.set_variable_sql(&v, "64MB").unwrap(),
            "SET work_mem = '64MB'"
        );
        let bad = ScriptVar {
            name: "x; DROP".into(),
            declared_type: None,
            line: 0,
        };
        assert!(k.set_variable_sql(&bad, "1").is_err());
        assert!(PgKit { cockroach: true }.probes().len() < k.probes().len());
    }
}
