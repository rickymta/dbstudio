//! The step program: the script split into steps with the dialect of the lease (package D).

use dbstudio_core::{Error, Result, ScriptDialect, StatementInfo, TextRange};

/// What a step is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepKind {
    /// A statement the debugger runs and stops at.
    Statement,
    /// A block that cannot be entered (`DO`, PL/SQL block, `CREATE FUNCTION`): one step.
    Block {
        /// Why it is opaque.
        reason: String,
    },
    /// Not sent to the server and never a stop (`DELIMITER`, `GO`).
    Meta,
    /// Empty or comment only.
    Skipped,
}

/// One step of the program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// Position among the steps.
    pub index: u32,
    /// Kind of step.
    pub kind: StepKind,
    /// Range of the step in the original text (char offsets).
    pub range: TextRange,
    /// First line (1-based).
    pub line_start: u32,
    /// Last line (1-based).
    pub line_end: u32,
    /// Analysis of the statement (class, verb, destructive, implicit commit).
    pub info: StatementInfo,
}

/// The whole script as steps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebugProgram {
    /// Dialect used to split.
    pub dialect: ScriptDialect,
    /// Snapshot of the text taken at start.
    pub text: String,
    /// Steps in order.
    pub steps: Vec<Step>,
}

impl DebugProgram {
    /// Build the program of `text`, limited to the statements intersecting `range`.
    pub fn build(dialect: ScriptDialect, text: &str, range: Option<TextRange>) -> Result<Self> {
        let _ = (dialect, text, range);
        Err(Error::Unsupported("sql debugger: program".into()))
    }
}

/// Text that makes the driver's own splitter see exactly one statement (MySQL adds a
/// `DELIMITER` for statements with top-level `;`); other dialects return the text unchanged.
pub fn wrap_single(dialect: ScriptDialect, step_text: &str) -> String {
    let _ = dialect;
    step_text.to_string()
}
