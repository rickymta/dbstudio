//! Leases: a console state (keyspace, consistency, tracing) over the keyspace session pool.
//! There are no transactions in CQL.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use dbstudio_core::lease_kit::{Canceller, LeaseBackend};
use dbstudio_core::{
    BulkInsertRequest, BulkInsertResult, Error, ExecSummary, ExplainRequest, LeaseContext,
    QueryRequest, RawPlan, Result, ResultSink, TableStreamRequest, TxControl, TxOptions, TxStatus,
};
use uuid::Uuid;

use crate::core::{Core, ExecState, Pending, first_words};

/// Cancels the request that is running on the lease.
struct LeaseCanceller {
    core: Arc<Core>,
    current: Arc<Mutex<Option<Uuid>>>,
}

#[async_trait]
impl Canceller for LeaseCanceller {
    async fn cancel(&self) -> Result<()> {
        let id = *self.current.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(id) = id {
            self.core.cancel(id);
        }
        Ok(())
    }

    async fn cancel_for(&self, query_id: Uuid) -> Result<()> {
        self.core.cancel(query_id);
        Ok(())
    }
}

pub(crate) struct CassBackend {
    core: Arc<Core>,
    state: ExecState,
    pending: Option<Pending>,
    current: Arc<Mutex<Option<Uuid>>>,
}

impl CassBackend {
    pub(crate) fn new(core: Arc<Core>, keyspace: Option<String>) -> CassBackend {
        let state = core.initial_state(keyspace);
        CassBackend {
            core,
            state,
            pending: None,
            current: Arc::new(Mutex::new(None)),
        }
    }

    fn set_current(&self, id: Option<Uuid>) {
        *self.current.lock().unwrap_or_else(|e| e.into_inner()) = id;
    }
}

#[async_trait]
impl LeaseBackend for CassBackend {
    async fn execute(
        &mut self,
        req: QueryRequest,
        sink: &mut dyn ResultSink,
        keep_open: bool,
    ) -> Result<ExecSummary> {
        self.pending = None;
        self.set_current(Some(req.query_id));
        // The lease keyspace wins over `req.database` (documented: ignored by leases).
        let mut req = req;
        req.database = None;
        let mut pending: Option<Pending> = None;
        let result = self
            .core
            .run_script(
                &mut self.state,
                &req,
                sink,
                if keep_open { Some(&mut pending) } else { None },
            )
            .await;
        self.set_current(None);
        self.pending = pending;
        result
    }

    async fn fetch_more(
        &mut self,
        max_rows: u64,
        batch: usize,
        sink: &mut dyn ResultSink,
    ) -> Result<ExecSummary> {
        let pending = self
            .pending
            .take()
            .ok_or_else(|| Error::NotFound("pending result".into()))?;
        let (summary, next) = self
            .core
            .continue_pending(pending, max_rows, batch, sink)
            .await?;
        self.pending = next;
        Ok(summary)
    }

    async fn discard_result(&mut self) -> Result<()> {
        self.pending = None;
        Ok(())
    }

    async fn tx(&mut self, op: TxControl, _opts: TxOptions) -> Result<()> {
        let _ = op;
        Err(Error::Unsupported("transactions".into()))
    }

    async fn set_auto_commit(&mut self, on: bool) -> Result<()> {
        if on {
            Ok(())
        } else {
            Err(Error::Unsupported("manual_transactions".into()))
        }
    }

    async fn set_context(&mut self, ctx: LeaseContext) -> Result<()> {
        self.pending = None;
        if let Some(db) = ctx.database.filter(|d| !d.is_empty()) {
            // Fails when the keyspace does not exist.
            self.core.pool.session(Some(&db)).await?;
            self.state.keyspace = Some(db);
        }
        Ok(())
    }

    async fn probe_tx(&mut self) -> Result<TxStatus> {
        Ok(TxStatus::Idle)
    }

    async fn refresh_context(&mut self, executed_text: &str) -> Result<Option<LeaseContext>> {
        if executed_text.to_ascii_lowercase().contains("use")
            && first_words(executed_text, 1).first().map(String::as_str) == Some("use")
        {
            return Ok(Some(LeaseContext {
                database: self.state.keyspace.clone(),
                schema: None,
            }));
        }
        Ok(None)
    }

    async fn table_select(&mut self, req: &TableStreamRequest) -> Result<String> {
        self.core.stream_select(req).await
    }

    async fn bulk_insert(&mut self, req: BulkInsertRequest) -> Result<BulkInsertResult> {
        self.core
            .bulk(&req, self.state.consistency, self.state.serial)
            .await
    }

    async fn explain(&mut self, req: ExplainRequest) -> Result<RawPlan> {
        let session = self
            .core
            .pool
            .session(req.database.as_deref().or(self.state.keyspace.as_deref()))
            .await?;
        self.set_current(Some(req.query_id));
        let r = self.core.explain_select(&self.state, session, &req).await;
        self.set_current(None);
        r
    }

    async fn ping(&mut self) -> Result<()> {
        let session = self
            .core
            .pool
            .session(self.state.keyspace.as_deref())
            .await?;
        session
            .query_unpaged("SELECT now() FROM system.local", &[])
            .await
            .map(|_| ())
            .map_err(|e| Error::Connection(e.to_string()))
    }

    async fn close(&mut self) -> Result<bool> {
        self.pending = None;
        Ok(false)
    }

    fn canceller(&self) -> Arc<dyn Canceller> {
        Arc::new(LeaseCanceller {
            core: self.core.clone(),
            current: self.current.clone(),
        })
    }

    fn has_pending(&self) -> Option<bool> {
        Some(self.pending.is_some())
    }
}
