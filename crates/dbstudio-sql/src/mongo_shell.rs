//! Parser for the supported subset of the MongoDB shell syntax (no JavaScript is executed).
//! The AST has no `bson` dependency; the driver converts [`ShellValue`] to `bson::Bson`.
//!
//! Grammar: `use <db>`, `show <what>`, `db.<coll>.<method>(args).chain()...`,
//! `db["coll"].<method>(..)`, `db.getCollection("coll").<method>(..)` and
//! `db.<method>(args)`. Arguments are relaxed EJSON (unquoted keys, single quotes, trailing
//! commas, comments) with the shell constructors (`ObjectId`, `ISODate`, `NumberLong`...).

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
    /// `null` (and `undefined`).
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
    /// `ISODate("..")` / `new Date(..)`; the argument text as written (`""` for `new Date()`,
    /// meaning "now"; milliseconds as decimal digits).
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

impl ShellValue {
    /// Field of an [`ShellValue::Object`].
    pub fn get(&self, key: &str) -> Option<&ShellValue> {
        match self {
            ShellValue::Object(f) => f.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// True for `{}`.
    pub fn is_empty_object(&self) -> bool {
        matches!(self, ShellValue::Object(f) if f.is_empty())
    }
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
    /// Method (or `use` database name / `show` argument).
    pub method: String,
    /// Arguments of the method call.
    pub args: Vec<ShellValue>,
    /// Chained calls (`.sort(..).limit(..)`).
    pub chain: Vec<(String, Vec<ShellValue>)>,
}

fn err(message: impl Into<String>, pos: usize) -> Error {
    Error::Query {
        message: message.into(),
        position: Some(u32::try_from(pos).unwrap_or(u32::MAX)),
    }
}

struct Parser<'a> {
    c: &'a [char],
    i: usize,
}

fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_' || c == '$'
}

