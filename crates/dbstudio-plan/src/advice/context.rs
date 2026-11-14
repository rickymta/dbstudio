//! Inputs of the advisor besides the plan: thresholds, dialect, server facts and (tier 2)
//! catalog metadata.

use std::collections::BTreeMap;

use dbstudio_core::{ColumnDetail, IndexDetail, ObjectPath, PlanFormat, RawPlan, ScriptDialect};
use serde::{Deserialize, Serialize};

use super::model::MetadataState;
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
            server_version: &raw.server_version,
            server: ServerFacts::default(),
            tables: BTreeMap::new(),
            metadata: MetadataState::NotRequested,
            captured_at: &raw.captured_at,
            now: "",
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
