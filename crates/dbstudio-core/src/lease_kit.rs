//! Optional helper for driver authors: an actor-based [`Lease`] implementation.
//!
//! **Signatures only** in the contract scaffold. The implementation (a tokio task that owns the
//! connection and receives commands over `mpsc`; busy handling, `query_id` matching, cached
//! [`LeaseState`] and close ordering) belongs to work package B (milestone B1). Until then
//! [`ActorLease::spawn`] fails with `Error::Unsupported("lease_kit")` and the [`Lease`]
//! implementation answers every call the same way.

use std::marker::PhantomData;
use std::sync::Arc;

use async_trait::async_trait;
use uuid::Uuid;

use crate::{
    Error, ExecSummary, Lease, LeaseCloseReport, LeaseContext, LeaseOptions, LeaseState,
    QueryRequest, Result, ResultSink, TxControl, TxOptions, TxStatus,
};

/// What a driver implements to get a [`Lease`] from [`ActorLease`]. All methods run on the actor
/// task that owns the connection, one at a time.
#[async_trait]
pub trait LeaseBackend: Send + 'static {
    /// Run the request. With `keep_open`, a result set that hits `max_rows` stays open for
    /// [`LeaseBackend::fetch_more`].
    async fn execute(
        &mut self,
        req: QueryRequest,
        sink: &mut dyn ResultSink,
        keep_open: bool,
    ) -> Result<ExecSummary>;
    /// Continue the pending result set.
    async fn fetch_more(
        &mut self,
        max_rows: u64,
        batch: usize,
        sink: &mut dyn ResultSink,
    ) -> Result<ExecSummary>;
    /// Drop the pending result set.
    async fn discard_result(&mut self) -> Result<()>;
    /// Transaction control.
    async fn tx(&mut self, op: TxControl, opts: TxOptions) -> Result<()>;
    /// Switch auto-commit.
    async fn set_auto_commit(&mut self, on: bool) -> Result<()>;
    /// Switch database / schema.
    async fn set_context(&mut self, ctx: LeaseContext) -> Result<()>;
    /// Ask the server for the transaction state (cheap probes only).
    async fn probe_tx(&mut self) -> Result<TxStatus>;
    /// Liveness check.
    async fn ping(&mut self) -> Result<()>;
    /// Close the connection; returns whether a transaction was rolled back.
    async fn close(&mut self) -> Result<bool>;
    /// Handle usable concurrently from another task to interrupt the running call.
    fn canceller(&self) -> Arc<dyn Canceller>;
}

/// Interrupts the call currently running on a [`LeaseBackend`].
#[async_trait]
pub trait Canceller: Send + Sync {
    /// Interrupt the running statement (protocol-level cancel).
    async fn cancel(&self) -> Result<()>;
}

/// [`Lease`] implementation over a [`LeaseBackend`] (not implemented yet, see module docs).
pub struct ActorLease<B: LeaseBackend> {
    state: LeaseState,
    _backend: PhantomData<fn() -> B>,
}

impl<B: LeaseBackend> std::fmt::Debug for ActorLease<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActorLease")
            .field("state", &self.state)
            .finish()
    }
}

impl<B: LeaseBackend> ActorLease<B> {
    /// Start the actor task owning `backend`. Placeholder: always `Err(Unsupported("lease_kit"))`.
    pub fn spawn(backend: B, opts: LeaseOptions) -> Result<ActorLease<B>> {
        let _ = (backend, opts);
        Err(Error::Unsupported("lease_kit".into()))
    }
}

fn unsupported<T>() -> Result<T> {
    Err(Error::Unsupported("lease_kit".into()))
}

#[async_trait]
impl<B: LeaseBackend> Lease for ActorLease<B> {
    async fn execute(&self, req: QueryRequest, sink: &mut dyn ResultSink) -> Result<ExecSummary> {
        let _ = (req, sink);
        unsupported()
    }
    async fn fetch_more(
        &self,
        query_id: Uuid,
        max_rows: u64,
        batch_size: Option<u32>,
        sink: &mut dyn ResultSink,
    ) -> Result<ExecSummary> {
        let _ = (query_id, max_rows, batch_size, sink);
        unsupported()
    }
    async fn discard_result(&self) -> Result<()> {
        unsupported()
    }
    async fn cancel(&self, query_id: Uuid) -> Result<()> {
        let _ = query_id;
        unsupported()
    }
    async fn begin(&self, opts: TxOptions) -> Result<()> {
        let _ = opts;
        unsupported()
    }
    async fn commit(&self) -> Result<()> {
        unsupported()
    }
    async fn rollback(&self) -> Result<()> {
        unsupported()
    }
    async fn set_auto_commit(&self, on: bool) -> Result<()> {
        let _ = on;
        unsupported()
    }
    async fn set_context(&self, ctx: LeaseContext) -> Result<()> {
        let _ = ctx;
        unsupported()
    }
    fn state(&self) -> LeaseState {
        self.state.clone()
    }
    async fn ping(&self) -> Result<()> {
        unsupported()
    }
    async fn close(&self) -> Result<LeaseCloseReport> {
        unsupported()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Never;

    struct NoCancel;
    #[async_trait]
    impl Canceller for NoCancel {
        async fn cancel(&self) -> Result<()> {
            Ok(())
        }
    }

    #[async_trait]
    impl LeaseBackend for Never {
        async fn execute(
            &mut self,
            _: QueryRequest,
            _: &mut dyn ResultSink,
            _: bool,
        ) -> Result<ExecSummary> {
            Ok(ExecSummary::default())
        }
        async fn fetch_more(
            &mut self,
            _: u64,
            _: usize,
            _: &mut dyn ResultSink,
        ) -> Result<ExecSummary> {
            Ok(ExecSummary::default())
        }
        async fn discard_result(&mut self) -> Result<()> {
            Ok(())
        }
        async fn tx(&mut self, _: TxControl, _: TxOptions) -> Result<()> {
            Ok(())
        }
        async fn set_auto_commit(&mut self, _: bool) -> Result<()> {
            Ok(())
        }
        async fn set_context(&mut self, _: LeaseContext) -> Result<()> {
            Ok(())
        }
        async fn probe_tx(&mut self) -> Result<TxStatus> {
            Ok(TxStatus::Idle)
        }
        async fn ping(&mut self) -> Result<()> {
            Ok(())
        }
        async fn close(&mut self) -> Result<bool> {
            Ok(false)
        }
        fn canceller(&self) -> Arc<dyn Canceller> {
            Arc::new(NoCancel)
        }
    }

    #[tokio::test]
    async fn spawn_is_a_placeholder() {
        let err = ActorLease::spawn(Never, LeaseOptions::default()).unwrap_err();
        assert!(matches!(err, Error::Unsupported(c) if c == "lease_kit"));
        assert!(Never.canceller().cancel().await.is_ok());
    }
}
