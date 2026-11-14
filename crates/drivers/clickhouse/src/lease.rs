//! Leases: a ClickHouse HTTP session (`session_id`, `session_timeout = 3600`) keeps `SET`
//! settings and temporary tables between statements; the current database (`USE`) is tracked by
//! the driver and sent with every request. There are no transactions.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dbstudio_core::lease_kit::{ActorLease, Canceller, LeaseBackend};
use dbstudio_core::{
    BulkInsertRequest, BulkInsertResult, EndOutcome, Error, ExecSummary, ExplainRequest, Lease,
    LeaseContext, LeaseOptions, QueryRequest, RawPlan, Result, ResultSink, StatementEnd,
    TableStreamRequest, TxControl, TxOptions, TxStatus,
};
use uuid::Uuid;

use crate::conn::{Core, Req, quote_literal};
use crate::exec::{Call, Pending, Registry, StreamOpts, finish_error, run_script, stream_rows};

pub(crate) struct ChBackend {
    core: Arc<Core>,
    reg: Arc<Registry>,
    session_id: String,
    db: Option<String>,
    pending: Option<Pending>,
}

impl std::fmt::Debug for ChBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChBackend")
            .field("db", &self.db)
            .field("pending", &self.pending.is_some())
            .finish_non_exhaustive()
    }
}

struct ChCanceller {
    core: Arc<Core>,
    reg: Arc<Registry>,
}

#[async_trait]
impl Canceller for ChCanceller {
    async fn cancel(&self) -> Result<()> {
        self.reg.cancel_all(&self.core).await;
        Ok(())
    }

    async fn cancel_for(&self, query_id: Uuid) -> Result<()> {
        match self.reg.cancel(&self.core, query_id).await {
            Err(Error::NotFound(_)) => Ok(()),
            other => other,
        }
    }
}

fn elapsed_ms(t: Instant) -> u64 {
    u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX)
}

pub(crate) async fn open(core: Arc<Core>, opts: LeaseOptions) -> Result<Box<dyn Lease>> {
    if !opts.auto_commit {
        return Err(Error::Unsupported("manual_transactions".into()));
    }
    let backend = ChBackend {
        core,
        reg: Arc::new(Registry::default()),
        session_id: format!("dbstudio-{}", Uuid::new_v4()),
        db: opts.database.clone().filter(|d| !d.is_empty()),
        pending: None,
    };
    // Prove the connection and the database before handing the lease out.
    let req = Req {
        session_id: Some(backend.session_id.clone()),
        database: backend.db.clone(),
        ..Req::default()
    };
    backend.core.exec(&req, "SELECT 1 FORMAT Null").await?;
    Ok(Box::new(ActorLease::spawn(backend, opts)?))
}

impl ChBackend {
    /// Drop the open result and stop its query (the HTTP session is locked until it ends).
    async fn drop_pending(&mut self) {
        if let Some(p) = self.pending.take() {
            let qid = p.qid.clone();
            drop(p);
            let _ = self.core.kill_exact(&qid, true).await;
        }
    }

    fn req(&self) -> Req {
        Req {
            session_id: Some(self.session_id.clone()),
            session_check: true,
            database: self.db.clone(),
            ..Req::default()
        }
    }
}

#[async_trait]
impl LeaseBackend for ChBackend {
    async fn execute(
        &mut self,
        req: QueryRequest,
        sink: &mut dyn ResultSink,
        keep_open: bool,
    ) -> Result<ExecSummary> {
        self.drop_pending().await;
        let core = self.core.clone();
        let reg = self.reg.clone();
        let guard = reg.register(req.query_id);
        let session = self.session_id.clone();
        let call = Call {
            core: &core,
            ctl: &guard.ctl,
            session_id: Some(&session),
            timeout: req.timeout_ms.map(Duration::from_millis),
            started: Instant::now(),
        };
        let keep = keep_open.then_some(&mut self.pending);
        run_script(&call, &mut self.db, &req, keep, sink).await
    }

