//! Last line of defence against secrets in log output: masks passwords in connection-string and
//! `key=value` shaped text. Commands never log secrets in the first place; this guards against a
//! dependency or an error message echoing a connection string.

use std::borrow::Cow;
use std::io;
use std::sync::LazyLock;

use regex::Regex;
use tracing_subscriber::fmt::MakeWriter;

const MASK: &str = "***";

// scheme://user:PASSWORD@host. The password ends at the FIRST `@` and cannot contain `/`, `?` or
// `#`, so `postgres://u:pw@host/db?x=a@b.com` only loses `pw`, and URLs with a port but no
// userinfo (`http://localhost:8080/api?user=a@b.com`) never match.
static URI: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"([A-Za-z][A-Za-z0-9+.\-]*://[^\s:/@?#]*:)[^\s/?#@]*@").unwrap());

// `name<sep>value` where the value is a bare word, a quoted string ('' escapes a quote), a
// Debug-escaped string (`\"..\"`) or `Some(..)`. Whether the name is sensitive is decided in
// code so that identifiers like `MYSQL_PWD`, `sslpassword` or `savePassword` are all seen whole.
static PAIR: LazyLock<Regex> = LazyLock::new(|| {
    let quoted = r#"\\"(?:[^\\]|\\[^"])*\\"|"(?:[^"\\]|\\.)*"|'(?:[^']|'')*'|Some\((?:"(?:[^"\\]|\\.)*"|[^)]*)\)"#;
    // `=` accepts any value; `:` only a quoted / `Some(..)` value, so prose like
    // `error: password: authentication failed` is never a match.
    let pattern = format!(
        r#"([A-Za-z_][A-Za-z0-9_]*)(["']?)(?:(\s*=\s*)({quoted}|[^\s,;&)"']+)|(\s*:\s*)({quoted}))"#
    );
    Regex::new(&pattern).unwrap()
});

const SENSITIVE_SUFFIXES: [&str; 5] = ["password", "passwd", "pwd", "secret", "token"];

fn is_sensitive_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n == "pass" || n.ends_with("_pass") || SENSITIVE_SUFFIXES.iter().any(|s| n.ends_with(s))
}

/// Values that are flags or placeholders rather than secrets (`has_password=true`).
fn is_plain_flag(value: &str) -> bool {
    let inner = value
        .strip_prefix("Some(")
        .and_then(|v| v.strip_suffix(')'))
        .unwrap_or(value)
        .trim();
    matches!(
        inner.to_ascii_lowercase().as_str(),
        "true" | "false" | "null" | "none" | "***" | ""
    )
}

fn mask_value(value: &str) -> String {
    if value.starts_with("\\\"") {
        format!("\\\"{MASK}\\\"")
    } else if value.starts_with('"') {
        format!("\"{MASK}\"")
    } else if value.starts_with('\'') {
        format!("'{MASK}'")
    } else if value.starts_with("Some(") {
        format!("Some({MASK})")
    } else {
        MASK.to_string()
    }
}

/// Mask passwords in `text`; returns the input unchanged (borrowed) when nothing was masked.
pub fn redact(text: &str) -> Cow<'_, str> {
    let step1 = URI.replace_all(text, format!("${{1}}{MASK}@").as_str());
    // Manual scan instead of `replace_all`: after a match that is NOT masked, rescanning starts
    // at its value, so a password inside a larger quoted message (`err: "x password=\"y\""`)
    // is still found.
    let mut step2 = String::with_capacity(step1.len());
    let (mut copied, mut pos) = (0, 0);
    while let Some(caps) = PAIR.captures_at(&step1, pos) {
        let whole = caps.get(0).expect("group 0 always exists");
        let value = caps
            .get(4)
            .or_else(|| caps.get(6))
            .expect("one value branch always matches");
        if is_sensitive_name(&caps[1]) && !is_plain_flag(value.as_str()) {
            step2.push_str(&step1[copied..value.start()]);
            step2.push_str(&mask_value(value.as_str()));
            copied = value.end();
            pos = value.end();
        } else {
            pos = value.start().max(whole.start() + 1);
        }
    }
    step2.push_str(&step1[copied..]);
    if step2 == text {
        Cow::Borrowed(text)
    } else {
        Cow::Owned(step2)
    }
}

/// Wraps a [`MakeWriter`] so everything written through it is redacted. The fmt layer writes one
/// formatted event per `write` call, so patterns are never split across calls.
#[derive(Debug, Clone)]
pub struct Redacting<M>(pub M);

