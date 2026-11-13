//! DDL text built from catalog structures.

use crate::catalog::{Check, Col, Fk, Idx};
use crate::sql::{lit, quote};

/// Whether names carry their schema.
#[derive(Debug, Clone, Copy)]
pub struct Names {
    pub qualify: bool,
}

impl Names {
    pub fn obj(&self, schema: &str, name: &str) -> String {
        if self.qualify {
            format!("{}.{}", quote(schema), quote(name))
        } else {
            quote(name)
        }
    }
}

/// One column definition (`[name] type ...`), as used in `CREATE TABLE` and `ALTER TABLE ADD`.
pub fn column_definition(c: &Col, db_collation: &str) -> String {
    let mut s = quote(&c.name);
    if let Some((expr, persisted)) = &c.computed {
        s.push_str(&format!(" AS {expr}"));
        if *persisted {
            s.push_str(" PERSISTED");
        }
        return s;
    }
    s.push(' ');
    s.push_str(&c.ddl_type);
    if c.sparse {
        s.push_str(" SPARSE");
    }
    if let Some(coll) = &c.collation {
        if !coll.eq_ignore_ascii_case(db_collation) {
            s.push_str(&format!(" COLLATE {coll}"));
        }
    }
    if let Some((seed, inc)) = &c.identity {
        s.push_str(&format!(" IDENTITY({seed},{inc})"));
    }
    if !c.rowversion {
        s.push_str(if c.nullable { " NULL" } else { " NOT NULL" });
    }
    if let Some((name, def)) = &c.default {
        s.push_str(&format!(" CONSTRAINT {} DEFAULT {def}", quote(name)));
    }
    s
}

fn key_list(keys: &[(String, bool)]) -> String {
    keys.iter()
        .map(|(c, desc)| format!("{} {}", quote(c), if *desc { "DESC" } else { "ASC" }))
        .collect::<Vec<_>>()
        .join(", ")
}

fn clustered(i: &Idx) -> &'static str {
    if i.type_id == 1 { "CLUSTERED" } else { "NONCLUSTERED" }
}

/// `CREATE TABLE` with columns, primary key, unique and check constraints.
pub fn create_table(
    names: Names,
    schema: &str,
    table: &str,
    cols: &[Col],
    indexes: &[Idx],
    checks: &[Check],
    db_collation: &str,
) -> String {
    let mut lines: Vec<String> = cols
        .iter()
        .map(|c| format!("    {}", column_definition(c, db_collation)))
        .collect();
    for i in indexes.iter().filter(|i| i.primary || i.unique_constraint) {
        lines.push(format!(
            "    CONSTRAINT {} {} {} ({})",
            quote(&i.name),
            if i.primary { "PRIMARY KEY" } else { "UNIQUE" },
            clustered(i),
            key_list(&i.keys)
        ));
    }
    for k in checks {
        lines.push(format!("    CONSTRAINT {} CHECK {}", quote(&k.name), k.definition));
    }
    format!(
        "CREATE TABLE {} (\n{}\n);",
        names.obj(schema, table),
        lines.join(",\n")
    )
}

/// `CREATE INDEX` of an index that is not a key constraint; `None` for XML/spatial indexes.
pub fn create_index(names: Names, schema: &str, table: &str, i: &Idx) -> Option<String> {
    let on = names.obj(schema, table);
    match i.type_id {
        1 | 2 => {
            let mut s = format!(
                "CREATE {}{} INDEX {} ON {} ({})",
                if i.unique { "UNIQUE " } else { "" },
                clustered(i),
                quote(&i.name),
                on,
                key_list(&i.keys)
            );
            if !i.included.is_empty() {
                let inc: Vec<String> = i.included.iter().map(|c| quote(c)).collect();
                s.push_str(&format!(" INCLUDE ({})", inc.join(", ")));
            }
            if let Some(f) = &i.filter {
                s.push_str(&format!(" WHERE {f}"));
            }
            s.push(';');
            Some(s)
        }
        5 => Some(format!(
            "CREATE CLUSTERED COLUMNSTORE INDEX {} ON {};",
            quote(&i.name),
            on
        )),
        6 => {
            let cols: Vec<String> = i
                .keys
                .iter()
                .map(|(c, _)| c.clone())
                .chain(i.included.iter().cloned())
                .map(|c| quote(&c))
                .collect();
            Some(format!(
                "CREATE NONCLUSTERED COLUMNSTORE INDEX {} ON {} ({});",
                quote(&i.name),
                on,
                cols.join(", ")
            ))
        }
        _ => None,
    }
}

