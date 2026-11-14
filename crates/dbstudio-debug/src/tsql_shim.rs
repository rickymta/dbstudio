//! T-SQL variable shim: keeps `DECLARE`d variables alive across single-statement steps
//! (package D, design section 4.5).
//!
//! T-SQL variables live inside one batch, so running a batch statement by statement would lose
//! them. The shim sends, for step `k`: `DECLARE <vars declared before k>; SELECT @a = <literal>,
//! ...; <statement k>; SELECT @a AS [@a], ...` and swallows the last result set (the read-out)
//! to remember the new values. Batches the shim cannot model (flow control, table variables,
//! cursors, `@@ROWCOUNT` ...) fall back to ONE step per batch.

use dbstudio_core::debug::TsqlStepUnit;
use dbstudio_core::{ScriptAnalysis, StatementInfo, TextRange, Value};
use dbstudio_sql::lexer::TokenKind;

use crate::ScriptVar;
use crate::eval::value_plain;
use crate::program::{DebugProgram, Lexed, Lines, Step, StepKind};

type Mk<'a> = dyn Fn(StepKind, TextRange, StatementInfo) -> Step + 'a;

const FLOW_WORDS: &[&str] = &[
    "CURSOR", "WHILE", "TRY", "CATCH", "GOTO", "RETURN", "WAITFOR",
];
const SYSTEM_VARS: &[&str] = &["@@ROWCOUNT", "@@ERROR", "@@IDENTITY", "@@FETCH_STATUS"];
const OBJECT_WORDS: &[&str] = &[
    "TABLE",
    "VIEW",
    "PROCEDURE",
    "PROC",
    "FUNCTION",
    "INDEX",
    "TRIGGER",
    "SCHEMA",
    "DATABASE",
    "TYPE",
    "SEQUENCE",
    "SYNONYM",
    "COLUMN",
    "CONSTRAINT",
];
const FIRST_IN_BATCH: &[&str] = &[
    "PROCEDURE",
    "PROC",
    "VIEW",
    "TRIGGER",
    "FUNCTION",
    "SCHEMA",
    "RULE",
    "DEFAULT",
];

/// Why a batch cannot be stepped statement by statement, if it cannot.
pub fn fallback_reason(words: &[String]) -> Option<String> {
    let w = |i: usize| words.get(i).map(String::as_str).unwrap_or("");
    for (i, word) in words.iter().enumerate() {
        let word = word.as_str();
        if FLOW_WORDS.contains(&word) {
            return Some(format!("batch: {word}"));
        }
        if SYSTEM_VARS.contains(&word) || word == "SCOPE_IDENTITY" {
            return Some(format!("batch: {word}"));
        }
        if word == "IF" && !(i > 0 && OBJECT_WORDS.contains(&w(i - 1)) && w(i + 1) == "EXISTS") {
            return Some("batch: flow control".into());
        }
        if word == "BEGIN"
            && !matches!(
                w(i + 1),
                "TRAN" | "TRANSACTION" | "DISTRIBUTED" | "DIALOG" | "CONVERSATION"
            )
        {
            return Some("batch: BEGIN...END".into());
        }
        if word == "TABLE"
            && i > 0
            && (w(i - 1).starts_with('@')
                || (w(i - 1) == "AS" && i > 1 && w(i - 2).starts_with('@')))
        {
            return Some("batch: table variable".into());
        }
        if matches!(word, "CREATE" | "ALTER") {
            let mut j = i + 1;
            if w(j) == "OR" && w(j + 1) == "ALTER" {
                j += 2;
            }
            if FIRST_IN_BATCH.contains(&w(j)) {
                return Some(format!("batch: {} {}", word, w(j)));
            }
        }
    }
    let exec = words.iter().any(|x| x == "EXEC" || x == "EXECUTE");
    if exec && words.iter().any(|x| x == "OUTPUT" || x == "OUT") {
        return Some("batch: EXEC with OUTPUT".into());
    }
    None
}

