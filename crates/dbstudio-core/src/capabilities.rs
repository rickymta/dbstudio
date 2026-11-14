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
    /// Supports server-side object search ([`crate::Session::search_objects`]).
    pub object_search: bool,
    /// Dedicated connection per editor tab ([`crate::Session::open_lease`]).
    #[serde(default)]
    pub leases: bool,
    /// Manual transactions (begin / commit / rollback, auto-commit off).
    #[serde(default)]
    pub manual_transactions: bool,
    /// `:name` / `$1` / `?` / `@p` query parameters.
    #[serde(default)]
    pub query_params: bool,
    /// EXPLAIN ANALYZE of data-modifying statements (run in a rolled back transaction).
    #[serde(default)]
    pub explain_analyze_writes: bool,
    /// Collections of documents (MongoDB).
    #[serde(default)]
    pub document_store: bool,
    /// Keys with typed values (Redis).
    #[serde(default)]
    pub key_value_store: bool,
    /// [`crate::Session::generate_ddl`] is available.
    #[serde(default)]
    pub ddl_generation: bool,
    /// [`crate::Session::schema_model`] is available (ERD, autocomplete).
    #[serde(default)]
    pub schema_model: bool,
    /// The schema model is inferred by sampling documents.
    #[serde(default)]
    pub schema_inference: bool,
    /// Whole-table streaming (export / backup) through a lease.
    #[serde(default)]
    pub table_stream: bool,
    /// Bulk insert (import) through a lease.
    #[serde(default)]
    pub bulk_insert: bool,
    /// Native backup tool integration (pg_dump, mysqldump...).
    #[serde(default)]
    pub native_backup: bool,
    /// The tree is paged ([`crate::Session::list_children_page`] with a real cursor).
    #[serde(default)]
    pub paged_tree: bool,
    /// The server enforces read-only for the session (otherwise only the app blocks writes).
    #[serde(default)]
    pub read_only_session: bool,
    /// Connections can go through an SSH tunnel.
    #[serde(default)]
    pub ssh_tunnel: bool,
    /// The data grid can sort by any column server-side (false: CQL, only clustering order).
    #[serde(default)]
    pub table_sort: bool,
    /// `count_table` returns an exact count cheaply enough for the grid (false: CQL full scan).
    #[serde(default)]
    pub exact_count: bool,
    /// The driver *can* host a routine debugger (PostgreSQL `pldbgapi`); whether the server really
    /// supports it is reported by `debug_support`.
    #[serde(default)]
    pub routine_debug: bool,
}

