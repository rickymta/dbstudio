//! Request layer: per-request settings, streaming result sources and small query helpers.
//!
//! Every statement is one HTTP request. Results are requested as `RowBinaryWithNamesAndTypes`
//! and decoded incrementally by [`Source`]; a result with a column type the decoder does not
//! know is re-requested once as `JSONCompactEachRowWithNamesAndTypes` and converted by type.

use bytes::Bytes;
use clickhouse::Client;
use clickhouse::query::BytesCursor;
use dbstudio_core::{ColumnMeta, Error, Result, Value};
use uuid::Uuid;

use crate::error::{from_server_text, map_client_error};
use crate::rowbinary::{self, Cur, DecErr};
use crate::types::CType;

const BINARY_FORMAT: &str = "RowBinaryWithNamesAndTypes";
const JSON_FORMAT: &str = "JSONCompactEachRowWithNamesAndTypes";
/// Rows decoded from the buffer before it is compacted.
const COMPACT_AT: usize = 256 * 1024;

/// Server version split into numbers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Version {
    pub major: u32,
    pub minor: u32,
}

impl Version {
    pub(crate) fn parse(text: &str) -> Version {
        let mut it = text
            .split('.')
            .map(|p| p.trim().parse::<u32>().unwrap_or(0));
        Version {
            major: it.next().unwrap_or(0),
            minor: it.next().unwrap_or(0),
        }
    }

    pub(crate) fn at_least(self, major: u32, minor: u32) -> bool {
        self >= Version { major, minor }
    }
}

/// State shared by a session and its leases.
#[derive(Debug)]
pub(crate) struct Core {
    /// Base client: URL, credentials, transport.
    pub client: Client,
    pub readonly: bool,
    pub default_db: Option<String>,
    pub version: Version,
    /// Settings sent with every request (version gated and user supplied).
    pub settings: Vec<(String, String)>,
    /// List the `system` / `INFORMATION_SCHEMA` databases in the tree.
    pub show_system: bool,
    /// Data editing is limited to inserts (option `clickhouse.editing = insertOnly`).
    pub insert_only: bool,
    /// The connection is configured read-only.
    pub config_read_only: bool,
    /// The user's profile is read-only (`readonly = 1`): the server refuses every setting
    /// change, so no settings are sent at all.
    pub settings_locked: bool,
}

/// Per-request parameters.
#[derive(Debug, Clone, Default)]
pub(crate) struct Req {
    pub database: Option<String>,
    pub query_id: Option<String>,
    pub session_id: Option<String>,
    pub timeout_ms: Option<u64>,
    /// Extra settings of this request only.
    pub settings: Vec<(String, String)>,
}

impl Req {
    pub(crate) fn with_id(mut self, id: Uuid) -> Self {
        self.query_id = Some(id.to_string());
        self
    }

    pub(crate) fn in_db(mut self, db: Option<&str>) -> Self {
        self.database = db.filter(|d| !d.is_empty()).map(str::to_string);
        self
    }
}

impl Core {
    /// Client for one request.
    pub(crate) fn client(&self, req: &Req) -> Client {
        let mut c = self.client.clone();
        let db = req.database.as_ref().or(self.default_db.as_ref());
        if let Some(db) = db {
            c = c.with_database(db);
        }
        if !self.settings_locked {
            for (k, v) in self.settings.iter().chain(&req.settings) {
                c = c.with_setting(k, v);
            }
        }
        if self.readonly {
            c = c.with_setting("readonly", "2");
        }
        if let Some(id) = &req.query_id {
            c = c.with_setting("query_id", id);
        }
        if let Some(s) = &req.session_id {
            c = c
                .with_setting("session_id", s)
                .with_setting("session_timeout", "3600");
        }
        if let Some(ms) = req.timeout_ms.filter(|_| !self.settings_locked) {
            c = c.with_setting("max_execution_time", format!("{}", ms as f64 / 1000.0));
        }
        c
    }

    /// Client without the read-only guard (KILL QUERY, backups run with the user's own rights).
    pub(crate) fn admin_client(&self, req: &Req) -> Client {
        let mut c = self.client.clone();
        if let Some(id) = &req.query_id {
            c = c.with_setting("query_id", id);
        }
        c
    }

