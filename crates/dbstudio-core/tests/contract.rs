//! JSON contract fixtures shared with the frontend (`src/lib/types/contract.test.ts`).
//!
//! `UPDATE_FIXTURES=1 cargo test -p dbstudio-core --test contract` rewrites
//! `contract/fixtures/<group>.json`; without it the test fails when a serialised sample no longer
//! equals the committed fixture. Groups `value` and `error` hold only `Value` / `Error` samples
//! (free names); every other group maps a Rust type name to one sample.

use std::collections::BTreeMap;

use dbstudio_core::testing::write_or_check;
use dbstudio_core::*;
use serde::Serialize;
use serde_json::{Map, Value as Json, json};
use uuid::Uuid;

fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

fn j<T: Serialize>(v: &T) -> Json {
    serde_json::to_value(v).unwrap()
}

/// Build a group object from `(name, sample)` pairs.
fn group(items: Vec<(&str, Json)>) -> Json {
    let mut m = Map::new();
    for (k, v) in items {
        assert!(m.insert(k.to_string(), v).is_none(), "duplicate sample {k}");
    }
    Json::Object(m)
}

fn path(kind_names: &[(ObjectKind, &str)]) -> ObjectPath {
    kind_names
        .iter()
        .fold(ObjectPath::root(), |p, (k, n)| p.child(*k, *n))
}

fn table_path(name: &str) -> ObjectPath {
    path(&[
        (ObjectKind::Database, "app"),
        (ObjectKind::Folder, "tables"),
        (ObjectKind::Table, name),
    ])
}

fn text(s: &str) -> Value {
    Value::Text(s.into())
}

fn column_detail() -> ColumnDetail {
    ColumnDetail {
        name: "id".into(),
        ordinal: 1,
        type_name: "int8".into(),
        value_kind: ValueKind::Int,
        nullable: false,
        default: None,
        comment: Some("primary key".into()),
        primary_key: true,
        extra: Some("identity".into()),
    }
}

fn progress() -> JobProgress {
    JobProgress {
        phase: "data".into(),
        object: Some("public.orders".into()),
        objects_done: 3,
        objects_total: Some(12),
        rows_done: 150_000,
        rows_total: Some(1_000_000),
        bytes_done: 20_971_520,
        bytes_total: None,
        message: None,
    }
}

fn lease_state() -> LeaseState {
    LeaseState {
        auto_commit: false,
        transaction: TxStatus::Active,
        running: Some(id(1)),
        pending_result: None,
        database: Some("app".into()),
        schema: Some("public".into()),
        broken: false,
    }
}

