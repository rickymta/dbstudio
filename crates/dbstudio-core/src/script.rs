//! Script language support types: dialect, statement analysis, parameters, destructive-statement
//! confirmation and the execute request/result of `query_execute` / `lease_execute`.

use serde::{Deserialize, Serialize};

use crate::{ExecSummary, LeaseState, QueryRequest};

/// `serde(default = ...)` helper: `true`.
pub fn yes() -> bool {
    true
}

/// Language of the editor / console for a session. Decides lexer, splitter, params, classifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "name", rename_all = "camelCase")]
pub enum ScriptDialect {
    /// PostgreSQL.
    Postgres,
    /// MySQL / MariaDB.
    #[serde(rename_all = "camelCase")]
    MySql {
        /// MariaDB flavour.
        mariadb: bool,
        /// `NO_BACKSLASH_ESCAPES` is off: `\` escapes inside string literals.
        backslash_escapes: bool,
    },
    /// SQL Server (T-SQL, `GO` batches).
    MsSql,
    /// Oracle (SQL and PL/SQL).
    Oracle,
    /// MongoDB shell syntax.
    MongoShell,
    /// redis-cli lines.
    RedisCli,
    /// Unknown language: one statement, classified `Unknown` (treated as a write).
    Opaque,
}

/// Range of characters in a text (Unicode scalar offsets from 0, `end` exclusive).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextRange {
    /// First character.
    pub start: u32,
    /// One past the last character.
    pub end: u32,
}

/// Result of analysing a script.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScriptAnalysis {
    /// Dialect used.
    pub dialect: ScriptDialect,
    /// Statements in order.
    pub statements: Vec<StatementInfo>,
    /// Parameters found.
    pub params: Vec<ParamRef>,
    /// Lexer problems (unterminated string/comment...). Non-empty means every statement is
    /// `Unknown`.
    pub problems: Vec<ScriptProblem>,
}

/// One statement of a script.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatementInfo {
    /// Zero-based index.
    pub index: u32,
    /// Range without trailing terminator / `GO` line.
    pub range: TextRange,
    /// Classification.
    pub class: StatementClass,
    /// Leading verb for display: `SELECT`, `ALTER TABLE`, `db.users.find`, `FLUSHDB`.
    pub verb: String,
    /// Why the statement counts as destructive, if it does.
    pub destructive: Option<DestructiveReason>,
    /// MySQL/Oracle DDL commits the open transaction implicitly.
    pub implicit_commit: bool,
    /// Best-effort referenced object names (for history and confirmation text).
    pub targets: Vec<String>,
}

/// Coarse class of a statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum StatementClass {
    /// `SELECT`, `SHOW`, `find`, `GET`...
    Read,
    /// `INSERT`/`UPDATE`/`DELETE`/`MERGE`, `insertOne`, `SET key`...
    Write,
    /// `CREATE`/`ALTER`/`DROP`/`TRUNCATE`, `createIndex`, `drop`.
    Ddl,
    /// Transaction control.
    Transaction {
        /// Which control statement.
        op: TxControl,
    },
    /// `SET search_path`, `USE db`, `SELECT db` (Redis), `ALTER SESSION`.
    Session,
    /// `CALL`/`EXEC`/anonymous PL/SQL, `runCommand`, `EVAL`.
    Procedural,
    /// `EXPLAIN`.
    Explain {
        /// `ANALYZE` (the statement is executed).
        analyze: bool,
        /// The explained statement modifies data.
        writes: bool,
    },
    /// `GRANT`, `SET GLOBAL`, `CONFIG SET`, `KILL`, `SHUTDOWN`.
    Admin,
    /// Comments only.
    Empty,
    /// Not understood; treated as a write.
    Unknown,
}

/// Transaction control statement kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TxControl {
    /// `BEGIN` / `START TRANSACTION`.
    Begin,
    /// `COMMIT`.
    Commit,
    /// `ROLLBACK`.
    Rollback,
    /// `SAVEPOINT`.
    Savepoint,
    /// `RELEASE SAVEPOINT`.
    Release,
    /// `ROLLBACK TO SAVEPOINT`.
    RollbackToSavepoint,
}

