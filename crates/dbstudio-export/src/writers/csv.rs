//! CSV writer: one file, or one archive entry per table plus `schema.csv`.

use std::io::Write;
use std::path::PathBuf;

use dbstudio_core::{ColumnMeta, Error, Result, ResultSink, StatementEnd, Value};

use super::{DICTIONARY_HEADER, NameSet, SchemaEntry, WriterConfig};
use crate::fsio::{Output, safe_name};
use crate::values::cell_text;
use crate::{
    BinaryFormat, CsvOptions, ExportContent, LineEnding, ObjectMeta, TableWriter, TextEncoding,
};

const BOM: &[u8] = &[0xEF, 0xBB, 0xBF];
/// Buffered text is written out once it reaches this size.
const FLUSH_AT: usize = 256 * 1024;

#[derive(Debug)]
pub(crate) struct CsvWriter {
    out: Output,
    opts: CsvOptions,
    binary: BinaryFormat,
    content: ExportContent,
    names: NameSet,
    schema: Vec<SchemaEntry>,
    buf: String,
    in_entry: bool,
    columns: usize,
}

/// Text of a cell made harmless for Excel: text that starts like a formula gets a `'` prefix
/// (numbers keep their sign).
fn excel_safe<'a>(v: &Value, text: std::borrow::Cow<'a, str>) -> std::borrow::Cow<'a, str> {
    let numeric = matches!(
        v,
        Value::Int(_) | Value::UInt(_) | Value::Float(_) | Value::Decimal(_)
    );
    if !numeric && text.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        std::borrow::Cow::Owned(format!("'{text}"))
    } else {
        text
    }
}

/// Append one field, quoting it when needed.
pub(crate) fn push_field(buf: &mut String, value: Option<&str>, o: &CsvOptions) {
    let Some(s) = value else {
        // NULL: the configured text (never quoted, so it stays distinguishable from "").
        buf.push_str(&o.null_text);
        return;
    };
    let q = o.quote;
    let needs = o.quote_all
        || (s.is_empty() && o.null_text.is_empty())
        // A real string equal to the NULL text is quoted so that it reads back as a string.
        || (!o.null_text.is_empty() && s == o.null_text)
        || s.starts_with(' ')
        || s.ends_with(' ')
        || s.chars()
            .any(|c| c == o.delimiter || c == q || c == '\n' || c == '\r');
    if !needs {
        buf.push_str(s);
        return;
    }
    buf.push(q);
    for c in s.chars() {
        if c == q {
            buf.push(q);
        }
        buf.push(c);
    }
    buf.push(q);
}

impl CsvWriter {
    pub fn create(cfg: &WriterConfig, opts: CsvOptions) -> Result<CsvWriter> {
        Ok(CsvWriter {
            out: Output::create(&cfg.dest, cfg.overwrite, cfg.zip)?,
            opts,
            binary: cfg.binary,
            content: cfg.content,
            names: NameSet::default(),
            schema: Vec::new(),
            buf: String::new(),
            in_entry: false,
            columns: 0,
        })
    }

    fn eol(&mut self) {
        self.buf.push_str(match self.opts.line_ending {
            LineEnding::Lf => "\n",
            LineEnding::Crlf => "\r\n",
        });
    }

    fn start_entry(&mut self, name: &str) -> Result<()> {
        self.out.start_entry(name)?;
        self.in_entry = true;
        if self.opts.encoding == TextEncoding::Utf8Bom {
            self.out.write_all(BOM)?;
        }
        Ok(())
    }

    fn flush_buf(&mut self) -> Result<()> {
        if !self.buf.is_empty() {
            self.out.write_all(self.buf.as_bytes())?;
            self.buf.clear();
        }
        Ok(())
    }

    fn record<'a>(&mut self, fields: impl Iterator<Item = Option<&'a str>>) {
        for (i, f) in fields.enumerate() {
            if i > 0 {
                self.buf.push(self.opts.delimiter);
            }
            let opts = self.opts.clone();
            push_field(&mut self.buf, f, &opts);
        }
        self.eol();
    }

    fn write_schema_file(&mut self) -> Result<()> {
        self.start_entry("schema.csv")?;
        // Empty cells of the dictionary are plain empty fields whatever NULL text is configured.
        let null_text = std::mem::take(&mut self.opts.null_text);
        // The dictionary is always written with a header and plain NULL handling.
        let header: Vec<&str> = DICTIONARY_HEADER.to_vec();
        self.record(header.iter().map(|h| Some(*h)));
        let rows: Vec<[String; 8]> = self
            .schema
            .iter()
            .flat_map(SchemaEntry::dictionary_rows)
            .collect();
        for r in &rows {
            self.record(
                r.iter()
                    .map(|c| if c.is_empty() { None } else { Some(c.as_str()) }),
            );
            if self.buf.len() >= FLUSH_AT {
                self.flush_buf()?;
            }
        }
        self.opts.null_text = null_text;
        self.flush_buf()
    }
}

