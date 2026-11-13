//! Dialect aware tokenizer: strings (`'..'`, `E'..'`, `N'..'`, `q'[..]'`), `$tag$` quotes,
//! quoted identifiers, comments (nested for PostgreSQL), `DELIMITER`, `GO` and `/` lines.

use dbstudio_core::{ScriptDialect, TextRange};

/// Lexical class of a [`Token`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// Keyword or bare identifier.
    Word,
    /// Numeric literal.
    Number,
    /// String literal of any flavour.
    String,
    /// Quoted identifier (`"x"`, `` `x` ``, `[x]`).
    QuotedIdent,
    /// `-- ...` / `# ...` comment.
    LineComment,
    /// `/* ... */` comment.
    BlockComment,
    /// Operator or punctuation.
    Punct,
    /// Whitespace run.
    Whitespace,
    /// Anything else (the whole text in the scaffold).
    Other,
}

/// A token and where it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    /// Class.
    pub kind: TokenKind,
    /// Character range in the text.
    pub range: TextRange,
}

/// Tokenize `text`. Scaffold: one [`TokenKind::Other`] token covering the whole (non-empty) text.
pub fn tokenize(d: ScriptDialect, text: &str) -> Vec<Token> {
    let _ = d;
    if text.is_empty() {
        return Vec::new();
    }
    vec![Token {
        kind: TokenKind::Other,
        range: TextRange {
            start: 0,
            end: crate::char_len(text),
        },
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_contract_one_token_for_whole_text() {
        assert!(tokenize(ScriptDialect::Postgres, "").is_empty());
        let t = tokenize(ScriptDialect::Postgres, "select 'é'");
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].kind, TokenKind::Other);
        assert_eq!(t[0].range, TextRange { start: 0, end: 10 });
    }
}
