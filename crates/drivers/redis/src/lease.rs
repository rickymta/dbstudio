//! `Lease`: one dedicated connection for a console tab (`SELECT` / `MULTI` only affect it).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use dbstudio_core::{
    Error, ExecSummary, Lease, LeaseCloseReport, LeaseContext, LeaseState, QueryRequest, Result,
    ResultSink, TxOptions, TxStatus,
};
use redis::aio::MultiplexedConnection;
use uuid::Uuid;

use crate::conn::{Shared, parse_db};
use crate::error::map_err;
use crate::exec::run_script;

pub struct RedisLease {
    shared: Arc<Shared>,
    conn: tokio::sync::Mutex<Option<MultiplexedConnection>>,
    client_id: i64,
    state: Mutex<LeaseState>,
    cancelled: AtomicBool,
}

impl RedisLease {
    pub async fn open(shared: Arc<Shared>, database: Option<&str>) -> Result<RedisLease> {
        let db = match database.filter(|d| !d.is_empty()) {
            Some(d) => parse_db(d).ok_or_else(|| Error::NotFound(format!("database {d}")))?,
            None => shared.default_db,
        };
        let mut conn = shared.dedicated(db).await?;
        let client_id: i64 = redis::cmd("CLIENT")
            .arg("ID")
            .query_async(&mut conn)
            .await
            .unwrap_or(-1);
        Ok(RedisLease {
            shared,
            conn: tokio::sync::Mutex::new(Some(conn)),
            client_id,
            state: Mutex::new(LeaseState {
                auto_commit: true,
                transaction: TxStatus::Idle,
                running: None,
                pending_result: None,
                database: Some(db.to_string()),
                schema: None,
                broken: false,
            }),
            cancelled: AtomicBool::new(false),
        })
    }

    fn with_state<T>(&self, f: impl FnOnce(&mut LeaseState) -> T) -> T {
        f(&mut self.state.lock().unwrap_or_else(|e| e.into_inner()))
    }

    fn broken_error(&self) -> Error {
        Error::Connection("the console connection was closed; open a new one".into())
    }
}

#[async_trait]
impl Lease for RedisLease {
    async fn execute(&self, req: QueryRequest, sink: &mut dyn ResultSink) -> Result<ExecSummary> {
        let query_id = req.query_id;
        let admitted = self.with_state(|s| {
            if s.broken {
                Err(self.broken_error())
            } else if s.running.is_some() {
                Err(Error::Busy("a statement is already running".into()))
            } else {
                s.running = Some(query_id);
                Ok(())
            }
        });
        admitted?;
        self.cancelled.store(false, Ordering::SeqCst);
        let result = {
            let mut guard = self.conn.lock().await;
            match guard.as_mut() {
                None => Err(self.broken_error()),
                Some(conn) => {
                    let state = &self.state;
                    let mut hook = move |name: &str, args: &[Vec<u8>]| {
                        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
                        match name {
                            "SELECT" => {
                                if let Some(n) = args
                                    .first()
                                    .and_then(|a| std::str::from_utf8(a).ok())
                                    .and_then(parse_db)
                                {
                                    s.database = Some(n.to_string());
                                }
                            }
                            "MULTI" => s.transaction = TxStatus::Active,
                            "EXEC" | "DISCARD" => s.transaction = TxStatus::Idle,
                            _ => {}
                        }
                    };
                    run_script(&self.shared, conn, &req, sink, Some(&mut hook)).await
                }
            }
        };
        let cancelled = self.cancelled.load(Ordering::SeqCst);
        self.with_state(|s| {
            s.running = None;
            // A cancelled or timed-out call killed the connection (CLIENT KILL).
            if cancelled {
                s.broken = true;
            }
        });
        match result {
            Err(_) if cancelled => Err(Error::Cancelled),
            Err(Error::Connection(m)) => {
                self.with_state(|s| s.broken = true);
                Err(Error::Connection(m))
            }
            other => other,
        }
    }

