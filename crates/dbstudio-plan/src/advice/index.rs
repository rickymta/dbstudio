//! `CREATE INDEX` generation (design section 5). Rules never write DDL themselves: they hand an
//! [`IndexCandidate`] to [`render`], which picks the dialect syntax, the name and checks the
//! indexes that already exist.
//!
//! Package R1 implements PostgreSQL (and YugabyteDB) only; the other dialects return
//! [`IndexOutcome::Unsupported`] until package R2 fills them in.

use std::collections::BTreeMap;

use dbstudio_core::ScriptDialect;

use super::context::AdvisorContext;
use super::model::{ActionKind, ActionScope, AdviceAction, Caveat, LockRisk, TableRef};

/// Most key columns of a suggested index.
pub(crate) const MAX_KEY_COLUMNS: usize = 4;
/// Most INCLUDE columns of a suggested index.
pub(crate) const MAX_INCLUDE_COLUMNS: usize = 6;

/// One key part of a suggested index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IndexKey {
    /// Plain column name; `None` for an expression key.
    pub column: Option<String>,
    /// Key text in SQL (quoted column, or the expression with its own parentheses).
    pub sql: String,
    /// Fragment used in the index name (`customer_id`, `lower_email`).
    pub name_part: String,
    /// Descending order.
    pub desc: bool,
}

impl IndexKey {
    /// A plain column key.
    pub fn column(ctx: &AdvisorContext<'_>, name: &str, desc: bool) -> Self {
        IndexKey {
            column: Some(name.to_string()),
            sql: quote_ident(ctx.dialect, name),
            name_part: name.to_string(),
            desc,
        }
    }

    /// An expression key such as `(lower(email))`; `normalized` is how the catalog would print
    /// it, for matching existing indexes.
    pub fn expression(sql: String, name_part: String) -> Self {
        IndexKey {
            column: None,
            sql,
            name_part,
            desc: false,
        }
    }

    fn match_key(&self) -> String {
        match &self.column {
            Some(c) => c.to_lowercase(),
            None => normalize_expr(&self.sql),
        }
    }
}

/// An index the engine would like to see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IndexCandidate {
    /// Table (schema and name as the plan prints them).
    pub table: TableRef,
    /// Key parts in order.
    pub keys: Vec<IndexKey>,
    /// Leading keys that are equality conditions (their order is interchangeable).
    pub eq_count: usize,
    /// INCLUDE columns (plain names).
    pub include: Vec<String>,
}

/// What [`render`] decided.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum IndexOutcome {
    /// Create this index.
    Create {
        action: Box<AdviceAction>,
        caveats: Vec<Caveat>,
        /// Name chosen for the index.
        name: String,
    },
    /// An existing index already starts with the suggested keys.
    Existing { index: String },
    /// No DDL for this dialect yet.
    Unsupported,
}

/// Quote `name` when the dialect needs it (PostgreSQL: only unsafe names and keywords).
pub(crate) fn quote_ident(dialect: ScriptDialect, name: &str) -> String {
    match dialect {
        ScriptDialect::Postgres => {
            let safe = name.chars().enumerate().all(|(i, c)| {
                c == '_' || c.is_ascii_lowercase() || (i > 0 && (c.is_ascii_digit() || c == '$'))
            }) && !name.is_empty();
            if safe && !PG_RESERVED.contains(&name) {
                name.to_string()
            } else {
                dbstudio_sql::Dialect::new(dialect).quote_ident(name)
            }
        }
        d => dbstudio_sql::Dialect::new(d).quote_ident(name),
    }
}

/// Reserved words of PostgreSQL that cannot be used as bare identifiers.
const PG_RESERVED: &[&str] = &[
    "all",
    "analyse",
    "analyze",
    "and",
    "any",
    "array",
    "as",
    "asc",
    "asymmetric",
    "both",
    "case",
    "cast",
    "check",
    "collate",
    "column",
    "constraint",
    "create",
    "current_catalog",
    "current_date",
    "current_role",
    "current_time",
    "current_timestamp",
    "current_user",
    "default",
    "deferrable",
    "desc",
    "distinct",
    "do",
    "else",
    "end",
    "except",
    "false",
    "fetch",
    "for",
    "foreign",
    "from",
    "grant",
    "group",
    "having",
    "in",
    "initially",
    "intersect",
    "into",
    "lateral",
    "leading",
    "limit",
    "localtime",
    "localtimestamp",
    "not",
    "null",
    "offset",
    "on",
    "only",
    "or",
    "order",
    "placing",
    "primary",
    "references",
    "returning",
    "select",
    "session_user",
    "some",
    "symmetric",
    "table",
    "then",
    "to",
    "trailing",
    "true",
    "union",
    "unique",
    "user",
    "using",
    "variadic",
    "when",
    "where",
    "window",
    "with",
];

