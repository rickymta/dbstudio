//! Readers of the import formats. Every reader yields rows as positional cells together with
//! the 1-based line / row / element number in the file (used in error messages).

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use calamine::{Data, Reader, open_workbook_auto};
use dbstudio_core::{EditValue, Error, Result, Value};
use serde::Deserialize;
use time::{Date, Duration, Month};

use crate::{ImportFormat, JsonLayout};

/// One row of the file.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SourceRow {
    /// 1-based line (CSV, NDJSON), row (XLSX) or element (JSON array) number.
    pub line: u64,
    pub cells: Vec<EditValue>,
}

/// Pull-based reader over the rows of a file.
pub(crate) trait RowSource: Send {
    /// Column names: the header, `#1`... without one, the key union of the first records (JSON).
    fn columns(&self) -> &[String];
    /// Next row; `None` at the end.
    fn next_row(&mut self) -> Result<Option<SourceRow>>;
    /// Bytes consumed from the file so far (progress), when known.
    fn bytes_read(&self) -> u64 {
        0
    }
    /// Rows the source knows it holds (sheet dimensions), header included.
    fn declared_rows(&self) -> Option<u64> {
        None
    }
}

/// Open the reader of `format`.
/// `preview`: only a sample is needed (JSON keys are learnt from the first records only).
pub(crate) fn open(
    path: &Path,
    format: &ImportFormat,
    preview: bool,
) -> Result<Box<dyn RowSource>> {
    match format {
        ImportFormat::Csv {
            delimiter,
            quote,
            header,
            ..
        } => Ok(Box::new(CsvSource::open(
            path, *delimiter, *quote, *header,
        )?)),
        ImportFormat::Xlsx { sheet, header } => xlsx_open(path, sheet, *header),
        ImportFormat::Json { layout } => Ok(Box::new(JsonSource::open(
            path,
            *layout,
            preview.then_some(PREVIEW_KEY_SCAN),
        )?)),
        ImportFormat::Sql { .. } => Err(Error::Other("a SQL script has no rows".into())),
    }
}

fn io_err(path: &Path, e: std::io::Error) -> Error {
    Error::Other(format!("cannot read {}: {e}", path.display()))
}

fn ascii_byte(c: char, what: &str) -> Result<u8> {
    u8::try_from(c as u32)
        .ok()
        .filter(u8::is_ascii)
        .ok_or_else(|| Error::Other(format!("the CSV {what} must be an ASCII character")))
}

