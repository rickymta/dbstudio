//! Format writers: SQL, CSV, JSON and XLSX.

use std::collections::HashSet;
use std::path::PathBuf;

use dbstudio_core::{ColumnDetail, ColumnMeta, ObjectKind, ObjectPath, Result, ScriptDialect};

use crate::{BinaryFormat, ExportContent, ExportFormat, ExportTuning, ObjectMeta, TableWriter};

mod csv;
mod json;
mod sql;
mod xlsx;

pub(crate) use csv::CsvWriter;
pub(crate) use json::JsonWriter;
pub(crate) use sql::{SqlWriter, sql_literal};
pub(crate) use xlsx::XlsxWriter;

/// Everything a writer needs to know up front.
#[derive(Debug, Clone)]
pub(crate) struct WriterConfig {
    pub format: ExportFormat,
    pub dialect: ScriptDialect,
    pub binary: BinaryFormat,
    pub content: ExportContent,
    pub dest: PathBuf,
    pub overwrite: bool,
    /// The output is a zip archive holding several files.
    pub zip: bool,
    pub tuning: ExportTuning,
}

/// Open the writer of `cfg.format`; the temporary output file is created right away.
pub(crate) fn create_writer(cfg: WriterConfig) -> Result<Box<dyn TableWriter>> {
    Ok(match cfg.format.clone() {
        ExportFormat::Sql(o) => Box::new(SqlWriter::create(&cfg, o)?),
        ExportFormat::Csv(o) => Box::new(CsvWriter::create(&cfg, o)?),
        ExportFormat::Json(o) => Box::new(JsonWriter::create(&cfg, o)?),
        ExportFormat::Xlsx(o) => Box::new(XlsxWriter::create(&cfg, o)?),
    })
}

/// Schema part of an exported object, collected for dictionaries and schema files.
#[derive(Debug, Clone)]
pub(crate) struct SchemaEntry {
    /// Display name (`schema.table` or the table name).
    pub name: String,
    pub schema: Option<String>,
    pub kind: ObjectKind,
    pub comment: Option<String>,
    pub columns: Vec<ColumnDetail>,
    pub primary_key: Vec<String>,
    pub unique_keys: Vec<Vec<String>>,
    pub estimated_rows: Option<u64>,
}

impl SchemaEntry {
    pub fn from_meta(meta: &ObjectMeta) -> SchemaEntry {
        match &meta.details {
            Some(t) => SchemaEntry {
                name: meta.name.clone(),
                schema: t.schema.clone(),
                kind: t.kind,
                comment: t.comment.clone(),
                columns: t.columns.clone(),
                primary_key: t.primary_key.clone(),
                unique_keys: t.unique_keys.clone(),
                estimated_rows: t.estimated_rows,
            },
            None => SchemaEntry {
                name: meta.name.clone(),
                schema: None,
                kind: ObjectKind::Table,
                comment: None,
                columns: columns_from_meta(&meta.columns),
                primary_key: Vec::new(),
                unique_keys: Vec::new(),
                estimated_rows: meta.estimated_rows,
            },
        }
    }

    /// Rows of the dictionary: table, column, position, type, nullable, default, primary key,
    /// comment.
    pub fn dictionary_rows(&self) -> Vec<[String; 8]> {
        self.columns
            .iter()
            .map(|c| {
                [
                    self.name.clone(),
                    c.name.clone(),
                    c.ordinal.to_string(),
                    c.type_name.clone(),
                    if c.nullable { "YES" } else { "NO" }.to_string(),
                    c.default.clone().unwrap_or_default(),
                    if c.primary_key { "YES" } else { "" }.to_string(),
                    c.comment.clone().unwrap_or_default(),
                ]
            })
            .collect()
    }
}

pub(crate) const DICTIONARY_HEADER: [&str; 8] = [
    "Table",
    "Column",
    "Position",
    "Type",
    "Nullable",
    "Default",
    "Primary key",
    "Comment",
];

fn columns_from_meta(columns: &[ColumnMeta]) -> Vec<ColumnDetail> {
    columns
        .iter()
        .enumerate()
        .map(|(i, c)| ColumnDetail {
            name: c.name.clone(),
            ordinal: (i + 1) as u32,
            type_name: c.type_name.clone(),
            value_kind: c.value_kind,
            nullable: c.nullable.unwrap_or(true),
            default: None,
            comment: None,
            primary_key: false,
            extra: None,
        })
        .collect()
}

