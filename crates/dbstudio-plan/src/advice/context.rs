//! Inputs of the advisor besides the plan: thresholds, dialect, server facts and (tier 2)
//! catalog metadata.

use std::collections::BTreeMap;

use dbstudio_core::{
    ColumnDetail, IndexDetail, ObjectKind, ObjectNode, ObjectPath, PlanFormat, RawPlan,
    ScriptDialect,
};
use serde::{Deserialize, Serialize};

use super::model::{MetadataState, TableRef};
use crate::PlanRules;

/// Facts about the server that change what is advisable.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ServerFacts {
    /// SQL Server / Oracle edition text (`Enterprise Edition`, `Developer Edition`...).
    pub edition: Option<String>,
    /// SQL Server `EngineEdition` (3 = Enterprise, 5 = Azure SQL DB, 8 = managed instance).
    pub engine_edition: Option<u32>,
    /// The plan ran on a lease that is inside a transaction (`CREATE INDEX CONCURRENTLY` fails).
    pub in_transaction: bool,
}

/// Lower-cased identity of a table inside the plan (`schema` may be unknown).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct TableKey {
    /// Schema or database, lower-cased, when the plan names it.
    pub schema: Option<String>,
    /// Table name, lower-cased.
    pub name: String,
}

impl TableKey {
    /// Key of `schema.name` (case folded).
    pub fn new(schema: Option<&str>, name: &str) -> Self {
        TableKey {
            schema: schema.map(str::to_lowercase),
            name: name.to_lowercase(),
        }
    }
}

/// What the catalog says about one table (tier 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableMeta {
    /// Resolved path.
    pub path: ObjectPath,
    /// Estimated row count from statistics.
    pub estimated_rows: Option<u64>,
    /// Columns.
    pub columns: Vec<ColumnDetail>,
    /// Indexes.
    pub indexes: Vec<IndexDetail>,
}

/// Everything [`super::advise`] knows besides the plan and the statement.
#[derive(Debug, Clone)]
pub struct AdvisorContext<'a> {
    /// Thresholds.
    pub rules: &'a PlanRules,
    /// Language of the connection (SQL flavour or mongo shell).
    pub dialect: ScriptDialect,
    /// Format of the plan; tells apart products that share a dialect (CockroachDB, TiDB).
    pub format: PlanFormat,
    /// `RawPlan.server_version`.
    pub server_version: &'a str,
    /// Server facts.
    pub server: ServerFacts,
    /// Catalog metadata by table; empty in tier 1.
    pub tables: BTreeMap<TableKey, TableMeta>,
    /// State of the catalog lookup behind `tables`.
    pub metadata: MetadataState,
    /// `RawPlan.captured_at` (RFC 3339), for the `planMayBeStale` caveat.
    pub captured_at: &'a str,
    /// Current time as RFC 3339 UTC when known (tests leave it empty: no staleness caveat).
    pub now: &'a str,
}

impl<'a> AdvisorContext<'a> {
    /// Tier 1 context: the plan alone, no catalog metadata.
    pub fn basic(raw: &'a RawPlan, rules: &'a PlanRules) -> Self {
        AdvisorContext {
            rules,
            dialect: dialect_of_format(raw.format, &raw.server_version),
            format: raw.format,
            server_version: &raw.server_version,
            server: ServerFacts::default(),
            tables: BTreeMap::new(),
            metadata: MetadataState::NotRequested,
            captured_at: &raw.captured_at,
            now: "",
        }
    }

    /// Database product behind the plan, for DDL syntax and naming limits.
    pub fn flavor(&self) -> Flavor {
        match self.dialect {
            ScriptDialect::Postgres => {
                if self.format == PlanFormat::CockroachText {
                    Flavor::Cockroach
                } else if self.is_yugabyte() {
                    Flavor::Yugabyte
                } else {
                    Flavor::Postgres
                }
            }
            ScriptDialect::MySql { mariadb, .. } => {
                if self.format == PlanFormat::TiDbRows {
                    Flavor::TiDb
                } else if mariadb {
                    Flavor::MariaDb
                } else {
                    Flavor::MySql
                }
            }
            ScriptDialect::MsSql => Flavor::MsSql,
            ScriptDialect::Oracle => Flavor::Oracle,
            ScriptDialect::MongoShell => Flavor::Mongo,
            ScriptDialect::ClickHouse => Flavor::ClickHouse,
            _ => Flavor::Other,
        }
    }