impl ResultSink for CsvWriter {
    fn on_columns(&mut self, _columns: Vec<ColumnMeta>) -> Result<()> {
        Ok(())
    }

    fn on_rows(&mut self, rows: Vec<Vec<Value>>) -> Result<()> {
        if !self.in_entry {
            return Ok(());
        }
        let binary = self.binary;
        let safe = self.opts.excel_safe;
        for row in &rows {
            let cells: Vec<_> = row
                .iter()
                .map(|v| {
                    let text = cell_text(v, binary)?;
                    Some(if safe { excel_safe(v, text) } else { text })
                })
                .collect();
            self.record(cells.iter().map(|c| c.as_deref()));
            if self.buf.len() >= FLUSH_AT {
                self.flush_buf()?;
            }
        }
        Ok(())
    }

    fn on_message(&mut self, _message: String) -> Result<()> {
        Ok(())
    }

    fn on_statement_end(&mut self, _end: StatementEnd) -> Result<()> {
        Ok(())
    }
}

impl TableWriter for CsvWriter {
    fn begin_object(&mut self, meta: &ObjectMeta) -> Result<()> {
        self.schema.push(SchemaEntry::from_meta(meta));
        if self.content == ExportContent::Schema {
            return Ok(());
        }
        let name = self.names.unique(&safe_name(&meta.name), 120);
        self.start_entry(&format!("{name}.csv"))?;
        self.columns = meta.columns.len();
        if self.opts.header {
            let names = super::dedupe_names(meta.columns.iter().map(|c| c.name.as_str()));
            self.record(names.iter().map(|n| Some(n.as_str())));
        }
        Ok(())
    }

    fn end_object(&mut self) -> Result<()> {
        self.flush_buf()?;
        self.in_entry = false;
        Ok(())
    }

    fn bytes(&self) -> u64 {
        self.out.bytes()
    }

    fn finish(mut self: Box<Self>) -> Result<Vec<PathBuf>> {
        if self.content != ExportContent::Data {
            self.write_schema_file()?;
        }
        if self.schema.is_empty() {
            return Err(Error::Other("nothing to export".into()));
        }
        let CsvWriter { out, .. } = *self;
        let (path, _) = out.commit()?;
        Ok(vec![path])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> CsvOptions {
        CsvOptions::default()
    }

    fn field(v: Option<&str>, o: &CsvOptions) -> String {
        let mut s = String::new();
        push_field(&mut s, v, o);
        s
    }

    #[test]
    fn formulas_get_a_prefix_but_numbers_do_not() {
        for (v, want) in [
            (Value::Text("=1+1".into()), "'=1+1"),
            (Value::Text("-x".into()), "'-x"),
            (Value::Text("@a".into()), "'@a"),
            (Value::Text("plain".into()), "plain"),
            (Value::Int(-5), "-5"),
            (Value::Decimal("-1.5".into()), "-1.5"),
        ] {
            let text = cell_text(&v, BinaryFormat::Base64).unwrap();
            assert_eq!(excel_safe(&v, text), want);
        }
    }

    #[test]
    fn quoting() {
        let o = opts();
        assert_eq!(field(Some("plain"), &o), "plain");
        assert_eq!(field(Some("a,b"), &o), "\"a,b\"");
        assert_eq!(field(Some("say \"hi\""), &o), "\"say \"\"hi\"\"\"");
        assert_eq!(field(Some("line\nbreak"), &o), "\"line\nbreak\"");
        assert_eq!(field(Some(" lead"), &o), "\" lead\"");
        // NULL and the empty string stay distinguishable while null_text is empty.
        assert_eq!(field(None, &o), "");
        assert_eq!(field(Some(""), &o), "\"\"");
        let all = CsvOptions {
            quote_all: true,
            ..opts()
        };
        assert_eq!(field(Some("x"), &all), "\"x\"");
        let custom = CsvOptions {
            delimiter: ';',
            quote: '\'',
            null_text: "NULL".into(),
            ..opts()
        };
        assert_eq!(field(Some("a;b"), &custom), "'a;b'");
        assert_eq!(field(Some("it's"), &custom), "'it''s'");
        assert_eq!(field(None, &custom), "NULL");
        assert_eq!(field(Some(""), &custom), "");
    }
}
