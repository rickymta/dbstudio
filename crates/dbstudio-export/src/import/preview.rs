//! `preview_import`: first rows, delimiter, sheets, guessed types and a row estimate.

use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::sync::OnceLock;

use dbstudio_core::{
    EditValue, Error, Result, ScriptDialect, StatementInfo, TextRange, Value, ValueKind,
};
use regex::Regex;

use super::source::{self, RowSource, xlsx_sheets};
use crate::{ImportFormat, ImportPreview, ImportPreviewRequest, JsonLayout};

const MAX_PREVIEW_ROWS: u32 = 1000;
const SQL_PREVIEW_STATEMENTS: usize = 50;
/// Bytes sampled to guess the delimiter / the average row size.
const SAMPLE_BYTES: usize = 64 * 1024;

/// Most plausible delimiter among `, ; TAB |` for the first lines of a file.
pub(crate) fn detect_delimiter(sample: &str) -> Option<char> {
    let lines: Vec<&str> = sample.lines().filter(|l| !l.is_empty()).take(20).collect();
    if lines.is_empty() {
        return None;
    }
    let mut best: Option<(char, usize)> = None;
    for d in [',', ';', '\t', '|'] {
        let counts: Vec<usize> = lines.iter().map(|l| l.matches(d).count()).collect();
        let min = *counts.iter().min().unwrap_or(&0);
        if min == 0 {
            continue;
        }
        // Every line has at least `min` delimiters; prefer the one that is most regular.
        let regular = counts.iter().filter(|&&c| c == counts[0]).count();
        let score = min * 10 + regular;
        if best.is_none_or(|(_, s)| score > s) {
            best = Some((d, score));
        }
    }
    best.map(|(d, _)| d)
}

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("valid pattern"))
}

/// Kind a user-typed text looks like.
fn text_kind(s: &str) -> ValueKind {
    static INT: OnceLock<Regex> = OnceLock::new();
    static FLOAT: OnceLock<Regex> = OnceLock::new();
    static DATE: OnceLock<Regex> = OnceLock::new();
    static DATETIME: OnceLock<Regex> = OnceLock::new();
    static UUID: OnceLock<Regex> = OnceLock::new();
    let t = s.trim();
    if re(&INT, r"^-?\d+$").is_match(t) {
        // "007" is an identifier, not a number.
        let digits = t.trim_start_matches('-');
        return if digits.len() > 1 && digits.starts_with('0') {
            ValueKind::Text
        } else {
            ValueKind::Int
        };
    }
    if re(&FLOAT, r"^-?(\d+\.\d*|\.\d+|\d+)([eE][-+]?\d+)?$").is_match(t) {
        return ValueKind::Float;
    }
    if t.eq_ignore_ascii_case("true") || t.eq_ignore_ascii_case("false") {
        return ValueKind::Bool;
    }
    if re(&DATE, r"^\d{4}-\d{2}-\d{2}$").is_match(t) {
        return ValueKind::Date;
    }
    if re(
        &DATETIME,
        r"^\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}(:\d{2}(\.\d+)?)?(Z|[+-]\d{2}:?\d{2})?$",
    )
    .is_match(t)
    {
        return ValueKind::DateTime;
    }
    if re(
        &UUID,
        r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$",
    )
    .is_match(t)
    {
        return ValueKind::Uuid;
    }
    ValueKind::Text
}

fn cell_kind(c: &EditValue) -> Option<ValueKind> {
    match c {
        EditValue::Text(s) if s.trim().is_empty() => None,
        EditValue::Text(s) => Some(text_kind(s)),
        EditValue::Value(Value::Null) | EditValue::Default => None,
        EditValue::Value(v) => Some(v.kind()),
    }
}

fn unify(a: ValueKind, b: ValueKind) -> ValueKind {
    use ValueKind::*;
    if a == b {
        return a;
    }
    match (a, b) {
        (Int | UInt, Int | UInt) => Int,
        (Int | UInt | Float | Decimal, Int | UInt | Float | Decimal) => Float,
        (Date, DateTime) | (DateTime, Date) | (DateTime, Timestamp) | (Timestamp, DateTime) => {
            DateTime
        }
        _ => Text,
    }
}

