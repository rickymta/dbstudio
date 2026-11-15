//! Editing the rows of a query result: which table a result comes from and whether (and how) its
//! rows can be edited by key. The row edits themselves are the ordinary [`crate::ChangeSet`]s of
//! the table, so the guards and drivers of the table editor apply unchanged.

use serde::{Deserialize, Serialize};

use crate::{
    ColumnMeta, ColumnOrigin, DbKind, EditNote, Editability, NotEditableReason, ObjectKind,
    ObjectPath,
};

/// Why the rows of a result cannot be edited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ResultNotEditableReason {
    /// No column is known to come from a table (expressions only, `JOIN`ed derived tables,
    /// statements the driver cannot analyse, non-SQL results).
    NoOrigin,
    /// The columns come from more than one table.
    MultipleTables,
    /// The table has no primary key or unique NOT NULL index.
    NoUniqueKey,
    /// The source is a view or another object that is not a table.
    NotATable,
    /// The connection is read-only.
    ReadOnlyConnection,
    /// A key column of the table is not in the result (see `missing_key_columns`).
    KeyNotInResult,
    /// The driver cannot edit this kind of result.
    Unsupported,
}

/// Whether the rows of one result set can be edited, and how.
///
/// `key_columns` and `read_only_columns` use the names of the TABLE's columns, like
/// [`Editability`]; `column_map` says which table column each result column is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResultEditability {
    /// Rows can be edited.
    pub editable: bool,
    /// The one table the result is read from, when there is one.
    pub table: Option<ObjectPath>,
    /// Key columns of the table (table column names).
    pub key_columns: Vec<String>,
    /// Table column of each result column; `None` for expressions, duplicates and columns the
    /// table does not have (shown, never edited).
    pub column_map: Vec<Option<String>>,
    /// Identity / generated / computed columns of the table (table column names).
    pub read_only_columns: Vec<String>,
    /// Why not editable.
    pub reason: Option<ResultNotEditableReason>,
    /// Key columns missing from the result (with [`ResultNotEditableReason::KeyNotInResult`]).
    #[serde(default)]
    pub missing_key_columns: Vec<String>,
    /// Applying a change set is all-or-nothing.
    pub atomic: bool,
    /// Caveats the UI must show before applying.
    #[serde(default)]
    pub notes: Vec<EditNote>,
}

impl ResultEditability {
    /// A result that cannot be edited, for `reason`.
    pub fn not_editable(columns: usize, reason: ResultNotEditableReason) -> Self {
        ResultEditability {
            editable: false,
            table: None,
            key_columns: vec![],
            column_map: vec![None; columns],
            read_only_columns: vec![],
            reason: Some(reason),
            missing_key_columns: vec![],
            atomic: true,
            notes: vec![],
        }
    }
}

/// A table named by the origins of a result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceTable {
    /// Database / keyspace the driver reported.
    pub database: Option<String>,
    /// Schema the driver reported.
    pub schema: Option<String>,
    /// Table name.
    pub table: String,
}

/// The one table all columns with an origin come from. Two instances of one table (a self
/// join: same table, different aliases) are different sources.
pub fn single_source(columns: &[ColumnMeta]) -> Result<SourceTable, ResultNotEditableReason> {
    let mut source: Option<(SourceTable, Option<&str>)> = None;
    for o in columns.iter().filter_map(|c| c.origin.as_ref()) {
        let this = SourceTable {
            database: o.database.clone(),
            schema: o.schema.clone(),
            table: o.table.clone(),
        };
        let alias = o.alias.as_deref();
        match &source {
            None => source = Some((this, alias)),
            Some((first, a)) if *first == this && *a == alias => {}
            Some(_) => return Err(ResultNotEditableReason::MultipleTables),
        }
    }
    source
        .map(|(s, _)| s)
        .ok_or(ResultNotEditableReason::NoOrigin)
}

/// Where a connection is when the result was produced: fills what an origin leaves out.
#[derive(Debug, Clone, Default)]
pub struct PathContext {
    /// Current database / keyspace.
    pub database: Option<String>,
    /// Current schema.
    pub schema: Option<String>,
    /// Login name (Oracle: the default schema is the user's).
    pub user: Option<String>,
}

