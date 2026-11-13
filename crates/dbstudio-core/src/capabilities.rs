use serde::{Deserialize, Serialize};

use crate::DbKind;

/// Feature flags advertised by a driver. The UI toggles features from these flags only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    /// Accepts a SQL (or SQL-like, e.g. CQL) query language in the editor.
    pub sql: bool,
    /// Supports multi-statement transactions (begin / commit / rollback).
    pub transactions: bool,
    /// Has a schema level between database and table.
    pub schemas: bool,
    /// One server hosts several databases (or keyspaces / numbered DBs) to browse.
    pub multiple_databases: bool,
    /// Can show a query plan without executing the statement.
    pub explain: bool,
    /// Can execute the statement and report actual plan statistics.
    pub explain_analyze: bool,
    /// Exposes foreign-key metadata, enabling ER diagrams.
    pub foreign_keys: bool,
    /// A running query can be cancelled from another connection or request.
    pub cancel_query: bool,
    /// Logical backup / restore (dump of schema and data) is feasible.
    pub logical_backup: bool,
    /// Rows or documents can be edited in place through the data grid.
    pub data_editing: bool,
    /// Has stored functions and procedures.
    pub stored_routines: bool,
    /// Has triggers.
    pub triggers: bool,
    /// Has (possibly materialized) views.
    pub views: bool,
    /// Has sequence objects.
    pub sequences: bool,
}

impl Capabilities {
    /// Expected capabilities of a product once its driver is implemented.
    pub fn planned_for(kind: DbKind) -> Capabilities {
        let none = Capabilities::default();
        match kind {
            DbKind::Postgres => Capabilities {
                sql: true,
                transactions: true,
                schemas: true,
                multiple_databases: true,
                explain: true,
                explain_analyze: true,
                foreign_keys: true,
                cancel_query: true,
                logical_backup: true,
                data_editing: true,
                stored_routines: true,
                triggers: true,
                views: true,
                sequences: true,
            },
            DbKind::MySql => Capabilities {
                sql: true,
                transactions: true,
                multiple_databases: true,
                explain: true,
                explain_analyze: true,
                foreign_keys: true,
                cancel_query: true,
                logical_backup: true,
                data_editing: true,
                stored_routines: true,
                triggers: true,
                views: true,
                ..none
            },
            DbKind::SqlServer => Capabilities {
                sql: true,
                transactions: true,
                schemas: true,
                multiple_databases: true,
                explain: true,
                explain_analyze: true,
                foreign_keys: true,
                cancel_query: true,
                data_editing: true,
                stored_routines: true,
                triggers: true,
                views: true,
                sequences: true,
                ..none
            },
            DbKind::Oracle => Capabilities {
                sql: true,
                transactions: true,
                schemas: true,
                explain: true,
                foreign_keys: true,
                cancel_query: true,
                data_editing: true,
                stored_routines: true,
                triggers: true,
                views: true,
                sequences: true,
                ..none
            },
            DbKind::MongoDb => Capabilities {
                transactions: true,
                multiple_databases: true,
                explain: true,
                explain_analyze: true,
                cancel_query: true,
                logical_backup: true,
                data_editing: true,
                views: true,
                ..none
            },
            DbKind::Redis => Capabilities {
                transactions: true,
                multiple_databases: true,
                data_editing: true,
                ..none
            },
            DbKind::Cassandra => Capabilities {
                sql: true,
                multiple_databases: true,
                data_editing: true,
                stored_routines: true,
                triggers: true,
                views: true,
                ..none
            },
            DbKind::ScyllaDb => Capabilities {
                sql: true,
                multiple_databases: true,
                data_editing: true,
                stored_routines: true,
                views: true,
                ..none
            },
            DbKind::ClickHouse => Capabilities {
                sql: true,
                multiple_databases: true,
                explain: true,
                cancel_query: true,
                data_editing: true,
                views: true,
                ..none
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redis_and_mongo_shape() {
        let redis = Capabilities::planned_for(DbKind::Redis);
        assert!(!redis.sql && !redis.foreign_keys);
        let mongo = Capabilities::planned_for(DbKind::MongoDb);
        assert!(mongo.explain && !mongo.sql);
    }

    #[test]
    fn serializes_camel_case() {
        let json = serde_json::to_value(Capabilities::default()).unwrap();
        assert!(json.get("explainAnalyze").is_some());
        assert!(json.get("foreignKeys").is_some());
    }
}