    /// Ask the server to kill the query `query_id` (and its JSON-fallback twin).
    pub(crate) async fn kill(&self, query_id: &str) -> Result<()> {
        self.kill_with(query_id, false).await
    }

    /// Like [`Core::kill`] but returns only when the query has stopped (and released its HTTP
    /// session, which a following statement of the same lease needs).
    pub(crate) async fn kill_sync(&self, query_id: &str) -> Result<()> {
        self.kill_with(query_id, true).await
    }

    async fn kill_with(&self, query_id: &str, sync: bool) -> Result<()> {
        // Query ids are UUID text; refuse anything else so the literal cannot be abused.
        if query_id.is_empty()
            || !query_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            return Err(Error::Invalid("bad query id".into()));
        }
        let mode = if sync { "SYNC" } else { "ASYNC" };
        let sql = format!("KILL QUERY WHERE startsWith(query_id, '{query_id}') {mode} FORMAT Null");
        self.admin_client(&Req::default())
            .query_raw(&sql)
            .execute()
            .await
            .map_err(map_client_error)
    }

    /// Run a statement for its side effect.
    pub(crate) async fn exec(&self, req: &Req, sql: &str) -> Result<()> {
        self.client(req)
            .query_raw(sql)
            .execute()
            .await
            .map_err(map_client_error)
    }

    /// Run `sql` and return all rows (metadata queries, small results).
    pub(crate) async fn rows(&self, req: &Req, sql: &str) -> Result<Vec<Vec<Value>>> {
        let opened = Source::open(self, req, sql).await?;
        let Opened::Rows(mut src) = opened else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        while let Some(row) = src.next_row().await? {
            out.push(row);
        }
        Ok(out)
    }

    pub(crate) async fn rows_db(&self, db: Option<&str>, sql: &str) -> Result<Vec<Vec<Value>>> {
        self.rows(&Req::default().in_db(db), sql).await
    }

    /// First cell of the first row as text.
    pub(crate) async fn scalar_text(&self, sql: &str) -> Result<Option<String>> {
        let rows = self.rows(&Req::default(), sql).await?;
        Ok(rows
            .into_iter()
            .next()
            .and_then(|r| r.into_iter().next())
            .map(|v| rowbinary::value_text(&v)))
    }
}

/// Connection option `name`, also accepted with the `clickhouse.` prefix.
pub(crate) fn option<'a>(cfg: &'a dbstudio_core::ConnectionConfig, name: &str) -> Option<&'a str> {
    cfg.options
        .get(&format!("clickhouse.{name}"))
        .or_else(|| cfg.options.get(name))
        .map(String::as_str)
}

/// Text of a cell (empty for NULL).
pub(crate) fn cell_text(row: &[Value], i: usize) -> String {
    match row.get(i) {
        Some(Value::Null) | None => String::new(),
        Some(v) => rowbinary::value_text(v),
    }
}

pub(crate) fn cell_opt(row: &[Value], i: usize) -> Option<String> {
    match row.get(i) {
        Some(Value::Null) | None => None,
        Some(v) => Some(rowbinary::value_text(v)),
    }
}

pub(crate) fn cell_u64(row: &[Value], i: usize) -> Option<u64> {
    match row.get(i)? {
        Value::UInt(u) => Some(*u),
        Value::Int(i) => u64::try_from(*i).ok(),
        Value::Decimal(s) | Value::Text(s) => s.parse().ok(),
        _ => None,
    }
}

pub(crate) fn cell_bool(row: &[Value], i: usize) -> bool {
    match row.get(i) {
        Some(Value::Bool(b)) => *b,
        Some(Value::Int(n)) => *n != 0,
        Some(Value::UInt(n)) => *n != 0,
        _ => false,
    }
}

