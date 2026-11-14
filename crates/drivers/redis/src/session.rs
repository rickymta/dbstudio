//! `Session` implementation.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use dbstudio_core::{
    ApplyResult, ChangeSet, ChildrenPage, ChildrenPageRequest, ConnectionConfig, Editability,
    Error, ExecSummary, KeyReadRequest, KeyScanPage, KeyScanRequest, KeyValuePage, Lease,
    LeaseOptions, ObjectNode, ObjectPath, QueryRequest, Result, ResultSink, ScriptDialect, Secret,
    ServerInfo, Session,
};
use uuid::Uuid;

use crate::conn::{Running, Shared};
use crate::convert::reply_text;
use crate::exec::run_script;
use crate::lease::RedisLease;

/// A connected Redis / Valkey server.
#[derive(Debug)]
pub struct RedisSession {
    shared: Arc<Shared>,
}

impl RedisSession {
    /// Connect and verify the credentials.
    pub async fn connect(config: &ConnectionConfig, secret: Option<&Secret>) -> Result<Self> {
        Ok(RedisSession {
            shared: Arc::new(Shared::connect(config, secret).await?),
        })
    }
}

/// `INFO` text as `field -> value`.
fn info_fields(info: &str) -> std::collections::BTreeMap<String, String> {
    info.lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

#[async_trait]
impl Session for RedisSession {
    async fn ping(&self) -> Result<()> {
        self.shared
            .run::<String>(self.shared.default_db, &redis::cmd("PING"))
            .await
            .map(|_| ())
    }

    async fn server_info(&self) -> Result<ServerInfo> {
        let db = self.shared.default_db;
        let text: String = self
            .shared
            .run(db, redis::cmd("INFO").arg("server"))
            .await?;
        let mut fields = info_fields(&text);
        // Memory, clients and replication are optional: ACL may hide them.
        for section in ["memory", "clients", "replication", "keyspace"] {
            if let Ok(extra) = self
                .shared
                .run::<String>(db, redis::cmd("INFO").arg(section))
                .await
            {
                fields.extend(info_fields(&extra));
            }
        }
        let valkey = fields.get("valkey_version").cloned();
        let (product, version) = match valkey {
            Some(v) => ("Valkey".to_string(), v),
            None => (
                "Redis".to_string(),
                fields.get("redis_version").cloned().unwrap_or_default(),
            ),
        };
        let mut extra = std::collections::BTreeMap::new();
        for (label, field) in [
            ("mode", "redis_mode"),
            ("os", "os"),
            ("role", "role"),
            ("uptimeSeconds", "uptime_in_seconds"),
            ("connectedClients", "connected_clients"),
            ("usedMemory", "used_memory_human"),
            ("maxMemory", "maxmemory_human"),
            ("tcpPort", "tcp_port"),
        ] {
            if let Some(v) = fields.get(field) {
                extra.insert(label.to_string(), v.clone());
            }
        }
        extra.insert(
            "compatibleRedisVersion".into(),
            fields.get("redis_version").cloned().unwrap_or_default(),
        );
        Ok(ServerInfo {
            product,
            version,
            extra,
        })
    }

    async fn execute(&self, req: QueryRequest, sink: &mut dyn ResultSink) -> Result<ExecSummary> {
        let db = match req.database.as_deref().filter(|d| !d.is_empty()) {
            Some(d) => {
                crate::conn::parse_db(d).ok_or_else(|| Error::NotFound(format!("database {d}")))?
            }
            None => self.shared.default_db,
        };
        // A private connection per call: `SELECT` or `MULTI` typed by the user must not leak
        // into the shared browsing connections.
        let mut conn = self.shared.dedicated(db).await?;
        let client_id: i64 = reply_text_i64(&mut conn).await;
        let cancelled = Arc::new(AtomicBool::new(false));
        self.shared
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                req.query_id,
                Running {
                    client_id,
                    cancelled: cancelled.clone(),
                },
            );
        let result = run_script(&self.shared, &mut conn, &req, sink, None).await;
        self.shared
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&req.query_id);
        match result {
            Err(_) if cancelled.load(Ordering::SeqCst) => Err(Error::Cancelled),
            Err(Error::Timeout) => {
                // The server may still be busy with the command: kill the private connection.
                let _ = self
                    .shared
                    .run::<i64>(
                        self.shared.default_db,
                        redis::cmd("CLIENT").arg("KILL").arg("ID").arg(client_id),
                    )
                    .await;
                Err(Error::Timeout)
            }
            other => other,
        }
    }

    async fn cancel(&self, query_id: Uuid) -> Result<()> {
        let running = self
            .shared
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&query_id)
            .cloned();
        let Some(running) = running else {
            return Ok(());
        };
        running.cancelled.store(true, Ordering::SeqCst);
        self.shared
            .run::<i64>(
                self.shared.default_db,
                redis::cmd("CLIENT")
                    .arg("KILL")
                    .arg("ID")
                    .arg(running.client_id),
            )
            .await
            .map(|_| ())
    }

    async fn list_children(&self, parent: &ObjectPath) -> Result<Vec<ObjectNode>> {
        if parent.is_root() {
            return self.shared.databases().await;
        }
        self.shared.children(parent).await
    }

    async fn list_children_page(&self, req: ChildrenPageRequest) -> Result<ChildrenPage> {
        self.shared.children_page(req).await
    }

    fn script_dialect(&self) -> ScriptDialect {
        ScriptDialect::RedisCli
    }

    async fn open_lease(&self, opts: LeaseOptions) -> Result<Box<dyn Lease>> {
        Ok(Box::new(
            RedisLease::open(self.shared.clone(), opts.database.as_deref()).await?,
        ))
    }

    async fn editability(&self, _path: &ObjectPath) -> Result<Editability> {
        Ok(self.shared.editability())
    }

    async fn preview_changes(&self, changes: &ChangeSet) -> Result<Vec<String>> {
        self.shared.preview(changes)
    }

    async fn apply_changes(&self, changes: ChangeSet) -> Result<ApplyResult> {
        self.shared.apply(changes).await
    }

    async fn read_key(&self, req: KeyReadRequest) -> Result<KeyValuePage> {
        self.shared.read_key(req).await
    }

    async fn scan_keys(&self, req: KeyScanRequest) -> Result<KeyScanPage> {
        self.shared.scan_keys(req).await
    }

    async fn close(&self) -> Result<()> {
        self.shared.close().await;
        Ok(())
    }
}

async fn reply_text_i64(conn: &mut redis::aio::MultiplexedConnection) -> i64 {
    redis::cmd("CLIENT")
        .arg("ID")
        .query_async::<redis::Value>(conn)
        .await
        .ok()
        .and_then(|v| reply_text(&v).parse().ok())
        .unwrap_or(-1)
}
