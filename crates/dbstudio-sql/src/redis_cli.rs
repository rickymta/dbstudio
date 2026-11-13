//! redis-cli line handling: tokenizer, command table and classification.

use dbstudio_core::{DestructiveReason, Error, Result, StatementClass};

/// Command flags (bit set in [`CommandSpec::flags`]).
pub mod flags {
    /// Reads data.
    pub const READ: u8 = 1;
    /// Writes data.
    pub const WRITE: u8 = 2;
    /// Administrative.
    pub const ADMIN: u8 = 4;
    /// Can destroy data or hurt the server.
    pub const DANGEROUS: u8 = 8;
    /// May block the connection.
    pub const BLOCKING: u8 = 16;
    /// Pub/sub.
    pub const PUBSUB: u8 = 32;
}

/// Static description of a Redis command (from the Redis 7.4 docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandSpec {
    /// Upper-case name, e.g. `FLUSHDB` (`CONFIG SET` for sub-commands).
    pub name: &'static str,
    /// Arity as in `COMMAND INFO` (negative = at least).
    pub arity: i32,
    /// Bit set of [`flags`].
    pub flags: u8,
    /// One-line summary.
    pub summary: &'static str,
}

/// Known commands. Scaffold: empty.
pub static COMMANDS: &[CommandSpec] = &[];

/// Split one line into arguments like redis-cli (`"..."` with escapes, `'...'` verbatim).
/// Scaffold: `Unsupported`.
pub fn tokenize_line(line: &str) -> Result<Vec<Vec<u8>>> {
    let _ = line;
    Err(Error::Unsupported("redis_cli".into()))
}

/// Class and destructive reason of a command line (already tokenized). Scaffold: `Unknown`.
pub fn classify(tokens: &[Vec<u8>]) -> (StatementClass, Option<DestructiveReason>) {
    let _ = tokens;
    (StatementClass::Unknown, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_contract_tokenize_is_unsupported() {
        assert_eq!(tokenize_line("GET k").unwrap_err().kind(), "unsupported");
    }

    #[test]
    fn stub_contract_table_is_empty_and_classify_unknown() {
        assert!(COMMANDS.is_empty());
        assert_eq!(
            classify(&[b"FLUSHALL".to_vec()]),
            (StatementClass::Unknown, None)
        );
    }
}
