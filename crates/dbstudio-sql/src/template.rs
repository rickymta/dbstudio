//! Script templates for a table: `SELECT` / `INSERT` / `UPDATE` / `DELETE` (BROWSE-05).
//!
//! Pure text generation per dialect: identifiers are quoted (special names included), `SELECT`
//! lists the columns and limits the rows (`LIMIT 100`, `TOP 100`, `FETCH FIRST 100 ROWS ONLY`),
//! `INSERT` / `UPDATE` / `DELETE` use one `:column` parameter per column (`@column` in T-SQL) and address the row by
//! the primary key (or the first complete unique index). MongoDB gets shell calls whose fields
//! are the inferred ones. Dialects without a table language (Redis, Opaque) give an empty text.

use dbstudio_core::{ColumnDetail, ObjectKind, ObjectPath, ScriptDialect, TableDetails, ValueKind};

use crate::dialect::Dialect;
use serde::{Deserialize, Serialize};

/// Which template to generate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ScriptTemplate {
    /// `SELECT` of the columns with a row limit.
    Select,
    /// `INSERT` with one parameter per column.
    Insert,
    /// `UPDATE` addressed by the primary / unique key.
    Update,
    /// `DELETE` addressed by the primary / unique key.
    Delete,
}

/// Rows returned by the `SELECT` template.
const LIMIT: u32 = 100;

/// Pure text generation: the script for `template` on the table `path` described by `details`,
/// written in `dialect` (quoting, row limit syntax, parameter style).
pub fn generate(
    dialect: ScriptDialect,
    details: &TableDetails,
    path: &ObjectPath,
    template: ScriptTemplate,
) -> String {
    let path = if path.is_root() { &details.path } else { path };
    match dialect {
        ScriptDialect::MongoShell => mongo(details, path, template),
        ScriptDialect::RedisCli | ScriptDialect::Opaque => String::new(),
        d => sql(d, details, path, template),
    }
}

/// Last two non-folder segments (`schema.table`, `database.table`, `keyspace.table`).
fn qualified_names(path: &ObjectPath) -> Vec<&str> {
    let names: Vec<&str> = path
        .segments
        .iter()
        .filter(|s| !matches!(s.kind, ObjectKind::Folder))
        .map(|s| s.name.as_str())
        .collect();
    names[names.len().saturating_sub(2)..].to_vec()
}

fn is_generated(c: &ColumnDetail) -> bool {
    c.extra
        .as_deref()
        .is_some_and(|e| e.to_ascii_lowercase().contains("generated"))
}

fn is_auto_increment(c: &ColumnDetail) -> bool {
    c.extra
        .as_deref()
        .is_some_and(|e| e.to_ascii_lowercase().contains("auto_increment"))
}

/// Columns that identify a row: the primary key, else the first complete unique index.
fn key_columns(details: &TableDetails) -> Vec<&ColumnDetail> {
    let find = |names: &[String]| -> Option<Vec<&ColumnDetail>> {
        names
            .iter()
            .map(|n| details.columns.iter().find(|c| &c.name == n))
            .collect()
    };
    let index_key = |primary: bool| {
        details
            .indexes
            .iter()
            .filter(|i| !i.partial && !i.columns.is_empty() && i.primary == primary)
            .filter(|i| primary || i.unique)
            .find_map(|i| find(&i.columns))
    };
    if let Some(k) = index_key(true) {
        return k;
    }
    let pk: Vec<&ColumnDetail> = details.columns.iter().filter(|c| c.primary_key).collect();
    if !pk.is_empty() {
        return pk;
    }
    index_key(false).unwrap_or_default()
}

/// `:name` parameter names, one per column of `cols`: letters, digits and `_` only, unique.
fn param_names(cols: &[&ColumnDetail]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(cols.len());
    for c in cols {
        let mut base: String = c
            .name
            .chars()
            .map(|ch| {
                if ch.is_alphanumeric() || ch == '_' {
                    ch
                } else {
                    '_'
                }
            })
            .collect();
        if base.is_empty() || base.starts_with(|ch: char| ch.is_ascii_digit()) {
            base.insert(0, 'p');
        }
        let mut name = base.clone();
        let mut n = 2;
        while out.contains(&name) {
            name = format!("{base}_{n}");
            n += 1;
        }
        out.push(name);
    }
    out
}