/// Strip a UTF-8 byte order mark at the start of `r`.
fn skip_bom<R: BufRead>(r: &mut R) -> std::io::Result<()> {
    let buf = r.fill_buf()?;
    if buf.starts_with(&[0xEF, 0xBB, 0xBF]) {
        r.consume(3);
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// CSV
// ---------------------------------------------------------------------------------------------

/// A parsed record: the line it starts on and its fields with their `was quoted` flag.
type CsvRecord = (u64, Vec<(String, bool)>);

/// Incremental CSV parser that tracks line numbers (quoted fields may span lines).
struct CsvParser<R: BufRead> {
    reader: R,
    delimiter: u8,
    quote: u8,
    /// Line the next byte belongs to (1-based).
    line: u64,
    consumed: u64,
    /// The previous record ended with `\r`; a following `\n` belongs to it.
    pending_cr: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CsvState {
    FieldStart,
    Unquoted,
    InQuotes,
    QuoteSeen,
}

impl<R: BufRead> CsvParser<R> {
    fn new(reader: R, delimiter: u8, quote: u8) -> Self {
        CsvParser {
            reader,
            delimiter,
            quote,
            line: 1,
            consumed: 0,
            pending_cr: false,
        }
    }

    /// Next record and the line it starts on; blank lines are skipped.
    fn next_record(&mut self) -> std::io::Result<Option<CsvRecord>> {
        let mut fields: Vec<(String, bool)> = Vec::new();
        let mut quoted = false;
        let mut field: Vec<u8> = Vec::new();
        let mut state = CsvState::FieldStart;
        let mut start_line = self.line;
        let mut any = false;
        loop {
            let buf = self.reader.fill_buf()?;
            if buf.is_empty() {
                if any || state != CsvState::FieldStart || !field.is_empty() {
                    fields.push((String::from_utf8_lossy(&field).into_owned(), quoted));
                    return Ok(Some((start_line, fields)));
                }
                return Ok(None);
            }
            let mut used = 0usize;
            let mut done = false;
            for &b in buf {
                used += 1;
                if self.pending_cr {
                    self.pending_cr = false;
                    if b == b'\n' {
                        // Second half of a CRLF that ended the previous record.
                        continue;
                    }
                }
                let at_start = state == CsvState::FieldStart && !any && field.is_empty();
                match state {
                    CsvState::InQuotes => {
                        if b == self.quote {
                            state = CsvState::QuoteSeen;
                        } else {
                            if b == b'\n' {
                                self.line += 1;
                            }
                            field.push(b);
                        }
                        continue;
                    }
                    CsvState::QuoteSeen if b == self.quote => {
                        field.push(self.quote);
                        state = CsvState::InQuotes;
                        continue;
                    }
                    _ => {}
                }
                if b == b'\n' || b == b'\r' {
                    if b == b'\r' {
                        self.pending_cr = true;
                    }
                    self.line += 1;
                    if at_start {
                        // Blank line.
                        start_line = self.line;
                        continue;
                    }
                    fields.push((String::from_utf8_lossy(&field).into_owned(), quoted));
                    done = true;
                    break;
                }
                if b == self.delimiter {
                    fields.push((String::from_utf8_lossy(&field).into_owned(), quoted));
                    field.clear();
                    quoted = false;
                    any = true;
                    state = CsvState::FieldStart;
                    continue;
                }
                match state {
                    CsvState::FieldStart if b == self.quote => {
                        state = CsvState::InQuotes;
                        quoted = true;
                    }
                    CsvState::FieldStart | CsvState::Unquoted | CsvState::QuoteSeen => {
                        // Text after a closing quote is kept as it is.
                        field.push(b);
                        state = CsvState::Unquoted;
                    }
                    CsvState::InQuotes => unreachable!("handled above"),
                }
            }
            self.reader.consume(used);
            self.consumed += used as u64;
            if done {
                return Ok(Some((start_line, fields)));
            }
        }
    }
}

pub(crate) struct CsvSource {
    parser: CsvParser<BufReader<File>>,
    columns: Vec<String>,
    pending: Option<SourceRow>,
}

impl CsvSource {
    pub fn open(path: &Path, delimiter: char, quote: char, header: bool) -> Result<CsvSource> {
        let delim = ascii_byte(delimiter, "delimiter")?;
        let quote = ascii_byte(quote, "quote")?;
        let file = File::open(path).map_err(|e| io_err(path, e))?;
        let mut buffered = BufReader::with_capacity(256 * 1024, file);
        skip_bom(&mut buffered).map_err(|e| io_err(path, e))?;
        let mut src = CsvSource {
            parser: CsvParser::new(buffered, delim, quote),
            columns: Vec::new(),
            pending: None,
        };
        if let Some(first) = src.read_record()? {
            if header {
                src.columns = first
                    .cells
                    .iter()
                    .enumerate()
                    .map(|(i, c)| match c {
                        EditValue::Text(t) if !t.trim().is_empty() => t.trim().to_string(),
                        _ => format!("#{}", i + 1),
                    })
                    .collect();
            } else {
                src.columns = (1..=first.cells.len()).map(|i| format!("#{i}")).collect();
                src.pending = Some(first);
            }
        }
        Ok(src)
    }

    fn read_record(&mut self) -> Result<Option<SourceRow>> {
        match self.parser.next_record() {
            Ok(None) => Ok(None),
            Ok(Some((line, fields))) => Ok(Some(SourceRow {
                line,
                // An empty field without quotes is NULL, `""` is the empty string: what the
                // CSV export writes.
                cells: fields
                    .into_iter()
                    .map(|(text, quoted)| {
                        if text.is_empty() && !quoted {
                            EditValue::Value(Value::Null)
                        } else {
                            EditValue::Text(text)
                        }
                    })
                    .collect(),
            })),
            Err(e) => Err(Error::Other(format!(
                "line {}: cannot read the file: {e}",
                self.parser.line
            ))),
        }
    }
}

impl RowSource for CsvSource {
    fn columns(&self) -> &[String] {
        &self.columns
    }

    fn next_row(&mut self) -> Result<Option<SourceRow>> {
        if let Some(row) = self.pending.take() {
            return Ok(Some(row));
        }
        self.read_record()
    }

    fn bytes_read(&self) -> u64 {
        self.parser.consumed
    }
}

// ---------------------------------------------------------------------------------------------
// XLSX
// ---------------------------------------------------------------------------------------------

pub(crate) struct XlsxMemSource {
    header: bool,
    columns: Vec<String>,
    rows: std::vec::IntoIter<(u64, Vec<Data>)>,
}

/// Names of the sheets of a workbook.
pub(crate) fn xlsx_sheets(path: &Path) -> Result<Vec<String>> {
    let wb = open_workbook_auto(path).map_err(|e| Error::Other(format!("xlsx: {e}")))?;
    Ok(wb.sheet_names())
}

impl XlsxMemSource {
    pub fn open(path: &Path, sheet: &str, header: bool) -> Result<XlsxMemSource> {
        let mut wb = open_workbook_auto(path).map_err(|e| Error::Other(format!("xlsx: {e}")))?;
        let name = if sheet.is_empty() {
            wb.sheet_names()
                .first()
                .cloned()
                .ok_or_else(|| Error::Other("the workbook has no sheets".into()))?
        } else {
            sheet.to_string()
        };
        let range = wb
            .worksheet_range(&name)
            .map_err(|e| Error::Other(format!("xlsx sheet {name}: {e}")))?;
        let first_row = u64::from(range.start().map_or(0, |s| s.0)) + 1;
        let mut rows: Vec<(u64, Vec<Data>)> = range
            .rows()
            .enumerate()
            .map(|(i, r)| (first_row + i as u64, r.to_vec()))
            .collect();
        let width = rows.iter().map(|(_, r)| r.len()).max().unwrap_or(0);
        let columns = if header && !rows.is_empty() {
            let (_, h) = rows.remove(0);
            (0..width)
                .map(|i| match h.get(i) {
                    Some(Data::Empty) | None => format!("#{}", i + 1),
                    Some(d) => {
                        let t = data_text(d);
                        if t.trim().is_empty() {
                            format!("#{}", i + 1)
                        } else {
                            t.trim().to_string()
                        }
                    }
                })
                .collect()
        } else {
            (1..=width).map(|i| format!("#{i}")).collect()
        };
        Ok(XlsxMemSource {
            header: header && width > 0,
            columns,
            rows: rows.into_iter(),
        })
    }
}

/// `.xlsx` / `.xlsm` files are read as a stream (the sheet never sits in memory); other
/// workbook formats fall back to loading the sheet.
fn xlsx_open(path: &Path, sheet: &str, header: bool) -> Result<Box<dyn RowSource>> {
    let streamable = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("xlsx") || e.eq_ignore_ascii_case("xlsm"));
    if streamable {
        Ok(Box::new(XlsxStream::open(path, sheet, header)?))
    } else {
        Ok(Box::new(XlsxMemSource::open(path, sheet, header)?))
    }
}

enum XlsxMsg {
    /// Rows the sheet declares (estimate).
    Meta(u64),
    Row(u64, Vec<EditValue>),
    Failed(String),
}

/// Streams the rows of an `.xlsx` sheet. The workbook and its cell reader live on a helper
/// thread (the reader borrows the workbook) that hands rows over a small bounded channel.
pub(crate) struct XlsxStream {
    columns: Vec<String>,
    pending: Option<SourceRow>,
    rx: std::sync::mpsc::Receiver<XlsxMsg>,
    estimated: u64,
}

fn xlsx_worker(path: std::path::PathBuf, sheet: String, tx: std::sync::mpsc::SyncSender<XlsxMsg>) {
    use calamine::{Xlsx, open_workbook};
    let fail = |m: String| {
        let _ = tx.send(XlsxMsg::Failed(m));
    };
    let mut wb: Xlsx<_> = match open_workbook(&path) {
        Ok(w) => w,
        Err(e) => return fail(format!("xlsx: {e}")),
    };
    let name = if sheet.is_empty() {
        match wb.sheet_names().first() {
            Some(n) => n.clone(),
            None => return fail("the workbook has no sheets".into()),
        }
    } else {
        sheet
    };
    let mut reader = match wb.worksheet_cells_reader(&name) {
        Ok(r) => r,
        Err(e) => return fail(format!("xlsx sheet {name}: {e}")),
    };
    let dims = reader.dimensions();
    let declared = u64::from(dims.end.0.saturating_sub(dims.start.0)) + 1;
    if tx.send(XlsxMsg::Meta(declared)).is_err() {
        return;
    }
    let mut current: Option<u32> = None;
    let mut cells: Vec<EditValue> = Vec::new();
    loop {
        match reader.next_cell() {
            Ok(Some(cell)) => {
                let (row, col) = cell.get_position();
                if current.is_some_and(|r| r != row) {
                    let line = u64::from(current.unwrap_or(0)) + 1;
                    if tx
                        .send(XlsxMsg::Row(line, std::mem::take(&mut cells)))
                        .is_err()
                    {
                        return;
                    }
                }
                current = Some(row);
                let col = col as usize;
                if cells.len() <= col {
                    cells.resize(col + 1, EditValue::Value(Value::Null));
                }
                let data: Data = cell.get_value().clone().into();
                cells[col] = data_cell(&data);
            }
            Ok(None) => break,
            Err(e) => return fail(format!("xlsx sheet {name}: {e}")),
        }
    }
    if let Some(row) = current {
        let _ = tx.send(XlsxMsg::Row(u64::from(row) + 1, cells));
    }
}

impl XlsxStream {
    pub fn open(path: &Path, sheet: &str, header: bool) -> Result<XlsxStream> {
        let (tx, rx) = std::sync::mpsc::sync_channel(256);
        let (path, sheet) = (path.to_path_buf(), sheet.to_string());
        std::thread::Builder::new()
            .name("xlsx-reader".into())
            .spawn(move || xlsx_worker(path, sheet, tx))
            .map_err(|e| Error::Other(format!("cannot start the xlsx reader: {e}")))?;
        let mut src = XlsxStream {
            columns: Vec::new(),
            pending: None,
            rx,
            estimated: 0,
        };
        // The first message is the sheet's size (or the reason it cannot be read).
        match src.rx.recv() {
            Ok(XlsxMsg::Meta(n)) => src.estimated = n,
            Ok(XlsxMsg::Failed(m)) => return Err(Error::Other(m)),
            _ => return Err(Error::Other("the xlsx reader stopped".into())),
        }
        if let Some(first) = src.read()? {
            if header {
                src.columns = first
                    .cells
                    .iter()
                    .enumerate()
                    .map(|(i, c)| match c {
                        EditValue::Text(t) if !t.trim().is_empty() => t.trim().to_string(),
                        EditValue::Value(Value::Null) => format!("#{}", i + 1),
                        EditValue::Value(v) => {
                            let t = crate::values::cell_text(v, crate::BinaryFormat::Base64)
                                .map(|c| c.into_owned())
                                .unwrap_or_default();
                            if t.trim().is_empty() {
                                format!("#{}", i + 1)
                            } else {
                                t.trim().to_string()
                            }
                        }
                        _ => format!("#{}", i + 1),
                    })
                    .collect();
            } else {
                src.columns = (1..=first.cells.len()).map(|i| format!("#{i}")).collect();
                src.pending = Some(first);
            }
        }
        Ok(src)
    }

    fn read(&mut self) -> Result<Option<SourceRow>> {
        match self.rx.recv() {
            Ok(XlsxMsg::Row(line, cells)) => Ok(Some(SourceRow { line, cells })),
            Ok(XlsxMsg::Failed(m)) => Err(Error::Other(m)),
            Ok(XlsxMsg::Meta(_)) | Err(_) => Ok(None),
        }
    }
}

impl RowSource for XlsxStream {
    fn columns(&self) -> &[String] {
        &self.columns
    }

    fn declared_rows(&self) -> Option<u64> {
        Some(self.estimated)
    }

    fn next_row(&mut self) -> Result<Option<SourceRow>> {
        if let Some(row) = self.pending.take() {
            return Ok(Some(row));
        }
        self.read()
    }
}

fn data_text(d: &Data) -> String {
    match d {
        Data::String(s) | Data::DateTimeIso(s) | Data::DurationIso(s) => s.clone(),
        Data::Int(i) => i.to_string(),
        Data::Float(f) => {
            if f.fract() == 0.0 && f.abs() < 1e15 {
                format!("{}", *f as i64)
            } else {
                f.to_string()
            }
        }
        Data::Bool(b) => b.to_string(),
        Data::DateTime(dt) => excel_serial(dt.as_f64())
            .map(|v| match v {
                Value::Date(s) | Value::DateTime(s) => s,
                _ => String::new(),
            })
            .unwrap_or_default(),
        Data::Error(e) => format!("{e:?}"),
        Data::Empty => String::new(),
    }
}

/// Excel serial day number (1900 date system) as a date or date-time value.
pub(crate) fn excel_serial(serial: f64) -> Option<Value> {
    if !serial.is_finite() || serial < 0.0 {
        return None;
    }
    let days = serial.floor();
    let secs = ((serial - days) * 86_400.0).round() as i64;
    let epoch = Date::from_calendar_date(1899, Month::December, 30).ok()?;
    let date = epoch.checked_add(Duration::days(days as i64))?;
    let (secs, date) = if secs >= 86_400 {
        (0, date.checked_add(Duration::days(1))?)
    } else {
        (secs, date)
    };
    let day = format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        u8::from(date.month()),
        date.day()
    );
    if secs == 0 {
        Some(Value::Date(day))
    } else {
        Some(Value::DateTime(format!(
            "{day} {:02}:{:02}:{:02}",
            secs / 3600,
            secs % 3600 / 60,
            secs % 60
        )))
    }
}

fn data_cell(d: &Data) -> EditValue {
    match d {
        Data::Empty => EditValue::Value(Value::Null),
        Data::String(s) => EditValue::Text(s.clone()),
        Data::Int(i) => EditValue::Value(Value::Int(*i)),
        Data::Float(f) => {
            if f.fract() == 0.0 && f.abs() < 9_007_199_254_740_992.0 {
                EditValue::Value(Value::Int(*f as i64))
            } else {
                EditValue::Value(Value::Float(*f))
            }
        }
        Data::Bool(b) => EditValue::Value(Value::Bool(*b)),
        Data::DateTime(dt) => match excel_serial(dt.as_f64()) {
            Some(v) if !dt.is_duration() => EditValue::Value(v),
            _ => EditValue::Value(Value::Float(dt.as_f64())),
        },
        Data::DateTimeIso(s) | Data::DurationIso(s) => EditValue::Text(s.clone()),
        Data::Error(e) => EditValue::Text(format!("{e:?}")),
    }
}

impl RowSource for XlsxMemSource {
    fn columns(&self) -> &[String] {
        &self.columns
    }

    fn declared_rows(&self) -> Option<u64> {
        Some(self.rows.len() as u64 + u64::from(self.header))
    }

    fn next_row(&mut self) -> Result<Option<SourceRow>> {
        Ok(self.rows.next().map(|(line, row)| SourceRow {
            line,
            cells: row.iter().map(data_cell).collect(),
        }))
    }
}

// ---------------------------------------------------------------------------------------------
// JSON
// ---------------------------------------------------------------------------------------------

/// A JSON object whose key order is kept.
#[derive(Debug)]
struct OrderedObject(Vec<(String, Box<serde_json::value::RawValue>)>);

impl<'de> Deserialize<'de> for OrderedObject {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = OrderedObject;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a JSON object")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<OrderedObject, A::Error> {
                let mut fields = Vec::new();
                while let Some((k, v)) =
                    map.next_entry::<String, Box<serde_json::value::RawValue>>()?
                {
                    fields.push((k, v));
                }
                Ok(OrderedObject(fields))
            }
        }
        d.deserialize_map(V)
    }
}

