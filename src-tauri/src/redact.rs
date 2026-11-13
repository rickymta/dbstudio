//! Last line of defence against secrets in log output: masks passwords in connection-string and
//! `key=value` shaped text. Commands never log secrets in the first place; this guards against a
//! dependency or an error message echoing a connection string.

use std::borrow::Cow;
use std::io;
use std::sync::LazyLock;

use regex::Regex;
use tracing_subscriber::fmt::MakeWriter;

const MASK: &str = "***";

// scheme://user:PASSWORD@host (password may contain anything except whitespace; the greedy
// match ends at the last `@` of the token).
static URI: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"([A-Za-z][A-Za-z0-9+.\-]*://[^\s:/@]*:)[^\s]*@").unwrap());
// password=..., pwd: ..., PGPASSWORD=... with a bare, single- or double-quoted value.
static KEY_VALUE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)(\b(?:password|passwd|pwd|pgpassword)\s*[=:]\s*)("[^"]*"|'[^']*'|[^\s,;&"']+)"#,
    )
    .unwrap()
});
// JSON: "password":"..."
static JSON_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)("(?:password|passwd|pwd)"\s*:\s*)"(?:[^"\\]|\\.)*""#).unwrap()
});

/// Mask passwords in `text`; returns the input unchanged (borrowed) when nothing matched.
pub fn redact(text: &str) -> Cow<'_, str> {
    let mut out = Cow::Borrowed(text);
    let uri = format!("${{1}}{MASK}@");
    let kv = format!("${{1}}{MASK}");
    let json = format!("${{1}}\"{MASK}\"");
    for (re, replacement) in [(&*URI, &uri), (&*KEY_VALUE, &kv), (&*JSON_KEY, &json)] {
        if let Cow::Owned(replaced) = re.replace_all(&out, replacement.as_str()) {
            out = Cow::Owned(replaced);
        }
    }
    out
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
            (
                "url=mysql://root:p@ss:w/rd@host/db ok",
                "url=mysql://root:***@host/db ok",
            ),
            ("mongodb+srv://u:pw@cluster", "mongodb+srv://u:***@cluster"),
            (
                "connect failed: password=hunter2 host=x",
                "connect failed: password=*** host=x",
            ),
            ("PGPASSWORD=hunter2 psql", "PGPASSWORD=*** psql"),
            ("pwd: 'two words' next", "pwd: *** next"),
            ("Password = \"a b\"; user=x", "Password = ***; user=x"),
            ("host=h;uid=u;pwd=abc;db=d", "host=h;uid=u;pwd=***;db=d"),
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
            "has_password=true save_password=false",
            "savePassword: true",
            "listening on http://127.0.0.1:1421",
            "postgres://admin@db.local/app",
            "user@example.com sent mail",
            "unicode ünï 日本 text",
        ] {
            assert!(matches!(redact(line), Cow::Borrowed(_)), "{line}");
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