impl Capabilities {
    /// Expected capabilities of a product once its driver is implemented.
    pub fn planned_for(kind: DbKind) -> Capabilities {
        let none = Capabilities::default();
        match kind {
            DbKind::Postgres => Capabilities {
                object_search: true,
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
                leases: true,
                manual_transactions: true,
                query_params: true,
                explain_analyze_writes: true,
                ddl_generation: true,
                schema_model: true,
                table_stream: true,
                bulk_insert: true,
                native_backup: true,
                read_only_session: true,
                ssh_tunnel: true,
                table_sort: true,
                exact_count: true,
                routine_debug: true,
                ..none
            },
            DbKind::MySql => Capabilities {
                object_search: true,
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
                leases: true,
                manual_transactions: true,
                query_params: true,
                ddl_generation: true,
                schema_model: true,
                table_stream: true,
                bulk_insert: true,
                native_backup: true,
                read_only_session: true,
                ssh_tunnel: true,
                table_sort: true,
                exact_count: true,
                ..none
            },
            DbKind::SqlServer => Capabilities {
                object_search: true,
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
                leases: true,
                manual_transactions: true,
                query_params: true,
                explain_analyze_writes: true,
                ddl_generation: true,
                schema_model: true,
                table_stream: true,
                bulk_insert: true,
                native_backup: true,
                ssh_tunnel: true,
                table_sort: true,
                exact_count: true,
                ..none
            },
            DbKind::Oracle => Capabilities {
                object_search: true,
                sql: true,
                transactions: true,
                schemas: true,
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
                leases: true,
                manual_transactions: true,
                query_params: true,
                explain_analyze_writes: true,
                ddl_generation: true,
                schema_model: true,
                table_stream: true,
                bulk_insert: true,
                native_backup: true,
                ssh_tunnel: true,
                table_sort: true,
                exact_count: true,
                ..none
            },
            DbKind::MongoDb => Capabilities {
                object_search: true,
                transactions: true,
                multiple_databases: true,
                explain: true,
                explain_analyze: true,
                cancel_query: true,
                logical_backup: true,
                data_editing: true,
                views: true,
                leases: true,
                manual_transactions: true,
                explain_analyze_writes: true,
                document_store: true,
                ddl_generation: true,
                schema_model: true,
                schema_inference: true,
                table_stream: true,
                bulk_insert: true,
                native_backup: true,
                ssh_tunnel: true,
                table_sort: true,
                exact_count: true,
                ..none
            },
            DbKind::Redis => Capabilities {
                transactions: true,
                multiple_databases: true,
                cancel_query: true,
                logical_backup: true,
                data_editing: true,
                leases: true,
                key_value_store: true,
                native_backup: true,
                paged_tree: true,
                ssh_tunnel: true,
                ..none
            },
            DbKind::Cassandra => cassandra_family(true),
            DbKind::ScyllaDb => cassandra_family(false),
            DbKind::ClickHouse => Capabilities {
                object_search: true,
                sql: true,
                multiple_databases: true,
                explain: true,
                cancel_query: true,
                logical_backup: true,
                data_editing: true,
                views: true,
                leases: true,
                query_params: true,
                ddl_generation: true,
                schema_model: true,
                table_stream: true,
                bulk_insert: true,
                native_backup: true,
                read_only_session: true,
                ssh_tunnel: true,
                table_sort: true,
                exact_count: true,
                ..none
            },
            // PostgreSQL flavours. `triggers` of CockroachDB depends on the server version (>= 24.3)
            // and is switched on by `Session::capabilities()`.
            DbKind::CockroachDb => Capabilities {
                triggers: false,
                routine_debug: false,
                ..Capabilities::planned_for(DbKind::Postgres)
            },
            DbKind::YugabyteDb => Capabilities::planned_for(DbKind::Postgres),
            // TiDB: MySQL protocol; no stored routines/triggers, has sequences. Read-only sessions
            // stay off until integration tests prove them.
            DbKind::TiDb => Capabilities {
                stored_routines: false,
                triggers: false,
                sequences: true,
                read_only_session: false,
                ..Capabilities::planned_for(DbKind::MySql)
            },
        }
    }
}