fn is_ident_cont(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<char> {
        self.c.get(self.i).copied()
    }

    fn peek_at(&self, k: usize) -> Option<char> {
        self.c.get(self.i + k).copied()
    }

    /// Skip whitespace (newlines included) and comments.
    fn trivia(&mut self) -> Result<()> {
        loop {
            match self.peek() {
                Some(c) if c.is_whitespace() => self.i += 1,
                Some('/') if self.peek_at(1) == Some('/') => {
                    while self.peek().is_some_and(|c| c != '\n') {
                        self.i += 1;
                    }
                }
                Some('/') if self.peek_at(1) == Some('*') => {
                    let start = self.i;
                    self.i += 2;
                    loop {
                        match self.peek() {
                            None => return Err(err("unterminated comment", start)),
                            Some('*') if self.peek_at(1) == Some('/') => {
                                self.i += 2;
                                break;
                            }
                            _ => self.i += 1,
                        }
                    }
                }
                _ => return Ok(()),
            }
        }
    }

    /// Skip spaces, tabs and comments but stop at a newline.
    fn inline_trivia(&mut self) -> Result<()> {
        loop {
            match self.peek() {
                Some(c) if c.is_whitespace() && c != '\n' => self.i += 1,
                Some('/') if self.peek_at(1) == Some('/') => {
                    while self.peek().is_some_and(|c| c != '\n') {
                        self.i += 1;
                    }
                }
                Some('/') if self.peek_at(1) == Some('*') => {
                    let start = self.i;
                    self.i += 2;
                    loop {
                        match self.peek() {
                            None => return Err(err("unterminated comment", start)),
                            Some('*') if self.peek_at(1) == Some('/') => {
                                self.i += 2;
                                break;
                            }
                            _ => self.i += 1,
                        }
                    }
                }
                _ => return Ok(()),
            }
        }
    }

    fn ident(&mut self) -> Option<String> {
        if !self.peek().is_some_and(is_ident_start) {
            return None;
        }
        let s = self.i;
        while self.peek().is_some_and(is_ident_cont) {
            self.i += 1;
        }
        Some(self.c[s..self.i].iter().collect())
    }

    fn expect(&mut self, ch: char) -> Result<()> {
        self.trivia()?;
        if self.peek() == Some(ch) {
            self.i += 1;
            Ok(())
        } else {
            Err(err(format!("expected `{ch}`"), self.i))
        }
    }

    /// True (and positioned on it) when the next significant character is `.`.
    fn dot_ahead(&mut self) -> Result<bool> {
        let save = self.i;
        self.trivia()?;
        if self.peek() == Some('.') {
            Ok(true)
        } else {
            self.i = save;
            Ok(false)
        }
    }

    fn statement(&mut self) -> Result<ShellStatement> {
        let start = self.i;
        let word = self
            .ident()
            .ok_or_else(|| err("unsupported statement", start))?;
        match word.as_str() {
            "use" => {
                self.inline_trivia()?;
                let s = self.i;
                while self.peek().is_some_and(|c| !c.is_whitespace() && c != ';') {
                    self.i += 1;
                }
                if s == self.i {
                    return Err(err("`use` needs a database name", s));
                }
                let mut name: String = self.c[s..self.i].iter().collect();
                if name.len() >= 2
                    && (name.starts_with('"') && name.ends_with('"')
                        || name.starts_with('\'') && name.ends_with('\''))
                {
                    name = name[1..name.len() - 1].to_string();
                }
                Ok(ShellStatement {
                    range: self.range(start),
                    kind: ShellKind::Use,
                    collection: None,
                    method: name,
                    args: vec![],
                    chain: vec![],
                })
            }
            "show" => {
                self.inline_trivia()?;
                let s = self.i;
                let what = self
                    .ident()
                    .ok_or_else(|| err("`show` needs an argument", s))?;
                Ok(ShellStatement {
                    range: self.range(start),
                    kind: ShellKind::Show,
                    collection: None,
                    method: what,
                    args: vec![],
                    chain: vec![],
                })
            }
            "db" => self.db_statement(start),
            _ => Err(err(
                format!(
                    "unsupported statement `{word}`: only `use`, `show` and `db...` are supported"
                ),
                start,
            )),
        }
    }

    fn range(&self, start: usize) -> TextRange {
        TextRange {
            start: u32::try_from(start).unwrap_or(u32::MAX),
            end: u32::try_from(self.i).unwrap_or(u32::MAX),
        }
    }

    fn db_statement(&mut self, start: usize) -> Result<ShellStatement> {
        let mut collection: Option<String> = None;
        let mut method: Option<(String, Vec<ShellValue>)> = None;
        loop {
            if method.is_some() {
                break;
            }
            let save = self.i;
            self.trivia()?;
            match self.peek() {
                Some('.') => {
                    self.i += 1;
                    self.trivia()?;
                    let at = self.i;
                    let name = self
                        .ident()
                        .ok_or_else(|| err("expected a name after `.`", at))?;
                    self.trivia()?;
                    if self.peek() == Some('(') {
                        let args = self.args()?;
                        if name == "getCollection" && collection.is_none() {
                            match args.as_slice() {
                                [ShellValue::String(s)] => collection = Some(s.clone()),
                                _ => {
                                    return Err(err("getCollection needs one string argument", at));
                                }
                            }
                        } else {
                            method = Some((name, args));
                        }
                    } else if collection.is_none() {
                        collection = Some(name);
                    } else {
                        return Err(err("expected a method call", self.i));
                    }
                }
                Some('[') if collection.is_none() => {
                    self.i += 1;
                    self.trivia()?;
                    let at = self.i;
                    match self.peek() {
                        Some(q @ ('"' | '\'')) => {
                            let s = self.string(q)?;
                            collection = Some(s);
                        }
                        _ => return Err(err("expected a string collection name", at)),
                    }
                    self.expect(']')?;
                }
                _ => {
                    self.i = save;
                    break;
                }
            }
        }
        let Some((m, args)) = method else {
            return Err(err("expected a method call such as db.coll.find()", self.i));
        };
        let end_of_call = self.i;
        let mut chain = Vec::new();
        let mut end = end_of_call;
        while self.dot_ahead()? {
            self.i += 1;
            self.trivia()?;
            let at = self.i;
            let name = self
                .ident()
                .ok_or_else(|| err("expected a method name after `.`", at))?;
            self.trivia()?;
            if self.peek() != Some('(') {
                return Err(err(
                    format!("unsupported expression `.{name}`: expected a call"),
                    self.i,
                ));
            }
            let a = self.args()?;
            chain.push((name, a));
            end = self.i;
        }
        self.i = end;
        Ok(ShellStatement {
            range: self.range(start),
            kind: if collection.is_some() {
                ShellKind::Collection
            } else {
                ShellKind::Database
            },
            collection,
            method: m,
            args,
            chain,
        })
    }

    fn args(&mut self) -> Result<Vec<ShellValue>> {
        self.expect('(')?;
        let mut out = Vec::new();
        loop {
            self.trivia()?;
            match self.peek() {
                Some(')') => {
                    self.i += 1;
                    return Ok(out);
                }
                None => return Err(err("unterminated argument list", self.i)),
                _ => {}
            }
            out.push(self.value()?);
            self.trivia()?;
            match self.peek() {
                Some(',') => self.i += 1,
                Some(')') => {}
                _ => return Err(err("expected `,` or `)`", self.i)),
            }
        }
    }

    fn value(&mut self) -> Result<ShellValue> {
        self.trivia()?;
        let at = self.i;
        match self.peek() {
            None => Err(err("unexpected end of input", at)),
            Some('{') => self.object(),
            Some('[') => self.array(),
            Some(q @ ('"' | '\'')) => Ok(ShellValue::String(self.string(q)?)),
            Some('/') => self.regex(),
            Some(c) if c.is_ascii_digit() || matches!(c, '-' | '+' | '.') => self.number(),
            Some(c) if is_ident_start(c) => self.word_value(),
            Some(c) => Err(err(format!("unexpected character `{c}`"), at)),
        }
    }

    fn object(&mut self) -> Result<ShellValue> {
        self.i += 1; // {
        let mut fields: Vec<(String, ShellValue)> = Vec::new();
        loop {
            self.trivia()?;
            let at = self.i;
            let key = match self.peek() {
                Some('}') => {
                    self.i += 1;
                    return Ok(ShellValue::Object(fields));
                }
                None => return Err(err("unterminated object", at)),
                Some(q @ ('"' | '\'')) => self.string(q)?,
                Some(c) if is_ident_cont(c) => {
                    let s = self.i;
                    while self.peek().is_some_and(is_ident_cont) {
                        self.i += 1;
                    }
                    self.c[s..self.i].iter().collect()
                }
                Some(c) => return Err(err(format!("unexpected character `{c}` in object"), at)),
            };
            self.trivia()?;
            if self.peek() != Some(':') {
                return Err(err("expected `:` after the key", self.i));
            }
            self.i += 1;
            let v = self.value()?;
            if let Some(slot) = fields.iter_mut().find(|(k, _)| *k == key) {
                slot.1 = v;
            } else {
                fields.push((key, v));
            }
            self.trivia()?;
            match self.peek() {
                Some(',') => self.i += 1,
                Some('}') => {}
                _ => return Err(err("expected `,` or `}`", self.i)),
            }
        }
    }

    fn array(&mut self) -> Result<ShellValue> {
        self.i += 1; // [
        let mut items = Vec::new();
        loop {
            self.trivia()?;
            match self.peek() {
                Some(']') => {
                    self.i += 1;
                    return Ok(ShellValue::Array(items));
                }
                None => return Err(err("unterminated array", self.i)),
                _ => {}
            }
            items.push(self.value()?);
            self.trivia()?;
            match self.peek() {
                Some(',') => self.i += 1,
                Some(']') => {}
                _ => return Err(err("expected `,` or `]`", self.i)),
            }
        }
    }

    /// String literal; `self.i` is on the opening quote.
    fn string(&mut self, quote: char) -> Result<String> {
        let start = self.i;
        self.i += 1;
        let mut out = String::new();
        loop {
            let Some(c) = self.peek() else {
                return Err(err("unterminated string", start));
            };
            self.i += 1;
            if c == quote {
                return Ok(out);
            }
            if c != '\\' {
                out.push(c);
                continue;
            }
            let Some(e) = self.peek() else {
                return Err(err("unterminated string", start));
            };
            self.i += 1;
            match e {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                'b' => out.push('\u{8}'),
                'f' => out.push('\u{c}'),
                'v' => out.push('\u{b}'),
                '0' => out.push('\0'),
                'x' => {
                    let h = self.hex(2)?;
                    out.push(char::from_u32(h).unwrap_or('\u{fffd}'));
                }
                'u' => {
                    let h = self.hex(4)?;
                    if (0xD800..0xDC00).contains(&h)
                        && self.peek() == Some('\\')
                        && self.peek_at(1) == Some('u')
                    {
                        let save = self.i;
                        self.i += 2;
                        let lo = self.hex(4)?;
                        if (0xDC00..0xE000).contains(&lo) {
                            let cp = 0x10000 + ((h - 0xD800) << 10) + (lo - 0xDC00);
                            out.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
                            continue;
                        }
                        self.i = save;
                    }
                    out.push(char::from_u32(h).unwrap_or('\u{fffd}'));
                }
                '\n' => {}
                other => out.push(other),
            }
        }
    }

    fn hex(&mut self, n: usize) -> Result<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let d = self
                .peek()
                .and_then(|c| c.to_digit(16))
                .ok_or_else(|| err("invalid escape sequence", self.i))?;
            v = v * 16 + d;
            self.i += 1;
        }
        Ok(v)
    }

    fn regex(&mut self) -> Result<ShellValue> {
        let start = self.i;
        self.i += 1;
        let mut pat = String::new();
        let mut class = false;
        loop {
            let Some(c) = self.peek() else {
                return Err(err("unterminated regular expression", start));
            };
            self.i += 1;
            match c {
                '\\' => {
                    pat.push(c);
                    if let Some(n) = self.peek() {
                        pat.push(n);
                        self.i += 1;
                    }
                }
                '[' => {
                    class = true;
                    pat.push(c);
                }
                ']' => {
                    class = false;
                    pat.push(c);
                }
                '/' if !class => break,
                '\n' => return Err(err("unterminated regular expression", start)),
                _ => pat.push(c),
            }
        }
        let fs = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
            self.i += 1;
        }
        Ok(ShellValue::Regex {
            pattern: pat,
            flags: self.c[fs..self.i].iter().collect(),
        })
    }

    fn number(&mut self) -> Result<ShellValue> {
        let start = self.i;
        let mut neg = false;
        match self.peek() {
            Some('-') => {
                neg = true;
                self.i += 1;
            }
            Some('+') => self.i += 1,
            _ => {}
        }
        if self.peek().is_some_and(is_ident_start) {
            // -Infinity / +Infinity
            let w = self.ident().unwrap_or_default();
            return match w.as_str() {
                "Infinity" => Ok(ShellValue::Double(if neg {
                    f64::NEG_INFINITY
                } else {
                    f64::INFINITY
                })),
                _ => Err(err("invalid number", start)),
            };
        }
        if self.peek() == Some('0') && matches!(self.peek_at(1), Some('x' | 'X')) {
            self.i += 2;
            let s = self.i;
            while self.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
                self.i += 1;
            }
            let t: String = self.c[s..self.i].iter().collect();
            let v = i64::from_str_radix(&t, 16).map_err(|_| err("invalid hex number", start))?;
            return Ok(ShellValue::Int(if neg { -v } else { v }));
        }
        let s = self.i;
        let mut float = false;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.i += 1;
        }
        if self.peek() == Some('.') {
            float = true;
            self.i += 1;
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                self.i += 1;
            }
        }
        if matches!(self.peek(), Some('e' | 'E')) {
            let save = self.i;
            self.i += 1;
            if matches!(self.peek(), Some('+' | '-')) {
                self.i += 1;
            }
            if self.peek().is_some_and(|c| c.is_ascii_digit()) {
                float = true;
                while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                    self.i += 1;
                }
            } else {
                self.i = save;
            }
        }
        let t: String = self.c[s..self.i].iter().collect();
        if t.is_empty() || t == "." {
            return Err(err("invalid number", start));
        }
        if !float {
            if let Ok(v) = t.parse::<i64>() {
                return Ok(ShellValue::Int(if neg { -v } else { v }));
            }
        }
        let v: f64 = t.parse().map_err(|_| err("invalid number", start))?;
        Ok(ShellValue::Double(if neg { -v } else { v }))
    }

    fn word_value(&mut self) -> Result<ShellValue> {
        let start = self.i;
        let mut w = self.ident().unwrap_or_default();
        if w == "new" {
            self.trivia()?;
            w = self
                .ident()
                .ok_or_else(|| err("expected a constructor after `new`", self.i))?;
        }
        match w.as_str() {
            "true" => return Ok(ShellValue::Bool(true)),
            "false" => return Ok(ShellValue::Bool(false)),
            "null" | "undefined" => return Ok(ShellValue::Null),
            "NaN" => return Ok(ShellValue::Double(f64::NAN)),
            "Infinity" => return Ok(ShellValue::Double(f64::INFINITY)),
            _ => {}
        }
        self.trivia()?;
        if self.peek() != Some('(') {
            return Err(err(format!("unsupported value `{w}`"), start));
        }
        let args = self.args()?;
        construct(&w, args, start)
    }
}

