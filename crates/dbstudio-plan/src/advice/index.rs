//! `CREATE INDEX` generation (design section 5). Rules never write DDL themselves: they hand an
//! [`IndexCandidate`] to [`render`], which picks the dialect syntax, the name and checks the
//! indexes that already exist.
//!
//! Every product with a B-tree style index gets its own syntax (PostgreSQL, YugabyteDB,
//! CockroachDB, MySQL, MariaDB, TiDB, SQL Server, Oracle), MongoDB gets a `createIndex` call and
//! ClickHouse a skip index; anything else is [`IndexOutcome::Unsupported`].

use std::collections::BTreeMap;

use dbstudio_core::ScriptDialect;

use super::context::{AdvisorContext, Flavor, TableMeta};
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
        /// Actions that must follow the `createIndex` one (ClickHouse `MATERIALIZE INDEX`).
        follow_up: Vec<AdviceAction>,
    },
    /// An existing index already starts with the suggested keys.
    Existing { index: String },
    /// No DDL for this dialect.
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

/// `base` cut to `max` characters (with a hash of `parts` as suffix) and made unique among
/// `taken` by a numeric suffix.
fn shorten_name(base: String, parts: &[String], max: usize, taken: &[String]) -> String {
    let mut base = base;
    if base.chars().count() > max {
        let suffix = format!("_{:06x}", fnv1a(parts) & 0x00ff_ffff);
        base = base
            .chars()
            .take(max.saturating_sub(suffix.len()))
            .collect::<String>()
            + &suffix;
    }
    let lower_taken: Vec<String> = taken.iter().map(|t| t.to_lowercase()).collect();
    let mut name = base.clone();
    let mut n = 2;
    while lower_taken.contains(&name.to_lowercase()) {
        let suffix = format!("_{n}");
        name = base
            .chars()
            .take(max.saturating_sub(suffix.len()))
            .collect::<String>()
            + &suffix;
        n += 1;
    }
    name
}

/// Longest index name the product accepts.
fn name_limit(ctx: &AdvisorContext<'_>) -> usize {
    match ctx.flavor() {
        Flavor::Postgres | Flavor::Yugabyte | Flavor::Cockroach | Flavor::Other => 63,
        Flavor::MySql | Flavor::MariaDb | Flavor::TiDb => 64,
        Flavor::MsSql | Flavor::ClickHouse => 128,
        Flavor::Oracle => {
            if ctx.version_before(12, 2) {
                30
            } else {
                128
            }
        }
        Flavor::Mongo => 127,
    }
}

/// Name before cutting: `ix_{table}_{cols}`, `IX_{Table}_{Col}` keeps the case of the source,
/// MongoDB follows the server (`f1_1_f2_-1`), ClickHouse names the skipped column.
fn base_name(flavor: Flavor, table: &str, keys: &[IndexKey]) -> String {
    match flavor {
        Flavor::Mongo => keys
            .iter()
            .map(|k| format!("{}_{}", k.name_part, if k.desc { -1 } else { 1 }))
            .collect::<Vec<_>>()
            .join("_"),
        Flavor::ClickHouse => format!(
            "ix_{}",
            sanitize(keys.first().map_or("", |k| k.name_part.as_str()))
        ),
        _ => {
            let prefix = if matches!(flavor, Flavor::MsSql | Flavor::Oracle) {
                "IX"
            } else {
                "ix"
            };
            let mut base = format!("{prefix}_{}", sanitize(table));
            for k in keys {
                base.push('_');
                base.push_str(&sanitize(&k.name_part));
            }
            base
        }
    }
}

/// Oracle identifiers that need no quotes: upper case, letters, digits, `_`, `$`, `#`.
fn oracle_safe(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_uppercase())
        && chars
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || matches!(c, '_' | '$' | '#'))
}

/// Identifier as SQL for the connection's product.
fn q(ctx: &AdvisorContext<'_>, name: &str) -> String {
    if ctx.flavor() == Flavor::Oracle && oracle_safe(name) {
        name.to_string()
    } else {
        quote_ident(ctx.dialect, name)
    }
}

/// Qualified table name as SQL (schema or database first, as the product spells it).
fn table_sql(ctx: &AdvisorContext<'_>, t: &TableRef) -> String {
    let qualifier = match ctx.flavor() {
        Flavor::MySql | Flavor::MariaDb | Flavor::TiDb | Flavor::ClickHouse => {
            t.schema.as_ref().or(t.database.as_ref())
        }
        _ => t.schema.as_ref(),
    };
    let mut s = String::new();
    if let Some(schema) = qualifier {
        s.push_str(&q(ctx, schema));
        s.push('.');
    }
    s.push_str(&q(ctx, &t.name));
    s
}

/// An index that already exists on the table, reduced to what matching needs.
struct Known {
    name: String,
    /// Key columns, lower-cased, without direction.
    keys: Vec<String>,
    /// INCLUDE / STORING columns read from the definition (original case).
    include: Vec<String>,
    unique: bool,
    primary: bool,
    method: Option<String>,
}

/// `col DESC` to `col`.
fn strip_direction(s: &str) -> &str {
    let t = s.trim_end();
    let lower = t.to_ascii_lowercase();
    for suffix in [" desc", " asc"] {
        if lower.ends_with(suffix) {
            return t[..t.len() - suffix.len()].trim_end();
        }
    }
    t
}

fn unquote(s: &str) -> String {
    s.trim()
        .trim_matches(|c| matches!(c, '"' | '`' | '[' | ']'))
        .to_string()
}

