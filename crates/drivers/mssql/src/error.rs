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
                if lower.contains("certificate")
                    || lower.contains("tls")
                    || lower.contains("handshake")
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

/// Text of a `tiberius` connect error that can come from a failed TLS handshake.
fn tls_failure_text(e: &Tds) -> Option<&str> {
    match e {
        Tds::Io { kind, message } if *kind != std::io::ErrorKind::TimedOut => Some(message),
        Tds::Tls(m) => Some(m),
        _ => None,
    }
}

/// True when a failed connect attempt looks like a TLS handshake that the server's TLS stack
/// refused (so a different TLS library may succeed): the server answered with a
/// `protocol_version` / `handshake_failure` alert, offered a protocol version or cipher suite the
/// library does not support, or dropped the connection mid-handshake.
///
/// rustls reports these as `tiberius::Error::Io` whose message is the rendered `io::Error`
/// (`received fatal alert: ProtocolVersion`, `received fatal alert: HandshakeFailure`,
/// `peer is incompatible: ...`, `peer misbehaved: ...`, `tls handshake eof`, `received corrupt
/// message`); the native backend reports `tiberius::Error::Tls`. Certificate problems never
/// qualify: another TLS library would reject the same certificate.
pub fn is_handshake_failure(e: &Tds) -> bool {
    const MARKERS: [&str; 12] = [
        "alert",
        "handshake",
        "protocolversion",
        "protocol version",
        "incompatible",
        "misbehaved",
        "cipher",
        "eof",
        "corrupt message",
        "invalid message",
        "connection reset",
        "tls",
    ];
    let Some(text) = tls_failure_text(e) else {
        return false;
    };
    let lower = text.to_ascii_lowercase();
    if lower.contains("certificate") {
        return false;
    }
    // The native backend reports every handshake problem as `Tds::Tls` with the OS library's
    // wording (`connection closed via error`, ...), so no marker is needed for it.
    matches!(e, Tds::Tls(_)) || MARKERS.iter().any(|m| lower.contains(m))
}

/// The error for a TLS handshake that failed and cannot be retried: names the cause and the
/// ways out. `suggest_native` adds the `tls_backend=native` option (Windows/macOS, not yet
/// used); `native_cause` is the failure of the native backend when it was tried as well.
pub fn handshake_error(cause: &str, suggest_native: bool, native_cause: Option<&str>) -> Error {
    let also = native_cause
        .map(|c| format!("; the operating system TLS library failed as well: {c}"))
        .unwrap_or_default();
    let native = if suggest_native {
        ", set option tls_backend=native (uses the operating system TLS library)"
    } else {
        ""
    };
    Error::Tls(format!(
        "TLS handshake with the server failed: {cause}{also}. SQL Server 2008/2008 R2 and servers \
         on Windows Server 2012 R2 or older only offer TLS 1.0/1.1 or legacy cipher suites. \
         Install the TLS 1.2 update (SQL Server 2008 R2 SP3, build 10.50.6542 or later){native}, \
         or set option encrypt=none (the login is then sent unencrypted)."
    ))
}

/// The cause text of a handshake failure (see [`is_handshake_failure`]).
pub fn handshake_cause(e: &Tds) -> String {
    tls_failure_text(e).unwrap_or_default().to_string()
}

/// Error of a statement. `position` is the character offset computed by the caller (or `None`).
pub fn map_query(e: Tds, position: Option<u32>) -> Error {
    match e {
        Tds::Server(s) => {
            if PERMISSION.contains(&s.code()) {
                return Error::Permission(s.message().to_string());
            }
            let position = if s.procedure().is_empty() {
                position
            } else {
                None
            };
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
    matches!(
        e,
        Tds::Io { .. } | Tds::Protocol(_) | Tds::Tls(_) | Tds::Encoding(_)
    )
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
    fn handshake_failures_are_classified() {
        let io = |m: &str| Tds::Io {
            kind: std::io::ErrorKind::InvalidData,
            message: m.into(),
        };
        for m in [
            "received fatal alert: ProtocolVersion",
            "received fatal alert: HandshakeFailure",
            "peer is incompatible: ServerTlsVersionIsDisabledByOurConfig",
            "peer misbehaved: SelectedUnofferedCipherSuite",
            "tls handshake eof",
            "received corrupt message of type InvalidContentType",
        ] {
            assert!(is_handshake_failure(&io(m)), "{m}");
        }
        assert!(is_handshake_failure(&Tds::Tls("no shared cipher".into())));
        assert!(is_handshake_failure(&Tds::Tls(
            "connection closed via error".into()
        )));
        // Certificate problems, refused connections, timeouts and logins are not handshake issues.
        assert!(!is_handshake_failure(&io(
            "invalid peer certificate: UnknownIssuer"
        )));
        assert!(!is_handshake_failure(&Tds::Tls(
            "the server's certificate was rejected during the TLS handshake: x".into()
        )));
        assert!(!is_handshake_failure(&io(
            "Connection refused (os error 111)"
        )));
        assert!(!is_handshake_failure(&Tds::Io {
            kind: std::io::ErrorKind::TimedOut,
            message: "tls handshake timed out".into()
        }));
        assert!(!is_handshake_failure(&Tds::Protocol("x".into())));
    }

    #[test]
    fn handshake_error_message_names_the_remedies() {
        let m = handshake_error("received fatal alert: ProtocolVersion", true, None).to_string();
        assert!(m.contains("ProtocolVersion") && m.contains("tls_backend=native"));
        assert!(m.contains("10.50.6542") && m.contains("encrypt=none"));
        let m = handshake_error("x", false, Some("y")).to_string();
        assert!(!m.contains("tls_backend=native") && m.contains("failed as well: y"));
        assert_eq!(handshake_error("x", false, None).kind(), "tls");
    }

    #[test]
    fn protocol_error_is_connection_problem() {
        assert!(is_fatal(&Tds::Protocol("x".into())));
        assert_eq!(
            map_query(Tds::Protocol("x".into()), None).kind(),
            "connection"
        );
        assert!(!is_fatal(&Tds::Utf8));
    }
}
