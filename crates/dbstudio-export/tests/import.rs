//! Import against a scripted session (no database needed).

mod common;

use std::io::Write;
use std::path::{Path, PathBuf};

use common::*;
use dbstudio_core::{Error, ObjectKind, OnConflict, Value, ValueKind};
use dbstudio_export::{
    ColumnMapping, ImportFormat, ImportMode, ImportPreviewRequest, ImportSpec, ImportTarget,
    NewColumn, TextEncoding, Transform, preview_import, run_import,
};
use uuid::Uuid;

fn write(dir: &Path, name: &str, content: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::File::create(&p)
        .unwrap()
        .write_all(content.as_bytes())
        .unwrap();
    p
}

fn csv_format() -> ImportFormat {
    ImportFormat::Csv {
        delimiter: ';',
        quote: '"',
        header: true,
        encoding: TextEncoding::Utf8,
    }
}

fn spec(file: &Path, format: ImportFormat, target: ImportTarget) -> ImportSpec {
    ImportSpec {
        connection_id: Uuid::nil(),
        file: file.to_string_lossy().into_owned(),
        format,
        target,
        mapping: vec![],
        on_conflict: None,
        mode: ImportMode::Batches { rows: 3 },
        max_errors: 0,
    }
}

fn existing(name: &str) -> ImportTarget {
    ImportTarget::Existing {
        table: table_path(name),
    }
}

fn inserts(session: &FakeSession) -> Vec<String> {
    session
        .statements()
        .into_iter()
        .filter(|s| s.starts_with("INSERT"))
        .collect()
}

fn map(source: &str, target: &str, transform: Transform) -> ColumnMapping {
    ColumnMapping {
        source: source.into(),
        target: target.into(),
        transform,
    }
}

#[tokio::test]
async fn csv_import_maps_columns_and_batches_rows() {
    let dir = tempfile::tempdir().unwrap();
    let file = write(
        dir.path(),
        "in.csv",
        "Code;Label;skip\n1; first ;x\n2;;y\n3;third;z\n4;fourth;w\n5;\"it's\";v\n",
    );
    let session = FakeSession::with_tables(&[("orders", 0)]);
    let mut s = spec(&file, csv_format(), existing("orders"));
    s.mapping = vec![
        map("Code", "id", Transform::None),
        map("#2", "name", Transform::TrimEmptyAsNull),
    ];
    let report = run_import(context(session.clone(), Recorder::new()), s)
        .await
        .unwrap();
    assert_eq!(report.rows_read, 5);
    assert_eq!(report.inserted, 5);
    assert!(report.errors.is_empty());
    let sql = inserts(&session);
    assert_eq!(sql.len(), 2, "batches of 3 and 2 rows: {sql:?}");
    assert_eq!(
        sql[0],
        "INSERT INTO \"public\".\"orders\" (\"id\", \"name\") VALUES\n('1', 'first'),\n('2', NULL),\n('3', 'third')"
    );
    assert!(sql[1].ends_with("('5', 'it''s')"), "{}", sql[1]);
}

#[tokio::test]
async fn automatic_mapping_matches_header_names() {
    let dir = tempfile::tempdir().unwrap();
    let file = write(dir.path(), "in.csv", "ID;Name;other\n7;x;y\n");
    let session = FakeSession::with_tables(&[("orders", 0)]);
    let report = run_import(
        context(session.clone(), Recorder::new()),
        spec(&file, csv_format(), existing("orders")),
    )
    .await
    .unwrap();
    assert_eq!(report.inserted, 1);
    assert!(inserts(&session)[0].contains("(\"id\", \"name\")"));
}