/// Writer produced by [`Redacting`].
#[derive(Debug)]
pub struct RedactingWriter<W>(W);

impl<W: io::Write> io::Write for RedactingWriter<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let text = String::from_utf8_lossy(data);
        match redact(&text) {
            Cow::Borrowed(_) => self.0.write_all(data)?,
            Cow::Owned(masked) => self.0.write_all(masked.as_bytes())?,
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl<'a, M: MakeWriter<'a>> MakeWriter<'a> for Redacting<M> {
    type Writer = RedactingWriter<M::Writer>;

    fn make_writer(&'a self) -> Self::Writer {
        RedactingWriter(self.0.make_writer())
    }

    fn make_writer_for(&'a self, meta: &tracing::Metadata<'_>) -> Self::Writer {
        RedactingWriter(self.0.make_writer_for(meta))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_connection_strings_and_pairs() {
        let cases = [
            (
                "postgres://admin:s3cr3t@db.local:5432/app",
                "postgres://admin:***@db.local:5432/app",
            ),
            ("mongodb+srv://u:pw@cluster", "mongodb+srv://u:***@cluster"),
            ("redis://:pw@host:6379/0", "redis://:***@host:6379/0"),
            (
                "postgres://u:pw@host/db?x=a@b.com",
                "postgres://u:***@host/db?x=a@b.com",
            ),
            (
                "connect failed: password=hunter2 host=x",
                "connect failed: password=*** host=x",
            ),
            ("PGPASSWORD=hunter2 psql", "PGPASSWORD=*** psql"),
            ("MYSQL_PWD=hunter2 mysql", "MYSQL_PWD=*** mysql"),
            (
                "sslpassword=hunter2&sslmode=require",
                "sslpassword=***&sslmode=require",
            ),
            ("host=h;uid=u;pwd=abc;db=d", "host=h;uid=u;pwd=***;db=d"),
            ("opts pass=hunter2 next", "opts pass=*** next"),
            ("api secret=abc token=def", "api secret=*** token=***"),
            ("Password = \"a b\"; user=x", "Password = \"***\"; user=x"),
            ("password='it''s fine' rest", "password='***' rest"),
            ("pwd: 'two words' next", "pwd: '***' next"),
            // Debug output
            (
                r#"Opts { user: Some("u"), pass: Some("hunter2"), db: None }"#,
                r#"Opts { user: Some("u"), pass: Some(***), db: None }"#,
            ),
            (
                r#"cfg password: Some("hunter2")"#,
                r#"cfg password: Some(***)"#,
            ),
            (
                r#"err: "connect password=\"hunter2 x\" failed""#,
                r#"err: "connect password=\"***\" failed""#,
            ),
            (
                r#"{"user":"u","password":"a\"b","x":1}"#,
                r#"{"user":"u","password":"***","x":1}"#,
            ),
            (
                "a postgres://u:one@h and mysql://v:two@g",
                "a postgres://u:***@h and mysql://v:***@g",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(redact(input), expected, "input: {input}");
        }
    }

    #[test]
    fn ordinary_lines_are_untouched() {
        for line in [
            "session opened session=1 connection=2 kind=postgres host=localhost user=admin",
            "authentication failed: password required",
            "error: password: authentication failed",
            "has_password=true save_password=false",
            "savePassword: true",
            "has_password: Some(false) token=None",
            "listening on http://127.0.0.1:1421",
            "http://localhost:8080/api?user=a@b.com",
            "postgres://admin@db.local/app",
            "user@example.com sent mail",
            "max_tokens=100 bypass=1",
            "unicode ünï 日本 text",
        ] {
            assert!(
                matches!(redact(line), Cow::Borrowed(_)),
                "{line} -> {}",
                redact(line)
            );
        }
    }

    #[test]
    fn writer_masks_output() {
        use std::io::Write;
        use std::sync::{Arc, Mutex};
        #[derive(Clone, Default)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl Write for Buf {
            fn write(&mut self, d: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(d);
                Ok(d.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let buf = Buf::default();
        let inner = buf.clone();
        let make = Redacting(move || inner.clone());
        let mut w = make.make_writer();
        w.write_all(b"dsn postgres://u:topsecret@h/db\n").unwrap();
        let text = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert_eq!(text, "dsn postgres://u:***@h/db\n");
    }
}
