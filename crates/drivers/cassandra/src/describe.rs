//! cqlsh `DESCRIBE`: the server's own implementation first, otherwise built from the schema.

use std::time::Instant;

use dbstudio_core::{
    ColumnMeta, DdlOptions, EndOutcome, Error, ObjectKind, QueryRequest, Result, ResultSink,
    StatementEnd, Value, ValueKind,
};
use scylla::response::PagingState;
use scylla::statement::Statement;

use crate::core::{Core, ExecState, first_words};
use crate::ddl;

fn meta(name: &str) -> ColumnMeta {
    ColumnMeta {
        name: name.into(),
        type_name: "text".into(),
        value_kind: ValueKind::Text,
        nullable: None,
    }
}

fn unquote(s: &str) -> String {
    let t = s.trim().trim_end_matches(';').trim();
    match t.strip_prefix('"').and_then(|x| x.strip_suffix('"')) {
        Some(inner) => inner.replace("\"\"", "\""),
        None => t.to_ascii_lowercase(),
    }
}

/// `ks.name` or `name` (with the default keyspace).
fn qualified_name(arg: &str, default_ks: Option<&str>) -> Result<(String, String)> {
    let arg = arg.trim().trim_end_matches(';').trim();
    // Split at the first dot outside double quotes.
    let mut in_q = false;
    let mut split_at = None;
    for (i, c) in arg.char_indices() {
        match c {
            '"' => in_q = !in_q,
            '.' if !in_q => {
                split_at = Some(i);
                break;
            }
            _ => {}
        }
    }
    match split_at {
        Some(i) => Ok((unquote(&arg[..i]), unquote(&arg[i + 1..]))),
        None => match default_ks {
            Some(ks) => Ok((ks.to_string(), unquote(arg))),
            None => Err(Error::Invalid(
                "no keyspace selected: use `ks.name` or USE a keyspace first".into(),
            )),
        },
    }
}

fn kind_label(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Database => "keyspace",
        ObjectKind::Type => "type",
        ObjectKind::Table => "table",
        ObjectKind::Index => "index",
        ObjectKind::MaterializedView => "materialized_view",
        ObjectKind::Function => "function",
        ObjectKind::Trigger => "trigger",
        _ => "object",
    }
}

impl Core {
    pub(crate) async fn describe(
        &self,
        state: &ExecState,
        text: &str,
        _req: &QueryRequest,
        sink: &mut dyn ResultSink,
    ) -> Result<()> {
        let started = Instant::now();
        // The server's DESCRIBE (Scylla 5.2+, Cassandra 5.1+/6) wins when it exists.
        let session = self.pool.session(state.keyspace.as_deref()).await?;
        let mut stmt = Statement::new(text.to_string());
        stmt.set_consistency(state.consistency);
        match self
            .page_loop(
                &session,
                stmt,
                PagingState::start(),
                None,
                crate::core::DEFAULT_BATCH,
                true,
                sink,
                started,
                false,
                text,
            )
            .await
        {
            Ok(_) => return Ok(()),
            Err(Error::Query { .. }) => {}
            Err(e) => return Err(e),
        }
        self.describe_from_schema(state, text, sink, started).await
    }