#[tokio::test]
async fn failing_rows_are_reported_with_their_line() {
    let dir = tempfile::tempdir().unwrap();
    let file = write(
        dir.path(),
        "in.csv",
        "id;name\n1;ok\n2;bad\n3;ok\n4;ok\n5;bad\n6;ok\n",
    );
    let session = FakeSession::with_tables(&[("orders", 0)]);
    *session.fail_on.lock().unwrap() = Some("'bad'".into());

    // max_errors = 0: stops at the first failing row and says which line it is.
    let err = run_import(
        context(session.clone(), Recorder::new()),
        spec(&file, csv_format(), existing("orders")),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&err, Error::Query { message, .. } if message.starts_with("line 3:")),
        "{err:?}"
    );

    // max_errors = 5: the bad rows are skipped and listed, the good ones are imported.
    let mut s = spec(&file, csv_format(), existing("orders"));
    s.max_errors = 5;
    let report = run_import(context(session.clone(), Recorder::new()), s.clone())
        .await
        .unwrap();
    assert_eq!(report.rows_read, 6);
    assert_eq!(report.inserted, 4);
    let lines: Vec<u64> = report.errors.iter().map(|e| e.line).collect();
    assert_eq!(lines, [3, 6]);
    assert!(report.errors[0].message.contains("insert refused"));

    // Too many errors: aborts at the one that exceeds the limit.
    s.max_errors = 1;
    let err = run_import(context(session, Recorder::new()), s)
        .await
        .unwrap_err();
    assert!(matches!(&err, Error::Query { message, .. } if message.starts_with("line 6:")));
}

#[tokio::test]
async fn single_transaction_needs_a_dedicated_connection() {
    let dir = tempfile::tempdir().unwrap();
    let file = write(dir.path(), "in.csv", "id;name\n1;a\n");
    let session = FakeSession::with_tables(&[("orders", 0)]);
    let mut s = spec(&file, csv_format(), existing("orders"));
    s.mode = ImportMode::SingleTransaction;
    let err = run_import(context(session.clone(), Recorder::new()), s)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Unsupported(_)));
    assert!(inserts(&session).is_empty());
}

#[tokio::test]
async fn new_table_is_created_first() {
    let dir = tempfile::tempdir().unwrap();
    let file = write(dir.path(), "in.csv", "id;label\n1;a\n2;b\n");
    let session = FakeSession::with_tables(&[]);
    let columns = vec![
        NewColumn {
            name: "id".into(),
            type_name: "bigint".into(),
            nullable: false,
            primary_key: true,
        },
        NewColumn {
            name: "label".into(),
            type_name: "text".into(),
            nullable: true,
            primary_key: false,
        },
    ];
    let target = ImportTarget::New {
        parent: schema_path(),
        name: "fresh".into(),
        columns,
    };
    let report = run_import(
        context(session.clone(), Recorder::new()),
        spec(&file, csv_format(), target),
    )
    .await
    .unwrap();
    assert_eq!(report.inserted, 2);
    let log = session.statements();
    assert_eq!(
        log[0],
        "CREATE TABLE \"public\".\"fresh\" (\n  \"id\" bigint NOT NULL,\n  \"label\" text,\n  PRIMARY KEY (\"id\")\n)"
    );
    assert!(log[1].starts_with("INSERT INTO \"public\".\"fresh\""));
}

#[tokio::test]
async fn conflict_modes_shape_the_statement() {
    let dir = tempfile::tempdir().unwrap();
    let file = write(dir.path(), "in.csv", "id;name\n1;a\n");
    let session = FakeSession::with_tables(&[("orders", 0)]);
    for (mode, tail) in [
        (OnConflict::Skip, "ON CONFLICT DO NOTHING"),
        (
            OnConflict::Update,
            "ON CONFLICT (\"id\") DO UPDATE SET \"name\" = EXCLUDED.\"name\"",
        ),
    ] {
        let mut s = spec(&file, csv_format(), existing("orders"));
        s.on_conflict = Some(mode);
        run_import(context(session.clone(), Recorder::new()), s)
            .await
            .unwrap();
        assert!(inserts(&session).last().unwrap().ends_with(tail));
    }
}