    async fn fetch_more(
        &self,
        query_id: Uuid,
        _max_rows: u64,
        _batch_size: Option<u32>,
        _sink: &mut dyn ResultSink,
    ) -> Result<ExecSummary> {
        // Replies are always materialized completely: there is never a pending result.
        Err(Error::NotFound(format!("pending result {query_id}")))
    }

    async fn discard_result(&self) -> Result<()> {
        Ok(())
    }

    async fn cancel(&self, query_id: Uuid) -> Result<()> {
        let running = self.with_state(|s| s.running == Some(query_id));
        if !running {
            return Ok(());
        }
        self.cancelled.store(true, Ordering::SeqCst);
        self.shared
            .run::<i64>(
                self.shared.default_db,
                redis::cmd("CLIENT")
                    .arg("KILL")
                    .arg("ID")
                    .arg(self.client_id),
            )
            .await
            .map(|_| ())
    }

    async fn begin(&self, _opts: TxOptions) -> Result<()> {
        Err(Error::Unsupported("manual_transactions".into()))
    }

    async fn commit(&self) -> Result<()> {
        Err(Error::Unsupported("manual_transactions".into()))
    }

    async fn rollback(&self) -> Result<()> {
        Err(Error::Unsupported("manual_transactions".into()))
    }

    async fn set_auto_commit(&self, on: bool) -> Result<()> {
        if on {
            Ok(())
        } else {
            Err(Error::Unsupported("manual_transactions".into()))
        }
    }

    async fn set_context(&self, ctx: LeaseContext) -> Result<()> {
        let Some(db) = ctx.database.as_deref().filter(|d| !d.is_empty()) else {
            return Ok(());
        };
        let n = parse_db(db).ok_or_else(|| Error::NotFound(format!("database {db}")))?;
        if self.with_state(|s| s.running.is_some()) {
            return Err(Error::Busy("a statement is running".into()));
        }
        let mut guard = self.conn.lock().await;
        let conn = guard.as_mut().ok_or_else(|| self.broken_error())?;
        redis::cmd("SELECT")
            .arg(n)
            .query_async::<()>(conn)
            .await
            .map_err(map_err)?;
        self.with_state(|s| s.database = Some(n.to_string()));
        Ok(())
    }

    fn state(&self) -> LeaseState {
        self.with_state(|s| s.clone())
    }

    async fn ping(&self) -> Result<()> {
        let mut guard = self.conn.lock().await;
        let conn = guard.as_mut().ok_or_else(|| self.broken_error())?;
        match redis::cmd("PING").query_async::<String>(conn).await {
            Ok(_) => Ok(()),
            Err(e) => {
                self.with_state(|s| s.broken = true);
                Err(map_err(e))
            }
        }
    }

    async fn close(&self) -> Result<LeaseCloseReport> {
        let mut rolled_back = false;
        let mut forced = false;
        if self.with_state(|s| s.running.is_some()) {
            // Interrupt the running call, then wait (bounded) for it to release the connection.
            self.cancelled.store(true, Ordering::SeqCst);
            let _ = self
                .shared
                .run::<i64>(
                    self.shared.default_db,
                    redis::cmd("CLIENT")
                        .arg("KILL")
                        .arg("ID")
                        .arg(self.client_id),
                )
                .await;
        }
        match tokio::time::timeout(std::time::Duration::from_secs(5), self.conn.lock()).await {
            Ok(mut guard) => {
                if let Some(conn) = guard.as_mut() {
                    if self.with_state(|s| s.transaction == TxStatus::Active) {
                        rolled_back = redis::cmd("DISCARD").query_async::<()>(conn).await.is_ok();
                    }
                }
                *guard = None;
            }
            Err(_) => forced = true,
        }
        self.with_state(|s| {
            s.broken = true;
            s.transaction = TxStatus::Idle;
        });
        Ok(LeaseCloseReport {
            rolled_back,
            forced,
        })
    }
}