/// Lower-case, drop whitespace and parentheses: a crude canonical form for comparing the
/// expression of a suggestion with the one the catalog printed.
fn normalize_expr(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace() && *c != '(' && *c != ')' && *c != '"')
        .flat_map(char::to_lowercase)
        .collect()
}

fn fnv1a(parts: &[String]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for p in parts {
        for b in p.bytes().chain(std::iter::once(0u8)) {
            h ^= u32::from(b);
            h = h.wrapping_mul(0x0100_0193);
        }
    }
    h
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// `ix_{table}_{col1}_{col2}`, at most `max` characters, unique among `taken`.
fn index_name(table: &str, keys: &[IndexKey], max: usize, taken: &[String]) -> String {
    let mut base = format!("ix_{}", sanitize(table));
    for k in keys {
        base.push('_');
        base.push_str(&sanitize(&k.name_part));
    }
    if base.chars().count() > max {
        let parts: Vec<String> = keys.iter().map(|k| k.name_part.clone()).collect();
        let suffix = format!("_{:06x}", fnv1a(&parts) & 0x00ff_ffff);
        base = base.chars().take(max - suffix.len()).collect::<String>() + &suffix;
    }
    let lower_taken: Vec<String> = taken.iter().map(|t| t.to_lowercase()).collect();
    let mut name = base.clone();
    let mut n = 2;
    while lower_taken.contains(&name.to_lowercase()) {
        let suffix = format!("_{n}");
        name = base.chars().take(max - suffix.len()).collect::<String>() + &suffix;
        n += 1;
    }
    name
}

/// Qualified table name as SQL.
fn table_sql(dialect: ScriptDialect, t: &TableRef) -> String {
    let mut s = String::new();
    if let Some(schema) = &t.schema {
        s.push_str(&quote_ident(dialect, schema));
        s.push('.');
    }
    s.push_str(&quote_ident(dialect, &t.name));
    s
}

/// Decide what to do about `candidate`.
pub(crate) fn render(ctx: &AdvisorContext<'_>, candidate: &IndexCandidate) -> IndexOutcome {
    match ctx.dialect {
        ScriptDialect::Postgres => render_pg(ctx, candidate),
        _ => IndexOutcome::Unsupported,
    }
}

fn render_pg(ctx: &AdvisorContext<'_>, c: &IndexCandidate) -> IndexOutcome {
    let meta = ctx.table_meta(c.table.schema.as_deref(), &c.table.name);
    let keys = &c.keys[..c.keys.len().min(MAX_KEY_COLUMNS)];
    let wanted: Vec<String> = keys.iter().map(IndexKey::match_key).collect();
    let mut caveats = vec![Caveat {
        key: "writeOverhead".into(),
        verify: false,
        params: BTreeMap::new(),
    }];
    let mut taken = Vec::new();
    if let Some(meta) = meta {
        for ix in &meta.indexes {
            taken.push(ix.name.clone());
            if ix.partial || !matches!(ix.method.as_deref(), None | Some("btree")) {
                continue;
            }
            let have: Vec<String> = ix
                .columns
                .iter()
                .map(|c| {
                    if c.starts_with('(') {
                        normalize_expr(c)
                    } else {
                        c.to_lowercase()
                    }
                })
                .collect();
            let n = wanted.len();
            if have.len() >= n && covers(&have[..n], &wanted, c.eq_count) {
                return IndexOutcome::Existing {
                    index: ix.name.clone(),
                };
            }
            if !have.is_empty() && have.len() < n && wanted[..have.len()] == have[..] {
                let mut params = BTreeMap::new();
                params.insert("index".to_string(), ix.name.clone());
                caveats.push(Caveat {
                    key: "extendsIndex".into(),
                    verify: false,
                    params,
                });
            }
        }
        if let Some(rows) = meta.estimated_rows.filter(|r| *r >= 1_000_000) {
            let mut params = BTreeMap::new();
            params.insert("rows".to_string(), rows.to_string());
            caveats.push(Caveat {
                key: "largeTableBuild".into(),
                verify: false,
                params,
            });
        }
    }
    let yugabyte = ctx.is_yugabyte();
    let name = index_name(&c.table.name, keys, 63, &taken);
    let cols: Vec<String> = keys
        .iter()
        .map(|k| {
            if k.desc {
                format!("{} DESC", k.sql)
            } else {
                k.sql.clone()
            }
        })
        .collect();
    let mut sql = format!(
        "CREATE INDEX {}IF NOT EXISTS {} ON {} ({})",
        if yugabyte { "" } else { "CONCURRENTLY " },
        quote_ident(ScriptDialect::Postgres, &name),
        table_sql(ScriptDialect::Postgres, &c.table),
        cols.join(", ")
    );
    let include: Vec<&String> = c.include.iter().take(MAX_INCLUDE_COLUMNS).collect();
    if !include.is_empty() {
        if ctx.major_version().is_some_and(|v| v < 11) {
            caveats.push(Caveat {
                key: "includeDropped".into(),
                verify: false,
                params: BTreeMap::new(),
            });
        } else {
            let inc: Vec<String> = include
                .iter()
                .map(|i| quote_ident(ScriptDialect::Postgres, i))
                .collect();
            sql.push_str(&format!(" INCLUDE ({})", inc.join(", ")));
        }
    }
    sql.push(';');
    if ctx.server.in_transaction && !yugabyte {
        caveats.push(Caveat {
            key: "notInTransaction".into(),
            verify: false,
            params: BTreeMap::new(),
        });
    }
    let mut params = BTreeMap::new();
    params.insert("index".to_string(), name.clone());
    params.insert("table".to_string(), c.table.name.clone());
    IndexOutcome::Create {
        action: Box::new(AdviceAction {
            kind: ActionKind::CreateIndex,
            label_key: "createIndex".into(),
            sql: Some(sql),
            template: false,
            scope: ActionScope::Object,
            lock: Some(if yugabyte {
                LockRisk::None
            } else {
                LockRisk::ShortLock
            }),
            params,
        }),
        caveats,
        name,
    }
}

/// `have` (the first columns of an existing index) serves `wanted`: equal, or equal up to the
/// order of the leading equality columns.
fn covers(have: &[String], wanted: &[String], eq_count: usize) -> bool {
    if have == wanted {
        return true;
    }
    let eq = eq_count.min(wanted.len());
    let mut a = have[..eq].to_vec();
    let mut b = wanted[..eq].to_vec();
    a.sort();
    b.sort();
    a == b && have[eq..] == wanted[eq..]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PlanRules, advice::context::AdvisorContext};
    use dbstudio_core::{IndexDetail, ObjectPath, PlanFormat, RawPlan};

    fn raw(version: &str) -> RawPlan {
        RawPlan {
            format: PlanFormat::PostgresJson,
            text: "[]".into(),
            statement: String::new(),
            analyzed: false,
            rolled_back: false,
            server_version: version.into(),
            captured_at: String::new(),
            extra: BTreeMap::new(),
        }
    }

    fn cand(ctx: &AdvisorContext<'_>, cols: &[&str], eq: usize) -> IndexCandidate {
        IndexCandidate {
            table: TableRef {
                database: None,
                schema: Some("public".into()),
                name: "orders".into(),
                alias: None,
                path: None,
            },
            keys: cols
                .iter()
                .map(|c| IndexKey::column(ctx, c, false))
                .collect(),
            eq_count: eq,
            include: vec![],
        }
    }

    fn sql(o: IndexOutcome) -> String {
        match o {
            IndexOutcome::Create { action, .. } => action.sql.unwrap(),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn pg_basic_and_quoting() {
        let rules = PlanRules::default();
        let r = raw("16.2");
        let ctx = AdvisorContext::basic(&r, &rules);
        assert_eq!(
            sql(render(&ctx, &cand(&ctx, &["customer_id"], 1))),
            "CREATE INDEX CONCURRENTLY IF NOT EXISTS ix_orders_customer_id ON public.orders (customer_id);"
        );
        let mut c = cand(&ctx, &["Order Id", "user"], 2);
        c.table.name = "Weird Table".into();
        assert_eq!(
            sql(render(&ctx, &c)),
            "CREATE INDEX CONCURRENTLY IF NOT EXISTS \"ix_Weird_Table_Order_Id_user\" ON public.\"Weird Table\" (\"Order Id\", \"user\");"
        );
    }

    #[test]
    fn pg_include_desc_yugabyte_and_old_versions() {
        let rules = PlanRules::default();
        let r = raw("PostgreSQL 11.2-YB-2.20.0.0");
        let ctx = AdvisorContext::basic(&r, &rules);
        let mut c = cand(&ctx, &["a"], 1);
        c.keys[0].desc = true;
        c.include = vec!["total".into()];
        assert_eq!(
            sql(render(&ctx, &c)),
            "CREATE INDEX IF NOT EXISTS ix_orders_a ON public.orders (a DESC) INCLUDE (total);"
        );
        let r = raw("10.5");
        let ctx = AdvisorContext::basic(&r, &rules);
        let mut c = cand(&ctx, &["a"], 1);
        c.include = vec!["total".into()];
        assert!(!sql(render(&ctx, &c)).contains("INCLUDE"));
    }

    #[test]
    fn pg_long_names_are_cut_with_a_hash() {
        let rules = PlanRules::default();
        let r = raw("16");
        let ctx = AdvisorContext::basic(&r, &rules);
        let long = "c".repeat(40);
        let c = cand(&ctx, &[long.as_str(), "d".repeat(40).as_str()], 2);
        let IndexOutcome::Create { name, .. } = render(&ctx, &c) else {
            panic!()
        };
        assert_eq!(name.len(), 63);
    }

    #[test]
    fn pg_existing_index_cases() {
        let rules = PlanRules::default();
        let r = raw("16");
        let mut ctx = AdvisorContext::basic(&r, &rules);
        let meta = |cols: &[&str]| crate::advice::context::TableMeta {
            path: ObjectPath::root(),
            estimated_rows: Some(5_000_000),
            columns: vec![],
            indexes: vec![IndexDetail {
                name: "ix_old".into(),
                columns: cols.iter().map(|c| c.to_string()).collect(),
                unique: false,
                primary: false,
                method: Some("btree".into()),
                definition: None,
                partial: false,
            }],
        };
        let key = crate::advice::context::TableKey::new(Some("public"), "orders");
        // The same keys in another order of the equality group: nothing to create.
        ctx.tables.insert(key.clone(), meta(&["b", "a"]));
        let c = cand(&ctx, &["a", "b"], 2);
        assert_eq!(
            render(&ctx, &c),
            IndexOutcome::Existing {
                index: "ix_old".into()
            }
        );
        // Longer than an existing index: create, mention it.
        ctx.tables.insert(key.clone(), meta(&["a"]));
        let IndexOutcome::Create { caveats, name, .. } = render(&ctx, &c) else {
            panic!()
        };
        assert!(caveats.iter().any(|c| c.key == "extendsIndex"));
        assert!(caveats.iter().any(|c| c.key == "largeTableBuild"));
        assert_eq!(name, "ix_orders_a_b");
        // Same name taken: numbered.
        ctx.tables.insert(key, meta(&["z"]));
        let mut c2 = c.clone();
        c2.keys = vec![IndexKey::column(&ctx, "old", false)];
        ctx.tables.values_mut().next().unwrap().indexes[0].name = "ix_orders_old".into();
        let IndexOutcome::Create { name, .. } = render(&ctx, &c2) else {
            panic!()
        };
        assert_eq!(name, "ix_orders_old_2");
    }

    #[test]
    fn other_dialects_are_not_supported_yet() {
        let rules = PlanRules::default();
        let mut r = raw("8");
        r.format = PlanFormat::MySqlJson;
        let ctx = AdvisorContext::basic(&r, &rules);
        assert_eq!(
            render(&ctx, &cand(&ctx, &["a"], 1)),
            IndexOutcome::Unsupported
        );
    }
}
