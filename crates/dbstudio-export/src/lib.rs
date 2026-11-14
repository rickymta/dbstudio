//! Export of tables / queries to SQL, CSV, XLSX and JSON, and import from files.
//!
//! `run_export` streams tables or query results through a [`TableWriter`] into a temporary file
//! that is renamed when the export completes; `run_import` reads CSV / XLSX / JSON / SQL files and
//! loads them into a table (or runs the script).

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use dbstudio_core::{
    ColumnMeta, DbKind, DdlItem, DdlOptions, DdlSection, JobReporter, ObjectPath, OnConflict,
    PutMode, Result, ResultSink, ScriptDialect, Session, StatementInfo, TableModel, Value,
    ValueKind,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub use dbstudio_sql::ddl::NewColumn;

mod conn;
mod engine;
mod fsio;
mod import;
mod values;
mod writers;

pub use engine::{run_export, run_export_tuned};
pub use fsio::cleanup_orphans;

/// Orphan temporary files older than this are removed.
const ORPHAN_MIN_AGE: std::time::Duration = std::time::Duration::from_secs(3600);
/// Delay before the startup cleanup touches the disk, so it never competes with startup.
const STARTUP_CLEANUP_DELAY: std::time::Duration = std::time::Duration::from_secs(10);

/// Remove orphan temporary export files left in the recently used export directories
/// (`export.recentDirs` setting) by a crash. Called once after startup: the list is read right
/// away, the directories are visited on a background thread 10 seconds later (only files named
/// `.<name>.dbstudio-tmp-<id>` older than an hour are removed). Directories that no longer exist
/// leave the list the next time an export records its directory.
pub fn startup_cleanup(store: &dbstudio_store::Store) {
    let Ok(dirs) = store.export_recent_dirs() else {
        return;
    };
    if dirs.is_empty() {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("export-orphan-cleanup".into())
        .spawn(move || {
            std::thread::sleep(STARTUP_CLEANUP_DELAY);
            cleanup_dirs(&dirs, ORPHAN_MIN_AGE);
        });
    drop(spawned);
}

/// Run [`cleanup_orphans`] over `dirs`; returns the number of files removed.
fn cleanup_dirs(dirs: &[String], older_than: std::time::Duration) -> usize {
    dirs.iter()
        .map(|d| cleanup_orphans(std::path::Path::new(d), older_than))
        .sum()
}
pub use import::{preview_import, run_import};

// ---------------------------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------------------------

/// What to export, in which format, where.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportSpec {
    /// Connection to read from.
    pub connection_id: Uuid,
    /// Objects or query.
    pub source: ExportSource,
    /// Schema, data or both.
    pub content: ExportContent,
    /// Output format.
    pub format: ExportFormat,
    /// Output file.
    pub destination: ExportDestination,
    /// Value rendering.
    #[serde(default)]
    pub values: ValueFormat,
}

/// Where the data comes from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ExportSource {
    /// Every table of a database.
    Database {
        /// Database node.
        path: ObjectPath,
    },
    /// Every table of a schema.
    Schema {
        /// Schema node.
        path: ObjectPath,
    },
    /// Chosen tables.
    Tables {
        /// Table nodes.
        paths: Vec<ObjectPath>,
    },
    /// Result of a query.
    Query {
        /// SQL / shell text.
        text: String,
        /// Database to run it in.
        #[serde(default)]
        database: Option<String>,
    },
    /// Redis keys matching a pattern (JSON only).
    Keys {
        /// Redis database node.
        database: ObjectPath,
        /// `SCAN MATCH` pattern.
        pattern: String,
    },
}

/// Part of the objects to export.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ExportContent {
    /// Structure only.
    Schema,
    /// Rows only.
    Data,
    /// Both.
    Both,
}

/// Output format with its options.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ExportFormat {
    /// SQL script.
    Sql(SqlOptions),
    /// Excel workbook.
    Xlsx(XlsxOptions),
    /// Delimited text.
    Csv(CsvOptions),
    /// JSON.
    Json(JsonOptions),
}

