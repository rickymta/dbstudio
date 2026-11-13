//! Differential test: the driver's splitter against `dbstudio_sql::split(Oracle)`.

use dbstudio_core::ScriptDialect;

use crate::script;

fn mine(text: &str) -> Vec<(u32, u32)> {
    script::split(text)
        .iter()
        .map(|s| (s.start as u32, s.end as u32))
        .collect()
}

fn shared(text: &str) -> Vec<(u32, u32)> {
    dbstudio_sql::split(ScriptDialect::Oracle, text)
        .iter()
        .map(|r| (r.start, r.end))
        .collect()
}

fn slice(text: &str, r: (u32, u32)) -> String {
    text.chars()
        .skip(r.0 as usize)
        .take((r.1 - r.0) as usize)
        .collect()
}

const CORPUS: &[&str] = &[
    "select 1 from dual; select 2 from dual;\n",
    "select 1 from dual",
    "  \n -- only a comment\n /* x */ ",
    ";;;",
    "select 'a;b', \"x;y\", q'[c;d]' from dual -- e;f\n; select 2 from dual",
    "select 1 /* ; */ from dual;",
    "select q'{it's;}' , 'it''s;' from dual;",
    "BEGIN\n  INSERT INTO t VALUES (1);\n  UPDATE t SET a = 2;\nEND;\nselect 1 from dual;",
    "DECLARE\n  x NUMBER := 0;\n  CURSOR c IS SELECT 1 FROM dual;\n  PROCEDURE p(a NUMBER) IS\n  BEGIN\n    IF a > 1 THEN NULL; ELSE NULL; END IF;\n  END p;\nBEGIN\n  FOR i IN 1..3 LOOP\n    x := x + CASE WHEN i = 1 THEN 1 ELSE 2 END;\n  END LOOP;\nEND;\nselect 2 from dual;",
    "BEGIN\n  NULL;\nEND;\n/\nselect 1 from dual;\n/\nselect 2 from dual",
    "select 4 / 2 from dual;",
    "CREATE OR REPLACE PROCEDURE hello(p_name IN VARCHAR2) AS\nBEGIN\n  NULL;\nEND hello;\n/\nselect 1 from dual;",
    "CREATE FUNCTION f RETURN NUMBER IS\n  x NUMBER;\nBEGIN\n  RETURN x;\nEND;\nselect 3 from dual;",
    "CREATE OR REPLACE TRIGGER trg BEFORE INSERT ON t FOR EACH ROW\nBEGIN\n  :new.a := 1;\nEND;\n/",
    "CREATE OR REPLACE PACKAGE pk AS\n  PROCEDURE a(x NUMBER);\nEND pk;\n/\nCREATE OR REPLACE PACKAGE BODY pk AS\n  PROCEDURE a(x NUMBER) IS BEGIN NULL; END a;\nEND pk;\n/\nselect 1 from dual",
    "CREATE PACKAGE p2 AS PROCEDURE x; END p2;\nCREATE PACKAGE BODY p2 AS PROCEDURE x IS BEGIN NULL; END; END p2;\nselect 1 from dual;",
    "CREATE TYPE pt AS OBJECT (a NUMBER, MEMBER FUNCTION f RETURN NUMBER);\n/\nCREATE TYPE BODY pt AS\n  MEMBER FUNCTION f RETURN NUMBER IS BEGIN RETURN a; END;\nEND;\n/",
    "CREATE OR REPLACE TRIGGER ct FOR INSERT ON t COMPOUND TRIGGER\n  v NUMBER;\n  BEFORE STATEMENT IS BEGIN v := 0; END BEFORE STATEMENT;\nEND ct;\nselect 1 from dual;",
    "SET SERVEROUTPUT ON\nSET DEFINE OFF;\nPROMPT hello world\nselect 1 from dual;\nEXEC p('x')\nEXECUTE p(1,\n 2);\nSET TRANSACTION READ ONLY;",
    "select 'é€' from dual;\n  select 2 from dual;",
    "-- first\nselect 1 from dual; /* c */ select 2 from dual",
    "BEGIN\n  -- end; begin\n  dbms_output.put_line('END; BEGIN');\n  NULL;\nEND;\nselect 1 from dual;",
    "BEGIN\n  <<outer>>\n  FOR i IN 1..2 LOOP\n    IF i = 1 THEN\n      CONTINUE outer;\n    END IF;\n  END LOOP outer;\nEND;\nselect 1 from dual;",
    "CREATE PROCEDURE j AS LANGUAGE JAVA NAME 'a.b()';\nselect 1 from dual;",
];