    /// Server version is known and older than `major.minor` (`12.1.0.2` < `12.2`).
    pub fn version_before(&self, major: u32, minor: u32) -> bool {
        let mut nums = self
            .server_version
            .split(|c: char| !c.is_ascii_digit())
            .filter(|p| !p.is_empty())
            .map(|p| p.parse::<u32>().unwrap_or(0));
        match (nums.next(), nums.next()) {
            (Some(a), Some(b)) => (a, b) < (major, minor),
            (Some(a), None) => a < major,
            _ => true,
        }
    }

    /// YugabyteDB reports `PostgreSQL 11.2-YB-2.20...`.
    pub fn is_yugabyte(&self) -> bool {
        self.server_version.contains("-YB-")
    }

    /// Major version of the server when the version string starts with it
    /// (`16.2`, `PostgreSQL 15.4 on ...`).
    pub fn major_version(&self) -> Option<u32> {
        self.server_version
            .split(|c: char| !c.is_ascii_digit())
            .find(|p| !p.is_empty())
            .and_then(|p| p.parse().ok())
    }

    /// Metadata of the table of a plan node, when tier 2 found it.
    pub fn table_meta(&self, schema: Option<&str>, name: &str) -> Option<&TableMeta> {
        let exact = TableKey::new(schema, name);
        self.tables.get(&exact).or_else(|| {
            let bare = TableKey::new(None, name);
            self.tables.get(&bare).or_else(|| {
                self.tables
                    .iter()
                    .find(|(k, _)| k.name == bare.name && schema.is_none())
                    .map(|(_, v)| v)
            })
        })
    }
}

/// Database product, finer than [`ScriptDialect`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    /// PostgreSQL.
    Postgres,
    /// YugabyteDB (PostgreSQL wire and syntax).
    Yugabyte,
    /// CockroachDB.
    Cockroach,
    /// MySQL.
    MySql,
    /// MariaDB.
    MariaDb,
    /// TiDB.
    TiDb,
    /// SQL Server.
    MsSql,
    /// Oracle.
    Oracle,
    /// MongoDB.
    Mongo,
    /// ClickHouse.
    ClickHouse,
    /// No index DDL known.
    Other,
}

/// Result of matching a plan table against the objects a catalog search returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// Exactly one table.
    Found(ObjectPath),
    /// Several tables share the name (and the schema, when the plan gave none).
    Ambiguous,
    /// No such table.
    NotFound,
}

/// Database node a catalog search for `table` should be limited to: the plan's database, the
/// schema of products where a schema is a database (MySQL, MongoDB, ClickHouse), else the
/// database of the explain request; the root when none is known.
pub fn search_scope(
    flavor: Flavor,
    table: &TableRef,
    request_database: Option<&str>,
) -> ObjectPath {
    let by_schema = matches!(
        flavor,
        Flavor::MySql | Flavor::MariaDb | Flavor::TiDb | Flavor::Mongo | Flavor::ClickHouse
    );
    let db = table
        .database
        .as_deref()
        .or(if by_schema {
            table.schema.as_deref()
        } else {
            None
        })
        .or(request_database)
        .filter(|d| !d.is_empty());
    match db {
        Some(d) => ObjectPath::root().child(ObjectKind::Database, d),
        None => ObjectPath::root(),
    }
}

/// Kind of object to search for.
pub fn search_kind(flavor: Flavor) -> ObjectKind {
    if flavor == Flavor::Mongo {
        ObjectKind::Collection
    } else {
        ObjectKind::Table
    }
}