/// SQL script options.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SqlOptions {
    /// DDL generation options.
    pub ddl: DdlOptions,
    /// Rows per multi-row `INSERT`.
    pub insert_batch_rows: u32,
    /// Maximum size of one statement in bytes.
    pub max_statement_bytes: u32,
    /// Wrap the data in `BEGIN` / `COMMIT`.
    pub wrap_in_transaction: bool,
    /// Disable foreign key checks while loading.
    pub disable_fk_checks: bool,
    /// Write column names in `INSERT`.
    pub include_column_names: bool,
}

impl Default for SqlOptions {
    fn default() -> Self {
        SqlOptions {
            ddl: DdlOptions::default(),
            insert_batch_rows: 100,
            max_statement_bytes: 1024 * 1024,
            wrap_in_transaction: true,
            disable_fk_checks: false,
            include_column_names: true,
        }
    }
}

/// Excel options.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct XlsxOptions {
    /// Add a `Data dictionary` sheet.
    pub data_dictionary: bool,
    /// Write a header row.
    pub header: bool,
    /// Freeze the header row.
    pub freeze_header: bool,
    /// Longer text is truncated to this many characters (Excel limit 32767).
    pub max_cell_chars: u32,
}

impl Default for XlsxOptions {
    fn default() -> Self {
        XlsxOptions {
            data_dictionary: false,
            header: true,
            freeze_header: true,
            max_cell_chars: 32_767,
        }
    }
}

/// CSV options.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CsvOptions {
    /// Field delimiter.
    pub delimiter: char,
    /// Quote character.
    pub quote: char,
    /// Quote every field.
    pub quote_all: bool,
    /// Write a header row.
    pub header: bool,
    /// File encoding.
    pub encoding: TextEncoding,
    /// Line terminator.
    pub line_ending: LineEnding,
    /// Text written for NULL.
    pub null_text: String,
    /// Prefix text cells that start with `=`, `+`, `-`, `@`, tab or CR with `'` so that Excel
    /// does not run them as formulas (CSV injection). Off by default: the file then imports back
    /// byte for byte.
    pub excel_safe: bool,
}

impl Default for CsvOptions {
    fn default() -> Self {
        CsvOptions {
            delimiter: ',',
            quote: '"',
            quote_all: false,
            header: true,
            encoding: TextEncoding::Utf8Bom,
            line_ending: LineEnding::Crlf,
            null_text: String::new(),
            excel_safe: false,
        }
    }
}

/// Text file encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TextEncoding {
    /// UTF-8.
    Utf8,
    /// UTF-8 with byte order mark (Excel friendly).
    Utf8Bom,
}

/// Line terminator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LineEnding {
    /// `\n`.
    Lf,
    /// `\r\n`.
    Crlf,
}

/// JSON options.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct JsonOptions {
    /// Array or one document per line.
    pub layout: JsonLayout,
    /// Extended JSON flavour for database specific values.
    pub extended_json: ExtendedJson,
    /// Indent the output.
    pub pretty: bool,
    /// Restrict to these fields; empty = all.
    pub fields: Vec<String>,
}

impl Default for JsonOptions {
    fn default() -> Self {
        JsonOptions {
            layout: JsonLayout::Array,
            extended_json: ExtendedJson::Off,
            pretty: false,
            fields: Vec::new(),
        }
    }
}

/// JSON layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum JsonLayout {
    /// One JSON array.
    Array,
    /// Newline delimited JSON.
    Ndjson,
}

/// Extended JSON flavour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ExtendedJson {
    /// Plain JSON.
    Off,
    /// Relaxed EJSON.
    Relaxed,
    /// Canonical EJSON.
    Canonical,
}

/// Output file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportDestination {
    /// File path, or `.zip` when several files are produced.
    pub path: String,
    /// Replace an existing file.
    #[serde(default)]
    pub overwrite: bool,
}

