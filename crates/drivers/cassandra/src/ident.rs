//! Identifier quoting and string literals of CQL.

const RESERVED: &[&str] = &[
    "default",
    "is",
    "materialized",
    "mbean",
    "mbeans",
    "unset",
    "add",
    "allow",
    "alter",
    "and",
    "apply",
    "asc",
    "authorize",
    "batch",
    "begin",
    "by",
    "columnfamily",
    "create",
    "delete",
    "desc",
    "describe",
    "drop",
    "entries",
    "execute",
    "from",
    "full",
    "grant",
    "if",
    "in",
    "index",
    "infinity",
    "insert",
    "into",
    "keyspace",
    "limit",
    "modify",
    "nan",
    "norecursive",
    "not",
    "null",
    "of",
    "on",
    "or",
    "order",
    "primary",
    "rename",
    "replace",
    "revoke",
    "schema",
    "select",
    "set",
    "table",
    "to",
    "token",
    "truncate",
    "unlogged",
    "update",
    "use",
    "using",
    "view",
    "where",
    "with",
];

/// Quote an identifier only when needed (case, characters or reserved word).
pub(crate) fn quote_ident(name: &str) -> String {
    let plain = !name.is_empty()
        && name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !RESERVED.contains(&name);
    if plain {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

/// `keyspace.table` with both parts quoted when needed.
pub(crate) fn qualified(keyspace: &str, name: &str) -> String {
    format!("{}.{}", quote_ident(keyspace), quote_ident(name))
}

/// Single-quoted string literal.
pub(crate) fn quote_text(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(quote_ident("users"), "users");
        assert_eq!(quote_ident("Users"), "\"Users\"");
        assert_eq!(quote_ident("select"), "\"select\"");
        for w in ["default", "is", "materialized", "mbean", "mbeans", "unset"] {
            assert_eq!(quote_ident(w), format!("\"{w}\""));
        }
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(quote_ident("1abc"), "\"1abc\"");
        assert_eq!(qualified("ks", "t t"), "ks.\"t t\"");
        assert_eq!(quote_text("it's"), "'it''s'");
    }
}
