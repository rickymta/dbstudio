//! JSON writer: an array or NDJSON per table, plus `schema.json`.

use std::io::Write;
use std::path::PathBuf;

use dbstudio_core::{ColumnMeta, Error, Result, ResultSink, StatementEnd, Value};
use serde_json::json;

use super::{NameSet, SchemaEntry, WriterConfig};
use crate::fsio::{Output, safe_name};
use crate::values::{JsonStyle, write_json, write_json_row};
use crate::{BinaryFormat, ExportContent, JsonLayout, JsonOptions, ObjectMeta, TableWriter};

const FLUSH_AT: usize = 256 * 1024;

#[derive(Debug)]
pub(crate) struct JsonWriter {
    out: Output,
    opts: JsonOptions,
    binary: BinaryFormat,
    content: ExportContent,
    names: NameSet,
    schema: Vec<SchemaEntry>,
    buf: String,
    in_entry: bool,
    first_row: bool,
    names_cols: Vec<String>,
    keep: Vec<usize>,
    /// The object is a document source (one `$document` column): every record is the
    /// document itself rather than `{"$document": ...}`.
    doc_mode: bool,
}

impl JsonWriter {
    pub fn create(cfg: &WriterConfig, opts: JsonOptions) -> Result<JsonWriter> {
        Ok(JsonWriter {
            out: Output::create(&cfg.dest, cfg.overwrite, cfg.zip)?,
            opts,
            binary: cfg.binary,
            content: cfg.content,
            names: NameSet::default(),
            schema: Vec::new(),
            buf: String::new(),
            in_entry: false,
            first_row: true,
            names_cols: Vec::new(),
            keep: Vec::new(),
            doc_mode: false,
        })
    }

    fn flush_buf(&mut self) -> Result<()> {
        if !self.buf.is_empty() {
            self.out.write_all(self.buf.as_bytes())?;
            self.buf.clear();
        }
        Ok(())
    }

    fn ndjson(&self) -> bool {
        self.opts.layout == JsonLayout::Ndjson
    }

    /// The document of a `$document` row, restricted to the configured top-level fields.
    fn document_of(&self, row: &[Value]) -> Value {
        match row.first() {
            Some(Value::Document(fields)) if !self.opts.fields.is_empty() => Value::Document(
                fields
                    .iter()
                    .filter(|(k, _)| self.opts.fields.contains(k))
                    .cloned()
                    .collect(),
            ),
            Some(v) => v.clone(),
            None => Value::Null,
        }
    }

    fn schema_document(&self) -> serde_json::Value {
        let tables: Vec<serde_json::Value> = self
            .schema
            .iter()
            .map(|t| {
                let columns: Vec<serde_json::Value> = t
                    .columns
                    .iter()
                    .map(|c| {
                        json!({
                            "name": c.name,
                            "ordinal": c.ordinal,
                            "type": c.type_name,
                            "kind": c.value_kind,
                            "nullable": c.nullable,
                            "default": c.default,
                            "primaryKey": c.primary_key,
                            "comment": c.comment,
                            "extra": c.extra,
                        })
                    })
                    .collect();
                json!({
                    "name": t.name,
                    "schema": t.schema,
                    "kind": t.kind,
                    "comment": t.comment,
                    "estimatedRows": t.estimated_rows,
                    "primaryKey": t.primary_key,
                    "uniqueKeys": t.unique_keys,
                    "columns": columns,
                })
            })
            .collect();
        json!({ "tables": tables })
    }
}

impl ResultSink for JsonWriter {
    fn on_columns(&mut self, _columns: Vec<ColumnMeta>) -> Result<()> {
        Ok(())
    }

    fn on_rows(&mut self, rows: Vec<Vec<Value>>) -> Result<()> {
        if !self.in_entry {
            return Ok(());
        }
        let ndjson = self.ndjson();
        let style = JsonStyle {
            binary: self.binary,
            ext: self.opts.extended_json,
            indent: if self.opts.pretty && !ndjson {
                Some(2)
            } else {
                None
            },
        };
        for row in &rows {
            if self.doc_mode {
                if !ndjson {
                    self.buf
                        .push_str(if self.first_row { "\n  " } else { ",\n  " });
                    self.first_row = false;
                }
                let doc = self.document_of(row);
                let level = usize::from(!ndjson);
                write_json(
                    &mut self.buf,
                    &doc,
                    style.binary,
                    style.ext,
                    style.indent,
                    level,
                );
                if ndjson {
                    self.buf.push('\n');
                }
            } else if ndjson {
                write_json_row(&mut self.buf, &self.names_cols, &self.keep, row, style, 0);
                self.buf.push('\n');
            } else {
                self.buf
                    .push_str(if self.first_row { "\n  " } else { ",\n  " });
                self.first_row = false;
                write_json_row(&mut self.buf, &self.names_cols, &self.keep, row, style, 1);
            }
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

impl TableWriter for JsonWriter {
    fn begin_object(&mut self, meta: &ObjectMeta) -> Result<()> {
        self.schema.push(SchemaEntry::from_meta(meta));
        if self.content == ExportContent::Schema {
            return Ok(());
        }
        let ext = if self.ndjson() { "ndjson" } else { "json" };
        let name = self.names.unique(&safe_name(&meta.name), 120);
        self.out.start_entry(&format!("{name}.{ext}"))?;
        self.in_entry = true;
        self.first_row = true;
        self.names_cols = super::dedupe_names(meta.columns.iter().map(|c| c.name.as_str()));
        self.doc_mode = self.names_cols == ["$document"];
        self.keep = (0..self.names_cols.len())
            .filter(|&i| {
                self.opts.fields.is_empty() || self.opts.fields.contains(&self.names_cols[i])
            })
            .collect();
        if !self.ndjson() {
            self.buf.push('[');
        }
        Ok(())
    }

    fn end_object(&mut self) -> Result<()> {
        if self.in_entry {
            if !self.ndjson() {
                self.buf
                    .push_str(if self.first_row { "]\n" } else { "\n]\n" });
            }
            self.flush_buf()?;
            self.in_entry = false;
        }
        Ok(())
    }

    fn bytes(&self) -> u64 {
        self.out.bytes()
    }

    fn finish(mut self: Box<Self>) -> Result<Vec<PathBuf>> {
        if self.schema.is_empty() {
            return Err(Error::Other("nothing to export".into()));
        }
        if self.content != ExportContent::Data {
            self.out.start_entry("schema.json")?;
            let doc = serde_json::to_string_pretty(&self.schema_document())
                .map_err(|e| Error::Other(e.to_string()))?;
            self.out.write_all(doc.as_bytes())?;
            self.out.write_all(b"\n")?;
        }
        let JsonWriter { out, .. } = *self;
        let (path, _) = out.commit()?;
        Ok(vec![path])
    }
}
