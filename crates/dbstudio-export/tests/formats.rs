//! Export formats against a scripted session (no database needed).

mod common;

use std::fs::File;
use std::io::Read;
use std::path::Path;

use common::*;
use dbstudio_core::{DdlOptions, Error, ObjectPath};
use dbstudio_export::{
    CsvOptions, ExportContent, ExportDestination, ExportFormat, ExportSource, ExportSpec,
    ExportTuning, JsonLayout, JsonOptions, LineEnding, SqlOptions, TextEncoding, ValueFormat,
    XlsxOptions, run_export, run_export_tuned,
};
use uuid::Uuid;

fn spec(
    source: ExportSource,
    content: ExportContent,
    format: ExportFormat,
    dest: &Path,
) -> ExportSpec {
    ExportSpec {
        connection_id: Uuid::nil(),
        source,
        content,
        format,
        destination: ExportDestination {
            path: dest.to_string_lossy().into_owned(),
            overwrite: false,
        },
        values: ValueFormat::default(),
    }
}

fn tables(names: &[&str]) -> ExportSource {
    ExportSource::Tables {
        paths: names.iter().map(|n| table_path(n)).collect(),
    }
}

fn read(path: &str) -> Vec<u8> {
    std::fs::read(path).unwrap()
}

fn zip_entries(path: &str) -> Vec<(String, Vec<u8>)> {
    let mut z = zip::ZipArchive::new(File::open(path).unwrap()).unwrap();
    (0..z.len())
        .map(|i| {
            let mut f = z.by_index(i).unwrap();
            let mut data = Vec::new();
            f.read_to_end(&mut data).unwrap();
            (f.name().unwrap().to_string(), data)
        })
        .collect()
}

fn leftovers(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

#[tokio::test]
async fn csv_single_table_has_bom_header_quotes_and_nulls() {
    let dir = tempfile::tempdir().unwrap();
    let session = FakeSession::with_tables(&[("orders", 5)]);
    let dest = dir.path().join("orders.csv");
    let report = run_export(
        context(session, Recorder::new()),
        spec(
            tables(&["orders"]),
            ExportContent::Data,
            ExportFormat::Csv(CsvOptions::default()),
            &dest,
        ),
    )
    .await
    .unwrap();
    assert_eq!(report.files, [dest.to_string_lossy()]);
    assert_eq!((report.rows, report.objects), (5, 1));
    let bytes = read(&report.files[0]);
    assert_eq!(&bytes[..3], [0xEF, 0xBB, 0xBF]);
    let text = String::from_utf8(bytes[3..].to_vec()).unwrap();
    let lines: Vec<&str> = text.split("\r\n").collect();
    assert_eq!(lines[0], "id,name,price");
    assert_eq!(lines[1], "1,\"name 1, \"\"quoted\"\"\",1.50");
    assert_eq!(lines[4], "4,,4.50", "NULL is an empty field");
    assert_eq!(lines.len(), 7, "header, 5 rows and the trailing terminator");
    assert_eq!(leftovers(dir.path()), ["orders.csv"]);
}

#[tokio::test]
async fn csv_options_delimiter_lf_no_bom_null_text() {
    let dir = tempfile::tempdir().unwrap();
    let session = FakeSession::with_tables(&[("orders", 4)]);
    let dest = dir.path().join("o.csv");
    let opts = CsvOptions {
        delimiter: ';',
        quote: '\'',
        header: false,
        encoding: TextEncoding::Utf8,
        line_ending: LineEnding::Lf,
        null_text: "NULL".into(),
        ..CsvOptions::default()
    };
    run_export(
        context(session, Recorder::new()),
        spec(
            tables(&["orders"]),
            ExportContent::Data,
            ExportFormat::Csv(opts),
            &dest,
        ),
    )
    .await
    .unwrap();
    let text = String::from_utf8(read(dest.to_str().unwrap())).unwrap();
    assert!(text.starts_with("1;name 1, \"quoted\";1.50\n"), "{text}");
    assert!(text.contains("\n4;NULL;4.50\n"));
}

#[tokio::test]
async fn csv_several_tables_become_a_zip_with_schema() {
    let dir = tempfile::tempdir().unwrap();
    let session = FakeSession::with_tables(&[("orders", 2), ("items", 1)]);
    let dest = dir.path().join("dump.csv");
    let report = run_export(
        context(session, Recorder::new()),
        spec(
            tables(&["orders", "items"]),
            ExportContent::Both,
            ExportFormat::Csv(CsvOptions::default()),
            &dest,
        ),
    )
    .await
    .unwrap();
    let zip_path = dir.path().join("dump.zip");
    assert_eq!(report.files, [zip_path.to_string_lossy()]);
    assert!(
        report.warnings.iter().any(|w| w.contains("dump.zip")),
        "{:?}",
        report.warnings
    );
    let entries = zip_entries(&report.files[0]);
    let names: Vec<&str> = entries.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["items.csv", "orders.csv", "schema.csv"]);
    for (name, data) in &entries {
        assert_eq!(&data[..3], [0xEF, 0xBB, 0xBF], "{name} starts with a BOM");
    }
    let schema = String::from_utf8(entries[2].1[3..].to_vec()).unwrap();
    assert!(schema.starts_with("Table,Column,Position,Type,Nullable,Default,Primary key,Comment"));
    assert!(schema.contains("orders,name,2,text,YES,,,display name"));
    assert_eq!(leftovers(dir.path()), ["dump.zip"]);
}