/// Kind per column guessed from sample rows (all-empty columns are text).
pub(crate) fn suggest_kinds(columns: usize, rows: &[Vec<EditValue>]) -> Vec<ValueKind> {
    (0..columns)
        .map(|i| {
            rows.iter()
                .filter_map(|r| r.get(i).and_then(cell_kind))
                .reduce(unify)
                .unwrap_or(ValueKind::Text)
        })
        .collect()
}

fn to_value(c: EditValue) -> Value {
    match c {
        EditValue::Text(s) => Value::Text(s),
        EditValue::Value(v) => v,
        EditValue::Default => Value::Null,
    }
}

fn read_sample(path: &Path) -> Result<(String, u64)> {
    let mut file = File::open(path)
        .map_err(|e| Error::Other(format!("cannot read {}: {e}", path.display())))?;
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    let mut buf = vec![0u8; SAMPLE_BYTES];
    let n = file
        .read(&mut buf)
        .map_err(|e| Error::Other(format!("cannot read {}: {e}", path.display())))?;
    buf.truncate(n);
    let text = String::from_utf8_lossy(&buf).into_owned();
    Ok((text.trim_start_matches('\u{feff}').to_string(), size))
}

/// Estimate from the average line length of the sample.
fn estimate_lines(sample: &str, size: u64, header: bool) -> u64 {
    let lines = sample.lines().count().max(1) as u64;
    if (sample.len() as u64) >= size {
        return lines.saturating_sub(u64::from(header));
    }
    let avg = (sample.len() as f64 / lines as f64).max(1.0);
    ((size as f64 / avg) as u64).saturating_sub(u64::from(header))
}

pub(crate) async fn preview(req: ImportPreviewRequest) -> Result<ImportPreview> {
    let path = std::path::PathBuf::from(&req.file);
    let limit = req.rows.clamp(1, MAX_PREVIEW_ROWS) as usize;
    tokio::task::spawn_blocking(move || preview_blocking(&path, &req.format, limit, req.dialect))
        .await
        .map_err(|e| Error::Other(format!("preview task failed: {e}")))?
}

fn preview_blocking(
    path: &Path,
    format: &ImportFormat,
    limit: usize,
    dialect: Option<ScriptDialect>,
) -> Result<ImportPreview> {
    let mut out = ImportPreview::default();
    match format {
        ImportFormat::Sql { .. } => return sql_preview(path, dialect),
        ImportFormat::Csv {
            delimiter,
            quote,
            header,
            ..
        } => {
            let (sample, size) = read_sample(path)?;
            out.detected_delimiter = detect_delimiter(&sample);
            out.estimated_rows = Some(estimate_lines(&sample, size, *header));
            let _ = (delimiter, quote);
        }
        ImportFormat::Xlsx { .. } => {
            out.sheets = xlsx_sheets(path)?;
        }
        ImportFormat::Json { layout } => {
            let (sample, size) = read_sample(path)?;
            if *layout == JsonLayout::Ndjson {
                out.estimated_rows = Some(estimate_lines(&sample, size, false));
            }
        }
    }
    let mut format = format.clone();
    if let ImportFormat::Xlsx { sheet, .. } = &mut format {
        if sheet.is_empty() {
            *sheet = out.sheets.first().cloned().unwrap_or_default();
        }
    }
    let mut src: Box<dyn RowSource> = source::open(path, &format, true)?;
    out.columns = src.columns().to_vec();
    if let (ImportFormat::Xlsx { header, .. }, Some(rows)) = (&format, src.declared_rows()) {
        out.estimated_rows = Some(rows.saturating_sub(u64::from(*header)));
    }
    let mut sample_rows: Vec<Vec<EditValue>> = Vec::new();
    while sample_rows.len() < limit {
        match src.next_row()? {
            Some(r) => sample_rows.push(r.cells),
            None => break,
        }
    }
    if matches!(
        format,
        ImportFormat::Json {
            layout: JsonLayout::Array
        }
    ) {
        // Exact when the whole array fits in the sample, otherwise a size based guess from the
        // number of objects in the first bytes of the file.
        out.estimated_rows = Some(if sample_rows.len() < limit {
            sample_rows.len() as u64
        } else {
            let (sample, size) = read_sample(path)?;
            let objects = sample.matches("},").count().max(1) as u64;
            let guess = (size as f64 / sample.len().max(1) as f64 * objects as f64) as u64;
            guess.max(sample_rows.len() as u64)
        });
    }
    out.suggested_kinds = suggest_kinds(out.columns.len(), &sample_rows);
    out.rows = sample_rows
        .into_iter()
        .map(|r| r.into_iter().map(to_value).collect())
        .collect();
    Ok(out)
}

