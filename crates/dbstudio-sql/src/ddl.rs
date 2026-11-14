//! `CREATE TABLE` generation for "import into a new table".

use dbstudio_core::ScriptDialect;
use serde::{Deserialize, Serialize};

use crate::dialect::Dialect;

/// A column of a table about to be created by an import.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewColumn {
    /// Column name (unquoted).
    pub name: String,
    /// Native type text chosen by the user (inserted verbatim).
    pub type_name: String,
    /// `NULL` allowed.
    pub nullable: bool,
    /// Part of the primary key.
    pub primary_key: bool,
}

/// `CREATE TABLE <name> (...)` for `cols`; names are quoted for `d`, type names are verbatim.
pub fn create_table(d: ScriptDialect, name: &str, cols: &[NewColumn]) -> String {
    let q = Dialect::new(d);
    let mut lines: Vec<String> = cols
        .iter()
        .map(|c| {
            format!(
                "  {} {}{}",
                q.quote_ident(&c.name),
                c.type_name,
                if c.nullable && !c.primary_key {
                    ""
                } else {
                    " NOT NULL"
                }
            )
        })
        .collect();
    let pk: Vec<String> = cols
        .iter()
        .filter(|c| c.primary_key)
        .map(|c| q.quote_ident(&c.name))
        .collect();
    if !pk.is_empty() {
        lines.push(format!("  PRIMARY KEY ({})", pk.join(", ")));
    }
    format!(
        "CREATE TABLE {} (\n{}\n)",
        q.quote_ident(name),
        lines.join(",\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_table_with_primary_key() {
        let cols = vec![
            NewColumn {
                name: "id".into(),
                type_name: "bigint".into(),
                nullable: false,
                primary_key: true,
            },
            NewColumn {
                name: "name".into(),
                type_name: "text".into(),
                nullable: true,
                primary_key: false,
            },
        ];
        assert_eq!(
            create_table(ScriptDialect::Postgres, "t", &cols),
            "CREATE TABLE \"t\" (\n  \"id\" bigint NOT NULL,\n  \"name\" text,\n  PRIMARY KEY (\"id\")\n)"
        );
        assert!(create_table(ScriptDialect::MsSql, "t", &cols).starts_with("CREATE TABLE [t]"));
    }

    fn col(name: &str, ty: &str, nullable: bool, pk: bool) -> NewColumn {
        NewColumn {
            name: name.into(),
            type_name: ty.into(),
            nullable,
            primary_key: pk,
        }
    }

    #[test]
    fn quotes_per_dialect() {
        let cols = vec![col("a`b", "int", false, false)];
        let my = ScriptDialect::mysql(false, true);
        assert_eq!(
            create_table(my, "t`x", &cols),
            "CREATE TABLE `t``x` (\n  `a``b` int NOT NULL\n)"
        );
        assert_eq!(
            create_table(
                ScriptDialect::Oracle,
                "T",
                &[col("ID", "number(19)", false, true)]
            ),
            "CREATE TABLE \"T\" (\n  \"ID\" number(19) NOT NULL,\n  PRIMARY KEY (\"ID\")\n)"
        );
        assert_eq!(
            create_table(
                ScriptDialect::MsSql,
                "t]",
                &[col("c", "nvarchar(max)", true, false)]
            ),
            "CREATE TABLE [t]]] (\n  [c] nvarchar(max)\n)"
        );
    }

    #[test]
    fn composite_and_implicit_not_null_primary_keys() {
        let cols = vec![col("a", "int", true, true), col("b", "int", true, true)];
        let sql = create_table(ScriptDialect::Postgres, "t", &cols);
        assert!(sql.contains("\"a\" int NOT NULL"));
        assert!(sql.ends_with("PRIMARY KEY (\"a\", \"b\")\n)"));
        assert!(
            !create_table(
                ScriptDialect::Postgres,
                "t",
                &[col("a", "int", true, false)]
            )
            .contains("PRIMARY KEY")
        );
    }

    #[test]
    fn generated_sql_is_a_single_statement() {
        for d in [
            ScriptDialect::Postgres,
            ScriptDialect::MsSql,
            ScriptDialect::Oracle,
        ] {
            let sql = create_table(d, "we;ird", &[col("a\";b", "int", true, false)]);
            assert_eq!(crate::split(d, &sql).len(), 1, "{d:?}");
            assert_eq!(
                crate::analyze(d, &sql).statements[0].class,
                dbstudio_core::StatementClass::Ddl
            );
        }
    }
}