/// `'..'` string literal (backslash and quote escaped).
pub(crate) fn quote_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\'' => out.push_str("\\'"),
            '\\' => out.push_str("\\\\"),
            '\0' => out.push_str("\\0"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// Backtick-quoted identifier.
pub(crate) fn quote_ident(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 2);
    out.push('`');
    for c in name.chars() {
        if c == '`' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('`');
    out
}

/// What a request produced.
pub(crate) enum Opened {
    /// A result set (possibly with zero rows).
    Rows(Source),
    /// The statement returned no data (DDL, INSERT, SET, ...).
    NoResult,
}

enum Mode {
    Binary,
    Json,
}

/// An open result set being decoded incrementally.
pub(crate) struct Source {
    cursor: BytesCursor,
    mode: Mode,
    buf: Vec<u8>,
    pos: usize,
    eof: bool,
    pub cols: Vec<(String, CType)>,
    types: Vec<CType>,
    /// Rows of the JSON fallback not yet handed out.
    json_lines_done: bool,
}

impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Source")
            .field("columns", &self.cols.len())
            .finish_non_exhaustive()
    }
}

/// Remove a trailing `FORMAT name` clause so the result format stays under our control.
pub(crate) fn strip_format_clause(sql: &str) -> String {
    let trimmed = sql.trim_end().trim_end_matches(';').trim_end();
    let lower = trimmed.to_ascii_lowercase();
    if let Some(i) = lower.rfind("format") {
        let before_ok = i == 0
            || !(lower.as_bytes()[i - 1].is_ascii_alphanumeric()
                || lower.as_bytes()[i - 1] == b'_');
        let tail = trimmed[i + 6..].trim();
        if before_ok
            && !tail.is_empty()
            && tail.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && trimmed[i + 6..].starts_with(char::is_whitespace)
            && !lower[..i].trim_end().ends_with(',')
        {
            return trimmed[..i].trim_end().to_string();
        }
    }
    trimmed.to_string()
}

impl Source {
    /// Send `sql` and read the response header.
    pub(crate) async fn open(core: &Core, req: &Req, sql: &str) -> Result<Opened> {
        let sql = strip_format_clause(sql);
        let client = core.client(req);
        let cursor = client
            .query_raw(&sql)
            .fetch_bytes(BINARY_FORMAT)
            .map_err(map_client_error)?;
        let mut src = Source {
            cursor,
            mode: Mode::Binary,
            buf: Vec::new(),
            pos: 0,
            eof: false,
            cols: Vec::new(),
            types: Vec::new(),
            json_lines_done: false,
        };
        // Read until the header is complete.
        let header = loop {
            match rowbinary::decode_header(&src.buf[src.pos..]) {
                Ok((cols, used)) => break Some((cols, used)),
                Err(DecErr::NeedMore) => {
                    if src.eof {
                        if src.buf.len() == src.pos {
                            break None;
                        }
                        return Err(Error::Other("truncated ClickHouse response header".into()));
                    }
                    src.fill().await?;
                }
                Err(DecErr::Bad(m)) | Err(DecErr::Unsupported(m)) => {
                    return Err(Error::Other(format!("bad ClickHouse response: {m}")));
                }
            }
        };
        let Some((cols, used)) = header else {
            return Ok(Opened::NoResult);
        };
        src.pos += used;
        let types: Vec<CType> = cols.iter().map(|(_, t)| CType::parse_lenient(t)).collect();
        if types.iter().all(CType::decodable) {
            src.cols = cols
                .into_iter()
                .zip(&types)
                .map(|((n, _), t)| (n, t.clone()))
                .collect();
            src.types = types;
            return Ok(Opened::Rows(src));
        }
        // Unsupported column type: ask the server for JSON instead.
        drop(src);
        let _ = core.kill_sync(req.query_id.as_deref().unwrap_or("")).await;
        Self::open_json(core, req, &sql).await
    }

