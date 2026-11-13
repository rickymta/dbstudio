//! XLSX writer: one constant-memory sheet per table (split before Excel's row limit) and an
//! optional data dictionary sheet.

use std::path::PathBuf;

use dbstudio_core::{ColumnMeta, Error, Result, ResultSink, StatementEnd, Value};
use rust_xlsxwriter::{Workbook, XlsxError};

use super::{DICTIONARY_HEADER, NameSet, SchemaEntry, WriterConfig};
use crate::fsio::Output;
use crate::values::cell_text;
use crate::{BinaryFormat, ExportContent, ObjectMeta, TableWriter, XlsxOptions};

/// Longest sheet name Excel accepts.
const MAX_SHEET_NAME: usize = 31;
/// Largest integer Excel keeps exactly.
const MAX_EXACT_INT: i64 = 9_007_199_254_740_991;

fn xerr(e: XlsxError) -> Error {
    Error::Other(format!("xlsx: {e}"))
}

#[derive(Debug)]
struct Sheet {
    index: usize,
    base: String,
    part: u32,
    columns: Vec<String>,
    next_row: u32,
    data_rows: u32,
}

pub(crate) struct XlsxWriter {
    out: Output,
    wb: Workbook,
    opts: XlsxOptions,
    binary: BinaryFormat,
    content: ExportContent,
    max_rows: u32,
    names: NameSet,
    sheet_count: usize,
    current: Option<Sheet>,
    schema: Vec<SchemaEntry>,
    truncated: u64,
}

impl std::fmt::Debug for XlsxWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("XlsxWriter")
            .field("sheets", &self.sheet_count)
            .finish_non_exhaustive()
    }
}

/// Sheet name valid for Excel: no `[]:*?/\`, not empty.
fn clean_sheet_name(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| match c {
            '[' | ']' | ':' | '*' | '?' | '/' | '\\' => '_',
            c => c,
        })
        .collect();
    let s = s.trim_matches('\'').trim().to_string();
    if s.is_empty() { "Sheet".into() } else { s }
}

impl XlsxWriter {
    pub fn create(cfg: &WriterConfig, opts: XlsxOptions) -> Result<XlsxWriter> {
        let out = Output::external(&cfg.dest, cfg.overwrite)?;
        let mut wb = Workbook::new();
        wb.set_tempdir(out.dir()).map_err(xerr)?;
        Ok(XlsxWriter {
            out,
            wb,
            opts,
            binary: cfg.binary,
            content: cfg.content,
            max_rows: cfg.tuning.xlsx_sheet_rows.max(1),
            names: NameSet::default(),
            sheet_count: 0,
            current: None,
            schema: Vec::new(),
            truncated: 0,
        })
    }

    /// Unique sheet name: `orders`, `orders (2)`...
    fn sheet_name(&mut self, base: &str, part: u32) -> String {
        let base = clean_sheet_name(base);
        let suffix = if part > 1 {
            format!(" ({part})")
        } else {
            String::new()
        };
        let room = MAX_SHEET_NAME.saturating_sub(suffix.chars().count());
        let candidate: String = base.chars().take(room).collect::<String>() + &suffix;
        // `unique` appends `_n` on a clash with the name of another table.
        self.names.unique(&candidate, MAX_SHEET_NAME)
    }

    fn open_sheet(&mut self, base: &str, part: u32, columns: Vec<String>) -> Result<()> {
        let name = self.sheet_name(base, part);
        let index = self.sheet_count;
        let ws = self.wb.add_worksheet_with_constant_memory().map_err(xerr)?;
        ws.set_name(&name).map_err(xerr)?;
        self.sheet_count += 1;
        let mut next_row = 0;
        if self.opts.header {
            for (c, h) in columns.iter().enumerate() {
                ws.write_string(0, c as u16, h).map_err(xerr)?;
            }
            if self.opts.freeze_header {
                ws.set_freeze_panes(1, 0).map_err(xerr)?;
            }
            next_row = 1;
        }
        self.current = Some(Sheet {
            index,
            base: base.to_string(),
            part,
            columns,
            next_row,
            data_rows: 0,
        });
        Ok(())
    }

