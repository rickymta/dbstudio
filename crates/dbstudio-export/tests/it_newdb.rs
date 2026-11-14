//! Export / import round trips on the newer databases (ScyllaDB, Cassandra, ClickHouse,
//! CockroachDB, YugabyteDB, TiDB). Only with `DBSTUDIO_IT=1`; a server that is not running skips
//! its test (fails with `DBSTUDIO_IT_STRICT=1`).

mod common;
#[path = "it_newdb/fixtures.rs"]
mod fixtures;

use std::path::Path;

use dbstudio_core::DdlOptions;
use dbstudio_export::{
    CsvOptions, ExportContent, ExportContext, ExportFormat, ImportFormat, ImportMode, ImportSpec,
    ImportTarget, JsonLayout, JsonOptions, SqlOptions, TextEncoding, XlsxOptions, run_export,
    run_import,
};
use fixtures::{Dst, E, Fx, probe};
use uuid::Uuid;

use common::Recorder;

fn ctx(fx: &Fx, d: &Dst) -> ExportContext {
    ExportContext {
        session: d.session.clone(),
        kind: fx.e.kind(),
        dialect: d.session.script_dialect(),
        reporter: Recorder::new(),
    }
}

fn spec(
    source: dbstudio_export::ExportSource,
    content: ExportContent,
    format: ExportFormat,
    dest: &Path,
) -> dbstudio_export::ExportSpec {
    dbstudio_export::ExportSpec {
        connection_id: Uuid::nil(),
        source,
        content,
        format,
        destination: dbstudio_export::ExportDestination {
            path: dest.to_string_lossy().into_owned(),
            overwrite: false,
        },
        values: dbstudio_export::ValueFormat::default(),
    }
}

fn assert_same(
    label: &str,
    what: &str,
    want: &[(String, Vec<String>)],
    got: &[(String, Vec<String>)],
) {
    for ((t, w), (_, g)) in want.iter().zip(got) {
        assert_eq!(w.len(), g.len(), "[{label}] {what}: row count of {t}");
        for (i, (a, b)) in w.iter().zip(g).enumerate() {
            assert_eq!(a, b, "[{label}] {what}: {t} differs at sorted row {i}");
        }
    }
}

/// Structure and rows as one SQL script replay into an empty namespace.
async fn sql_round_trip(fx: &Fx) {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("dump.sql");
    let report = run_export(
        ctx(fx, &fx.src),
        spec(
            fx.source(&fx.src),
            ExportContent::Both,
            ExportFormat::Sql(SqlOptions {
                ddl: DdlOptions {
                    qualify_schema: false,
                    ..DdlOptions::default()
                },
                insert_batch_rows: 100,
                ..SqlOptions::default()
            }),
            &file,
        ),
    )
    .await
    .unwrap_or_else(|e| panic!("[{}] sql export: {e}", fx.label));
    assert_eq!(report.rows, fx.total_rows(), "[{}] exported rows", fx.label);
    let dst = fx.fresh("sql").await;
    let imported = run_import(
        ctx(fx, &dst),
        ImportSpec {
            connection_id: Uuid::nil(),
            file: file.to_string_lossy().into_owned(),
            format: ImportFormat::Sql {
                continue_on_error: false,
            },
            target: ImportTarget::Script {
                database: dst.script_db.clone(),
            },
            mapping: vec![],
            on_conflict: None,
            mode: ImportMode::Batches { rows: 500 },
            max_errors: 0,
        },
    )
    .await
    .unwrap_or_else(|e| {
        let sql = std::fs::read_to_string(&file).unwrap();
        panic!(
            "[{}] sql replay: {e}\n{}",
            fx.label,
            &sql[..sql.len().min(6000)]
        )
    });
    assert!(
        imported.errors.is_empty(),
        "[{}] {:?}",
        fx.label,
        imported.errors
    );
    assert_same(
        &fx.label,
        "sql",
        &fx.contents(&fx.src).await,
        &fx.contents(&dst).await,
    );
}

/// CSV, JSON and XLSX files imported back into empty tables of the same shape.
async fn file_round_trips(fx: &Fx) {
    let dir = tempfile::tempdir().unwrap();
    let want = fx.contents(&fx.src).await;
    let cases: [(&str, ExportFormat, ImportFormat); 4] = [
        (
            "csv",
            ExportFormat::Csv(CsvOptions::default()),
            ImportFormat::Csv {
                delimiter: ',',
                quote: '"',
                header: true,
                encoding: TextEncoding::Utf8Bom,
                null_text: None,
            },
        ),
        (
            "json",
            ExportFormat::Json(JsonOptions::default()),
            ImportFormat::Json {
                layout: JsonLayout::Array,
            },
        ),
        (
            "ndjson",
            ExportFormat::Json(JsonOptions {
                layout: JsonLayout::Ndjson,
                ..JsonOptions::default()
            }),
            ImportFormat::Json {
                layout: JsonLayout::Ndjson,
            },
        ),
        (
            "xlsx",
            ExportFormat::Xlsx(XlsxOptions::default()),
            ImportFormat::Xlsx {
                sheet: String::new(),
                header: true,
            },
        ),
    ];
    for (name, export, import) in cases {
        let dst = fx.fresh(name).await;
        fx.create_schema(&dst).await;
        for (t, rows) in fx.tables() {
            let file = dir.path().join(format!("{t}.{name}"));
            run_export(
                ctx(fx, &fx.src),
                spec(
                    dbstudio_export::ExportSource::Tables {
                        paths: vec![fx.table(&fx.src, t)],
                    },
                    ExportContent::Data,
                    export.clone(),
                    &file,
                ),
            )
            .await
            .unwrap_or_else(|e| panic!("[{}] {name} export of {t}: {e}", fx.label));
            let report = run_import(
                ctx(fx, &dst),
                ImportSpec {
                    connection_id: Uuid::nil(),
                    file: file.to_string_lossy().into_owned(),
                    format: import.clone(),
                    target: ImportTarget::Existing {
                        table: fx.table(&dst, t),
                    },
                    mapping: vec![],
                    on_conflict: None,
                    mode: ImportMode::Batches { rows: 100 },
                    max_errors: 0,
                },
            )
            .await
            .unwrap_or_else(|e| {
                let head: String = std::fs::read(&file)
                    .map(|b| String::from_utf8_lossy(&b[..b.len().min(1500)]).into_owned())
                    .unwrap_or_default();
                panic!("[{}] {name} import of {t}: {e}\n{head}", fx.label)
            });
            assert_eq!(
                report.inserted, rows,
                "[{}] {name} {t}: {:?}",
                fx.label, report.errors
            );
        }
        assert_same(&fx.label, name, &want, &fx.contents(&dst).await);
    }
}

async fn all(e: E, label: &str) {
    if !probe(e) {
        return;
    }
    let fx = Fx::new(e, label).await;
    sql_round_trip(&fx).await;
    file_round_trips(&fx).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn clickhouse() {
    all(E::ClickHouse, "ch").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn tidb() {
    all(E::TiDb, "tidb").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn scylla() {
    all(E::Scylla, "scylla").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cassandra5() {
    all(E::Cassandra, "cas5").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cockroach() {
    all(E::Cockroach, "crdb").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn yugabyte() {
    all(E::Yugabyte, "yb").await;
}
