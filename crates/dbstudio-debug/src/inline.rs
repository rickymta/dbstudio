//! Inline values: the grey text at the end of a line after a step ran (package D, design 2.8).

use dbstudio_core::debug::{InlineKind, InlineValue, SourceRef};
use dbstudio_core::{ColumnMeta, EndOutcome, StatementEnd, Value};

use crate::eval::display_value;
use crate::program::Step;

/// Most inline values kept for a session.
const MAX_INLINE: usize = 500;

fn make(step: &Step, text: String, kind: InlineKind) -> InlineValue {
    InlineValue {
        source: SourceRef::Script,
        line: step.line_end,
        text,
        kind,
    }
}

/// Inline value of the result of a step that ran: `3 rows affected`, `42 rows`,
/// `1 row: id=5, name='a'`. `None` for statements without a result (`Done`).
pub fn result_inline(
    step: &Step,
    end: &StatementEnd,
    first_row: Option<(&[ColumnMeta], &[Value])>,
) -> Option<InlineValue> {
    let text = match &end.outcome {
        EndOutcome::Affected { rows } => {
            format!("{rows} row{} affected", if *rows == 1 { "" } else { "s" })
        }
        EndOutcome::Rows {
            row_count,
            truncated,
        } => match (row_count, first_row) {
            (1, Some((cols, row))) if !*truncated => {
                let cells: Vec<String> = cols
                    .iter()
                    .zip(row.iter())
                    .take(4)
                    .map(|(c, v)| format!("{}={}", c.name, display_value(v)))
                    .collect();
                format!("1 row: {}", cells.join(", "))
            }
            (n, _) => format!(
                "{n}{} row{}",
                if *truncated { "+" } else { "" },
                if *n == 1 { "" } else { "s" }
            ),
        },
        EndOutcome::Done => return None,
    };
    Some(make(step, text, InlineKind::Result))
}

/// Inline value of a failed step (message shortened).
pub fn error_inline(step: &Step, message: &str) -> InlineValue {
    let first = message.lines().next().unwrap_or("");
    let short: String = first.chars().take(80).collect();
    make(step, short, InlineKind::Error)
}

/// `@x = 5` after a statement that assigned a script variable.
pub fn variable_inline(step: &Step, name: &str, display: &str) -> InlineValue {
    make(step, format!("{name} = {display}"), InlineKind::Variable)
}

/// Add `new` replacing an older value of the same line and kind (the newest wins).
pub fn merge(list: &mut Vec<InlineValue>, new: InlineValue) {
    list.retain(|v| !(v.line == new.line && v.kind == new.kind && v.source == new.source));
    list.push(new);
    if list.len() > MAX_INLINE {
        list.remove(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::program::DebugProgram;
    use dbstudio_core::{ScriptDialect, ValueKind};

    fn step() -> Step {
        DebugProgram::build(
            ScriptDialect::Postgres,
            "SELECT 1;\nUPDATE t\nSET a = 1",
            None,
        )
        .unwrap()
        .steps
        .remove(1)
    }

    fn end(o: EndOutcome) -> StatementEnd {
        StatementEnd {
            outcome: o,
            elapsed_ms: 1,
        }
    }

    #[test]
    fn texts() {
        let s = step();
        assert_eq!(s.line_end, 3);
        let v = result_inline(&s, &end(EndOutcome::Affected { rows: 3 }), None).unwrap();
        assert_eq!((v.line, v.text.as_str()), (3, "3 rows affected"));
        let v = result_inline(
            &s,
            &end(EndOutcome::Rows {
                row_count: 42,
                truncated: false,
            }),
            None,
        )
        .unwrap();
        assert_eq!(v.text, "42 rows");
        let cols = vec![
            ColumnMeta {
                name: "id".into(),
                type_name: "int".into(),
                value_kind: ValueKind::Int,
                nullable: None,
                origin: None,
            },
            ColumnMeta {
                name: "name".into(),
                type_name: "text".into(),
                value_kind: ValueKind::Text,
                nullable: None,
                origin: None,
            },
        ];
        let row = vec![Value::Int(5), Value::Text("a".into())];
        let v = result_inline(
            &s,
            &end(EndOutcome::Rows {
                row_count: 1,
                truncated: false,
            }),
            Some((&cols, &row)),
        )
        .unwrap();
        assert_eq!(v.text, "1 row: id=5, name='a'");
        assert!(result_inline(&s, &end(EndOutcome::Done), None).is_none());
        assert_eq!(error_inline(&s, "boom\nmore").text, "boom");
    }

    #[test]
    fn merge_keeps_the_newest_per_line_and_kind() {
        let s = step();
        let mut list = vec![];
        merge(&mut list, variable_inline(&s, "@x", "1"));
        merge(&mut list, variable_inline(&s, "@x", "2"));
        merge(&mut list, error_inline(&s, "e"));
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].text, "@x = 2");
    }
}