/// Steps of a T-SQL script: statements of shimmable batches, one step per fallback batch.
pub(crate) fn build_steps(
    text: &str,
    analysis: &ScriptAnalysis,
    lexed: &Lexed,
    unit: TsqlStepUnit,
    mk: &Mk<'_>,
) -> Vec<Step> {
    let _ = text;
    let mut out = Vec::new();
    for u in dbstudio_sql::split::split_batches(dbstudio_core::ScriptDialect::MsSql, text) {
        let inside: Vec<&StatementInfo> = analysis
            .statements
            .iter()
            .filter(|s| s.range.start >= u.range.start && s.range.end <= u.range.end)
            .collect();
        if inside.is_empty() {
            continue;
        }
        let words = lexed.words(u.range.start as usize, u.range.end as usize, usize::MAX);
        let reason = fallback_reason(&words);
        let mut batch_steps: Vec<Step> = Vec::new();
        let whole = |kind: StepKind| {
            let pick = inside
                .iter()
                .find(|s| s.destructive.is_some())
                .unwrap_or(&inside[0]);
            let mut info = (*pick).clone();
            info.range = u.range;
            mk(kind, u.range, info)
        };
        if unit == TsqlStepUnit::Batch {
            batch_steps.push(whole(StepKind::Statement));
        } else if let (Some(r), true) = (&reason, inside.len() > 1) {
            batch_steps.push(whole(StepKind::Block { reason: r.clone() }));
        } else {
            for s in &inside {
                let kind = if s.class == dbstudio_core::StatementClass::Empty {
                    StepKind::Skipped
                } else if let (Some(r), true) = (&reason, inside.len() == 1) {
                    StepKind::Block { reason: r.clone() }
                } else {
                    StepKind::Statement
                };
                batch_steps.push(mk(kind, s.range, (*s).clone()));
            }
        }
        for _ in 0..u.repeat.max(1) {
            out.extend(batch_steps.iter().cloned());
        }
    }
    out
}

/// A scalar variable the shim carries.
#[derive(Debug, Clone, PartialEq)]
pub struct ShimVar {
    /// `@name`.
    pub name: String,
    /// Declared type text (`int`, `nvarchar(50)`).
    pub ty: String,
    /// Initializer expression of the `DECLARE`, if any.
    pub init: Option<String>,
    /// Last known value (`None` = never read, so `NULL`).
    pub value: Option<Value>,
    /// First line where it is declared.
    pub line: u32,
}

/// Variables declared by the `DECLARE` statement in `[start, end)`.
fn parse_declares(lexed: &Lexed, lines: &Lines, start: usize, end: usize) -> Vec<ShimVar> {
    let toks = lexed.tokens_in(start, end);
    let mut out = Vec::new();
    let is_word = |i: usize, s: &str| {
        toks.get(i).is_some_and(|(k, a, b)| {
            *k == TokenKind::Word && lexed.slice(*a, *b).eq_ignore_ascii_case(s)
        })
    };
    if !is_word(0, "DECLARE") {
        return out;
    }
    let mut i = 1;
    while i < toks.len() {
        let (k, a, b) = toks[i];
        let name = lexed.slice(a, b);
        if k != TokenKind::Word || !name.starts_with('@') {
            break;
        }
        i += 1;
        if is_word(i, "AS") {
            i += 1;
        }
        let (mut depth, mut ty_start, mut ty_end) = (0i32, None, None);
        let mut init: Option<(usize, usize)> = None;
        let mut in_init = false;
        while i < toks.len() {
            let (tk, ta, tb) = toks[i];
            let t = lexed.slice(ta, tb);
            if tk == TokenKind::Punct {
                if t == "(" {
                    depth += 1;
                } else if t == ")" {
                    depth -= 1;
                } else if depth == 0 && t == "," {
                    break;
                } else if depth == 0 && t == "=" && !in_init {
                    in_init = true;
                    i += 1;
                    continue;
                }
            }
            if in_init {
                init = Some((init.map(|x| x.0).unwrap_or(ta), tb));
            } else {
                ty_start.get_or_insert(ta);
                ty_end = Some(tb);
            }
            i += 1;
        }
        if let (Some(s), Some(e)) = (ty_start, ty_end) {
            out.push(ShimVar {
                name,
                ty: lexed.slice(s, e),
                init: init.map(|(s, e)| lexed.slice(s, e)),
                value: None,
                line: lines.line_of(a),
            });
        }
        i += 1; // the comma
    }
    out
}

