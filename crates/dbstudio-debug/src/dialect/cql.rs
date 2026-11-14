//! Script debugger kit for Cassandra/ScyllaDB (package D). No transaction: guard only.

use dbstudio_core::{Error, Result, TxStatus};

use super::{literal, no_frame};
use crate::program::DebugProgram;
use crate::{DialectKit, EvalFrame, Probe, ScriptVar, Step};

/// CQL kit.
#[derive(Debug, Clone, Copy)]
pub struct CqlKit;

impl DialectKit for CqlKit {
    fn wrap_single(&self, step_text: &str) -> String {
        step_text.to_string()
    }

    fn eval_frame(&self, _tx: TxStatus) -> EvalFrame {
        no_frame()
    }

    fn condition_query(&self, cond: &str) -> String {
        // CQL has no CASE: the condition is a SELECT, true when it returns a row
        cond.trim().trim_end_matches(';').to_string()
    }

    fn probes(&self) -> Vec<Probe> {
        vec![]
    }

    fn script_variables(&self, _program: &DebugProgram) -> Vec<ScriptVar> {
        vec![]
    }

    fn set_variable_sql(&self, var: &ScriptVar, value_text: &str) -> Result<String> {
        match var.name.to_lowercase().as_str() {
            "consistency" => Ok(format!("CONSISTENCY {}", value_text.trim().to_uppercase())),
            "tracing" => {
                let on = matches!(
                    value_text.trim().to_lowercase().as_str(),
                    "on" | "true" | "1"
                );
                Ok(format!("TRACING {}", if on { "ON" } else { "OFF" }))
            }
            _ => Err(Error::Unsupported(format!(
                "{} cannot be changed ({})",
                var.name,
                literal(value_text)
            ))),
        }
    }

    fn preserves_counters(&self, _next_step: &Step) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consistency_and_tracing() {
        let v = |n: &str| ScriptVar {
            name: n.into(),
            declared_type: None,
            line: 0,
        };
        assert_eq!(
            CqlKit
                .set_variable_sql(&v("consistency"), "quorum")
                .unwrap(),
            "CONSISTENCY QUORUM"
        );
        assert_eq!(
            CqlKit.set_variable_sql(&v("tracing"), "on").unwrap(),
            "TRACING ON"
        );
        assert!(CqlKit.set_variable_sql(&v("x"), "1").is_err());
    }
}
