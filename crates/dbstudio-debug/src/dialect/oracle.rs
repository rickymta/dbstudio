//! Script debugger kit for Oracle (package D).

use dbstudio_core::{Error, Result, TxStatus};

use super::{case_condition, frame};
use crate::program::DebugProgram;
use crate::{DialectKit, EvalFrame, Probe, ScriptVar, Step};

/// Oracle kit.
#[derive(Debug, Clone, Copy)]
pub struct OracleKit;

impl DialectKit for OracleKit {
    fn wrap_single(&self, step_text: &str) -> String {
        step_text.to_string()
    }

    fn eval_frame(&self, tx: TxStatus) -> EvalFrame {
        match tx {
            TxStatus::Active => frame(&["SAVEPOINT dbg"], &["ROLLBACK TO dbg"]),
            _ => frame(&["SET TRANSACTION READ ONLY"], &["ROLLBACK"]),
        }
    }

    fn condition_query(&self, cond: &str) -> String {
        case_condition(cond, " FROM dual")
    }

    fn probes(&self) -> Vec<Probe> {
        vec![
            Probe {
                group: "Session".into(),
                sql: "SELECT SYS_CONTEXT('USERENV','CURRENT_SCHEMA') AS current_schema, SYS_CONTEXT('USERENV','SID') AS sid, DBMS_TRANSACTION.LOCAL_TRANSACTION_ID AS tx_id FROM dual".into(),
                always: true,
            },
            Probe {
                group: "Temporary tables".into(),
                sql: "SELECT table_name FROM user_private_temp_tables ORDER BY 1".into(),
                always: false,
            },
        ]
    }

    fn script_variables(&self, _program: &DebugProgram) -> Vec<ScriptVar> {
        vec![]
    }

    fn set_variable_sql(&self, var: &ScriptVar, _value_text: &str) -> Result<String> {
        Err(Error::Unsupported(format!(
            "{}: Oracle script variables are bind parameters",
            var.name
        )))
    }

    fn preserves_counters(&self, _next_step: &Step) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn condition_reads_dual() {
        assert_eq!(
            OracleKit.condition_query("x > 1"),
            "SELECT CASE WHEN (x > 1) THEN 1 ELSE 0 END FROM dual"
        );
        assert_eq!(
            OracleKit.eval_frame(TxStatus::Active).after,
            ["ROLLBACK TO dbg"]
        );
    }
}
