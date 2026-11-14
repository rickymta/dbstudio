//! Script debugger kit for MySQL/MariaDB/TiDB (package D).

use dbstudio_core::{Error, Result, ScriptDialect, TxStatus};

use super::{case_condition, failed_frame, frame, literal};
use crate::program::{DebugProgram, wrap_single};
use crate::{DialectKit, EvalFrame, Probe, ScriptVar, Step};

/// MySQL family kit.
#[derive(Debug, Clone, Copy)]
pub struct MySqlKit {
    /// TiDB has no `READ ONLY` transaction: `BEGIN ... ROLLBACK`.
    pub tidb: bool,
    /// `@@tx_isolation` instead of `@@transaction_isolation` (5.7, MariaDB).
    pub old_isolation_var: bool,
}

/// MySQL version text older than 8.0 (`5.7.44`).
pub fn before_8(version: Option<&str>) -> bool {
    version
        .and_then(|v| v.split('.').next())
        .and_then(|m| m.trim().parse::<u32>().ok())
        .is_some_and(|major| major < 8)
}

/// Words that make the next step depend on the counters a probe would reset.
const COUNTER_WORDS: &[&str] = &[
    "ROW_COUNT",
    "FOUND_ROWS",
    "LAST_INSERT_ID",
    "@@WARNING_COUNT",
    "@@ERROR_COUNT",
];

/// `SELECT @a AS \`@a\`, ...` for the script variables.
pub fn vars_query(names: &[String]) -> String {
    let cols: Vec<String> = names.iter().map(|n| format!("{n} AS `{n}`")).collect();
    format!("SELECT {}", cols.join(", "))
}

impl DialectKit for MySqlKit {
    fn wrap_single(&self, step_text: &str) -> String {
        wrap_single(ScriptDialect::mysql(false, true), step_text)
    }

    fn eval_frame(&self, tx: TxStatus) -> EvalFrame {
        match tx {
            TxStatus::Failed => failed_frame(),
            TxStatus::Active => frame(
                &["SAVEPOINT __dbg"],
                &["ROLLBACK TO SAVEPOINT __dbg", "RELEASE SAVEPOINT __dbg"],
            ),
            _ if self.tidb => frame(&["BEGIN"], &["ROLLBACK"]),
            _ => frame(&["START TRANSACTION READ ONLY"], &["ROLLBACK"]),
        }
    }

    fn condition_query(&self, cond: &str) -> String {
        case_condition(cond, "")
    }

    fn probes(&self) -> Vec<Probe> {
        let iso = if self.old_isolation_var || self.tidb {
            "@@tx_isolation"
        } else {
            "@@transaction_isolation"
        };
        vec![Probe {
            group: "Session".into(),
            sql: format!(
                "SELECT DATABASE() AS db, CONNECTION_ID() AS connection_id, @@autocommit AS autocommit, {iso} AS isolation"
            ),
            always: true,
        }]
    }

    fn script_variables(&self, program: &DebugProgram) -> Vec<ScriptVar> {
        program
            .at_variables()
            .into_iter()
            .map(|(name, line)| ScriptVar {
                name,
                declared_type: None,
                line,
            })
            .collect()
    }

    fn set_variable_sql(&self, var: &ScriptVar, value_text: &str) -> Result<String> {
        let n = var.name.strip_prefix('@').unwrap_or(&var.name);
        if n.is_empty()
            || !n
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | '$'))
        {
            return Err(Error::Invalid(format!("not a user variable: {}", var.name)));
        }
        Ok(format!("SET @{n} = {}", literal(value_text)))
    }

    fn preserves_counters(&self, next_step: &Step) -> bool {
        let upper = next_step.text.to_uppercase();
        COUNTER_WORDS.iter().any(|w| upper.contains(w))
            || upper.contains("SHOW WARNINGS")
            || upper.contains("SHOW ERRORS")
            || upper.contains("GET DIAGNOSTICS")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(text: &str) -> Step {
        DebugProgram::build(ScriptDialect::mysql(false, true), text, None)
            .unwrap()
            .steps
            .remove(0)
    }

    fn kit() -> MySqlKit {
        MySqlKit {
            tidb: false,
            old_isolation_var: false,
        }
    }

    #[test]
    fn counter_rule() {
        let k = kit();
        for t in [
            "SELECT ROW_COUNT()",
            "SELECT found_rows()",
            "SELECT LAST_INSERT_ID()",
            "SELECT @@warning_count",
            "SHOW WARNINGS",
            "GET DIAGNOSTICS @n = ROW_COUNT",
        ] {
            assert!(k.preserves_counters(&step(t)), "{t}");
        }
        assert!(!k.preserves_counters(&step("SELECT 1")));
    }

    #[test]
    fn frames_and_variables() {
        assert_eq!(
            kit().eval_frame(TxStatus::Idle).before,
            ["START TRANSACTION READ ONLY"]
        );
        let tidb = MySqlKit {
            tidb: true,
            old_isolation_var: true,
        };
        assert_eq!(tidb.eval_frame(TxStatus::Idle).before, ["BEGIN"]);
        assert_eq!(
            kit().eval_frame(TxStatus::Active).before,
            ["SAVEPOINT __dbg"]
        );
        let v = ScriptVar {
            name: "@x".into(),
            declared_type: None,
            line: 1,
        };
        assert_eq!(kit().set_variable_sql(&v, "5").unwrap(), "SET @x = 5");
        assert_eq!(
            vars_query(&["@a".into(), "@b".into()]),
            "SELECT @a AS `@a`, @b AS `@b`"
        );
        assert!(before_8(Some("5.7.44")) && !before_8(Some("8.4.0")) && !before_8(None));
    }
}