/// Object-tree path of `source` on a server of `kind`, or `None` when the kind has no SQL tables
/// to edit by key here (MongoDB, Redis).
pub fn table_path(kind: DbKind, source: &SourceTable, ctx: &PathContext) -> Option<ObjectPath> {
    let database = source.database.clone().or_else(|| ctx.database.clone());
    let root = ObjectPath::root();
    match kind {
        DbKind::Postgres | DbKind::CockroachDb | DbKind::YugabyteDb => {
            let schema = source
                .schema
                .clone()
                .or_else(|| ctx.schema.clone())
                .unwrap_or_else(|| "public".into());
            Some(
                root.child(ObjectKind::Database, database?)
                    .child(ObjectKind::Schema, schema)
                    .child(ObjectKind::Table, source.table.clone()),
            )
        }
        DbKind::SqlServer => {
            let schema = source
                .schema
                .clone()
                .or_else(|| ctx.schema.clone())
                .unwrap_or_else(|| "dbo".into());
            Some(
                root.child(ObjectKind::Database, database?)
                    .child(ObjectKind::Schema, schema)
                    .child(ObjectKind::Table, source.table.clone()),
            )
        }
        DbKind::Oracle => {
            let schema = source
                .schema
                .clone()
                .or_else(|| ctx.schema.clone())
                .or_else(|| ctx.user.as_ref().map(|u| u.to_uppercase()))?;
            Some(
                root.child(ObjectKind::Schema, schema)
                    .child(ObjectKind::Table, source.table.clone()),
            )
        }
        DbKind::MySql
        | DbKind::TiDb
        | DbKind::ClickHouse
        | DbKind::Cassandra
        | DbKind::ScyllaDb => Some(
            root.child(ObjectKind::Database, database?)
                .child(ObjectKind::Table, source.table.clone()),
        ),
        DbKind::MongoDb | DbKind::Redis => None,
    }
}

fn reason_of(r: NotEditableReason) -> ResultNotEditableReason {
    match r {
        NotEditableReason::NoUniqueKey => ResultNotEditableReason::NoUniqueKey,
        NotEditableReason::NotATable => ResultNotEditableReason::NotATable,
        NotEditableReason::ReadOnlyConnection => ResultNotEditableReason::ReadOnlyConnection,
        NotEditableReason::Unsupported => ResultNotEditableReason::Unsupported,
    }
}

/// Combine the table's [`Editability`] with the columns of the result.
///
/// Result columns are matched to table columns by the origin's column name; the spelling of key
/// and read-only columns (known from `ed`) wins over a differently cased origin, but only with
/// `fold_case` (origins from static analysis of case-insensitive dialects); origins reported by
/// the server (PostgreSQL, MySQL) are matched exactly. A column that maps to a table column already taken by
/// an earlier result column (`SELECT id, id AS id2`) stays read-only.
pub fn resolve_result_editability(
    columns: &[ColumnMeta],
    table: ObjectPath,
    ed: Editability,
    fold_case: bool,
) -> ResultEditability {
    let canonical = |name: &str| -> String {
        let known = || ed.key_columns.iter().chain(&ed.read_only_columns);
        known()
            .find(|k| k.as_str() == name)
            .or_else(|| {
                fold_case
                    .then(|| known().find(|k| k.eq_ignore_ascii_case(name)))
                    .flatten()
            })
            .cloned()
            .unwrap_or_else(|| name.to_string())
    };
    let mut column_map: Vec<Option<String>> = Vec::with_capacity(columns.len());
    for c in columns {
        let mapped = c.origin.as_ref().map(|o| canonical(&o.column));
        let taken = mapped
            .as_ref()
            .is_some_and(|m| column_map.contains(&Some(m.clone())));
        column_map.push(if taken { None } else { mapped });
    }
    let missing_key_columns: Vec<String> = ed
        .key_columns
        .iter()
        .filter(|k| !column_map.contains(&Some((*k).clone())))
        .cloned()
        .collect();
    let (editable, reason) = if !ed.editable {
        (
            false,
            Some(reason_of(
                ed.reason.unwrap_or(NotEditableReason::Unsupported),
            )),
        )
    } else if !missing_key_columns.is_empty() {
        (false, Some(ResultNotEditableReason::KeyNotInResult))
    } else {
        (true, None)
    };
    ResultEditability {
        editable,
        table: Some(table),
        key_columns: ed.key_columns,
        column_map,
        read_only_columns: ed.read_only_columns,
        reason,
        missing_key_columns: if reason == Some(ResultNotEditableReason::KeyNotInResult) {
            missing_key_columns
        } else {
            vec![]
        },
        atomic: ed.atomic,
        notes: ed.notes,
    }
}