/// A JSON value as a cell. Numbers keep their exact text (big integers and decimals do not go
/// through a float), nested documents keep their original text and key order.
fn json_cell(raw: &serde_json::value::RawValue) -> EditValue {
    let t = raw.get().trim();
    match t.as_bytes().first() {
        Some(b'n') => EditValue::Value(Value::Null),
        Some(b't') => EditValue::Value(Value::Bool(true)),
        Some(b'f') => EditValue::Value(Value::Bool(false)),
        Some(b'"') => EditValue::Text(serde_json::from_str::<String>(t).unwrap_or_default()),
        Some(b'{' | b'[') => EditValue::Value(Value::Json(t.to_string())),
        _ => {
            if let Ok(i) = t.parse::<i64>() {
                EditValue::Value(Value::Int(i))
            } else if let Ok(u) = t.parse::<u64>() {
                EditValue::Value(Value::UInt(u))
            } else {
                EditValue::Value(Value::Decimal(t.to_string()))
            }
        }
    }
}

/// Splits the top-level array of a JSON document into the text of its elements.
struct ArrayReader<R: BufRead> {
    reader: R,
    started: bool,
    done: bool,
}

impl<R: BufRead> ArrayReader<R> {
    fn new(reader: R) -> Self {
        ArrayReader {
            reader,
            started: false,
            done: false,
        }
    }

