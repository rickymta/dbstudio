//! Script debugger kit for SQL Server (package D). Variables live in the T-SQL shim.

use dbstudio_core::{Error, Result, TxStatus};

use super::{case_condition, failed_frame, frame};
use crate::program::DebugProgram;
use crate::{DialectKit, EvalFrame, Probe, ScriptVar, Step};

/// SQL Server kit.
#[derive(Debug, Clone, Copy)]
pub struct MsSqlKit;

impl DialectKit for MsSqlKit {
    fn wrap_single(&self, step_text: &str) -> String {
        step_text.to_string()
    }

    fn eval_frame(&self, tx: TxStatus) -> EvalFrame {
        match tx {
            TxStatus::Failed => failed_frame(),
            TxStatus::Active => frame(
                &["SAVE TRAN __dbg"],
                &["IF XACT_STATE() = 1 ROLLBACK TRAN __dbg"],
            ),
            _ => frame(&["BEGIN TRAN"], &["ROLLBACK"]),
        }
    }

    fn condition_query(&self, cond: &str) -> String {
        case_condition(cond, "")
    }

    fn probes(&self) -> Vec<Probe> {
        vec![Probe {
            group: "Session".into(),
            sql: "SELECT DB_NAME() AS db, @@SPID AS spid, @@TRANCOUNT AS trancount, XACT_STATE() AS xact_state, (SELECT transaction_isolation_level FROM sys.dm_exec_sessions WHERE session_id = @@SPID) AS isolation".into(),
            always: true,
        }]
    }

    fn script_variables(&self, program: &DebugProgram) -> Vec<ScriptVar> {
        crate::tsql_shim::declared_vars(program)
    }

    fn set_variable_sql(&self, var: &ScriptVar, _value_text: &str) -> Result<String> {
        Err(Error::Unsupported(format!(
            "{} lives in the T-SQL shim, not on the server",
            var.name
        )))
    }

    fn preserves_counters(&self, _next_step: &Step) -> bool {
        // batches using @@ROWCOUNT are one step, so a probe between batches cannot disturb them
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_uses_save_tran_inside_a_transaction() {
        let k = MsSqlKit;
        assert_eq!(k.eval_frame(TxStatus::Idle).before, ["BEGIN TRAN"]);
        let a = k.eval_frame(TxStatus::Active);
        assert_eq!(a.before, ["SAVE TRAN __dbg"]);
        assert_eq!(a.after, ["IF XACT_STATE() = 1 ROLLBACK TRAN __dbg"]);
        assert!(k.eval_frame(TxStatus::Failed).skip.is_some());
    }
}