    async fn fetch_more(
        &mut self,
        max_rows: u64,
        batch: usize,
        sink: &mut dyn ResultSink,
    ) -> Result<ExecSummary> {
        let mut pending = self
            .pending
            .take()
            .ok_or_else(|| Error::NotFound("pending result".into()))?;
        let started = Instant::now();
        let core = self.core.clone();
        let reg = self.reg.clone();
        let guard = reg.register(pending.query_id);
        let session = self.session_id.clone();
        let call = Call {
            core: &core,
            ctl: &guard.ctl,
            session_id: Some(&session),
            timeout: None,
            started,
        };
        let o = StreamOpts {
            batch: batch.max(1),
            max_rows: Some(max_rows.max(1)),
        };
        let first = pending.row.take();
        let qid = pending.qid.clone();
        let streamed = stream_rows(&call, &mut pending.src, first, &qid, &o, sink)
            .await
            .map_err(|e| finish_error(&call, e))?;
        sink.on_statement_end(StatementEnd {
            outcome: EndOutcome::Rows {
                row_count: streamed.count,
                truncated: streamed.truncated,
            },
            elapsed_ms: elapsed_ms(started),
        })?;
        if streamed.truncated {
            pending.row = streamed.extra;
            self.pending = Some(pending);
        }
        Ok(ExecSummary {
            statements: 1,
            elapsed_ms: elapsed_ms(started),
        })
    }

    async fn discard_result(&mut self) -> Result<()> {
        self.drop_pending().await;
        Ok(())
    }

    async fn tx(&mut self, _op: TxControl, _opts: TxOptions) -> Result<()> {
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
        self.drop_pending().await;
        if let Some(db) = ctx.database.filter(|d| !d.is_empty()) {
            let n = self
                .core
                .rows(
                    &Req::default(),
                    &format!(
                        "SELECT count() FROM system.databases WHERE name = {}",
                        quote_literal(&db)
                    ),
                )
                .await?
                .first()
                .and_then(|r| crate::conn::cell_u64(r, 0))
                .unwrap_or(0);
            if n == 0 {
                return Err(Error::NotFound(format!("database `{db}`")));
            }
            self.db = Some(db);
        }
        Ok(())
    }

    async fn probe_tx(&mut self) -> Result<TxStatus> {
        Ok(TxStatus::Idle)
    }

    async fn refresh_context(&mut self, _executed_text: &str) -> Result<Option<LeaseContext>> {
        Ok(Some(LeaseContext {
            database: self.db.clone(),
            schema: None,
        }))
    }

    async fn table_select(&mut self, req: &TableStreamRequest) -> Result<String> {
        crate::bulk::table_select(&self.core, req).await
    }

    async fn bulk_insert(&mut self, req: BulkInsertRequest) -> Result<BulkInsertResult> {
        crate::bulk::bulk_insert(&self.core, Some(&self.session_id), req).await
    }

    async fn explain(&mut self, req: ExplainRequest) -> Result<RawPlan> {
        self.drop_pending().await;
        let mut req = req;
        if req.database.is_none() {
            req.database = self.db.clone();
        }
        crate::explain::explain(&self.core, &self.reg, Some(&self.session_id), &req).await
    }

    async fn ping(&mut self) -> Result<()> {
        self.core.exec(&self.req(), "SELECT 1 FORMAT Null").await
    }

    async fn close(&mut self) -> Result<bool> {
        self.drop_pending().await;
        self.reg.cancel_all(&self.core).await;
        Ok(false)
    }

    fn canceller(&self) -> Arc<dyn Canceller> {
        Arc::new(ChCanceller {
            core: self.core.clone(),
            reg: self.reg.clone(),
        })
    }

    fn has_pending(&self) -> Option<bool> {
        Some(self.pending.is_some())
    }
}