#[tokio::test]
async fn json_array_ndjson_and_schema_json() {
    let dir = tempfile::tempdir().unwrap();
    let session = FakeSession::with_tables(&[("orders", 4)]);

    // Array, data only: one plain file.
    let dest = dir.path().join("o.json");
    let report = run_export(
        context(session.clone(), Recorder::new()),
        spec(
            tables(&["orders"]),
            ExportContent::Data,
            ExportFormat::Json(JsonOptions::default()),
            &dest,
        ),
    )
    .await
    .unwrap();
    let parsed: serde_json::Value =
        serde_json::from_slice(&read(&report.files[0])).expect("valid JSON");
    let arr = parsed.as_array().unwrap();
    assert_eq!(arr.len(), 4);
    assert_eq!(arr[0]["id"], 1);
    assert_eq!(arr[0]["price"], "1.50", "decimals are strings");
    assert_eq!(arr[3]["name"], serde_json::Value::Null);

    // NDJSON with schema.json: a zip.
    let dest = dir.path().join("o.zip");
    let report = run_export(
        context(session.clone(), Recorder::new()),
        spec(
            tables(&["orders"]),
            ExportContent::Both,
            ExportFormat::Json(JsonOptions {
                layout: JsonLayout::Ndjson,
                ..JsonOptions::default()
            }),
            &dest,
        ),
    )
    .await
    .unwrap();
    let entries = zip_entries(&report.files[0]);
    assert_eq!(entries[0].0, "orders.ndjson");
    assert_eq!(entries[1].0, "schema.json");
    let lines: Vec<&str> = std::str::from_utf8(&entries[0].1)
        .unwrap()
        .lines()
        .collect();
    assert_eq!(lines.len(), 4);
    for l in &lines {
        serde_json::from_str::<serde_json::Value>(l).unwrap();
    }
    let schema: serde_json::Value = serde_json::from_slice(&entries[1].1).unwrap();
    assert_eq!(schema["tables"][0]["name"], "orders");
    assert_eq!(schema["tables"][0]["primaryKey"][0], "id");
    assert_eq!(schema["tables"][0]["columns"][1]["comment"], "display name");

    // Pretty array with a field filter.
    let dest = dir.path().join("p.json");
    let report = run_export(
        context(session, Recorder::new()),
        spec(
            tables(&["orders"]),
            ExportContent::Data,
            ExportFormat::Json(JsonOptions {
                pretty: true,
                fields: vec!["id".into()],
                ..JsonOptions::default()
            }),
            &dest,
        ),
    )
    .await
    .unwrap();
    let text = String::from_utf8(read(&report.files[0])).unwrap();
    assert!(text.starts_with("[\n  {\n    \"id\": 1\n  },"), "{text}");
    serde_json::from_str::<serde_json::Value>(&text).unwrap();
}

#[tokio::test]
async fn json_schema_only_is_a_plain_file() {
    let dir = tempfile::tempdir().unwrap();
    let session = FakeSession::with_tables(&[("orders", 4)]);
    let dest = dir.path().join("schema.json");
    let report = run_export(
        context(session.clone(), Recorder::new()),
        spec(
            tables(&["orders"]),
            ExportContent::Schema,
            ExportFormat::Json(JsonOptions::default()),
            &dest,
        ),
    )
    .await
    .unwrap();
    let schema: serde_json::Value = serde_json::from_slice(&read(&report.files[0])).unwrap();
    assert_eq!(schema["tables"].as_array().unwrap().len(), 1);
    assert_eq!(report.rows, 0);
    assert!(
        session.statements().is_empty(),
        "schema only reads no data: {:?}",
        session.statements()
    );
}

