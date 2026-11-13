//! Removal of secrets from text before it is stored in the query history.
//!
//! SQL: the literal after `PASSWORD [=]`, `PASSWORD('..')`, `OLD_PASSWORD =`,
//! `IDENTIFIED BY` (and Oracle's `REPLACE`) becomes `'***'`. Redis: `AUTH`, `HELLO .. AUTH`,
//! `MIGRATE .. AUTH/AUTH2`, `CONFIG SET requirepass|masterauth`, `ACL SETUSER` password rules.
//! Mongo: `pwd:` / `password:` values, `db.auth(user, pwd)`, `db.changeUserPassword(user, pwd)`.
//! Everything else is returned untouched.

use dbstudio_core::ScriptDialect;

use crate::lexer::{Lexer, TokenKind};
use crate::redis_cli;

const MASK: &str = "'***'";

/// Replace secrets in `text` with `'***'`.
pub fn redact(d: ScriptDialect, text: &str) -> String {
    match d {
        ScriptDialect::RedisCli => redact_redis(text),
        ScriptDialect::MongoShell => redact_mongo(text),
        ScriptDialect::Opaque => text.to_string(),
        _ => redact_sql(d, text),
    }
}

struct T {
    kind: TokenKind,
    start: usize,
    end: usize,
    up: String,
}

fn sig_tokens(d: ScriptDialect, chars: &[char]) -> Vec<T> {
    let lx = Lexer::new(chars, d);
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let (kind, e, _) = lx.next(i);
        let e = e.max(i + 1);
        if !kind.is_trivia() {
            let up = if matches!(kind, TokenKind::Word | TokenKind::Punct) {
                chars[i..e].iter().collect::<String>().to_uppercase()
            } else {
                String::new()
            };
            out.push(T {
                kind,
                start: i,
                end: e,
                up,
            });
        }
        i = e;
    }
    out
}

fn apply_with(chars: &[char], mut spans: Vec<(usize, usize, String)>) -> String {
    spans.sort_by_key(|s| (s.0, s.1));
    spans.dedup_by_key(|s| (s.0, s.1));
    let mut out = String::with_capacity(chars.len());
    let mut pos = 0;
    for (s, e, rep) in spans {
        if s < pos {
            continue;
        }
        out.extend(&chars[pos..s]);
        out.push_str(&rep);
        pos = e;
    }
    out.extend(&chars[pos..]);
    out
}

/// Hide the password of connection strings inside `text`: `password=...` / `pwd=...` /
/// `passwd=...` (libpq, ODBC, ADO.NET) and `scheme://user:pass@host` URIs. Returns `None` when
/// there is nothing to hide.
fn mask_connection_secrets(text: &str) -> Option<String> {
    let c: Vec<char> = text.chars().collect();
    let n = c.len();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut changed = false;
    let boundary = |i: usize| i == 0 || !(c[i - 1].is_alphanumeric() || c[i - 1] == '_');
    while i < n {
        // key = value
        if boundary(i) {
            for key in ["password", "passwd", "pwd"] {
                let k: Vec<char> = key.chars().collect();
                if i + k.len() <= n
                    && c[i..i + k.len()]
                        .iter()
                        .zip(&k)
                        .all(|(a, b)| a.eq_ignore_ascii_case(b))
                {
                    let mut j = i + k.len();
                    while j < n && c[j] == ' ' {
                        j += 1;
                    }
                    if j < n && c[j] == '=' {
                        j += 1;
                        while j < n && c[j] == ' ' {
                            j += 1;
                        }
                        // value: doubled quotes (inside an SQL literal), a quote, or bare
                        let start = j;
                        if c[j..].starts_with(&['\'', '\'']) {
                            j += 2;
                            while j < n && !c[j..].starts_with(&['\'', '\'']) {
                                j += 1;
                            }
                            j = (j + 2).min(n);
                        } else if j < n && matches!(c[j], '"' | '\'') {
                            let q = c[j];
                            j += 1;
                            while j < n && c[j] != q {
                                j += 1;
                            }
                            j = (j + 1).min(n);
                        } else {
                            while j < n
                                && !c[j].is_whitespace()
                                && !matches!(c[j], ';' | '\'' | '"')
                            {
                                j += 1;
                            }
                        }
                        if j > start {
                            out.extend(&c[i..start]);
                            out.push_str("***");
                            i = j;
                            changed = true;
                            break;
                        }
                    }
                }
            }
            if i >= n {
                break;
            }
        }
        // scheme://user:pass@host
        if c[i..].starts_with(&[':', '/', '/']) {
            let ustart = i + 3;
            let mut j = ustart;
            while j < n
                && !matches!(c[j], '/' | '?' | '#' | '@' | '\'' | '"')
                && !c[j].is_whitespace()
            {
                j += 1;
            }
            if j < n && c[j] == '@' {
                if let Some(colon) = c[ustart..j].iter().position(|x| *x == ':') {
                    out.extend(&c[i..ustart + colon + 1]);
                    out.push_str("***");
                    i = j;
                    changed = true;
                    continue;
                }
            }
        }
        out.push(c[i]);
        i += 1;
    }
    changed.then_some(out)
}

