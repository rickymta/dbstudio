//! The connection an export or import runs on: a lease (one physical connection, so a
//! transaction / snapshot spans the whole job) when the driver has leases, else the session.

use std::sync::Arc;

use dbstudio_core::{
    CancelToken, Error, ExecSummary, Lease, LeaseOptions, QueryRequest, Result, ResultSink,
    Session, TxOptions,
};
use uuid::Uuid;

pub(crate) struct Conn {
    session: Arc<dyn Session>,
    lease: Option<Box<dyn Lease>>,
}

impl std::fmt::Debug for Conn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Conn")
            .field("lease", &self.lease.is_some())
            .finish()
    }
}

impl Conn {
    /// Open a lease when the driver supports it. `tx` is started on the lease right away
    /// (`None` leaves it in auto-commit mode). Without lease support the session is used and
    /// the returned note says why there is no transaction.
    pub async fn open(
        session: Arc<dyn Session>,
        tx: Option<TxOptions>,
    ) -> Result<(Conn, Option<String>)> {
        match session.open_lease(LeaseOptions::default()).await {
            Ok(lease) => {
                if let Some(opts) = tx {
                    if let Err(e) = lease.begin(opts).await {
                        let _ = lease.close().await;
                        return Err(e);
                    }
                }
                Ok((
                    Conn {
                        session,
                        lease: Some(lease),
                    },
                    None,
                ))
            }
            Err(Error::Unsupported(_)) => Ok((
                Conn {
                    session,
                    lease: None,
                },
                Some("the driver has no dedicated connections: no consistent snapshot".into()),
            )),
            Err(e) => Err(e),
        }
    }

    pub fn session(&self) -> &Arc<dyn Session> {
        &self.session
    }

    pub fn lease(&self) -> Option<&dyn Lease> {
        self.lease.as_deref()
    }

    pub async fn execute(
        &self,
        req: QueryRequest,
        sink: &mut dyn ResultSink,
    ) -> Result<ExecSummary> {
        match &self.lease {
            Some(l) => l.execute(req, sink).await,
            None => self.session.execute(req, sink).await,
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

    /// Roll back whatever is open and close the lease.
    pub async fn close(self) {
        if let Some(l) = self.lease {
            let _ = l.close().await;
        }
    }
}
