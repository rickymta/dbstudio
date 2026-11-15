//! Origin of result columns for drivers whose protocol does not report the base table (SQL
//! Server, Oracle, CQL, ClickHouse): the statement that produced a result set is recognised
//! statically ([`dbstudio_sql::infer_origins`]) and the columns are annotated on their way to the
//! frontend. PostgreSQL and MySQL drivers fill `origin` themselves, from protocol metadata.

use dbstudio_core::{QueryEvent, ScriptDialect, TextRange};
use dbstudio_sql::{infer_origins, parse_simple_select, split};

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

/// Dialects whose driver reports origins itself, but which only keep them for a plain
/// one-table SELECT: the protocol cannot tell two instances of a table apart (self join,
/// derived table), and editing by key would then hit the wrong row.
fn needs_plain_select(d: ScriptDialect) -> bool {
    matches!(d, ScriptDialect::MySql { .. })
}

/// Where the connection is when a result set starts: `(database, schema)`.
pub(crate) type ContextFn = Box<dyn Fn() -> (Option<String>, Option<String>) + Send>;

/// Annotates the `Columns` events of one request.
pub(crate) struct OriginTracker {
    dialect: ScriptDialect,
    chars: Vec<char>,
    units: Vec<TextRange>,
    /// Range reported by the last `StatementStart` (positions in the typed text).
    current: Option<TextRange>,
    context: ContextFn,
}

impl OriginTracker {
    /// `None` when the dialect does not need static analysis.
    pub(crate) fn new(dialect: ScriptDialect, text: &str, context: ContextFn) -> Option<Self> {
        (needs_static_origin(dialect) || needs_plain_select(dialect)).then(|| OriginTracker {
            dialect,
            chars: text.chars().collect(),
            units: split(dialect, text),
            current: None,
            context,
        })
    }