/// Replacement spans for the connection strings inside String tokens.
fn connection_spans(chars: &[char], t: &[T]) -> Vec<(usize, usize, String)> {
    t.iter()
        .filter(|x| x.kind == TokenKind::String)
        .filter_map(|x| {
            let text: String = chars[x.start..x.end].iter().collect();
            mask_connection_secrets(&text).map(|r| (x.start, x.end, r))
        })
        .collect()
}

fn is_str(t: Option<&T>) -> bool {
    t.is_some_and(|t| t.kind == TokenKind::String)
}

fn redact_sql(d: ScriptDialect, text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let t = sig_tokens(d, &chars);
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < t.len() {
        let x = &t[i];
        if x.kind != TokenKind::Word {
            i += 1;
            continue;
        }
        match x.up.as_str() {
            "PASSWORD" | "OLD_PASSWORD" | "NEW_PASSWORD" | "PASSWD" => {
                let mut j = i + 1;
                if t.get(j).is_some_and(|n| n.up == "FOR") {
                    // MySQL: SET PASSWORD FOR 'u'@'h' = 'x'
                    let limit = (j + 10).min(t.len());
                    while j < limit && t[j].up != "=" {
                        j += 1;
                    }
                }
                if t.get(j).is_some_and(|n| n.up == "=") {
                    j += 1;
                }
                if t.get(j).is_some_and(|n| n.up == "(") && j == i + 1 {
                    // PASSWORD('x')
                    j += 1;
                }
                if is_str(t.get(j)) {
                    spans.push((t[j].start, t[j].end));
                }
            }
            "IDENTIFIED" => {
                // IDENTIFIED [WITH plugin] BY <secret> | IDENTIFIED BY PASSWORD 'hash'
                let mut j = i + 1;
                if t.get(j).is_some_and(|n| n.up == "WITH") {
                    j += 2;
                    if t.get(j).is_some_and(|n| n.up == "AS") {
                        if is_str(t.get(j + 1)) {
                            spans.push((t[j + 1].start, t[j + 1].end));
                        }
                        i += 1;
                        continue;
                    }
                }
                if t.get(j).is_some_and(|n| n.up == "BY") {
                    j += 1;
                    if t.get(j).is_some_and(|n| n.up == "PASSWORD") {
                        j += 1;
                    }
                    secret_at(&t, j, &mut spans);
                    // Oracle: IDENTIFIED BY new REPLACE old
                    let mut k = j + 1;
                    if t.get(k).is_some_and(|n| n.up == "REPLACE") {
                        k += 1;
                        secret_at(&t, k, &mut spans);
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    let mut all: Vec<(usize, usize, String)> = connection_spans(&chars, &t);
    // A literal already hidden as a whole (PASSWORD '...') wins over partial masking.
    all.retain(|c| !spans.iter().any(|s| s.0 == c.0));
    all.extend(spans.into_iter().map(|(s, e)| (s, e, MASK.to_string())));
    apply_with(&chars, all)
}

fn secret_at(t: &[T], j: usize, spans: &mut Vec<(usize, usize)>) {
    if let Some(n) = t.get(j) {
        let ok = match n.kind {
            TokenKind::String | TokenKind::QuotedIdent => true,
            TokenKind::Word => !matches!(n.up.as_str(), "NULL" | "REPLACE" | "PASSWORD"),
            _ => false,
        };
        if ok {
            spans.push((n.start, n.end));
        }
    }
}

fn redact_redis(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (n, line) in text.split('\n').enumerate() {
        if n > 0 {
            out.push('\n');
        }
        out.push_str(&redact_redis_line(line));
    }
    out
}

fn redact_redis_line(line: &str) -> String {
    let spans = redis_cli::token_spans(line);
    let words: Vec<String> = spans
        .iter()
        .map(|&(s, e)| line[s..e].to_ascii_uppercase())
        .collect();
    let mut hide: Vec<usize> = Vec::new();
    match words.first().map(String::as_str) {
        Some("AUTH") => hide.extend(1..words.len()),
        Some("HELLO") => {
            if let Some(p) = words.iter().position(|w| w == "AUTH") {
                hide.extend(p + 1..(p + 3).min(words.len()));
            }
        }
        Some("MIGRATE") => {
            if let Some(p) = words.iter().position(|w| w == "AUTH2") {
                hide.extend(p + 1..(p + 3).min(words.len()));
            } else if let Some(p) = words.iter().position(|w| w == "AUTH") {
                hide.extend(p + 1..(p + 2).min(words.len()));
            }
        }
        Some("CONFIG") if words.get(1).map(String::as_str) == Some("SET") => {
            let mut k = 2;
            while k + 1 < words.len() {
                let name = words[k].trim_matches(|c| c == '"' || c == '\'').to_string();
                if matches!(name.as_str(), "REQUIREPASS" | "MASTERAUTH" | "MASTERUSER") {
                    hide.push(k + 1);
                }
                k += 2;
            }
        }
        Some("ACL") if words.get(1).map(String::as_str) == Some("SETUSER") => {
            for (k, w) in words.iter().enumerate().skip(3) {
                let raw = &line[spans[k].0..spans[k].1];
                if raw.starts_with('>')
                    || raw.starts_with('<')
                    || raw.starts_with('#')
                    || raw.starts_with('!')
                {
                    let _ = w;
                    hide.push(k);
                }
            }
        }
        _ => {}
    }
    let mut out = String::new();
    let mut pos = 0;
    for (k, &(s, e)) in spans.iter().enumerate() {
        if hide.contains(&k) {
            out.push_str(&line[pos..s]);
            out.push_str(MASK);
            pos = e;
        } else if let Some(r) = mask_connection_secrets(&line[s..e]) {
            out.push_str(&line[pos..s]);
            out.push_str(&r);
            pos = e;
        }
    }
    out.push_str(&line[pos..]);
    out
}

fn redact_mongo(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let t = sig_tokens(ScriptDialect::MongoShell, &chars);
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let key_is = |x: &T| -> bool {
        match x.kind {
            TokenKind::Word => matches!(x.up.as_str(), "PWD" | "PASSWORD"),
            TokenKind::String => {
                let s: String = chars[x.start + 1..x.end.saturating_sub(1).max(x.start + 1)]
                    .iter()
                    .collect();
                matches!(s.to_ascii_lowercase().as_str(), "pwd" | "password")
            }
            _ => false,
        }
    };
    for i in 0..t.len() {
        // pwd: "x"
        if key_is(&t[i]) && t.get(i + 1).is_some_and(|n| n.up == ":") && is_str(t.get(i + 2)) {
            spans.push((t[i + 2].start, t[i + 2].end));
        }
        // db.auth("u", "p") / db.changeUserPassword("u", "p")
        if t[i].kind == TokenKind::Word
            && matches!(t[i].up.as_str(), "AUTH" | "CHANGEUSERPASSWORD")
            && i > 0
            && t[i - 1].up == "."
            && t.get(i + 1).is_some_and(|n| n.up == "(")
            && is_str(t.get(i + 2))
            && t.get(i + 3).is_some_and(|n| n.up == ",")
            && is_str(t.get(i + 4))
        {
            spans.push((t[i + 4].start, t[i + 4].end));
        }
    }
    let mut all: Vec<(usize, usize, String)> = connection_spans(&chars, &t);
    all.retain(|c| !spans.iter().any(|s| s.0 == c.0));
    all.extend(spans.into_iter().map(|(s, e)| (s, e, MASK.to_string())));
    apply_with(&chars, all)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PG: ScriptDialect = ScriptDialect::Postgres;
    const MY: ScriptDialect = ScriptDialect::MySql {
        mariadb: false,
        backslash_escapes: true,
    };

    #[test]
    fn identity_without_secrets() {
        for d in [PG, MY, ScriptDialect::MsSql, ScriptDialect::Oracle] {
            let t = "select 'password' as password, 1 from t";
            assert_eq!(redact(d, t), t);
        }
        assert_eq!(redact(ScriptDialect::Opaque, "AUTH x"), "AUTH x");
        assert_eq!(redact(PG, ""), "");
    }

    #[test]
    fn postgres_passwords() {
        assert_eq!(
            redact(PG, "CREATE ROLE r LOGIN PASSWORD 'secret'"),
            "CREATE ROLE r LOGIN PASSWORD '***'"
        );
        assert_eq!(
            redact(PG, "alter user u with encrypted password 'it''s'"),
            "alter user u with encrypted password '***'"
        );
        assert_eq!(
            redact(PG, "ALTER ROLE r PASSWORD NULL"),
            "ALTER ROLE r PASSWORD NULL"
        );
        assert_eq!(
            redact(PG, "create role a password 'x'; create role b password 'y'"),
            "create role a password '***'; create role b password '***'"
        );
    }

    #[test]
    fn mysql_passwords() {
        assert_eq!(
            redact(MY, "CREATE USER 'u'@'h' IDENTIFIED BY 'secret'"),
            "CREATE USER 'u'@'h' IDENTIFIED BY '***'"
        );
        assert_eq!(
            redact(
                MY,
                "ALTER USER u IDENTIFIED WITH mysql_native_password BY 'secret'"
            ),
            "ALTER USER u IDENTIFIED WITH mysql_native_password BY '***'"
        );
        assert_eq!(
            redact(MY, "SET PASSWORD FOR 'u'@'h' = 'secret'"),
            "SET PASSWORD FOR 'u'@'h' = '***'"
        );
        assert_eq!(redact(MY, "SET PASSWORD = 'x'"), "SET PASSWORD = '***'");
        assert_eq!(
            redact(MY, "SET PASSWORD = PASSWORD('x')"),
            "SET PASSWORD = PASSWORD('***')"
        );
        assert_eq!(
            redact(MY, "CREATE USER u IDENTIFIED BY PASSWORD '*HASH'"),
            "CREATE USER u IDENTIFIED BY PASSWORD '***'"
        );
        assert_eq!(
            redact(MY, "ALTER USER u IDENTIFIED WITH x AS '*HASH'"),
            "ALTER USER u IDENTIFIED WITH x AS '***'"
        );
    }

    #[test]
    fn oracle_and_mssql_passwords() {
        let o = ScriptDialect::Oracle;
        assert_eq!(
            redact(o, "CREATE USER u IDENTIFIED BY secret"),
            "CREATE USER u IDENTIFIED BY '***'"
        );
        assert_eq!(
            redact(o, "ALTER USER u IDENTIFIED BY \"Pw 1\" REPLACE oldpw"),
            "ALTER USER u IDENTIFIED BY '***' REPLACE '***'"
        );
        let m = ScriptDialect::MsSql;
        assert_eq!(
            redact(
                m,
                "CREATE LOGIN l WITH PASSWORD = 'x' MUST_CHANGE, CHECK_EXPIRATION = ON"
            ),
            "CREATE LOGIN l WITH PASSWORD = '***' MUST_CHANGE, CHECK_EXPIRATION = ON"
        );
        assert_eq!(
            redact(
                m,
                "ALTER LOGIN l WITH PASSWORD = N'new' OLD_PASSWORD = N'old'"
            ),
            "ALTER LOGIN l WITH PASSWORD = '***' OLD_PASSWORD = '***'"
        );
    }

    #[test]
    fn comments_and_strings_are_not_secrets() {
        assert_eq!(
            redact(PG, "select 'PASSWORD ''x''' -- password 'y'"),
            "select 'PASSWORD ''x''' -- password 'y'"
        );
        assert_eq!(
            redact(PG, "/* password 'z' */ select 1"),
            "/* password 'z' */ select 1"
        );
    }

    #[test]
    fn redis_secrets() {
        let r = ScriptDialect::RedisCli;
        assert_eq!(redact(r, "AUTH hunter2"), "AUTH '***'");
        assert_eq!(redact(r, "auth user hunter2"), "auth '***' '***'");
        assert_eq!(
            redact(r, "HELLO 3 AUTH user pw SETNAME x"),
            "HELLO 3 AUTH '***' '***' SETNAME x"
        );
        assert_eq!(
            redact(r, "CONFIG SET requirepass \"a b\""),
            "CONFIG SET requirepass '***'"
        );
        assert_eq!(
            redact(r, "config set maxmemory 1 masterauth x"),
            "config set maxmemory 1 masterauth '***'"
        );
        assert_eq!(
            redact(r, "MIGRATE h 6379 k 0 100 AUTH2 u p KEYS k"),
            "MIGRATE h 6379 k 0 100 AUTH2 '***' '***' KEYS k"
        );
        assert_eq!(
            redact(r, "ACL SETUSER bob on >pw1 ~* +@all"),
            "ACL SETUSER bob on '***' ~* +@all"
        );
        assert_eq!(
            redact(r, "GET a\nAUTH x\nSET k v"),
            "GET a\nAUTH '***'\nSET k v"
        );
        assert_eq!(
            redact(r, "CONFIG GET requirepass"),
            "CONFIG GET requirepass"
        );
        assert_eq!(redact(r, "SET auth x"), "SET auth x");
    }

    #[test]
    fn mongo_secrets() {
        let m = ScriptDialect::MongoShell;
        assert_eq!(
            redact(m, "db.createUser({user: 'u', pwd: 'secret', roles: []})"),
            "db.createUser({user: 'u', pwd: '***', roles: []})"
        );
        assert_eq!(
            redact(m, "db.createUser({\"password\": \"x\"})"),
            "db.createUser({\"password\": '***'})"
        );
        assert_eq!(redact(m, "db.auth('u', 'p')"), "db.auth('u', '***')");
        assert_eq!(
            redact(m, "db.changeUserPassword(\"u\", \"p\")"),
            "db.changeUserPassword(\"u\", '***')"
        );
        assert_eq!(redact(m, "db.a.find({pwd: 1})"), "db.a.find({pwd: 1})");
    }

    #[test]
    fn unicode_is_preserved() {
        assert_eq!(
            redact(PG, "select 'é😀'; create role r password 'é😀'"),
            "select 'é😀'; create role r password '***'"
        );
    }

    #[test]
    fn connection_strings_inside_literals() {
        assert_eq!(
            redact(
                PG,
                "SELECT dblink_connect('host=h user=u password=secret dbname=d')"
            ),
            "SELECT dblink_connect('host=h user=u password=*** dbname=d')"
        );
        assert_eq!(
            redact(
                PG,
                "CREATE SUBSCRIPTION s CONNECTION 'host=h password=''a b'' port=5432' PUBLICATION p"
            ),
            "CREATE SUBSCRIPTION s CONNECTION 'host=h password=*** port=5432' PUBLICATION p"
        );
        assert_eq!(
            redact(
                ScriptDialect::MsSql,
                "SELECT * FROM OPENROWSET('SQLNCLI', 'Server=s;Uid=u;Pwd=Secret!;', 'select 1')"
            ),
            "SELECT * FROM OPENROWSET('SQLNCLI', 'Server=s;Uid=u;Pwd=***;', 'select 1')"
        );
        assert_eq!(
            redact(MY, "SELECT 'DRIVER={x};PASSWORD = \"p w\";UID=u'"),
            "SELECT 'DRIVER={x};PASSWORD = ***;UID=u'"
        );
        assert_eq!(
            redact(
                PG,
                "SELECT dblink_connect('postgres://user:pa55@host:5432/db?sslmode=require')"
            ),
            "SELECT dblink_connect('postgres://user:***@host:5432/db?sslmode=require')"
        );
        // no password in the URI: untouched
        assert_eq!(
            redact(
                PG,
                "SELECT 'postgres://user@host/db', 'https://example.com/a:b@c'"
            ),
            "SELECT 'postgres://user@host/db', 'https://example.com/a:b@c'"
        );
        // words that merely contain the key are not secrets
        assert_eq!(
            redact(PG, "SELECT 'mypassword=1', 'nopwd=2'"),
            "SELECT 'mypassword=1', 'nopwd=2'"
        );
        // a PASSWORD literal is hidden whole, not partially
        assert_eq!(
            redact(PG, "ALTER ROLE r PASSWORD 'password=abc'"),
            "ALTER ROLE r PASSWORD '***'"
        );
    }

    #[test]
    fn connection_strings_in_mongo_and_redis() {
        assert_eq!(
            redact(
                ScriptDialect::MongoShell,
                "db.getMongo().connect('mongodb://admin:hunter2@h:27017/x')"
            ),
            "db.getMongo().connect('mongodb://admin:***@h:27017/x')"
        );
        assert_eq!(
            redact(ScriptDialect::RedisCli, "SET url 'redis://u:pw@h:6379/0'"),
            "SET url 'redis://u:***@h:6379/0'"
        );
        assert_eq!(
            redact(ScriptDialect::RedisCli, "SET url redis://h:6379"),
            "SET url redis://h:6379"
        );
    }

    #[test]
    fn every_listed_secret_form_is_covered() {
        let cases: [(ScriptDialect, &str, &str); 7] = [
            (
                MY,
                "CREATE USER u IDENTIFIED BY 'p'",
                "CREATE USER u IDENTIFIED BY '***'",
            ),
            (
                PG,
                "CREATE USER u PASSWORD 'p'",
                "CREATE USER u PASSWORD '***'",
            ),
            (MY, "SET PASSWORD = 'p'", "SET PASSWORD = '***'"),
            (
                PG,
                "CREATE USER u WITH PASSWORD 'p'",
                "CREATE USER u WITH PASSWORD '***'",
            ),
            (
                PG,
                "ALTER ROLE r WITH LOGIN PASSWORD 'p'",
                "ALTER ROLE r WITH LOGIN PASSWORD '***'",
            ),
            (
                ScriptDialect::MsSql,
                "CREATE LOGIN l WITH PASSWORD = 'p'",
                "CREATE LOGIN l WITH PASSWORD = '***'",
            ),
            (
                ScriptDialect::Oracle,
                "ALTER USER u IDENTIFIED BY p",
                "ALTER USER u IDENTIFIED BY '***'",
            ),
        ];
        for (d, input, want) in cases {
            assert_eq!(redact(d, input), want);
        }
    }
}
