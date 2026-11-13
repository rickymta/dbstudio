//! A small JSON reader that keeps what `serde_json::Value` throws away: the order of object
//! members, *duplicate* keys (MariaDB emits several `"table"` members in one object) and the
//! line range of every value (so plan nodes can point back into the raw text).

use std::fmt::Write as _;

/// A JSON value with its position (0-based, inclusive line range) in the source text.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Js {
    pub val: JVal,
    pub start: u32,
    pub end: u32,
}

/// JSON value kinds. Objects are an ordered list so duplicate keys survive.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum JVal {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Js>),
    Obj(Vec<(String, Js)>),
}

impl Js {
    /// First member named `key`.
    pub fn get(&self, key: &str) -> Option<&Js> {
        match &self.val {
            JVal::Obj(m) => m.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// First member whose name equals `key` ignoring ASCII case (Oracle column names).
    pub fn get_ci(&self, key: &str) -> Option<&Js> {
        match &self.val {
            JVal::Obj(m) => m
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| v),
            _ => None,
        }
    }

    /// Members of an object.
    pub fn members(&self) -> &[(String, Js)] {
        match &self.val {
            JVal::Obj(m) => m,
            _ => &[],
        }
    }

    /// Items of an array.
    pub fn items(&self) -> &[Js] {
        match &self.val {
            JVal::Arr(a) => a,
            _ => &[],
        }
    }

    pub fn is_obj(&self) -> bool {
        matches!(self.val, JVal::Obj(_))
    }

    pub fn as_str(&self) -> Option<&str> {
        match &self.val {
            JVal::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match &self.val {
            JVal::Bool(b) => Some(*b),
            JVal::Str(s) => match s.as_str() {
                "true" | "TRUE" | "YES" | "yes" => Some(true),
                "false" | "FALSE" | "NO" | "no" => Some(false),
                _ => None,
            },
            _ => None,
        }
    }

    /// Number, also from a numeric string (MySQL prints costs as strings) and from the
    /// MongoDB extended JSON wrappers (`{"$numberLong": "5"}`).
    pub fn as_f64(&self) -> Option<f64> {
        match &self.val {
            JVal::Num(n) => Some(*n),
            JVal::Str(s) => s.trim().parse::<f64>().ok().filter(|n| n.is_finite()),
            JVal::Obj(m) if m.len() == 1 => match m[0].0.as_str() {
                "$numberInt" | "$numberLong" | "$numberDouble" | "$numberDecimal" => {
                    m[0].1.as_f64()
                }
                _ => None,
            },
            _ => None,
        }
    }

    /// Member `key` as a number.
    pub fn num(&self, key: &str) -> Option<f64> {
        self.get(key).and_then(Js::as_f64)
    }

    /// Member `key` as a string slice.
    pub fn text(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(Js::as_str)
    }

    /// Short human text of a scalar (or a compact rendering of a container).
    pub fn display(&self) -> String {
        match &self.val {
            JVal::Null => "null".into(),
            JVal::Bool(b) => b.to_string(),
            JVal::Num(n) => fmt_num(*n),
            JVal::Str(s) => s.clone(),
            JVal::Arr(_) | JVal::Obj(_) => self.compact(),
        }
    }

    /// Compact JSON rendering (`{"a":1,"b":[2]}`).
    pub fn compact(&self) -> String {
        let mut out = String::new();
        self.write_compact(&mut out);
        out
    }

    fn write_compact(&self, out: &mut String) {
        match &self.val {
            JVal::Null => out.push_str("null"),
            JVal::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            JVal::Num(n) => out.push_str(&fmt_num(*n)),
            JVal::Str(s) => {
                out.push('"');
                for c in s.chars() {
                    match c {
                        '"' => out.push_str("\\\""),
                        '\\' => out.push_str("\\\\"),
                        '\n' => out.push_str("\\n"),
                        c if (c as u32) < 0x20 => {
                            let _ = write!(out, "\\u{:04x}", c as u32);
                        }
                        c => out.push(c),
                    }
                }
                out.push('"');
            }
            JVal::Arr(a) => {
                out.push('[');
                for (i, v) in a.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    v.write_compact(out);
                }
                out.push(']');
            }
            JVal::Obj(m) => {
                out.push('{');
                for (i, (k, v)) in m.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    let _ = write!(out, "\"{k}\":");
                    v.write_compact(out);
                }
                out.push('}');
            }
        }
    }
}

