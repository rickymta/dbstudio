//! The connection an export or import runs on: a lease (one physical connection, so a
//! transaction / snapshot spans the whole job) when the driver has leases, else the session.

use std::sync::Arc;

use dbstudio_core::{
    CancelToken, CollectingSink, Error, ExecSummary, Lease, LeaseOptions, ObjectKind, ObjectPath,
    QueryRequest, Result, ResultSink, ScriptDialect, Session, TxOptions,
};
use uuid::Uuid;

/// Database a path belongs to (its `Database` segment).
pub(crate) fn database_of(path: &ObjectPath) -> Option<String> {
    path.segments
        .iter()
        .find(|s| s.kind == ObjectKind::Database)
        .map(|s| s.name.clone())
}

pub(crate) struct Conn {
    session: Arc<dyn Session>,
    lease: Option<Box<dyn Lease>>,
    /// Database requests run in when the session (not a lease) executes them.
    database: Option<String>,
}

impl std::fmt::Debug for Conn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Conn")
            .field("lease", &self.lease.is_some())
            .field("database", &self.database)
            .finish()
    }
}

impl Conn {
    /// Open a lease on `database` when the driver supports it, with UTC session time and the
    /// transaction `tx` (`None` = auto-commit). A lease that cannot start its transaction is an
    /// error: the caller asked for a snapshot / read-only guarantee and must not lose it
    /// silently. Without lease support the session is used and the returned note says that
    /// there is no transaction.
    pub async fn open(
        session: Arc<dyn Session>,
        database: Option<String>,
        tx: Option<TxOptions>,
    ) -> Result<(Conn, Option<String>)> {
        let opts = LeaseOptions {
            database: database.clone(),
            ..LeaseOptions::default()
        };
        match session.open_lease(opts).await {
            Ok(lease) => {
                let conn = Conn {
                    session: Arc::clone(&session),
                    lease: Some(lease),
                    database,
                };
                if let Err(e) = conn.prepare(tx).await {
                    conn.close().await;
                    return Err(e);
                }
                Ok((conn, None))
            }
            Err(Error::Unsupported(_)) => Ok((
                Conn {
                    session,
                    lease: None,
                    database,
                },
                Some("the driver has no dedicated connections: no consistent snapshot".into()),
            )),
            Err(e) => Err(e),
        }
    }

    /// UTC session time (values are exported the same whatever the server's zone), then the
    /// transaction.
    async fn prepare(&self, tx: Option<TxOptions>) -> Result<()> {
        let Some(lease) = self.lease.as_deref() else {
            return Ok(());
        };
        let set_zone = match self.session.script_dialect() {
            ScriptDialect::MySql { .. } => Some("SET time_zone = '+00:00'"),
            ScriptDialect::Postgres => Some("SET TIME ZONE 'UTC'"),
            _ => None,
        };
        if let Some(sql) = set_zone {
            lease
                .execute(QueryRequest::new(sql), &mut CollectingSink::new())
                .await?;
        }
        if let Some(opts) = tx {
            lease.begin(opts).await?;
        }
        Ok(())
    }

    pub fn session(&self) -> &Arc<dyn Session> {
        &self.session
    }

    pub fn lease(&self) -> Option<&dyn Lease> {
        self.lease.as_deref()
    }

    pub async fn execute(
        &self,
        mut req: QueryRequest,
        sink: &mut dyn ResultSink,
    ) -> Result<ExecSummary> {
        match &self.lease {
            Some(l) => l.execute(req, sink).await,
            None => {
                if req.database.is_none() {
                    req.database.clone_from(&self.database);
                }
                self.session.execute(req, sink).await
            }
        }
    }

    pub async fn cancel(&self, query_id: Uuid) -> Result<()> {
        match &self.lease {
            Some(l) => l.cancel(query_id).await,
            None => self.session.cancel(query_id).await,
        }
    }

    /// Run `req`; cancelling `token` aborts the statement on the server and ends the call with
    /// `Cancelled`.
    pub async fn execute_cancellable(
        self: &Arc<Self>,
        token: &CancelToken,
        req: QueryRequest,
        sink: &mut dyn ResultSink,
    ) -> Result<ExecSummary> {
        let query_id = req.query_id;
        let watcher = {
            let conn = Arc::clone(self);
            let token = token.clone();
            tokio::spawn(async move {
                token.cancelled().await;
                let _ = conn.cancel(query_id).await;
            })
        };
        let result = self.execute(req, sink).await;
        watcher.abort();
        match result {
            Err(_) if token.is_cancelled() => Err(Error::Cancelled),
            other => other,
        }
    }

    /// Roll back whatever is open and close the lease (safe to call more than once).
    pub async fn close(&self) {
        if let Some(l) = &self.lease {
            let _ = l.close().await;
        }
    }
}
