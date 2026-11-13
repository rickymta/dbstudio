//! Parser for the supported subset of the MongoDB shell syntax (no JavaScript is executed).
//! The AST has no `bson` dependency; the driver converts [`ShellValue`] to `bson::Bson`.

use dbstudio_core::{DestructiveReason, Error, Result, StatementClass, TextRange};

/// Shape of a statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    /// `use <db>`.
    Use,
    /// `show dbs|collections|...`.
    Show,
    /// `db.<coll>.<method>(..)` (including `db["coll"]` and `db.getCollection("coll")`).
    Collection,
    /// `db.<method>(..)`, e.g. `db.runCommand({..})`.
    Database,
}

/// A value of the shell language: JSON plus the shell literals.
#[derive(Debug, Clone, PartialEq)]
pub enum ShellValue {
    /// `null`.
    Null,
    /// `true` / `false`.
    Bool(bool),
    /// Integer literal.
    Int(i64),
    /// Floating point literal.
    Double(f64),
    /// String literal.
    String(String),
    /// Array.
    Array(Vec<ShellValue>),
    /// Document; key order preserved.
    Object(Vec<(String, ShellValue)>),
    /// `ObjectId("hex")`.
    ObjectId(String),
    /// `ISODate("..")` / `new Date(..)`; the text is kept as written.
    Date(String),
    /// `NumberInt(..)`.
    NumberInt(i32),
    /// `NumberLong(..)`.
    NumberLong(i64),
    /// `NumberDecimal("..")`.
    NumberDecimal(String),
    /// `UUID("..")`.
    Uuid(String),
    /// `BinData(subtype, "base64")`.
    BinData {
        /// Binary subtype.
        subtype: u8,
        /// Base64 payload.
        base64: String,
    },
    /// `Timestamp(t, i)`.
    Timestamp {
        /// Seconds.
        t: u32,
        /// Increment.
        i: u32,
    },
    /// `MinKey()`.
    MinKey,
    /// `MaxKey()`.
    MaxKey,
    /// `/pattern/flags`.
    Regex {
        /// Pattern.
        pattern: String,
        /// Flags.
        flags: String,
    },
}

/// One parsed statement.
#[derive(Debug, Clone, PartialEq)]
pub struct ShellStatement {
    /// Range in the text.
    pub range: TextRange,
    /// Shape.
    pub kind: ShellKind,
    /// Collection name for [`ShellKind::Collection`].
    pub collection: Option<String>,
    /// Method (or `use` / `show` argument).
    pub method: String,
    /// Arguments of the method call.
    pub args: Vec<ShellValue>,
    /// Chained calls (`.sort(..).limit(..)`).
    pub chain: Vec<(String, Vec<ShellValue>)>,
}

/// Parse a script. Scaffold: `Unsupported`.
pub fn parse(text: &str) -> Result<Vec<ShellStatement>> {
    let _ = text;
    Err(Error::Unsupported("mongo_shell".into()))
}

/// Class and destructive reason of a statement. Scaffold: `Unknown`, not destructive.
pub fn classify(stmt: &ShellStatement) -> (StatementClass, Option<DestructiveReason>) {
    let _ = stmt;
    (StatementClass::Unknown, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_contract_parse_is_unsupported() {
        let err = parse("db.users.find({})").unwrap_err();
        assert_eq!(err.kind(), "unsupported");
    }

    #[test]
    fn stub_contract_classify_is_unknown() {
        let s = ShellStatement {
            range: TextRange { start: 0, end: 1 },
            kind: ShellKind::Collection,
            collection: Some("users".into()),
            method: "drop".into(),
            args: vec![],
            chain: vec![],
        };
        assert_eq!(classify(&s), (StatementClass::Unknown, None));
    }
}
