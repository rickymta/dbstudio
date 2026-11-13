//! Differential test: the PostgreSQL splitter of `dbstudio-sql` against a verbatim copy of the
//! legacy splitter of the PostgreSQL driver (`crates/drivers/postgres/src/sql.rs`).

use dbstudio_core::ScriptDialect;
use proptest::prelude::*;

// ------------------------------------------------------------------------------------------
// Legacy implementation (copied, only the splitting part).
// ------------------------------------------------------------------------------------------

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

fn mask(sql: &str) -> String {
    let b = sql.as_bytes();
    let n = b.len();
    let mut out = b.to_vec();
    let mut i = 0;
    let blank = |out: &mut [u8], from: usize, to: usize, with: u8| {
        for byte in &mut out[from..to] {
            if *byte != b'\n' {
                *byte = with;
            }
        }
    };
    while i < n {
        match b[i] {
            b'\'' => {
                let escape =
                    i > 0 && matches!(b[i - 1], b'e' | b'E') && (i < 2 || !is_ident_byte(b[i - 2]));
                let start = i;
                i += 1;
                while i < n {
                    if escape && b[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    if b[i] == b'\'' {
                        if i + 1 < n && b[i + 1] == b'\'' {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                let end = i.min(n);
                blank(&mut out, start, end, b'_');
            }
            b'"' => {
                let start = i;
                i += 1;
                while i < n {
                    if b[i] == b'"' {
                        if i + 1 < n && b[i + 1] == b'"' {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                let end = i.min(n);
                blank(&mut out, start, end, b'_');
            }
            b'-' if i + 1 < n && b[i + 1] == b'-' => {
                let start = i;
                while i < n && b[i] != b'\n' {
                    i += 1;
                }
                blank(&mut out, start, i, b' ');
            }
            b'/' if i + 1 < n && b[i + 1] == b'*' => {
                let start = i;
                let mut depth = 0usize;
                while i < n {
                    if i + 1 < n && b[i] == b'/' && b[i + 1] == b'*' {
                        depth += 1;
                        i += 2;
                    } else if i + 1 < n && b[i] == b'*' && b[i + 1] == b'/' {
                        depth -= 1;
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
                blank(&mut out, start, i.min(n), b' ');
            }
            b'$' if i == 0 || !is_ident_byte(b[i - 1]) => {
                let mut j = i + 1;
                while j < n && (b[j].is_ascii_alphanumeric() || b[j] == b'_' || b[j] >= 0x80) {
                    j += 1;
                }
                let tag_ok = j < n && b[j] == b'$' && !(j > i + 1 && b[i + 1].is_ascii_digit());
                if tag_ok {
                    let delim = &sql[i..=j];
                    let end = match sql[j + 1..].find(delim) {
                        Some(p) => j + 1 + p + delim.len(),
                        None => n,
                    };
                    blank(&mut out, i, end, b'_');
                    i = end;
                } else {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn words(sql: &str) -> Vec<String> {
    mask(sql)
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .map(|w| w.to_ascii_uppercase())
        .collect()
}

fn open_atomic_body(stmt: &str) -> bool {
    let w = words(stmt);
    let mut it = w.iter().map(String::as_str);
    if it.next() != Some("CREATE") {
        return false;
    }
    let head: Vec<&str> = w.iter().take(6).map(String::as_str).collect();
    if !head.iter().any(|x| matches!(*x, "FUNCTION" | "PROCEDURE")) {
        return false;
    }
    let Some(atomic) = w
        .windows(2)
        .position(|p| p[0] == "BEGIN" && p[1] == "ATOMIC")
    else {
        return false;
    };
    let mut depth = 0i32;
    for x in &w[atomic..] {
        match x.as_str() {
            "BEGIN" | "CASE" => depth += 1,
            "END" => depth -= 1,
            _ => {}
        }
    }
    depth > 0
}

/// (text, char offset) of every statement.
fn legacy_split(sql: &str) -> Vec<(String, usize)> {
    let masked = mask(sql);
    let mut out = Vec::new();
    let mut start = 0;
    let mut push = |from: usize, to: usize| {
        if masked[from..to].trim().is_empty() {
            return;
        }
        out.push((sql[from..to].to_string(), sql[..from].chars().count()));
    };
    for (i, c) in masked.char_indices() {
        if c == ';' {
            if open_atomic_body(&masked[start..i]) {
                continue;
            }
            push(start, i);
            start = i + 1;
        }
    }
    push(start, sql.len());
    out
}

fn new_split(sql: &str) -> Vec<(String, usize)> {
    let chars: Vec<char> = sql.chars().collect();
    dbstudio_sql::split(ScriptDialect::Postgres, sql)
        .into_iter()
        .map(|r| {
            (
                chars[r.start as usize..r.end as usize].iter().collect(),
                r.start as usize,
            )
        })
        .collect()
}

fn check(sql: &str) {
    let old = legacy_split(sql);
    let new = new_split(sql);
    let old_trim: Vec<(String, usize)> = old
        .iter()
        .map(|(t, off)| {
            let lead = t.chars().take_while(|c| c.is_whitespace()).count();
            (t.trim().to_string(), off + lead)
        })
        .collect();
    assert_eq!(new, old_trim, "script: {sql:?}");
}

// ------------------------------------------------------------------------------------------
// Corpus
// ------------------------------------------------------------------------------------------

#[test]
fn corpus_cases() {
    let cases = [
        "select 1; select ';' ; -- c;\n select \"a;b\"; /* x; */ ;  ",
        "do $$ begin perform 1; end $$; select $a$;$a$; select 2",
        "select E'it\\'s;'; select 'it''s;'",
        "select 1;\nselect 'é'; select 3",
        "  -- nothing\n /* at all */ ; ",
        "select $1",
        "CREATE FUNCTION f(a int) RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT CASE WHEN a > 1 THEN 1 ELSE 2 END; SELECT a; END; SELECT 2; CREATE PROCEDURE p() BEGIN ATOMIC SELECT 1; END;",
        "select 1 /* ; /* nested ; */ ; */ ; select 2",
        "",
        ";;;",
        "select 'unterminated; select 2",
        "select $tag$ unterminated; select 2",
        "select \"unterminated; select 2",
        "create table t(a int);insert into t values (1);select * from t",
        "select '\n;\n'; select 2",
        "COPY t FROM stdin; select 1;",
        "select 1 -- trailing comment without newline",
        "select e'\\\\'; select 2",
        "select 1\r\n;\r\nselect 2\r\n",
        "begin; update t set a = 'x;y' where b = $q$;$q$; commit;",
    ];
    for c in cases {
        check(c);
    }
}

fn fragment() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("select 1".to_string()),
        Just("select a, b from t where c = 1".to_string()),
        Just(";".to_string()),
        Just(";".to_string()),
        Just("'a;b'".to_string()),
        Just("'it''s;'".to_string()),
        Just("E'a\\';b'".to_string()),
        Just("\"x;y\"".to_string()),
        Just("$$ a; b $$".to_string()),
        Just("$t$ ; $t$".to_string()),
        Just("$t$ $$ ; $t$".to_string()),
        Just("-- c;\n".to_string()),
        Just("-- only comment\n".to_string()),
        Just("/* ; */".to_string()),
        Just("/* /* ; */ ; */".to_string()),
        Just("\n".to_string()),
        Just("-- é;ñ\n".to_string()),
        Just("\"é;ñ\"".to_string()),
        Just("'é😀;'".to_string()),
        Just("begin atomic".to_string()),
        Just("create function f() returns int language sql begin atomic".to_string()),
        Just("create procedure p() begin atomic".to_string()),
        Just("end".to_string()),
        Just("case when a then b else c end".to_string()),
        Just("create table t (a int)".to_string()),
        Just("$1".to_string()),
        Just("x::int".to_string()),
        Just("do $$ begin null; end $$".to_string()),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(3000))]

    #[test]
    fn pg_matches_legacy_splitter(frags in proptest::collection::vec(fragment(), 0..10), seps in proptest::collection::vec(prop_oneof![Just(" "), Just("\n"), Just("  ")], 10)) {
        let mut sql = String::new();
        for (i, f) in frags.iter().enumerate() {
            sql.push_str(f);
            sql.push_str(seps[i % seps.len()]);
        }
        check(&sql);
        // Also without the trailing separator.
        check(sql.trim_end());
    }
}
