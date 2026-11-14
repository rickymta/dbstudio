//! Mapping of `clickhouse` client errors and server exception codes onto
//! [`dbstudio_core::Error`].

use dbstudio_core::Error;

/// Server exception codes that matter for classification.
const AUTH_CODES: &[u32] = &[
    516, // AUTHENTICATION_FAILED
    192, // UNKNOWN_USER
    193, // WRONG_PASSWORD
    194, // REQUIRED_PASSWORD
];
const ACCESS_DENIED: u32 = 497;
const TIMEOUT_EXCEEDED: u32 = 159;
const QUERY_WAS_CANCELLED: u32 = 394;
const READONLY: u32 = 164;
const SESSION_NOT_FOUND: u32 = 372;

/// `Code: NNN.` at the start of a server error text.
pub(crate) fn exception_code(text: &str) -> Option<u32> {
    let rest = text.trim_start().strip_prefix("Code:")?;
    let digits: String = rest
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// Character position the server reports (`at position 12`), when it does.
fn error_position(text: &str) -> Option<u32> {
    let i = text.find("position ")? + "position ".len();
    let digits: String = text[i..].chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

fn clean_message(text: &str) -> String {
    let t = text.trim();
    // Drop the trailing ` (version 26.3.1.1 (official build))` noise.
    match t.rfind(" (version ") {
        Some(i) if t.ends_with(')') => t[..i].to_string(),
        _ => t.to_string(),
    }
}

/// Classify a server error text.
pub(crate) fn from_server_text(text: &str) -> Error {
    let message = clean_message(text);
    match exception_code(text) {
        Some(c) if AUTH_CODES.contains(&c) => Error::Authentication(message),
        Some(ACCESS_DENIED) => Error::Permission(message),
        Some(SESSION_NOT_FOUND) => Error::Connection(
            "the ClickHouse session expired; close this editor connection and open a new one"
                .into(),
        ),
        Some(TIMEOUT_EXCEEDED) => Error::Timeout,
        Some(QUERY_WAS_CANCELLED) => Error::Cancelled,
        Some(READONLY) => Error::ReadOnlyViolation {
            message,
            position: None,
        },
        _ => Error::Query {
            position: error_position(text),
            message,
        },
    }
}

fn chain_has<T: std::error::Error + 'static>(e: &(dyn std::error::Error + 'static)) -> bool {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(err) = cur {
        if err.downcast_ref::<T>().is_some() {
            return true;
        }
        cur = err.source();
    }
    false
}

fn chain_text(e: &(dyn std::error::Error + 'static)) -> String {
    let mut parts = vec![e.to_string()];
    let mut cur = e.source();
    while let Some(err) = cur {
        parts.push(err.to_string());
        cur = err.source();
    }
    parts.join(": ")
}

/// Map a client error (connection, TLS, protocol or server exception).
pub(crate) fn map_client_error(e: clickhouse::error::Error) -> Error {
    use clickhouse::error::Error as E;
    match &e {
        E::BadResponse(text) => from_server_text(text),
        E::TimedOut => Error::Timeout,
        E::Network(inner) => {
            let text = chain_text(inner.as_ref());
            if chain_has::<rustls::Error>(inner.as_ref()) || looks_like_tls(&text) {
                Error::Tls(text)
            } else {
                Error::Connection(text)
            }
        }
        E::InvalidParams(inner) => Error::Invalid(inner.to_string()),
        E::Unsupported(m) => Error::Unsupported(m.clone()),
        other => Error::Other(chain_text(other)),
    }
}

fn looks_like_tls(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    [
        "certificate",
        "handshake",
        "unknownissuer",
        "notvalidforname",
        "tls",
    ]
    .iter()
    .any(|k| m.contains(k))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_codes() {
        assert!(matches!(
            from_server_text(
                "Code: 516. DB::Exception: dbstudio: Authentication failed (AUTHENTICATION_FAILED)"
            ),
            Error::Authentication(_)
        ));
        assert!(matches!(
            from_server_text("Code: 497. DB::Exception: no grant"),
            Error::Permission(_)
        ));
        assert!(matches!(
            from_server_text("Code: 159. DB::Exception: Timeout exceeded"),
            Error::Timeout
        ));
        assert!(matches!(
            from_server_text("Code: 394. DB::Exception: cancelled"),
            Error::Cancelled
        ));
        assert!(matches!(
            from_server_text("Code: 164. DB::Exception: Cannot execute query in readonly mode"),
            Error::ReadOnlyViolation { .. }
        ));
        match from_server_text(
            "Code: 62. DB::Exception: Syntax error: failed at position 8 ('x'). (SYNTAX_ERROR) (version 26.3.1.1 (official build))",
        ) {
            Error::Query { message, position } => {
                assert_eq!(position, Some(8));
                assert!(!message.contains("(version"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn code_parsing() {
        assert_eq!(exception_code("Code: 60. DB::Exception"), Some(60));
        assert_eq!(exception_code("whatever"), None);
    }
}
