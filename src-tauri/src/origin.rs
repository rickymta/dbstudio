//! Origin of result columns for drivers whose protocol does not report the base table (SQL
//! Server, Oracle, CQL, ClickHouse): the statement that produced a result set is recognised
//! statically ([`dbstudio_sql::infer_origins`]) and the columns are annotated on their way to the
//! frontend. PostgreSQL and MySQL drivers fill `origin` themselves, from protocol metadata.

use dbstudio_core::{QueryEvent, ScriptDialect, TextRange};
use dbstudio_sql::{infer_origins, split};

/// Dialects whose columns get their origin from the text of the statement.
fn needs_static_origin(d: ScriptDialect) -> bool {
    matches!(
        d,
        ScriptDialect::MsSql
            | ScriptDialect::Oracle
            | ScriptDialect::Cql
            | ScriptDialect::ClickHouse
    )
}

/// Annotates the `Columns` events of one request.
pub(crate) struct OriginTracker {
    dialect: ScriptDialect,
    chars: Vec<char>,
    units: Vec<TextRange>,
    /// Range reported by the last `StatementStart` (positions in the typed text).
    current: Option<TextRange>,
}

impl OriginTracker {
    /// `None` when the dialect does not need static analysis.
    pub(crate) fn new(dialect: ScriptDialect, text: &str) -> Option<Self> {
        needs_static_origin(dialect).then(|| OriginTracker {
            dialect,
            chars: text.chars().collect(),
            units: split(dialect, text),
            current: None,
        })
    }

    /// The statement a result set belongs to: the only statement of the request, else the one
    /// the driver reported (a range that spans several statements finds nothing).
    fn statement(&self) -> Option<String> {
        let range = match (self.units.as_slice(), self.current) {
            ([only], _) => *only,
            (_, Some(r)) => r,
            _ => return None,
        };
        let slice = self.chars.get(range.start as usize..range.end as usize)?;
        Some(slice.iter().collect())
    }

    pub(crate) fn observe(&mut self, event: &mut QueryEvent) {
        match event {
            QueryEvent::StatementStart { start } => self.current = Some(start.range),
            QueryEvent::Columns { columns } => {
                let origins = self
                    .statement()
                    .and_then(|text| infer_origins(self.dialect, &text, columns));
                if let Some(origins) = origins {
                    for (c, o) in columns.iter_mut().zip(origins) {
                        c.origin = o;
                    }
                }
            }
            QueryEvent::StatementEnd { .. } => self.current = None,
            _ => {}
        }
    }
}

/// Wrap `emit` so result columns carry their origin (identity when the dialect needs none).
pub(crate) fn origin_emit<F>(
    dialect: ScriptDialect,
    text: &str,
    mut emit: F,
) -> impl FnMut(QueryEvent) -> bool + Send
where
    F: FnMut(QueryEvent) -> bool + Send,
{
    let mut tracker = OriginTracker::new(dialect, text);
    move |mut event| {
        if let Some(t) = tracker.as_mut() {
            t.observe(&mut event);
        }
        emit(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_core::{ColumnMeta, StatementStart, ValueKind};

    fn columns(names: &[&str]) -> QueryEvent {
        QueryEvent::Columns {
            columns: names
                .iter()
                .map(|n| ColumnMeta {
                    name: (*n).into(),
                    type_name: "int".into(),
                    value_kind: ValueKind::Int,
                    nullable: None,
                    origin: None,
                })
                .collect(),
        }
    }

    fn origins(e: &QueryEvent) -> Vec<Option<String>> {
        match e {
            QueryEvent::Columns { columns } => columns
                .iter()
                .map(|c| {
                    c.origin
                        .as_ref()
                        .map(|o| format!("{}.{}", o.table, o.column))
                })
                .collect(),
            _ => panic!("not columns"),
        }
    }

    #[test]
    fn single_statement_gets_origins() {
        let mut t = OriginTracker::new(ScriptDialect::MsSql, "SELECT id, n FROM dbo.t u").unwrap();
        let mut e = columns(&["id", "n"]);
        t.observe(&mut e);
        assert_eq!(origins(&e), vec![Some("t.id".into()), Some("t.n".into())]);
    }

    #[test]
    fn script_uses_the_reported_range() {
        let text = "SELECT 1; SELECT id FROM t";
        let mut t = OriginTracker::new(ScriptDialect::ClickHouse, text).unwrap();
        let mut start = QueryEvent::StatementStart {
            start: StatementStart {
                index: 1,
                range: TextRange { start: 10, end: 26 },
            },
        };
        t.observe(&mut start);
        let mut e = columns(&["id"]);
        t.observe(&mut e);
        assert_eq!(origins(&e), vec![Some("t.id".into())]);
        // No range reported for a multi statement script: nothing is guessed.
        let mut t = OriginTracker::new(ScriptDialect::ClickHouse, text).unwrap();
        let mut e = columns(&["id"]);
        t.observe(&mut e);
        assert_eq!(origins(&e), vec![None]);
        // A range spanning both statements finds nothing either.
        let mut whole = QueryEvent::StatementStart {
            start: StatementStart {
                index: 0,
                range: TextRange { start: 0, end: 26 },
            },
        };
        t.observe(&mut whole);
        let mut e = columns(&["id"]);
        t.observe(&mut e);
        assert_eq!(origins(&e), vec![None]);
    }

    #[test]
    fn other_dialects_are_left_alone() {
        assert!(OriginTracker::new(ScriptDialect::Postgres, "SELECT 1").is_none());
        let mut seen = Vec::new();
        let mut emit = origin_emit(ScriptDialect::Postgres, "SELECT id FROM t", |e| {
            seen.push(e);
            true
        });
        assert!(emit(columns(&["id"])));
        drop(emit);
        assert_eq!(origins(&seen[0]), vec![None]);
    }
}
