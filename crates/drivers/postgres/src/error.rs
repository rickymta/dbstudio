//! Mapping of `tokio-postgres` errors onto [`dbstudio_core::Error`].
//!
//! Messages come from the server or the driver stack and never include the password.

use std::error::Error as StdError;

use dbstudio_core::Error;
use tokio_postgres::error::{DbError, ErrorPosition, SqlState};

/// Whether `err` (or anything in its source chain) is a TLS failure.
pub fn is_tls_error(err: &(dyn StdError + 'static)) -> bool {
    let mut cur: Option<&(dyn StdError + 'static)> = Some(err);
    while let Some(e) = cur {
        if e.downcast_ref::<rustls::Error>().is_some() {
            return true;
        }
        // `io::Error::source` skips the wrapped error itself, so look at it explicitly.
        if let Some(inner) = e
            .downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::get_ref)
        {
            if is_tls_error(inner) {
                return true;
            }
        }
        let text = e.to_string();
        if text.contains("TLS handshake") || text.contains("server does not support TLS") {
            return true;
        }
        cur = e.source();
    }
    false
}

fn format_db_message(db: &DbError) -> String {
    let mut msg = db.message().to_string();
    if let Some(detail) = db.detail() {
        msg.push_str("\nDETAIL: ");
        msg.push_str(detail);
    }
    if let Some(hint) = db.hint() {
        msg.push_str("\nHINT: ");
        msg.push_str(hint);
    }
    msg
}

/// 1-based character position reported by the server, as a 0-based offset.
pub fn db_position(db: &DbError) -> Option<u32> {
    match db.position() {
        Some(ErrorPosition::Original(p)) => p.checked_sub(1),
        _ => None,
    }
}

/// Error for a statement failure: server errors become [`Error::Query`] (with the position
/// shifted by `offset`, the statement's character offset in the script); a server-side cancel
/// requested through [`dbstudio_core::Session::cancel`] becomes [`Error::Cancelled`].
pub fn map_statement_error(err: tokio_postgres::Error, offset: usize, cancelled: bool) -> Error {
    if let Some(db) = err.as_db_error() {
        if *db.code() == SqlState::QUERY_CANCELED && cancelled {
            return Error::Cancelled;
        }
        return Error::Query {
            message: format_db_message(db),
            position: db_position(db).map(|p| p.saturating_add(offset as u32)),
        };
    }
    map_transport_error(err)
}

/// Error for a failure outside a user statement (catalog queries etc.).
pub fn map_error(err: tokio_postgres::Error) -> Error {
    map_statement_error(err, 0, false)
}

fn map_transport_error(err: tokio_postgres::Error) -> Error {
    if is_tls_error(&err) {
        return Error::Tls(error_chain(&err));
    }
    if err.is_closed() {
        return Error::Connection("connection closed".to_string());
    }
    Error::Other(error_chain(&err))
}

/// Error for a failure while establishing a connection.
pub fn map_connect_error(err: tokio_postgres::Error) -> Error {
    if let Some(db) = err.as_db_error() {
        let code = db.code().code();
        let message = format_db_message(db);
        return if code.starts_with("28") {
            Error::Authentication(message)
        } else if code.starts_with("08")
            || code.starts_with("53")
            || *db.code() == SqlState::INVALID_CATALOG_NAME
            || *db.code() == SqlState::CANNOT_CONNECT_NOW
        {
            Error::Connection(message)
        } else {
            Error::Query {
                message,
                position: None,
            }
        };
    }
    if is_tls_error(&err) {
        return Error::Tls(error_chain(&err));
    }
    let chain = error_chain(&err);
    if chain.contains("password missing") {
        return Error::Authentication(
            "the server requested a password but none was provided".to_string(),
        );
    }
    Error::Connection(chain)
}

/// `top: cause: cause` rendering of an error chain.
fn error_chain(err: &(dyn StdError + 'static)) -> String {
    let mut out = err.to_string();
    let mut cur = err.source();
    while let Some(e) = cur {
        let s = e.to_string();
        if !out.ends_with(&s) {
            out.push_str(": ");
            out.push_str(&s);
        }
        cur = e.source();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rustls_errors_are_tls() {
        let e = rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer);
        assert!(is_tls_error(&e));
        let io = std::io::Error::new(std::io::ErrorKind::InvalidData, e);
        assert!(is_tls_error(&io));
        let plain = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused");
        assert!(!is_tls_error(&plain));
    }

    #[test]
    fn chain_rendering_has_no_duplicates() {
        let inner = std::io::Error::other("boom");
        let rendered = error_chain(&inner);
        assert_eq!(rendered, "boom");
    }
}