/// Dialect guess for a script when the preview request names no dialect: backticks or
/// `DELIMITER` mean MySQL, everything else PostgreSQL.
pub(crate) fn guess_dialect(sample: &str) -> ScriptDialect {
    if sample.contains('`') || sample.contains("DELIMITER ") {
        ScriptDialect::mysql(false, true)
    } else {
        ScriptDialect::Postgres
    }
}

fn sql_preview(path: &Path, dialect: Option<ScriptDialect>) -> Result<ImportPreview> {
    let (sample, _) = read_sample(path)?;
    let dialect = dialect.unwrap_or_else(|| guess_dialect(&sample));
    let file = File::open(path)
        .map_err(|e| Error::Other(format!("cannot read {}: {e}", path.display())))?;
    let mut reader = BufReader::new(file);
    let mut splitter = dbstudio_sql::StreamSplitter::new(dialect);
    let mut statements: Vec<String> = Vec::new();
    let mut line = String::new();
    while statements.len() < SQL_PREVIEW_STATEMENTS {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .map_err(|e| Error::Other(format!("cannot read {}: {e}", path.display())))?;
        if n == 0 {
            statements.extend(splitter.clone().finish());
            break;
        }
        statements.extend(splitter.push(&line));
    }
    statements.truncate(SQL_PREVIEW_STATEMENTS);
    let mut offset = 0u32;
    let mut infos: Vec<StatementInfo> = Vec::new();
    for (i, text) in statements.iter().enumerate() {
        let analysis = dbstudio_sql::analyze(dialect, text);
        let len = text.chars().count() as u32;
        if let Some(mut info) = analysis.statements.into_iter().next() {
            info.index = i as u32;
            info.range = TextRange {
                start: offset,
                end: offset + len,
            };
            infos.push(info);
        }
        offset += len;
    }
    Ok(ImportPreview {
        statements_preview: infos,
        ..ImportPreview::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delimiter_detection() {
        assert_eq!(detect_delimiter("a,b,c\n1,2,3\n"), Some(','));
        assert_eq!(detect_delimiter("a;b;c\n1;2;3\n"), Some(';'));
        assert_eq!(detect_delimiter("a\tb\n1\t2\n"), Some('\t'));
        assert_eq!(detect_delimiter("a;b,c\n1;2,3\nx;y\n"), Some(';'));
        assert_eq!(detect_delimiter("single\nlines\n"), None);
        assert_eq!(detect_delimiter(""), None);
    }

    #[test]
    fn kind_guessing() {
        let rows: Vec<Vec<EditValue>> = [
            ["1", "1.5", "2024-01-02", "007", "true", ""],
            ["22", "2", "2024-01-03 10:00:00", "008", "false", ""],
        ]
        .iter()
        .map(|r| r.iter().map(|s| EditValue::Text((*s).into())).collect())
        .collect();
        assert_eq!(
            suggest_kinds(6, &rows),
            [
                ValueKind::Int,
                ValueKind::Float,
                ValueKind::DateTime,
                ValueKind::Text,
                ValueKind::Bool,
                ValueKind::Text
            ]
        );
        let typed = vec![
            vec![EditValue::Value(Value::Int(1))],
            vec![EditValue::Text("x".into())],
        ];
        assert_eq!(suggest_kinds(1, &typed), [ValueKind::Text]);
    }

    #[test]
    fn dialect_guess() {
        assert!(matches!(
            guess_dialect("SELECT `a`"),
            ScriptDialect::MySql { .. }
        ));
        assert_eq!(guess_dialect("SELECT 1"), ScriptDialect::Postgres);
    }
}
