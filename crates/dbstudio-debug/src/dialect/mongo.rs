//! Script debugger kit for MongoDB (package D): read commands only, guard is the protection.

use dbstudio_core::{Error, Result, TxStatus};

use super::no_frame;
use crate::program::DebugProgram;
use crate::{DialectKit, EvalFrame, Probe, ScriptVar, Step};

/// MongoDB kit (also used for opaque dialects).
#[derive(Debug, Clone, Copy)]
pub struct MongoKit;

impl DialectKit for MongoKit {
    fn wrap_single(&self, step_text: &str) -> String {
        step_text.to_string()
    }

    fn eval_frame(&self, _tx: TxStatus) -> EvalFrame {
        no_frame()
    }

    fn condition_query(&self, cond: &str) -> String {
        // the condition is itself a read command: true when it returns a document
        cond.trim().trim_end_matches(';').to_string()
    }

    fn probes(&self) -> Vec<Probe> {
        vec![]
    }

    fn script_variables(&self, _program: &DebugProgram) -> Vec<ScriptVar> {
        vec![]
    }

    fn set_variable_sql(&self, var: &ScriptVar, _value_text: &str) -> Result<String> {
        Err(Error::Unsupported(format!(
            "{}: the shell does not run JavaScript, there are no script variables",
            var.name
        )))
    }

    fn preserves_counters(&self, _next_step: &Step) -> bool {
        false
    }
}