    fn next_byte(&mut self) -> std::io::Result<Option<u8>> {
        let buf = self.reader.fill_buf()?;
        match buf.first().copied() {
            Some(b) => {
                self.reader.consume(1);
                Ok(Some(b))
            }
            None => Ok(None),
        }
    }

    /// Text of the next element, `None` after the closing bracket.
    fn next_element(&mut self) -> Result<Option<String>> {
        if self.done {
            return Ok(None);
        }
        let io = |e: std::io::Error| Error::Other(format!("cannot read the JSON file: {e}"));
        if !self.started {
            loop {
                match self.next_byte().map_err(io)? {
                    Some(b) if b.is_ascii_whitespace() => {}
                    Some(0xEF) => {
                        // BOM (EF BB BF)
                        self.next_byte().map_err(io)?;
                        self.next_byte().map_err(io)?;
                    }
                    Some(b'[') => break,
                    _ => {
                        return Err(Error::Other(
                            "the JSON file must hold an array of objects".into(),
                        ));
                    }
                }
            }
            self.started = true;
        }
        let mut out: Vec<u8> = Vec::new();
        let (mut depth, mut in_str, mut escaped) = (0i32, false, false);
        loop {
            let Some(b) = self.next_byte().map_err(io)? else {
                return Err(Error::Other("the JSON array is not closed".into()));
            };
            if in_str {
                out.push(b);
                if escaped {
                    escaped = false;
                } else if b == b'\\' {
                    escaped = true;
                } else if b == b'"' {
                    in_str = false;
                }
                continue;
            }
            match b {
                b'"' => {
                    in_str = true;
                    out.push(b);
                }
                b'{' | b'[' => {
                    depth += 1;
                    out.push(b);
                }
                b'}' => {
                    depth -= 1;
                    out.push(b);
                }
                b']' if depth == 0 => {
                    self.done = true;
                    return Ok(non_blank(out));
                }
                b']' => {
                    depth -= 1;
                    out.push(b);
                }
                b',' if depth == 0 => {
                    return non_blank(out)
                        .ok_or_else(|| Error::Other("empty element in the JSON array".into()))
                        .map(Some);
                }
                _ => out.push(b),
            }
        }
    }
}