#[tokio::test]
async fn json_import_keeps_types() {
    let dir = tempfile::tempdir().unwrap();
    let file = write(
        dir.path(),
        "in.json",
        r#"[{"id": 1, "name": "a", "price": 2.5}, {"id": 2, "name": null, "price": 3}]"#,
    );
    let session = FakeSession::with_tables(&[("orders", 0)]);
    let report = run_import(
        context(session.clone(), Recorder::new()),
        spec(
            &file,
            ImportFormat::Json {
                layout: dbstudio_export::JsonLayout::Array,
            },
            existing("orders"),
        ),
    )
    .await
    .unwrap();
    assert_eq!(report.inserted, 2);
    assert!(inserts(&session)[0].ends_with("(1, 'a', 2.5),\n(2, NULL, 3)"));
}

#[tokio::test]
async fn sql_script_import_counts_statements_and_reports_errors() {
    let dir = tempfile::tempdir().unwrap();
    let file = write(
        dir.path(),
        "dump.sql",
        "CREATE TABLE t (a int);\nINSERT INTO t VALUES\n(1),\n(2);\nINSERT INTO t VALUES (3);\nINSERT INTO t VALUES (99);\nSELECT 'a;b';\n",
    );
    let session = FakeSession::with_tables(&[]);
    let target = ImportTarget::Script { database: None };

    let ok = run_import(
        context(session.clone(), Recorder::new()),
        spec(
            &file,
            ImportFormat::Sql {
                continue_on_error: false,
            },
            target.clone(),
        ),
    )
    .await
    .unwrap();
    assert_eq!(ok.rows_read, 5);
    assert_eq!(ok.inserted, 4);
    assert_eq!(session.statements()[4], "SELECT 'a;b'");

    *session.fail_on.lock().unwrap() = Some("99".into());
    let err = run_import(
        context(session.clone(), Recorder::new()),
        spec(
            &file,
            ImportFormat::Sql {
                continue_on_error: false,
            },
            target.clone(),
        ),
    )
    .await
    .unwrap_err();
    assert!(matches!(&err, Error::Query { message, .. } if message.starts_with("statement 4:")));

    let tolerant = run_import(
        context(session, Recorder::new()),
        spec(
            &file,
            ImportFormat::Sql {
                continue_on_error: true,
            },
            target,
        ),
    )
    .await
    .unwrap();
    assert_eq!(tolerant.rows_read, 5);
    assert_eq!(tolerant.errors.len(), 1);
    assert_eq!(tolerant.errors[0].line, 4);
}

#[tokio::test]
async fn cancelled_import_stops() {
    let dir = tempfile::tempdir().unwrap();
    let file = write(dir.path(), "in.csv", "id;name\n1;a\n");
    let session = FakeSession::with_tables(&[("orders", 0)]);
    let reporter = Recorder::new();
    reporter.token.cancel();
    let err = run_import(
        context(session.clone(), reporter),
        spec(&file, csv_format(), existing("orders")),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, Error::Cancelled));
    assert!(inserts(&session).is_empty());
}

#[tokio::test]
async fn invalid_targets_and_mappings_fail_early() {
    let dir = tempfile::tempdir().unwrap();
    let file = write(dir.path(), "in.csv", "id;name\n1;a\n");
    let session = FakeSession::with_tables(&[("orders", 0)]);
    let mut s = spec(&file, csv_format(), existing("orders"));
    s.mapping = vec![map("nope", "id", Transform::None)];
    assert!(
        run_import(context(session.clone(), Recorder::new()), s)
            .await
            .is_err()
    );
    let s = spec(&file, csv_format(), existing("missing"));
    assert!(matches!(
        run_import(context(session.clone(), Recorder::new()), s)
            .await
            .unwrap_err(),
        Error::NotFound(_)
    ));
    let s = spec(&file, csv_format(), ImportTarget::Script { database: None });
    assert!(
        run_import(context(session.clone(), Recorder::new()), s)
            .await
            .is_err()
    );
    assert!(session.statements().is_empty());
}

