//! Dialect aware tokenizer: strings (`'..'`, `E'..'`, `N'..'`, `q'[..]'`), `$tag$` quotes,
//! quoted identifiers, comments (nested for PostgreSQL and T-SQL), plus the helpers the
//! splitter uses for `DELIMITER`, `GO` and `/` lines.
//!
//! Tokens cover the text without gaps: concatenating the token ranges gives back the input.
//! Positions are character offsets. Lexical problems (unterminated string or comment) never
//! stop the scan; the offending token simply runs to the end of the text and the problem is
//! reported by [`tokenize_with_problems`].

use dbstudio_core::{ScriptDialect, ScriptProblem, TextRange};

/// Lexical class of a [`Token`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// Keyword or bare identifier (also `@var`, `@@var`, `#tmp` where the dialect has them).
    Word,
    /// Numeric literal; also a PostgreSQL positional parameter `$1`.
    Number,
    /// String literal of any flavour (dollar quoted bodies included).
    String,
    /// Quoted identifier (`"x"`, `` `x` ``, `[x]`).
    QuotedIdent,
    /// `-- ...` / `# ...` / `// ...` comment (the newline is not part of it).
    LineComment,
    /// `/* ... */` comment.
    BlockComment,
    /// Operator or punctuation. `::` and `:=` are single tokens, everything else one character.
    Punct,
    /// Whitespace run.
    Whitespace,
    /// Anything else (not produced by the current lexer; kept for forward compatibility).
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

impl TokenKind {
    /// Whitespace and comments: tokens that carry no meaning for statements.
    pub fn is_trivia(self) -> bool {
        matches!(
            self,
            TokenKind::Whitespace | TokenKind::LineComment | TokenKind::BlockComment
        )
    }
}

/// Per dialect lexical switches.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Rules {
    pg: bool,
    mysql: bool,
    mssql: bool,
    oracle: bool,
    /// `/* /* */ */` nests.
    nested_comments: bool,
    /// `"x"` is a string (MySQL, Mongo, Redis) rather than an identifier.
    dq_string: bool,
    /// Backslash escapes inside `'..'` / `".."` strings.
    backslash: bool,
    /// `#` starts a line comment.
    hash_comment: bool,
    /// `//` starts a line comment.
    slash_comment: bool,
    /// `-- ` comments exist.
    dash_comment: bool,
    /// Words may start with `@` (MySQL, T-SQL variables).
    at_words: bool,
    /// Redis and Mongo shells have no SQL quoting.
    shell: bool,
}

impl Rules {
    pub(crate) fn new(d: ScriptDialect) -> Rules {
        let mut r = Rules {
            pg: false,
            mysql: false,
            mssql: false,
            oracle: false,
            nested_comments: false,
            dq_string: false,
            backslash: false,
            hash_comment: false,
            slash_comment: false,
            dash_comment: true,
            at_words: false,
            shell: false,
        };
        match d {
            ScriptDialect::Postgres => {
                r.pg = true;
                r.nested_comments = true;
            }
            ScriptDialect::MySql {
                backslash_escapes, ..
            } => {
                r.mysql = true;
                r.dq_string = true;
                r.backslash = backslash_escapes;
                r.hash_comment = true;
                r.at_words = true;
            }
            ScriptDialect::MsSql => {
                r.mssql = true;
                r.nested_comments = true;
                r.at_words = true;
            }
            ScriptDialect::Oracle => r.oracle = true,
            ScriptDialect::MongoShell => {
                r.shell = true;
                r.dq_string = true;
                r.backslash = true;
                r.slash_comment = true;
                r.dash_comment = false;
            }
            ScriptDialect::RedisCli => {
                r.shell = true;
                r.dq_string = true;
                r.backslash = true;
                r.dash_comment = false;
            }
            // CQL and ClickHouse behave like `Opaque` until package S adds their lexers.
            ScriptDialect::Opaque | ScriptDialect::Cql | ScriptDialect::ClickHouse => {}
        }
        r
    }
}

fn is_word_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn is_word_cont(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// Character level lexer over a prepared `&[char]`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Lexer<'a> {
    pub chars: &'a [char],
    pub rules: Rules,
}

