//! The [`Session`] of an Oracle server: a small pool of connections plus the command set.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use dbstudio_core::{
    ApplyResult, ChangeSet, DdlItem, DdlRequest, Editability, Error, ExecSummary, ExplainRequest,
    Lease, LeaseOptions, ObjectKind, ObjectNode, ObjectPath, ObjectSearchRequest, QueryRequest,
    RawPlan, Result, ResultSink, RowChange, SchemaModel, SchemaModelRequest, ScriptDialect,
    ServerInfo, Session, TableCountRequest, TableDataRequest, TableDetails,
};
use uuid::Uuid;

use crate::exec::{ExecParams, run_script};
use crate::explain::{self, TxMode};
use crate::lease::OracleLease;
use crate::runtime::{Control, NullSink, Shared, stream};
use crate::sql::{non_folder_segments, relation_of, schema_object_of};
use crate::{browse, catalog, data, ddl, edit, model};

/// Session on an Oracle server (one user, one service).
#[derive(Debug)]
pub struct OracleSession {
    shared: Arc<Shared>,
}

impl OracleSession {
    pub(crate) fn new(shared: Arc<Shared>) -> Self {
        OracleSession { shared }
    }
}

fn elapsed_ms(t: Instant) -> u64 {
    u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// `(schema, kind, name)` of what `object_ddl` can describe, including indexes and triggers
/// listed under a table.
fn ddl_target(path: &ObjectPath) -> Result<(String, ObjectKind, String)> {
    use ObjectKind as K;
    match non_folder_segments(path).as_slice() {
        [
            (K::Schema, s),
            (K::Table | K::View | K::MaterializedView, _),
            (kind @ (K::Index | K::Trigger), n),
        ] => Ok(((*s).to_string(), *kind, (*n).to_string())),
        _ => schema_object_of(path),
    }
}

/// Schema named by a scope (root = the connected user).
fn scope_schema(scope: &ObjectPath) -> Result<Option<String>> {
    use ObjectKind as K;
    match non_folder_segments(scope).as_slice() {
        [] => Ok(None),
        [(K::Schema, s)] => Ok(Some((*s).to_string())),
        _ => Err(Error::NotFound(
            "the scope must be the root or a schema".into(),
        )),
    }
}

fn needs_first_column(set: &dbstudio_core::RowChangeSet) -> bool {
    set.changes
        .iter()
        .any(|c| matches!(c, RowChange::Insert { values } if values.is_empty()))
}

#[async_trait]
impl Session for OracleSession {
    async fn ping(&self) -> Result<()> {
        self.shared
            .with_conn(|conn| conn.ping().map_err(crate::error::map_error))
            .await
    }

    async fn server_info(&self) -> Result<ServerInfo> {
        self.shared
            .with_conn(|conn| {
                let (version, banner) = conn.server_version().map_err(crate::error::map_error)?;
                let mut extra = BTreeMap::new();
                extra.insert("banner".to_string(), banner.trim().to_string());
                let ctx = catalog::rows(
                    conn,
                    "SELECT SYS_CONTEXT('USERENV','SERVICE_NAME'), SYS_CONTEXT('USERENV','DB_NAME'), \
                            SYS_CONTEXT('USERENV','INSTANCE_NAME'), SYS_CONTEXT('USERENV','SESSION_USER'), \
                            SYS_CONTEXT('USERENV','CON_NAME'), SYS_CONTEXT('USERENV','SERVER_HOST') \
                     FROM dual",
                    &[],
                )?;
                if let Some(row) = ctx.first() {
                    for (key, i) in [
                        ("service", 0),
                        ("database", 1),
                        ("instance", 2),
                        ("user", 3),
                        ("container", 4),
                        ("host", 5),
                    ] {
                        if let Some(v) = row[i].clone().filter(|v| !v.is_empty()) {
                            extra.insert(key.to_string(), v);
                        }
                    }
                }
                Ok(ServerInfo {
                    product: "Oracle Database".to_string(),
                    version: version.to_string(),
                    extra,
                })
            })
            .await
    }

    async fn execute(&self, req: QueryRequest, sink: &mut dyn ResultSink) -> Result<ExecSummary> {
        let mut pooled = self.shared.acquire().await?;
        let conn = pooled.conn().clone();
        let ctl = Arc::new(Control::default());
        self.shared
            .register(req.query_id, conn.clone(), ctl.clone());
        let params = ExecParams {
            text: req.text.clone(),
            max_rows: req.max_rows,
            batch_size: req.batch_size.unwrap_or(500).clamp(1, 100_000) as usize,
            auto_commit: true,
            read_only: self.shared.read_only,
            max_lob: self.shared.target.max_lob_bytes,
            keep_open: false,
        };
        let timeout = req
            .timeout_ms
            .filter(|t| *t > 0)
            .map(std::time::Duration::from_millis);
        let result = stream(conn, ctl.clone(), timeout, sink, move |conn, em, ctl| {
            let r = run_script(conn, &params, em, ctl);
            if r.is_err() {
                let _ = conn.rollback();
            }
            r.map(|o| o.summary)
        })
        .await;
        self.shared.unregister(req.query_id);
        if let Err(Error::Connection(_)) = &result {
            pooled.discard();
        }
        result
    }

    async fn cancel(&self, query_id: Uuid) -> Result<()> {
        self.shared.cancel(query_id).await
    }

    async fn list_children(&self, parent: &ObjectPath) -> Result<Vec<ObjectNode>> {
        browse::list_children(&self.shared, parent).await
    }

    async fn object_ddl(&self, path: &ObjectPath) -> Result<String> {
        let (schema, kind, name) = ddl_target(path)?;
        self.shared
            .with_conn(move |conn| ddl::object_ddl(conn, &schema, kind, &name))
            .await
    }

    async fn table_details(&self, path: &ObjectPath) -> Result<TableDetails> {
        let (schema, name) = relation_of(path)?;
        self.shared
            .with_conn(move |conn| catalog::table_details(conn, &schema, &name))
            .await
    }

    async fn fetch_table(
        &self,
        req: TableDataRequest,
        sink: &mut dyn ResultSink,
    ) -> Result<ExecSummary> {
        let (schema, name) = relation_of(&req.table)?;
        let mut pooled = self.shared.acquire().await?;
        let conn = pooled.conn().clone();
        let ctl = Arc::new(Control::default());
        self.shared
            .register(req.query_id, conn.clone(), ctl.clone());
        let batch = req.batch_size.unwrap_or(500).clamp(1, 100_000) as usize;
        let max_lob = self.shared.target.max_lob_bytes;
        let started = Instant::now();
        let id = req.query_id;
        let result = stream(conn, ctl.clone(), None, sink, move |conn, em, ctl| {
            data::fetch_table_worker(
                conn,
                &schema,
                &name,
                &req.filters,
                req.raw_where.as_deref(),
                &req.sort,
                req.offset,
                req.limit,
                batch,
                max_lob,
                em,
                ctl,
            )
        })
        .await;
        self.shared.unregister(id);
        if ctl.is_abandoned() || matches!(&result, Err(Error::Connection(_))) {
            pooled.discard();
        }
        result.map(|()| ExecSummary {
            statements: 1,
            elapsed_ms: elapsed_ms(started),
        })
    }

    async fn count_table(&self, req: TableCountRequest) -> Result<u64> {
        let (schema, name) = relation_of(&req.table)?;
        let mut pooled = self.shared.acquire().await?;
        let conn = pooled.conn().clone();
        let ctl = Arc::new(Control::default());
        if let Some(id) = req.query_id {
            self.shared.register(id, conn.clone(), ctl);
        }
        let result = tokio::task::spawn_blocking(move || {
            data::count_worker(
                &conn,
                &schema,
                &name,
                &req.filters,
                req.raw_where.as_deref(),
            )
        })
        .await
        .map_err(|e| Error::Other(format!("worker failed: {e}")))?;
        if let Some(id) = req.query_id {
            self.shared.unregister(id);
        }
        if let Err(Error::Connection(_)) = &result {
            pooled.discard();
        }
        result
    }

    async fn search_objects(&self, req: ObjectSearchRequest) -> Result<Vec<ObjectNode>> {
        browse::search(&self.shared, req).await
    }

    fn script_dialect(&self) -> ScriptDialect {
        ScriptDialect::Oracle
    }

    async fn open_lease(&self, opts: LeaseOptions) -> Result<Box<dyn Lease>> {
        let conn = self.shared.open_dedicated().await?;
        let lease = OracleLease::new(self.shared.clone(), conn, &opts);
        lease.init_schema(opts.schema.as_deref()).await?;
        Ok(Box::new(lease))
    }

    async fn editability(&self, path: &ObjectPath) -> Result<Editability> {
        let (schema, name) = relation_of(path)?;
        let read_only = self.shared.read_only;
        self.shared
            .with_conn(move |conn| edit::editability(conn, &schema, &name, read_only))
            .await
    }

    async fn preview_changes(&self, changes: &ChangeSet) -> Result<Vec<String>> {
        let ChangeSet::Rows(set) = changes else {
            return Err(Error::Unsupported("data_editing".into()));
        };
        let first = if needs_first_column(set) {
            let (schema, name) = relation_of(&set.table)?;
            self.shared
                .with_conn(move |conn| {
                    let info = catalog::key_info(conn, &schema, &name)?;
                    Ok(info
                        .columns
                        .first()
                        .map(|c| c.0.clone())
                        .unwrap_or_default())
                })
                .await?
        } else {
            String::new()
        };
        edit::preview(changes, &first)
    }

    async fn apply_changes(&self, changes: ChangeSet) -> Result<ApplyResult> {
        if self.shared.read_only {
            return Err(Error::ReadOnlyViolation {
                message: "data cannot be changed on a read-only connection".into(),
                position: None,
            });
        }
        let ChangeSet::Rows(set) = changes else {
            return Err(Error::Unsupported("data_editing".into()));
        };
        let (schema, name) = relation_of(&set.table)?;
        self.shared
            .with_conn(move |conn| {
                let first = if needs_first_column(&set) {
                    let info = catalog::key_info(conn, &schema, &name)?;
                    info.columns
                        .first()
                        .map(|c| c.0.clone())
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                let built = edit::build(&set, &schema, &name, &first)?;
                edit::apply(conn, &built)
            })
            .await
    }

    async fn explain(&self, req: ExplainRequest) -> Result<RawPlan> {
        let mut pooled = self.shared.acquire().await?;
        let conn = pooled.conn().clone();
        let ctl = Arc::new(Control::default());
        self.shared
            .register(req.query_id, conn.clone(), ctl.clone());
        let timeout = req
            .timeout_ms
            .filter(|t| *t > 0)
            .map(std::time::Duration::from_millis);
        let read_only = self.shared.read_only;
        let id = req.query_id;
        let result = stream(
            conn,
            ctl.clone(),
            timeout,
            &mut NullSink,
            move |conn, _em, _ctl| {
                let r = explain::explain(conn, &req, read_only, TxMode::Detached);
                if r.is_err() {
                    let _ = conn.rollback();
                }
                r
            },
        )
        .await;
        self.shared.unregister(id);
        if ctl.is_abandoned() || matches!(&result, Err(Error::Connection(_))) {
            pooled.discard();
        }
        result
    }

    async fn schema_model(&self, req: SchemaModelRequest) -> Result<SchemaModel> {
        self.shared
            .with_conn(move |conn| {
                let schemas = model::scope_schemas(conn, &req.scope)?;
                model::build(conn, &req, &schemas)
            })
            .await
    }

    async fn generate_ddl(&self, req: DdlRequest) -> Result<Vec<DdlItem>> {
        let scope = scope_schema(&req.scope)?;
        self.shared
            .with_conn(move |conn| {
                let schema = match scope {
                    Some(s) => s,
                    None => catalog::scalar(
                        conn,
                        "SELECT SYS_CONTEXT('USERENV','SESSION_USER') FROM dual",
                        &[],
                    )?
                    .ok_or_else(|| Error::Other("cannot determine the current user".into()))?,
                };
                ddl::generate(conn, &req, &schema)
            })
            .await
    }

    async fn close(&self) -> Result<()> {
        self.shared.close().await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ddl_targets_cover_indexes_under_tables() {
        use ObjectKind as K;
        let table = ObjectPath::root()
            .child(K::Schema, "HR")
            .child(K::Folder, "tables")
            .child(K::Table, "EMP");
        let idx = table.child(K::Folder, "indexes").child(K::Index, "EMP_IX");
        assert_eq!(
            ddl_target(&idx).unwrap(),
            ("HR".into(), K::Index, "EMP_IX".into())
        );
        assert_eq!(
            ddl_target(&table).unwrap(),
            ("HR".into(), K::Table, "EMP".into())
        );
        let col = table.child(K::Folder, "columns").child(K::Column, "ID");
        assert!(ddl_target(&col).is_err());
    }

    #[test]
    fn scopes() {
        use ObjectKind as K;
        assert_eq!(scope_schema(&ObjectPath::root()).unwrap(), None);
        let s = ObjectPath::root().child(K::Schema, "HR");
        assert_eq!(scope_schema(&s).unwrap().as_deref(), Some("HR"));
        assert!(scope_schema(&s.child(K::Folder, "tables").child(K::Table, "T")).is_err());
    }
}