/// Scalar variables declared anywhere in the program (Variables panel).
pub fn declared_vars(program: &DebugProgram) -> Vec<ScriptVar> {
    let lexed = Lexed::new(program.dialect, &program.text);
    let lines = Lines::new(&lexed.chars);
    let mut out: Vec<ScriptVar> = Vec::new();
    for s in &program.steps {
        for v in parse_declares(&lexed, &lines, s.range.start as usize, s.range.end as usize) {
            if !out.iter().any(|o| o.name.eq_ignore_ascii_case(&v.name)) {
                out.push(ScriptVar {
                    name: v.name,
                    declared_type: Some(v.ty),
                    line: v.line,
                });
            }
        }
    }
    out
}

/// What the shim sends for a step.
#[derive(Debug, Clone, PartialEq)]
pub struct Composed {
    /// Text to execute.
    pub text: String,
    /// Variables of the trailing read-out, in column order (empty = no read-out).
    pub readout: Vec<String>,
    /// Variables this step declares (added to the shim after it ran).
    new_vars: Vec<ShimVar>,
}

/// Value carrier across the steps of one batch.
#[derive(Debug)]
pub struct TsqlShim {
    lexed: Lexed,
    lines: Lines,
    batches: Vec<TextRange>,
    current: Option<usize>,
    vars: Vec<ShimVar>,
}

fn cast(text: &str, ty: &str) -> String {
    format!("CAST(N'{}' AS {ty})", text.replace('\'', "''"))
}

fn is_float_type(ty: &str) -> bool {
    let t = ty.to_lowercase();
    t.starts_with("float") || t.starts_with("real")
}

/// T-SQL literal that restores `v` into a variable of type `ty`.
pub fn literal(v: Option<&Value>, ty: &str) -> String {
    match v {
        None | Some(Value::Null) => "NULL".into(),
        Some(Value::Bytes(b)) => {
            let hex: String = b.iter().map(|x| format!("{x:02X}")).collect();
            if hex.is_empty() {
                format!("CAST(0x AS {ty})")
            } else {
                format!("CAST(0x{hex} AS {ty})")
            }
        }
        Some(Value::Array(_) | Value::Document(_) | Value::Ext(_)) => "NULL".into(),
        Some(other) => cast(&value_plain(other), ty),
    }
}

impl TsqlShim {
    /// Shim of `program` (a T-SQL program).
    pub fn new(program: &DebugProgram) -> Self {
        let lexed = Lexed::new(program.dialect, &program.text);
        let lines = Lines::new(&lexed.chars);
        TsqlShim {
            batches: dbstudio_sql::split::split_batches(program.dialect, &program.text)
                .into_iter()
                .map(|u| u.range)
                .collect(),
            lexed,
            lines,
            current: None,
            vars: Vec::new(),
        }
    }

    fn batch_of(&self, step: &Step) -> Option<usize> {
        self.batches
            .iter()
            .position(|b| b.start <= step.range.start && step.range.end <= b.end)
    }

    /// Variables known right now.
    pub fn vars(&self) -> &[ShimVar] {
        &self.vars
    }

    /// Forget every variable (new run).
    pub fn reset(&mut self) {
        self.vars.clear();
        self.current = None;
    }

    /// Change the value of a variable (Variables panel, "Set Value"). `text` is what the user
    /// typed: `NULL`, a quoted string or a bare value.
    pub fn set_value(&mut self, name: &str, text: &str) -> bool {
        let Some(v) = self
            .vars
            .iter_mut()
            .find(|v| v.name.eq_ignore_ascii_case(name))
        else {
            return false;
        };
        let t = text.trim();
        v.value = if t.eq_ignore_ascii_case("NULL") {
            None
        } else if t.len() >= 2 && t.starts_with('\'') && t.ends_with('\'') {
            Some(Value::Text(t[1..t.len() - 1].replace("''", "'")))
        } else {
            Some(Value::Text(t.to_string()))
        };
        true
    }