/// How cells are rendered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ValueFormat {
    /// Binary encoding in text formats.
    pub binary: BinaryFormat,
    /// Mongo CSV/XLSX dotted paths; empty = top-level keys of the first 1000 documents.
    pub flatten_fields: Vec<String>,
}

impl Default for ValueFormat {
    fn default() -> Self {
        ValueFormat {
            binary: BinaryFormat::Base64,
            flatten_fields: Vec::new(),
        }
    }
}

/// Binary text encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BinaryFormat {
    /// Base64.
    Base64,
    /// Hexadecimal.
    Hex,
}

/// Everything an export or import needs from the application.
pub struct ExportContext {
    /// Connection to read from / write to.
    pub session: Arc<dyn Session>,
    /// Database product.
    pub kind: DbKind,
    /// Script dialect of the session.
    pub dialect: ScriptDialect,
    /// Progress and cancellation.
    pub reporter: Arc<dyn JobReporter>,
}

impl fmt::Debug for ExportContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExportContext")
            .field("kind", &self.kind)
            .field("dialect", &self.dialect)
            .finish_non_exhaustive()
    }
}

/// Outcome of an export.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportReport {
    /// Files written.
    pub files: Vec<String>,
    /// Objects exported.
    pub objects: u32,
    /// Rows written.
    pub rows: u64,
    /// Bytes written.
    pub bytes: u64,
    /// Non fatal problems.
    pub warnings: Vec<String>,
}

/// Description of the object a [`TableWriter`] is about to receive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObjectMeta {
    /// Source path; `None` for a query.
    pub path: Option<ObjectPath>,
    /// Name used for the file entry / sheet / table.
    pub name: String,
    /// Result columns.
    pub columns: Vec<ColumnMeta>,
    /// Full definition when exporting schema too.
    pub details: Option<TableModel>,
    /// Estimated rows for progress.
    pub estimated_rows: Option<u64>,
}

impl ObjectMeta {
    /// Plain table name (without the schema prefix that `name` may carry).
    pub fn table_name(&self) -> &str {
        if let Some(d) = &self.details {
            return &d.name;
        }
        if let Some(p) = self.path.as_ref().and_then(|p| p.last()) {
            return &p.name;
        }
        &self.name
    }
}

/// Limits the engine applies; tests lower them to exercise rare paths with little data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExportTuning {
    /// Data rows per XLSX sheet before the next sheet is started (Excel allows 1,048,576 rows
    /// including the header).
    pub xlsx_sheet_rows: u32,
}

impl Default for ExportTuning {
    fn default() -> Self {
        ExportTuning {
            xlsx_sheet_rows: 1_048_575,
        }
    }
}

/// A format writer. Writers are result sinks so query results stream straight to disk.
pub trait TableWriter: ResultSink {
    /// Start a new object (table / sheet / file entry).
    fn begin_object(&mut self, meta: &ObjectMeta) -> Result<()>;
    /// Finish the current object.
    fn end_object(&mut self) -> Result<()>;
    /// Close the output and return the files produced.
    fn finish(self: Box<Self>) -> Result<Vec<PathBuf>>;
    /// Structure statements of the objects (only the SQL writer uses them).
    fn write_ddl(&mut self, section: DdlSection, items: &[DdlItem]) -> Result<()> {
        let _ = (section, items);
        Ok(())
    }
    /// Bytes written to disk so far (progress).
    fn bytes(&self) -> u64 {
        0
    }
    /// Non fatal problems noticed while writing (cut cells...).
    fn warnings(&mut self) -> Vec<String> {
        Vec::new()
    }
}

// ---------------------------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------------------------