/// Convenience for tests and callers that build origins by hand.
impl ColumnOrigin {
    /// `database.schema.table.column` with the parts that are known.
    pub fn qualified(&self) -> String {
        [
            self.database.as_deref(),
            self.schema.as_deref(),
            Some(self.table.as_str()),
            Some(self.column.as_str()),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(".")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ValueKind;

    fn col(name: &str, origin: Option<(&str, &str)>) -> ColumnMeta {
        ColumnMeta {
            name: name.into(),
            type_name: "int".into(),
            value_kind: ValueKind::Int,
            nullable: None,
            origin: origin.map(|(t, c)| ColumnOrigin {
                database: Some("db".into()),
                schema: Some("s".into()),
                table: t.into(),
                column: c.into(),
                alias: None,
            }),
        }
    }

    fn ed(
        editable: bool,
        keys: &[&str],
        ro: &[&str],
        reason: Option<NotEditableReason>,
    ) -> Editability {
        Editability {
            editable,
            key_columns: keys.iter().map(|s| (*s).into()).collect(),
            read_only_columns: ro.iter().map(|s| (*s).into()).collect(),
            reason,
            atomic: true,
            notes: vec![],
        }
    }

    fn path() -> ObjectPath {
        ObjectPath::root().child(ObjectKind::Table, "t")
    }

    #[test]
    fn single_source_cases() {
        assert_eq!(
            single_source(&[
                col("a", Some(("t", "a"))),
                col("x", None),
                col("b", Some(("t", "b")))
            ])
            .unwrap()
            .table,
            "t"
        );
        assert_eq!(
            single_source(&[col("a", Some(("t", "a"))), col("b", Some(("u", "b")))]),
            Err(ResultNotEditableReason::MultipleTables)
        );
        assert_eq!(
            single_source(&[col("x", None)]),
            Err(ResultNotEditableReason::NoOrigin)
        );
        assert_eq!(single_source(&[]), Err(ResultNotEditableReason::NoOrigin));
    }

    #[test]
    fn two_instances_of_one_table_are_two_sources() {
        let mut p = col("pid", Some(("tree", "id")));
        let mut c = col("name", Some(("tree", "name")));
        p.origin.as_mut().unwrap().alias = Some("p".into());
        c.origin.as_mut().unwrap().alias = Some("c".into());
        assert_eq!(
            single_source(&[p.clone(), c]),
            Err(ResultNotEditableReason::MultipleTables)
        );
        let mut p2 = col("pname", Some(("tree", "name")));
        p2.origin.as_mut().unwrap().alias = Some("p".into());
        assert!(single_source(&[p, p2]).is_ok());
    }

    #[test]
    fn editable_with_alias_expression_and_readonly() {
        let cols = [
            col("ident", Some(("t", "id"))),
            col("n", Some(("t", "n"))),
            col("twice", None),
            col("gen", Some(("t", "gen"))),
        ];
        let r = resolve_result_editability(&cols, path(), ed(true, &["id"], &["gen"], None), false);
        assert!(r.editable && r.reason.is_none());
        assert_eq!(
            r.column_map,
            vec![
                Some("id".into()),
                Some("n".into()),
                None,
                Some("gen".into())
            ]
        );
        assert_eq!(r.key_columns, vec!["id"]);
        assert_eq!(r.read_only_columns, vec!["gen"]);
        assert!(r.missing_key_columns.is_empty());
    }

    #[test]
    fn key_not_in_result_names_what_is_missing() {
        let cols = [col("a", Some(("t", "a")))];
        let r = resolve_result_editability(&cols, path(), ed(true, &["id", "b"], &[], None), false);
        assert!(!r.editable);
        assert_eq!(r.reason, Some(ResultNotEditableReason::KeyNotInResult));
        assert_eq!(r.missing_key_columns, vec!["id", "b"]);
        // The columns stay mapped so the grid can still show them as such.
        assert_eq!(r.column_map, vec![Some("a".into())]);
        // A composite key is complete only with every part.
        let cols = [col("id", Some(("t", "id"))), col("b", Some(("t", "b")))];
        assert!(
            resolve_result_editability(&cols, path(), ed(true, &["id", "b"], &[], None), false)
                .editable
        );
    }

    #[test]
    fn table_reasons_pass_through() {
        let cols = [col("id", Some(("t", "id")))];
        for (from, to) in [
            (
                NotEditableReason::NoUniqueKey,
                ResultNotEditableReason::NoUniqueKey,
            ),
            (
                NotEditableReason::NotATable,
                ResultNotEditableReason::NotATable,
            ),
            (
                NotEditableReason::ReadOnlyConnection,
                ResultNotEditableReason::ReadOnlyConnection,
            ),
            (
                NotEditableReason::Unsupported,
                ResultNotEditableReason::Unsupported,
            ),
        ] {
            let r =
                resolve_result_editability(&cols, path(), ed(false, &[], &[], Some(from)), false);
            assert!(!r.editable);
            assert_eq!(r.reason, Some(to));
        }
    }

    #[test]
    fn key_spelling_wins_and_duplicates_stay_readonly() {
        let cols = [
            col("ID", Some(("t", "ID"))),
            col("again", Some(("t", "Id"))),
            col("Name", Some(("t", "Name"))),
        ];
        let r = resolve_result_editability(&cols, path(), ed(true, &["id"], &[], None), true);
        assert!(r.editable);
        assert_eq!(
            r.column_map,
            vec![Some("id".into()), None, Some("Name".into())]
        );
        // Names reported by the server are exact: "ID" is not the key `id`.
        let cols = [col("ID", Some(("t", "ID"))), col("id2", Some(("t", "id")))];
        let r = resolve_result_editability(&cols, path(), ed(true, &["id"], &[], None), false);
        assert_eq!(r.column_map, vec![Some("ID".into()), Some("id".into())]);
        assert!(r.editable);
        let r = resolve_result_editability(&cols[..1], path(), ed(true, &["id"], &[], None), false);
        assert_eq!(r.reason, Some(ResultNotEditableReason::KeyNotInResult));
    }

    #[test]
    fn paths_per_server_kind() {
        let src = SourceTable {
            database: None,
            schema: None,
            table: "t".into(),
        };
        let ctx = PathContext {
            database: Some("app".into()),
            schema: Some("sch".into()),
            user: Some("scott".into()),
        };
        let names = |p: Option<ObjectPath>| {
            p.unwrap()
                .segments
                .iter()
                .map(|s| s.name.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(table_path(DbKind::Postgres, &src, &ctx)),
            ["app", "sch", "t"]
        );
        assert_eq!(names(table_path(DbKind::MySql, &src, &ctx)), ["app", "t"]);
        assert_eq!(
            names(table_path(
                DbKind::SqlServer,
                &src,
                &PathContext {
                    schema: None,
                    ..ctx.clone()
                }
            )),
            ["app", "dbo", "t"]
        );
        assert_eq!(
            names(table_path(
                DbKind::Oracle,
                &src,
                &PathContext {
                    schema: None,
                    ..ctx.clone()
                }
            )),
            ["SCOTT", "t"]
        );
        assert_eq!(
            names(table_path(DbKind::ClickHouse, &src, &ctx)),
            ["app", "t"]
        );
        assert!(table_path(DbKind::MongoDb, &src, &ctx).is_none());
        assert!(table_path(DbKind::Redis, &src, &ctx).is_none());
        // An origin's own database / schema win over the connection's.
        let src = SourceTable {
            database: Some("other".into()),
            schema: Some("x".into()),
            table: "t".into(),
        };
        assert_eq!(
            names(table_path(DbKind::Postgres, &src, &ctx)),
            ["other", "x", "t"]
        );
        // No database anywhere: no path.
        assert!(table_path(DbKind::Postgres, &src, &PathContext::default()).is_some());
        let none = SourceTable {
            database: None,
            schema: None,
            table: "t".into(),
        };
        assert!(table_path(DbKind::MySql, &none, &PathContext::default()).is_none());
    }

    #[test]
    fn json_shape() {
        let r = ResultEditability::not_editable(2, ResultNotEditableReason::MultipleTables);
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["reason"], "multipleTables");
        assert_eq!(v["columnMap"], serde_json::json!([null, null]));
        assert_eq!(v["keyColumns"], serde_json::json!([]));
    }
}