fn sheet_names(path: &str) -> Vec<String> {
    use calamine::Reader;
    calamine::open_workbook_auto(path).unwrap().sheet_names()
}

fn sheet_rows(path: &str, sheet: &str) -> Vec<Vec<calamine::Data>> {
    use calamine::Reader;
    calamine::open_workbook_auto(path)
        .unwrap()
        .worksheet_range(sheet)
        .unwrap()
        .rows()
        .map(<[_]>::to_vec)
        .collect()
}

#[tokio::test]
async fn xlsx_splits_sheets_and_adds_dictionary() {
    let dir = tempfile::tempdir().unwrap();
    let session = FakeSession::with_tables(&[("orders", 10)]);
    let dest = dir.path().join("o.xlsx");
    let report = run_export_tuned(
        context(session, Recorder::new()),
        spec(
            tables(&["orders"]),
            ExportContent::Both,
            ExportFormat::Xlsx(XlsxOptions::default()),
            &dest,
        ),
        ExportTuning { xlsx_sheet_rows: 4 },
    )
    .await
    .unwrap();
    let path = &report.files[0];
    assert_eq!(
        sheet_names(path),
        ["orders", "orders (2)", "orders (3)", "Data dictionary"]
    );
    // Header repeated on every part; 4 + 4 + 2 data rows.
    assert_eq!(sheet_rows(path, "orders").len(), 5);
    assert_eq!(sheet_rows(path, "orders (2)").len(), 5);
    let last = sheet_rows(path, "orders (3)");
    assert_eq!(last.len(), 3);
    assert_eq!(last[0][0], calamine::Data::String("id".into()));
    assert_eq!(last[1][0], calamine::Data::Float(9.0));
    // Decimals stay text.
    assert_eq!(last[1][2], calamine::Data::String("9.50".into()));
    let dict = sheet_rows(path, "Data dictionary");
    assert_eq!(dict.len(), 4, "header and the three columns");
    assert_eq!(dict[2][1], calamine::Data::String("name".into()));
    assert_eq!(dict[2][7], calamine::Data::String("display name".into()));
    assert_eq!(report.rows, 10);
}

#[tokio::test]
async fn xlsx_default_has_no_dictionary_for_data_only_and_unique_sheet_names() {
    let dir = tempfile::tempdir().unwrap();
    let session =
        FakeSession::with_tables(&[("a_very_long_table_name_beyond_thirty_one", 1), ("b", 1)]);
    let dest = dir.path().join("o.xlsx");
    let report = run_export(
        context(session, Recorder::new()),
        spec(
            ExportSource::Schema {
                path: schema_path(),
            },
            ExportContent::Data,
            ExportFormat::Xlsx(XlsxOptions::default()),
            &dest,
        ),
    )
    .await
    .unwrap();
    let names = sheet_names(&report.files[0]);
    assert_eq!(names.len(), 2);
    assert!(names.iter().all(|n| n.chars().count() <= 31));
    assert!(!names.contains(&"Data dictionary".to_string()));
}

#[tokio::test]
async fn sql_both_orders_ddl_data_and_batches_inserts() {
    let dir = tempfile::tempdir().unwrap();
    let session = FakeSession::with_tables(&[("orders", 5)]);
    let dest = dir.path().join("o.sql");
    let opts = SqlOptions {
        insert_batch_rows: 2,
        ..SqlOptions::default()
    };
    let report = run_export(
        context(session, Recorder::new()),
        spec(
            tables(&["orders"]),
            ExportContent::Both,
            ExportFormat::Sql(opts),
            &dest,
        ),
    )
    .await
    .unwrap();
    let sql = String::from_utf8(read(&report.files[0])).unwrap();
    let pos = |needle: &str| {
        sql.find(needle)
            .unwrap_or_else(|| panic!("{needle} in\n{sql}"))
    };
    assert!(pos("CREATE TABLE") < pos("BEGIN;"));
    assert!(pos("BEGIN;") < pos("INSERT INTO"));
    assert!(pos("INSERT INTO") < pos("COMMIT;"));
    assert!(pos("COMMIT;") < pos("CREATE INDEX"));
    assert_eq!(
        sql.matches("INSERT INTO").count(),
        3,
        "5 rows in batches of 2"
    );
    assert!(sql.contains("INSERT INTO \"public\".\"orders\" (\"id\", \"name\", \"price\") VALUES\n(1, 'name 1, \"quoted\"', 1.50),\n(2,"));
    assert!(sql.contains("(4, NULL, 4.50)"));
    assert!(
        sql.trim_end()
            .ends_with("CREATE INDEX \"orders_idx\" ON \"public\".\"orders\" (name);")
    );
}

