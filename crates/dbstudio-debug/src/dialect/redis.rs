//! Script debugger kit for Redis/Valkey (package D). `MULTI` is tracked by the actor.

use dbstudio_core::{Error, Result, TxStatus};

use super::no_frame;
use crate::program::DebugProgram;
use crate::{DialectKit, EvalFrame, Probe, ScriptVar, Step};

/// Redis kit.
#[derive(Debug, Clone, Copy)]
pub struct RedisKit;

impl DialectKit for RedisKit {
    fn wrap_single(&self, step_text: &str) -> String {
        step_text.to_string()
    }

    fn eval_frame(&self, _tx: TxStatus) -> EvalFrame {
        no_frame()
    }

    fn condition_query(&self, cond: &str) -> String {
        cond.trim().to_string()
    }

    fn probes(&self) -> Vec<Probe> {
        vec![Probe {
            group: "Session".into(),
            sql: "CLIENT INFO".into(),
            always: true,
        }]
    }

    fn script_variables(&self, _program: &DebugProgram) -> Vec<ScriptVar> {
        vec![]
    }

    fn set_variable_sql(&self, var: &ScriptVar, _value_text: &str) -> Result<String> {
        Err(Error::Unsupported(format!(
            "{}: Redis has no script variables",
            var.name
        )))
    }

    fn preserves_counters(&self, _next_step: &Step) -> bool {
        false
    }
}
