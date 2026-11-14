//! The step program: the script split into steps with the dialect of the lease (package D).

use dbstudio_core::debug::TsqlStepUnit;
use dbstudio_core::{Error, Result, ScriptDialect, StatementClass, StatementInfo, TextRange};
use dbstudio_sql::lexer::{TokenKind, tokenize};

use crate::tsql_shim;

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
    /// Text of the step (the slice of the snapshot `range` covers, not yet bound).
    pub text: String,
}

impl Step {
    /// The debugger stops at this step.
    pub fn is_stop(&self) -> bool {
        matches!(self.kind, StepKind::Statement | StepKind::Block { .. })
    }
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

/// Where a breakpoint line lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snap {
    /// Step index, `None` when the line cannot stop.
    pub step: Option<usize>,
    /// Line after snapping.
    pub line: u32,
    /// Why it cannot stop.
    pub message: Option<String>,
}

/// Characters and significant words of a text (shared by the dialect helpers).
#[derive(Debug)]
pub(crate) struct Lexed {
    pub chars: Vec<char>,
    /// Non trivia tokens: `(kind, start, end)`.
    pub toks: Vec<(TokenKind, usize, usize)>,
}

impl Lexed {
    pub(crate) fn new(d: ScriptDialect, text: &str) -> Lexed {
        let toks = tokenize(d, text)
            .into_iter()
            .filter(|t| !t.kind.is_trivia() && t.kind != TokenKind::Whitespace)
            .map(|t| (t.kind, t.range.start as usize, t.range.end as usize))
            .collect();
        Lexed {
            chars: text.chars().collect(),
            toks,
        }
    }

    pub(crate) fn slice(&self, start: usize, end: usize) -> String {
        self.chars[start.min(self.chars.len())..end.min(self.chars.len())]
            .iter()
            .collect()
    }

    /// Upper-cased words (`Word` tokens only, quoted identifiers and strings excluded) that
    /// start inside `[start, end)`, at most `limit`.
    pub(crate) fn words(&self, start: usize, end: usize, limit: usize) -> Vec<String> {
        self.toks
            .iter()
            .filter(|(_, s, _)| *s >= start && *s < end)
            .take(limit)
            .map(|(k, s, e)| {
                if *k == TokenKind::Word {
                    self.slice(*s, *e).to_uppercase()
                } else {
                    String::new()
                }
            })
            .collect()
    }

    /// Tokens inside `[start, end)`.
    pub(crate) fn tokens_in(&self, start: usize, end: usize) -> Vec<(TokenKind, usize, usize)> {
        self.toks
            .iter()
            .filter(|(_, s, _)| *s >= start && *s < end)
            .copied()
            .collect()
    }
}

/// SQL*Plus commands of Oracle scripts (never sent as SQL).
const SQLPLUS: &[&str] = &[
    "SET", "DEFINE", "UNDEFINE", "PROMPT", "SPOOL", "ACCEPT", "COLUMN", "WHENEVER", "EXIT", "QUIT",
    "REM", "REMARK", "SHOW", "VARIABLE", "PRINT", "CLEAR", "BREAK", "TTITLE", "BTITLE", "TIMING",
    "HOST",
];

fn kind_of(d: ScriptDialect, w: &[String], info: &StatementInfo) -> StepKind {
    let w0 = w.first().map(String::as_str).unwrap_or("");
    let has = |names: &[&str]| {
        w.iter()
            .take(10)
            .skip(1)
            .any(|x| names.contains(&x.as_str()))
    };
    let block = |r: &str| StepKind::Block { reason: r.into() };
    match d {
        _ if info.class == StatementClass::Empty => StepKind::Skipped,
        ScriptDialect::Postgres => {
            if w0 == "DO" {
                block("DO block")
            } else if w0 == "CREATE" && has(&["FUNCTION", "PROCEDURE"]) {
                block("CREATE FUNCTION/PROCEDURE")
            } else {
                StepKind::Statement
            }
        }
        ScriptDialect::MySql { .. } => {
            if w0 == "CREATE" && has(&["PROCEDURE", "FUNCTION", "TRIGGER", "EVENT"]) {
                block("stored program definition")
            } else {
                StepKind::Statement
            }
        }
        ScriptDialect::Oracle => {
            let second = w.get(1).map(String::as_str).unwrap_or("");
            if SQLPLUS.contains(&w0)
                && !(w0 == "SET"
                    && matches!(
                        second,
                        "TRANSACTION" | "ROLE" | "CONSTRAINT" | "CONSTRAINTS"
                    ))
                && info.class != StatementClass::Read
            {
                StepKind::Meta
            } else if info.class == StatementClass::Procedural && matches!(w0, "BEGIN" | "DECLARE")
            {
                block("PL/SQL block")
            } else if w0 == "CREATE"
                && has(&["FUNCTION", "PROCEDURE", "PACKAGE", "TRIGGER", "TYPE"])
            {
                block("PL/SQL unit")
            } else {
                StepKind::Statement
            }
        }
        ScriptDialect::Cql => {
            if w0 == "BEGIN" {
                block("BATCH")
            } else if w0 == "CREATE" && has(&["FUNCTION", "AGGREGATE"]) {
                block("CREATE FUNCTION")
            } else {
                StepKind::Statement
            }
        }
        _ => StepKind::Statement,
    }
}