/// `ALTER TABLE ... ADD CONSTRAINT ... FOREIGN KEY`, plus `NOCHECK` for a disabled key.
pub fn add_foreign_key(names: Names, schema: &str, table: &str, f: &Fk) -> String {
    let cols: Vec<String> = f.cols.iter().map(|c| quote(c)).collect();
    let rcols: Vec<String> = f.ref_cols.iter().map(|c| quote(c)).collect();
    let mut s = format!(
        "ALTER TABLE {} WITH {} ADD CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({}) ON DELETE {} ON UPDATE {};",
        names.obj(schema, table),
        if f.not_trusted || f.disabled { "NOCHECK" } else { "CHECK" },
        quote(&f.name),
        cols.join(", "),
        names.obj(&f.ref_schema, &f.ref_table),
        rcols.join(", "),
        f.on_delete,
        f.on_update
    );
    if f.disabled {
        s.push_str(&format!(
            "\nALTER TABLE {} NOCHECK CONSTRAINT {};",
            names.obj(schema, table),
            quote(&f.name)
        ));
    }
    s
}

/// `sp_addextendedproperty` for a table or column comment.
pub fn comment_sql(schema: &str, table: &str, column: Option<&str>, kind: &str, comment: &str) -> String {
    let mut s = format!(
        "EXEC sys.sp_addextendedproperty @name = N'MS_Description', @value = {}, @level0type = N'SCHEMA', @level0name = {}, @level1type = N'{kind}', @level1name = {}",
        lit(comment),
        lit(schema),
        lit(table)
    );
    if let Some(c) = column {
        s.push_str(&format!(", @level2type = N'COLUMN', @level2name = {}", lit(c)));
    }
    s.push(';');
    s
}