fn num_text(v: &ShellValue) -> Option<String> {
    match v {
        ShellValue::Int(i) => Some(i.to_string()),
        ShellValue::Double(d) => Some(d.to_string()),
        ShellValue::String(s) => Some(s.clone()),
        _ => None,
    }
}

fn construct(name: &str, args: Vec<ShellValue>, at: usize) -> Result<ShellValue> {
    let bad = |m: &str| err(format!("{name}: {m}"), at);
    match name {
        "ObjectId" => match args.as_slice() {
            [ShellValue::String(s)]
                if s.len() == 24 && s.chars().all(|c| c.is_ascii_hexdigit()) =>
            {
                Ok(ShellValue::ObjectId(s.to_ascii_lowercase()))
            }
            _ => Err(bad("expects a 24 character hex string")),
        },
        "ISODate" | "Date" => match args.as_slice() {
            [] => Ok(ShellValue::Date(String::new())),
            [v @ (ShellValue::String(_) | ShellValue::Int(_) | ShellValue::Double(_))] => {
                Ok(ShellValue::Date(num_text(v).unwrap_or_default()))
            }
            _ => Err(bad("expects a date string or milliseconds")),
        },
        "NumberInt" | "Int32" => match args.as_slice() {
            [v] => num_text(v)
                .and_then(|t| t.trim().parse::<i32>().ok())
                .map(ShellValue::NumberInt)
                .or_else(|| match v {
                    ShellValue::Double(d) if d.fract() == 0.0 && d.abs() < 2e9 => {
                        Some(ShellValue::NumberInt(*d as i32))
                    }
                    _ => None,
                })
                .ok_or_else(|| bad("expects a 32 bit integer")),
            _ => Err(bad("expects one argument")),
        },
        "NumberLong" | "Long" => match args.as_slice() {
            [v] => num_text(v)
                .and_then(|t| t.trim().parse::<i64>().ok())
                .map(ShellValue::NumberLong)
                .ok_or_else(|| bad("expects a 64 bit integer")),
            _ => Err(bad("expects one argument")),
        },
        "NumberDecimal" | "Decimal128" => match args.as_slice() {
            [v] => num_text(v)
                .map(ShellValue::NumberDecimal)
                .ok_or_else(|| bad("expects a decimal string")),
            _ => Err(bad("expects one argument")),
        },
        "UUID" => match args.as_slice() {
            [ShellValue::String(s)] => Ok(ShellValue::Uuid(s.clone())),
            _ => Err(bad("expects a string")),
        },
        "BinData" => match args.as_slice() {
            [ShellValue::Int(t), ShellValue::String(b)] => Ok(ShellValue::BinData {
                subtype: u8::try_from(*t).map_err(|_| bad("subtype must be 0..255"))?,
                base64: b.clone(),
            }),
            _ => Err(bad("expects (subtype, \"base64\")")),
        },
        "Timestamp" => match args.as_slice() {
            [ShellValue::Int(t), ShellValue::Int(i)] => Ok(ShellValue::Timestamp {
                t: u32::try_from(*t).map_err(|_| bad("seconds out of range"))?,
                i: u32::try_from(*i).map_err(|_| bad("increment out of range"))?,
            }),
            _ => Err(bad("expects (seconds, increment)")),
        },
        "MinKey" if args.is_empty() => Ok(ShellValue::MinKey),
        "MaxKey" if args.is_empty() => Ok(ShellValue::MaxKey),
        "RegExp" => match args.as_slice() {
            [ShellValue::String(p)] => Ok(ShellValue::Regex {
                pattern: p.clone(),
                flags: String::new(),
            }),
            [ShellValue::String(p), ShellValue::String(f)] => Ok(ShellValue::Regex {
                pattern: p.clone(),
                flags: f.clone(),
            }),
            _ => Err(bad("expects (pattern[, flags])")),
        },
        _ => Err(err(format!("unsupported function `{name}`"), at)),
    }
}