    fn write_dictionary(&mut self) -> Result<()> {
        let name = self.names.unique("Data dictionary", MAX_SHEET_NAME);
        let max_chars = self.opts.max_cell_chars as usize;
        let rows: Vec<[String; 8]> = self
            .schema
            .iter()
            .flat_map(SchemaEntry::dictionary_rows)
            .collect();
        let ws = self.wb.add_worksheet();
        ws.set_name(&name).map_err(xerr)?;
        for (c, h) in DICTIONARY_HEADER.iter().enumerate() {
            ws.write_string(0, c as u16, *h).map_err(xerr)?;
        }
        ws.set_freeze_panes(1, 0).map_err(xerr)?;
        for (r, row) in rows.iter().enumerate() {
            for (c, cell) in row.iter().enumerate() {
                if !cell.is_empty() {
                    let text: String = cell.chars().take(max_chars).collect();
                    ws.write_string(r as u32 + 1, c as u16, text)
                        .map_err(xerr)?;
                }
            }
        }
        self.sheet_count += 1;
        Ok(())
    }
}

impl ResultSink for XlsxWriter {
    fn on_columns(&mut self, _columns: Vec<ColumnMeta>) -> Result<()> {
        Ok(())
    }

    fn on_rows(&mut self, rows: Vec<Vec<Value>>) -> Result<()> {
        for row in &rows {
            let Some(cur) = self.current.as_ref() else {
                return Ok(());
            };
            if cur.data_rows >= self.max_rows {
                let (base, part, columns) = (cur.base.clone(), cur.part + 1, cur.columns.clone());
                self.open_sheet(&base, part, columns)?;
            }
            let cur = self.current.as_mut().expect("sheet is open");
            let r = cur.next_row;
            cur.next_row += 1;
            cur.data_rows += 1;
            let index = cur.index;
            let max_chars = self.opts.max_cell_chars as usize;
            let binary = self.binary;
            let ws = self.wb.worksheet_from_index(index).map_err(xerr)?;
            for (c, v) in row.iter().enumerate() {
                let c = c as u16;
                match v {
                    Value::Null => {}
                    Value::Bool(b) => {
                        ws.write_boolean(r, c, *b).map_err(xerr)?;
                    }
                    Value::Int(i) if (-MAX_EXACT_INT..=MAX_EXACT_INT).contains(i) => {
                        ws.write_number(r, c, *i as f64).map_err(xerr)?;
                    }
                    Value::UInt(u) if *u <= MAX_EXACT_INT as u64 => {
                        ws.write_number(r, c, *u as f64).map_err(xerr)?;
                    }
                    Value::Float(f) if f.is_finite() => {
                        ws.write_number(r, c, *f).map_err(xerr)?;
                    }
                    other => {
                        // Everything else (decimals, dates, big integers, bytes...) is text.
                        if let Some(text) = cell_text(other, binary) {
                            if text.chars().count() > max_chars {
                                self.truncated += 1;
                                let cut: String = text.chars().take(max_chars).collect();
                                ws.write_string(r, c, cut).map_err(xerr)?;
                            } else {
                                ws.write_string(r, c, text.as_ref()).map_err(xerr)?;
                            }
                        }
                    }
                }
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

impl TableWriter for XlsxWriter {
    fn begin_object(&mut self, meta: &ObjectMeta) -> Result<()> {
        self.schema.push(SchemaEntry::from_meta(meta));
        if self.content == ExportContent::Schema {
            return Ok(());
        }
        let columns = meta.columns.iter().map(|c| c.name.clone()).collect();
        self.open_sheet(&meta.name, 1, columns)
    }

    fn end_object(&mut self) -> Result<()> {
        self.current = None;
        Ok(())
    }

    fn warnings(&mut self) -> Vec<String> {
        if self.truncated > 0 {
            vec![format!(
                "{} cell(s) were cut to {} characters",
                self.truncated, self.opts.max_cell_chars
            )]
        } else {
            Vec::new()
        }
    }

    fn finish(mut self: Box<Self>) -> Result<Vec<PathBuf>> {
        if self.schema.is_empty() {
            return Err(Error::Other("nothing to export".into()));
        }
        if self.content != ExportContent::Data || self.opts.data_dictionary {
            self.write_dictionary()?;
        }
        let XlsxWriter { out, mut wb, .. } = *self;
        wb.save(out.tmp_path()).map_err(xerr)?;
        let (path, _) = out.commit()?;
        Ok(vec![path])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sheet_names_are_clean() {
        assert_eq!(clean_sheet_name("a/b[c]"), "a_b_c_");
        assert_eq!(clean_sheet_name("''"), "Sheet");
    }
}