/// Statement texts without a trailing terminator, for comparing the two splitters.
fn trimmed(text: &str, ranges: &[(u32, u32)]) -> Vec<String> {
    ranges
        .iter()
        .map(|r| {
            let s = slice(text, *r);
            let s = s.trim_end();
            s.strip_suffix(';').unwrap_or(s).trim_end().to_string()
        })
        .collect()
}

/// Differences found on the corpus (documented in the Oracle package report):
///
/// * `dbstudio_sql` follows SQL*Plus strictly: a PL/SQL block or unit ends only at a `/` line, so
///   `BEGIN ... END;` followed by more text without `/` stays one statement. The driver also ends
///   a block at its balanced final `END;`, which is what editor users expect.
/// * `EXEC p(1)` on consecutive lines: SQL*Plus runs them as two commands (driver), `dbstudio_sql`
///   joins them into one.
/// * A comment before the terminating `;` belongs to the statement in `dbstudio_sql`, the driver
///   leaves it out of the text; a `CREATE TYPE ... AS OBJECT (...)` keeps its `;` there.
#[test]
fn splitters_agree_on_slash_terminated_scripts() {
    let agree = [
        CORPUS[0], CORPUS[1], CORPUS[2], CORPUS[3], CORPUS[5], CORPUS[6], CORPUS[9], CORPUS[10],
        CORPUS[11], CORPUS[13], CORPUS[14], CORPUS[19], CORPUS[20],
    ];
    for t in agree {
        let (a, b) = (mine(t), shared(t));
        assert_eq!(trimmed(t, &a), trimmed(t, &b), "{t:?}");
        let starts = |v: &[(u32, u32)]| v.iter().map(|r| r.0).collect::<Vec<_>>();
        assert_eq!(starts(&a), starts(&b), "{t:?}");
    }
}

#[test]
fn block_without_slash_is_where_they_differ() {
    let t = "BEGIN NULL; END;\nselect 1 from dual;";
    assert_eq!(mine(t).len(), 2);
    assert_eq!(shared(t).len(), 1);
}

use proptest::prelude::*;

proptest! {
    /// Plain statements, `/`-terminated blocks and comments split identically.
    #[test]
    fn generated_scripts_split_the_same(parts in proptest::collection::vec(0usize..5, 1..8), pad in "[ \\n]{0,3}") {
        let mut text = String::new();
        for p in parts {
            match p {
                0 => text.push_str("select 'a;b' from dual;"),
                1 => text.push_str("update t set a = 1 where b = 2;"),
                2 => text.push_str("BEGIN\n  NULL;\n  x := 1;\nEND;\n/"),
                3 => text.push_str("-- note\nselect \"q;x\" from dual;"),
                _ => text.push_str("CREATE OR REPLACE PROCEDURE p AS\nBEGIN\n  IF a THEN NULL; END IF;\nEND p;\n/"),
            }
            text.push('\n');
            text.push_str(&pad);
        }
        let (a, b) = (mine(&text), shared(&text));
        prop_assert_eq!(trimmed(&text, &a), trimmed(&text, &b));
    }

    /// The driver splitter never panics and its ranges stay inside the text.
    #[test]
    fn arbitrary_text_never_panics(text in ".{0,200}") {
        let n = text.chars().count() as u32;
        for r in mine(&text) {
            prop_assert!(r.0 <= r.1 && r.1 <= n);
        }
    }
}