    async fn describe_from_schema(
        &self,
        state: &ExecState,
        text: &str,
        sink: &mut dyn ResultSink,
        started: Instant,
    ) -> Result<()> {
        let words = first_words(text, 4);
        let what = words.get(1).map(String::as_str).unwrap_or("");
        let arg_from = |n: usize| -> String {
            let mut rest = text.trim_start();
            for _ in 0..n {
                rest = rest.trim_start();
                let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
                rest = &rest[end..];
            }
            rest.trim().trim_end_matches(';').trim().to_string()
        };
        let snap = self.snapshot().await?;
        let text_row =
            |vals: Vec<String>| -> Vec<Value> { vals.into_iter().map(Value::Text).collect() };
        let (columns, rows): (Vec<ColumnMeta>, Vec<Vec<Value>>) = match what {
            "keyspaces" => (
                vec![meta("keyspace_name"), meta("type"), meta("name")],
                snap.keyspaces
                    .keys()
                    .map(|k| text_row(vec![k.clone(), "keyspace".into(), k.clone()]))
                    .collect(),
            ),
            "tables" => {
                let ks = state
                    .keyspace
                    .clone()
                    .ok_or_else(|| Error::Invalid("no keyspace selected".into()))?;
                let k = snap.keyspace(&ks)?;
                (
                    vec![meta("keyspace_name"), meta("type"), meta("name")],
                    k.tables
                        .keys()
                        .map(|t| text_row(vec![ks.clone(), "table".into(), t.clone()]))
                        .collect(),
                )
            }
            "keyspace" | "schema" => {
                let arg = arg_from(2);
                let ks = if arg.is_empty() {
                    state
                        .keyspace
                        .clone()
                        .ok_or_else(|| Error::Invalid("no keyspace selected".into()))?
                } else {
                    unquote(&arg)
                };
                let items = ddl::generate(&snap, Some(&ks), &[], &DdlOptions::default())?;
                (
                    vec![
                        meta("keyspace_name"),
                        meta("type"),
                        meta("name"),
                        meta("create_statement"),
                    ],
                    items
                        .into_iter()
                        .map(|i| {
                            let name = i.path.last().map(|s| s.name.clone()).unwrap_or_default();
                            text_row(vec![ks.clone(), kind_label(i.kind).into(), name, i.sql])
                        })
                        .collect(),
                )
            }
            "table" | "type" | "index" | "view" | "materialized" | "function" | "aggregate" => {
                let skip = if what == "materialized" { 3 } else { 2 };
                let (ks, name) = qualified_name(&arg_from(skip - 1), state.keyspace.as_deref())?;
                let path = match what {
                    "table" => ddl::table_path(&ks, &name, false),
                    "view" | "materialized" => ddl::table_path(&ks, &name, true),
                    "type" => ddl::type_path(&ks, &name),
                    "function" => ddl::function_path(&ks, &name),
                    "aggregate" => ddl::aggregate_path(&ks, &name),
                    _ => {
                        // An index lives under its table: find it by name.
                        let k = snap.keyspace(&ks)?;
                        let t = k
                            .tables
                            .values()
                            .chain(k.views.values())
                            .find(|t| t.indexes.iter().any(|i| i.name == name))
                            .ok_or_else(|| Error::NotFound(format!("index {ks}.{name}")))?;
                        ddl::index_path(&ks, t, &name)
                    }
                };
                let sql = self.ddl_of(&path).await?;
                (
                    vec![
                        meta("keyspace_name"),
                        meta("type"),
                        meta("name"),
                        meta("create_statement"),
                    ],
                    vec![text_row(vec![ks, what.to_string(), name, sql])],
                )
            }
            _ => {
                return Err(Error::Unsupported(format!(
                    "DESCRIBE {}",
                    words.get(1).cloned().unwrap_or_default()
                )));
            }
        };
        let count = rows.len() as u64;
        sink.on_columns(columns)?;
        if !rows.is_empty() {
            sink.on_rows(rows)?;
        }
        sink.on_statement_end(StatementEnd {
            outcome: EndOutcome::Rows {
                row_count: count,
                truncated: false,
            },
            elapsed_ms: started.elapsed().as_millis() as u64,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(
            qualified_name("shop.orders;", None).unwrap(),
            ("shop".into(), "orders".into())
        );
        assert_eq!(
            qualified_name("\"My.Ks\".\"T\"", None).unwrap(),
            ("My.Ks".into(), "T".into())
        );
        assert_eq!(
            qualified_name("Orders", Some("shop")).unwrap(),
            ("shop".into(), "orders".into())
        );
        assert!(qualified_name("t", None).is_err());
    }
}
