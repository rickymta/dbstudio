use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};

use crate::script::ConfirmationRequest;

/// Convenient result alias.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Classified error returned by drivers and sessions.
///
/// Serializes as `{ "kind": "...", "message": "...", ... }` for the frontend.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Network or protocol problem while connecting or talking to the server.
    #[error("connection error: {0}")]
    Connection(String),
    /// Credentials were rejected.
    #[error("authentication failed: {0}")]
    Authentication(String),
    /// The server rejected or failed a statement.
    #[error("{message}")]
    Query {
        /// Server supplied message.
        message: String,
        /// Zero-based character offset in the statement where the problem was detected.
        position: Option<u32>,
    },
    /// The requested operation needs a capability the driver does not have.
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// The operation was cancelled by the user.
    #[error("operation cancelled")]
    Cancelled,
    /// The operation exceeded its time limit.
    #[error("operation timed out")]
    Timeout,
    /// TLS setup or handshake failure (bad certificate files, verification failure...).
    #[error("tls error: {0}")]
    Tls(String),
    /// A referenced object (connection, session, table...) does not exist.
    #[error("not found: {0}")]
    NotFound(String),
    /// Failure of local persistent storage (settings database, keychain...).
    #[error("storage error: {0}")]
    Storage(String),
    /// Local I/O failure.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    /// Anything that does not fit another category.
    #[error("{0}")]
    Other(String),
    /// Statement rejected because the connection is read-only (CONN-10).
    #[error("read-only connection: {message}")]
    ReadOnlyViolation {
        /// Explanation.
        message: String,
        /// Character offset of the offending statement in the text sent.
        position: Option<u32>,
    },
    /// Destructive statements on a production connection need confirmation (CONN-11).
    #[error("confirmation required")]
    ConfirmationRequired(Box<ConfirmationRequest>),
    /// The lease is already running a statement.
    #[error("busy: {0}")]
    Busy(String),
    /// Server denied a privilege (e.g. Oracle V$ views for actual plans).
    #[error("permission denied: {0}")]
    Permission(String),
    /// Driver cannot run on this machine (Oracle Instant Client missing...).
    #[error("driver unavailable: {0}")]
    DriverUnavailable(String),
    /// SSH host key unknown or changed.
    #[error("ssh host key: {host}")]
    SshHostKey {
        /// Host name as typed in the connection.
        host: String,
        /// SSH port.
        port: u16,
        /// Key algorithm (`ssh-ed25519`...).
        key_type: String,
        /// `SHA256:...` fingerprint.
        fingerprint: String,
        /// True when a different key is already trusted for this host.
        changed: bool,
    },
    /// Checksum / decryption failure of a backup file (wrong passphrase looks the same).
    #[error("integrity error: {0}")]
    Integrity(String),
    /// Change #index of a ChangeSet failed; everything was rolled back (when atomic).
    #[error("change {index} failed: {message}")]
    ChangeFailed {
        /// Zero-based index of the failing change.
        index: u32,
        /// Failure text.
        message: String,
        /// Changes applied (and kept) before the failure; 0 when rolled back.
        applied: u32,
    },
}

impl Error {
    /// Stable machine-readable category.
    pub fn kind(&self) -> &'static str {
        match self {
            Error::Connection(_) => "connection",
            Error::Authentication(_) => "authentication",
            Error::Query { .. } => "query",
            Error::Unsupported(_) => "unsupported",
            Error::Cancelled => "cancelled",
            Error::Timeout => "timeout",
            Error::Tls(_) => "tls",
            Error::NotFound(_) => "notFound",
            Error::Storage(_) => "storage",
            Error::Io(_) => "io",
            Error::Other(_) => "other",
            Error::ReadOnlyViolation { .. } => "readOnly",
            Error::ConfirmationRequired(_) => "confirmationRequired",
            Error::Busy(_) => "busy",
            Error::Permission(_) => "permission",
            Error::DriverUnavailable(_) => "driverUnavailable",
            Error::SshHostKey { .. } => "sshHostKey",
            Error::Integrity(_) => "integrity",
            Error::ChangeFailed { .. } => "changeFailed",
        }
    }

    /// Message without the English category prefix that `Display` adds ("connection error: ...").
    /// This is what is sent to the frontend, which localizes the category itself.
    pub fn message(&self) -> String {
        match self {
            Error::Connection(m)
            | Error::Authentication(m)
            | Error::Unsupported(m)
            | Error::Tls(m)
            | Error::NotFound(m)
            | Error::Storage(m)
            | Error::Other(m)
            | Error::Busy(m)
            | Error::Permission(m)
            | Error::DriverUnavailable(m)
            | Error::Integrity(m) => m.clone(),
            Error::Query { message, .. }
            | Error::ReadOnlyViolation { message, .. }
            | Error::ChangeFailed { message, .. } => message.clone(),
            Error::SshHostKey { host, .. } => host.clone(),
            Error::Io(e) => e.to_string(),
            Error::Cancelled | Error::Timeout | Error::ConfirmationRequired(_) => self.to_string(),
        }
    }

    /// Shorthand for a query error without position.
    pub fn query(message: impl Into<String>) -> Self {
        Error::Query {
            message: message.into(),
            position: None,
        }
    }
}