/// Columns named by the `INCLUDE (...)` / `STORING (...)` clause of an index definition.
fn included_columns(definition: Option<&str>) -> Vec<String> {
    let Some(def) = definition else {
        return Vec::new();
    };
    let lower = def.to_ascii_lowercase();
    for kw in ["include", "storing", "covering"] {
        let mut from = 0;
        while let Some(pos) = lower[from..].find(kw) {
            let at = from + pos;
            from = at + kw.len();
            let before_ok = at == 0 || !lower.as_bytes()[at - 1].is_ascii_alphanumeric();
            let rest = lower[from..].trim_start();
            if before_ok && rest.starts_with('(') {
                let open = def.len() - rest.len();
                if let Some(close) = def[open..].find(')') {
                    return def[open + 1..open + close]
                        .split(',')
                        .map(unquote)
                        .collect();
                }
            }
        }
    }
    Vec::new()
}

/// Access methods whose key order serves the lookups the advisor suggests.
fn ordered_method(flavor: Flavor, method: Option<&str>) -> bool {
    if flavor == Flavor::ClickHouse {
        return true;
    }
    match method.map(str::to_ascii_lowercase).as_deref() {
        None | Some("" | "btree" | "normal" | "clustered" | "nonclustered") => true,
        Some(_) => false,
    }
}

fn known_indexes(flavor: Flavor, meta: &TableMeta) -> Vec<Known> {
    meta.indexes
        .iter()
        .filter(|ix| !ix.partial && ordered_method(flavor, ix.method.as_deref()))
        .map(|ix| Known {
            name: ix.name.clone(),
            keys: ix
                .columns
                .iter()
                .map(|c| {
                    if c.starts_with('(') {
                        normalize_expr(c)
                    } else {
                        strip_direction(c).to_lowercase()
                    }
                })
                .collect(),
            include: included_columns(ix.definition.as_deref()),
            unique: ix.unique,
            primary: ix.primary,
            method: ix.method.clone(),
        })
        .collect()
}

/// What the table already has, compared with the suggested key (design section 5.4).
enum Prior {
    /// Case 4: nothing similar.
    None,
    /// Case 1: an index starts with the suggested keys.
    Existing(String),
    /// Case 2: the suggestion starts with all the keys of this index.
    Extends(usize),
    /// Case 3: same keys, the suggestion adds INCLUDE columns.
    SameKeys(usize),
}

fn compare_with_existing(
    wanted: &[String],
    eq_count: usize,
    want_include: &[String],
    has_include: bool,
    known: &[Known],
) -> Prior {
    let n = wanted.len();
    let mut same = None;
    let mut extends: Option<usize> = None;
    for (i, ix) in known.iter().enumerate() {
        let have = &ix.keys;
        if have.is_empty() {
            continue;
        }
        if have.len() >= n && covers(&have[..n], wanted, eq_count) {
            let covered = want_include
                .iter()
                .all(|w| ix.include.iter().any(|x| x.eq_ignore_ascii_case(w)));
            if have.len() == n && has_include && !covered {
                same.get_or_insert(i);
                continue;
            }
            return Prior::Existing(ix.name.clone());
        }
        if have.len() < n
            && covers(have, &wanted[..have.len()], eq_count.min(have.len()))
            && extends.is_none_or(|e| known[e].keys.len() < have.len())
        {
            extends = Some(i);
        }
    }
    match (same, extends) {
        (Some(i), _) => Prior::SameKeys(i),
        (None, Some(i)) => Prior::Extends(i),
        (None, None) => Prior::None,
    }
}

fn caveat(key: &str, verify: bool) -> Caveat {
    Caveat {
        key: key.into(),
        verify,
        params: BTreeMap::new(),
    }
}

fn caveat_with(key: &str, verify: bool, param: &str, value: &str) -> Caveat {
    let mut c = caveat(key, verify);
    c.params.insert(param.to_string(), value.to_string());
    c
}

/// SQL Server lets the index be built online on Enterprise, Developer, Evaluation and Azure.
fn mssql_online(ctx: &AdvisorContext<'_>) -> Option<bool> {
    if let Some(e) = ctx.server.engine_edition {
        return Some(matches!(e, 3 | 5 | 8));
    }
    let ed = ctx.server.edition.as_deref()?.to_lowercase();
    Some(
        ["enterprise", "developer", "evaluation", "azure"]
            .iter()
            .any(|w| ed.contains(w)),
    )
}

/// Oracle builds an index online only on Enterprise Edition.
fn oracle_online(ctx: &AdvisorContext<'_>) -> Option<bool> {
    let ed = ctx.server.edition.as_deref()?.to_lowercase();
    Some(ed.contains("enterprise"))
}

fn is_text_type(type_name: &str) -> bool {
    let t = type_name.to_ascii_lowercase();
    t.contains("text") || t.contains("blob")
}