    async fn open_json(core: &Core, req: &Req, sql: &str) -> Result<Opened> {
        let mut req = req.clone();
        req.settings.extend([
            ("output_format_json_quote_64bit_integers".into(), "1".into()),
            ("output_format_json_quote_decimals".into(), "1".into()),
            ("output_format_json_quote_denormals".into(), "1".into()),
            ("output_format_decimal_trailing_zeros".into(), "1".into()),
            ("session_timezone".into(), "UTC".into()),
        ]);
        if let Some(id) = &req.query_id {
            req.query_id = Some(format!("{id}-json"));
        }
        let cursor = core
            .client(&req)
            .query_raw(sql)
            .fetch_bytes(JSON_FORMAT)
            .map_err(map_client_error)?;
        let mut src = Source {
            cursor,
            mode: Mode::Json,
            buf: Vec::new(),
            pos: 0,
            eof: false,
            cols: Vec::new(),
            types: Vec::new(),
            json_lines_done: false,
        };
        let names = loop {
            if let Some(line) = src.json_line() {
                break line;
            }
            if src.eof {
                return Ok(Opened::NoResult);
            }
            src.fill().await?;
        };
        let types = loop {
            if let Some(line) = src.json_line() {
                break line;
            }
            if src.eof {
                return Err(Error::Other("truncated ClickHouse JSON header".into()));
            }
            src.fill().await?;
        };
        let parse = |l: &str| -> Result<Vec<String>> {
            serde_json::from_str::<Vec<String>>(l)
                .map_err(|e| Error::Other(format!("bad ClickHouse JSON header: {e}")))
        };
        let names = parse(&names)?;
        let types: Vec<CType> = parse(&types)?
            .iter()
            .map(|t| CType::parse_lenient(t))
            .collect();
        src.cols = names.into_iter().zip(types.iter().cloned()).collect();
        src.types = types;
        Ok(Opened::Rows(src))
    }

    /// Column metadata for the sink.
    pub(crate) fn metas(&self) -> Vec<ColumnMeta> {
        self.cols
            .iter()
            .map(|(name, t)| ColumnMeta {
                name: name.clone(),
                type_name: type_display(t, &self.cols, name),
                value_kind: t.value_kind(),
                nullable: Some(t.is_nullable()),
            })
            .collect()
    }