/// Why a statement needs confirmation on a production connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DestructiveReason {
    /// `DROP ...`
    Drop,
    /// `TRUNCATE`
    Truncate,
    /// `DELETE` without `WHERE`.
    DeleteWithoutWhere,
    /// `UPDATE` without `WHERE`.
    UpdateWithoutWhere,
    /// `ALTER TABLE ... DROP ...`
    AlterDrop,
    /// Redis `FLUSHDB`.
    FlushDb,
    /// Redis `FLUSHALL`.
    FlushAll,
    /// Redis delete by pattern.
    DeleteByPattern,
    /// Mongo `drop()` of a collection.
    DropCollection,
    /// Mongo `dropDatabase()`.
    DropDatabase,
    /// Mongo `deleteMany({})`.
    DeleteManyAll,
    /// Mongo `updateMany({}, ...)`.
    UpdateManyAll,
    /// Restore of a backup (app operation).
    Restore,
    /// Import in replace mode (app operation).
    ImportReplace,
}

/// A lexer problem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScriptProblem {
    /// Where.
    pub range: TextRange,
    /// What.
    pub message: String,
}

/// Parameter placeholder style.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ParamStyle {
    /// `:name`
    Named,
    /// `$1`
    Dollar,
    /// `?`
    Question,
    /// `@p`
    At,
}

/// A parameter and all its occurrences.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParamRef {
    /// Key used in [`ParamValue::name`]: `:id`, `$1`, `?3` (3rd question mark), `@p`.
    pub name: String,
    /// Placeholder style.
    pub style: ParamStyle,
    /// Occurrences.
    pub ranges: Vec<TextRange>,
}

/// A user-supplied parameter value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParamValue {
    /// Matches [`ParamRef::name`].
    pub name: String,
    /// The value.
    pub value: ParamInput,
}

/// How a parameter value is rendered into the text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "v", rename_all = "camelCase")]
pub enum ParamInput {
    /// `NULL`.
    Null,
    /// Rendered as a dialect string literal (escaped).
    Text(String),
    /// Validated against `/^-?\d+(\.\d+)?([eE][-+]?\d+)?$/`.
    Number(String),
    /// Boolean literal.
    Bool(bool),
    /// SQL expression typed by the user, inserted verbatim (trusted).
    Raw(String),
}

/// Payload of [`crate::Error::ConfirmationRequired`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfirmationRequest {
    /// 32 hex chars, single use.
    pub token: String,
    /// Text the user must type exactly (database name, `db0` for Redis).
    pub confirm_text: String,
    /// Statements that triggered the confirmation.
    pub statements: Vec<DestructiveStatement>,
    /// Remaining validity of the token.
    pub expires_in_ms: u64,
}

/// A destructive statement listed in a [`ConfirmationRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DestructiveStatement {
    /// Statement index.
    pub index: u32,
    /// Range in the ORIGINAL (pre-binding) text.
    pub range: TextRange,
    /// Statement text, truncated to 2000 chars.
    pub text: String,
    /// Why it is destructive.
    pub reason: DestructiveReason,
}

/// The user's answer to a [`ConfirmationRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfirmationGrant {
    /// Token from the request.
    pub token: String,
    /// What the user typed.
    pub typed: String,
}

/// Request body of `query_execute` / `lease_execute`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecuteRequest {
    /// The statements and limits.
    #[serde(flatten)]
    pub query: QueryRequest,
    /// Parameter values.
    #[serde(default)]
    pub params: Vec<ParamValue>,
    /// Answer to a previous confirmation request.
    #[serde(default)]
    pub confirm: Option<ConfirmationGrant>,
    /// Record in query history (default true).
    #[serde(default = "crate::script::yes")]
    pub record_history: bool,
}