/// What to import and where.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportSpec {
    /// Connection to write to.
    pub connection_id: Uuid,
    /// Source file.
    pub file: String,
    /// File format.
    pub format: ImportFormat,
    /// Destination.
    pub target: ImportTarget,
    /// Column mapping; ignored for SQL.
    #[serde(default)]
    pub mapping: Vec<ColumnMapping>,
    /// Conflict handling.
    #[serde(default)]
    pub on_conflict: Option<OnConflict>,
    /// Transaction strategy.
    pub mode: ImportMode,
    /// Errors tolerated before stopping; 0 = stop at the first.
    #[serde(default)]
    pub max_errors: u32,
}

/// Format of the file being imported.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ImportFormat {
    /// Delimited text.
    #[serde(rename_all = "camelCase")]
    Csv {
        /// Field delimiter.
        delimiter: char,
        /// Quote character.
        quote: char,
        /// First row is a header.
        header: bool,
        /// Encoding.
        encoding: TextEncoding,
        /// Text that stands for NULL. `None` keeps the old behaviour (an empty unquoted cell is
        /// NULL). `Some(t)`: an unquoted cell equal to `t` is NULL, an empty cell is an empty
        /// string, a quoted cell is always text (matches the CSV writer).
        #[serde(default)]
        null_text: Option<String>,
    },
    /// Excel sheet.
    Xlsx {
        /// Sheet name.
        sheet: String,
        /// First row is a header.
        header: bool,
    },
    /// JSON.
    Json {
        /// Array or NDJSON.
        layout: JsonLayout,
    },
    /// SQL script.
    #[serde(rename_all = "camelCase")]
    Sql {
        /// Keep going after a failing statement.
        continue_on_error: bool,
    },
}

/// Where imported rows go.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ImportTarget {
    /// An existing table.
    Existing {
        /// Table node.
        table: ObjectPath,
    },
    /// A table created by the import.
    New {
        /// Parent (database / schema) node.
        parent: ObjectPath,
        /// Table name.
        name: String,
        /// Columns.
        columns: Vec<NewColumn>,
    },
    /// SQL script run as is.
    Script {
        /// Database to run it in.
        #[serde(default)]
        database: Option<String>,
    },
    /// Redis keys: every record must have the shape of [`dbstudio_core::KeyRecord`].
    Keys {
        /// Database node (`[Database "0"]`).
        database: ObjectPath,
        /// What to do with a key that exists.
        #[serde(default)]
        mode: PutMode,
    },
}

/// File column to table column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ColumnMapping {
    /// Header name or `#3` (1-based position).
    pub source: String,
    /// Table column.
    pub target: String,
    /// Value transformation.
    #[serde(default)]
    pub transform: Transform,
}

/// Value transformation applied while importing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Transform {
    /// Leave as is.
    #[default]
    None,
    /// Trim whitespace.
    Trim,
    /// Empty string becomes NULL.
    EmptyAsNull,
    /// Trim, then empty string becomes NULL.
    TrimEmptyAsNull,
}

/// Transaction strategy of an import.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ImportMode {
    /// All or nothing.
    SingleTransaction,
    /// Commit every `rows` rows.
    Batches {
        /// Rows per batch.
        rows: u32,
    },
}

fn default_preview_rows() -> u32 {
    50
}

/// Request of [`preview_import`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportPreviewRequest {
    /// Source file.
    pub file: String,
    /// File format.
    pub format: ImportFormat,
    /// Rows to return (default 50).
    #[serde(default = "default_preview_rows")]
    pub rows: u32,
    /// Dialect of the target connection, used to split `.sql` files (guessed when absent).
    #[serde(default)]
    pub dialect: Option<ScriptDialect>,
    /// Connection the file is going to be imported into; the host resolves it to `dialect`.
    #[serde(default)]
    pub connection_id: Option<Uuid>,
}