    /// Pull the next network chunk into the buffer.
    async fn fill(&mut self) -> Result<()> {
        if self.pos >= COMPACT_AT {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        match self.cursor.next().await.map_err(map_client_error)? {
            Some(chunk) => append(&mut self.buf, &chunk),
            None => self.eof = true,
        }
        Ok(())
    }

    /// One complete text line of the JSON mode.
    fn json_line(&mut self) -> Option<String> {
        let rest = &self.buf[self.pos..];
        let nl = rest.iter().position(|b| *b == b'\n')?;
        let line = String::from_utf8_lossy(&rest[..nl]).into_owned();
        self.pos += nl + 1;
        Some(line)
    }

    /// Next row, or `None` at the end of the result.
    pub(crate) async fn next_row(&mut self) -> Result<Option<Vec<Value>>> {
        loop {
            match self.mode {
                Mode::Binary => {
                    if self.pos < self.buf.len() {
                        let mut cur = Cur::new(&self.buf[self.pos..]);
                        match rowbinary::decode_row(&mut cur, &self.types) {
                            Ok(row) => {
                                self.pos += cur.pos();
                                return Ok(Some(row));
                            }
                            Err(DecErr::NeedMore) => {}
                            Err(DecErr::Bad(m)) => return Err(self.bad_data(m)),
                            Err(DecErr::Unsupported(m)) => {
                                return Err(Error::Unsupported(format!(
                                    "cannot read values of type {m}; wrap the column in CAST(.. AS String)"
                                )));
                            }
                        }
                    }
                    if self.eof {
                        if self.pos < self.buf.len() {
                            return Err(self.bad_data("response ended inside a row".into()));
                        }
                        return Ok(None);
                    }
                    self.fill().await?;
                }
                Mode::Json => {
                    if let Some(line) = self.json_line() {
                        if line.trim().is_empty() {
                            continue;
                        }
                        return json_row(&self.types, &line).map(Some);
                    }
                    if self.eof {
                        self.json_lines_done = true;
                        return Ok(None);
                    }
                    self.fill().await?;
                }
            }
        }
    }

    /// The server may append `Code: NNN. DB::Exception` after a 200 header when a query fails
    /// halfway; the client crate normally reports that itself, this is the fallback.
    fn bad_data(&self, why: String) -> Error {
        let tail = String::from_utf8_lossy(&self.buf[self.pos..]);
        if let Some(i) = tail.find("Code: ") {
            let text = &tail[i..];
            if crate::error::exception_code(text).is_some() {
                return from_server_text(text);
            }
        }
        Error::Other(format!("bad ClickHouse data: {why}"))
    }
}

fn append(buf: &mut Vec<u8>, chunk: &Bytes) {
    buf.extend_from_slice(chunk);
}

/// Column type shown to the user (the name from the header, re-rendered from the parse when the
/// header text is not available).
fn type_display(t: &CType, _cols: &[(String, CType)], _name: &str) -> String {
    render_type(t)
}

/// Render a [`CType`] as ClickHouse type text.
pub(crate) fn render_type(t: &CType) -> String {
    match t {
        CType::Int8 => "Int8".into(),
        CType::Int16 => "Int16".into(),
        CType::Int32 => "Int32".into(),
        CType::Int64 => "Int64".into(),
        CType::Int128 => "Int128".into(),
        CType::Int256 => "Int256".into(),
        CType::UInt8 => "UInt8".into(),
        CType::UInt16 => "UInt16".into(),
        CType::UInt32 => "UInt32".into(),
        CType::UInt64 => "UInt64".into(),
        CType::UInt128 => "UInt128".into(),
        CType::UInt256 => "UInt256".into(),
        CType::Float32 => "Float32".into(),
        CType::Float64 => "Float64".into(),
        CType::BFloat16 => "BFloat16".into(),
        CType::Bool => "Bool".into(),
        CType::String => "String".into(),
        CType::FixedString(n) => format!("FixedString({n})"),
        CType::Uuid => "UUID".into(),
        CType::Date => "Date".into(),
        CType::Date32 => "Date32".into(),
        CType::DateTime(None) => "DateTime".into(),
        CType::DateTime(Some(tz)) => format!("DateTime({})", quote_literal(tz)),
        CType::DateTime64(p, None) => format!("DateTime64({p})"),
        CType::DateTime64(p, Some(tz)) => format!("DateTime64({p}, {})", quote_literal(tz)),
        CType::Decimal(p, s) => format!("Decimal({p}, {s})"),
        CType::Enum8(items) | CType::Enum16(items) => {
            let body: Vec<String> = items
                .iter()
                .map(|(l, v)| format!("{} = {v}", quote_literal(l)))
                .collect();
            format!(
                "{}({})",
                if matches!(t, CType::Enum8(_)) {
                    "Enum8"
                } else {
                    "Enum16"
                },
                body.join(", ")
            )
        }
        CType::IPv4 => "IPv4".into(),
        CType::IPv6 => "IPv6".into(),
        CType::Nullable(i) => format!("Nullable({})", render_type(i)),
        CType::LowCardinality(i) => format!("LowCardinality({})", render_type(i)),
        CType::Array(i) => format!("Array({})", render_type(i)),
        CType::Tuple(items) => {
            let body: Vec<String> = items
                .iter()
                .map(|(n, t)| match n {
                    Some(n) => format!("{} {}", quote_ident(n), render_type(t)),
                    None => render_type(t),
                })
                .collect();
            format!("Tuple({})", body.join(", "))
        }
        CType::Map(k, v) => format!("Map({}, {})", render_type(k), render_type(v)),
        CType::Json => "JSON".into(),
        CType::Variant(items) => format!(
            "Variant({})",
            items.iter().map(render_type).collect::<Vec<_>>().join(", ")
        ),
        CType::Dynamic => "Dynamic".into(),
        CType::Nothing => "Nothing".into(),
        CType::Unsupported(name) => name.clone(),
    }
}

// ---------------------------------------------------------------------------------------------
// JSON fallback conversion
// ---------------------------------------------------------------------------------------------

fn json_row(types: &[CType], line: &str) -> Result<Vec<Value>> {
    let j: serde_json::Value = serde_json::from_str(line)
        .map_err(|e| Error::Other(format!("bad ClickHouse JSON row: {e}")))?;
    let serde_json::Value::Array(cells) = j else {
        // A server exception arrives as plain text in the stream.
        return Err(from_server_text(line));
    };
    Ok(cells
        .iter()
        .enumerate()
        .map(|(i, c)| match types.get(i) {
            Some(t) => json_cell(t, c),
            None => json_generic(c),
        })
        .collect())
}

/// Give a decimal text exactly `scale` fractional digits.
fn pad_decimal(text: &str, scale: u8) -> String {
    let scale = usize::from(scale);
    if scale == 0 {
        return text.to_string();
    }
    match text.split_once('.') {
        Some((int, frac)) if frac.len() < scale => {
            format!("{int}.{frac}{}", "0".repeat(scale - frac.len()))
        }
        Some(_) => text.to_string(),
        None => format!("{text}.{}", "0".repeat(scale)),
    }
}

fn json_text(j: &serde_json::Value) -> String {
    match j {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn json_generic(j: &serde_json::Value) -> Value {
    match j {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => n
            .as_i64()
            .map(Value::Int)
            .or_else(|| n.as_u64().map(Value::UInt))
            .unwrap_or_else(|| Value::Float(n.as_f64().unwrap_or(f64::NAN))),
        serde_json::Value::String(s) => Value::Text(s.clone()),
        serde_json::Value::Array(a) => Value::Array(a.iter().map(json_generic).collect()),
        serde_json::Value::Object(o) => Value::Document(
            o.iter()
                .map(|(k, v)| (k.clone(), json_generic(v)))
                .collect(),
        ),
    }
}

fn json_cell(t: &CType, j: &serde_json::Value) -> Value {
    use serde_json::Value as J;
    if j.is_null() {
        return Value::Null;
    }
    match t {
        CType::Nullable(i) | CType::LowCardinality(i) => json_cell(i, j),
        CType::Int8
        | CType::Int16
        | CType::Int32
        | CType::Int64
        | CType::UInt8
        | CType::UInt16
        | CType::UInt32 => match j {
            J::Number(n) => n
                .as_i64()
                .map(Value::Int)
                .unwrap_or_else(|| json_generic(j)),
            J::String(s) => s
                .parse()
                .map(Value::Int)
                .unwrap_or_else(|_| Value::Text(s.clone())),
            _ => json_generic(j),
        },
        CType::UInt64 => match j {
            J::Number(n) => n
                .as_u64()
                .map(Value::UInt)
                .unwrap_or_else(|| json_generic(j)),
            J::String(s) => s
                .parse()
                .map(Value::UInt)
                .unwrap_or_else(|_| Value::Text(s.clone())),
            _ => json_generic(j),
        },
        CType::Int128 | CType::Int256 | CType::UInt128 | CType::UInt256 => {
            Value::Decimal(json_text(j))
        }
        CType::Decimal(_, scale) => Value::Decimal(pad_decimal(&json_text(j), *scale)),
        CType::Float32 | CType::Float64 | CType::BFloat16 => match j {
            J::Number(n) => Value::Float(n.as_f64().unwrap_or(f64::NAN)),
            J::String(s) => Value::Float(match s.as_str() {
                "nan" | "NaN" => f64::NAN,
                "inf" | "Infinity" => f64::INFINITY,
                "-inf" | "-Infinity" => f64::NEG_INFINITY,
                other => other.parse().unwrap_or(f64::NAN),
            }),
            _ => json_generic(j),
        },
        CType::Bool => match j {
            J::Bool(b) => Value::Bool(*b),
            _ => json_generic(j),
        },
        CType::Uuid => Value::Uuid(json_text(j)),
        CType::Date | CType::Date32 => Value::Date(json_text(j)),
        CType::DateTime(_) | CType::DateTime64(..) => {
            let text = json_text(j);
            match rowbinary::parse_datetime(&text) {
                Some((days, tod, nanos)) => {
                    let secs = days * 86_400 + tod;
                    let precision = match t {
                        CType::DateTime64(p, _) => *p,
                        _ => 0,
                    };
                    let scale = 10i64.pow(u32::from(precision.min(9)));
                    let ticks = secs * scale + i64::from(nanos) / (1_000_000_000 / scale);
                    Value::Timestamp(rowbinary::fmt_timestamp(ticks, precision))
                }
                None => Value::Text(text),
            }
        }
        CType::Array(inner) => match j {
            J::Array(a) => Value::Array(a.iter().map(|x| json_cell(inner, x)).collect()),
            _ => json_generic(j),
        },
        CType::Tuple(items) => match j {
            J::Array(a) => {
                let vals: Vec<Value> = a
                    .iter()
                    .enumerate()
                    .map(|(i, x)| match items.get(i) {
                        Some((_, it)) => json_cell(it, x),
                        None => json_generic(x),
                    })
                    .collect();
                if !items.is_empty() && items.iter().all(|(n, _)| n.is_some()) {
                    Value::Document(
                        items
                            .iter()
                            .zip(vals)
                            .map(|((n, _), v)| (n.clone().unwrap_or_default(), v))
                            .collect(),
                    )
                } else {
                    Value::Array(vals)
                }
            }
            _ => json_generic(j),
        },
        CType::Map(_, v) => match j {
            J::Object(o) => Value::Document(
                o.iter()
                    .map(|(k, x)| (k.clone(), json_cell(v, x)))
                    .collect(),
            ),
            _ => json_generic(j),
        },
        CType::Json => Value::Json(match j {
            J::String(s) => s.clone(),
            other => other.to_string(),
        }),
        CType::Unsupported(_) => Value::Text(json_text(j)),
        _ => match j {
            J::String(s) => Value::Text(s.clone()),
            other => json_generic(other),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_parsing() {
        let v = Version::parse("26.3.62.3");
        assert_eq!((v.major, v.minor), (26, 3));
        assert!(v.at_least(24, 8));
        assert!(!Version::parse("23.8.16.40").at_least(24, 8));
        assert!(Version::parse("23.8.16.40").at_least(23, 3));
    }

    #[test]
    fn format_clause_is_removed() {
        assert_eq!(strip_format_clause("SELECT 1 FORMAT JSON"), "SELECT 1");
        assert_eq!(
            strip_format_clause("SELECT 1 format TabSeparated;"),
            "SELECT 1"
        );
        assert_eq!(strip_format_clause("SELECT 1"), "SELECT 1");
        assert_eq!(
            strip_format_clause("SELECT format FROM t"),
            "SELECT format FROM t"
        );
        assert_eq!(
            strip_format_clause("SELECT 'a FORMAT x'"),
            "SELECT 'a FORMAT x'"
        );
        assert_eq!(
            strip_format_clause("SELECT 1 AS format"),
            "SELECT 1 AS format"
        );
    }

    #[test]
    fn literals_and_identifiers() {
        assert_eq!(quote_literal("it's\\"), "'it\\'s\\\\'");
        assert_eq!(quote_ident("a`b"), "`a\\`b`");
    }

    #[test]
    fn type_rendering_round_trips() {
        for name in [
            "Array(Nullable(String))",
            "DateTime64(3, 'UTC')",
            "Decimal(18, 4)",
            "Map(String, UInt8)",
            "Enum8('a' = 1, 'b' = 2)",
            "LowCardinality(Nullable(String))",
        ] {
            assert_eq!(render_type(&CType::parse(name).unwrap()), name);
        }
    }

    #[test]
    fn json_cells_follow_types() {
        let t = CType::parse("Array(Nullable(UInt64))").unwrap();
        let j: serde_json::Value =
            serde_json::from_str("[\"18446744073709551615\", null]").unwrap();
        assert_eq!(
            json_cell(&t, &j),
            Value::Array(vec![Value::UInt(u64::MAX), Value::Null])
        );
        let t = CType::parse("Decimal(10, 2)").unwrap();
        assert_eq!(
            json_cell(&t, &serde_json::json!("1.50")),
            Value::Decimal("1.50".into())
        );
        let t = CType::parse("DateTime64(3)").unwrap();
        assert_eq!(
            json_cell(&t, &serde_json::json!("2024-01-02 03:04:05.678")),
            Value::Timestamp("2024-01-02T03:04:05.678Z".into())
        );
    }
}