    /// Text to send for `step` whose bound statement text is `stmt` (`None` for blocks).
    pub fn compose(&mut self, step: &Step, stmt: &str) -> Composed {
        let batch = self.batch_of(step);
        if batch != self.current {
            self.vars.clear();
            self.current = batch;
        }
        if !matches!(step.kind, StepKind::Statement) {
            return Composed {
                text: stmt.to_string(),
                readout: vec![],
                new_vars: vec![],
            };
        }
        let new_vars = parse_declares(
            &self.lexed,
            &self.lines,
            step.range.start as usize,
            step.range.end as usize,
        );
        // A DECLARE met again (`GO n`, Jump To backwards) declares its variables anew: the
        // carried copy would make the batch fail with "already declared".
        let carried: Vec<&ShimVar> = self
            .vars
            .iter()
            .filter(|v| {
                !new_vars
                    .iter()
                    .any(|n| n.name.eq_ignore_ascii_case(&v.name))
            })
            .collect();
        if carried.is_empty() && new_vars.is_empty() {
            // nothing declared yet: the statement runs as it is
            return Composed {
                text: stmt.to_string(),
                readout: vec![],
                new_vars: vec![],
            };
        }
        let mut text = String::new();
        if !carried.is_empty() {
            let decl: Vec<String> = carried
                .iter()
                .map(|v| format!("{} {}", v.name, v.ty))
                .collect();
            text.push_str(&format!("DECLARE {};", decl.join(", ")));
            let sets: Vec<String> = carried
                .iter()
                .map(|v| format!("{} = {}", v.name, literal(v.value.as_ref(), &v.ty)))
                .collect();
            text.push_str(&format!(" SELECT {}; ", sets.join(", ")));
        }
        text.push_str(stmt);
        text.push_str("; SELECT ");
        let all: Vec<&ShimVar> = carried.iter().copied().chain(new_vars.iter()).collect();
        let cols: Vec<String> = all
            .iter()
            .map(|v| {
                if is_float_type(&v.ty) {
                    format!("CONVERT(varchar(64), {}, 3) AS [{}]", v.name, v.name)
                } else {
                    format!("{} AS [{}]", v.name, v.name)
                }
            })
            .collect();
        text.push_str(&cols.join(", "));
        Composed {
            text,
            readout: all.iter().map(|v| v.name.clone()).collect(),
            new_vars,
        }
    }

