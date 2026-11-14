//! Fuzz: the CQL, ClickHouse and ANSI_QUOTES paths never panic and keep their invariants
//! (tokens tile the text, ranges stay inside the text and are ordered).

use dbstudio_core::ScriptDialect;
use dbstudio_sql::lexer::tokenize;
use dbstudio_sql::redact::redact;
use dbstudio_sql::{StreamSplitter, analyze, find_params, split, vocabulary};
use proptest::prelude::*;

const FRAGMENTS: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "INSERT INTO",
    "VALUES",
    "UPDATE",
    "SET",
    "DELETE",
    "DROP",
    "TRUNCATE",
    "ALTER TABLE",
    "BEGIN BATCH",
    "APPLY BATCH",
    "BEGIN UNLOGGED BATCH",
    "USE",
    "CREATE ROLE",
    "PASSWORD",
    "IDENTIFIED BY",
    "IDENTIFIED WITH x BY",
    "mysql(",
    "remote(",
    "'",
    "''",
    "\"",
    "`",
    "$$",
    "$t$",
    "\\",
    "\\'",
    "/*",
    "*/",
    "/*T!",
    "/*T![a_b]",
    "/*!50000",
    "--",
    "#",
    "//",
    ";",
    ",",
    "(",
    ")",
    "{",
    "}",
    "{id:UInt64}",
    ":a",
    "?",
    "=",
    "1",
    "0x1F",
    "é",
    "😀",
    " ",
    "\n",
    "\t",
    "t",
    "ks.t",
    "WHERE 1",
    "DETACH",
    "SYSTEM",
    "EXPLAIN",
    "WITH",
    "TTL",
    "UPSERT",
    "BATCH ON",
    "CANCEL",
    "SHOW RANGES",
];

fn dialects() -> Vec<ScriptDialect> {
    vec![
        ScriptDialect::Cql,
        ScriptDialect::ClickHouse,
        ScriptDialect::mysql(false, true),
        ScriptDialect::MySql {
            mariadb: false,
            backslash_escapes: true,
            ansi_quotes: true,
        },
        ScriptDialect::MySql {
            mariadb: true,
            backslash_escapes: false,
            ansi_quotes: true,
        },
        ScriptDialect::Postgres,
    ]
}

fn text() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            8 => prop::sample::select(FRAGMENTS).prop_map(str::to_string),
            1 => "\\PC{0,6}",
        ],
        0..24,
    )
    .prop_map(|v| v.join(" "))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    #[test]
    fn no_panic_and_invariants(src in text()) {
        let n = src.chars().count() as u32;
        for d in dialects() {
            let toks = tokenize(d, &src);
            let total: u32 = toks.iter().map(|t| t.range.end - t.range.start).sum();
            prop_assert_eq!(total, n);
            let mut prev = 0;
            for t in &toks {
                prop_assert_eq!(t.range.start, prev);
                prop_assert!(t.range.end > t.range.start);
                prev = t.range.end;
            }
            let mut last_end = 0;
            for r in split(d, &src) {
                prop_assert!(r.start < r.end && r.end <= n);
                prop_assert!(r.start >= last_end);
                last_end = r.end;
            }
            let a = analyze(d, &src);
            for s in &a.statements {
                prop_assert!(s.range.end <= n);
            }
            for p in find_params(d, &src) {
                for r in p.ranges {
                    prop_assert!(r.end <= n);
                }
            }
            let _ = redact(d, &src);
            let _ = vocabulary(d);
        }
    }

    #[test]
    fn streaming_gives_the_same_statements(src in text(), cut in 0usize..40) {
        for d in [ScriptDialect::Cql, ScriptDialect::ClickHouse] {
            let chars: Vec<char> = src.chars().collect();
            let at = cut.min(chars.len());
            let mut s = StreamSplitter::new(d);
            let first: String = chars[..at].iter().collect();
            let rest: String = chars[at..].iter().collect();
            let mut got = s.push(&first).len();
            got += s.push(&rest).len();
            got += s.finish().len();
            prop_assert_eq!(got, split(d, &src).len());
        }
    }
}