#[tokio::test]
async fn preview_csv_detects_delimiter_and_kinds() {
    let dir = tempfile::tempdir().unwrap();
    let mut text = String::from("id;price;day;label\n");
    for i in 0..200 {
        text.push_str(&format!("{i};{i}.5;2024-01-{:02};row {i}\n", i % 28 + 1));
    }
    let file = write(dir.path(), "in.csv", &text);
    let p = preview_import(ImportPreviewRequest {
        file: file.to_string_lossy().into_owned(),
        format: csv_format(),
        rows: 10,
    })
    .await
    .unwrap();
    assert_eq!(p.columns, ["id", "price", "day", "label"]);
    assert_eq!(p.rows.len(), 10);
    assert_eq!(p.rows[0][0], Value::Text("0".into()));
    assert_eq!(p.detected_delimiter, Some(';'));
    assert_eq!(
        p.suggested_kinds,
        [
            ValueKind::Int,
            ValueKind::Float,
            ValueKind::Date,
            ValueKind::Text
        ]
    );
    assert_eq!(p.estimated_rows, Some(200));
}

#[tokio::test]
async fn preview_sql_lists_statements() {
    let dir = tempfile::tempdir().unwrap();
    let file = write(
        dir.path(),
        "d.sql",
        "CREATE TABLE t (a int);\nDROP TABLE old;\nINSERT INTO t VALUES (1);\n",
    );
    let p = preview_import(ImportPreviewRequest {
        file: file.to_string_lossy().into_owned(),
        format: ImportFormat::Sql {
            continue_on_error: false,
        },
        rows: 50,
    })
    .await
    .unwrap();
    assert_eq!(p.statements_preview.len(), 3);
    assert_eq!(p.statements_preview[1].verb, "DROP TABLE");
    assert!(p.statements_preview[1].destructive.is_some());
    assert!(p.columns.is_empty());
    let _ = ObjectKind::Table;
}

#[tokio::test]
async fn preview_xlsx_lists_sheets() {
    use rust_xlsxwriter::Workbook;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("in.xlsx");
    let mut wb = Workbook::new();
    let ws = wb.add_worksheet();
    ws.set_name("Data").unwrap();
    ws.write_string(0, 0, "id").unwrap();
    ws.write_string(0, 1, "name").unwrap();
    ws.write_number(1, 0, 1.0).unwrap();
    ws.write_string(1, 1, "a").unwrap();
    ws.write_number(2, 0, 2.0).unwrap();
    wb.add_worksheet().set_name("Other").unwrap();
    wb.save(&path).unwrap();
    let p = preview_import(ImportPreviewRequest {
        file: path.to_string_lossy().into_owned(),
        format: ImportFormat::Xlsx {
            sheet: String::new(),
            header: true,
        },
        rows: 50,
    })
    .await
    .unwrap();
    assert_eq!(p.sheets, ["Data", "Other"]);
    assert_eq!(p.columns, ["id", "name"]);
    assert_eq!(p.rows.len(), 2);
    assert_eq!(p.rows[0][0], Value::Int(1));
    assert_eq!(p.suggested_kinds[0], ValueKind::Int);
    assert_eq!(p.estimated_rows, Some(2));

    // The imported rows come out typed, with the Excel row number as the line.
    let session = FakeSession::with_tables(&[("orders", 0)]);
    let report = run_import(
        context(session.clone(), Recorder::new()),
        spec(
            &path,
            ImportFormat::Xlsx {
                sheet: "Data".into(),
                header: true,
            },
            existing("orders"),
        ),
    )
    .await
    .unwrap();
    assert_eq!(report.inserted, 2);
    assert!(inserts(&session)[0].ends_with("(1, 'a'),\n(2, NULL)"));
}