#[tokio::test]
async fn sql_data_only_and_schema_only() {
    let dir = tempfile::tempdir().unwrap();
    let session = FakeSession::with_tables(&[("orders", 2)]);
    let data = run_export(
        context(session.clone(), Recorder::new()),
        spec(
            tables(&["orders"]),
            ExportContent::Data,
            ExportFormat::Sql(SqlOptions {
                ddl: DdlOptions {
                    qualify_schema: false,
                    ..DdlOptions::default()
                },
                wrap_in_transaction: false,
                include_column_names: false,
                ..SqlOptions::default()
            }),
            &dir.path().join("d.sql"),
        ),
    )
    .await
    .unwrap();
    let sql = String::from_utf8(read(&data.files[0])).unwrap();
    assert!(!sql.contains("CREATE"));
    assert!(!sql.contains("BEGIN"));
    assert!(sql.contains("INSERT INTO \"orders\" VALUES\n(1,"), "{sql}");

    let schema = run_export(
        context(session, Recorder::new()),
        spec(
            tables(&["orders"]),
            ExportContent::Schema,
            ExportFormat::Sql(SqlOptions::default()),
            &dir.path().join("s.sql"),
        ),
    )
    .await
    .unwrap();
    let sql = String::from_utf8(read(&schema.files[0])).unwrap();
    assert!(sql.contains("CREATE TABLE") && sql.contains("CREATE INDEX"));
    assert!(!sql.contains("INSERT"));
    assert_eq!(schema.rows, 0);
}

#[tokio::test]
async fn query_source_works_for_every_format() {
    let dir = tempfile::tempdir().unwrap();
    let session = FakeSession::with_tables(&[("orders", 1)]);
    let query = || ExportSource::Query {
        text: "SELECT 1 AS n UNION SELECT 2".into(),
        database: None,
    };
    let formats = [
        ("q.csv", ExportFormat::Csv(CsvOptions::default())),
        ("q.json", ExportFormat::Json(JsonOptions::default())),
        ("q.xlsx", ExportFormat::Xlsx(XlsxOptions::default())),
        ("q.sql", ExportFormat::Sql(SqlOptions::default())),
    ];
    for (name, format) in formats {
        let report = run_export(
            context(session.clone(), Recorder::new()),
            spec(query(), ExportContent::Both, format, &dir.path().join(name)),
        )
        .await
        .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(report.rows, 2, "{name}");
        assert_eq!(report.files.len(), 1, "{name}");
        assert!(Path::new(&report.files[0]).ends_with(name), "{name}");
    }
    let csv = String::from_utf8(read(dir.path().join("q.csv").to_str().unwrap())).unwrap();
    assert!(csv.contains("n\r\n1\r\n2\r\n"));
    let sql = String::from_utf8(read(dir.path().join("q.sql").to_str().unwrap())).unwrap();
    assert!(
        sql.contains("INSERT INTO \"query\" (\"n\") VALUES\n(1),\n(2);"),
        "{sql}"
    );
}

#[tokio::test]
async fn a_query_that_writes_is_refused_and_leaves_no_file() {
    let dir = tempfile::tempdir().unwrap();
    let session = FakeSession::with_tables(&[("orders", 1)]);
    let dest = dir.path().join("q.csv");
    let err = run_export(
        context(session.clone(), Recorder::new()),
        spec(
            ExportSource::Query {
                text: "DELETE FROM orders".into(),
                database: None,
            },
            ExportContent::Data,
            ExportFormat::Csv(CsvOptions::default()),
            &dest,
        ),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, Error::ReadOnlyViolation { .. }), "{err:?}");
    assert!(leftovers(dir.path()).is_empty());
    assert!(session.statements().is_empty());
}