    /// Store the read-out row of a step that ran.
    pub fn absorb(&mut self, c: &Composed, row: &[Value]) {
        for v in &c.new_vars {
            match self
                .vars
                .iter_mut()
                .find(|o| o.name.eq_ignore_ascii_case(&v.name))
            {
                Some(old) => *old = v.clone(),
                None => self.vars.push(v.clone()),
            }
        }
        for (name, value) in c.readout.iter().zip(row.iter()) {
            if let Some(v) = self.vars.iter_mut().find(|v| &v.name == name) {
                v.value = if matches!(value, Value::Null) {
                    None
                } else {
                    Some(value.clone())
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_core::ScriptDialect;

    fn program(text: &str) -> DebugProgram {
        DebugProgram::build(ScriptDialect::MsSql, text, None).unwrap()
    }

    #[test]
    fn a_declare_met_again_does_not_redeclare_the_carried_variable() {
        let p = program("DECLARE @a int = 1;\nSELECT @a");
        let mut shim = TsqlShim::new(&p);
        let c = shim.compose(&p.steps[0], "DECLARE @a int = 1");
        shim.absorb(&c, &[Value::Int(1)]);
        let c = shim.compose(&p.steps[1], "SELECT @a");
        shim.absorb(&c, &[Value::Int(1)]);
        // Jump back (or `GO 2`): the DECLARE runs again.
        let again = shim.compose(&p.steps[0], "DECLARE @a int = 1");
        assert_eq!(again.text.matches("DECLARE").count(), 1, "{}", again.text);
        shim.absorb(&again, &[Value::Int(1)]);
        assert_eq!(shim.vars().len(), 1);
    }

    #[test]
    fn statements_of_a_plain_batch_are_steps() {
        let p = program("DECLARE @a int = 1;\nSET @a = @a + 1;\nSELECT @a");
        assert_eq!(p.steps.len(), 3);
        assert!(p.steps.iter().all(|s| s.kind == StepKind::Statement));
    }

    #[test]
    fn unsafe_batches_fall_back_to_one_step() {
        for (text, why) in [
            (
                "DECLARE @t TABLE (a int); INSERT @t VALUES (1); SELECT * FROM @t",
                "table variable",
            ),
            ("SELECT 1; IF 1 = 1 SELECT 2", "flow control"),
            ("SELECT 1; SELECT @@ROWCOUNT", "@@ROWCOUNT"),
            (
                "INSERT INTO t VALUES (1); SELECT SCOPE_IDENTITY()",
                "SCOPE_IDENTITY",
            ),
            ("SELECT 1; WHILE 1 = 0 SELECT 2", "WHILE"),
            (
                "BEGIN TRY SELECT 1; END TRY BEGIN CATCH SELECT 2; END CATCH",
                "BEGIN",
            ),
            ("DECLARE @c CURSOR; SELECT 1", "CURSOR"),
        ] {
            let p = program(text);
            assert_eq!(p.steps.len(), 1, "{text}");
            match &p.steps[0].kind {
                StepKind::Block { reason } => assert!(reason.contains(why), "{reason} vs {why}"),
                other => panic!("{text}: {other:?}"),
            }
        }
        // DROP ... IF EXISTS is not flow control
        let p = program("DROP TABLE IF EXISTS t; SELECT 1");
        assert_eq!(p.steps.len(), 2);
    }

    #[test]
    fn go_repeats_the_steps_and_batches_stay_apart() {
        let p = program("SELECT 1;\nSELECT 2\nGO 2\nSELECT 3");
        assert_eq!(p.steps.len(), 5);
        let texts: Vec<_> = p.steps.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(
            texts,
            ["SELECT 1", "SELECT 2", "SELECT 1", "SELECT 2", "SELECT 3"]
        );
    }

    #[test]
    fn batch_unit_makes_one_step_per_batch() {
        let p = DebugProgram::build_with(
            ScriptDialect::MsSql,
            "DECLARE @a int = 1;\nSELECT @a\nGO\nSELECT 2",
            None,
            TsqlStepUnit::Batch,
        )
        .unwrap();
        assert_eq!(p.steps.len(), 2);
    }

    #[test]
    fn declared_variables_have_types() {
        let p = program("DECLARE @a int = 1, @b nvarchar(50), @c decimal(10, 2) = 5;\nSELECT @a");
        let vars = declared_vars(&p);
        let got: Vec<(String, String)> = vars
            .into_iter()
            .map(|v| (v.name, v.declared_type.unwrap()))
            .collect();
        assert_eq!(
            got,
            [
                ("@a".to_string(), "int".to_string()),
                ("@b".to_string(), "nvarchar(50)".to_string()),
                ("@c".to_string(), "decimal(10, 2)".to_string())
            ]
        );
    }

    #[test]
    fn compose_snapshot_and_absorb() {
        let p = program("DECLARE @a int = 1, @f float;\nSET @a = @a + 1;\nSELECT @a");
        let mut shim = TsqlShim::new(&p);
        let c0 = shim.compose(&p.steps[0], &p.steps[0].text);
        assert_eq!(
            c0.text,
            "DECLARE @a int = 1, @f float; SELECT @a AS [@a], CONVERT(varchar(64), @f, 3) AS [@f]"
        );
        shim.absorb(&c0, &[Value::Int(1), Value::Null]);
        assert_eq!(shim.vars().len(), 2);
        let c1 = shim.compose(&p.steps[1], &p.steps[1].text);
        assert_eq!(
            c1.text,
            "DECLARE @a int, @f float; SELECT @a = CAST(N'1' AS int), @f = NULL; SET @a = @a + 1; SELECT @a AS [@a], CONVERT(varchar(64), @f, 3) AS [@f]"
        );
        shim.absorb(&c1, &[Value::Int(2), Value::Null]);
        assert_eq!(shim.vars()[0].value, Some(Value::Int(2)));
        assert!(shim.set_value("@A", "'o''k'"));
        assert_eq!(shim.vars()[0].value, Some(Value::Text("o'k".into())));
        assert!(!shim.set_value("@zz", "1"));
    }

    #[test]
    fn a_new_batch_forgets_the_variables() {
        let p = program("DECLARE @a int = 1;\nSELECT @a\nGO\nSELECT 5");
        let mut shim = TsqlShim::new(&p);
        let c = shim.compose(&p.steps[0], &p.steps[0].text);
        shim.absorb(&c, &[Value::Int(1)]);
        assert_eq!(shim.vars().len(), 1);
        let c = shim.compose(&p.steps[2], &p.steps[2].text);
        assert!(shim.vars().is_empty());
        assert_eq!(c.text, "SELECT 5");
        assert!(c.readout.is_empty());
    }

    #[test]
    fn literals() {
        assert_eq!(
            literal(Some(&Value::Text("it's".into())), "nvarchar(5)"),
            "CAST(N'it''s' AS nvarchar(5))"
        );
        assert_eq!(
            literal(Some(&Value::Bytes(vec![1, 255])), "varbinary(4)"),
            "CAST(0x01FF AS varbinary(4))"
        );
        assert_eq!(literal(None, "int"), "NULL");
    }
}