/// Parse a script into statements. Errors are `Error::Query` with the character position.
pub fn parse(text: &str) -> Result<Vec<ShellStatement>> {
    let chars: Vec<char> = text.chars().collect();
    let mut p = Parser { c: &chars, i: 0 };
    let mut out = Vec::new();
    loop {
        p.trivia()?;
        while p.peek() == Some(';') {
            p.i += 1;
            p.trivia()?;
        }
        if p.peek().is_none() {
            return Ok(out);
        }
        out.push(p.statement()?);
        p.inline_trivia()?;
        match p.peek() {
            None | Some('\n' | '\r' | ';') => {}
            Some(c) => {
                return Err(err(
                    format!("unexpected `{c}`: expected `;` or a new line"),
                    p.i,
                ));
            }
        }
    }
}

/// Collection and database operations whose first argument is a filter.
fn filter_is_all(stmt: &ShellStatement) -> bool {
    match stmt.args.first() {
        None => true,
        Some(v) => v.is_empty_object(),
    }
}

fn pipeline_writes(stmt: &ShellStatement) -> bool {
    match stmt.args.first() {
        Some(ShellValue::Array(stages)) => stages
            .iter()
            .any(|s| s.get("$out").is_some() || s.get("$merge").is_some()),
        _ => false,
    }
}