fn sql(
    d: ScriptDialect,
    details: &TableDetails,
    path: &ObjectPath,
    template: ScriptTemplate,
) -> String {
    let q = Dialect::new(d);
    let table = qualified_names(path)
        .iter()
        .map(|n| q.quote_ident(n))
        .collect::<Vec<_>>()
        .join(".");
    let all: Vec<&ColumnDetail> = details.columns.iter().collect();
    let params = param_names(&all);
    let param_of = |c: &ColumnDetail| -> String {
        let i = details
            .columns
            .iter()
            .position(|x| x.name == c.name)
            .unwrap_or(0);
        // T-SQL variables are `@name`; every other dialect binds `:name`.
        let sigil = if matches!(d, ScriptDialect::MsSql) {
            '@'
        } else {
            ':'
        };
        format!("{sigil}{}", params[i])
    };
    let ident = |c: &ColumnDetail| q.quote_ident(&c.name);
    let key = key_columns(details);
    let where_clause = if key.is_empty() {
        "WHERE 1 = 0 /* replace with the condition that picks the row */".to_string()
    } else {
        format!(
            "WHERE {}",
            key.iter()
                .map(|c| format!("{} = {}", ident(c), param_of(c)))
                .collect::<Vec<_>>()
                .join("\n  AND ")
        )
    };
    let clickhouse = matches!(d, ScriptDialect::ClickHouse);
    // Warnings written into the generated text (comments only, the statement is unchanged).
    let warning = if clickhouse && !key.is_empty() {
        // The primary key of a MergeTree table does not identify one row.
        "-- ClickHouse primary keys are not unique: every row with this key is affected.\n"
    } else if matches!(d, ScriptDialect::Cql)
        && details.columns.iter().any(|c| {
            c.extra
                .as_deref()
                .is_some_and(|e| e.to_ascii_lowercase().contains("clustering"))
                && !key.iter().any(|k| k.name == c.name)
        })
    {
        // Only the partition key is addressed: the whole partition goes.
        "-- The clustering columns are not in the WHERE: this affects the whole partition.\n"
    } else {
        ""
    };
    match template {
        ScriptTemplate::Select => {
            let cols = if all.is_empty() {
                vec!["*".to_string()]
            } else {
                all.iter().map(|c| ident(c)).collect()
            };
            let list = cols.join(",\n  ");
            match d {
                ScriptDialect::MsSql => {
                    format!("SELECT TOP {LIMIT}\n  {list}\nFROM {table};")
                }
                ScriptDialect::Oracle => {
                    format!("SELECT\n  {list}\nFROM {table}\nFETCH FIRST {LIMIT} ROWS ONLY;")
                }
                _ => format!("SELECT\n  {list}\nFROM {table}\nLIMIT {LIMIT};"),
            }
        }
        ScriptTemplate::Insert => {
            let cols: Vec<&ColumnDetail> = all
                .iter()
                .copied()
                .filter(|c| !is_generated(c) && !is_auto_increment(c))
                .collect();
            let cols = if cols.is_empty() { all.clone() } else { cols };
            let names = cols.iter().map(|c| ident(c)).collect::<Vec<_>>().join(", ");
            let values = cols
                .iter()
                .map(|c| param_of(c))
                .collect::<Vec<_>>()
                .join(", ");
            format!("INSERT INTO {table} ({names})\nVALUES ({values});")
        }
        ScriptTemplate::Update => {
            let mut cols: Vec<&ColumnDetail> = all
                .iter()
                .copied()
                .filter(|c| !is_generated(c) && !key.iter().any(|k| k.name == c.name))
                .collect();
            if cols.is_empty() {
                cols = all.iter().copied().filter(|c| !is_generated(c)).collect();
            }
            let assignments: Vec<String> = cols
                .iter()
                .map(|c| format!("{} = {}", ident(c), param_of(c)))
                .collect();
            if clickhouse {
                format!(
                    "{warning}ALTER TABLE {table}\n  UPDATE {}\n{where_clause};",
                    assignments.join(", ")
                )
            } else {
                format!(
                    "{warning}UPDATE {table}\nSET {}\n{where_clause};",
                    assignments.join(",\n    ")
                )
            }
        }
        ScriptTemplate::Delete => {
            if clickhouse {
                format!("{warning}ALTER TABLE {table}\n  DELETE\n{where_clause};")
            } else {
                format!("{warning}DELETE FROM {table}\n{where_clause};")
            }
        }
    }
}