#[test]
fn value_fixtures() {
    let nested = Value::Document(vec![
        ("zeta".into(), Value::Int(1)),
        ("alpha".into(), text("x")),
        (
            "nested".into(),
            Value::Document(vec![
                (
                    "id".into(),
                    Value::Ext(ExtValue::new("objectId", "65f0a1b2c3d4e5f6a7b8c9d0")),
                ),
                ("tags".into(), Value::Array(vec![text("a"), Value::Null])),
            ]),
        ),
    ]);
    let samples = group(vec![
        ("null", j(&Value::Null)),
        ("bool", j(&Value::Bool(true))),
        ("int", j(&Value::Int(42))),
        ("intUnsafe", j(&Value::Int(9_007_199_254_740_993))),
        ("uInt", j(&Value::UInt(u64::MAX))),
        ("float", j(&Value::Float(1.5))),
        ("floatNaN", j(&Value::Float(f64::NAN))),
        (
            "decimal",
            j(&Value::Decimal("12345678901234567890.123456789".into())),
        ),
        ("text", j(&text("hello"))),
        ("bytes", j(&Value::Bytes(vec![0, 1, 2, 255]))),
        ("date", j(&Value::Date("2024-05-31".into()))),
        ("time", j(&Value::Time("13:45:00".into()))),
        (
            "dateTime",
            j(&Value::DateTime("2024-05-31T13:45:00".into())),
        ),
        (
            "timestamp",
            j(&Value::Timestamp(
                "2024-05-31T13:45:00.123456789+07:00[Asia/Ho_Chi_Minh]".into(),
            )),
        ),
        (
            "uuid",
            j(&Value::Uuid("00000000-0000-0000-0000-000000000001".into())),
        ),
        ("json", j(&Value::Json(r#"{"b":1,"a":2}"#.into()))),
        (
            "array",
            j(&Value::Array(vec![Value::Int(1), text("two"), Value::Null])),
        ),
        ("document", j(&nested)),
        (
            "ext",
            j(&Value::Ext(ExtValue::new("bsonTimestamp", "1700000000,1"))),
        ),
    ]);
    // Shapes the frontend depends on.
    assert_eq!(
        samples["document"]["v"][0],
        json!(["zeta", {"t": "int", "v": 1}]),
        "document is an ordered list of pairs"
    );
    assert_eq!(
        samples["ext"],
        json!({"t": "ext", "v": {"type": "bsonTimestamp", "text": "1700000000,1"}})
    );
    write_or_check("value", &samples);
}

#[test]
fn error_fixtures() {
    let confirmation = ConfirmationRequest {
        token: "0123456789abcdef0123456789abcdef".into(),
        confirm_text: "app".into(),
        statements: vec![DestructiveStatement {
            index: 0,
            range: TextRange { start: 0, end: 12 },
            text: "DROP TABLE t".into(),
            reason: DestructiveReason::Drop,
        }],
        expires_in_ms: 120_000,
    };
    let errors: Vec<(&str, Error)> = vec![
        ("connection", Error::Connection("refused".into())),
        (
            "authentication",
            Error::Authentication("bad password".into()),
        ),
        (
            "query",
            Error::Query {
                message: "syntax error".into(),
                position: Some(7),
            },
        ),
        ("unsupported", Error::Unsupported("explain".into())),
        ("cancelled", Error::Cancelled),
        ("timeout", Error::Timeout),
        ("tls", Error::Tls("bad certificate".into())),
        ("notFound", Error::NotFound("session".into())),
        ("storage", Error::Storage("disk full".into())),
        ("io", Error::Io(std::io::Error::other("broken pipe"))),
        ("other", Error::Other("unexpected".into())),
        ("invalid", Error::Invalid("name is empty".into())),
        (
            "readOnly",
            Error::ReadOnlyViolation {
                message: "writes are blocked".into(),
                position: Some(0),
            },
        ),
        (
            "confirmationRequired",
            Error::ConfirmationRequired(Box::new(confirmation)),
        ),
        ("busy", Error::Busy("a statement is running".into())),
        (
            "permission",
            Error::Permission("cannot read V$SQL_PLAN".into()),
        ),
        (
            "driverUnavailable",
            Error::DriverUnavailable("Instant Client not found".into()),
        ),
        (
            "sshHostKey",
            Error::SshHostKey {
                host: "bastion.example.com".into(),
                port: 22,
                key_type: "ssh-ed25519".into(),
                fingerprint: "SHA256:abcdef".into(),
                changed: false,
            },
        ),
        ("integrity", Error::Integrity("checksum mismatch".into())),
        (
            "changeFailed",
            Error::ChangeFailed {
                index: 2,
                message: "expected 1 row, got 0".into(),
                applied: 0,
            },
        ),
    ];
    let mut m = Map::new();
    for (name, e) in &errors {
        let v = j(e);
        assert_eq!(v["kind"], *name, "fixture name equals the kind");
        m.insert((*name).to_string(), v);
    }
    assert_eq!(m["readOnly"]["position"], 0);
    assert_eq!(m["sshHostKey"]["keyType"], "ssh-ed25519");
    write_or_check("error", &Json::Object(m));
}

#[test]
fn script_fixtures() {
    let dialect = ScriptDialect::mysql(false, true);
    let analysis = ScriptAnalysis {
        dialect,
        statements: vec![
            StatementInfo {
                index: 0,
                range: TextRange { start: 0, end: 28 },
                class: StatementClass::Explain {
                    analyze: true,
                    writes: false,
                },
                verb: "EXPLAIN".into(),
                destructive: None,
                implicit_commit: false,
                targets: vec!["users".into()],
            },
            StatementInfo {
                index: 1,
                range: TextRange { start: 30, end: 48 },
                class: StatementClass::Transaction {
                    op: TxControl::Begin,
                },
                verb: "BEGIN".into(),
                destructive: None,
                implicit_commit: false,
                targets: vec![],
            },
            StatementInfo {
                index: 2,
                range: TextRange { start: 50, end: 70 },
                class: StatementClass::Write,
                verb: "DELETE".into(),
                destructive: Some(DestructiveReason::DeleteWithoutWhere),
                implicit_commit: false,
                targets: vec!["users".into()],
            },
        ],
        params: vec![ParamRef {
            name: ":id".into(),
            style: ParamStyle::Named,
            ranges: vec![TextRange { start: 1, end: 4 }],
        }],
        problems: vec![ScriptProblem {
            range: TextRange { start: 9, end: 10 },
            message: "unterminated string".into(),
        }],
    };
    let mut q = QueryRequest::new("SELECT * FROM t WHERE id = :id");
    q.query_id = id(2);
    q.max_rows = Some(1000);
    let exec = ExecuteRequest {
        query: q,
        params: vec![ParamValue {
            name: ":id".into(),
            value: ParamInput::Number("42".into()),
        }],
        confirm: Some(ConfirmationGrant {
            token: "0123456789abcdef0123456789abcdef".into(),
            typed: "app".into(),
        }),
        record_history: true,
    };
    let request = ConfirmationRequest {
        token: "0123456789abcdef0123456789abcdef".into(),
        confirm_text: "app".into(),
        statements: vec![DestructiveStatement {
            index: 0,
            range: TextRange { start: 0, end: 12 },
            text: "DROP TABLE t".into(),
            reason: DestructiveReason::Drop,
        }],
        expires_in_ms: 120_000,
    };
    let samples = group(vec![
        ("ScriptDialect", j(&dialect)),
        ("ScriptDialect#cql", j(&ScriptDialect::Cql)),
        ("ScriptDialect#clickHouse", j(&ScriptDialect::ClickHouse)),
        (
            "ScriptDialect#mySqlAnsi",
            j(&ScriptDialect::MySql {
                mariadb: true,
                backslash_escapes: false,
                ansi_quotes: true,
            }),
        ),
        ("DestructiveReason#write", j(&DestructiveReason::Write)),
        (
            "DestructiveReason#keywordScan",
            j(&DestructiveReason::KeywordScan),
        ),
        ("TextRange", j(&TextRange { start: 3, end: 9 })),
        ("ScriptAnalysis", j(&analysis)),
        ("StatementInfo", j(&analysis.statements[2])),
        (
            "StatementClass",
            j(&StatementClass::Transaction {
                op: TxControl::RollbackToSavepoint,
            }),
        ),
        ("TxControl", j(&TxControl::Savepoint)),
        (
            "DestructiveReason",
            j(&DestructiveReason::DeleteWithoutWhere),
        ),
        ("ScriptProblem", j(&analysis.problems[0])),
        ("ParamStyle", j(&ParamStyle::Question)),
        ("ParamRef", j(&analysis.params[0])),
        ("ParamValue", j(&exec.params[0])),
        ("ParamInput", j(&ParamInput::Text("o'brien".into()))),
        ("ConfirmationRequest", j(&request)),
        ("DestructiveStatement", j(&request.statements[0])),
        ("ConfirmationGrant", j(exec.confirm.as_ref().unwrap())),
        ("ExecuteRequest", j(&exec)),
        (
            "ExecuteResult",
            j(&ExecuteResult {
                summary: ExecSummary {
                    statements: 3,
                    elapsed_ms: 12,
                },
                state: Some(lease_state()),
                history_id: Some(7),
            }),
        ),
    ]);
    assert_eq!(
        samples["ScriptDialect"],
        json!({"name": "mySql", "mariadb": false, "backslashEscapes": true, "ansiQuotes": false})
    );
    assert_eq!(samples["ScriptDialect#cql"], json!({"name": "cql"}));
    assert_eq!(
        samples["ScriptDialect#clickHouse"],
        json!({"name": "clickHouse"})
    );
    assert_eq!(
        samples["ScriptAnalysis"]["statements"][1]["class"],
        json!({"kind": "transaction", "op": "begin"})
    );
    assert_eq!(samples["ExecuteRequest"]["maxRows"], 1000);
    write_or_check("script", &samples);
}

#[test]
fn lease_fixtures() {
    let samples = group(vec![
        (
            "LeaseOptions",
            j(&LeaseOptions {
                database: Some("app".into()),
                schema: None,
                auto_commit: false,
            }),
        ),
        ("IsolationLevel", j(&IsolationLevel::RepeatableRead)),
        (
            "TxOptions",
            j(&TxOptions {
                read_only: true,
                isolation: Some(IsolationLevel::Serializable),
            }),
        ),
        ("TxStatus", j(&TxStatus::Failed)),
        ("LeaseState", j(&lease_state())),
        (
            "LeaseContext",
            j(&LeaseContext {
                database: Some("other".into()),
                schema: Some("s".into()),
            }),
        ),
        (
            "LeaseCloseReport",
            j(&LeaseCloseReport {
                rolled_back: true,
                forced: false,
            }),
        ),
    ]);
    write_or_check("lease", &samples);
}

#[test]
fn edit_fixtures() {
    let rows = ChangeSet::Rows(RowChangeSet {
        table: table_path("users"),
        key_columns: vec!["id".into()],
        changes: vec![
            RowChange::Update {
                key: vec![Value::Int(1)],
                set: vec![CellEdit {
                    column: "name".into(),
                    value: EditValue::Text("Ann".into()),
                }],
            },
            RowChange::Insert {
                values: vec![
                    CellEdit {
                        column: "created".into(),
                        value: EditValue::Default,
                    },
                    CellEdit {
                        column: "note".into(),
                        value: EditValue::Value(Value::Null),
                    },
                ],
            },
            RowChange::Delete {
                key: vec![Value::Int(2)],
            },
        ],
    });
    let documents = ChangeSet::Documents(DocumentChangeSet {
        collection: path(&[
            (ObjectKind::Database, "app"),
            (ObjectKind::Folder, "collections"),
            (ObjectKind::Collection, "users"),
        ]),
        changes: vec![
            DocumentChange::Insert {
                document: DocInput::Ejson {
                    text: r#"{"name":"Ann"}"#.into(),
                },
            },
            DocumentChange::Update {
                id: Value::Ext(ExtValue::new("objectId", "65f0a1b2c3d4e5f6a7b8c9d0")),
                set: vec![FieldEdit {
                    path: vec!["address".into(), "city".into()],
                    value: text("Hue"),
                }],
                unset: vec![vec!["legacy".into()]],
                expected: vec![],
            },
            DocumentChange::Replace {
                id: Value::Int(1),
                document: DocInput::Value {
                    value: Value::Document(vec![("a".into(), Value::Int(1))]),
                },
                expected: vec![],
            },
            DocumentChange::Delete {
                id: Value::Int(3),
                expected: vec![],
            },
        ],
    });
    let keys = ChangeSet::Keys(KeyChangeSet {
        database: path(&[(ObjectKind::Database, "0")]),
        changes: vec![
            KeyChange::SetString {
                key: "greeting".into(),
                value: text("hello"),
                ttl_ms: Some(60_000),
            },
            KeyChange::Rename {
                key: "a".into(),
                new_key: "b".into(),
            },
            KeyChange::Expire {
                key: "a".into(),
                ttl_ms: None,
            },
            KeyChange::ZAdd {
                key: "board".into(),
                members: vec![(text("ann"), "10.5".into())],
            },
            KeyChange::Delete {
                keys: vec!["x".into(), "y".into()],
            },
        ],
    });
    assert_eq!(j(&keys)["changes"][0]["ttlMs"], 60_000);
    assert_eq!(j(&keys)["changes"][1]["newKey"], "b");
    let samples = group(vec![
        (
            "ResultEditability",
            j(&ResultEditability {
                editable: false,
                table: Some(path(&[
                    (ObjectKind::Database, "app"),
                    (ObjectKind::Table, "t"),
                ])),
                key_columns: vec!["id".into()],
                column_map: vec![Some("id".into()), None],
                read_only_columns: vec![],
                reason: Some(ResultNotEditableReason::KeyNotInResult),
                missing_key_columns: vec!["id".into()],
                atomic: true,
                notes: vec![],
            }),
        ),
        (
            "ResultNotEditableReason",
            j(&ResultNotEditableReason::MultipleTables),
        ),
        (
            "Editability",
            j(&Editability {
                editable: true,
                key_columns: vec!["id".into()],
                read_only_columns: vec!["created_at".into()],
                reason: None,
                atomic: true,
                notes: vec![],
            }),
        ),
        (
            "Editability#notes",
            j(&Editability {
                editable: true,
                key_columns: vec!["pk".into()],
                read_only_columns: vec![],
                reason: None,
                atomic: false,
                notes: vec![
                    EditNote::UpsertSemantics,
                    EditNote::ConditionalWrites,
                    EditNote::AsyncMutation,
                ],
            }),
        ),
        ("EditNote", j(&EditNote::AsyncMutation)),
        ("PutMode", j(&PutMode::IfAbsent)),
        (
            "FieldOriginal",
            j(&FieldOriginal {
                path: vec!["address".into(), "city".into()],
                value: Some(text("Hue")),
            }),
        ),
        (
            "FieldOriginal#absent",
            j(&FieldOriginal {
                path: vec!["legacy".into()],
                value: None,
            }),
        ),
        (
            "DocumentChange#updateExpected",
            j(&DocumentChange::Update {
                id: Value::Int(1),
                set: vec![FieldEdit {
                    path: vec!["a".into()],
                    value: Value::Int(2),
                }],
                unset: vec![],
                expected: vec![FieldOriginal {
                    path: vec!["a".into()],
                    value: Some(Value::Int(1)),
                }],
            }),
        ),
        (
            "KeyChange#listSetExpected",
            j(&KeyChange::ListSet {
                key: "l".into(),
                index: 0,
                value: text("new"),
                expected: Some(text("old")),
            }),
        ),
        (
            "KeyChange#put",
            j(&KeyChange::Put {
                key: "k".into(),
                data: KeyData::Hash {
                    fields: vec![(text("f"), text("v"))],
                },
                ttl_ms: Some(60_000),
                mode: PutMode::IfAbsent,
            }),
        ),
        (
            "ChangeSet#keysPut",
            j(&ChangeSet::Keys(KeyChangeSet {
                database: path(&[(ObjectKind::Database, "0")]),
                changes: vec![
                    KeyChange::ListSet {
                        key: "l".into(),
                        index: 1,
                        value: text("v"),
                        expected: Some(text("old")),
                    },
                    KeyChange::ListRemoveAt {
                        key: "l".into(),
                        index: 0,
                        expected: None,
                    },
                    KeyChange::Put {
                        key: "s".into(),
                        data: KeyData::String { value: text("x") },
                        ttl_ms: None,
                        mode: PutMode::Replace,
                    },
                ],
            })),
        ),
        (
            "ChangeSet#documentsExpected",
            j(&ChangeSet::Documents(DocumentChangeSet {
                collection: path(&[
                    (ObjectKind::Database, "app"),
                    (ObjectKind::Folder, "collections"),
                    (ObjectKind::Collection, "users"),
                ]),
                changes: vec![
                    DocumentChange::Replace {
                        id: Value::Int(1),
                        document: DocInput::Ejson { text: "{}".into() },
                        expected: vec![FieldOriginal {
                            path: vec!["a".into()],
                            value: None,
                        }],
                    },
                    DocumentChange::Delete {
                        id: Value::Int(1),
                        expected: vec![],
                    },
                ],
            })),
        ),
        ("NotEditableReason", j(&NotEditableReason::NoUniqueKey)),
        ("ChangeSet", j(&rows)),
        (
            "RowChangeSet",
            j(match &rows {
                ChangeSet::Rows(r) => r,
                _ => unreachable!(),
            }),
        ),
        (
            "RowChange",
            j(&RowChange::Delete {
                key: vec![Value::Int(2)],
            }),
        ),
        (
            "CellEdit",
            j(&CellEdit {
                column: "c".into(),
                value: EditValue::Value(text("v")),
            }),
        ),
        ("EditValue", j(&EditValue::Text("typed".into()))),
        (
            "DocumentChangeSet",
            j(match &documents {
                ChangeSet::Documents(d) => d,
                _ => unreachable!(),
            }),
        ),
        (
            "DocumentChange",
            j(&DocumentChange::Delete {
                id: Value::Int(3),
                expected: vec![],
            }),
        ),
        ("DocInput", j(&DocInput::Ejson { text: "{}".into() })),
        (
            "FieldEdit",
            j(&FieldEdit {
                path: vec!["a".into()],
                value: Value::Int(1),
            }),
        ),
        (
            "KeyChangeSet",
            j(match &keys {
                ChangeSet::Keys(k) => k,
                _ => unreachable!(),
            }),
        ),
        (
            "KeyChange",
            j(&KeyChange::SetString {
                key: "k".into(),
                value: text("v"),
                ttl_ms: Some(1500),
            }),
        ),
        (
            "ApplyResult",
            j(&ApplyResult {
                applied: 2,
                affected: vec![1, 1],
                atomic: true,
            }),
        ),
    ]);
    write_or_check("edit", &samples);

    // Only one `ChangeSet` sample can be keyed by its type name (the rows model, which the
    // frontend guard checks); the other two models are asserted here.
    assert_eq!(j(&documents)["model"], "documents");
    assert_eq!(j(&keys)["model"], "keys");
}

#[test]
fn keyvalue_fixtures() {
    let key = path(&[(ObjectKind::Database, "0"), (ObjectKind::Key, "user:1")]);
    let page = KeyValuePage {
        key: key.clone(),
        key_type: "hash".into(),
        ttl_ms: Some(30_000),
        length: Some(2),
        memory_bytes: Some(120),
        encoding: Some("listpack".into()),
        entries: KeyEntries::Hash {
            fields: vec![
                (text("name"), text("Ann")),
                (text("blob"), Value::Bytes(vec![0, 255])),
            ],
        },
        next_cursor: Some("0".into()),
    };
    let string_entries = KeyEntries::String {
        value: text("hello"),
        total_bytes: 5,
        truncated: false,
    };
    assert_eq!(j(&string_entries)["totalBytes"], 5);
    let node = ObjectNode {
        path: path(&[
            (ObjectKind::Database, "0"),
            (ObjectKind::KeyPrefix, "user:1:"),
        ]),
        kind: ObjectKind::KeyPrefix,
        name: "user:1:".into(),
        has_children: true,
        detail: None,
    };
    let samples = group(vec![
        (
            "KeyReadRequest",
            j(&KeyReadRequest {
                key,
                cursor: None,
                count: 100,
                pattern: Some("a*".into()),
                max_string_bytes: Some(1_048_576),
            }),
        ),
        ("KeyValuePage", j(&page)),
        (
            "KeyScanRequest",
            j(&KeyScanRequest {
                database: path(&[(ObjectKind::Database, "0")]),
                pattern: Some("user:*".into()),
                cursor: None,
                count: 1000,
                type_filter: Some("hash".into()),
            }),
        ),
        (
            "KeyScanPage",
            j(&KeyScanPage {
                keys: vec!["user:1".into(), "\u{0}b64:AJ+Slv8=".into()],
                next_cursor: Some("17".into()),
            }),
        ),
        (
            "KeyData",
            j(&KeyData::ZSet {
                members: vec![(text("ann"), "10.5".into())],
            }),
        ),
        (
            "KeyData#stream",
            j(&KeyData::Stream {
                entries: vec![StreamEntry {
                    id: "1-0".into(),
                    fields: vec![(text("f"), text("v"))],
                }],
            }),
        ),
        ("KeyData#json", j(&KeyData::Json { text: "{}".into() })),
        (
            "KeyRecord",
            j(&KeyRecord {
                key: "user:1".into(),
                data: KeyData::List {
                    items: vec![text("a"), Value::Bytes(vec![0, 255])],
                },
                expire_at_ms: Some(1_800_000_000_000),
            }),
        ),
        ("KeyEntries", j(&string_entries)),
        (
            "StreamEntry",
            j(&StreamEntry {
                id: "1700000000000-0".into(),
                fields: vec![(text("f"), text("v"))],
            }),
        ),
        (
            "ChildrenPageRequest",
            j(&ChildrenPageRequest {
                parent: path(&[(ObjectKind::Database, "0")]),
                cursor: Some("17".into()),
                limit: 500,
                pattern: Some("user:*".into()),
            }),
        ),
        (
            "ChildrenPage",
            j(&ChildrenPage {
                nodes: vec![node.clone()],
                next_cursor: Some("18".into()),
                approximate_total: Some(1000),
            }),
        ),
        ("ObjectNode", j(&node)),
        (
            "ObjectNode#dictionary",
            j(&ObjectNode {
                path: path(&[
                    (ObjectKind::Database, "app"),
                    (ObjectKind::Folder, folder::DICTIONARIES),
                    (ObjectKind::Dictionary, "countries"),
                ]),
                kind: ObjectKind::Dictionary,
                name: "countries".into(),
                has_children: false,
                detail: None,
            }),
        ),
    ]);
    write_or_check("keyvalue", &samples);
}

#[test]
fn plan_fixtures() {
    let mut extra = BTreeMap::new();
    extra.insert("xplan".to_string(), "Plan hash value: 1".to_string());
    let samples = group(vec![
        (
            "ExplainRequest",
            j(&ExplainRequest {
                query_id: id(3),
                text: "SELECT 1".into(),
                database: Some("app".into()),
                analyze: true,
                options: ExplainOptions {
                    buffers: true,
                    verbose: false,
                    settings: true,
                },
                timeout_ms: Some(30_000),
            }),
        ),
        (
            "ExplainOptions",
            j(&ExplainOptions {
                buffers: true,
                verbose: true,
                settings: false,
            }),
        ),
        (
            "RawPlan",
            j(&RawPlan {
                format: PlanFormat::OraclePlanRows,
                text: "[]".into(),
                statement: "SELECT 1 FROM dual".into(),
                analyzed: true,
                rolled_back: true,
                server_version: "23.4".into(),
                captured_at: "2026-11-13T00:00:00Z".into(),
                extra,
            }),
        ),
        ("PlanFormat", j(&PlanFormat::MySqlJsonV2)),
        ("PlanFormat#clickHouseJson", j(&PlanFormat::ClickHouseJson)),
        ("PlanFormat#cockroachText", j(&PlanFormat::CockroachText)),
        ("PlanFormat#tiDbRows", j(&PlanFormat::TiDbRows)),
        ("PlanFormat#cqlTrace", j(&PlanFormat::CqlTrace)),
    ]);
    write_or_check("plan", &samples);
}

#[test]
fn model_fixtures() {
    let table = TableModel {
        path: table_path("orders"),
        kind: ObjectKind::Table,
        schema: Some("public".into()),
        name: "orders".into(),
        comment: Some("customer orders".into()),
        columns: vec![column_detail()],
        primary_key: vec!["id".into()],
        unique_keys: vec![vec!["code".into()]],
        estimated_rows: Some(1000),
    };
    let relation = Relation {
        name: "orders_user_fk".into(),
        from_table: table_path("orders"),
        from_columns: vec!["user_id".into()],
        to_table: table_path("users"),
        to_columns: vec!["id".into()],
        on_update: None,
        on_delete: Some("CASCADE".into()),
        from_optional: true,
        one_to_one: false,
        inferred: false,
    };
    let ddl = DdlItem {
        path: table_path("orders"),
        kind: ObjectKind::Table,
        section: DdlSection::PreData,
        sql: "CREATE TABLE orders (id bigint PRIMARY KEY);".into(),
        language: DdlLanguage::Sql,
        depends_on: vec![table_path("users")],
    };
    let samples = group(vec![
        ("ModelDetail", j(&ModelDetail::Keys)),
        (
            "SchemaModelRequest",
            j(&SchemaModelRequest {
                scope: path(&[(ObjectKind::Database, "app")]),
                tables: vec![],
                include_views: true,
                include_comments: false,
                detail: ModelDetail::Full,
                sample_size: Some(1000),
            }),
        ),
        (
            "SchemaModel",
            j(&SchemaModel {
                scope: path(&[(ObjectKind::Database, "app")]),
                tables: vec![table.clone()],
                relations: vec![relation.clone()],
                inferred: false,
                truncated: false,
            }),
        ),
        ("TableModel", j(&table)),
        ("Relation", j(&relation)),
        (
            "DdlRequest",
            j(&DdlRequest {
                scope: path(&[(ObjectKind::Database, "app")]),
                objects: vec![table_path("orders")],
                options: DdlOptions::default(),
            }),
        ),
        ("DdlOptions", j(&DdlOptions::default())),
        ("DdlSection", j(&DdlSection::PostData)),
        ("DdlLanguage", j(&DdlLanguage::MongoShell)),
        ("DdlLanguage#cql", j(&DdlLanguage::Cql)),
        ("DdlItem", j(&ddl)),
        (
            "TableStreamRequest",
            j(&TableStreamRequest {
                query_id: id(4),
                table: table_path("orders"),
                columns: vec!["id".into()],
                filters: vec![],
                raw_where: Some("id > 10".into()),
                order_by_key: true,
                batch_size: Some(1000),
            }),
        ),
        ("OnConflict", j(&OnConflict::Skip)),
        (
            "BulkInsertRequest",
            j(&BulkInsertRequest {
                table: table_path("orders"),
                columns: vec!["id".into(), "note".into()],
                rows: vec![vec![
                    EditValue::Text("1".into()),
                    EditValue::Value(Value::Null),
                ]],
                on_conflict: OnConflict::Update,
                conflict_columns: vec!["id".into()],
            }),
        ),
        (
            "BulkInsertResult",
            j(&BulkInsertResult {
                inserted: 10,
                updated: 2,
                skipped: 1,
            }),
        ),
    ]);
    assert_eq!(samples["DdlItem"]["section"], "preData");
    assert_eq!(samples["SchemaModel"]["relations"][0]["fromOptional"], true);
    write_or_check("model", &samples);
}

#[test]
fn job_fixtures() {
    let info = JobInfo {
        id: id(5),
        kind: JobKind::Export,
        title: "Export app -> orders.xlsx".into(),
        connection_id: Some(id(6)),
        status: JobStatus::Succeeded,
        progress: progress(),
        created_at: "2026-11-13T00:00:00Z".into(),
        started_at: Some("2026-11-13T00:00:01Z".into()),
        finished_at: Some("2026-11-13T00:00:09Z".into()),
        error: None,
        result: Some(
            json!({"path": "/tmp/orders.xlsx", "sizeBytes": 123, "rows": 10, "warnings": []}),
        ),
        cancellable: false,
    };
    let event = JobEvent::Progress {
        job_id: id(5),
        progress: progress(),
    };
    assert_eq!(j(&event)["jobId"], j(&id(5)));
    let samples = group(vec![
        ("JobKind", j(&JobKind::SqlScript)),
        ("JobStatus", j(&JobStatus::Running)),
        ("JobProgress", j(&progress())),
        (
            "JobError",
            j(&JobError {
                kind: "query".into(),
                message: "boom".into(),
            }),
        ),
        ("JobInfo", j(&info)),
        ("JobEvent", j(&event)),
    ]);
    write_or_check("job", &samples);
}

#[test]
fn ssh_fixtures() {
    let cfg = SshConfig {
        host: "bastion.example.com".into(),
        port: 2222,
        username: "deploy".into(),
        auth: SshAuth::PrivateKey {
            key_path: "/home/me/.ssh/id_ed25519".into(),
            has_passphrase: true,
        },
        keepalive_secs: Some(30),
        connect_timeout_ms: Some(10_000),
    };
    let samples = group(vec![
        ("SshConfig", j(&cfg)),
        ("SshAuth", j(&SshAuth::Agent)),
    ]);
    assert_eq!(
        samples["SshConfig"]["auth"]["keyPath"],
        "/home/me/.ssh/id_ed25519"
    );
    write_or_check("ssh", &samples);
}

#[test]
fn capabilities_fixtures() {
    let samples = group(vec![(
        "Capabilities",
        j(&Capabilities::planned_for(DbKind::Postgres)),
    )]);
    let mut samples = samples;
    samples.as_object_mut().unwrap().insert(
        "Capabilities#cassandra".into(),
        j(&Capabilities::planned_for(DbKind::Cassandra)),
    );
    write_or_check("capabilities", &samples);
}

#[test]
fn driver_fixtures() {
    let mut cfg = ConnectionConfig::new(DbKind::Postgres, "prod", "db.internal");
    cfg.id = id(7);
    cfg.database = Some("app".into());
    cfg.username = Some("admin".into());
    cfg.tls.mode = TlsMode::VerifyFull;
    cfg.tls.server_name = Some("db.internal".into());
    cfg.environment = Some(Environment::Production);
    cfg.environment_label = Some("PROD-EU".into());
    cfg.ssh = Some(SshConfig {
        host: "bastion".into(),
        port: 22,
        username: "me".into(),
        auth: SshAuth::Password,
        keepalive_secs: None,
        connect_timeout_ms: None,
    });
    let mut oracle = DriverDescriptor::planned(DbKind::Oracle);
    oracle.status = DriverStatus::Unavailable {
        reason: UnavailableReason::NativeClientMissing,
    };
    let samples = group(vec![
        (
            "DriverStatus",
            j(&DriverStatus::Unavailable {
                reason: UnavailableReason::NativeClientTooOld,
            }),
        ),
        (
            "UnavailableReason",
            j(&UnavailableReason::NativeClientMissing),
        ),
        ("DriverDescriptor", j(&oracle)),
        (
            "DriverDescriptor#cockroachdb",
            j(&DriverDescriptor::planned(DbKind::CockroachDb)),
        ),
        (
            "DriverDescriptor#yugabytedb",
            j(&DriverDescriptor::planned(DbKind::YugabyteDb)),
        ),
        (
            "DriverDescriptor#tidb",
            j(&DriverDescriptor::planned(DbKind::TiDb)),
        ),
        ("ConnectionConfig", j(&cfg)),
        ("TlsConfig", j(&cfg.tls)),
        (
            "ServerInfo",
            j(&ServerInfo {
                product: "PostgreSQL".into(),
                version: "17.2".into(),
                extra: BTreeMap::new(),
            }),
        ),
    ]);
    assert_eq!(
        samples["DriverStatus"],
        json!({"state": "unavailable", "reason": "nativeClientTooOld"})
    );
    write_or_check("driver", &samples);
}

#[test]
fn query_fixtures() {
    let event = QueryEvent::StatementStart {
        start: StatementStart {
            index: 0,
            range: TextRange { start: 0, end: 17 },
        },
    };
    assert_eq!(
        j(&event),
        json!({"type": "statementStart", "start": {"index": 0, "range": {"start": 0, "end": 17}}})
    );
    let mut q = QueryRequest::new("SELECT 1");
    q.query_id = id(8);
    let samples = group(vec![
        ("QueryEvent", j(&event)),
        (
            "StatementStart",
            j(&StatementStart {
                index: 1,
                range: TextRange { start: 18, end: 30 },
            }),
        ),
        ("QueryRequest", j(&q)),
        (
            "ExecSummary",
            j(&ExecSummary {
                statements: 1,
                elapsed_ms: 4,
            }),
        ),
        (
            "ColumnMeta",
            j(&ColumnMeta {
                name: "$document".into(),
                type_name: "document".into(),
                value_kind: ValueKind::Document,
                nullable: Some(false),
                origin: Some(ColumnOrigin {
                    database: Some("app".into()),
                    schema: None,
                    table: "docs".into(),
                    column: "doc".into(),
                    alias: Some("d".into()),
                }),
            }),
        ),
        (
            "StatementEnd",
            j(&StatementEnd {
                outcome: EndOutcome::Rows {
                    row_count: 5,
                    truncated: true,
                },
                elapsed_ms: 9,
            }),
        ),
    ]);
    write_or_check("query", &samples);
}

/// Groups whose types live in other crates (`dbstudio-export`, `dbstudio-backup`,
/// `dbstudio-store`): created empty here, extended by those crates' own contract tests through
/// `dbstudio_core::testing::write_or_check`.
#[test]
fn groups_owned_by_other_crates_exist() {
    for g in ["export", "backup", "history"] {
        write_or_check(g, &json!({}));
    }
}