/// Turn `CREATE VIEW ...` into `CREATE OR ALTER VIEW ...` (idempotent re-runs).
pub fn create_or_alter(definition: &str) -> String {
    let mut i = 0usize;
    // Skip leading whitespace and comments.
    loop {
        i += definition[i..].len() - definition[i..].trim_start().len();
        if definition[i..].starts_with("--") {
            i += definition[i..].find('\n').map_or(definition.len() - i, |n| n + 1);
        } else if definition[i..].starts_with("/*") {
            i += definition[i..].find("*/").map_or(definition.len() - i, |n| n + 2);
        } else {
            break;
        }
    }
    let rest = &definition[i..];
    let upper = rest.to_ascii_uppercase();
    if upper.starts_with("CREATE OR ALTER") || !upper.starts_with("CREATE") {
        return definition.to_string();
    }
    format!("{}CREATE OR ALTER{}", &definition[..i], &rest["CREATE".len()..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: &str, nullable: bool) -> Col {
        Col {
            id: 1,
            name: name.into(),
            display_type: ty.to_lowercase(),
            ddl_type: ty.into(),
            base_type: ty.to_lowercase(),
            nullable,
            identity: None,
            computed: None,
            default: None,
            collation: None,
            comment: None,
            sparse: false,
            generated: 0,
            rowversion: false,
        }
    }

    #[test]
    fn table_with_constraints() {
        let mut id = col("id", "INT", false);
        id.identity = Some(("1".into(), "1".into()));
        let mut qty = col("qty", "INT", true);
        qty.default = Some(("DF_t_qty".into(), "((0))".into()));
        let mut calc = col("dbl", "INT", true);
        calc.computed = Some(("([qty]*(2))".into(), true));
        let pk = Idx {
            name: "PK_t".into(),
            type_id: 1,
            type_desc: "CLUSTERED".into(),
            unique: true,
            primary: true,
            unique_constraint: false,
            filter: None,
            disabled: false,
            keys: vec![("id".into(), false)],
            included: vec![],
        };
        let ck = Check {
            name: "CK_t".into(),
            definition: "([qty]>=(0))".into(),
            disabled: false,
        };
        let sql = create_table(Names { qualify: true }, "dbo", "t", &[id, qty, calc], &[pk], &[ck], "x");
        assert!(sql.starts_with("CREATE TABLE [dbo].[t] ("));
        assert!(sql.contains("[id] INT IDENTITY(1,1) NOT NULL"));
        assert!(sql.contains("[qty] INT NULL CONSTRAINT [DF_t_qty] DEFAULT ((0))"));
        assert!(sql.contains("[dbl] AS ([qty]*(2)) PERSISTED"));
        assert!(sql.contains("CONSTRAINT [PK_t] PRIMARY KEY CLUSTERED ([id] ASC)"));
        assert!(sql.contains("CONSTRAINT [CK_t] CHECK ([qty]>=(0))"));
        assert!(sql.ends_with("\n);"));
    }

    #[test]
    fn index_variants() {
        let mut i = Idx {
            name: "IX".into(),
            type_id: 2,
            type_desc: "NONCLUSTERED".into(),
            unique: true,
            primary: false,
            unique_constraint: false,
            filter: Some("([a] IS NOT NULL)".into()),
            disabled: false,
            keys: vec![("a".into(), false), ("b".into(), true)],
            included: vec!["c".into()],
        };
        let n = Names { qualify: false };
        assert_eq!(
            create_index(n, "dbo", "t", &i).unwrap(),
            "CREATE UNIQUE NONCLUSTERED INDEX [IX] ON [t] ([a] ASC, [b] DESC) INCLUDE ([c]) WHERE ([a] IS NOT NULL);"
        );
        i.type_id = 5;
        assert_eq!(
            create_index(n, "dbo", "t", &i).unwrap(),
            "CREATE CLUSTERED COLUMNSTORE INDEX [IX] ON [t];"
        );
        i.type_id = 3;
        assert!(create_index(n, "dbo", "t", &i).is_none());
    }

    #[test]
    fn foreign_key_text() {
        let f = Fk {
            name: "FK".into(),
            cols: vec!["a".into()],
            ref_schema: "s".into(),
            ref_table: "r".into(),
            ref_cols: vec!["id".into()],
            on_update: "NO ACTION".into(),
            on_delete: "CASCADE".into(),
            disabled: false,
            not_trusted: false,
        };
        assert_eq!(
            add_foreign_key(Names { qualify: true }, "dbo", "t", &f),
            "ALTER TABLE [dbo].[t] WITH CHECK ADD CONSTRAINT [FK] FOREIGN KEY ([a]) REFERENCES [s].[r] ([id]) ON DELETE CASCADE ON UPDATE NO ACTION;"
        );
    }

    #[test]
    fn create_or_alter_rewrites_first_create() {
        assert_eq!(create_or_alter("CREATE VIEW v AS SELECT 1"), "CREATE OR ALTER VIEW v AS SELECT 1");
        assert_eq!(
            create_or_alter("-- c\n/* x */\n  create procedure p as select 1"),
            "-- c\n/* x */\n  CREATE OR ALTER procedure p as select 1"
        );
        assert_eq!(create_or_alter("CREATE OR ALTER VIEW v AS SELECT 1"), "CREATE OR ALTER VIEW v AS SELECT 1");
        assert_eq!(create_or_alter("ALTER VIEW v AS SELECT 1"), "ALTER VIEW v AS SELECT 1");
    }

    #[test]
    fn comment_statement() {
        let s = comment_sql("dbo", "t", Some("c"), "TABLE", "it's");
        assert!(s.contains("@value = N'it''s'") && s.ends_with("@level2name = N'c';"));
    }
}