type Next = (TokenKind, usize, Option<&'static str>);

impl<'a> Lexer<'a> {
    pub(crate) fn new(chars: &'a [char], d: ScriptDialect) -> Self {
        Lexer {
            chars,
            rules: Rules::new(d),
        }
    }

    fn at(&self, i: usize) -> Option<char> {
        self.chars.get(i).copied()
    }

    /// Scan a quoted body whose opening quote is at `open`. Returns `(end, terminated)`.
    fn quoted(&self, open: usize, quote: char, backslash: bool) -> (usize, bool) {
        let n = self.chars.len();
        let mut i = open + 1;
        while i < n {
            let c = self.chars[i];
            if backslash && c == '\\' {
                i += 2;
                continue;
            }
            if c == quote {
                if self.at(i + 1) == Some(quote) {
                    i += 2;
                    continue;
                }
                return (i + 1, true);
            }
            i += 1;
        }
        (n, false)
    }

    fn string_token(&self, open: usize, backslash: bool) -> Next {
        let (end, ok) = self.quoted(open, '\'', backslash);
        (
            TokenKind::String,
            end,
            if ok {
                None
            } else {
                Some("unterminated string")
            },
        )
    }

    /// Oracle `q'[...]'` literal; `quote` is the index of the `'` after the `q`.
    fn alt_quote(&self, quote: usize) -> Option<Next> {
        let delim = self.at(quote + 1)?;
        if delim.is_whitespace() {
            return None;
        }
        let close = match delim {
            '[' => ']',
            '{' => '}',
            '(' => ')',
            '<' => '>',
            c => c,
        };
        let n = self.chars.len();
        let mut i = quote + 2;
        while i < n {
            if self.chars[i] == close && self.at(i + 1) == Some('\'') {
                return Some((TokenKind::String, i + 2, None));
            }
            i += 1;
        }
        Some((TokenKind::String, n, Some("unterminated string")))
    }

    fn block_comment(&self, i: usize) -> Next {
        let n = self.chars.len();
        let mut depth = 1usize;
        let mut j = i + 2;
        while j < n {
            if self.rules.nested_comments && self.chars[j] == '/' && self.at(j + 1) == Some('*') {
                depth += 1;
                j += 2;
            } else if self.chars[j] == '*' && self.at(j + 1) == Some('/') {
                depth -= 1;
                j += 2;
                if depth == 0 {
                    return (TokenKind::BlockComment, j, None);
                }
            } else {
                j += 1;
            }
        }
        (
            TokenKind::BlockComment,
            n,
            Some("unterminated block comment"),
        )
    }

    fn line_comment(&self, i: usize) -> Next {
        let mut j = i;
        while j < self.chars.len() && self.chars[j] != '\n' && self.chars[j] != '\r' {
            j += 1;
        }
        (TokenKind::LineComment, j, None)
    }

    fn word_end(&self, mut j: usize) -> usize {
        while j < self.chars.len() && is_word_cont(self.chars[j]) {
            j += 1;
        }
        j
    }

    fn number(&self, i: usize) -> usize {
        let n = self.chars.len();
        let mut j = i;
        while j < n && self.chars[j].is_ascii_digit() {
            j += 1;
        }
        if j < n && self.chars[j] == '.' && self.at(j + 1).is_some_and(|c| c.is_ascii_digit()) {
            j += 1;
            while j < n && self.chars[j].is_ascii_digit() {
                j += 1;
            }
        } else if j < n && self.chars[j] == '.' && j > i {
            // `1.` is a number, but `1..2` and `1.col` keep the dot separate.
            if !self.at(j + 1).is_some_and(|c| is_word_start(c) || c == '.') {
                j += 1;
            }
        }
        if j < n && matches!(self.chars[j], 'e' | 'E') {
            let mut k = j + 1;
            if k < n && matches!(self.chars[k], '+' | '-') {
                k += 1;
            }
            if k < n && self.chars[k].is_ascii_digit() {
                while k < n && self.chars[k].is_ascii_digit() {
                    k += 1;
                }
                j = k;
            }
        }
        while j < n && (self.chars[j].is_alphanumeric() || self.chars[j] == '_') {
            j += 1;
        }
        j
    }

    /// Token that starts at `i` (`i < len`): kind, end (exclusive), and a problem message.
    pub(crate) fn next(&self, i: usize) -> Next {
        let n = self.chars.len();
        let c = self.chars[i];
        let r = &self.rules;
        let nx = self.at(i + 1);

        if c.is_whitespace() {
            let mut j = i + 1;
            while j < n && self.chars[j].is_whitespace() {
                j += 1;
            }
            return (TokenKind::Whitespace, j, None);
        }
        if c == '-' && nx == Some('-') && r.dash_comment {
            let ok = if r.mysql {
                // MySQL: only ASCII blanks and control characters (NBSP is an identifier char).
                self.at(i + 2)
                    .is_none_or(|x| x.is_ascii_whitespace() || x.is_ascii_control())
            } else {
                true
            };
            if ok {
                return self.line_comment(i);
            }
        }
        if c == '#' && r.hash_comment {
            return self.line_comment(i);
        }
        if c == '/' && nx == Some('/') && r.slash_comment {
            return self.line_comment(i);
        }
        if r.mysql {
            // Executable comments `/*! ... */`, `/*!50000 ... */`, `/*M! ... */` are code:
            // the opener (marker and version) and the closer are blanks, the content is lexed
            // as ordinary SQL.
            if c == '/' && nx == Some('*') {
                let m = match self.at(i + 2) {
                    Some('!') => Some(i + 3),
                    Some('M') if self.at(i + 3) == Some('!') => Some(i + 4),
                    _ => None,
                };
                if let Some(mut j) = m {
                    while self.at(j).is_some_and(|d| d.is_ascii_digit()) {
                        j += 1;
                    }
                    return (TokenKind::Whitespace, j, None);
                }
            }
            if c == '*' && nx == Some('/') {
                return (TokenKind::Whitespace, i + 2, None);
            }
        }
        if c == '/' && nx == Some('*') {
            return self.block_comment(i);
        }
        if c == '\'' {
            return self.string_token(i, r.backslash);
        }
        if c == '"' {
            if r.dq_string {
                let (end, ok) = self.quoted(i, '"', r.backslash);
                return (
                    TokenKind::String,
                    end,
                    if ok {
                        None
                    } else {
                        Some("unterminated string")
                    },
                );
            }
            let (end, ok) = self.quoted(i, '"', false);
            return (
                TokenKind::QuotedIdent,
                end,
                if ok {
                    None
                } else {
                    Some("unterminated quoted identifier")
                },
            );
        }
        if c == '`' && r.mysql {
            let (end, ok) = self.quoted(i, '`', false);
            return (
                TokenKind::QuotedIdent,
                end,
                if ok {
                    None
                } else {
                    Some("unterminated quoted identifier")
                },
            );
        }
        if c == '[' && r.mssql {
            let mut j = i + 1;
            while j < n {
                if self.chars[j] == ']' {
                    if self.at(j + 1) == Some(']') {
                        j += 2;
                        continue;
                    }
                    return (TokenKind::QuotedIdent, j + 1, None);
                }
                j += 1;
            }
            return (
                TokenKind::QuotedIdent,
                n,
                Some("unterminated quoted identifier"),
            );
        }
        if c == '$' && r.pg {
            return self.dollar(i);
        }
        if c.is_ascii_digit() || (c == '.' && nx.is_some_and(|x| x.is_ascii_digit())) {
            return (TokenKind::Number, self.number(i), None);
        }
        if is_word_start(c) {
            let end = self.word_end(i);
            // String prefixes: only when the word is exactly the prefix.
            if self.at(end) == Some('\'') {
                let word: String = self.chars[i..end].iter().collect();
                let w = word.to_ascii_uppercase();
                let hit = match (w.as_str(), r) {
                    ("E", r) if r.pg => Some(self.string_token(end, true)),
                    ("B" | "X" | "N", r) if r.pg || r.mysql => {
                        Some(self.string_token(end, r.backslash))
                    }
                    ("N", r) if r.mssql || r.oracle => Some(self.string_token(end, false)),
                    ("Q" | "NQ", r) if r.oracle => self.alt_quote(end),
                    _ => None,
                };
                if let Some((k, e, p)) = hit {
                    return (k, e, p);
                }
            }
            if r.pg
                && matches!(self.chars[i], 'u' | 'U')
                && end == i + 1
                && self.at(end) == Some('&')
                && self.at(end + 1) == Some('\'')
            {
                return self.string_token(end + 1, false);
            }
            return (TokenKind::Word, end, None);
        }
        if (c == '@' && r.at_words) || (c == '#' && r.mssql) {
            let mut j = i + 1;
            if self.at(j) == Some(c) {
                j += 1;
            }
            let end = self.word_end(j);
            if end > j {
                return (TokenKind::Word, end, None);
            }
            if c == '@' {
                return (TokenKind::Punct, if j > i + 1 { j } else { i + 1 }, None);
            }
        }
        if c == ':' && matches!(nx, Some(':' | '=')) {
            return (TokenKind::Punct, i + 2, None);
        }
        (TokenKind::Punct, i + 1, None)
    }

    /// PostgreSQL `$` forms: `$1`, `$tag$ ... $tag$`, or a bare `$`.
    fn dollar(&self, i: usize) -> Next {
        let n = self.chars.len();
        let mut j = i + 1;
        if self.at(j).is_some_and(|c| c.is_ascii_digit()) {
            while j < n && self.chars[j].is_ascii_digit() {
                j += 1;
            }
            return (TokenKind::Number, j, None);
        }
        if self.at(j).is_some_and(is_word_start) {
            while j < n && (self.chars[j].is_alphanumeric() || self.chars[j] == '_') {
                j += 1;
            }
        }
        if self.at(j) == Some('$') {
            let delim = &self.chars[i..=j];
            let mut k = j + 1;
            while k + delim.len() <= n {
                if self.chars[k..k + delim.len()] == *delim {
                    return (TokenKind::String, k + delim.len(), None);
                }
                k += 1;
            }
            return (
                TokenKind::String,
                n,
                Some("unterminated dollar-quoted string"),
            );
        }
        (TokenKind::Punct, i + 1, None)
    }
}

/// Tokenize `text`; the tokens tile the whole text.
pub fn tokenize(d: ScriptDialect, text: &str) -> Vec<Token> {
    tokenize_with_problems(d, text).0
}

/// Like [`tokenize`], also returning lexical problems (unterminated strings / comments).
pub fn tokenize_with_problems(d: ScriptDialect, text: &str) -> (Vec<Token>, Vec<ScriptProblem>) {
    let chars: Vec<char> = text.chars().collect();
    tokenize_chars(d, &chars)
}

pub(crate) fn tokenize_chars(d: ScriptDialect, chars: &[char]) -> (Vec<Token>, Vec<ScriptProblem>) {
    let lx = Lexer::new(chars, d);
    let mut out = Vec::new();
    let mut problems = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let (kind, end, problem) = lx.next(i);
        debug_assert!(end > i);
        let end = end.max(i + 1);
        let range = TextRange {
            start: to_u32(i),
            end: to_u32(end),
        };
        out.push(Token { kind, range });
        if let Some(message) = problem {
            problems.push(ScriptProblem {
                range,
                message: message.to_string(),
            });
        }
        i = end;
    }
    (out, problems)
}