/// 1-based line of every char offset boundary.
#[derive(Debug)]
pub(crate) struct Lines {
    /// Offset of the first char of each line.
    starts: Vec<usize>,
}

impl Lines {
    pub(crate) fn new(chars: &[char]) -> Lines {
        let mut starts = vec![0];
        for (i, c) in chars.iter().enumerate() {
            if *c == '\n' {
                starts.push(i + 1);
            }
        }
        Lines { starts }
    }

    /// Line (1-based) of the char at `offset`.
    pub(crate) fn line_of(&self, offset: usize) -> u32 {
        (self.starts.partition_point(|s| *s <= offset)).max(1) as u32
    }

    pub(crate) fn count(&self) -> usize {
        self.starts.len()
    }

    pub(crate) fn start_of(&self, line: u32) -> usize {
        self.starts[(line as usize - 1).min(self.starts.len() - 1)]
    }
}

fn intersects(r: TextRange, sel: TextRange) -> bool {
    if sel.start >= sel.end {
        return r.start <= sel.start && sel.start <= r.end;
    }
    r.start < sel.end && r.end > sel.start
}

impl DebugProgram {
    /// Build the program of `text`, limited to the statements intersecting `range`.
    pub fn build(dialect: ScriptDialect, text: &str, range: Option<TextRange>) -> Result<Self> {
        Self::build_with(dialect, text, range, TsqlStepUnit::Statement)
    }

    /// Like [`DebugProgram::build`] with the T-SQL step unit (statement + variable shim, or one
    /// step per batch).
    pub fn build_with(
        dialect: ScriptDialect,
        text: &str,
        range: Option<TextRange>,
        tsql_unit: TsqlStepUnit,
    ) -> Result<Self> {
        let analysis = dbstudio_sql::analyze(dialect, text);
        let lexed = Lexed::new(dialect, text);
        let lines = Lines::new(&lexed.chars);
        let mk = |kind: StepKind, range: TextRange, info: StatementInfo| {
            let (s, e) = (range.start as usize, range.end as usize);
            Step {
                index: 0,
                kind,
                range,
                line_start: lines.line_of(s),
                line_end: lines.line_of(e.saturating_sub(1).max(s)),
                info,
                text: lexed.slice(s, e),
            }
        };
        let mut steps: Vec<Step> = if dialect == ScriptDialect::MsSql {
            tsql_shim::build_steps(text, &analysis, &lexed, tsql_unit, &mk)
        } else {
            analysis
                .statements
                .iter()
                .map(|info| {
                    let words = lexed.words(info.range.start as usize, info.range.end as usize, 12);
                    mk(kind_of(dialect, &words, info), info.range, info.clone())
                })
                .collect()
        };
        if let Some(sel) = range {
            steps.retain(|s| intersects(s.range, sel));
        }
        for (i, s) in steps.iter_mut().enumerate() {
            s.index = i as u32;
        }
        if steps.is_empty() && !text.trim().is_empty() && range.is_none() {
            return Err(Error::Invalid("script has no statement".into()));
        }
        Ok(DebugProgram {
            dialect,
            text: text.to_string(),
            steps,
        })
    }

    /// Index of the first step that stops (not `Meta`/`Skipped`) at or after `from`.
    pub fn next_stop(&self, from: usize) -> Option<usize> {
        (from..self.steps.len()).find(|i| self.steps[*i].is_stop())
    }

    /// Words of a step, upper case.
    pub fn words(&self, step: &Step, limit: usize) -> Vec<String> {
        Lexed::new(self.dialect, &step.text).words(0, usize::MAX, limit)
    }