fn bulk_destructive(v: &ShellValue) -> Option<DestructiveReason> {
    let ops = match v {
        ShellValue::Array(a) => a,
        _ => return None,
    };
    let mut found = None;
    for op in ops {
        if let Some(d) = op.get("deleteMany") {
            if d.get("filter").is_none_or(ShellValue::is_empty_object) {
                return Some(DestructiveReason::DeleteManyAll);
            }
        }
        if let Some(u) = op.get("updateMany") {
            if u.get("filter").is_none_or(ShellValue::is_empty_object) {
                found = Some(DestructiveReason::UpdateManyAll);
            }
        }
    }
    found
}

const READ_COMMANDS: &[&str] = &[
    "ping",
    "find",
    "count",
    "distinct",
    "listCollections",
    "listDatabases",
    "listIndexes",
    "buildInfo",
    "buildinfo",
    "serverStatus",
    "dbStats",
    "dbstats",
    "collStats",
    "collstats",
    "hello",
    "isMaster",
    "ismaster",
    "hostInfo",
    "connectionStatus",
    "getParameter",
    "validate",
    "explain",
    "whatsmyuri",
    "currentOp",
    "top",
    "usersInfo",
    "rolesInfo",
    "replSetGetStatus",
];

/// Class and destructive reason of a statement.
pub fn classify(stmt: &ShellStatement) -> (StatementClass, Option<DestructiveReason>) {
    use StatementClass as C;
    match stmt.kind {
        ShellKind::Use => return (C::Session, None),
        ShellKind::Show => return (C::Read, None),
        _ => {}
    }
    let m = stmt.method.as_str();
    // `.explain(...)` anywhere in the chain turns a read into an Explain.
    if let Some((_, a)) = stmt.chain.iter().find(|(n, _)| n == "explain") {
        let analyze = matches!(
            a.first(),
            Some(ShellValue::String(s)) if s == "executionStats" || s == "allPlansExecution"
        ) || matches!(a.first(), Some(ShellValue::Bool(true)));
        return (
            C::Explain {
                analyze,
                writes: false,
            },
            None,
        );
    }
    if stmt.kind == ShellKind::Database {
        return classify_database(stmt);
    }
    match m {
        "find"
        | "findOne"
        | "countDocuments"
        | "estimatedDocumentCount"
        | "count"
        | "distinct"
        | "getIndexes"
        | "stats"
        | "validate"
        | "totalSize"
        | "storageSize"
        | "dataSize"
        | "totalIndexSize"
        | "latencyStats" => (C::Read, None),
        "aggregate" => {
            if pipeline_writes(stmt) {
                (C::Write, None)
            } else {
                (C::Read, None)
            }
        }
        "insertOne" | "insertMany" | "insert" | "save" | "updateOne" | "replaceOne"
        | "deleteOne" | "findOneAndUpdate" | "findOneAndReplace" | "findOneAndDelete"
        | "findAndModify" => (C::Write, None),
        "updateMany" => (
            C::Write,
            filter_is_all(stmt).then_some(DestructiveReason::UpdateManyAll),
        ),
        "update" => {
            let multi = stmt
                .args
                .get(2)
                .and_then(|o| o.get("multi"))
                .is_some_and(|v| *v == ShellValue::Bool(true));
            (
                C::Write,
                (multi && filter_is_all(stmt)).then_some(DestructiveReason::UpdateManyAll),
            )
        }
        "deleteMany" => (
            C::Write,
            filter_is_all(stmt).then_some(DestructiveReason::DeleteManyAll),
        ),
        "remove" => {
            let just_one = match stmt.args.get(1) {
                Some(ShellValue::Bool(true)) => true,
                Some(o) => o.get("justOne") == Some(&ShellValue::Bool(true)),
                None => false,
            };
            (
                C::Write,
                (filter_is_all(stmt) && !just_one).then_some(DestructiveReason::DeleteManyAll),
            )
        }
        "bulkWrite" => (C::Write, stmt.args.first().and_then(bulk_destructive)),
        "drop" => (C::Ddl, Some(DestructiveReason::DropCollection)),
        "createIndex" | "createIndexes" | "dropIndex" | "dropIndexes" | "renameCollection"
        | "reIndex" | "createSearchIndex" | "dropSearchIndex" | "convertToCapped" => (C::Ddl, None),
        _ => (C::Unknown, None),
    }
}