/// Cassandra (`cassandra = true`) and ScyllaDB (`false`): same driver, Scylla has no triggers and
/// its UDFs are experimental.
fn cassandra_family(cassandra: bool) -> Capabilities {
    Capabilities {
        sql: true,
        multiple_databases: true,
        // Tracing (`system_traces`) is exposed as the plan.
        explain_analyze: true,
        // Client-side cancel (the request future is dropped).
        cancel_query: true,
        logical_backup: true,
        data_editing: true,
        stored_routines: cassandra,
        triggers: cassandra,
        views: true,
        object_search: true,
        leases: true,
        query_params: true,
        ddl_generation: true,
        schema_model: true,
        table_stream: true,
        bulk_insert: true,
        ssh_tunnel: true,
        ..Capabilities::default()
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
        assert!(mongo.object_search && !redis.object_search);
        assert!(Capabilities::planned_for(DbKind::Postgres).object_search);
    }

    #[test]
    fn serializes_camel_case() {
        let json = serde_json::to_value(Capabilities::default()).unwrap();
        assert!(json.get("explainAnalyze").is_some());
        assert!(json.get("foreignKeys").is_some());
        assert_eq!(json["objectSearch"], false);
    }

    #[test]
    fn phase2_table() {
        let pg = Capabilities::planned_for(DbKind::Postgres);
        assert!(pg.leases && pg.query_params && pg.read_only_session && pg.explain_analyze_writes);
        assert!(!pg.document_store && !pg.key_value_store && !pg.paged_tree);
        let my = Capabilities::planned_for(DbKind::MySql);
        assert!(!my.explain_analyze_writes && my.read_only_session && !my.schemas);
        let ms = Capabilities::planned_for(DbKind::SqlServer);
        assert!(ms.logical_backup && ms.explain_analyze_writes && !ms.read_only_session);
        let ora = Capabilities::planned_for(DbKind::Oracle);
        assert!(
            ora.explain_analyze && ora.logical_backup && ora.schemas && !ora.multiple_databases
        );
        let mongo = Capabilities::planned_for(DbKind::MongoDb);
        assert!(mongo.document_store && mongo.schema_inference && mongo.manual_transactions);
        assert!(!mongo.query_params && !mongo.sql && !mongo.foreign_keys);
        let redis = Capabilities::planned_for(DbKind::Redis);
        assert!(
            redis.key_value_store && redis.paged_tree && redis.logical_backup && redis.cancel_query
        );
        assert!(!redis.manual_transactions && !redis.schema_model && !redis.bulk_insert);
    }

    #[test]
    fn backlog_v2_table() {
        for kind in [
            DbKind::Postgres,
            DbKind::MySql,
            DbKind::SqlServer,
            DbKind::Oracle,
            DbKind::MongoDb,
            DbKind::ClickHouse,
            DbKind::CockroachDb,
            DbKind::YugabyteDb,
            DbKind::TiDb,
        ] {
            let c = Capabilities::planned_for(kind);
            assert!(c.table_sort && c.exact_count, "{kind:?}");
        }
        let redis = Capabilities::planned_for(DbKind::Redis);
        assert!(!redis.table_sort && !redis.exact_count);
        let cass = Capabilities::planned_for(DbKind::Cassandra);
        assert!(cass.sql && cass.triggers && cass.explain_analyze && cass.leases);
        assert!(!cass.table_sort && !cass.exact_count && !cass.schemas && !cass.transactions);
        assert!(!cass.explain && !cass.foreign_keys && !cass.native_backup);
        let scylla = Capabilities::planned_for(DbKind::ScyllaDb);
        assert!(!scylla.triggers && !scylla.stored_routines && scylla.views);
        let ch = Capabilities::planned_for(DbKind::ClickHouse);
        assert!(ch.explain && !ch.explain_analyze && !ch.transactions && ch.native_backup);
        assert!(ch.read_only_session && ch.table_sort && !ch.schemas);
        let crdb = Capabilities::planned_for(DbKind::CockroachDb);
        assert!(!crdb.triggers && crdb.native_backup && crdb.schemas);
        let tidb = Capabilities::planned_for(DbKind::TiDb);
        assert!(
            tidb.sequences && !tidb.triggers && !tidb.stored_routines && !tidb.read_only_session
        );
    }

    #[test]
    fn missing_new_flags_default_to_false() {
        let caps: Capabilities = serde_json::from_value(serde_json::json!({
            "sql": true, "transactions": false, "schemas": false, "multipleDatabases": false,
            "explain": false, "explainAnalyze": false, "foreignKeys": false, "cancelQuery": false,
            "logicalBackup": false, "dataEditing": false, "storedRoutines": false,
            "triggers": false, "views": false, "sequences": false, "objectSearch": false
        }))
        .unwrap();
        assert!(caps.sql && !caps.leases && !caps.ssh_tunnel);
        let json = serde_json::to_value(Capabilities::default()).unwrap();
        for key in [
            "leases",
            "manualTransactions",
            "queryParams",
            "explainAnalyzeWrites",
            "documentStore",
            "keyValueStore",
            "ddlGeneration",
            "schemaModel",
            "schemaInference",
            "tableStream",
            "bulkInsert",
            "nativeBackup",
            "pagedTree",
            "readOnlySession",
            "sshTunnel",
            "tableSort",
            "exactCount",
        ] {
            assert_eq!(json[key], false, "{key}");
        }
    }
}
