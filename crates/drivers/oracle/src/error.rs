//! Mapping of `oracle::Error` to [`dbstudio_core::Error`].

use dbstudio_core::Error;

/// ORA codes that mean "the credentials were refused".
const AUTH_CODES: [i32; 7] = [1017, 1005, 1045, 28000, 28001, 28002, 28009];
/// ORA codes that mean "the connection failed or was lost".
const CONNECTION_CODES: [i32; 32] = [
    12154, 12514, 12505, 12541, 12543, 12545, 12170, 12537, 12547, 12560, 12528, 12518, 12519,
    12520, 12521, 12523, 12526, 12527, 12571, 12535, 12203, 12152, 12157, 3113, 3114, 3135, 3136,
    1012, 1033, 1034, 1089, 1090,
];
/// ORA codes of TLS / wallet problems.
const TLS_CODES: [i32; 8] = [28759, 28750, 28860, 29024, 29106, 29002, 29003, 28865];

/// Error text without the trailing documentation line Oracle 23 appends.
fn clean_message(message: &str) -> String {
    let mut lines: Vec<&str> = message.lines().collect();
    while lines
        .last()
        .is_some_and(|l| l.trim().is_empty() || l.trim_start().starts_with("Help:"))
    {
        lines.pop();
    }
    lines.join("\n")
}

/// ORA / DPI numbers carried by `e`.
fn codes(e: &oracle::Error) -> (Option<i32>, Option<i32>) {
    (e.oci_code(), e.dpi_code())
}

/// Convert an error that did not come from running a statement of the user.
pub fn map_error(e: oracle::Error) -> Error {
    map_with_position(e, None)
}

/// Convert an error of a user statement. `statement_start` is the character offset of the
/// statement in the whole script and `statement` its text, so that the position of a parse error
/// (ODPI-C reports a byte offset inside the statement) becomes an absolute character offset.
pub fn map_statement_error(e: oracle::Error, statement_start: u32, statement: &str) -> Error {
    map_with_position(e, Some((statement_start, statement)))
}

fn map_with_position(e: oracle::Error, at: Option<(u32, &str)>) -> Error {
    let (oci, dpi) = codes(&e);
    let db = e.db_error();
    let message = db
        .map(|d| clean_message(d.message()))
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| clean_message(&e.to_string()));
    if dpi == Some(1047) || dpi.is_some_and(|c| matches!(c, 1050 | 1072)) {
        return Error::DriverUnavailable(message);
    }
    match oci {
        Some(1013) => return Error::Cancelled,
        Some(3156) => return Error::Timeout,
        Some(c) if AUTH_CODES.contains(&c) => return Error::Authentication(message),
        Some(c) if TLS_CODES.contains(&c) => return Error::Tls(message),
        Some(c) if CONNECTION_CODES.contains(&c) => return Error::Connection(message),
        Some(1031) => return Error::Permission(message),
        _ => {}
    }
    match dpi {
        Some(1067) => return Error::Timeout,
        Some(1010 | 1080 | 1081 | 1032) => return Error::Connection(message),
        _ => {}
    }
    let position = at.and_then(|(start, text)| {
        let d = db?;
        let code = d.code();
        let offset = d.offset();
        (offset > 0 || (900..=999).contains(&code))
            .then(|| start + byte_to_char_offset(text, offset as usize))
    });
    Error::Query { message, position }
}

/// Character index of byte `offset` in `text` (clamped to the text length).
pub fn byte_to_char_offset(text: &str, offset: usize) -> u32 {
    let end = offset.min(text.len());
    let mut idx = end;
    while idx > 0 && !text.is_char_boundary(idx) {
        idx -= 1;
    }
    u32::try_from(text[..idx].chars().count()).unwrap_or(u32::MAX)
}

/// An error reported while a statement ran for a user, turned into `Cancelled`/`Timeout` when the
/// driver itself interrupted the call.
pub fn classify_interrupt(e: Error, cancelled: bool, timed_out: bool) -> Error {
    match e {
        Error::Cancelled if timed_out => Error::Timeout,
        Error::Query { .. } if timed_out && cancelled => Error::Timeout,
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_line_is_removed() {
        assert_eq!(
            clean_message(
                "ORA-00942: table or view does not exist\nHelp: https://docs.oracle.com/error-help/db/ora-00942/"
            ),
            "ORA-00942: table or view does not exist"
        );
        assert_eq!(clean_message("plain"), "plain");
        assert_eq!(clean_message("a\nb\n\n"), "a\nb");
    }

    #[test]
    fn byte_offsets_become_char_offsets() {
        assert_eq!(byte_to_char_offset("abc", 2), 2);
        // "é" is two bytes: byte offset 3 is after "aé".
        assert_eq!(byte_to_char_offset("aébc", 3), 2);
        // An offset inside a character rounds down.
        assert_eq!(byte_to_char_offset("aébc", 2), 1);
        assert_eq!(byte_to_char_offset("ab", 99), 2);
    }

    fn db_error(code: i32, offset: u32, message: &str) -> oracle::Error {
        #[allow(deprecated)]
        oracle::Error::OciError(oracle::DbError::new(code, offset, message, "fn", "act"))
    }

    #[test]
    fn maps_codes_to_kinds() {
        assert!(matches!(
            map_error(db_error(1017, 0, "ORA-01017: invalid username/password")),
            Error::Authentication(_)
        ));
        assert!(matches!(
            map_error(db_error(12541, 0, "ORA-12541: no listener")),
            Error::Connection(_)
        ));
        assert!(matches!(
            map_error(db_error(28759, 0, "ORA-28759: failure to open file")),
            Error::Tls(_)
        ));
        assert!(matches!(
            map_error(db_error(1013, 0, "x")),
            Error::Cancelled
        ));
        assert!(matches!(map_error(db_error(3156, 0, "x")), Error::Timeout));
        assert!(matches!(
            map_error(db_error(1031, 0, "ORA-01031: insufficient privileges")),
            Error::Permission(_)
        ));
        match map_error(db_error(1722, 0, "ORA-01722: invalid number")) {
            Error::Query { position, .. } => assert_eq!(position, None),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parse_errors_get_an_absolute_char_position() {
        // Statement "select é from nope" starts at char 10 of the script; ORA-00942 at byte 16
        // ("select é from " is 15 chars and 16 bytes).
        let text = "select é from nope";
        match map_statement_error(
            db_error(942, 16, "ORA-00942: table or view does not exist"),
            10,
            text,
        ) {
            Error::Query { position, message } => {
                assert_eq!(position, Some(10 + 15));
                assert!(message.starts_with("ORA-00942"));
            }
            other => panic!("{other:?}"),
        }
        // ORA-00900 reports offset 0 but is still a parse error: points at the statement start.
        match map_statement_error(
            db_error(900, 0, "ORA-00900: invalid SQL statement"),
            4,
            "foo",
        ) {
            Error::Query { position, .. } => assert_eq!(position, Some(4)),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn interrupts_become_timeouts() {
        assert!(matches!(
            classify_interrupt(Error::Cancelled, true, true),
            Error::Timeout
        ));
        assert!(matches!(
            classify_interrupt(Error::Cancelled, true, false),
            Error::Cancelled
        ));
    }
}
