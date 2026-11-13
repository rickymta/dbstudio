//! Mapping of `tiberius` errors to [`dbstudio_core::Error`].

use dbstudio_core::Error;
use tiberius::error::Error as Tds;

/// Server error numbers that mean "login failed".
const LOGIN_FAILED: [u32; 3] = [18456, 18452, 18451];
/// Server error numbers that mean a missing privilege.
const PERMISSION: [u32; 7] = [229, 230, 262, 297, 300, 916, 15247];

/// Error of a connection attempt (login, TLS, TCP).
pub fn map_connect(e: Tds) -> Error {
    match e {
        Tds::Server(s) if LOGIN_FAILED.contains(&s.code()) => {
            Error::Authentication(s.message().to_string())
        }
        Tds::Server(s) => Error::Connection(s.message().to_string()),
        Tds::Tls(m) => Error::Tls(m),
        Tds::Io { kind, message } => {
            if kind == std::io::ErrorKind::TimedOut {
                Error::Timeout
            } else {
                // A failed TLS handshake surfaces as an I/O error mentioning the certificate.
                let lower = message.to_ascii_lowercase();
                if lower.contains("certificate") || lower.contains("tls") || lower.contains("handshake")
                {
                    Error::Tls(message)
                } else {
                    Error::Connection(message)
                }
            }
        }
        Tds::Routing { host, port } => Error::Connection(format!(
            "the server redirected the connection to {host}:{port}, which is not supported"
        )),
        other => Error::Connection(other.to_string()),
    }
}

/// Error of a statement. `position` is the character offset computed by the caller (or `None`).
pub fn map_query(e: Tds, position: Option<u32>) -> Error {
    match e {
        Tds::Server(s) => {
            if PERMISSION.contains(&s.code()) {
                return Error::Permission(s.message().to_string());
            }
            let position = if s.procedure().is_empty() { position } else { None };
            Error::Query {
                message: s.message().to_string(),
                position,
            }
        }
        Tds::Io { kind, message } => {
            if kind == std::io::ErrorKind::TimedOut {
                Error::Timeout
            } else {
                Error::Connection(message)
            }
        }
        Tds::Tls(m) => Error::Tls(m),
        Tds::Protocol(m) => Error::Connection(format!("protocol error: {m}")),
        other => Error::Other(other.to_string()),
    }
}

/// Server line number (1-based) of an error, when it refers to the sent batch.
pub fn error_line(e: &Tds) -> Option<u32> {
    match e {
        Tds::Server(s) if s.procedure().is_empty() && s.line() > 0 => Some(s.line()),
        _ => None,
    }
}

/// True when the error means the connection can no longer be used.
pub fn is_fatal(e: &Tds) -> bool {
    matches!(e, Tds::Io { .. } | Tds::Protocol(_) | Tds::Tls(_) | Tds::Encoding(_))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_timeout_is_timeout() {
        let e = map_connect(Tds::Io {
            kind: std::io::ErrorKind::TimedOut,
            message: "x".into(),
        });
        assert_eq!(e.kind(), "timeout");
        let e = map_connect(Tds::Io {
            kind: std::io::ErrorKind::ConnectionRefused,
            message: "refused".into(),
        });
        assert_eq!(e.kind(), "connection");
        let e = map_connect(Tds::Io {
            kind: std::io::ErrorKind::InvalidData,
            message: "invalid peer certificate: UnknownIssuer".into(),
        });
        assert_eq!(e.kind(), "tls");
    }

    #[test]
    fn protocol_error_is_connection_problem() {
        assert!(is_fatal(&Tds::Protocol("x".into())));
        assert_eq!(map_query(Tds::Protocol("x".into()), None).kind(), "connection");
        assert!(!is_fatal(&Tds::Utf8));
    }
}