/// Pick the table `table` names among `found` (design section 5.5): exact name first, then
/// case-insensitive; the plan's schema filters; without one the first schema of `search_path`
/// (in order, as PostgreSQL resolves a bare name) that holds a match wins.
pub fn resolve_table(
    flavor: Flavor,
    table: &TableRef,
    search_path: &[String],
    found: &[ObjectNode],
) -> Resolution {
    let kind = search_kind(flavor);
    let of_kind: Vec<&ObjectNode> = found.iter().filter(|n| n.kind == kind).collect();
    let mut named: Vec<&ObjectNode> = of_kind
        .iter()
        .copied()
        .filter(|n| n.name == table.name)
        .collect();
    if named.is_empty() {
        named = of_kind
            .iter()
            .copied()
            .filter(|n| n.name.eq_ignore_ascii_case(&table.name))
            .collect();
    }
    let schema_kinds_only = matches!(
        flavor,
        Flavor::Postgres | Flavor::Yugabyte | Flavor::Cockroach | Flavor::MsSql | Flavor::Oracle
    );
    let schema_of = |n: &ObjectNode| -> Vec<String> {
        n.path
            .segments
            .iter()
            .filter(|s| {
                s.kind == ObjectKind::Schema
                    || (!schema_kinds_only && s.kind == ObjectKind::Database)
            })
            .map(|s| s.name.to_lowercase())
            .collect()
    };
    let mut cands = named;
    if let Some(schema) = table.schema.as_deref().filter(|s| !s.is_empty()) {
        let want = schema.to_lowercase();
        cands.retain(|n| schema_of(n).contains(&want));
    } else if cands.len() > 1 {
        for want in search_path.iter().map(|d| d.to_lowercase()) {
            let in_schema: Vec<&ObjectNode> = cands
                .iter()
                .copied()
                .filter(|n| schema_of(n).contains(&want))
                .collect();
            if !in_schema.is_empty() {
                cands = in_schema;
                break;
            }
        }
    }
    let mut paths: Vec<&ObjectPath> = cands.iter().map(|n| &n.path).collect();
    paths.dedup();
    match paths.as_slice() {
        [] => Resolution::NotFound,
        [one] => Resolution::Found((*one).clone()),
        _ => Resolution::Ambiguous,
    }
}