/// First rows of a file with detected properties.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportPreview {
    /// Column names (or `#1`...).
    pub columns: Vec<String>,
    /// Sample rows.
    pub rows: Vec<Vec<Value>>,
    /// Sheet names (XLSX).
    pub sheets: Vec<String>,
    /// Delimiter guessed for CSV.
    pub detected_delimiter: Option<char>,
    /// Kind guessed per column.
    pub suggested_kinds: Vec<ValueKind>,
    /// Estimated number of rows.
    pub estimated_rows: Option<u64>,
    /// SQL: first 50 statements.
    pub statements_preview: Vec<StatementInfo>,
    /// The dialect was guessed from the connection kind (no live session, no remembered one).
    #[serde(default)]
    pub dialect_guessed: bool,
}

/// Outcome of an import.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportReport {
    /// Rows (or statements) read.
    pub rows_read: u64,
    /// Rows inserted.
    pub inserted: u64,
    /// Rows updated.
    pub updated: u64,
    /// Rows skipped.
    pub skipped: u64,
    /// Failed rows.
    pub errors: Vec<RowError>,
}

/// One failed row or statement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RowError {
    /// 1-based line / row / statement number in the file.
    pub line: u64,
    /// Failure text.
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_core::testing::write_or_check;
    use dbstudio_core::{CancelToken, ColumnDetail, Error, JobProgress, ObjectKind};
    use serde_json::{Value as Json, json};

    fn j<T: Serialize>(v: &T) -> Json {
        serde_json::to_value(v).unwrap()
    }

    fn id() -> Uuid {
        Uuid::from_u128(7)
    }

    fn table(name: &str) -> ObjectPath {
        ObjectPath::root()
            .child(ObjectKind::Database, "app")
            .child(ObjectKind::Folder, "tables")
            .child(ObjectKind::Table, name)
    }

    fn export_spec() -> ExportSpec {
        ExportSpec {
            connection_id: id(),
            source: ExportSource::Tables {
                paths: vec![table("orders")],
            },
            content: ExportContent::Both,
            format: ExportFormat::Csv(CsvOptions::default()),
            destination: ExportDestination {
                path: "/tmp/orders.zip".into(),
                overwrite: false,
            },
            values: ValueFormat::default(),
        }
    }

    fn import_spec() -> ImportSpec {
        ImportSpec {
            connection_id: id(),
            file: "/tmp/in.csv".into(),
            format: ImportFormat::Csv {
                delimiter: ';',
                quote: '"',
                header: true,
                encoding: TextEncoding::Utf8,
                null_text: None,
            },
            target: ImportTarget::New {
                parent: ObjectPath::root().child(ObjectKind::Database, "app"),
                name: "t".into(),
                columns: vec![NewColumn {
                    name: "id".into(),
                    type_name: "bigint".into(),
                    nullable: false,
                    primary_key: true,
                }],
            },
            mapping: vec![ColumnMapping {
                source: "#1".into(),
                target: "id".into(),
                transform: Transform::Trim,
            }],
            on_conflict: Some(OnConflict::Skip),
            mode: ImportMode::Batches { rows: 500 },
            max_errors: 10,
        }
    }

    fn column_meta() -> ColumnMeta {
        ColumnMeta {
            name: "id".into(),
            type_name: "int8".into(),
            value_kind: ValueKind::Int,
            nullable: Some(false),
        }
    }

    fn object_meta() -> ObjectMeta {
        ObjectMeta {
            path: Some(table("orders")),
            name: "orders".into(),
            columns: vec![column_meta()],
            details: Some(TableModel {
                path: table("orders"),
                kind: ObjectKind::Table,
                schema: Some("public".into()),
                name: "orders".into(),
                comment: None,
                columns: vec![ColumnDetail {
                    name: "id".into(),
                    ordinal: 1,
                    type_name: "int8".into(),
                    value_kind: ValueKind::Int,
                    nullable: false,
                    default: None,
                    comment: None,
                    primary_key: true,
                    extra: None,
                }],
                primary_key: vec!["id".into()],
                unique_keys: vec![],
                estimated_rows: Some(10),
            }),
            estimated_rows: Some(10),
        }
    }

    fn export_report() -> ExportReport {
        ExportReport {
            files: vec!["/tmp/orders.zip".into()],
            objects: 1,
            rows: 10,
            bytes: 2048,
            warnings: vec!["truncated cell".into()],
        }
    }

    fn import_report() -> ImportReport {
        ImportReport {
            rows_read: 10,
            inserted: 8,
            updated: 1,
            skipped: 0,
            errors: vec![row_error()],
        }
    }

    fn row_error() -> RowError {
        RowError {
            line: 4,
            message: "duplicate key".into(),
        }
    }

    fn import_preview() -> ImportPreview {
        ImportPreview {
            columns: vec!["id".into(), "name".into()],
            rows: vec![vec![Value::Text("1".into()), Value::Text("a".into())]],
            sheets: vec![],
            detected_delimiter: Some(';'),
            suggested_kinds: vec![ValueKind::Int, ValueKind::Text],
            estimated_rows: Some(1000),
            statements_preview: vec![],
            dialect_guessed: false,
        }
    }

    #[test]
    fn defaults_follow_the_design() {
        let s = SqlOptions::default();
        assert_eq!((s.insert_batch_rows, s.max_statement_bytes), (100, 1 << 20));
        let c = CsvOptions::default();
        assert_eq!((c.delimiter, c.quote), (',', '"'));
        assert_eq!(c.encoding, TextEncoding::Utf8Bom);
        assert_eq!(c.line_ending, LineEnding::Crlf);
        assert_eq!(c.null_text, "");
        let x = XlsxOptions::default();
        assert_eq!(x.max_cell_chars, 32_767);
        assert!(x.header && x.freeze_header && !x.data_dictionary);
        let jo = JsonOptions::default();
        assert_eq!(jo.layout, JsonLayout::Array);
        assert_eq!(jo.extended_json, ExtendedJson::Off);
        assert_eq!(ValueFormat::default().binary, BinaryFormat::Base64);
    }

    #[test]
    fn partial_json_takes_defaults() {
        let f: ExportFormat = serde_json::from_value(json!({"kind": "csv"})).unwrap();
        assert_eq!(f, ExportFormat::Csv(CsvOptions::default()));
        let f: ExportFormat =
            serde_json::from_value(json!({"kind": "sql", "insertBatchRows": 5})).unwrap();
        match f {
            ExportFormat::Sql(o) => {
                assert_eq!(o.insert_batch_rows, 5);
                assert_eq!(o.max_statement_bytes, 1 << 20);
            }
            other => panic!("{other:?}"),
        }
        let r: ImportPreviewRequest = serde_json::from_value(
            json!({"file": "a", "format": {"kind": "json", "layout": "ndjson"}}),
        )
        .unwrap();
        assert_eq!(r.rows, 50);
    }

    #[test]
    fn json_round_trips() {
        let e = j(&export_spec());
        assert_eq!(e["format"]["kind"], "csv");
        assert_eq!(e["source"]["kind"], "tables");
        assert_eq!(
            serde_json::from_value::<ExportSpec>(e).unwrap(),
            export_spec()
        );
        let i = j(&import_spec());
        assert_eq!(i["mode"], json!({"kind": "batches", "rows": 500}));
        assert_eq!(i["target"]["kind"], "new");
        assert_eq!(
            serde_json::from_value::<ImportSpec>(i).unwrap(),
            import_spec()
        );
        let p = j(&import_preview());
        assert_eq!(
            serde_json::from_value::<ImportPreview>(p).unwrap(),
            import_preview()
        );
        let r = j(&import_report());
        assert_eq!(r["rowsRead"], 10);
        assert_eq!(
            serde_json::from_value::<ImportReport>(r).unwrap(),
            import_report()
        );
        let m = j(&object_meta());
        assert_eq!(
            serde_json::from_value::<ObjectMeta>(m).unwrap(),
            object_meta()
        );
        assert_eq!(
            j(&ImportFormat::Sql {
                continue_on_error: true
            }),
            json!({"kind": "sql", "continueOnError": true})
        );
    }

    struct Reporter(CancelToken);
    impl JobReporter for Reporter {
        fn progress(&self, _p: &JobProgress) {}
        fn cancel_token(&self) -> &CancelToken {
            &self.0
        }
    }

    struct NoSession;
    #[async_trait::async_trait]
    impl Session for NoSession {
        async fn ping(&self) -> Result<()> {
            Ok(())
        }
        async fn server_info(&self) -> Result<dbstudio_core::ServerInfo> {
            Ok(Default::default())
        }
        async fn execute(
            &self,
            _req: dbstudio_core::QueryRequest,
            _sink: &mut dyn ResultSink,
        ) -> Result<dbstudio_core::ExecSummary> {
            Err(Error::Unsupported("test".into()))
        }
        async fn list_children(&self, _p: &ObjectPath) -> Result<Vec<dbstudio_core::ObjectNode>> {
            Ok(vec![])
        }
        async fn close(&self) -> Result<()> {
            Ok(())
        }
    }

    fn ctx() -> ExportContext {
        ExportContext {
            session: Arc::new(NoSession),
            kind: DbKind::Postgres,
            dialect: ScriptDialect::Postgres,
            reporter: Arc::new(Reporter(CancelToken::default())),
        }
    }

    #[test]
    fn cleanup_visits_every_recent_dir_and_spares_other_files() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        for d in [&a, &b] {
            std::fs::write(d.path().join(".out.csv.dbstudio-tmp-1"), "x").unwrap();
            std::fs::write(d.path().join("keep.csv"), "x").unwrap();
            std::fs::write(d.path().join(".hidden"), "x").unwrap();
        }
        let dirs: Vec<String> = [a.path(), b.path(), &a.path().join("missing")]
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        // Fresh files are spared with the real age limit.
        assert_eq!(cleanup_dirs(&dirs, ORPHAN_MIN_AGE), 0);
        assert_eq!(cleanup_dirs(&dirs, std::time::Duration::ZERO), 2);
        for d in [&a, &b] {
            assert!(d.path().join("keep.csv").exists());
            assert!(d.path().join(".hidden").exists());
            assert!(!d.path().join(".out.csv.dbstudio-tmp-1").exists());
        }
    }

    #[test]
    fn startup_cleanup_without_directories_does_nothing() {
        let store = dbstudio_store::Store::open_in_memory().unwrap();
        startup_cleanup(&store);
    }

    #[test]
    fn context_debug_is_available() {
        assert!(format!("{:?}", ctx()).contains("ExportContext"));
        assert_eq!(ExportTuning::default().xlsx_sheet_rows, 1_048_575);
    }

    #[test]
    fn fixtures() {
        write_or_check(
            "export",
            &json!({
                "ExportSpec": j(&export_spec()),
                "ImportSpec": j(&import_spec()),
                "ImportPreview": j(&import_preview()),
                "ImportPreview#guessed": j(&ImportPreview {
                    dialect_guessed: true,
                    ..import_preview()
                }),
                "ImportSpec#csvNullText": j(&ImportSpec {
                    format: ImportFormat::Csv {
                        delimiter: ';',
                        quote: '"',
                        header: true,
                        encoding: TextEncoding::Utf8,
                        null_text: Some("\\N".into()),
                    },
                    ..import_spec()
                }),
                "ImportSpec#keys": j(&ImportSpec {
                    format: ImportFormat::Json {
                        layout: JsonLayout::Ndjson,
                    },
                    target: ImportTarget::Keys {
                        database: ObjectPath::root().child(ObjectKind::Database, "0"),
                        mode: PutMode::IfAbsent,
                    },
                    ..import_spec()
                }),
                "ExportReport": j(&export_report()),
                "ImportReport": j(&import_report()),
                "RowError": j(&row_error()),
                "ObjectMeta": j(&object_meta()),
            }),
        );
    }
}
