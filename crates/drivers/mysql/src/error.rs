//! Mapping of `mysql_async` errors onto the classified [`dbstudio_core::Error`].

use dbstudio_core::Error;
use mysql_async::{DriverError, Error as MyError, IoError};

/// Server error codes meaning "credentials / privileges rejected".
const AUTH_CODES: &[u16] = &[
    1044, // ER_DBACCESS_DENIED_ERROR
    1045, // ER_ACCESS_DENIED_ERROR
    1698, // ER_ACCESS_DENIED_NO_PASSWORD_ERROR
    1251, // ER_NOT_SUPPORTED_AUTH_MODE
    1396, // ER_CANNOT_USER
];

/// True when the chain of errors contains something that came from TLS.
fn is_tls_io(e: &std::io::Error) -> bool {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = e.get_ref().map(|x| x as _);
    while let Some(err) = cur {
        if err.downcast_ref::<rustls::Error>().is_some() {
            return true;
        }
        cur = err.source();
    }
    false
}

fn looks_like_tls(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    [
        "certificate",
        "tls",
        "handshake",
        "unknownissuer",
        "notvalidforname",
        "peer",
    ]
    .iter()
    .any(|k| m.contains(k))
}

fn scrub(mut msg: String, secret: Option<&str>) -> String {
    if let Some(s) = secret.filter(|s| s.len() >= 3) {
        msg = msg.replace(s, "***");
    }
    msg
}

/// Map an error that happened while establishing a connection.
pub fn map_connect_error(e: MyError, secret: Option<&str>) -> Error {
    let out = match &e {
        MyError::Server(s) if AUTH_CODES.contains(&s.code) => {
            Error::Authentication(format!("{} (code {})", s.message, s.code))
        }
        MyError::Server(s) => Error::Connection(format!("{} (code {})", s.message, s.code)),
        MyError::Io(IoError::Tls(t)) => Error::Tls(t.to_string()),
        MyError::Io(IoError::Io(io)) if is_tls_io(io) => Error::Tls(io.to_string()),
        MyError::Io(IoError::Io(io)) if io.kind() == std::io::ErrorKind::TimedOut => Error::Timeout,
        MyError::Io(IoError::Io(io)) => Error::Connection(io.to_string()),
        MyError::Driver(DriverError::NoClientSslFlagFromServer) => {
            Error::Tls("the server does not support TLS".into())
        }
        MyError::Driver(DriverError::ConnectionClosed) => {
            Error::Connection("the server closed the connection".into())
        }
        MyError::Driver(DriverError::UnknownAuthPlugin { name }) => Error::Authentication(format!(
            "the server asked for the unsupported authentication plugin `{name}` \
             (this is also what MySQL answers for unknown users)"
        )),
        MyError::Driver(DriverError::NoKeyFound) => {
            Error::Tls("no private key found in the client key file".into())
        }
        other => {
            let msg = other.to_string();
            if looks_like_tls(&msg) {
                Error::Tls(msg)
            } else {
                Error::Connection(msg)
            }
        }
    };
    rewrap(out, secret)
}

/// Map an error that happened while running a statement or metadata query.
pub fn map_query_error(e: MyError) -> Error {
    match e {
        MyError::Server(s) => Error::Query {
            message: format!(
                "{} (MySQL error {}, SQLSTATE {})",
                s.message, s.code, s.state
            ),
            position: None,
        },
        MyError::Io(IoError::Io(io)) if io.kind() == std::io::ErrorKind::TimedOut => Error::Timeout,
        MyError::Io(_) | MyError::Driver(DriverError::ConnectionClosed) => {
            Error::Connection(e.to_string())
        }
        other => Error::Other(other.to_string()),
    }
}

/// Server error code carried by `e`, if it is a server error.
pub fn server_code(e: &MyError) -> Option<u16> {
    match e {
        MyError::Server(s) => Some(s.code),
        _ => None,
    }
}

fn rewrap(e: Error, secret: Option<&str>) -> Error {
    match e {
        Error::Connection(m) => Error::Connection(scrub(m, secret)),
        Error::Authentication(m) => Error::Authentication(scrub(m, secret)),
        Error::Tls(m) => Error::Tls(scrub(m, secret)),
        other => other,
    }
}

/// True when a failed connect attempt with TLS should fall back to plain text (`Prefer`).
pub fn is_tls_unavailable(e: &MyError) -> bool {
    match e {
        MyError::Io(IoError::Tls(_)) => true,
        MyError::Io(IoError::Io(io)) => is_tls_io(io),
        MyError::Driver(DriverError::NoClientSslFlagFromServer) => true,
        MyError::Other(_) => looks_like_tls(&e.to_string()),
        _ => false,
    }
}

/// True when TiDB refused a statement because it only has a noop implementation of it (ERROR
/// 1235, e.g. `SET SESSION TRANSACTION READ ONLY`).
pub fn is_tidb_noop_error(e: &MyError) -> bool {
    e.to_string().contains("only noop implementation in tidb")
}

#[cfg(test)]
mod tests {
    use super::*;
    use mysql_async::ServerError;

    fn server(code: u16) -> MyError {
        MyError::Server(ServerError {
            code,
            message: "msg".into(),
            state: "28000".into(),
        })
    }

    #[test]
    fn auth_errors() {
        for c in [1045, 1044, 1698] {
            assert!(matches!(
                map_connect_error(server(c), None),
                Error::Authentication(_)
            ));
        }
        assert!(matches!(
            map_connect_error(server(1049), None),
            Error::Connection(_)
        ));
    }

    #[test]
    fn query_errors_carry_code() {
        match map_query_error(server(1064)) {
            Error::Query { message, .. } => assert!(message.contains("1064")),
            e => panic!("unexpected {e:?}"),
        }
    }

    #[test]
    fn io_and_tls() {
        let refused = MyError::Io(IoError::Io(std::io::Error::from(
            std::io::ErrorKind::ConnectionRefused,
        )));
        assert!(matches!(
            map_connect_error(refused, None),
            Error::Connection(_)
        ));
        let tls_io = std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            rustls::Error::General("boom".into()),
        );
        let e = MyError::Io(IoError::Io(tls_io));
        assert!(is_tls_unavailable(&e));
        assert!(matches!(map_connect_error(e, None), Error::Tls(_)));
        assert!(matches!(
            map_connect_error(
                MyError::Driver(DriverError::NoClientSslFlagFromServer),
                None
            ),
            Error::Tls(_)
        ));
        let timeout = MyError::Io(IoError::Io(std::io::Error::from(
            std::io::ErrorKind::TimedOut,
        )));
        assert!(matches!(map_connect_error(timeout, None), Error::Timeout));
    }

    #[test]
    fn tidb_noop_error_is_recognised() {
        let e = MyError::Server(ServerError {
            code: 1235,
            message: "function READ ONLY has only noop implementation in tidb now, use \
                      tidb_enable_noop_functions to enable these functions"
                .into(),
            state: "42000".into(),
        });
        assert!(is_tidb_noop_error(&e));
        assert!(!is_tidb_noop_error(&server(1045)));
    }

    #[test]
    fn secret_is_scrubbed() {
        let e = MyError::Other("bad thing hunter2 happened".into());
        match map_connect_error(e, Some("hunter2")) {
            Error::Connection(m) => assert!(!m.contains("hunter2")),
            e => panic!("unexpected {e:?}"),
        }
    }
}