fn classify_database(stmt: &ShellStatement) -> (StatementClass, Option<DestructiveReason>) {
    use StatementClass as C;
    match stmt.method.as_str() {
        "getCollectionNames" | "getCollectionInfos" | "stats" | "getName" | "version"
        | "hostInfo" | "serverStatus" | "listCommands" | "getMongo" | "isMaster" | "hello"
        | "currentOp" | "getUsers" | "getRoles" | "getUser" | "getRole" | "getProfilingStatus" => {
            (C::Read, None)
        }
        "createCollection"
        | "createView"
        | "createUser"
        | "createRole"
        | "dropUser"
        | "dropRole"
        | "grantRolesToUser"
        | "revokeRolesFromUser" => (C::Ddl, None),
        "dropDatabase" => (C::Ddl, Some(DestructiveReason::DropDatabase)),
        "runCommand" | "adminCommand" => {
            let Some(ShellValue::Object(fields)) = stmt.args.first() else {
                return (C::Procedural, None);
            };
            let Some((cmd, val)) = fields.first() else {
                return (C::Procedural, None);
            };
            match cmd.as_str() {
                "drop" => (C::Ddl, Some(DestructiveReason::DropCollection)),
                "dropDatabase" => (C::Ddl, Some(DestructiveReason::DropDatabase)),
                "delete" => {
                    let all = fields
                        .iter()
                        .find(|(k, _)| k == "deletes")
                        .is_some_and(|(_, d)| match d {
                            ShellValue::Array(a) => a.iter().any(|x| {
                                x.get("q").is_none_or(ShellValue::is_empty_object)
                                    && matches!(x.get("limit"), Some(ShellValue::Int(0)) | None)
                            }),
                            _ => false,
                        });
                    (C::Write, all.then_some(DestructiveReason::DeleteManyAll))
                }
                "update" => {
                    let all = fields
                        .iter()
                        .find(|(k, _)| k == "updates")
                        .is_some_and(|(_, u)| match u {
                            ShellValue::Array(a) => a.iter().any(|x| {
                                x.get("q").is_none_or(ShellValue::is_empty_object)
                                    && x.get("multi") == Some(&ShellValue::Bool(true))
                            }),
                            _ => false,
                        });
                    (C::Write, all.then_some(DestructiveReason::UpdateManyAll))
                }
                "insert" | "findAndModify" | "findandmodify" | "create" | "createIndexes"
                | "renameCollection" => (C::Write, None),
                "aggregate" => {
                    let writes = fields.iter().any(|(k, v)| {
                        k == "pipeline"
                            && matches!(v, ShellValue::Array(a)
                                if a.iter().any(|s| s.get("$out").is_some() || s.get("$merge").is_some()))
                    });
                    (if writes { C::Write } else { C::Read }, None)
                }
                c if READ_COMMANDS.contains(&c) => {
                    let _ = val;
                    (C::Read, None)
                }
                _ => (C::Procedural, None),
            }
        }
        _ => (C::Unknown, None),
    }
}