/// Column names made unique: the second `id` becomes `id#2`, the third `id#3`.
pub(crate) fn dedupe_names<'a>(names: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut seen: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    let mut out: Vec<String> = Vec::new();
    for n in names {
        let count = seen.entry(n.to_string()).or_insert(0);
        *count += 1;
        out.push(if *count == 1 {
            n.to_string()
        } else {
            format!("{n}#{count}")
        });
    }
    // A generated name may clash with a real one; make those unique too.
    let mut taken: std::collections::HashSet<String> = std::collections::HashSet::new();
    for n in &mut out {
        while !taken.insert(n.clone()) {
            n.push('_');
        }
    }
    out
}

/// Hands out names that are unique (case-insensitively) within one output.
#[derive(Debug, Default)]
pub(crate) struct NameSet(HashSet<String>);

impl NameSet {
    /// `base`, or `base_2`, `base_3`... when taken. `max_len` bounds the whole name in chars.
    pub fn unique(&mut self, base: &str, max_len: usize) -> String {
        let clip = |s: &str, n: usize| s.chars().take(n).collect::<String>();
        let first = clip(base, max_len);
        if self.0.insert(first.to_lowercase()) {
            return first;
        }
        for n in 2.. {
            let suffix = format!("_{n}");
            let candidate = format!(
                "{}{suffix}",
                clip(base, max_len.saturating_sub(suffix.chars().count()))
            );
            if self.0.insert(candidate.to_lowercase()) {
                return candidate;
            }
        }
        unreachable!()
    }
}

/// Schema qualifier of a table: its schema, else the last `Schema` segment of its path, else
/// (MySQL, where the database is the schema) its database.
pub(crate) fn qualifier(
    path: Option<&ObjectPath>,
    schema: Option<&str>,
    dialect: ScriptDialect,
) -> Option<String> {
    if let Some(s) = schema.filter(|s| !s.is_empty()) {
        return Some(s.to_string());
    }
    let path = path?;
    if let Some(s) = path
        .segments
        .iter()
        .rev()
        .find(|s| s.kind == ObjectKind::Schema)
    {
        return Some(s.name.clone());
    }
    if matches!(dialect, ScriptDialect::MySql { .. }) {
        return path
            .segments
            .iter()
            .find(|s| s.kind == ObjectKind::Database)
            .map(|s| s.name.clone());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_names() {
        let mut n = NameSet::default();
        assert_eq!(n.unique("orders", 31), "orders");
        assert_eq!(n.unique("Orders", 31), "Orders_2");
        assert_eq!(n.unique("orders", 31), "orders_3");
        let long = "x".repeat(40);
        assert_eq!(n.unique(&long, 10).len(), 10);
        assert_eq!(n.unique(&long, 10).len(), 10);
    }

    #[test]
    fn duplicate_columns_are_numbered() {
        assert_eq!(
            dedupe_names(["id", "name", "id", "id", "id#2"].into_iter()),
            ["id", "name", "id#2", "id#3", "id#2_"]
        );
    }

    #[test]
    fn qualifiers() {
        let p = ObjectPath::root()
            .child(ObjectKind::Database, "d")
            .child(ObjectKind::Schema, "s")
            .child(ObjectKind::Folder, "tables")
            .child(ObjectKind::Table, "t");
        assert_eq!(
            qualifier(Some(&p), Some("x"), ScriptDialect::Postgres).as_deref(),
            Some("x")
        );
        assert_eq!(
            qualifier(Some(&p), None, ScriptDialect::Postgres).as_deref(),
            Some("s")
        );
        let m = ObjectPath::root()
            .child(ObjectKind::Database, "d")
            .child(ObjectKind::Folder, "tables")
            .child(ObjectKind::Table, "t");
        let my = ScriptDialect::MySql {
            mariadb: false,
            backslash_escapes: true,
        };
        assert_eq!(qualifier(Some(&m), None, my).as_deref(), Some("d"));
        assert_eq!(qualifier(Some(&m), None, ScriptDialect::Postgres), None);
        assert_eq!(qualifier(None, None, my), None);
    }
}