/// Script dialect a plan format belongs to.
pub fn dialect_of_format(format: PlanFormat, server_version: &str) -> ScriptDialect {
    let mysql = |mariadb| ScriptDialect::MySql {
        mariadb,
        backslash_escapes: true,
        ansi_quotes: false,
    };
    match format {
        PlanFormat::PostgresJson | PlanFormat::CockroachText => ScriptDialect::Postgres,
        PlanFormat::MySqlJson | PlanFormat::MySqlJsonV2 | PlanFormat::MySqlTree => {
            mysql(server_version.to_ascii_lowercase().contains("mariadb"))
        }
        PlanFormat::MariaDbJson => mysql(true),
        PlanFormat::TiDbRows => mysql(false),
        PlanFormat::MsSqlShowplanXml => ScriptDialect::MsSql,
        PlanFormat::OraclePlanRows => ScriptDialect::Oracle,
        PlanFormat::MongoExplain => ScriptDialect::MongoShell,
        PlanFormat::ClickHouseJson => ScriptDialect::ClickHouse,
        PlanFormat::CqlTrace => ScriptDialect::Cql,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(kind: ObjectKind, schema: &str, name: &str) -> ObjectNode {
        ObjectNode {
            path: ObjectPath::root()
                .child(ObjectKind::Database, "shop")
                .child(ObjectKind::Schema, schema)
                .child(kind, name),
            kind,
            name: name.into(),
            has_children: false,
            detail: None,
        }
    }

    fn table(schema: Option<&str>, name: &str) -> TableRef {
        TableRef {
            database: None,
            schema: schema.map(str::to_string),
            name: name.into(),
            alias: None,
            path: None,
        }
    }

    #[test]
    fn scope_follows_the_product() {
        let t = table(Some("shop"), "orders");
        assert_eq!(
            search_scope(Flavor::MySql, &t, Some("other")),
            ObjectPath::root().child(ObjectKind::Database, "shop")
        );
        // PostgreSQL: the schema is not a database; the request database is.
        assert_eq!(
            search_scope(Flavor::Postgres, &t, Some("other")),
            ObjectPath::root().child(ObjectKind::Database, "other")
        );
        assert_eq!(search_scope(Flavor::Postgres, &t, None), ObjectPath::root());
        let mut with_db = t.clone();
        with_db.database = Some("db2".into());
        assert_eq!(
            search_scope(Flavor::MsSql, &with_db, Some("other")),
            ObjectPath::root().child(ObjectKind::Database, "db2")
        );
        assert_eq!(search_kind(Flavor::Mongo), ObjectKind::Collection);
        assert_eq!(search_kind(Flavor::Oracle), ObjectKind::Table);
    }

    #[test]
    fn resolution_rules() {
        let found = vec![
            node(ObjectKind::Table, "sales", "orders"),
            node(ObjectKind::Table, "archive", "orders"),
            node(ObjectKind::Table, "public", "orders_old"),
            node(ObjectKind::View, "sales", "orders"),
        ];
        // A schema in the plan decides.
        let r = resolve_table(
            Flavor::Postgres,
            &table(Some("SALES"), "orders"),
            &[],
            &found,
        );
        assert!(matches!(r, Resolution::Found(p) if p.segments[1].name == "sales"));
        // None given: two candidates are ambiguous...
        assert_eq!(
            resolve_table(
                Flavor::Postgres,
                &table(None, "orders"),
                &["public".to_string()],
                &found
            ),
            Resolution::Ambiguous
        );
        // ... unless exactly one is in the default schema.
        let r = resolve_table(
            Flavor::Postgres,
            &table(None, "orders"),
            &["archive".to_string()],
            &found,
        );
        assert!(matches!(r, Resolution::Found(p) if p.segments[1].name == "archive"));
        // The search path decides in order: `archive` first even though `public` has one too.
        let path = ["archive".to_string(), "public".to_string()];
        let r = resolve_table(Flavor::Postgres, &table(None, "orders"), &path, &found);
        assert!(matches!(r, Resolution::Found(p) if p.segments[1].name == "archive"));
        let path = ["public".to_string(), "archive".to_string()];
        let r = resolve_table(Flavor::Postgres, &table(None, "orders"), &path, &found);
        assert!(matches!(r, Resolution::Found(p) if p.segments[1].name == "public"));
        // Neither schema on the path: guessing would be wrong.
        let path = ["elsewhere".to_string()];
        assert_eq!(
            resolve_table(Flavor::Postgres, &table(None, "orders"), &path, &found),
            Resolution::Ambiguous
        );
        // Only exact names count (`orders_old` is a substring match of the search).
        assert_eq!(
            resolve_table(
                Flavor::Postgres,
                &table(Some("public"), "orders"),
                &[],
                &found
            ),
            Resolution::NotFound
        );
        // Case-insensitive fallback.
        let r = resolve_table(Flavor::Oracle, &table(Some("sales"), "ORDERS"), &[], &found);
        assert!(matches!(r, Resolution::Found(_)));
        // Views are not tables.
        let only_view = vec![node(ObjectKind::View, "sales", "v")];
        assert_eq!(
            resolve_table(Flavor::Postgres, &table(None, "v"), &[], &only_view),
            Resolution::NotFound
        );
    }

    #[test]
    fn flavor_tells_products_of_one_dialect_apart() {
        let rules = PlanRules::default();
        let raw = |format, version: &str| RawPlan {
            format,
            text: String::new(),
            statement: String::new(),
            analyzed: false,
            rolled_back: false,
            server_version: version.into(),
            captured_at: String::new(),
            extra: BTreeMap::new(),
        };
        let flavor = |format, version: &str| {
            let r = raw(format, version);
            AdvisorContext::basic(&r, &rules).flavor()
        };
        assert_eq!(flavor(PlanFormat::PostgresJson, "16"), Flavor::Postgres);
        assert_eq!(
            flavor(PlanFormat::PostgresJson, "11.2-YB-2.20"),
            Flavor::Yugabyte
        );
        assert_eq!(flavor(PlanFormat::CockroachText, "v23"), Flavor::Cockroach);
        assert_eq!(flavor(PlanFormat::MySqlJson, "8.0"), Flavor::MySql);
        assert_eq!(
            flavor(PlanFormat::MySqlTree, "10.6.1-MariaDB"),
            Flavor::MariaDb
        );
        assert_eq!(flavor(PlanFormat::TiDbRows, "8.0.11-TiDB"), Flavor::TiDb);
        assert_eq!(flavor(PlanFormat::CqlTrace, "4"), Flavor::Other);
        let r = raw(PlanFormat::OraclePlanRows, "12.1.0.2.0");
        let ctx = AdvisorContext::basic(&r, &rules);
        assert!(ctx.version_before(12, 2));
        assert!(!ctx.version_before(12, 1));
        assert!(!ctx.version_before(11, 5));
    }
}
