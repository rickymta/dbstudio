//! Fuzz style property tests: nothing may panic and the structural invariants must hold for
//! arbitrary input in every dialect.

use dbstudio_core::{ParamInput, ParamValue, ScriptDialect, TextRange};
use dbstudio_sql::{
    StreamSplitter, analyze, bind, find_params, lexer, mongo_shell, redact, redis_cli, split,
    split::split_batches,
};
use proptest::prelude::*;

const DIALECTS: [ScriptDialect; 9] = [
    ScriptDialect::Postgres,
    ScriptDialect::MySql {
        mariadb: false,
        backslash_escapes: true,
    },
    ScriptDialect::MySql {
        mariadb: true,
        backslash_escapes: false,
    },
    ScriptDialect::MsSql,
    ScriptDialect::Oracle,
    ScriptDialect::MongoShell,
    ScriptDialect::RedisCli,
    ScriptDialect::Opaque,
    ScriptDialect::Postgres,
];

fn text() -> impl Strategy<Value = String> {
    prop_oneof![
        // arbitrary unicode
        ".{0,80}",
        // SQL-ish soup with the interesting characters over-represented
        proptest::collection::vec(
            prop_oneof![
                Just("select"),
                Just("delete from t"),
                Just("drop table x"),
                Just("begin"),
                Just("end"),
                Just("create function f() as"),
                Just("declare"),
                Just("delimiter $$\n"),
                Just("DELIMITER ;\n"),
                Just("\nGO\n"),
                Just("\nGO 3\n"),
                Just("\n/\n"),
                Just(";"),
                Just("'"),
                Just("\""),
                Just("`"),
                Just("["),
                Just("]"),
                Just("$$"),
                Just("$a$"),
                Just("/*"),
                Just("*/"),
                Just("--"),
                Just("#"),
                Just("//"),
                Just("\n"),
                Just(" "),
                Just(":x"),
                Just("::"),
                Just(":="),
                Just("?"),
                Just("@p"),
                Just("@@v"),
                Just("$1"),
                Just("q'["),
                Just("]'"),
                Just("E'"),
                Just("\\"),
                Just("("),
                Just(")"),
                Just("{"),
                Just("}"),
                Just("é"),
                Just("😀"),
                Just("db.a.find({a: 1})"),
                Just("use x"),
                Just("SET k v"),
                Just("FLUSHALL"),
                Just("AUTH pw"),
            ],
            0..24
        )
        .prop_map(|v| v.concat()),
    ]
}

fn check_ranges(chars: usize, ranges: &[TextRange], text: &str) {
    let c: Vec<char> = text.chars().collect();
    let mut prev_end = 0u32;
    for r in ranges {
        assert!(r.start < r.end, "empty range {r:?} in {text:?}");
        assert!(
            r.start >= prev_end,
            "overlap/unsorted {ranges:?} in {text:?}"
        );
        assert!((r.end as usize) <= chars, "out of bounds {r:?} in {text:?}");
        assert!(
            !c[r.start as usize].is_whitespace() && !c[r.end as usize - 1].is_whitespace(),
            "untrimmed {r:?} in {text:?}"
        );
        prev_end = r.end;
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1500))]

    #[test]
    fn lexer_tiles_and_never_panics(t in text()) {
        for d in DIALECTS {
            let toks = lexer::tokenize(d, &t);
            let mut pos = 0u32;
            for k in &toks {
                prop_assert_eq!(k.range.start, pos);
                prop_assert!(k.range.end > k.range.start);
                pos = k.range.end;
            }
            prop_assert_eq!(pos as usize, t.chars().count());
        }
    }

    #[test]
    fn split_ranges_are_well_formed(t in text()) {
        let n = t.chars().count();
        for d in DIALECTS {
            let r = split(d, &t);
            check_ranges(n, &r, &t);
            let b: Vec<TextRange> = split_batches(d, &t).into_iter().map(|u| u.range).collect();
            prop_assert_eq!(r, b);
        }
    }

    #[test]
    fn stream_splitter_equals_split(t in text(), chunk in 1usize..9) {
        for d in DIALECTS {
            let chars: Vec<char> = t.chars().collect();
            let mut want = Vec::new();
            for u in split_batches(d, &t) {
                let s: String = chars[u.range.start as usize..u.range.end as usize].iter().collect();
                for _ in 0..u.repeat.clamp(1, 10_000) {
                    want.push(s.clone());
                }
            }
            let mut s = StreamSplitter::new(d);
            let mut got = Vec::new();
            for c in chars.chunks(chunk) {
                let part: String = c.iter().collect();
                got.extend(s.push(&part));
            }
            got.extend(s.finish());
            let got: Vec<String> = got.into_iter().map(|x| x.trim().to_string()).collect();
            prop_assert_eq!(got, want, "dialect {:?} text {:?} chunk {}", d, t, chunk);
        }
    }

    #[test]
    fn analyze_never_panics_and_ranges_are_valid(t in text()) {
        let n = t.chars().count();
        for d in DIALECTS {
            let a = analyze(d, &t);
            let mut prev = 0u32;
            for (i, s) in a.statements.iter().enumerate() {
                prop_assert_eq!(s.index as usize, i);
                prop_assert!(s.range.start < s.range.end && (s.range.end as usize) <= n);
                prop_assert!(s.range.start >= prev, "{:?} {:?}", d, a.statements);
                prev = s.range.end;
            }
            if !a.problems.is_empty() {
                prop_assert!(a.statements.iter().all(|s| s.class == dbstudio_core::StatementClass::Unknown
                    || matches!(d, ScriptDialect::MongoShell | ScriptDialect::RedisCli | ScriptDialect::Opaque)
                    || s.class == dbstudio_core::StatementClass::Empty));
            }
        }
    }

    #[test]
    fn params_and_bind_never_panic(t in text(), v in ".{0,20}") {
        let n = t.chars().count() as u32;
        for d in DIALECTS {
            let refs = find_params(d, &t);
            let mut values = Vec::new();
            for r in &refs {
                for rg in &r.ranges {
                    prop_assert!(rg.start < rg.end && rg.end <= n);
                }
                values.push(ParamValue { name: r.name.clone(), value: ParamInput::Text(v.clone()) });
            }
            if let Ok(b) = bind(d, &t, &values) {
                let mut prev = 0;
                for pos in 0..=(b.text.chars().count() as u32) {
                    let o = b.map.to_original(pos);
                    prop_assert!(o <= n, "{o} > {n}");
                    prop_assert!(o >= prev, "not monotonic at {pos}");
                    prev = o;
                }
            }
        }
    }

    #[test]
    fn shell_parsers_never_panic(t in text()) {
        let _ = mongo_shell::parse(&t);
        for line in t.lines() {
            let _ = redis_cli::tokenize_line(line);
            let _ = redis_cli::token_spans(line);
        }
        for d in DIALECTS {
            let _ = redact::redact(d, &t);
        }
    }

    #[test]
    fn redact_keeps_non_secret_text(t in "[a-z ;,0-9\n]{0,60}") {
        for d in DIALECTS {
            prop_assert_eq!(redact::redact(d, &t), t.clone());
        }
    }
}