    /// The statement is the first of the request, so the connection's context (database,
    /// schema) at this moment is the one it runs in (a later `USE` has not happened yet).
    fn runs_in_initial_context(&self) -> bool {
        match (self.units.first(), self.current) {
            (Some(first), Some(cur)) if self.units.len() > 1 => cur.start == first.start,
            _ => true,
        }
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
                let text = self.statement();
                if needs_plain_select(self.dialect) {
                    let plain = text
                        .as_deref()
                        .is_some_and(|t| parse_simple_select(self.dialect, t).is_some());
                    if !plain {
                        columns.iter_mut().for_each(|c| c.origin = None);
                    }
                } else {
                    let origins = text
                        .filter(|_| self.runs_in_initial_context())
                        .and_then(|text| infer_origins(self.dialect, &text, columns));
                    let (database, schema) = (self.context)();
                    for (i, c) in columns.iter_mut().enumerate() {
                        c.origin = origins.as_ref().and_then(|o| o[i].clone());
                        // Pin the context the statement ran in: later (a changed dropdown, a
                        // `USE`) the connection may be somewhere else.
                        if let Some(o) = c.origin.as_mut() {
                            if o.database.is_none() {
                                o.database.clone_from(&database);
                            }
                            if o.schema.is_none() && self.dialect == ScriptDialect::Oracle {
                                o.schema.clone_from(&schema);
                            }
                        }
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
    context: ContextFn,
    mut emit: F,
) -> impl FnMut(QueryEvent) -> bool + Send
where
    F: FnMut(QueryEvent) -> bool + Send,
{
    let mut tracker = OriginTracker::new(dialect, text, context);
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

    fn ctx(db: &'static str) -> ContextFn {
        Box::new(move || (Some(db.to_string()), Some("sch".to_string())))
    }

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

    fn mysql() -> ScriptDialect {
        ScriptDialect::MySql {
            mariadb: false,
            backslash_escapes: true,
            ansi_quotes: false,
        }
    }

    fn with_origin(table: &str, alias: &str, name: &str) -> ColumnMeta {
        let mut c = match columns(&[name]) {
            QueryEvent::Columns { mut columns } => columns.remove(0),
            _ => unreachable!(),
        };
        c.origin = Some(dbstudio_core::ColumnOrigin {
            database: Some("app".into()),
            schema: None,
            table: table.into(),
            column: name.into(),
            alias: Some(alias.into()),
        });
        c
    }

    #[test]
    fn mysql_origins_survive_only_a_plain_select() {
        let run = |sql: &str| {
            let mut t = OriginTracker::new(mysql(), sql, ctx("app")).unwrap();
            let mut e = QueryEvent::Columns {
                columns: vec![
                    with_origin("tree", "p", "id"),
                    with_origin("tree", "c", "name"),
                ],
            };
            t.observe(&mut e);
            origins(&e)
        };
        assert_eq!(
            run("SELECT p.id, c.name FROM tree p JOIN tree c ON c.parent_id = p.id"),
            vec![None, None]
        );
        assert_eq!(
            run("SELECT id, name FROM (SELECT * FROM tree) x"),
            vec![None, None]
        );
        assert_eq!(
            run("SELECT id, name FROM tree"),
            vec![Some("tree.id".into()), Some("tree.name".into())]
        );
        // A script whose statement cannot be told: nothing is kept.
        assert_eq!(run("SELECT 1; SELECT id, name FROM tree"), vec![None, None]);
    }

    #[test]
    fn the_context_of_the_statement_is_pinned_into_the_origin() {
        let mut t =
            OriginTracker::new(ScriptDialect::MsSql, "SELECT id FROM dbo.t", ctx("sales")).unwrap();
        let mut e = columns(&["id"]);
        t.observe(&mut e);
        let QueryEvent::Columns { columns: cs } = &e else {
            unreachable!()
        };
        assert_eq!(
            cs[0].origin.as_ref().unwrap().database.as_deref(),
            Some("sales")
        );
        // Oracle also pins the schema (current_schema); an explicit one wins.
        let mut t =
            OriginTracker::new(ScriptDialect::Oracle, "SELECT id FROM t", ctx("x")).unwrap();
        let mut e = columns_ora(&["ID"]);
        t.observe(&mut e);
        let QueryEvent::Columns { columns: cs } = &e else {
            unreachable!()
        };
        assert_eq!(
            cs[0].origin.as_ref().unwrap().schema.as_deref(),
            Some("sch")
        );
        // After a USE in the same script the connection context is stale: no origin.
        let text = "USE other; SELECT id FROM dbo.t";
        let mut t = OriginTracker::new(ScriptDialect::MsSql, text, ctx("sales")).unwrap();
        let mut start = QueryEvent::StatementStart {
            start: StatementStart {
                index: 1,
                range: TextRange { start: 11, end: 31 },
            },
        };
        t.observe(&mut start);
        let mut e = columns(&["id"]);
        t.observe(&mut e);
        assert_eq!(origins(&e), vec![None]);
    }

    fn columns_ora(names: &[&str]) -> QueryEvent {
        columns(names)
    }

    #[test]
    fn single_statement_gets_origins() {
        let mut t = OriginTracker::new(
            ScriptDialect::MsSql,
            "SELECT id, n FROM dbo.t u",
            ctx("app"),
        )
        .unwrap();
        let mut e = columns(&["id", "n"]);
        t.observe(&mut e);
        assert_eq!(origins(&e), vec![Some("t.id".into()), Some("t.n".into())]);
    }

    #[test]
    fn script_uses_the_reported_range() {
        let text = "SELECT id FROM t; SELECT 1";
        let mut t = OriginTracker::new(ScriptDialect::ClickHouse, text, ctx("app")).unwrap();
        let mut start = QueryEvent::StatementStart {
            start: StatementStart {
                index: 0,
                range: TextRange { start: 0, end: 16 },
            },
        };
        t.observe(&mut start);
        let mut e = columns(&["id"]);
        t.observe(&mut e);
        assert_eq!(origins(&e), vec![Some("t.id".into())]);
        // The second statement may run after a USE: its context is not known.
        let mut t = OriginTracker::new(
            ScriptDialect::ClickHouse,
            "SELECT 1; SELECT id FROM t",
            ctx("app"),
        )
        .unwrap();
        let mut later = QueryEvent::StatementStart {
            start: StatementStart {
                index: 1,
                range: TextRange { start: 10, end: 26 },
            },
        };
        t.observe(&mut later);
        let mut e = columns(&["id"]);
        t.observe(&mut e);
        assert_eq!(origins(&e), vec![None]);
        // No range reported for a multi statement script: nothing is guessed.
        let mut t = OriginTracker::new(ScriptDialect::ClickHouse, text, ctx("app")).unwrap();
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
        assert!(OriginTracker::new(ScriptDialect::Postgres, "SELECT 1", ctx("app")).is_none());
        let mut seen = Vec::new();
        let mut emit = origin_emit(
            ScriptDialect::Postgres,
            "SELECT id FROM t",
            ctx("app"),
            |e| {
                seen.push(e);
                true
            },
        );
        assert!(emit(columns(&["id"])));
        drop(emit);
        assert_eq!(origins(&seen[0]), vec![None]);
    }
}
