use serde::{Deserialize, Serialize};

/// Broad data-model family of a database product.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DbFamily {
    /// Tables, rows and (usually) SQL.
    Relational,
    /// JSON-like document stores.
    Document,
    /// Key-value stores.
    KeyValue,
    /// Wide-column stores.
    WideColumn,
    /// Column-oriented analytical databases.
    Columnar,
}

/// A supported database product.
///
/// The serialized form equals [`DbKind::id`], which is stable and safe to persist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum DbKind {
    #[serde(rename = "postgres")]
    Postgres,
    #[serde(rename = "mysql")]
    MySql,
    #[serde(rename = "sqlserver")]
    SqlServer,
    #[serde(rename = "oracle")]
    Oracle,
    #[serde(rename = "mongodb")]
    MongoDb,
    #[serde(rename = "redis")]
    Redis,
    #[serde(rename = "cassandra")]
    Cassandra,
    #[serde(rename = "scylladb")]
    ScyllaDb,
    #[serde(rename = "clickhouse")]
    ClickHouse,
}

impl DbKind {
    /// Every known kind, in a stable display order.
    pub const ALL: [DbKind; 9] = [
        DbKind::Postgres,
        DbKind::MySql,
        DbKind::SqlServer,
        DbKind::Oracle,
        DbKind::MongoDb,
        DbKind::Redis,
        DbKind::Cassandra,
        DbKind::ScyllaDb,
        DbKind::ClickHouse,
    ];

    /// Stable machine identifier (also the serialized form).
    pub fn id(self) -> &'static str {
        match self {
            DbKind::Postgres => "postgres",
            DbKind::MySql => "mysql",
            DbKind::SqlServer => "sqlserver",
            DbKind::Oracle => "oracle",
            DbKind::MongoDb => "mongodb",
            DbKind::Redis => "redis",
            DbKind::Cassandra => "cassandra",
            DbKind::ScyllaDb => "scylladb",
            DbKind::ClickHouse => "clickhouse",
        }
    }

    /// Human readable product name.
    pub fn display_name(self) -> &'static str {
        match self {
            DbKind::Postgres => "PostgreSQL",
            DbKind::MySql => "MySQL",
            DbKind::SqlServer => "SQL Server",
            DbKind::Oracle => "Oracle",
            DbKind::MongoDb => "MongoDB",
            DbKind::Redis => "Redis",
            DbKind::Cassandra => "Cassandra",
            DbKind::ScyllaDb => "ScyllaDB",
            DbKind::ClickHouse => "ClickHouse",
        }
    }

    /// Default TCP port (ClickHouse: the HTTP interface).
    pub fn default_port(self) -> u16 {
        match self {
            DbKind::Postgres => 5432,
            DbKind::MySql => 3306,
            DbKind::SqlServer => 1433,
            DbKind::Oracle => 1521,
            DbKind::MongoDb => 27017,
            DbKind::Redis => 6379,
            DbKind::Cassandra | DbKind::ScyllaDb => 9042,
            DbKind::ClickHouse => 8123,
        }
    }

    /// Data-model family.
    pub fn family(self) -> DbFamily {
        match self {
            DbKind::Postgres | DbKind::MySql | DbKind::SqlServer | DbKind::Oracle => {
                DbFamily::Relational
            }
            DbKind::MongoDb => DbFamily::Document,
            DbKind::Redis => DbFamily::KeyValue,
            DbKind::Cassandra | DbKind::ScyllaDb => DbFamily::WideColumn,
            DbKind::ClickHouse => DbFamily::Columnar,
        }
    }

    /// Parse a stable id produced by [`DbKind::id`].
    pub fn from_id(id: &str) -> Option<DbKind> {
        DbKind::ALL.into_iter().find(|k| k.id() == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_roundtrip_and_match_serde() {
        for kind in DbKind::ALL {
            assert_eq!(DbKind::from_id(kind.id()), Some(kind));
            assert_eq!(
                serde_json::to_string(&kind).unwrap(),
                format!("\"{}\"", kind.id())
            );
        }
        assert_eq!(DbKind::from_id("nope"), None);
    }

    #[test]
    fn families() {
        assert_eq!(DbKind::Redis.family(), DbFamily::KeyValue);
        assert_eq!(DbKind::ScyllaDb.family(), DbFamily::WideColumn);
        assert_eq!(
            serde_json::to_string(&DbFamily::WideColumn).unwrap(),
            "\"wideColumn\""
        );
    }
}