/// Result of `query_execute` / `lease_execute`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecuteResult {
    /// Totals reported by the driver.
    pub summary: ExecSummary,
    /// Lease state after the call; `None` for `query_execute`.
    pub state: Option<LeaseState>,
    /// Row id in the query history, when recorded.
    pub history_id: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use uuid::Uuid;

    fn rt<T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug>(
        v: &T,
    ) -> serde_json::Value {
        let json = serde_json::to_value(v).unwrap();
        let back: T = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(&back, v, "round trip of {json}");
        json
    }

    #[test]
    fn dialect_shapes() {
        assert_eq!(rt(&ScriptDialect::Postgres), json!({"name": "postgres"}));
        assert_eq!(rt(&ScriptDialect::MsSql), json!({"name": "msSql"}));
        assert_eq!(
            rt(&ScriptDialect::MongoShell),
            json!({"name": "mongoShell"})
        );
        assert_eq!(rt(&ScriptDialect::RedisCli), json!({"name": "redisCli"}));
        assert_eq!(rt(&ScriptDialect::Opaque), json!({"name": "opaque"}));
        assert_eq!(
            rt(&ScriptDialect::MySql {
                mariadb: true,
                backslash_escapes: false
            }),
            json!({"name": "mySql", "mariadb": true, "backslashEscapes": false})
        );
    }

    #[test]
    fn statement_class_shapes() {
        assert_eq!(rt(&StatementClass::Read), json!({"kind": "read"}));
        assert_eq!(
            rt(&StatementClass::Transaction {
                op: TxControl::RollbackToSavepoint
            }),
            json!({"kind": "transaction", "op": "rollbackToSavepoint"})
        );
        assert_eq!(
            rt(&StatementClass::Explain {
                analyze: true,
                writes: false
            }),
            json!({"kind": "explain", "analyze": true, "writes": false})
        );
        assert_eq!(rt(&StatementClass::Unknown), json!({"kind": "unknown"}));
        assert_eq!(rt(&StatementClass::Ddl), json!({"kind": "ddl"}));
    }

    #[test]
    fn analysis_shape() {
        let a = ScriptAnalysis {
            dialect: ScriptDialect::Postgres,
            statements: vec![StatementInfo {
                index: 0,
                range: TextRange { start: 0, end: 20 },
                class: StatementClass::Write,
                verb: "DELETE".into(),
                destructive: Some(DestructiveReason::DeleteWithoutWhere),
                implicit_commit: false,
                targets: vec!["users".into()],
            }],
            params: vec![ParamRef {
                name: ":id".into(),
                style: ParamStyle::Named,
                ranges: vec![TextRange { start: 3, end: 6 }],
            }],
            problems: vec![ScriptProblem {
                range: TextRange { start: 9, end: 10 },
                message: "unterminated string".into(),
            }],
        };
        let json = rt(&a);
        assert_eq!(json["statements"][0]["destructive"], "deleteWithoutWhere");
        assert_eq!(json["statements"][0]["implicitCommit"], false);
        assert_eq!(json["params"][0]["style"], "named");
    }

    #[test]
    fn param_input_shapes() {
        assert_eq!(rt(&ParamInput::Null), json!({"kind": "null"}));
        assert_eq!(
            rt(&ParamInput::Text("o'x".into())),
            json!({"kind": "text", "v": "o'x"})
        );
        assert_eq!(
            rt(&ParamInput::Number("1.5e3".into())),
            json!({"kind": "number", "v": "1.5e3"})
        );
        assert_eq!(
            rt(&ParamInput::Bool(true)),
            json!({"kind": "bool", "v": true})
        );
        assert_eq!(
            rt(&ParamValue {
                name: "$1".into(),
                value: ParamInput::Raw("now()".into())
            }),
            json!({"name": "$1", "value": {"kind": "raw", "v": "now()"}})
        );
    }

    #[test]
    fn execute_request_flattens_query() {
        let id = Uuid::nil();
        let mut q = QueryRequest::new("SELECT 1");
        q.query_id = id;
        q.max_rows = Some(100);
        let req = ExecuteRequest {
            query: q,
            params: vec![],
            confirm: Some(ConfirmationGrant {
                token: "t".into(),
                typed: "app".into(),
            }),
            record_history: false,
        };
        let json = rt(&req);
        assert_eq!(json["text"], "SELECT 1");
        assert_eq!(json["maxRows"], 100);
        assert_eq!(json["recordHistory"], false);
        assert_eq!(json["confirm"], json!({"token": "t", "typed": "app"}));
        assert!(json.get("query").is_none(), "flattened");

        // Minimal input: only the QueryRequest fields; recordHistory defaults to true.
        let minimal: ExecuteRequest = serde_json::from_value(json!({
            "text": "SELECT 1", "queryId": "00000000-0000-0000-0000-000000000000"
        }))
        .unwrap();
        assert!(minimal.record_history);
        assert!(minimal.params.is_empty() && minimal.confirm.is_none());
        assert_eq!(minimal.query.query_id, id);
    }

    #[test]
    fn execute_result_shape() {
        let r = ExecuteResult {
            summary: ExecSummary {
                statements: 1,
                elapsed_ms: 5,
            },
            state: None,
            history_id: Some(7),
        };
        assert_eq!(
            rt(&r),
            json!({"summary": {"statements": 1, "elapsedMs": 5}, "state": null, "historyId": 7})
        );
    }
}