/// A JavaScript identifier-safe property key, quoted when it is not a plain identifier.
fn js_key(name: &str) -> String {
    let plain = name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
    if plain {
        name.to_string()
    } else {
        js_string(name)
    }
}

fn js_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Placeholder value for a field, from its inferred kind / type name.
fn mongo_sample(c: &ColumnDetail) -> &'static str {
    let t = c.type_name.to_ascii_lowercase();
    if t.contains("objectid") {
        return "ObjectId()";
    }
    match c.value_kind {
        ValueKind::Bool => "false",
        ValueKind::Int | ValueKind::UInt | ValueKind::Float => "0",
        ValueKind::Decimal => "NumberDecimal(\"0\")",
        ValueKind::Date | ValueKind::DateTime | ValueKind::Timestamp => "new Date()",
        ValueKind::Uuid => "UUID()",
        ValueKind::Array => "[]",
        ValueKind::Document | ValueKind::Json => "{}",
        ValueKind::Null => "null",
        _ => "\"\"",
    }
}

fn mongo(details: &TableDetails, path: &ObjectPath, template: ScriptTemplate) -> String {
    let names = qualified_names(path);
    let coll = names.last().copied().unwrap_or("collection");
    let target = if coll.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && coll.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        format!("db.{coll}")
    } else {
        format!("db.getCollection({})", js_string(coll))
    };
    let id = details.columns.iter().find(|c| c.name == "_id");
    let filter = match id {
        Some(c) => format!("{{ _id: {} }}", mongo_sample(c)),
        None => {
            let key = key_columns(details);
            if key.is_empty() {
                "{ _id: ObjectId() }".to_string()
            } else {
                let f: Vec<String> = key
                    .iter()
                    .map(|c| format!("{}: {}", js_key(&c.name), mongo_sample(c)))
                    .collect();
                format!("{{ {} }}", f.join(", "))
            }
        }
    };
    let fields: Vec<&ColumnDetail> = details.columns.iter().filter(|c| c.name != "_id").collect();
    let body = |indent: &str| {
        fields
            .iter()
            .map(|c| format!("{indent}{}: {}", js_key(&c.name), mongo_sample(c)))
            .collect::<Vec<_>>()
            .join(",\n")
    };
    match template {
        ScriptTemplate::Select => format!("{target}.find({{}}).limit({LIMIT})"),
        ScriptTemplate::Insert if fields.is_empty() => format!("{target}.insertOne({{}})"),
        ScriptTemplate::Insert => format!("{target}.insertOne({{\n{}\n}})", body("  ")),
        ScriptTemplate::Update if fields.is_empty() => {
            format!("{target}.updateOne({filter}, {{ $set: {{}} }})")
        }
        ScriptTemplate::Update => format!(
            "{target}.updateOne(\n  {filter},\n  {{ $set: {{\n{}\n  }} }}\n)",
            body("    ")
        ),
        ScriptTemplate::Delete => format!("{target}.deleteOne({filter})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_names_match_the_ipc_contract() {
        for (t, s) in [
            (ScriptTemplate::Select, "select"),
            (ScriptTemplate::Insert, "insert"),
            (ScriptTemplate::Update, "update"),
            (ScriptTemplate::Delete, "delete"),
        ] {
            assert_eq!(serde_json::to_value(t).unwrap(), s);
        }
    }

    fn col(name: &str, kind: ValueKind, ty: &str, pk: bool) -> ColumnDetail {
        ColumnDetail {
            name: name.into(),
            ordinal: 1,
            type_name: ty.into(),
            value_kind: kind,
            nullable: !pk,
            default: None,
            comment: None,
            primary_key: pk,
            extra: None,
        }
    }

    fn details(path: &ObjectPath, pk_index: bool) -> TableDetails {
        TableDetails {
            path: path.clone(),
            kind: ObjectKind::Table,
            comment: None,
            columns: vec![
                col("id", ValueKind::Int, "bigint", true),
                col("user name", ValueKind::Text, "text", false),
                col("e-mail", ValueKind::Text, "text", false),
            ],
            indexes: if pk_index {
                vec![dbstudio_core::IndexDetail {
                    name: "pk".into(),
                    columns: vec!["id".into()],
                    unique: true,
                    primary: true,
                    method: None,
                    definition: None,
                    partial: false,
                }]
            } else {
                Vec::new()
            },
            foreign_keys: Vec::new(),
            estimated_rows: None,
            size_bytes: None,
        }
    }

    fn path_of(segments: &[(ObjectKind, &str)]) -> ObjectPath {
        segments
            .iter()
            .fold(ObjectPath::root(), |p, (k, n)| p.child(*k, *n))
    }

    fn all(d: ScriptDialect, path: &ObjectPath) -> Vec<String> {
        let det = details(path, true);
        [
            ScriptTemplate::Select,
            ScriptTemplate::Insert,
            ScriptTemplate::Update,
            ScriptTemplate::Delete,
        ]
        .iter()
        .map(|t| generate(d, &det, path, *t))
        .collect()
    }

    /// Expected output per dialect: SELECT, INSERT, UPDATE, DELETE.
    const SNAPSHOTS: &[(&str, [&str; 4])] = &[
        (
            "pg",
            [
                "SELECT\n  \"id\",\n  \"user name\",\n  \"e-mail\"\nFROM \"public\".\"my table\"\nLIMIT 100;",
                "INSERT INTO \"public\".\"my table\" (\"id\", \"user name\", \"e-mail\")\nVALUES (:id, :user_name, :e_mail);",
                "UPDATE \"public\".\"my table\"\nSET \"user name\" = :user_name,\n    \"e-mail\" = :e_mail\nWHERE \"id\" = :id;",
                "DELETE FROM \"public\".\"my table\"\nWHERE \"id\" = :id;",
            ],
        ),
        (
            "my",
            [
                "SELECT\n  `id`,\n  `user name`,\n  `e-mail`\nFROM `app`.`my table`\nLIMIT 100;",
                "INSERT INTO `app`.`my table` (`id`, `user name`, `e-mail`)\nVALUES (:id, :user_name, :e_mail);",
                "UPDATE `app`.`my table`\nSET `user name` = :user_name,\n    `e-mail` = :e_mail\nWHERE `id` = :id;",
                "DELETE FROM `app`.`my table`\nWHERE `id` = :id;",
            ],
        ),
        (
            "maria",
            [
                "SELECT\n  `id`,\n  `user name`,\n  `e-mail`\nFROM `app`.`my table`\nLIMIT 100;",
                "INSERT INTO `app`.`my table` (`id`, `user name`, `e-mail`)\nVALUES (:id, :user_name, :e_mail);",
                "UPDATE `app`.`my table`\nSET `user name` = :user_name,\n    `e-mail` = :e_mail\nWHERE `id` = :id;",
                "DELETE FROM `app`.`my table`\nWHERE `id` = :id;",
            ],
        ),
        (
            "ms",
            [
                "SELECT TOP 100\n  [id],\n  [user name],\n  [e-mail]\nFROM [dbo].[my table];",
                "INSERT INTO [dbo].[my table] ([id], [user name], [e-mail])\nVALUES (@id, @user_name, @e_mail);",
                "UPDATE [dbo].[my table]\nSET [user name] = @user_name,\n    [e-mail] = @e_mail\nWHERE [id] = @id;",
                "DELETE FROM [dbo].[my table]\nWHERE [id] = @id;",
            ],
        ),
        (
            "ora",
            [
                "SELECT\n  \"id\",\n  \"user name\",\n  \"e-mail\"\nFROM \"HR\".\"my table\"\nFETCH FIRST 100 ROWS ONLY;",
                "INSERT INTO \"HR\".\"my table\" (\"id\", \"user name\", \"e-mail\")\nVALUES (:id, :user_name, :e_mail);",
                "UPDATE \"HR\".\"my table\"\nSET \"user name\" = :user_name,\n    \"e-mail\" = :e_mail\nWHERE \"id\" = :id;",
                "DELETE FROM \"HR\".\"my table\"\nWHERE \"id\" = :id;",
            ],
        ),
        (
            "mongo",
            [
                "db.getCollection(\"my table\").find({}).limit(100)",
                "db.getCollection(\"my table\").insertOne({\n  id: 0,\n  \"user name\": \"\",\n  \"e-mail\": \"\"\n})",
                "db.getCollection(\"my table\").updateOne(\n  { id: 0 },\n  { $set: {\n    id: 0,\n    \"user name\": \"\",\n    \"e-mail\": \"\"\n  } }\n)",
                "db.getCollection(\"my table\").deleteOne({ id: 0 })",
            ],
        ),
        (
            "cql",
            [
                "SELECT\n  \"id\",\n  \"user name\",\n  \"e-mail\"\nFROM \"ks\".\"my table\"\nLIMIT 100;",
                "INSERT INTO \"ks\".\"my table\" (\"id\", \"user name\", \"e-mail\")\nVALUES (:id, :user_name, :e_mail);",
                "UPDATE \"ks\".\"my table\"\nSET \"user name\" = :user_name,\n    \"e-mail\" = :e_mail\nWHERE \"id\" = :id;",
                "DELETE FROM \"ks\".\"my table\"\nWHERE \"id\" = :id;",
            ],
        ),
        (
            "ch",
            [
                "SELECT\n  `id`,\n  `user name`,\n  `e-mail`\nFROM `default`.`my table`\nLIMIT 100;",
                "INSERT INTO `default`.`my table` (`id`, `user name`, `e-mail`)\nVALUES (:id, :user_name, :e_mail);",
                "-- ClickHouse primary keys are not unique: every row with this key is affected.\nALTER TABLE `default`.`my table`\n  UPDATE `user name` = :user_name, `e-mail` = :e_mail\nWHERE `id` = :id;",
                "-- ClickHouse primary keys are not unique: every row with this key is affected.\nALTER TABLE `default`.`my table`\n  DELETE\nWHERE `id` = :id;",
            ],
        ),
    ];

    fn case(name: &str) -> (ScriptDialect, ObjectPath) {
        use ObjectKind as K;
        match name {
            "pg" => (
                ScriptDialect::Postgres,
                path_of(&[
                    (K::Database, "app"),
                    (K::Schema, "public"),
                    (K::Folder, "tables"),
                    (K::Table, "my table"),
                ]),
            ),
            "my" => (
                ScriptDialect::mysql(false, true),
                path_of(&[
                    (K::Database, "app"),
                    (K::Folder, "tables"),
                    (K::Table, "my table"),
                ]),
            ),
            "maria" => (
                ScriptDialect::mysql(true, true),
                path_of(&[
                    (K::Database, "app"),
                    (K::Folder, "tables"),
                    (K::Table, "my table"),
                ]),
            ),
            "ms" => (
                ScriptDialect::MsSql,
                path_of(&[
                    (K::Database, "app"),
                    (K::Schema, "dbo"),
                    (K::Table, "my table"),
                ]),
            ),
            "ora" => (
                ScriptDialect::Oracle,
                path_of(&[(K::Schema, "HR"), (K::Table, "my table")]),
            ),
            "mongo" => (
                ScriptDialect::MongoShell,
                path_of(&[
                    (K::Database, "app"),
                    (K::Folder, "collections"),
                    (K::Collection, "my table"),
                ]),
            ),
            "cql" => (
                ScriptDialect::Cql,
                path_of(&[(K::Database, "ks"), (K::Table, "my table")]),
            ),
            "ch" => (
                ScriptDialect::ClickHouse,
                path_of(&[(K::Database, "default"), (K::Table, "my table")]),
            ),
            other => panic!("{other}"),
        }
    }

    #[test]
    fn snapshots_for_8_dialects_x_4_templates() {
        assert_eq!(SNAPSHOTS.len(), 8);
        for (name, want) in SNAPSHOTS {
            let (d, p) = case(name);
            assert_eq!(&all(d, &p)[..], &want[..], "{name}");
        }
    }

    #[test]
    fn generated_sql_is_understood_by_the_analyzer() {
        use dbstudio_core::StatementClass as C;
        for (name, _) in SNAPSHOTS.iter().filter(|(n, _)| *n != "mongo") {
            let (d, p) = case(name);
            let texts = all(d, &p);
            let want = [C::Read, C::Write, C::Write, C::Write];
            for (text, want) in texts.iter().zip(want) {
                let a = crate::analyze(d, text);
                assert_eq!(a.statements.len(), 1, "{name}: {text}");
                assert!(a.problems.is_empty(), "{name}: {text}");
                assert_eq!(a.statements[0].class, want, "{name}: {text}");
                // addressed by the key: never a destructive template
                assert_eq!(a.statements[0].destructive, None, "{name}: {text}");
            }
            // the parameters are found (T-SQL variables or `:name`)
            let params = crate::find_params(d, &texts[2]);
            let names: Vec<&str> = params.iter().map(|p| p.name.as_str()).collect();
            let sigil = if name == &"ms" { "@" } else { ":" };
            assert_eq!(
                names,
                [
                    format!("{sigil}user_name"),
                    format!("{sigil}e_mail"),
                    format!("{sigil}id")
                ],
                "{name}"
            );
        }
    }

    #[test]
    fn key_falls_back_to_unique_index_then_to_a_safe_placeholder() {
        let (d, p) = case("pg");
        let mut det = details(&p, false);
        for c in &mut det.columns {
            c.primary_key = false;
        }
        // no key at all: the WHERE can never match by accident
        let del = generate(d, &det, &p, ScriptTemplate::Delete);
        assert!(del.contains("WHERE 1 = 0 /*"), "{del}");
        // unique index
        det.indexes.push(dbstudio_core::IndexDetail {
            name: "u".into(),
            columns: vec!["e-mail".into()],
            unique: true,
            primary: false,
            method: None,
            definition: None,
            partial: false,
        });
        let del = generate(d, &det, &p, ScriptTemplate::Delete);
        assert!(del.ends_with("WHERE \"e-mail\" = :e_mail;"), "{del}");
        // a partial unique index identifies nothing
        det.indexes[0].partial = true;
        assert!(generate(d, &det, &p, ScriptTemplate::Delete).contains("1 = 0"));
        // a composite primary key lists every column
        let mut det = details(&p, false);
        det.columns[1].primary_key = true;
        let del = generate(d, &det, &p, ScriptTemplate::Delete);
        assert!(
            del.contains("\"id\" = :id\n  AND \"user name\" = :user_name"),
            "{del}"
        );
    }

    #[test]
    fn generated_and_auto_increment_columns_are_not_inserted_or_updated() {
        let (d, p) = case("my");
        let mut det = details(&p, true);
        det.columns[0].extra = Some("auto_increment".into());
        det.columns[2].extra = Some("VIRTUAL GENERATED".into());
        let ins = generate(d, &det, &p, ScriptTemplate::Insert);
        assert_eq!(
            ins,
            "INSERT INTO `app`.`my table` (`user name`)\nVALUES (:user_name);"
        );
        let upd = generate(d, &det, &p, ScriptTemplate::Update);
        assert!(!upd.contains("e-mail"), "{upd}");
    }

    #[test]
    fn parameter_names_are_unique_and_valid() {
        let (d, p) = case("pg");
        let mut det = details(&p, true);
        det.columns = vec![
            col("a b", ValueKind::Text, "text", true),
            col("a_b", ValueKind::Text, "text", false),
            col("1st", ValueKind::Text, "text", false),
            col("日本", ValueKind::Text, "text", false),
        ];
        det.indexes.clear();
        let ins = generate(d, &det, &p, ScriptTemplate::Insert);
        assert!(
            ins.ends_with("VALUES (:a_b, :a_b_2, :p1st, :日本);"),
            "{ins}"
        );
        let params = crate::find_params(d, &ins);
        assert_eq!(params.len(), 4);
    }

    #[test]
    fn mongo_templates() {
        let (d, p) = case("mongo");
        let mut det = details(&p, false);
        det.columns = vec![
            col("_id", ValueKind::Ext, "objectId", true),
            col("name", ValueKind::Text, "string", false),
            col("age", ValueKind::Int, "int", false),
            col("tags", ValueKind::Array, "array", false),
            col("at", ValueKind::DateTime, "date", false),
        ];
        let plain = ObjectPath::root()
            .child(ObjectKind::Database, "app")
            .child(ObjectKind::Collection, "users");
        assert_eq!(
            generate(d, &det, &plain, ScriptTemplate::Select),
            "db.users.find({}).limit(100)"
        );
        assert_eq!(
            generate(d, &det, &plain, ScriptTemplate::Insert),
            "db.users.insertOne({\n  name: \"\",\n  age: 0,\n  tags: [],\n  at: new Date()\n})"
        );
        assert_eq!(
            generate(d, &det, &plain, ScriptTemplate::Delete),
            "db.users.deleteOne({ _id: ObjectId() })"
        );
        let upd = generate(d, &det, &plain, ScriptTemplate::Update);
        assert!(
            upd.starts_with("db.users.updateOne(\n  { _id: ObjectId() },"),
            "{upd}"
        );
        assert!(!upd.contains("_id: ObjectId()\n"), "{upd}");
        det.columns.clear();
        assert_eq!(
            generate(d, &det, &plain, ScriptTemplate::Insert),
            "db.users.insertOne({})"
        );
        let quoted = ObjectPath::root().child(ObjectKind::Collection, "we\"ird");
        assert_eq!(
            generate(d, &det, &quoted, ScriptTemplate::Select),
            "db.getCollection(\"we\\\"ird\").find({}).limit(100)"
        );
    }

    #[test]
    fn dialects_without_a_table_language_give_nothing() {
        let (_, p) = case("pg");
        let det = details(&p, true);
        for d in [ScriptDialect::RedisCli, ScriptDialect::Opaque] {
            assert_eq!(generate(d, &det, &p, ScriptTemplate::Select), "");
        }
    }

    #[test]
    fn no_columns_selects_star_and_uses_the_details_path_for_the_root() {
        let (d, p) = case("pg");
        let mut det = details(&p, true);
        det.columns.clear();
        let root = ObjectPath::root();
        assert_eq!(
            generate(d, &det, &root, ScriptTemplate::Select),
            "SELECT\n  *\nFROM \"public\".\"my table\"\nLIMIT 100;"
        );
    }

    #[test]
    fn warnings_for_non_unique_keys() {
        let (d, p) = case("cql");
        let mut det = details(&p, true);
        det.columns[1].extra = Some("clustering".into());
        let del = generate(d, &det, &p, ScriptTemplate::Delete);
        assert!(
            del.starts_with("-- The clustering columns are not in the WHERE"),
            "{del}"
        );
        assert!(del.ends_with("WHERE \"id\" = :id;"), "{del}");
        det.columns[1].primary_key = true;
        det.indexes.clear();
        let del = generate(d, &det, &p, ScriptTemplate::Delete);
        assert!(del.starts_with("DELETE FROM"), "{del}");
        let (d, p) = case("ch");
        let upd = generate(d, &details(&p, true), &p, ScriptTemplate::Update);
        assert!(
            upd.starts_with("-- ClickHouse primary keys are not unique"),
            "{upd}"
        );
        let a = crate::analyze(d, &upd);
        assert_eq!(a.statements.len(), 1);
        assert_eq!(a.statements[0].destructive, None);
    }
}