pub(crate) fn to_u32(i: usize) -> u32 {
    u32::try_from(i).unwrap_or(u32::MAX)
}

/// Text of a token range.
pub(crate) fn slice(chars: &[char], r: TextRange) -> String {
    chars[r.start as usize..(r.end as usize).min(chars.len())]
        .iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PG: ScriptDialect = ScriptDialect::Postgres;
    const MY: ScriptDialect = ScriptDialect::mysql(false, true);
    const MY_NB: ScriptDialect = ScriptDialect::mysql(false, false);

    fn kinds(d: ScriptDialect, text: &str) -> Vec<(TokenKind, String)> {
        let chars: Vec<char> = text.chars().collect();
        tokenize(d, text)
            .into_iter()
            .filter(|t| t.kind != TokenKind::Whitespace)
            .map(|t| (t.kind, slice(&chars, t.range)))
            .collect()
    }

    fn one(d: ScriptDialect, text: &str) -> TokenKind {
        let k = kinds(d, text);
        assert_eq!(k.len(), 1, "{text:?} -> {k:?}");
        k[0].0
    }

    #[test]
    fn empty_text_has_no_tokens() {
        assert!(tokenize(PG, "").is_empty());
    }

    #[test]
    fn tokens_tile_the_text() {
        for d in [PG, MY, ScriptDialect::MsSql, ScriptDialect::Oracle] {
            let text = "select 'é', \"x\" /* c */ -- d\n from t where a::int = $1;";
            let toks = tokenize(d, text);
            let mut pos = 0;
            for t in &toks {
                assert_eq!(t.range.start, pos);
                pos = t.range.end;
            }
            assert_eq!(pos as usize, text.chars().count());
        }
    }

    #[test]
    fn strings_and_prefixes() {
        assert_eq!(one(PG, "'it''s'"), TokenKind::String);
        assert_eq!(one(PG, "E'it\\'s'"), TokenKind::String);
        assert_eq!(one(PG, "e'a'"), TokenKind::String);
        assert_eq!(one(PG, "U&'d\\0061t'"), TokenKind::String);
        assert_eq!(one(PG, "B'101'"), TokenKind::String);
        assert_eq!(one(ScriptDialect::MsSql, "N'x'"), TokenKind::String);
        assert_eq!(one(ScriptDialect::Oracle, "q'[a'b]'"), TokenKind::String);
        assert_eq!(one(ScriptDialect::Oracle, "nq'{a}'"), TokenKind::String);
        assert_eq!(one(MY, "'a\\'b'"), TokenKind::String);
        assert_eq!(one(MY, "\"a\\\"b\""), TokenKind::String);
        assert_eq!(one(MY, "X'ab'"), TokenKind::String);
    }

    #[test]
    fn standard_strings_do_not_use_backslash() {
        // In PG a plain string ends at the second quote even after a backslash.
        let k = kinds(PG, "'a\\' x");
        assert_eq!(k[0], (TokenKind::String, "'a\\'".to_string()));
        let k = kinds(MY_NB, "'a\\' x");
        assert_eq!(k[0], (TokenKind::String, "'a\\'".to_string()));
        let k = kinds(MY, "'a\\' x'");
        assert_eq!(k.len(), 1);
    }

    #[test]
    fn word_ending_in_e_is_not_an_escape_prefix() {
        let k = kinds(PG, "ae'x\\'");
        assert_eq!(k[0].0, TokenKind::Word);
        assert_eq!(k[1], (TokenKind::String, "'x\\'".to_string()));
        let k = kinds(PG, "1e'x\\'");
        assert_eq!(k[0].0, TokenKind::Number);
        assert_eq!(k[1].1, "'x\\'");
    }

    #[test]
    fn dollar_quoting() {
        assert_eq!(one(PG, "$$ a; b $$"), TokenKind::String);
        assert_eq!(one(PG, "$tag$ $$ ; $tag$"), TokenKind::String);
        assert_eq!(one(PG, "$1"), TokenKind::Number);
        let k = kinds(PG, "a$b $x");
        assert_eq!(k[0].1, "a$b");
        assert_eq!(k[1].0, TokenKind::Punct);
        let (_, p) = tokenize_with_problems(PG, "$$ open");
        assert_eq!(p.len(), 1);
        // not a dollar quote outside PostgreSQL
        assert_eq!(kinds(MY, "$$").len(), 2);
    }

    #[test]
    fn quoted_identifiers() {
        assert_eq!(one(PG, "\"a\"\"b\""), TokenKind::QuotedIdent);
        assert_eq!(one(MY, "`a``b`"), TokenKind::QuotedIdent);
        assert_eq!(one(ScriptDialect::MsSql, "[a]]b]"), TokenKind::QuotedIdent);
        // brackets are punctuation in PG
        assert_eq!(kinds(PG, "a[1]").len(), 4);
    }

    #[test]
    fn comments() {
        assert_eq!(one(PG, "-- x"), TokenKind::LineComment);
        assert_eq!(one(MY, "# x"), TokenKind::LineComment);
        assert_eq!(one(MY, "-- x"), TokenKind::LineComment);
        // MySQL needs a space after the dashes
        assert_eq!(kinds(MY, "--x").len(), 3);
        assert_eq!(one(PG, "--x"), TokenKind::LineComment);
        assert_eq!(one(PG, "/* a /* b */ c */"), TokenKind::BlockComment);
        assert_eq!(kinds(MY, "/* a /* b */ c */").len(), 2);
        assert_eq!(
            one(ScriptDialect::MsSql, "/* a /* b */ c */"),
            TokenKind::BlockComment
        );
        assert_eq!(kinds(PG, "# x").len(), 2);
        assert_eq!(
            one(ScriptDialect::MongoShell, "// x"),
            TokenKind::LineComment
        );
        let k = kinds(PG, "a -- x\nb");
        assert_eq!(k.len(), 3);
    }

    #[test]
    fn mysql_executable_comments_are_code() {
        let k = kinds(MY, "/*!50000 DROP TABLE t */");
        let words: Vec<_> = k.iter().map(|(_, t)| t.as_str()).collect();
        assert_eq!(words, ["DROP", "TABLE", "t"]);
        assert_eq!(kinds(MY, "/*! SELECT 1 */").len(), 2);
        assert_eq!(kinds(MY, "/*M!100000 SELECT 1 */").len(), 2);
        // ordinary comments stay comments
        assert_eq!(one(MY, "/* DROP TABLE t */"), TokenKind::BlockComment);
        // other dialects: a comment, whatever follows `/*`
        assert_eq!(one(PG, "/*! x */"), TokenKind::BlockComment);
    }

    #[test]
    fn mysql_dash_comment_needs_ascii_blank() {
        assert_eq!(kinds(MY, "--\u{a0}x").len(), 3);
        assert_eq!(one(MY, "-- x"), TokenKind::LineComment);
        assert_eq!(one(MY, "--\tx"), TokenKind::LineComment);
    }

    #[test]
    fn unterminated_things_are_problems() {
        for text in ["'abc", "/* abc", "\"abc", "$$ abc"] {
            let (t, p) = tokenize_with_problems(PG, text);
            assert_eq!(t.len(), 1, "{text}");
            assert_eq!(p.len(), 1, "{text}");
            assert_eq!(p[0].range.end as usize, text.chars().count());
        }
        let (_, p) = tokenize_with_problems(ScriptDialect::MsSql, "[abc");
        assert_eq!(p.len(), 1);
        let (_, p) = tokenize_with_problems(PG, "select 1");
        assert!(p.is_empty());
    }

    #[test]
    fn numbers_and_puncts() {
        assert_eq!(one(PG, "12.5e-3"), TokenKind::Number);
        assert_eq!(one(PG, ".5"), TokenKind::Number);
        assert_eq!(one(PG, "0xFF"), TokenKind::Number);
        let k = kinds(PG, "a::int");
        assert_eq!(k[1], (TokenKind::Punct, "::".to_string()));
        let k = kinds(PG, "a := 1");
        assert_eq!(k[1], (TokenKind::Punct, ":=".to_string()));
        let k = kinds(PG, "t.col");
        assert_eq!(k.len(), 3);
        let k = kinds(PG, "1.col");
        assert_eq!(k[0].1, "1");
    }

    #[test]
    fn variables() {
        assert_eq!(one(ScriptDialect::MsSql, "@p"), TokenKind::Word);
        assert_eq!(one(ScriptDialect::MsSql, "@@rowcount"), TokenKind::Word);
        assert_eq!(one(ScriptDialect::MsSql, "#tmp"), TokenKind::Word);
        assert_eq!(one(MY, "@v"), TokenKind::Word);
        let k = kinds(MY, "'a'@'b'");
        assert_eq!(k[1], (TokenKind::Punct, "@".to_string()));
        // `@` is an operator in PostgreSQL
        assert_eq!(kinds(PG, "@p")[0].0, TokenKind::Punct);
    }

    #[test]
    fn unicode_offsets_are_characters() {
        let toks = tokenize(PG, "'é😀' x");
        assert_eq!(toks[0].range, TextRange { start: 0, end: 4 });
        assert_eq!(toks[2].range, TextRange { start: 5, end: 6 });
    }

    #[test]
    fn mongo_and_redis_strings() {
        assert_eq!(
            one(ScriptDialect::MongoShell, "\"a\\\"b\""),
            TokenKind::String
        );
        assert_eq!(one(ScriptDialect::RedisCli, "'a b'"), TokenKind::String);
    }
}
