use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};

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
            | Error::Unsupported(_) => 1,
            _ => 0,
        };
        let mut s = serializer.serialize_struct("Error", 2 + extra)?;
        s.serialize_field("kind", self.kind())?;
        s.serialize_field("message", &self.to_string())?;
        match self {
            Error::Query {
                position: Some(p), ..
            } => s.serialize_field("position", p)?,
            Error::Unsupported(cap) => s.serialize_field("capability", cap)?,
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
            json!({"kind": "unsupported", "message": "unsupported: explain", "capability": "explain"})
        );
        assert_eq!(
            serde_json::to_value(Error::Cancelled).unwrap(),
            json!({"kind": "cancelled", "message": "operation cancelled"})
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
}