/// Short text for display: `db.users.find`, `db.runCommand`, `use`, `show dbs`.
pub fn verb(stmt: &ShellStatement) -> String {
    match stmt.kind {
        ShellKind::Use => "use".to_string(),
        ShellKind::Show => format!("show {}", stmt.method),
        ShellKind::Collection => format!(
            "db.{}.{}",
            stmt.collection.as_deref().unwrap_or_default(),
            stmt.method
        ),
        ShellKind::Database => format!("db.{}", stmt.method),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use StatementClass as C;

    fn one(text: &str) -> ShellStatement {
        let mut v = parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}"));
        assert_eq!(v.len(), 1, "{text}");
        v.remove(0)
    }

    fn class(text: &str) -> (StatementClass, Option<DestructiveReason>) {
        classify(&one(text))
    }

    fn obj(fields: Vec<(&str, ShellValue)>) -> ShellValue {
        ShellValue::Object(
            fields
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        )
    }

    #[test]
    fn parses_find_with_chain() {
        let s = one("db.users.find({age: {$gt: 18}}, {name: 1}).sort({age: -1}).limit(10)");
        assert_eq!(s.kind, ShellKind::Collection);
        assert_eq!(s.collection.as_deref(), Some("users"));
        assert_eq!(s.method, "find");
        assert_eq!(s.args.len(), 2);
        assert_eq!(
            s.args[0],
            obj(vec![("age", obj(vec![("$gt", ShellValue::Int(18))]))])
        );
        assert_eq!(s.chain.len(), 2);
        assert_eq!(s.chain[0].0, "sort");
        assert_eq!(s.chain[1], ("limit".to_string(), vec![ShellValue::Int(10)]));
        assert_eq!(s.range, TextRange { start: 0, end: 68 });
    }

    #[test]
    fn use_and_show() {
        let v = parse("use shop\nshow dbs; show collections").unwrap();
        assert_eq!(v.len(), 3);
        assert_eq!((v[0].kind, v[0].method.as_str()), (ShellKind::Use, "shop"));
        assert_eq!((v[1].kind, v[1].method.as_str()), (ShellKind::Show, "dbs"));
        assert_eq!(v[2].method, "collections");
    }

    #[test]
    fn collection_accessors() {
        let s = one("db[\"my-coll\"].find()");
        assert_eq!(s.collection.as_deref(), Some("my-coll"));
        let s = one("db.getCollection('a.b').findOne({})");
        assert_eq!(s.collection.as_deref(), Some("a.b"));
        assert_eq!(s.method, "findOne");
        let s = one("db.runCommand({ping: 1})");
        assert_eq!(s.kind, ShellKind::Database);
        assert_eq!(s.collection, None);
        let s = one("db.getCollectionNames()");
        assert_eq!(s.kind, ShellKind::Database);
        let s = one("db.stats()");
        assert_eq!(s.kind, ShellKind::Database);
    }

    #[test]
    fn relaxed_syntax() {
        let s = one("db.a.insertOne({ 'k': \"v\", 'x y': 1, $in: [1, 2,], /* c */ z: null, })");
        assert_eq!(
            s.args[0],
            obj(vec![
                ("k", ShellValue::String("v".into())),
                ("x y", ShellValue::Int(1)),
                (
                    "$in",
                    ShellValue::Array(vec![ShellValue::Int(1), ShellValue::Int(2)])
                ),
                ("z", ShellValue::Null),
            ])
        );
        let s = one("db.a.find( // c\n {a: true, b: false, c: undefined},\n)");
        assert_eq!(s.args.len(), 1);
    }

    #[test]
    fn numbers() {
        let s =
            one("db.a.find({a: -5, b: 1.5, c: 1e3, d: 0x10, e: -0.5e-2, f: 9223372036854775808})");
        let a = &s.args[0];
        assert_eq!(a.get("a"), Some(&ShellValue::Int(-5)));
        assert_eq!(a.get("b"), Some(&ShellValue::Double(1.5)));
        assert_eq!(a.get("c"), Some(&ShellValue::Double(1000.0)));
        assert_eq!(a.get("d"), Some(&ShellValue::Int(16)));
        assert_eq!(a.get("e"), Some(&ShellValue::Double(-0.005)));
        assert_eq!(a.get("f"), Some(&ShellValue::Double(9.223372036854776e18)));
    }

    #[test]
    fn shell_constructors() {
        let s = one(
            "db.a.insertOne({_id: ObjectId(\"507f1f77bcf86cd799439011\"), d: ISODate(\"2024-01-02T03:04:05Z\"), \
             e: new Date(\"2020-01-01\"), f: new Date(1700000000000), g: NumberInt(5), h: NumberLong(\"9007199254740993\"), \
             i: NumberDecimal(\"1.10\"), j: UUID(\"123e4567-e89b-12d3-a456-426614174000\"), k: BinData(0, \"AQID\"), \
             l: Timestamp(10, 2), m: MinKey(), n: MaxKey(), o: /ab+c/i, p: new Date()})",
        );
        let a = &s.args[0];
        assert_eq!(
            a.get("_id"),
            Some(&ShellValue::ObjectId("507f1f77bcf86cd799439011".into()))
        );
        assert_eq!(
            a.get("d"),
            Some(&ShellValue::Date("2024-01-02T03:04:05Z".into()))
        );
        assert_eq!(a.get("e"), Some(&ShellValue::Date("2020-01-01".into())));
        assert_eq!(a.get("f"), Some(&ShellValue::Date("1700000000000".into())));
        assert_eq!(a.get("g"), Some(&ShellValue::NumberInt(5)));
        assert_eq!(
            a.get("h"),
            Some(&ShellValue::NumberLong(9_007_199_254_740_993))
        );
        assert_eq!(a.get("i"), Some(&ShellValue::NumberDecimal("1.10".into())));
        assert_eq!(
            a.get("j"),
            Some(&ShellValue::Uuid(
                "123e4567-e89b-12d3-a456-426614174000".into()
            ))
        );
        assert_eq!(
            a.get("k"),
            Some(&ShellValue::BinData {
                subtype: 0,
                base64: "AQID".into()
            })
        );
        assert_eq!(a.get("l"), Some(&ShellValue::Timestamp { t: 10, i: 2 }));
        assert_eq!(a.get("m"), Some(&ShellValue::MinKey));
        assert_eq!(a.get("n"), Some(&ShellValue::MaxKey));
        assert_eq!(
            a.get("o"),
            Some(&ShellValue::Regex {
                pattern: "ab+c".into(),
                flags: "i".into()
            })
        );
        assert_eq!(a.get("p"), Some(&ShellValue::Date(String::new())));
    }

    #[test]
    fn string_escapes() {
        let s = one(r#"db.a.find({a: "x\ny\t\"q\" é \x41 😀", b: 'it\'s'})"#);
        assert_eq!(
            s.args[0].get("a"),
            Some(&ShellValue::String("x\ny\t\"q\" é A 😀".into()))
        );
        assert_eq!(s.args[0].get("b"), Some(&ShellValue::String("it's".into())));
    }

    #[test]
    fn regex_with_slash_in_class() {
        let s = one("db.a.find({p: /[/]x\\/y/gi})");
        assert_eq!(
            s.args[0].get("p"),
            Some(&ShellValue::Regex {
                pattern: "[/]x\\/y".into(),
                flags: "gi".into()
            })
        );
    }

    #[test]
    fn multiple_statements_and_comments() {
        let v = parse("// first\ndb.a.find()\n/* two */ db.b.drop();;\nuse x // trailing\n");
        // `db.b.drop()` follows a block comment on its own line: accepted.
        let v = v.unwrap();
        assert_eq!(v.len(), 3);
        assert_eq!(v[2].method, "x");
    }

    #[test]
    fn chain_on_next_line() {
        let s = one("db.a.find({})\n  .sort({a: 1})\n  .limit(1)");
        assert_eq!(s.chain.len(), 2);
        let v = parse("db.a.find({})\ndb.b.find({})").unwrap();
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn statement_ranges_are_character_offsets() {
        let v = parse("db.a.find({n: 'é😀'}); db.b.find()").unwrap();
        assert_eq!(v[0].range, TextRange { start: 0, end: 20 });
        assert_eq!(v[1].range.start, 22);
    }

    #[test]
    fn errors_have_positions() {
        for (text, pos) in [
            ("db.a.find({a: })", 14),
            ("db.a.find(", 10),
            ("var x = 1", 0),
            ("db.a.find({a: 1}) db.b.find()", 18),
            ("db.a", 4),
            ("db.a.find({a: 'x})", 14),
            ("db.a.find({a: foo})", 14),
            ("db.a.insertOne({_id: ObjectId(\"zz\")})", 21),
            ("db.getSiblingDB('x').a.find()", 22),
        ] {
            match parse(text) {
                Err(Error::Query {
                    position: Some(p), ..
                }) => assert_eq!(p as usize, pos, "{text}"),
                other => panic!("{text}: {other:?}"),
            }
        }
    }

    #[test]
    fn empty_script_is_empty() {
        assert!(parse("").unwrap().is_empty());
        assert!(parse(" ;; // x\n").unwrap().is_empty());
    }

    #[test]
    fn classify_reads_and_writes() {
        assert_eq!(class("db.a.find({})"), (C::Read, None));
        assert_eq!(class("db.a.aggregate([{$match: {}}])"), (C::Read, None));
        assert_eq!(class("db.a.aggregate([{$out: 'b'}])"), (C::Write, None));
        assert_eq!(
            class("db.a.aggregate([{$merge: {into: 'b'}}])"),
            (C::Write, None)
        );
        assert_eq!(class("db.a.countDocuments()"), (C::Read, None));
        assert_eq!(class("db.a.insertOne({})"), (C::Write, None));
        assert_eq!(class("db.a.insertMany([{}])"), (C::Write, None));
        assert_eq!(
            class("db.a.updateOne({a: 1}, {$set: {b: 2}})"),
            (C::Write, None)
        );
        assert_eq!(class("db.a.deleteOne({})"), (C::Write, None));
        assert_eq!(class("db.a.findOneAndDelete({})"), (C::Write, None));
        assert_eq!(class("db.a.createIndex({a: 1})"), (C::Ddl, None));
        assert_eq!(class("use x"), (C::Session, None));
        assert_eq!(class("show dbs"), (C::Read, None));
        assert_eq!(class("db.a.frobnicate()"), (C::Unknown, None));
    }

    #[test]
    fn classify_destructive() {
        assert_eq!(
            class("db.a.drop()"),
            (C::Ddl, Some(DestructiveReason::DropCollection))
        );
        assert_eq!(
            class("db.dropDatabase()"),
            (C::Ddl, Some(DestructiveReason::DropDatabase))
        );
        assert_eq!(
            class("db.a.deleteMany({})"),
            (C::Write, Some(DestructiveReason::DeleteManyAll))
        );
        assert_eq!(class("db.a.deleteMany({x: 1})"), (C::Write, None));
        assert_eq!(
            class("db.a.remove({})"),
            (C::Write, Some(DestructiveReason::DeleteManyAll))
        );
        assert_eq!(class("db.a.remove({}, {justOne: true})"), (C::Write, None));
        assert_eq!(class("db.a.remove({x: 1})"), (C::Write, None));
        assert_eq!(
            class("db.a.updateMany({}, {$set: {a: 1}})"),
            (C::Write, Some(DestructiveReason::UpdateManyAll))
        );
        assert_eq!(
            class("db.a.updateMany({a: 1}, {$set: {a: 1}})"),
            (C::Write, None)
        );
        assert_eq!(
            class("db.a.update({}, {$set: {a: 1}}, {multi: true})"),
            (C::Write, Some(DestructiveReason::UpdateManyAll))
        );
        assert_eq!(class("db.a.update({}, {$set: {a: 1}})"), (C::Write, None));
        assert_eq!(
            class("db.a.bulkWrite([{insertOne: {document: {}}}, {deleteMany: {filter: {}}}])"),
            (C::Write, Some(DestructiveReason::DeleteManyAll))
        );
        assert_eq!(
            class("db.a.bulkWrite([{updateMany: {filter: {}, update: {}}}])"),
            (C::Write, Some(DestructiveReason::UpdateManyAll))
        );
        assert_eq!(
            class("db.runCommand({drop: 'a'})"),
            (C::Ddl, Some(DestructiveReason::DropCollection))
        );
        assert_eq!(
            class("db.runCommand({dropDatabase: 1})"),
            (C::Ddl, Some(DestructiveReason::DropDatabase))
        );
        assert_eq!(
            class("db.runCommand({delete: 'a', deletes: [{q: {}, limit: 0}]})"),
            (C::Write, Some(DestructiveReason::DeleteManyAll))
        );
        assert_eq!(
            class("db.runCommand({delete: 'a', deletes: [{q: {a: 1}, limit: 0}]})"),
            (C::Write, None)
        );
    }

    #[test]
    fn classify_commands_and_explain() {
        assert_eq!(class("db.runCommand({ping: 1})"), (C::Read, None));
        assert_eq!(
            class("db.adminCommand({listDatabases: 1})"),
            (C::Read, None)
        );
        assert_eq!(class("db.runCommand({shutdown: 1})"), (C::Procedural, None));
        assert_eq!(class("db.runCommand(1)"), (C::Procedural, None));
        assert_eq!(
            class("db.a.find({}).explain()"),
            (
                C::Explain {
                    analyze: false,
                    writes: false
                },
                None
            )
        );
        assert_eq!(
            class("db.a.find({}).explain('executionStats')"),
            (
                C::Explain {
                    analyze: true,
                    writes: false
                },
                None
            )
        );
        assert_eq!(class("db.getCollectionNames()"), (C::Read, None));
        assert_eq!(class("db.createCollection('x')"), (C::Ddl, None));
    }

    #[test]
    fn verbs() {
        assert_eq!(verb(&one("db.users.find()")), "db.users.find");
        assert_eq!(verb(&one("db.runCommand({})")), "db.runCommand");
        assert_eq!(verb(&one("use a")), "use");
        assert_eq!(verb(&one("show dbs")), "show dbs");
    }
}