impl Serialize for Error {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let extra = match self {
            Error::Query {
                position: Some(_), ..
            }
            | Error::ReadOnlyViolation {
                position: Some(_), ..
            }
            | Error::Unsupported(_)
            | Error::ConfirmationRequired(_) => 1,
            Error::ChangeFailed { .. } => 2,
            Error::SshHostKey { .. } => 5,
            _ => 0,
        };
        let mut s = serializer.serialize_struct("Error", 2 + extra)?;
        s.serialize_field("kind", self.kind())?;
        s.serialize_field("message", &self.message())?;
        match self {
            Error::Query {
                position: Some(p), ..
            }
            | Error::ReadOnlyViolation {
                position: Some(p), ..
            } => s.serialize_field("position", p)?,
            Error::Unsupported(cap) => s.serialize_field("capability", cap)?,
            Error::ConfirmationRequired(req) => s.serialize_field("confirmation", req)?,
            Error::ChangeFailed { index, applied, .. } => {
                s.serialize_field("index", index)?;
                s.serialize_field("applied", applied)?;
            }
            Error::SshHostKey {
                host,
                port,
                key_type,
                fingerprint,
                changed,
            } => {
                s.serialize_field("host", host)?;
                s.serialize_field("port", port)?;
                s.serialize_field("keyType", key_type)?;
                s.serialize_field("fingerprint", fingerprint)?;
                s.serialize_field("changed", changed)?;
            }
            _ => {}
        }
        s.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn serializes_with_kind_and_message() {
        let e = Error::Query {
            message: "syntax error".into(),
            position: Some(7),
        };
        assert_eq!(
            serde_json::to_value(&e).unwrap(),
            json!({"kind": "query", "message": "syntax error", "position": 7})
        );
        let e = Error::Unsupported("explain".into());
        assert_eq!(
            serde_json::to_value(&e).unwrap(),
            json!({"kind": "unsupported", "message": "explain", "capability": "explain"})
        );
        assert_eq!(
            serde_json::to_value(Error::Cancelled).unwrap(),
            json!({"kind": "cancelled", "message": "operation cancelled"})
        );
        assert_eq!(
            serde_json::to_value(Error::Connection("refused".into())).unwrap()["message"],
            "refused"
        );
        assert_eq!(
            Error::Connection("refused".into()).to_string(),
            "connection error: refused"
        );
        let io: Error = std::io::Error::other("boom").into();
        assert_eq!(serde_json::to_value(&io).unwrap()["kind"], "io");
        assert_eq!(
            serde_json::to_value(Error::Tls("x".into())).unwrap()["kind"],
            "tls"
        );
        assert_eq!(
            serde_json::to_value(Error::NotFound("x".into())).unwrap()["kind"],
            "notFound"
        );
        assert_eq!(
            serde_json::to_value(Error::Storage("x".into())).unwrap()["kind"],
            "storage"
        );
    }

    #[test]
    fn new_variants_serialize_per_contract() {
        use crate::script::{
            ConfirmationRequest, DestructiveReason, DestructiveStatement, TextRange,
        };
        let j = |e: Error| serde_json::to_value(&e).unwrap();
        assert_eq!(
            j(Error::ReadOnlyViolation {
                message: "writes blocked".into(),
                position: Some(4)
            }),
            json!({"kind": "readOnly", "message": "writes blocked", "position": 4})
        );
        assert_eq!(
            j(Error::ReadOnlyViolation {
                message: "x".into(),
                position: None
            }),
            json!({"kind": "readOnly", "message": "x"})
        );
        assert_eq!(
            j(Error::Busy("running".into())),
            json!({"kind": "busy", "message": "running"})
        );
        assert_eq!(
            j(Error::Permission("V$SQL".into())),
            json!({"kind": "permission", "message": "V$SQL"})
        );
        assert_eq!(
            j(Error::DriverUnavailable("no client".into())),
            json!({"kind": "driverUnavailable", "message": "no client"})
        );
        assert_eq!(
            j(Error::Integrity("bad tag".into())),
            json!({"kind": "integrity", "message": "bad tag"})
        );
        assert_eq!(
            j(Error::ChangeFailed {
                index: 2,
                message: "no row".into(),
                applied: 0
            }),
            json!({"kind": "changeFailed", "message": "no row", "index": 2, "applied": 0})
        );
        assert_eq!(
            j(Error::SshHostKey {
                host: "bastion".into(),
                port: 22,
                key_type: "ssh-ed25519".into(),
                fingerprint: "SHA256:abc".into(),
                changed: true
            }),
            json!({"kind": "sshHostKey", "message": "bastion", "host": "bastion", "port": 22,
                   "keyType": "ssh-ed25519", "fingerprint": "SHA256:abc", "changed": true})
        );
        let req = ConfirmationRequest {
            token: "0123456789abcdef0123456789abcdef".into(),
            confirm_text: "app".into(),
            statements: vec![DestructiveStatement {
                index: 0,
                range: TextRange { start: 0, end: 12 },
                text: "DROP TABLE t".into(),
                reason: DestructiveReason::Drop,
            }],
            expires_in_ms: 120_000,
        };
        let e = Error::ConfirmationRequired(Box::new(req));
        assert_eq!(e.kind(), "confirmationRequired");
        assert_eq!(
            j(e),
            json!({"kind": "confirmationRequired", "message": "confirmation required",
                   "confirmation": {"token": "0123456789abcdef0123456789abcdef", "confirmText": "app",
                     "statements": [{"index": 0, "range": {"start": 0, "end": 12},
                                     "text": "DROP TABLE t", "reason": "drop"}],
                     "expiresInMs": 120000}})
        );
    }
}