fn non_blank(bytes: Vec<u8>) -> Option<String> {
    let s = String::from_utf8_lossy(&bytes).into_owned();
    if s.trim().is_empty() { None } else { Some(s) }
}

enum JsonReader {
    Array(ArrayReader<BufReader<File>>),
    Lines(BufReader<File>),
}

pub(crate) struct JsonSource {
    layout: JsonLayout,
    columns: Vec<String>,
    index: std::collections::HashMap<String, usize>,
    reader: JsonReader,
    line: u64,
}

/// Records inspected for the column names in a preview.
pub(crate) const PREVIEW_KEY_SCAN: usize = 1000;

impl JsonSource {
    /// `key_scan_limit`: records read to learn the column names (`None` = all of them, so a key
    /// that first appears late is not lost).
    pub fn open(
        path: &Path,
        layout: JsonLayout,
        key_scan_limit: Option<usize>,
    ) -> Result<JsonSource> {
        let mut columns: Vec<String> = Vec::new();
        // Pass 1: the key union of the first records.
        {
            let mut scan = Self::reader(path, layout)?;
            let mut seen = 0usize;
            let mut line = 0u64;
            while key_scan_limit.is_none_or(|n| seen < n) {
                let Some((_, text)) = Self::read_text(&mut scan, layout, &mut line)? else {
                    break;
                };
                if let Ok(obj) = serde_json::from_str::<OrderedObject>(&text) {
                    for (k, _) in obj.0 {
                        if !columns.contains(&k) {
                            columns.push(k);
                        }
                    }
                }
                seen += 1;
            }
        }
        let index = columns
            .iter()
            .enumerate()
            .map(|(i, c)| (c.clone(), i))
            .collect();
        Ok(JsonSource {
            layout,
            columns,
            index,
            reader: Self::reader(path, layout)?,
            line: 0,
        })
    }

