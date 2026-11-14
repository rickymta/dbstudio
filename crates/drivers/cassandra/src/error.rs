//! Mapping of driver errors to `dbstudio_core::Error`.

use dbstudio_core::Error;
use scylla::errors::{DbError, ExecutionError, NewSessionError, PrepareError, RequestAttemptError};

/// `line L:C` of a CQL syntax error to a zero-based character offset into `text`.
pub(crate) fn position_from_message(message: &str, text: &str) -> Option<u32> {
    let idx = message.find("line ")?;
    let rest = &message[idx + 5..];
    let (line, rest) = rest.split_once(':')?;
    let line: usize = line.trim().parse().ok()?;
    let col: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let col: usize = col.parse().ok()?;
    if line == 0 {
        return None;
    }
    let mut offset = 0usize;
    for (i, l) in text.split('\n').enumerate() {
        if i + 1 == line {
            let len = l.chars().count();
            return u32::try_from(offset + col.min(len)).ok();
        }
        offset += l.chars().count() + 1;
    }
    None
}

fn looks_like_tls(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    [
        "certificate",
        "tls",
        "handshake",
        "invalid peer",
        "unknownissuer",
        "notvalidforname",
        "received fatal alert",
        "ssl",
    ]
    .iter()
    .any(|k| m.contains(k))
}

fn looks_like_auth(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("bad credentials")
        || m.contains("authenticationerror")
        || m.contains("authentication")
        || m.contains("username and/or password")
}

fn from_attempt(e: &RequestAttemptError, text: &str) -> Error {
    match e {
        RequestAttemptError::DbError(db, msg) => match db {
            DbError::AuthenticationError => Error::Authentication(msg.clone()),
            DbError::Unauthorized => Error::Permission(msg.clone()),
            DbError::ReadTimeout { .. } | DbError::WriteTimeout { .. } => Error::Query {
                message: format!("{db}: {msg}"),
                position: None,
            },
            _ => Error::Query {
                message: msg.clone(),
                position: position_from_message(msg, text),
            },
        },
        other => {
            let msg = other.to_string();
            if looks_like_tls(&msg) {
                Error::Tls(msg)
            } else {
                Error::Connection(msg)
            }
        }
    }
}

/// Map a request failure. `text` is the statement (for the error position).
pub(crate) fn from_execution(e: &ExecutionError, text: &str) -> Error {
    match e {
        ExecutionError::LastAttemptError(a) => from_attempt(a, text),
        ExecutionError::RequestTimeout(_) => Error::Timeout,
        ExecutionError::PrepareError(PrepareError::AllAttemptsFailed { first_attempt }) => {
            from_attempt(first_attempt, text)
        }
        ExecutionError::BadQuery(b) => Error::Invalid(b.to_string()),
        ExecutionError::ConnectionPoolError(_) | ExecutionError::EmptyPlan => {
            Error::Connection(e.to_string())
        }
        other => Error::Query {
            message: other.to_string(),
            position: None,
        },
    }
}

/// Map a failure to open a session.
pub(crate) fn from_new_session(e: &NewSessionError) -> Error {
    let msg = e.to_string();
    // The root cause is often only in the Debug form of the nested errors.
    let detail = format!("{e:?}");
    if looks_like_auth(&detail) {
        Error::Authentication(msg)
    } else if looks_like_tls(&detail) {
        Error::Tls(msg)
    } else {
        match e {
            NewSessionError::FailedToResolveAnyHostname(_)
            | NewSessionError::EmptyKnownNodesList => Error::Connection(msg),
            NewSessionError::UseKeyspaceError(_) => Error::Query {
                message: msg,
                position: None,
            },
            NewSessionError::IllegalConfig(_) => Error::Invalid(msg),
            _ => Error::Connection(msg),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_of_syntax_errors() {
        let msg = "line 1:7 no viable alternative at input 'FROMM'";
        assert_eq!(position_from_message(msg, "SELECT FROMM t"), Some(7));
        let msg = "SyntaxException: line 2:3 mismatched input";
        assert_eq!(position_from_message(msg, "SELECT 1;\nSELEC 2"), Some(13));
        assert_eq!(position_from_message("nothing here", "x"), None);
        assert_eq!(position_from_message("line 9:1 x", "SELECT"), None);
    }

    #[test]
    fn classifies_text() {
        assert!(looks_like_tls("invalid peer certificate: UnknownIssuer"));
        assert!(!looks_like_tls("connection refused"));
        assert!(looks_like_auth("Bad credentials"));
    }
}