    /// Where a breakpoint on `line` lands (design 2.5): the step holding the first
    /// non-blank character of the line, a blank/comment line snaps down to the next step.
    pub fn snap(&self, line: u32) -> Snap {
        let chars: Vec<char> = self.text.chars().collect();
        let lines = Lines::new(&chars);
        if line == 0 || line as usize > lines.count() {
            return Snap {
                step: None,
                line,
                message: Some("line outside the script".into()),
            };
        }
        let start = lines.start_of(line);
        let end = if (line as usize) < lines.count() {
            lines.start_of(line + 1)
        } else {
            chars.len()
        };
        let first = (start..end).find(|i| !chars[*i].is_whitespace());
        let target = first.unwrap_or(start) as u32;
        let hit = self
            .steps
            .iter()
            .position(|s| s.range.start <= target && target < s.range.end)
            .or_else(|| self.steps.iter().position(|s| s.range.start >= target));
        let Some(idx) = hit else {
            return Snap {
                step: None,
                line,
                message: Some("no statement at or after this line".into()),
            };
        };
        let s = &self.steps[idx];
        match &s.kind {
            StepKind::Meta | StepKind::Skipped => Snap {
                step: None,
                line,
                message: Some("meta command: cannot stop here".into()),
            },
            StepKind::Block { .. } if line > s.line_start && first.is_some() => Snap {
                step: None,
                line,
                message: Some("cannot stop inside a block".into()),
            },
            _ => Snap {
                step: Some(idx),
                line: s.line_start,
                message: None,
            },
        }
    }

    /// Index of the step holding `line` (JumpTo / RunToCursor), snapping like a breakpoint.
    pub fn step_at_line(&self, line: u32) -> Option<usize> {
        let chars: Vec<char> = self.text.chars().collect();
        let lines = Lines::new(&chars);
        if line == 0 || line as usize > lines.count() {
            return None;
        }
        let start = lines.start_of(line);
        let first = (start..chars.len()).find(|i| !chars[*i].is_whitespace())? as u32;
        self.steps
            .iter()
            .position(|s| s.is_stop() && s.range.end > first)
    }

    /// Script variable names (`@x`, not `@@x`) used in the program, in order of appearance.
    pub fn at_variables(&self) -> Vec<(String, u32)> {
        let lexed = Lexed::new(self.dialect, &self.text);
        let lines = Lines::new(&lexed.chars);
        let mut out: Vec<(String, u32)> = Vec::new();
        for (k, s, e) in &lexed.toks {
            if *k != TokenKind::Word {
                continue;
            }
            let w = lexed.slice(*s, *e);
            if w.starts_with('@') && !w.starts_with("@@") && w.len() > 1 {
                let key = w.to_lowercase();
                if !out.iter().any(|(n, _)| n.to_lowercase() == key) {
                    out.push((w, lines.line_of(*s)));
                }
            }
        }
        out
    }
}