    fn reader(path: &Path, layout: JsonLayout) -> Result<JsonReader> {
        let file = File::open(path).map_err(|e| io_err(path, e))?;
        let mut r = BufReader::with_capacity(256 * 1024, file);
        Ok(match layout {
            JsonLayout::Array => JsonReader::Array(ArrayReader::new(r)),
            JsonLayout::Ndjson => {
                skip_bom(&mut r).map_err(|e| io_err(path, e))?;
                JsonReader::Lines(r)
            }
        })
    }

    /// `(line, text)` of the next record.
    fn read_text(
        reader: &mut JsonReader,
        layout: JsonLayout,
        line: &mut u64,
    ) -> Result<Option<(u64, String)>> {
        match reader {
            JsonReader::Array(a) => Ok(a.next_element()?.map(|t| {
                *line += 1;
                (*line, t)
            })),
            JsonReader::Lines(r) => loop {
                let mut text = String::new();
                let n = r
                    .read_line(&mut text)
                    .map_err(|e| Error::Other(format!("cannot read the JSON file: {e}")))?;
                if n == 0 {
                    return Ok(None);
                }
                *line += 1;
                if !text.trim().is_empty() {
                    let _ = layout;
                    return Ok(Some((*line, text)));
                }
            },
        }
    }
}