/// A name usable bare in a mongo shell expression (`a.b` and `a-b` are not).
fn js_ident(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

fn js_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Collection methods and JavaScript words a collection name must not collide with.
const JS_RESERVED: &[&str] = &[
    "aggregate",
    "class",
    "constructor",
    "count",
    "createIndex",
    "default",
    "delete",
    "drop",
    "dropIndex",
    "find",
    "findOne",
    "function",
    "getIndexes",
    "getName",
    "insert",
    "new",
    "remove",
    "save",
    "stats",
    "toString",
    "update",
    "valueOf",
    "validate",
    "var",
];

fn mongo_ddl(t: &TableRef, keys: &[IndexKey], name: &str) -> String {
    let db = t.database.as_ref().or(t.schema.as_ref());
    let root = match db {
        Some(d) => format!("db.getSiblingDB({})", js_str(d)),
        None => "db".to_string(),
    };
    let coll = if js_ident(&t.name) && !JS_RESERVED.contains(&t.name.as_str()) {
        format!(".{}", t.name)
    } else {
        format!(".getCollection({})", js_str(&t.name))
    };
    let fields: Vec<String> = keys
        .iter()
        .map(|k| {
            let f = k.name_part.as_str();
            let field = if js_ident(f) {
                f.to_string()
            } else {
                js_str(f)
            };
            format!("{field}: {}", if k.desc { -1 } else { 1 })
        })
        .collect();
    format!(
        "{root}{coll}.createIndex({{ {} }}, {{ name: {} }})",
        fields.join(", "),
        js_str(name)
    )
}

/// Decide what to do about `candidate`.
pub(crate) fn render(ctx: &AdvisorContext<'_>, c: &IndexCandidate) -> IndexOutcome {
    let flavor = ctx.flavor();
    if flavor == Flavor::Other || c.keys.is_empty() {
        return IndexOutcome::Unsupported;
    }
    let meta = ctx.table_meta(c.table.schema.as_deref(), &c.table.name);
    // ClickHouse skips granules by one column: the first key.
    let key_cap = if flavor == Flavor::ClickHouse {
        1
    } else {
        MAX_KEY_COLUMNS
    };
    let mut keys: Vec<IndexKey> = c.keys[..c.keys.len().min(key_cap)].to_vec();
    let wanted: Vec<String> = keys.iter().map(IndexKey::match_key).collect();
    let has_include = matches!(
        flavor,
        Flavor::Postgres | Flavor::Yugabyte | Flavor::Cockroach | Flavor::MsSql
    );
    let mut include: Vec<String> = c.include.clone();
    let want_include: Vec<String> = include.iter().map(|i| i.to_lowercase()).collect();
    let mut caveats = vec![caveat("writeOverhead", false)];

    // 1. Compare with what the table has.
    let known = meta.map(|m| known_indexes(flavor, m)).unwrap_or_default();
    let taken: Vec<String> = meta
        .map(|m| m.indexes.iter().map(|i| i.name.clone()).collect())
        .unwrap_or_default();
    let eq_count = c.eq_count.min(keys.len());
    let mut replace: Option<usize> = None;
    match compare_with_existing(&wanted, eq_count, &want_include, has_include, &known) {
        Prior::Existing(index) => return IndexOutcome::Existing { index },
        Prior::Extends(i) | Prior::SameKeys(i) => {
            caveats.push(caveat_with("extendsIndex", false, "index", &known[i].name));
            let old = &known[i];
            let plain = !old.unique
                && !old.primary
                && old
                    .method
                    .as_deref()
                    .is_none_or(|m| m.eq_ignore_ascii_case("nonclustered"));
            if flavor == Flavor::MsSql && plain {
                replace = Some(i);
            }
        }
        Prior::None => {}
    }
    if let Some(rows) = meta
        .and_then(|m| m.estimated_rows)
        .filter(|r| *r >= 1_000_000)
    {
        caveats.push(caveat_with(
            "largeTableBuild",
            false,
            "rows",
            &rows.to_string(),
        ));
    }

    // 2. Columns the product cannot INCLUDE.
    match flavor {
        Flavor::MySql | Flavor::MariaDb | Flavor::TiDb if !include.is_empty() => {
            let type_of = |col: &str| {
                meta.and_then(|m| {
                    m.columns
                        .iter()
                        .find(|cd| cd.name.eq_ignore_ascii_case(col))
                        .map(|cd| cd.type_name.clone())
                })
            };
            let foldable = keys.len() + include.len() <= 5
                && include
                    .iter()
                    .all(|i| type_of(i).is_some_and(|t| !is_text_type(&t)));
            if foldable {
                for i in std::mem::take(&mut include) {
                    keys.push(IndexKey::column(ctx, &i, false));
                }
            } else {
                include.clear();
                caveats.push(caveat("includeDropped", false));
            }
        }
        Flavor::Oracle | Flavor::Mongo | Flavor::ClickHouse if !include.is_empty() => {
            include.clear();
            caveats.push(caveat("includeDropped", false));
        }
        Flavor::MsSql if include.len() > 8 => {
            include.clear();
            caveats.push(caveat("includeTrimmed", false));
        }
        Flavor::Postgres if ctx.major_version().is_some_and(|v| v < 11) && !include.is_empty() => {
            include.clear();
            caveats.push(caveat("includeDropped", false));
        }
        _ => {}
    }
    if let Some(i) = replace {
        // DROP_EXISTING rebuilds the old index: keep what it already includes.
        for old in &known[i].include {
            if !include.iter().any(|x| x.eq_ignore_ascii_case(old)) {
                include.push(old.clone());
            }
        }
    } else {
        include.truncate(MAX_INCLUDE_COLUMNS);
    }

    // 3. Name.
    let name = match replace {
        Some(i) => known[i].name.clone(),
        None => {
            let parts: Vec<String> = keys.iter().map(|k| k.name_part.clone()).collect();
            let base = base_name(flavor, &c.table.name, &keys);
            shorten_name(base, &parts, name_limit(ctx), &taken)
        }
    };

    // 4. Key text.
    let key_sql = |k: &IndexKey, caveats: &mut Vec<Caveat>| -> String {
        let mut text = match &k.column {
            Some(col) => q(ctx, col),
            None => k.sql.clone(),
        };
        if matches!(flavor, Flavor::MySql | Flavor::MariaDb | Flavor::TiDb) {
            if let Some(col) = &k.column {
                let long = meta.is_some_and(|m| {
                    m.columns
                        .iter()
                        .any(|cd| cd.name.eq_ignore_ascii_case(col) && is_text_type(&cd.type_name))
                });
                if long {
                    text.push_str("(191)");
                    caveats.push(caveat_with("prefixIndex", false, "column", col));
                }
            }
        }
        if k.desc {
            text.push_str(" DESC");
        }
        text
    };
    let yugabyte_asc = flavor == Flavor::Yugabyte && c.eq_count == 0;
    let cols: Vec<String> = keys
        .iter()
        .enumerate()
        .map(|(i, k)| {
            let mut t = key_sql(k, &mut caveats);
            if yugabyte_asc && i == 0 && !k.desc {
                t.push_str(" ASC");
            }
            t
        })
        .collect();
    let inc: Vec<String> = include.iter().map(|i| q(ctx, i)).collect();
    let include_sql = |kw: &str| {
        if inc.is_empty() {
            String::new()
        } else {
            format!(" {kw} ({})", inc.join(", "))
        }
    };
    let table = table_sql(ctx, &c.table);
    let mut follow_up = Vec::new();
    let (sql, lock) = match flavor {
        Flavor::Postgres | Flavor::Yugabyte => {
            if ctx.server.in_transaction && flavor == Flavor::Postgres {
                caveats.push(caveat("notInTransaction", false));
            }
            (
                format!(
                    "CREATE INDEX {}IF NOT EXISTS {} ON {table} ({}){};",
                    if flavor == Flavor::Postgres {
                        "CONCURRENTLY "
                    } else {
                        ""
                    },
                    q(ctx, &name),
                    cols.join(", "),
                    include_sql("INCLUDE")
                ),
                if flavor == Flavor::Postgres {
                    LockRisk::ShortLock
                } else {
                    LockRisk::None
                },
            )
        }
        Flavor::Cockroach => (
            format!(
                "CREATE INDEX IF NOT EXISTS {} ON {table} ({}){};",
                q(ctx, &name),
                cols.join(", "),
                include_sql("STORING")
            ),
            LockRisk::None,
        ),
        Flavor::MySql | Flavor::MariaDb => {
            caveats.push(caveat("onlineDdlMayFail", false));
            (
                format!(
                    "ALTER TABLE {table} ADD INDEX {} ({}), ALGORITHM=INPLACE, LOCK=NONE;",
                    q(ctx, &name),
                    cols.join(", ")
                ),
                LockRisk::ShortLock,
            )
        }
        Flavor::TiDb => (
            format!(
                "CREATE INDEX {} ON {table} ({});",
                q(ctx, &name),
                cols.join(", ")
            ),
            LockRisk::None,
        ),
        Flavor::MsSql => {
            let online = mssql_online(ctx);
            let mut with: Vec<&str> = Vec::new();
            if replace.is_some() {
                with.push("DROP_EXISTING = ON");
            }
            match online {
                Some(false) => caveats.push(caveat("offlineIndexBuild", false)),
                Some(true) => with.push("ONLINE = ON"),
                None => {
                    with.push("ONLINE = ON");
                    caveats.push(caveat("onlineDdlMayFail", true));
                }
            }
            let with_sql = if with.is_empty() {
                String::new()
            } else {
                format!(" WITH ({})", with.join(", "))
            };
            (
                format!(
                    "CREATE NONCLUSTERED INDEX {} ON {table} ({}){}{with_sql};",
                    q(ctx, &name),
                    cols.join(", "),
                    include_sql("INCLUDE")
                ),
                if online == Some(false) {
                    LockRisk::BlocksWrites
                } else {
                    LockRisk::ShortLock
                },
            )
        }
        Flavor::Oracle => {
            let online = oracle_online(ctx);
            match online {
                Some(true) => {}
                Some(false) => caveats.push(caveat("offlineIndexBuild", false)),
                None => caveats.push(caveat("offlineIndexBuild", true)),
            }
            (
                format!(
                    "CREATE INDEX {} ON {table} ({}){};",
                    q(ctx, &name),
                    cols.join(", "),
                    if online == Some(true) { " ONLINE" } else { "" }
                ),
                if online == Some(true) {
                    LockRisk::ShortLock
                } else {
                    LockRisk::BlocksWrites
                },
            )
        }
        Flavor::Mongo => (mongo_ddl(&c.table, &keys, &name), LockRisk::ShortLock),
        Flavor::ClickHouse => {
            let first = &keys[0];
            let kind = if c.eq_count == 0 {
                "minmax"
            } else {
                "bloom_filter"
            };
            let col = match &first.column {
                Some(col) => q(ctx, col),
                None => first.sql.clone(),
            };
            caveats.push(caveat("skipIndexOnly", false));
            let mut params = BTreeMap::new();
            params.insert("index".to_string(), name.clone());
            params.insert("table".to_string(), c.table.name.clone());
            follow_up.push(AdviceAction {
                kind: ActionKind::Maintenance,
                label_key: "materializeIndex".into(),
                sql: Some(format!(
                    "ALTER TABLE {table} MATERIALIZE INDEX {};",
                    q(ctx, &name)
                )),
                template: false,
                scope: ActionScope::Object,
                lock: Some(LockRisk::HeavyMutation),
                params,
            });
            (
                format!(
                    "ALTER TABLE {table} ADD INDEX IF NOT EXISTS {} {col} TYPE {kind} GRANULARITY 4;",
                    q(ctx, &name)
                ),
                LockRisk::HeavyMutation,
            )
        }
        Flavor::Other => return IndexOutcome::Unsupported,
    };
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
            lock: Some(lock),
            params,
        }),
        caveats,
        name,
        follow_up,
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
    use crate::{PlanRules, advice::context::TableKey};
    use dbstudio_core::{ColumnDetail, IndexDetail, ObjectPath, PlanFormat, RawPlan, ValueKind};

    /// One product to render for: plan format and server version.
    const FLAVORS: &[(&str, PlanFormat, &str)] = &[
        ("postgres", PlanFormat::PostgresJson, "16.2"),
        (
            "yugabyte",
            PlanFormat::PostgresJson,
            "PostgreSQL 11.2-YB-2.20.0.0",
        ),
        (
            "cockroach",
            PlanFormat::CockroachText,
            "CockroachDB CCL v23.1.0",
        ),
        ("mysql", PlanFormat::MySqlJson, "8.0.36"),
        ("mariadb", PlanFormat::MariaDbJson, "10.11.6-MariaDB"),
        ("tidb", PlanFormat::TiDbRows, "8.0.11-TiDB-v7.5.0"),
        ("mssql", PlanFormat::MsSqlShowplanXml, "16.0.1000.6"),
        ("oracle", PlanFormat::OraclePlanRows, "19.0.0.0.0"),
        ("mongo", PlanFormat::MongoExplain, "7.0.5"),
        ("clickhouse", PlanFormat::ClickHouseJson, "24.8.1"),
    ];

    struct Rig {
        raw: RawPlan,
        rules: PlanRules,
    }

    impl Rig {
        fn new(format: PlanFormat, version: &str) -> Self {
            Rig {
                raw: RawPlan {
                    format,
                    text: "[]".into(),
                    statement: String::new(),
                    analyzed: false,
                    rolled_back: false,
                    server_version: version.into(),
                    captured_at: String::new(),
                    extra: BTreeMap::new(),
                },
                rules: PlanRules::default(),
            }
        }

        fn ctx(&self) -> AdvisorContext<'_> {
            AdvisorContext::basic(&self.raw, &self.rules)
        }
    }

    fn table_for(flavor: Flavor, name: &str) -> TableRef {
        let schema = match flavor {
            Flavor::Postgres | Flavor::Yugabyte => Some("public"),
            Flavor::Cockroach | Flavor::Other => None,
            Flavor::MySql | Flavor::MariaDb | Flavor::TiDb => Some("shop"),
            Flavor::MsSql => Some("dbo"),
            Flavor::Oracle => Some("SHOP"),
            Flavor::Mongo | Flavor::ClickHouse => Some("shop"),
        };
        TableRef {
            database: None,
            schema: schema.map(str::to_string),
            name: name.into(),
            alias: None,
            path: None,
        }
    }

    fn cand(ctx: &AdvisorContext<'_>, table: &str, cols: &[&str], eq: usize) -> IndexCandidate {
        IndexCandidate {
            table: table_for(ctx.flavor(), table),
            keys: cols
                .iter()
                .map(|c| IndexKey::column(ctx, c, false))
                .collect(),
            eq_count: eq,
            include: vec![],
        }
    }

    fn created(o: IndexOutcome) -> (String, String, Vec<Caveat>) {
        match o {
            IndexOutcome::Create {
                action,
                caveats,
                name,
                ..
            } => (action.sql.unwrap(), name, caveats),
            other => panic!("{other:?}"),
        }
    }

    fn sql(o: IndexOutcome) -> String {
        created(o).0
    }

    fn has(caveats: &[Caveat], key: &str) -> bool {
        caveats.iter().any(|c| c.key == key)
    }

    fn meta(rows: Option<u64>, indexes: Vec<IndexDetail>) -> TableMeta {
        TableMeta {
            path: ObjectPath::root(),
            estimated_rows: rows,
            columns: vec![],
            indexes,
        }
    }

    fn index(name: &str, cols: &[&str]) -> IndexDetail {
        IndexDetail {
            name: name.into(),
            columns: cols.iter().map(|c| c.to_string()).collect(),
            unique: false,
            primary: false,
            method: Some("btree".into()),
            definition: None,
            partial: false,
        }
    }

    fn column(name: &str, type_name: &str) -> ColumnDetail {
        ColumnDetail {
            name: name.into(),
            ordinal: 1,
            type_name: type_name.into(),
            value_kind: ValueKind::Text,
            nullable: true,
            default: None,
            comment: None,
            primary_key: false,
            extra: None,
        }
    }

    /// Like [`cand`], but Oracle names are upper case as its catalog stores them.
    fn cand_native(
        ctx: &AdvisorContext<'_>,
        table: &str,
        cols: &[&str],
        eq: usize,
    ) -> IndexCandidate {
        if ctx.flavor() == Flavor::Oracle {
            let up: Vec<String> = cols.iter().map(|c| c.to_uppercase()).collect();
            let refs: Vec<&str> = up.iter().map(String::as_str).collect();
            cand(ctx, &table.to_uppercase(), &refs, eq)
        } else {
            cand(ctx, table, cols, eq)
        }
    }

    fn snapshot() -> String {
        let mut out = String::new();
        for (label, format, version) in FLAVORS {
            let rig = Rig::new(*format, version);
            let mut ctx = rig.ctx();
            let flavor = ctx.flavor();
            let mut line = |case: &str, ctx: &AdvisorContext<'_>, c: &IndexCandidate| {
                let (sql, name, _) = created(render(ctx, c));
                assert!(
                    name.chars().count() <= name_limit(ctx),
                    "{label} {case}: {name}"
                );
                out.push_str(&format!("{label}/{case}: {sql}\n"));
            };
            // Basic: an equality column and a range column.
            let basic = cand_native(&ctx, "orders", &["customer_id", "created_at"], 1);
            line("basic", &ctx, &basic);
            // With INCLUDE columns.
            let mut inc = cand_native(&ctx, "orders", &["customer_id"], 1);
            inc.include = vec!["total".into(), "status".into()];
            // MySQL family folds INCLUDE into the key only when the column types are known.
            if matches!(flavor, Flavor::MySql | Flavor::MariaDb | Flavor::TiDb) {
                let key = TableKey::new(Some("shop"), "orders");
                let mut m = meta(None, vec![]);
                m.columns = vec![
                    column("total", "decimal(10,2)"),
                    column("status", "varchar(20)"),
                ];
                ctx.tables.insert(key, m);
            }
            line("include", &ctx, &inc);
            ctx.tables.clear();
            // A long name.
            let long = cand_native(
                &ctx,
                "order_line_items_archive",
                &[&"c".repeat(40), &"d".repeat(40)],
                2,
            );
            line("longName", &ctx, &long);
            // Names that need quoting.
            let mut quoted = cand(&ctx, "Weird Table", &["Order Id", "user"], 2);
            if flavor == Flavor::Oracle {
                quoted.table.name = "Weird \"Table\"".into();
            }
            line("quote", &ctx, &quoted);
            // The generated name is already taken.
            let plain = cand_native(&ctx, "orders", &["customer_id"], 1);
            let (_, taken_name, _) = created(render(&ctx, &plain));
            let key = TableKey::new(plain.table.schema.as_deref(), "orders");
            ctx.tables
                .insert(key, meta(None, vec![index(&taken_name, &["unrelated"])]));
            line("duplicateName", &ctx, &plain);
        }
        out
    }

    #[test]
    fn ddl_snapshot_for_every_dialect() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/advice_fixtures/index_ddl.snap.txt");
        let got = snapshot();
        if std::env::var_os("UPDATE_FIXTURES").is_some() {
            std::fs::write(&path, &got).unwrap();
        }
        let want = std::fs::read_to_string(&path).unwrap_or_default();
        assert_eq!(got, want, "run with UPDATE_FIXTURES=1 to refresh {path:?}");
    }

    #[test]
    fn pg_basic_and_quoting() {
        let rig = Rig::new(PlanFormat::PostgresJson, "16.2");
        let ctx = rig.ctx();
        assert_eq!(
            sql(render(&ctx, &cand(&ctx, "orders", &["customer_id"], 1))),
            "CREATE INDEX CONCURRENTLY IF NOT EXISTS ix_orders_customer_id ON public.orders (customer_id);"
        );
        let c = cand(&ctx, "Weird Table", &["Order Id", "user"], 2);
        assert_eq!(
            sql(render(&ctx, &c)),
            "CREATE INDEX CONCURRENTLY IF NOT EXISTS \"ix_Weird_Table_Order_Id_user\" ON public.\"Weird Table\" (\"Order Id\", \"user\");"
        );
    }

    #[test]
    fn pg_include_desc_yugabyte_and_old_versions() {
        let rig = Rig::new(PlanFormat::PostgresJson, "PostgreSQL 11.2-YB-2.20.0.0");
        let ctx = rig.ctx();
        let mut c = cand(&ctx, "orders", &["a"], 1);
        c.keys[0].desc = true;
        c.include = vec!["total".into()];
        assert_eq!(
            sql(render(&ctx, &c)),
            "CREATE INDEX IF NOT EXISTS ix_orders_a ON public.orders (a DESC) INCLUDE (total);"
        );
        // The first key is a range column: YugabyteDB hashes it unless it says ASC.
        let range_first = cand(&ctx, "orders", &["created_at"], 0);
        assert_eq!(
            sql(render(&ctx, &range_first)),
            "CREATE INDEX IF NOT EXISTS ix_orders_created_at ON public.orders (created_at ASC);"
        );
        let rig = Rig::new(PlanFormat::PostgresJson, "10.5");
        let ctx = rig.ctx();
        let mut c = cand(&ctx, "orders", &["a"], 1);
        c.include = vec!["total".into()];
        let (text, _, caveats) = created(render(&ctx, &c));
        assert!(!text.contains("INCLUDE"));
        assert!(has(&caveats, "includeDropped"));
    }

    #[test]
    fn transaction_caveat_only_for_concurrent_builds() {
        let rig = Rig::new(PlanFormat::PostgresJson, "16");
        let mut ctx = rig.ctx();
        ctx.server.in_transaction = true;
        let (_, _, caveats) = created(render(&ctx, &cand(&ctx, "orders", &["a"], 1)));
        assert!(has(&caveats, "notInTransaction"));
    }

    #[test]
    fn long_names_are_cut_with_a_hash_per_dialect_limit() {
        for (label, format, version) in FLAVORS {
            let rig = Rig::new(*format, version);
            let ctx = rig.ctx();
            let c = cand(
                &ctx,
                "order_line_items_archive",
                &["c".repeat(70).as_str(), "d".repeat(70).as_str()],
                2,
            );
            let (_, name, _) = created(render(&ctx, &c));
            if ctx.flavor() == Flavor::ClickHouse {
                // The name only holds the skipped column.
                assert!(name.starts_with("ix_c") && name.len() <= 128, "{label}");
                continue;
            }
            assert_eq!(name.chars().count(), name_limit(&ctx), "{label}: {name}");
        }
        // Oracle before 12.2 allows 30 characters.
        let rig = Rig::new(PlanFormat::OraclePlanRows, "12.1.0.2.0");
        let ctx = rig.ctx();
        let c = cand(
            &ctx,
            "ORDER_LINE_ITEMS",
            &["CUSTOMER_IDENTIFIER", "CREATED_AT"],
            2,
        );
        let (_, name, _) = created(render(&ctx, &c));
        assert_eq!(name.len(), 30);
    }

    #[test]
    fn existing_index_cases_postgres() {
        let rig = Rig::new(PlanFormat::PostgresJson, "16");
        let mut ctx = rig.ctx();
        let key = TableKey::new(Some("public"), "orders");
        let c = cand(&ctx, "orders", &["a", "b"], 2);
        // 1. The same keys, equality columns in another order: nothing to create.
        ctx.tables.insert(
            key.clone(),
            meta(Some(5_000_000), vec![index("ix_old", &["b", "a"])]),
        );
        assert_eq!(
            render(&ctx, &c),
            IndexOutcome::Existing {
                index: "ix_old".into()
            }
        );
        // 1b. A longer index starting with the keys also serves.
        ctx.tables.insert(
            key.clone(),
            meta(None, vec![index("ix_long", &["a", "b", "z"])]),
        );
        assert!(matches!(render(&ctx, &c), IndexOutcome::Existing { .. }));
        // 2. Shorter than the suggestion: create, mention it; no DROP.
        ctx.tables.insert(
            key.clone(),
            meta(Some(5_000_000), vec![index("ix_old", &["a"])]),
        );
        let (text, name, caveats) = created(render(&ctx, &c));
        assert_eq!(name, "ix_orders_a_b");
        assert!(text.starts_with("CREATE INDEX CONCURRENTLY"));
        assert!(!text.contains("DROP"));
        assert!(has(&caveats, "extendsIndex") && has(&caveats, "largeTableBuild"));
        // 3. Same keys, new INCLUDE column (read from the definition): create, mention it.
        let mut with_inc = cand(&ctx, "orders", &["a", "b"], 2);
        with_inc.include = vec!["total".into()];
        let mut old = index("ix_ab", &["a", "b"]);
        old.definition =
            Some("CREATE INDEX ix_ab ON public.orders USING btree (a, b) INCLUDE (status)".into());
        ctx.tables
            .insert(key.clone(), meta(None, vec![old.clone()]));
        let (text, _, caveats) = created(render(&ctx, &with_inc));
        assert!(text.ends_with("(a, b) INCLUDE (total);"), "{text}");
        assert!(has(&caveats, "extendsIndex"));
        // ... but an index that already includes it is enough.
        old.definition = Some(
            "CREATE INDEX ix_ab ON public.orders USING btree (a, b) INCLUDE (total, x)".into(),
        );
        ctx.tables.insert(key.clone(), meta(None, vec![old]));
        assert!(matches!(
            render(&ctx, &with_inc),
            IndexOutcome::Existing { .. }
        ));
        // 4. Unrelated or partial indexes: a plain create; a taken name is numbered.
        let mut partial = index("ix_orders_a_b", &["a", "b"]);
        partial.partial = true;
        ctx.tables.insert(
            key,
            meta(
                None,
                vec![partial, index("ix_other", &["z"]), {
                    let mut h = index("ix_hash", &["a", "b"]);
                    h.method = Some("hash".into());
                    h
                }],
            ),
        );
        let (_, name, caveats) = created(render(&ctx, &c));
        assert_eq!(name, "ix_orders_a_b_2");
        assert!(!has(&caveats, "extendsIndex"));
    }

    #[test]
    fn existing_index_cases_mssql_use_drop_existing() {
        let rig = Rig::new(PlanFormat::MsSqlShowplanXml, "16.0");
        let mut ctx = rig.ctx();
        ctx.server.edition = Some("Enterprise Edition (64-bit)".into());
        let key = TableKey::new(Some("dbo"), "Orders");
        let mut c = cand(&ctx, "Orders", &["CustomerId", "OrderDate"], 1);
        c.include = vec!["Total".into()];
        let mut old = index("IX_Orders_CustomerId", &["CustomerId"]);
        old.method = Some("NONCLUSTERED".into());
        old.definition = Some(
            "CREATE NONCLUSTERED INDEX [IX_Orders_CustomerId] ON [dbo].[Orders] ([CustomerId])\nINCLUDE ([Status])"
                .into(),
        );
        ctx.tables
            .insert(key.clone(), meta(None, vec![old.clone()]));
        // 2. Extends a plain index: rebuild it under its own name, keeping its INCLUDE.
        let (text, name, caveats) = created(render(&ctx, &c));
        assert_eq!(name, "IX_Orders_CustomerId");
        assert_eq!(
            text,
            "CREATE NONCLUSTERED INDEX [IX_Orders_CustomerId] ON [dbo].[Orders] ([CustomerId], [OrderDate]) INCLUDE ([Total], [Status]) WITH (DROP_EXISTING = ON, ONLINE = ON);"
        );
        assert!(has(&caveats, "extendsIndex"));
        // 3. Same keys, only the INCLUDE differs.
        let mut same = cand(&ctx, "Orders", &["CustomerId"], 1);
        same.include = vec!["Total".into()];
        let (text, _, _) = created(render(&ctx, &same));
        assert!(
            text.ends_with("INCLUDE ([Total], [Status]) WITH (DROP_EXISTING = ON, ONLINE = ON);"),
            "{text}"
        );
        // A unique or primary index is never dropped.
        old.unique = true;
        ctx.tables
            .insert(key.clone(), meta(None, vec![old.clone()]));
        let (text, name, _) = created(render(&ctx, &c));
        assert!(!text.contains("DROP_EXISTING"));
        assert_eq!(name, "IX_Orders_CustomerId_OrderDate");
        // A clustered index is never replaced either.
        old.unique = false;
        old.method = Some("CLUSTERED".into());
        ctx.tables.insert(key, meta(None, vec![old]));
        assert!(!sql(render(&ctx, &c)).contains("DROP_EXISTING"));
    }

    #[test]
    fn mssql_online_depends_on_the_edition() {
        let rig = Rig::new(PlanFormat::MsSqlShowplanXml, "16.0");
        let mut ctx = rig.ctx();
        let c = cand(&ctx, "Orders", &["CustomerId"], 1);
        // Unknown: keep ONLINE, say it may fail.
        let (text, _, caveats) = created(render(&ctx, &c));
        assert!(text.contains("WITH (ONLINE = ON)"));
        assert!(has(&caveats, "onlineDdlMayFail"));
        ctx.server.edition = Some("Standard Edition (64-bit)".into());
        let (text, _, caveats) = created(render(&ctx, &c));
        assert!(!text.contains("WITH"));
        assert!(has(&caveats, "offlineIndexBuild"));
        ctx.server.engine_edition = Some(5);
        let (text, _, _) = created(render(&ctx, &c));
        assert!(text.contains("ONLINE = ON"));
    }

    #[test]
    fn oracle_online_only_on_enterprise_and_quotes_when_needed() {
        let rig = Rig::new(PlanFormat::OraclePlanRows, "19.0.0.0.0");
        let mut ctx = rig.ctx();
        let c = cand(&ctx, "ORDERS", &["CUSTOMER_ID"], 1);
        assert_eq!(
            sql(render(&ctx, &c)),
            "CREATE INDEX IX_ORDERS_CUSTOMER_ID ON SHOP.ORDERS (CUSTOMER_ID);"
        );
        ctx.server.edition = Some("Oracle Database 19c Enterprise Edition".into());
        assert!(sql(render(&ctx, &c)).ends_with("(CUSTOMER_ID) ONLINE;"));
        let c = cand(&ctx, "Orders", &["Customer Id"], 1);
        assert!(
            sql(render(&ctx, &c))
                .contains("\"IX_Orders_Customer_Id\" ON SHOP.\"Orders\" (\"Customer Id\")")
        );
    }

    #[test]
    fn mysql_family_folds_include_and_prefixes_text_columns() {
        let rig = Rig::new(PlanFormat::MySqlJson, "8.0.36");
        let mut ctx = rig.ctx();
        let key = TableKey::new(Some("shop"), "orders");
        let mut c = cand(&ctx, "orders", &["customer_id"], 1);
        c.include = vec!["total".into()];
        // Unknown column types: INCLUDE is dropped.
        let (text, _, caveats) = created(render(&ctx, &c));
        assert_eq!(
            text,
            "ALTER TABLE `shop`.`orders` ADD INDEX `ix_orders_customer_id` (`customer_id`), ALGORITHM=INPLACE, LOCK=NONE;"
        );
        assert!(has(&caveats, "includeDropped"));
        // Known, short types: appended to the key.
        let mut m = meta(None, vec![]);
        m.columns = vec![column("total", "decimal(10,2)"), column("title", "text")];
        ctx.tables.insert(key.clone(), m.clone());
        assert!(sql(render(&ctx, &c)).contains("(`customer_id`, `total`)"));
        // TEXT cannot be appended, and a TEXT key needs a prefix.
        c.include = vec!["title".into()];
        let (text, _, caveats) = created(render(&ctx, &c));
        assert!(!text.contains("title") && has(&caveats, "includeDropped"));
        let by_title = cand(&ctx, "orders", &["title"], 1);
        let (text, _, caveats) = created(render(&ctx, &by_title));
        assert!(text.contains("(`title`(191))"), "{text}");
        assert!(has(&caveats, "prefixIndex"));
        // TiDB has no ALTER ... ALGORITHM clause.
        let rig = Rig::new(PlanFormat::TiDbRows, "8.0.11-TiDB-v7.5.0");
        let ctx = rig.ctx();
        assert_eq!(
            sql(render(&ctx, &cand(&ctx, "orders", &["a", "b"], 1))),
            "CREATE INDEX `ix_orders_a_b` ON `shop`.`orders` (`a`, `b`);"
        );
        ctx.tables.get(&key);
    }

    #[test]
    fn mongo_and_clickhouse_syntax() {
        let rig = Rig::new(PlanFormat::MongoExplain, "7.0.5");
        let ctx = rig.ctx();
        let mut c = cand(&ctx, "orders", &["status", "created.at"], 1);
        c.keys[1].desc = true;
        let (text, name, _) = created(render(&ctx, &c));
        assert_eq!(
            text,
            "db.getSiblingDB(\"shop\").orders.createIndex({ status: 1, \"created.at\": -1 }, { name: \"status_1_created.at_-1\" })"
        );
        assert_eq!(name, "status_1_created.at_-1");
        let odd = cand(&ctx, "order-items", &["a"], 1);
        assert!(sql(render(&ctx, &odd)).contains(".getCollection(\"order-items\").createIndex("));
        let reserved = cand(&ctx, "stats", &["a"], 1);
        assert!(sql(render(&ctx, &reserved)).contains(".getCollection(\"stats\")"));
        // Server-named existing index with the same fields.
        let mut ctx = rig.ctx();
        ctx.tables.insert(
            TableKey::new(Some("shop"), "orders"),
            meta(None, vec![index("status_1", &["status"])]),
        );
        let plain = cand(&ctx, "orders", &["status"], 1);
        assert!(matches!(
            render(&ctx, &plain),
            IndexOutcome::Existing { .. }
        ));

        let rig = Rig::new(PlanFormat::ClickHouseJson, "24.8");
        let ctx = rig.ctx();
        let c = cand(&ctx, "orders", &["status", "created_at"], 1);
        let IndexOutcome::Create {
            action, follow_up, ..
        } = render(&ctx, &c)
        else {
            panic!()
        };
        assert_eq!(
            action.sql.as_deref(),
            Some(
                "ALTER TABLE `shop`.`orders` ADD INDEX IF NOT EXISTS `ix_status` `status` TYPE bloom_filter GRANULARITY 4;"
            )
        );
        assert_eq!(action.lock, Some(LockRisk::HeavyMutation));
        assert_eq!(follow_up.len(), 1);
        assert_eq!(
            follow_up[0].sql.as_deref(),
            Some("ALTER TABLE `shop`.`orders` MATERIALIZE INDEX `ix_status`;")
        );
        let range = cand(&ctx, "orders", &["created_at"], 0);
        assert!(sql(render(&ctx, &range)).contains("TYPE minmax"));
    }

    #[test]
    fn unknown_dialects_have_no_ddl() {
        let mut rig = Rig::new(PlanFormat::CqlTrace, "4.1");
        rig.raw.server_version = "4.1".into();
        let ctx = rig.ctx();
        assert_eq!(
            render(&ctx, &cand(&ctx, "orders", &["a"], 1)),
            IndexOutcome::Unsupported
        );
    }

    #[test]
    fn included_columns_are_read_from_definitions() {
        assert_eq!(
            included_columns(Some(
                "CREATE INDEX x ON t USING btree (a) INCLUDE (b, \"C d\")"
            )),
            vec!["b", "C d"]
        );
        assert_eq!(
            included_columns(Some(
                "CREATE NONCLUSTERED INDEX [x] ON [t] ([a])\nINCLUDE ([b])"
            )),
            vec!["b"]
        );
        assert_eq!(
            included_columns(Some("CREATE INDEX x ON t (a_include)")),
            Vec::<String>::new()
        );
        assert_eq!(included_columns(None), Vec::<String>::new());
    }
}
