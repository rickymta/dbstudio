//! The `Session` of a Cassandra / ScyllaDB connection.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use dbstudio_core::lease_kit::ActorLease;
use dbstudio_core::{
    ApplyResult, Capabilities, ChangeSet, ConnectionConfig, DbKind, DdlItem, DdlRequest,
    Editability, Error, ExecSummary, ExplainRequest, Lease, LeaseOptions, ObjectNode, ObjectPath,
    ObjectSearchRequest, QueryRequest, RawPlan, Result, ResultSink, SchemaModel,
    SchemaModelRequest, ScriptDialect, Secret, ServerInfo, Session, TableCountRequest,
    TableDataRequest, TableDetails,
};
use uuid::Uuid;

use crate::conn::Pool;
use crate::core::{Core, ServerFacts};
use crate::ddl;
use crate::lease::CassBackend;
use crate::rows::query_rows;
use crate::tree::{Node, parse_path};

/// A live connection to a Cassandra or ScyllaDB cluster.
pub struct CassandraSession {
    core: Arc<Core>,
}

impl std::fmt::Debug for CassandraSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CassandraSession")
            .field("flavor", &self.core.facts.flavor())
            .finish()
    }
}

async fn detect(pool: &Pool) -> Result<ServerFacts> {
    let base = pool.base();
    let rows = query_rows(
        &base,
        "SELECT release_version, cluster_name, data_center, partitioner FROM system.local",
    )
    .await?;
    let r = rows
        .first()
        .ok_or_else(|| Error::Connection("system.local is empty".into()))?;
    // Only ScyllaDB has this column.
    let scylla_version = query_rows(
        &base,
        "SELECT version FROM system.versions WHERE key = 'local'",
    )
    .await
    .ok()
    .and_then(|rows| rows.first().and_then(|r| r.str("version")));
    Ok(ServerFacts {
        version: scylla_version
            .clone()
            .unwrap_or_else(|| r.string("release_version")),
        cluster_name: r.string("cluster_name"),
        data_center: r.string("data_center"),
        partitioner: r.string("partitioner"),
        scylla: scylla_version.is_some(),
    })
}

impl CassandraSession {
    /// Connect with the profile of `config`.
    pub async fn connect(config: &ConnectionConfig, secret: Option<&Secret>) -> Result<Self> {
        let pool = Pool::connect(config, secret).await?;
        let facts = detect(&pool).await?;
        Ok(CassandraSession {
            core: Arc::new(Core::new(pool, facts)),
        })
    }

    fn kind(&self) -> DbKind {
        if self.core.facts.scylla {
            DbKind::ScyllaDb
        } else {
            DbKind::Cassandra
        }
    }
}

#[async_trait]
impl Session for CassandraSession {
    async fn ping(&self) -> Result<()> {
        self.core
            .pool
            .base()
            .query_unpaged("SELECT now() FROM system.local", &[])
            .await
            .map(|_| ())
            .map_err(|e| crate::error::from_execution(&e, ""))
    }

    async fn server_info(&self) -> Result<ServerInfo> {
        let f = &self.core.facts;
        let mut extra = BTreeMap::new();
        extra.insert("flavor".to_string(), f.flavor().to_string());
        extra.insert("clusterName".to_string(), f.cluster_name.clone());
        extra.insert("datacenter".to_string(), f.data_center.clone());
        extra.insert("partitioner".to_string(), f.partitioner.clone());
        if let Some(n) = &self.core.pool.notice {
            extra.insert("notice".to_string(), n.clone());
        }
        Ok(ServerInfo {
            product: if f.scylla {
                "ScyllaDB"
            } else {
                "Apache Cassandra"
            }
            .to_string(),
            version: f.version.clone(),
            extra,
        })
    }

    async fn execute(&self, req: QueryRequest, sink: &mut dyn ResultSink) -> Result<ExecSummary> {
        let mut state = self.core.initial_state(req.database.clone());
        self.core.run_script(&mut state, &req, sink, None).await
    }

    async fn cancel(&self, query_id: Uuid) -> Result<()> {
        self.core.cancel(query_id);
        Ok(())
    }

    async fn list_children(&self, parent: &ObjectPath) -> Result<Vec<ObjectNode>> {
        self.core.children(parent).await
    }

    async fn object_ddl(&self, path: &ObjectPath) -> Result<String> {
        self.core.ddl_of(path).await
    }

    async fn table_details(&self, path: &ObjectPath) -> Result<TableDetails> {
        self.core.details(path).await
    }

    async fn fetch_table(
        &self,
        req: TableDataRequest,
        sink: &mut dyn ResultSink,
    ) -> Result<ExecSummary> {
        self.core.fetch(req, sink).await
    }

    async fn count_table(&self, req: TableCountRequest) -> Result<u64> {
        let _ = req;
        Err(Error::Unsupported("exact_count".into()))
    }

    async fn search_objects(&self, req: ObjectSearchRequest) -> Result<Vec<ObjectNode>> {
        self.core.search(req).await
    }

    fn script_dialect(&self) -> ScriptDialect {
        ScriptDialect::Cql
    }

    async fn open_lease(&self, opts: LeaseOptions) -> Result<Box<dyn Lease>> {
        let keyspace = opts.database.clone().filter(|d| !d.is_empty());
        let backend = CassBackend::new(self.core.clone(), keyspace.clone());
        // Fail now when the keyspace does not exist.
        if let Some(ks) =
            keyspace
                .as_deref()
                .or(self.core.pool.settings.default_keyspace.as_deref())
        {
            self.core.pool.session(Some(ks)).await?;
        }
        Ok(Box::new(ActorLease::spawn(backend, opts)?))
    }

    async fn editability(&self, path: &ObjectPath) -> Result<Editability> {
        self.core.editability_of(path).await
    }

    async fn preview_changes(&self, changes: &ChangeSet) -> Result<Vec<String>> {
        self.core.preview(changes).await
    }

    async fn apply_changes(&self, changes: ChangeSet) -> Result<ApplyResult> {
        self.core.apply(changes).await
    }

    async fn explain(&self, req: ExplainRequest) -> Result<RawPlan> {
        let state = self.core.initial_state(req.database.clone());
        let session = self
            .core
            .pool
            .session(req.database.as_deref().or(state.keyspace.as_deref()))
            .await?;
        self.core.explain_select(&state, session, &req).await
    }

    async fn schema_model(&self, req: SchemaModelRequest) -> Result<SchemaModel> {
        self.core.model(req).await
    }

    async fn generate_ddl(&self, req: DdlRequest) -> Result<Vec<DdlItem>> {
        let snap = self.core.snapshot().await?;
        let scope = match parse_path(&req.scope)? {
            Node::Root | Node::SystemFolder => None,
            Node::Keyspace(k) | Node::KsFolder(k, _) => Some(k),
            _ => return Err(Error::Invalid("the DDL scope must be a keyspace".into())),
        };
        let mut tables = Vec::new();
        for o in &req.objects {
            match parse_path(o)? {
                Node::Table(k, t) => tables.push((k, t)),
                Node::Keyspace(_) => {}
                other => {
                    return Err(Error::Invalid(format!(
                        "only tables can be selected for the DDL, got {other:?}"
                    )));
                }
            }
        }
        if !req.objects.is_empty() && tables.is_empty() {
            return Ok(Vec::new());
        }
        ddl::generate(&snap, scope.as_deref(), &tables, &req.options)
    }

    fn capabilities(&self) -> Option<Capabilities> {
        Some(Capabilities::planned_for(self.kind()))
    }

    async fn close(&self) -> Result<()> {
        self.core.pool.clear();
        Ok(())
    }
}