/// Text that makes the driver's own splitter see exactly one statement (MySQL adds a
/// `DELIMITER` for statements with top-level `;`); other dialects return the text unchanged.
pub fn wrap_single(dialect: ScriptDialect, step_text: &str) -> String {
    if !matches!(dialect, ScriptDialect::MySql { .. }) {
        return step_text.to_string();
    }
    if dbstudio_sql::split(dialect, step_text).len() <= 1 {
        return step_text.to_string();
    }
    let mut end = String::from("$$__dbg$$");
    while step_text.contains(&end) {
        end.insert(end.len() - 2, '_');
    }
    format!("DELIMITER {end}\n{step_text}\n{end}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const PG: ScriptDialect = ScriptDialect::Postgres;

    fn mysql() -> ScriptDialect {
        ScriptDialect::mysql(false, true)
    }

    #[test]
    fn steps_have_lines_and_text() {
        let p =
            DebugProgram::build(PG, "SELECT 1;\n\nUPDATE t\n  SET a = 1;\nSELECT 2", None).unwrap();
        assert_eq!(p.steps.len(), 3);
        assert_eq!((p.steps[1].line_start, p.steps[1].line_end), (3, 4));
        assert_eq!(p.steps[1].text, "UPDATE t\n  SET a = 1");
        assert!(p.steps.iter().all(Step::is_stop));
    }

    #[test]
    fn blocks_are_one_step() {
        let p = DebugProgram::build(
            PG,
            "DO $$ BEGIN NULL; END $$;\nCREATE OR REPLACE FUNCTION f() RETURNS int AS $$ SELECT 1 $$ LANGUAGE sql;\nSELECT f();",
            None,
        )
        .unwrap();
        assert!(matches!(p.steps[0].kind, StepKind::Block { .. }));
        assert!(matches!(p.steps[1].kind, StepKind::Block { .. }));
        assert_eq!(p.steps[2].kind, StepKind::Statement);
        let p = DebugProgram::build(
            ScriptDialect::Oracle,
            "SET SERVEROUTPUT ON\nSELECT 1 FROM dual;\nBEGIN null; END;\n/\n",
            None,
        )
        .unwrap();
        assert_eq!(p.steps[0].kind, StepKind::Meta);
        assert_eq!(p.steps[1].kind, StepKind::Statement);
        assert!(matches!(p.steps[2].kind, StepKind::Block { .. }));
        let p = DebugProgram::build(
            ScriptDialect::Cql,
            "BEGIN BATCH INSERT INTO a(x) VALUES (1); APPLY BATCH; SELECT * FROM t;",
            None,
        )
        .unwrap();
        assert!(matches!(p.steps[0].kind, StepKind::Block { .. }));
    }

    #[test]
    fn range_keeps_only_intersecting_statements() {
        let text = "SELECT 1;\nSELECT 2;\nSELECT 3;";
        let p = DebugProgram::build(PG, text, Some(TextRange { start: 12, end: 25 })).unwrap();
        assert_eq!(
            p.steps.iter().map(|s| s.text.as_str()).collect::<Vec<_>>(),
            ["SELECT 2", "SELECT 3"]
        );
        assert_eq!(p.steps[0].index, 0);
    }

    #[test]
    fn breakpoints_snap_down_and_reject_block_interior() {
        let text = "SELECT 1;\n\n-- note\nSELECT 2;\nDO $$\nBEGIN NULL;\nEND $$;";
        let p = DebugProgram::build(PG, text, None).unwrap();
        let s = p.snap(2);
        // the leading comment belongs to the statement that follows it
        assert_eq!((s.step, s.line), (Some(1), 3));
        let s = p.snap(4);
        assert_eq!(s.step, Some(1));
        assert_eq!(p.snap(5).step, Some(2));
        let inside = p.snap(6);
        assert_eq!(inside.step, None);
        assert!(inside.message.is_some());
        assert_eq!(p.snap(99).step, None);
    }

    #[test]
    fn at_variables_skip_system_variables() {
        let p =
            DebugProgram::build(mysql(), "SET @a = 1;\nSELECT @a, @@version, @b;", None).unwrap();
        let names: Vec<String> = p.at_variables().into_iter().map(|v| v.0).collect();
        assert_eq!(names, ["@a", "@b"]);
    }

    #[test]
    fn wrap_single_keeps_one_statement_for_every_dialect() {
        let proc_text = "CREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END";
        let w = wrap_single(mysql(), proc_text);
        assert!(w.starts_with("DELIMITER "));
        assert_eq!(dbstudio_sql::split(mysql(), &w).len(), 1);
        assert_eq!(wrap_single(mysql(), "SELECT 1"), "SELECT 1");
        assert_eq!(wrap_single(PG, "SELECT 1"), "SELECT 1");
        // the delimiter never appears inside the statement
        let tricky = "CREATE PROCEDURE p() BEGIN SELECT '$$__dbg$$'; SELECT 2; END";
        let w = wrap_single(mysql(), tricky);
        assert_eq!(dbstudio_sql::split(mysql(), &w).len(), 1);
    }

    #[test]
    fn every_step_text_splits_to_a_single_statement() {
        let cases: Vec<(ScriptDialect, &str)> = vec![
            (
                PG,
                "SELECT 1;\nDO $$ BEGIN NULL; END $$;\nINSERT INTO t VALUES (1);",
            ),
            (
                mysql(),
                "SELECT 1;\nCREATE TRIGGER x BEFORE INSERT ON t FOR EACH ROW BEGIN SET NEW.a = 1; SET NEW.b = 2; END;",
            ),
            (
                ScriptDialect::MsSql,
                "DECLARE @a int = 1;\nSELECT @a;\nUPDATE t SET x = @a",
            ),
            (
                ScriptDialect::Oracle,
                "SELECT 1 FROM dual;\nBEGIN null; END;\n/\nSELECT 2 FROM dual;",
            ),
            (
                ScriptDialect::Cql,
                "SELECT * FROM a;\nBEGIN BATCH INSERT INTO a(x) VALUES (1); APPLY BATCH;",
            ),
            (ScriptDialect::ClickHouse, "SELECT 1;\nSELECT 2;"),
            (ScriptDialect::MongoShell, "db.a.find({});\ndb.b.find({})"),
            (ScriptDialect::RedisCli, "SET a 1\nGET a"),
        ];
        for (d, text) in cases {
            let p = DebugProgram::build(d, text, None).unwrap();
            for s in p.steps.iter().filter(|s| s.is_stop()) {
                let w = wrap_single(d, &s.text);
                assert_eq!(dbstudio_sql::split(d, &w).len(), 1, "{d:?}: {:?}", s.text);
            }
        }
    }
}
