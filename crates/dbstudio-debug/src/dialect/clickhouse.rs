//! Script debugger kit for ClickHouse (package D). No transaction: guard only.

use dbstudio_core::{Result, TxStatus};

use super::{case_condition, literal, no_frame, safe_name};
use crate::program::DebugProgram;
use crate::{DialectKit, EvalFrame, Probe, ScriptVar, Step};

/// ClickHouse kit.
#[derive(Debug, Clone, Copy)]
pub struct ClickHouseKit;

impl DialectKit for ClickHouseKit {
    fn wrap_single(&self, step_text: &str) -> String {
        step_text.to_string()
    }

    fn eval_frame(&self, _tx: TxStatus) -> EvalFrame {
        no_frame()
    }

    fn condition_query(&self, cond: &str) -> String {
        case_condition(cond, "")
    }

    fn probes(&self) -> Vec<Probe> {
        vec![
            Probe {
                group: "Settings".into(),
                sql: "SELECT name, value FROM system.settings WHERE changed ORDER BY name".into(),
                always: false,
            },
            Probe {
                group: "Temporary tables".into(),
                sql: "SELECT name FROM system.tables WHERE is_temporary ORDER BY name".into(),
                always: false,
            },
        ]
    }

    fn script_variables(&self, _program: &DebugProgram) -> Vec<ScriptVar> {
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
