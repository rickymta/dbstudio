//! Mapping of client errors to [`dbstudio_core::Error`].

use dbstudio_core::Error;
use redis::{ErrorKind, RedisError};

/// Convert a client error into the matching `Error` kind.
pub fn map_err(e: RedisError) -> Error {
    let message = e.to_string();
    match e.code() {
        Some("NOPERM") => return Error::Permission(message),
        Some("WRONGPASS" | "NOAUTH") => return Error::Authentication(message),
        _ => {}
    }
    if e.kind() == ErrorKind::AuthenticationFailed {
        return Error::Authentication(message);
    }
    if e.is_timeout() {
        return Error::Timeout;
    }
    if e.is_io_error() || e.is_connection_dropped() || e.is_connection_refusal() {
        let lower = message.to_ascii_lowercase();
        if lower.contains("tls")
            || lower.contains("certificate")
            || lower.contains("handshake")
            || lower.contains("invalid peer")
        {
            return Error::Tls(message);
        }
        return Error::Connection(message);
    }
    if e.kind() == ErrorKind::InvalidClientConfig {
        return Error::Tls(message);
    }
    Error::Query {
        message,
        position: None,
    }
}
