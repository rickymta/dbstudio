//! Invariant of design 2.2: the text sent for a step is ONE statement for the driver's splitter,
//! for every dialect (the guard judges the statement the driver runs).

use dbstudio_core::ScriptDialect;
use dbstudio_debug::{DebugProgram, wrap_single};
use proptest::prelude::*;

fn fragments(d: &ScriptDialect) -> Vec<&'static str> {
    match d {
        ScriptDialect::Postgres => vec![
            "SELECT 1;",
            "INSERT INTO t VALUES ('a;b');",
            "DO $$ BEGIN NULL; END $$;",
            "CREATE FUNCTION f() RETURNS int AS $$ SELECT 1; $$ LANGUAGE sql;",
            "-- c\nSELECT 2;",
            "/* x; */ SELECT 3;",
        ],
        ScriptDialect::MySql { .. } => vec![
            "DELIMITER $$\nCREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END$$\nDELIMITER ;",
            "SELECT 1;",
            "SET @a = 1;",
            "INSERT INTO t VALUES ('x;y');",
            "DELIMITER //\nCREATE TRIGGER g BEFORE INSERT ON t FOR EACH ROW BEGIN SET NEW.a = 1; SET NEW.b = 2; END//\nDELIMITER ;",
        ],
        ScriptDialect::MsSql => vec![
            "SELECT 1;",
            "DECLARE @a int = 1;",
            "GO",
            "UPDATE t SET a = 1;",
            "BEGIN TRAN;",
            "PRINT 'a;b';",
        ],
        ScriptDialect::Oracle => vec![
            "SELECT 1 FROM dual;",
            "BEGIN null; END;\n/",
            "INSERT INTO t VALUES (1);",
            "SET SERVEROUTPUT ON",
        ],
        ScriptDialect::MongoShell => vec!["db.a.find({});", "db.b.insertOne({x: 1});"],
        ScriptDialect::RedisCli => vec!["SET a 1", "GET a", "MULTI", "EXEC"],
        ScriptDialect::Cql => vec![
            "SELECT * FROM a;",
            "INSERT INTO a (x) VALUES (1);",
            "BEGIN BATCH INSERT INTO a(x) VALUES (1); APPLY BATCH;",
        ],
        ScriptDialect::ClickHouse => vec!["SELECT 1;", "INSERT INTO t VALUES (1);"],
        ScriptDialect::Opaque => vec!["anything"],
    }
}

fn dialects() -> Vec<ScriptDialect> {
    vec![
        ScriptDialect::Postgres,
        ScriptDialect::mysql(false, true),
        ScriptDialect::MsSql,
        ScriptDialect::Oracle,
        ScriptDialect::MongoShell,
        ScriptDialect::RedisCli,
        ScriptDialect::Cql,
        ScriptDialect::ClickHouse,
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn a_step_is_one_statement_for_the_driver_splitter(
        di in 0usize..8,
        picks in proptest::collection::vec(0usize..16, 1..6),
    ) {
        let d = dialects()[di];
        let frags = fragments(&d);
        let text = picks.iter().map(|i| frags[i % frags.len()]).collect::<Vec<_>>().join("\n");
        let program = DebugProgram::build(d, &text, None).unwrap();
        for step in program.steps.iter().filter(|s| s.is_stop()) {
            let wrapped = wrap_single(d, &step.text);
            let n = dbstudio_sql::split(d, &wrapped).len();
            prop_assert_eq!(n, 1, "{:?}: step {:?} wrapped as {:?}", d, step.text, wrapped);
        }
        // steps are in order, never overlap, and carry 1-based lines
        for w in program.steps.windows(2) {
            prop_assert!(w[0].range.end <= w[1].range.start || w[0].range == w[1].range);
        }
        prop_assert!(program.steps.iter().all(|s| s.line_start >= 1 && s.line_end >= s.line_start));
    }
}