#[tokio::test]
async fn cancelling_mid_stream_leaves_nothing_behind() {
    let dir = tempfile::tempdir().unwrap();
    let session = FakeSession::with_tables(&[("orders", 30)]);
    for (name, format) in [
        ("c.csv", ExportFormat::Csv(CsvOptions::default())),
        ("c.json", ExportFormat::Json(JsonOptions::default())),
        ("c.sql", ExportFormat::Sql(SqlOptions::default())),
        ("c.xlsx", ExportFormat::Xlsx(XlsxOptions::default())),
    ] {
        // A fresh token per format: the previous one stays cancelled.
        let reporter = Recorder::new();
        *session.cancel_after.lock().unwrap() = Some((6, reporter.token.clone()));
        let err = run_export(
            context(session.clone(), reporter),
            spec(
                tables(&["orders"]),
                ExportContent::Data,
                format,
                &dir.path().join(name),
            ),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, Error::Cancelled), "{name}: {err:?}");
        assert!(
            leftovers(dir.path()).is_empty(),
            "{name}: {:?}",
            leftovers(dir.path())
        );
    }
}

#[tokio::test]
async fn a_cancelled_token_stops_before_anything_is_written() {
    let dir = tempfile::tempdir().unwrap();
    let session = FakeSession::with_tables(&[("orders", 3)]);
    let reporter = Recorder::new();
    reporter.token.cancel();
    let err = run_export(
        context(session, reporter),
        spec(
            tables(&["orders"]),
            ExportContent::Data,
            ExportFormat::Csv(CsvOptions::default()),
            &dir.path().join("a.csv"),
        ),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, Error::Cancelled));
    assert!(leftovers(dir.path()).is_empty());
}

#[tokio::test]
async fn existing_destination_needs_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("o.csv");
    std::fs::write(&dest, "old").unwrap();
    let session = FakeSession::with_tables(&[("orders", 1)]);
    let mut s = spec(
        tables(&["orders"]),
        ExportContent::Data,
        ExportFormat::Csv(CsvOptions::default()),
        &dest,
    );
    let err = run_export(context(session.clone(), Recorder::new()), s.clone())
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Io(_)), "{err:?}");
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), "old");
    s.destination.overwrite = true;
    run_export(context(session, Recorder::new()), s)
        .await
        .unwrap();
    assert_ne!(std::fs::read_to_string(&dest).unwrap(), "old");
    assert_eq!(leftovers(dir.path()), ["o.csv"]);
}

#[tokio::test]
async fn progress_reports_rows_and_phases() {
    let dir = tempfile::tempdir().unwrap();
    let session = FakeSession::with_tables(&[("orders", 7), ("items", 2)]);
    let reporter = Recorder::new();
    run_export(
        context(session, reporter.clone()),
        spec(
            tables(&["orders", "items"]),
            ExportContent::Data,
            ExportFormat::Csv(CsvOptions::default()),
            &dir.path().join("p.zip"),
        ),
    )
    .await
    .unwrap();
    let seen = reporter.seen.lock().unwrap();
    let last = seen.iter().rev().find(|p| p.phase == "data").unwrap();
    assert_eq!(last.rows_done, 9);
    assert_eq!(last.rows_total, Some(9));
    assert_eq!(last.objects_done, 2);
    assert_eq!(last.objects_total, Some(2));
    assert!(seen.iter().any(|p| p.object.as_deref() == Some("orders")));
    assert!(seen.windows(2).all(|w| w[0].rows_done <= w[1].rows_done));
}

#[tokio::test]
async fn invalid_requests_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let session = FakeSession::with_tables(&[("orders", 1)]);
    let csv = ExportFormat::Csv(CsvOptions::default());
    let err = run_export(
        context(session.clone(), Recorder::new()),
        spec(
            ExportSource::Keys {
                database: ObjectPath::root(),
                pattern: "*".into(),
            },
            ExportContent::Data,
            ExportFormat::Json(JsonOptions::default()),
            &dir.path().join("k.json"),
        ),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, Error::Unsupported(_)));
    let err = run_export(
        context(session.clone(), Recorder::new()),
        spec(
            ExportSource::Tables { paths: vec![] },
            ExportContent::Data,
            csv.clone(),
            &dir.path().join("n.csv"),
        ),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, Error::Other(_)));
    let err = run_export(
        context(session, Recorder::new()),
        spec(
            tables(&["missing"]),
            ExportContent::Data,
            csv,
            &dir.path().join("m.csv"),
        ),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, Error::NotFound(_)));
    assert!(leftovers(dir.path()).is_empty());
}