/// Format a number without a trailing `.0` for whole values.
pub(crate) fn fmt_num(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        let s = format!("{n:.4}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// Parse `text`; the error is a message with the line.
pub(crate) fn parse(text: &str) -> Result<Js, String> {
    let mut p = Parser {
        b: text.as_bytes(),
        i: 0,
        line: 0,
        depth: 0,
    };
    p.ws();
    let v = p.value()?;
    p.ws();
    if p.i != p.b.len() {
        return Err(p.err("trailing characters after the JSON value"));
    }
    Ok(v)
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
    line: u32,
    depth: u32,
}

/// Deepest JSON nesting accepted; bounds the recursion of every later pass.
pub(crate) const MAX_DEPTH: u32 = 256;

impl Parser<'_> {
    fn err(&self, msg: &str) -> String {
        format!("{msg} (line {})", self.line + 1)
    }

    fn ws(&mut self) {
        while let Some(&c) = self.b.get(self.i) {
            match c {
                b'\n' => {
                    self.line += 1;
                    self.i += 1;
                }
                b' ' | b'\t' | b'\r' => self.i += 1,
                _ => break,
            }
        }
    }

    fn lit(&mut self, word: &str, val: JVal) -> Result<Js, String> {
        if self.b[self.i..].starts_with(word.as_bytes()) {
            let line = self.line;
            self.i += word.len();
            Ok(Js {
                val,
                start: line,
                end: line,
            })
        } else {
            Err(self.err("invalid JSON token"))
        }
    }

    fn value(&mut self) -> Result<Js, String> {
        match self.b.get(self.i) {
            None => Err(self.err("unexpected end of JSON")),
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => {
                let line = self.line;
                let s = self.string()?;
                Ok(Js {
                    val: JVal::Str(s),
                    start: line,
                    end: self.line,
                })
            }
            Some(b't') => self.lit("true", JVal::Bool(true)),
            Some(b'f') => self.lit("false", JVal::Bool(false)),
            Some(b'n') => self.lit("null", JVal::Null),
            Some(c) if *c == b'-' || c.is_ascii_digit() => self.number(),
            Some(_) => Err(self.err("unexpected character")),
        }
    }

    fn number(&mut self) -> Result<Js, String> {
        let s = self.i;
        while let Some(&c) = self.b.get(self.i) {
            if c.is_ascii_digit() || matches!(c, b'-' | b'+' | b'.' | b'e' | b'E') {
                self.i += 1;
            } else {
                break;
            }
        }
        let text = std::str::from_utf8(&self.b[s..self.i]).map_err(|_| self.err("bad number"))?;
        let n: f64 = text.parse().map_err(|_| self.err("bad number"))?;
        Ok(Js {
            val: JVal::Num(n),
            start: self.line,
            end: self.line,
        })
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let h = self
            .b
            .get(self.i..self.i + 4)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u32::from_str_radix(h, 16).ok())
            .ok_or_else(|| self.err("bad \\u escape"))?;
        self.i += 4;
        Ok(h)
    }

    fn string(&mut self) -> Result<String, String> {
        self.i += 1; // opening quote
        let mut out: Vec<u8> = Vec::new();
        loop {
            let Some(&c) = self.b.get(self.i) else {
                return Err(self.err("unterminated string"));
            };
            self.i += 1;
            match c {
                b'"' => break,
                b'\\' => {
                    let Some(&e) = self.b.get(self.i) else {
                        return Err(self.err("unterminated string"));
                    };
                    self.i += 1;
                    match e {
                        b'"' => out.push(b'"'),
                        b'\\' => out.push(b'\\'),
                        b'/' => out.push(b'/'),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'u' => {
                            let mut cp = self.hex4()?;
                            if (0xD800..0xDC00).contains(&cp)
                                && self.b[self.i..].starts_with(b"\\u")
                            {
                                self.i += 2;
                                let lo = self.hex4()?;
                                cp = 0x10000 + ((cp - 0xD800) << 10) + lo.wrapping_sub(0xDC00);
                            }
                            let ch = char::from_u32(cp).unwrap_or('\u{fffd}');
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                        }
                        _ => return Err(self.err("bad escape")),
                    }
                }
                b'\n' => {
                    // Not valid JSON, but tolerate raw newlines inside strings.
                    self.line += 1;
                    out.push(b'\n');
                }
                c => out.push(c),
            }
        }
        String::from_utf8(out).map_err(|_| self.err("invalid UTF-8 in string"))
    }

    fn array(&mut self) -> Result<Js, String> {
        let start = self.line;
        self.enter()?;
        self.i += 1;
        let mut items = Vec::new();
        self.ws();
        if self.b.get(self.i) == Some(&b']') {
            self.i += 1;
        } else {
            loop {
                self.ws();
                items.push(self.value()?);
                self.ws();
                match self.b.get(self.i) {
                    Some(b',') => self.i += 1,
                    Some(b']') => {
                        self.i += 1;
                        break;
                    }
                    _ => return Err(self.err("expected ',' or ']'")),
                }
            }
        }
        self.depth -= 1;
        Ok(Js {
            val: JVal::Arr(items),
            start,
            end: self.line,
        })
    }

    fn object(&mut self) -> Result<Js, String> {
        let start = self.line;
        self.enter()?;
        self.i += 1;
        let mut members = Vec::new();
        self.ws();
        if self.b.get(self.i) == Some(&b'}') {
            self.i += 1;
        } else {
            loop {
                self.ws();
                if self.b.get(self.i) != Some(&b'"') {
                    return Err(self.err("expected a member name"));
                }
                let key = self.string()?;
                self.ws();
                if self.b.get(self.i) != Some(&b':') {
                    return Err(self.err("expected ':'"));
                }
                self.i += 1;
                self.ws();
                members.push((key, self.value()?));
                self.ws();
                match self.b.get(self.i) {
                    Some(b',') => self.i += 1,
                    Some(b'}') => {
                        self.i += 1;
                        break;
                    }
                    _ => return Err(self.err("expected ',' or '}'")),
                }
            }
        }
        self.depth -= 1;
        Ok(Js {
            val: JVal::Obj(members),
            start,
            end: self.line,
        })
    }

    fn enter(&mut self) -> Result<(), String> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            Err(self.err("JSON nested too deeply"))
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_duplicate_keys_and_lines() {
        let v = parse("{\n \"a\": 1,\n \"t\": {\"x\": \"1\"},\n \"t\": [true,\n null]\n}").unwrap();
        assert_eq!(v.members().iter().filter(|(k, _)| k == "t").count(), 2);
        assert_eq!(v.num("a"), Some(1.0));
        let second = &v
            .members()
            .iter()
            .filter(|(k, _)| k == "t")
            .nth(1)
            .unwrap()
            .1;
        assert_eq!((second.start, second.end), (3, 4));
        assert_eq!((v.start, v.end), (0, 5));
    }

    #[test]
    fn numeric_strings_and_ejson() {
        let v = parse(r#"{"c":"27646.50","n":{"$numberLong":"12"},"e":1e3}"#).unwrap();
        assert_eq!(v.num("c"), Some(27646.5));
        assert_eq!(v.num("n"), Some(12.0));
        assert_eq!(v.num("e"), Some(1000.0));
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse("").is_err());
        assert!(parse("{\"a\":}").is_err());
        assert!(parse("[1,2] x").is_err());
        assert_eq!(parse(r#""café 😀""#).unwrap().as_str(), Some("café 😀"));
    }

    #[test]
    fn compact_roundtrip() {
        let v = parse(r#"{"a": [1, "x\"y", {"b": null}], "c": 2.5}"#).unwrap();
        assert_eq!(v.compact(), r#"{"a":[1,"x\"y",{"b":null}],"c":2.5}"#);
    }
}