impl RowSource for JsonSource {
    fn columns(&self) -> &[String] {
        &self.columns
    }

    fn next_row(&mut self) -> Result<Option<SourceRow>> {
        let Some((line, text)) = Self::read_text(&mut self.reader, self.layout, &mut self.line)?
        else {
            return Ok(None);
        };
        let obj: OrderedObject = serde_json::from_str(&text)
            .map_err(|e| Error::Other(format!("record {line}: not a JSON object ({e})")))?;
        let mut cells = vec![EditValue::Value(Value::Null); self.columns.len()];
        for (k, v) in &obj.0 {
            if let Some(&i) = self.index.get(k) {
                cells[i] = json_cell(v);
            }
        }
        Ok(Some(SourceRow { line, cells }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(dir: &tempfile::TempDir, name: &str, content: &[u8]) -> std::path::PathBuf {
        let p = dir.path().join(name);
        File::create(&p).unwrap().write_all(content).unwrap();
        p
    }

    fn null() -> EditValue {
        EditValue::Value(Value::Null)
    }

    fn text(v: &str) -> EditValue {
        EditValue::Text(v.into())
    }

    #[test]
    fn csv_with_header_bom_and_quotes() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            "a.csv",
            b"\xEF\xBB\xBFid;name\r\n1;\"a;b\"\r\n2;\"multi\nline\"\r\n3;x\r\n",
        );
        let mut s = CsvSource::open(&p, ';', '"', true).unwrap();
        assert_eq!(s.columns(), ["id", "name"]);
        let r1 = s.next_row().unwrap().unwrap();
        assert_eq!((r1.line, r1.cells), (2, vec![text("1"), text("a;b")]));
        let r2 = s.next_row().unwrap().unwrap();
        assert_eq!(r2.line, 3);
        assert_eq!(r2.cells[1], text("multi\nline"));
        // The record after a multi-line field reports the line it starts on.
        assert_eq!(s.next_row().unwrap().unwrap().line, 5);
        assert!(s.next_row().unwrap().is_none());
    }

    #[test]
    fn csv_edge_cases() {
        let dir = tempfile::tempdir().unwrap();
        // Blank lines are skipped, a lone \r ends a record, "" is a quote, trailing delimiter
        // gives an empty last field, a file without final newline keeps its last record.
        let p = write(&dir, "a.csv", b"a,b\n\n1,\"x\"\"y\"\n2,\r3,\nlast,field");
        let mut s = CsvSource::open(&p, ',', '"', true).unwrap();
        let r = s.next_row().unwrap().unwrap();
        assert_eq!((r.line, r.cells), (3, vec![text("1"), text("x\"y")]));
        let r = s.next_row().unwrap().unwrap();
        assert_eq!((r.line, r.cells), (4, vec![text("2"), null()]));
        let r = s.next_row().unwrap().unwrap();
        assert_eq!((r.line, r.cells), (5, vec![text("3"), null()]));
        let r = s.next_row().unwrap().unwrap();
        assert_eq!((r.line, r.cells), (6, vec![text("last"), text("field")]));
        assert!(s.next_row().unwrap().is_none());
        assert_eq!(s.bytes_read(), 30);
    }

    #[test]
    fn csv_null_versus_empty_string() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, "n.csv", b"a,b,c\n,\"\",x\n\"\",,\n");
        let mut s = CsvSource::open(&p, ',', '"', true).unwrap();
        assert_eq!(
            s.next_row().unwrap().unwrap().cells,
            [null(), text(""), text("x")]
        );
        assert_eq!(
            s.next_row().unwrap().unwrap().cells,
            [text(""), null(), null()]
        );
    }

    #[test]
    fn csv_without_header_names_columns_by_position() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, "a.csv", b"1,2\n3,4\n");
        let mut s = CsvSource::open(&p, ',', '"', false).unwrap();
        assert_eq!(s.columns(), ["#1", "#2"]);
        assert_eq!(s.next_row().unwrap().unwrap().line, 1);
        assert_eq!(s.next_row().unwrap().unwrap().line, 2);
        assert!(s.next_row().unwrap().is_none());
        assert!(CsvSource::open(&p, 'é', '"', true).is_err());
    }

    #[test]
    fn json_array_and_ndjson() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            "a.json",
            br#"[ {"id": 1, "name": "a,]x"}, {"id": 2, "tags": [1,2], "name": null}, {"name": "c\"]"} ]"#,
        );
        let mut s = JsonSource::open(&p, JsonLayout::Array, None).unwrap();
        assert_eq!(s.columns(), ["id", "name", "tags"]);
        let r1 = s.next_row().unwrap().unwrap();
        assert_eq!(r1.cells[1], text("a,]x"));
        assert_eq!(r1.cells[0], EditValue::Value(Value::Int(1)));
        assert_eq!(r1.cells[2], EditValue::Value(Value::Null));
        let r2 = s.next_row().unwrap().unwrap();
        assert_eq!(r2.line, 2);
        assert_eq!(r2.cells[2], EditValue::Value(Value::Json("[1,2]".into())));
        let r3 = s.next_row().unwrap().unwrap();
        assert_eq!(r3.cells[1], text("c\"]"));
        assert!(s.next_row().unwrap().is_none());

        let nd = write(&dir, "a.ndjson", b"{\"a\":1}\n\n{\"a\":2,\"b\":true}\n");
        let mut s = JsonSource::open(&nd, JsonLayout::Ndjson, None).unwrap();
        assert_eq!(s.columns(), ["a", "b"]);
        assert_eq!(s.next_row().unwrap().unwrap().line, 1);
        let r = s.next_row().unwrap().unwrap();
        assert_eq!(r.line, 3);
        assert_eq!(r.cells[1], EditValue::Value(Value::Bool(true)));

        let bad = write(&dir, "b.json", b"{\"a\": 1}");
        assert!(JsonSource::open(&bad, JsonLayout::Array, None).is_err());
    }

    #[test]
    fn excel_dates() {
        assert_eq!(
            excel_serial(45000.0),
            Some(Value::Date("2023-03-15".into()))
        );
        assert_eq!(
            excel_serial(45000.5),
            Some(Value::DateTime("2023-03-15 12:00:00".into()))
        );
        assert_eq!(excel_serial(f64::NAN), None);
    }

    #[test]
    fn xlsx_is_streamed_row_by_row_with_row_numbers() {
        use rust_xlsxwriter::Workbook;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.xlsx");
        let mut wb = Workbook::new();
        let ws = wb.add_worksheet_with_constant_memory().unwrap();
        ws.set_name("Data").unwrap();
        ws.write_string(0, 0, "id").unwrap();
        ws.write_string(0, 1, "name").unwrap();
        // Row 3 is blank; row 5 has an empty first cell.
        for r in 1..=5000u32 {
            if r == 2 {
                continue;
            }
            if r != 4 {
                ws.write_number(r, 0, f64::from(r)).unwrap();
            }
            ws.write_string(r, 1, format!("n{r}")).unwrap();
        }
        wb.save(&path).unwrap();

        let mut s = XlsxStream::open(&path, "", true).unwrap();
        assert_eq!(s.columns(), ["id", "name"]);
        assert_eq!(s.declared_rows(), Some(5001));
        let first = s.next_row().unwrap().unwrap();
        assert_eq!((first.line, &first.cells[1]), (2, &text("n1")));
        assert_eq!(first.cells[0], EditValue::Value(Value::Int(1)));
        // The blank row is skipped; row numbers are the sheet's.
        assert_eq!(s.next_row().unwrap().unwrap().line, 4);
        let fifth = s.next_row().unwrap().unwrap();
        assert_eq!((fifth.line, &fifth.cells[0]), (5, &null()));
        let mut count = 3;
        while s.next_row().unwrap().is_some() {
            count += 1;
        }
        assert_eq!(count, 4999);
        // A sheet that does not exist is an error, not an empty import.
        assert!(XlsxStream::open(&path, "Nope", true).is_err());
        // Dropping a half-read stream ends the helper thread without hanging.
        let mut again = XlsxStream::open(&path, "Data", false).unwrap();
        assert_eq!(again.columns(), ["#1", "#2"]);
        assert_eq!(again.next_row().unwrap().unwrap().line, 1);
        drop(again);
    }
}
