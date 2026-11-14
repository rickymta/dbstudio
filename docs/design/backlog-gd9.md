# DBStudio — Thiết kế đợt "Tồn đọng + GĐ9+ driver mới" (trừ Oracle, chỉ backend)

> Trạng thái: bản thiết kế v1 (kiến trúc sư), viết trên nhánh `feature/quandh/backlog` (tách từ `develop` @ `2cab204`).
> Nguồn đã đọc: `docs/{roadmap,requirements,architecture}.md`, `docs/design/phase2-8.md` (§7, §9), `crates/dbstudio-core/src/*`, `crates/dbstudio-{sql,plan,export,backup,store}`, `crates/drivers/*`, `src-tauri/src/{lib,state,session,lease,guard,events}.rs` + `commands/*`, `src/lib/types/**`, `src/lib/mock/*`, `docker/compose.test.yml`, `.github/workflows/ci.yml`, mã nguồn crate `scylla` 1.10.0, `clickhouse` 0.15.3, `mysql_async` 0.37.1, `mongodb` 3.10.1, `redis` 1.8.1 (registry cục bộ).
> Phiên bản crate kiểm ngày 2026-11-14 bằng `cargo search`/`cargo info`; tag image kiểm qua Docker Hub API (`hub.docker.com/v2/repositories/<repo>/tags/<tag>`; `docker manifest inspect` bị treo trên máy này). Không pull image.
> Quy ước giống `phase2-8.md`: "char offset" = chỉ số ký tự Unicode; serde camelCase; mọi field mới có `#[serde(default)]` để dữ liệu/JSON cũ vẫn đọc được.

## 0. Phạm vi và quyết định lớn

**Yêu cầu chủ dự án**: "Ngoài Oracle ra thì triển khai toàn bộ các phần còn lại trước; nhóm tính năng còn thiếu có thể triển khai các phần chưa gồm giao diện trước." Một session khác đang tối ưu frontend ở nhánh riêng, nên **đợt này không làm UI mới** (component, màn hình, menu, i18n hiển thị). Được phép sửa phía TS: kiểu trong `src/lib/types/**`, wrapper `src/lib/ipc/**`, mock tối thiểu trong `src/lib/mock/**`, và **các bảng dữ liệu bắt buộc theo kiểu** (vd. `Record<DbKind, …>`) để `pnpm lint/test/build` không vỡ (§1.13).

Loại khỏi phạm vi: mọi việc Oracle (IT với DB thật, Data Pump, UI chọn Instant Client, `Skip` khi import Oracle, phương án `oracle-rs`); GĐ9 đóng gói/phát hành; kiểm tay trong cửa sổ Tauri.

| # | Quyết định | Lý do ngắn |
|---|---|---|
| G1 | Làm như GĐ2–8: **Bước 0 dựng khung hợp đồng v2** (một agent) rồi chia gói song song có vùng file độc quyền (§6). | Tránh xung đột trên `lib.rs`, `types/*.ts`, `Cargo.toml`, migration, compose. |
| G2 | NewSQL có **`DbKind` riêng**: `cockroachdb`, `yugabytedb`, `tidb`; driver là **flavor** của driver PG/MySQL (cùng crate, đăng ký thêm `Driver`). Driver tự nhận diện flavor theo `version()` kể cả khi người dùng chọn nhầm `postgres`/`mysql`, và trả capabilities theo server qua `Session::capabilities()` (mới). | Cổng mặc định, nhãn, capabilities, backup native khác nhau; tái dùng ~90% mã driver. |
| G3 | Cassandra + ScyllaDB dùng **một crate `drivers/cassandra`** với `scylla = "1.10.0"` (driver chính thức, tương thích Cassandra), đăng ký 2 `Driver`. | Một mã nguồn; khác biệt nhỏ (trigger, UDF, tablets). |
| G4 | ClickHouse dùng **HTTP** qua crate chính thức `clickhouse = "0.15.3"` (`Query::fetch_bytes(format)` + `with_setting("query_id")`), định dạng `RowBinaryWithNamesAndTypes` với **decoder riêng** trong driver; insert hàng loạt bằng POST `FORMAT RowBinary` qua cùng client hyper. Không dùng `klickhouse` (native, cộng đồng). | Chính thức, chạy qua proxy/SSH/HTTPS dễ, Cloud mở 8443; RowBinary chính xác (không mất byte, Int128/Decimal đúng). |
| G5 | `scylla` 1.10 yêu cầu **MSRV 1.94** → nâng `rust-version` workspace 1.85 → 1.94 (máy dev đang 1.98.1). | Bản 1.x mới nhất; tránh khoá bản cũ. |
| G6 | R6 (kiểm tên host TLS qua SSH) **khả thi** cho cả MySQL, Redis, MongoDB, Scylla, ClickHouse — mỗi DB một kỹ thuật khác (§2.25). | Đã đọc mã nguồn client: có hook thật. |
| G7 | Tồn đọng chạm nhiều crate (`Error::Invalid`) làm theo kiểu "Bước 0 thêm biến thể + dùng ở core/store/src-tauri; mỗi gói tự đổi trong vùng mình; gói Z quét phần còn lại". | Không gói nào phải sửa ngoài vùng. |
| G8 | Kiểm thử tích hợp mới theo mẫu **probe-rồi-bỏ-qua**: thiếu container → test in `skip` và pass; `DBSTUDIO_IT_STRICT=1` (CI) biến thiếu container thành lỗi. Áp luôn cho `tunnel_reaches_mssql_when_it_runs`. | Docker VM chỉ ~8 GB, không chạy hết cùng lúc. |
| G9 | Compose thêm service theo **profile** (`wide`, `analytics`, `newsql`, `heavy`, `extra`), mỗi service có `mem_limit` nhỏ nhất chạy được; CI chạy theo **matrix nhóm service**. | RAM. |
| G10 | Mọi hợp đồng giữ quy tắc D5 của GĐ2: method trait mới đều có mặc định; thêm field vào struct dựng bằng literal chỉ khi Bước 0 sửa hết chỗ dựng (đã đếm, §1.1). | Driver cũ biên dịch được sau Bước 0. |

---

## 1. Hợp đồng v2 (Bước 0)

### 1.1 Bảng thay đổi (tương thích ngược)

| Thay đổi | Tương thích? | Chỗ phải sửa ở Bước 0 |
|---|---|---|
| `DbKind` thêm `CockroachDb`/`YugabyteDb`/`TiDb` (`ALL` 9 → 12) | Thêm (enum `non_exhaustive`) | TS union `DbKind`; `kindConfig.ts` (`DATABASE_FIELD`, `KIND_BADGE`); mock `sessions.ts` (`DIALECTS`); `planned_for`, `planned_phase` (10); fixture. |
| `Error::Invalid(String)` (kind `"invalid"`) | Thêm | `ERROR_KINDS` trong `src/lib/ipc`; fixture; dùng ở core/store/src-tauri (§1.3). |
| `ScriptDialect::MySql` thêm `ansi_quotes: bool` (`#[serde(default)]`); thêm biến thể `Cql`, `ClickHouse` | Thêm (JSON cũ đọc được) | 92 chỗ `ScriptDialect::MySql {` — chỗ dựng thêm `ansi_quotes: false` (hoặc dùng helper `ScriptDialect::mysql(mariadb, bs)` mới), chỗ match đã có `..` giữ nguyên; TS thêm `ansiQuotes?: boolean`, `{name:'cql'}`, `{name:'clickHouse'}`. |
| `DestructiveReason` thêm `Write`, `KeywordScan` | Thêm | TS union; `commands/plan.rs` bỏ đệm `UpdateWithoutWhere` (gói E). |
| `Capabilities` thêm `table_sort`, `exact_count` | Thêm (serde default false) | `planned_for`: đặt `true` cho mọi kind hiện có trừ Redis/Mongo theo bảng §1.5; TS. |
| Trait `Session` thêm `capabilities()`, `scan_keys()` (mặc định) | Thêm | Không driver nào phải sửa. |
| `KeyChange::ListSet`/`ListRemoveAt` thêm `expected: Option<Value>`; `KeyChange::Put`; kiểu `KeyData`, `PutMode` | Thêm | 7 chỗ dựng `ListSet/ListRemoveAt` (Rust+TS). |
| `DocumentChange::{Replace,Update,Delete}` thêm `expected: Vec<FieldOriginal>` | Thêm | Chỗ dựng trong test Mongo/core. |
| `Editability.notes: Vec<EditNote>` | Thêm | 23 chỗ `Editability {` thêm `notes: vec![]`. |
| `PlanFormat` thêm `ClickHouseJson`, `CockroachText`, `TiDbRows`, `CqlTrace` | Thêm | `dbstudio-plan::parse` dispatch tới module stub; TS. |
| `ObjectKind::Dictionary`; folder `DICTIONARIES`, `AGGREGATES` | Thêm | TS union + `FOLDER_KEYS`; icon/i18n để UI (§7). |
| `DdlLanguage::Cql` | Thêm | TS. |
| `ConnectionConfig.environment_label: Option<String>` | Thêm | Store đọc/ghi cột mới; chỗ dựng literal `ConnectionConfig {` thêm `environment_label: None`. |
| `AppSettings`: **bỏ `Copy`**, thêm 7 field (§1.10) | Thêm (Rust: sửa chỗ copy ngầm → `.clone()`) | `settings.rs`, `from_value_lenient`, validate; TS `AppSettings` + `DEFAULT_SETTINGS`. |
| `EditorTabState.dirty: bool` | Thêm | Store `tabs.rs`, TS. |
| `DriverDiagnostics.checks: Vec<DiagnosticCheck>` | Thêm | `commands/app.rs`, TS, mock. |
| `NativeTool` thêm `ClickHouseServerBackup`, `CockroachServerBackup`, `YsqlDump`, `Ysqlsh` | Thêm | `ALL` 11 → 15; `jobForm.ts` dùng `Partial<Record>` nên không vỡ. |
| `ImportFormat::Csv` thêm `null_text`; `ImportTarget::Keys`; `ImportPreview.dialect_guessed: bool` | Thêm | Variant `Csv` phải thêm `#[serde(rename_all = "camelCase")]` trên chính variant (field nhiều từ). |
| `BackupInspect.objects: Vec<BackupObject>` (`{kind, schema, name, phases, rows}`) | Thêm | `dbstudio-backup`, TS. |
| `BulkInsertRequest`: quy ước cột `$document` (Mongo) | Chỉ tài liệu | Doc comment. |
| Store migration **v7** | Thêm | §1.11; test `MIGRATIONS.len() == 7`. |
| Command IPC mới + event `session://state` | Thêm | §1.12; command GĐ1–8 không đổi chữ ký. |

**Quy tắc serde** giữ nguyên `phase2-8.md` §1.1 (struct variant có field nhiều từ phải có `rename_all` trên variant; enum mặc định là struct variant → `impl Default` viết tay).

### 1.2 `DbKind` (`kind.rs`)

```rust
#[serde(rename = "cockroachdb")] CockroachDb,   // "CockroachDB", cổng 26257, Relational
#[serde(rename = "yugabytedb")]  YugabyteDb,    // "YugabyteDB",  cổng 5433 (YSQL), Relational
#[serde(rename = "tidb")]        TiDb,          // "TiDB",        cổng 4000, Relational
pub const ALL: [DbKind; 12] = [Postgres, MySql, SqlServer, Oracle, MongoDb, Redis,
    Cassandra, ScyllaDb, ClickHouse, CockroachDb, YugabyteDb, TiDb];
```
`DriverDescriptor::planned_phase` trả 10 cho 6 kind mới (giữ `Planned` tới khi gói driver đăng ký). Chế độ YCQL của Yugabyte (giao thức Cassandra) **không** thuộc kind này; người dùng có thể dùng kind `cassandra` trỏ tới cổng 9042 của YB (không kiểm thử, ghi trong tài liệu người dùng).

### 1.3 `Error::Invalid`

```rust
/// Input rejected before reaching the server: malformed argument, out-of-range setting,
/// bad name, unsupported combination of options chosen by the caller.
#[error("invalid input: {0}")]
Invalid(String),
```
`kind()` = `"invalid"`, `message()` = chuỗi, serialize không có field phụ. Quy tắc dùng (ghi vào doc comment của `Error`):
- `Invalid`: lỗi do **đầu vào của người gọi** (tham số IPC, setting ngoài khoảng, tên rỗng/quá dài, cursor/khóa nhị phân hỏng, chu trình thư mục saved query, file import sai cấu trúc).
- `Permission`: server hoặc chính sách của app **từ chối quyền** (NOPERM, ORA-1031, `PathGrants`, lệnh Redis bị chặn). Không dùng cho đầu vào sai.
- `Other`: chỉ khi thật sự không phân loại được.
- `Storage("invalid data: …")` của store (`db::invalid`) chỉ cho **dữ liệu đã lưu bị hỏng**; validate đầu vào của store chuyển sang `Invalid`.

Bước 0 đổi trong: `dbstudio-core` (`keyvalue::decode_key_name`, validate trong `lease_kit`/`fallback` nếu có), `dbstudio-store` (`validate.rs`, `saved.rs` chu trình thư mục, `settings.rs` validate), `src-tauri/src/commands/*` (tham số sai). Các crate khác do gói sở hữu tự đổi (§6), gói Z quét lại bằng `grep -rn "Error::Other(" crates src-tauri`.

### 1.4 Script (`script.rs`)

```rust
pub enum ScriptDialect {
    Postgres,                                   // cả CockroachDB, YugabyteDB (YSQL)
    #[serde(rename_all = "camelCase")]
    MySql {
        mariadb: bool,
        backslash_escapes: bool,
        /// sql_mode ANSI_QUOTES (hoặc ANSI): `"x"` là định danh, không phải chuỗi.
        #[serde(default)]
        ansi_quotes: bool,
    },                                          // cả TiDB
    MsSql, Oracle, MongoShell, RedisCli,
    /// CQL (Cassandra / ScyllaDB).
    Cql,
    /// ClickHouse SQL.
    ClickHouse,
    Opaque,
}
impl ScriptDialect {
    /// MySQL dialect without ANSI_QUOTES (helper for the many existing call sites).
    pub const fn mysql(mariadb: bool, backslash_escapes: bool) -> Self { .. }
}

pub enum DestructiveReason {
    /* giữ nguyên 14 biến thể */
    /// An ordinary data-modifying statement listed for confirmation (e.g. EXPLAIN ANALYZE of a
    /// write on a production connection). Replaces the `UpdateWithoutWhere` placeholder.
    Write,
    /// A destructive keyword (DROP/TRUNCATE/ALTER/DELETE/UPDATE) found while scanning a
    /// statement the classifier did not understand (`Unknown`/`Procedural`).
    KeywordScan,
}
```
JSON: `{"name":"cql"}`, `{"name":"clickHouse"}`, `"write"`, `"keywordScan"`. Quét từ khoá trong `src-tauri/src/guard.rs`: `DROP`→`Drop`, `TRUNCATE`→`Truncate` giữ nguyên; `ALTER`/`DELETE`/`UPDATE` trong câu không hiểu → `KeywordScan` (trước đây đoán `AlterDrop`/`DeleteWithoutWhere`).

### 1.5 `Capabilities` mới và giá trị

```rust
/// The data grid can sort by any column server-side (false: CQL — only clustering order).
#[serde(default)] pub table_sort: bool,
/// `count_table` returns an exact count cheaply enough for the grid (false: CQL full scan).
#[serde(default)] pub exact_count: bool,
```

| Kind | Thay đổi so với `planned_for` hiện tại |
|---|---|
| PG, MySQL, MSSQL, Oracle, Mongo | `table_sort = true`, `exact_count = true` |
| Redis | cả hai `false` (không có lưới bảng) |
| Cassandra | `sql, multiple_databases, explain_analyze (tracing, §3.1.8), cancel_query (phía client), logical_backup, data_editing, stored_routines, triggers, views, object_search, leases, query_params, ddl_generation, schema_model, table_stream, bulk_insert, ssh_tunnel`; **không**: `transactions, manual_transactions, explain, foreign_keys, sequences, explain_analyze_writes, native_backup, read_only_session, table_sort, exact_count, schemas` |
| ScyllaDB | như Cassandra, trừ `triggers = false`, `stored_routines = false` (UDF Lua thử nghiệm) |
| ClickHouse | `sql, multiple_databases, explain, cancel_query, logical_backup, data_editing (hạn chế, §3.2.6), views, object_search, leases, query_params, ddl_generation, schema_model, table_stream, bulk_insert, native_backup (BACKUP phía server), read_only_session (readonly=2), ssh_tunnel, table_sort, exact_count`; **không**: `transactions, manual_transactions, explain_analyze, foreign_keys, stored_routines, triggers, sequences, schemas` |
| CockroachDB | như PG, trừ `triggers` (bật theo phiên bản ≥ 24.3 qua `Session::capabilities()`), `native_backup` = `CockroachServerBackup`, `stored_routines` theo phiên bản (≥ 23.1) |
| YugabyteDB | như PG; `native_backup` = `ysql_dump` |
| TiDB | như MySQL, trừ `stored_routines = false`, `triggers = false`, `sequences = true`, `read_only_session = false` cho tới khi IT chứng minh (§3.3.3) |

### 1.6 Trait `Session` — method mới (đều có mặc định)

```rust
/// Capabilities of THIS server when they differ from the driver descriptor (flavour detected
/// from `version()`, server version gates). `session_open` puts them in
/// `SessionInfo.descriptor.capabilities`.
fn capabilities(&self) -> Option<Capabilities> { None }

/// One page of raw key names of a key-value database (export, backup). Names use the
/// `Key` segment encoding (`keyvalue::encode_key_name`).
async fn scan_keys(&self, req: KeyScanRequest) -> Result<KeyScanPage> {
    let _ = req; Err(Error::Unsupported("scan_keys".into()))
}
```
Driver flavor ghi thêm `ServerInfo.extra["flavor"]` = `postgres|cockroachdb|yugabytedb|mysql|mariadb|tidb|cassandra|scylladb`.

### 1.7 Key-value (`keyvalue.rs`, `edit.rs`)

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyScanRequest {
    pub database: ObjectPath,              // [Database "0"]
    #[serde(default)] pub pattern: Option<String>,   // SCAN MATCH
    #[serde(default)] pub cursor: Option<String>,
    pub count: u32,                        // gợi ý COUNT, kẹp 1..=10_000
    #[serde(default)] pub type_filter: Option<String>, // SCAN TYPE (Redis ≥ 6)
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyScanPage { pub keys: Vec<String>, pub next_cursor: Option<String> }

/// Full value of a key, by type (export/import/backup and `KeyChange::Put`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum KeyData {
    String { value: Value },
    Hash { fields: Vec<(Value, Value)> },
    List { items: Vec<Value> },
    Set { members: Vec<Value> },
    ZSet { members: Vec<(Value, String)> },
    Stream { entries: Vec<StreamEntry> },   // consumer group KHÔNG được sao lưu (cảnh báo)
    Json { text: String },
}

/// One exported key (JSON export, backup NDJSON line, Redis import record).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyRecord {
    pub key: String,                        // encode_key_name
    pub data: KeyData,
    /// Absolute expiry (Unix ms); `None` = persistent.
    #[serde(default)] pub expire_at_ms: Option<i64>,
}

// edit.rs — KeyChange
ListSet { key: String, index: i64, value: Value,
          /// Value read by the UI; mismatch => ChangeFailed (WATCH + LINDEX + MULTI).
          #[serde(default)] expected: Option<Value> },
ListRemoveAt { key: String, index: i64, #[serde(default)] expected: Option<Value> },
/// Write a whole key (import/restore). Built in a temp key then RENAME(NX) so a reader never
/// sees a half-written value; big values are written in chunks.
#[serde(rename_all = "camelCase")]
Put { key: String, data: KeyData, #[serde(default)] ttl_ms: Option<u64>,
      #[serde(default)] mode: PutMode },

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PutMode {
    /// Overwrite an existing key.
    #[default] Replace,
    /// Keep an existing key; `ApplyResult.affected[i] = 0` (skipped, not an error).
    IfAbsent,
}
```

### 1.8 Document và editability (`edit.rs`)

```rust
/// Value of a field when the document was read; `None` = the field was absent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FieldOriginal { pub path: Vec<String>, pub value: Option<Value> }

// DocumentChange
Replace { id: Value, document: DocInput, #[serde(default)] expected: Vec<FieldOriginal> },
Update  { id: Value, set: Vec<FieldEdit>, #[serde(default)] unset: Vec<Vec<String>>,
          #[serde(default)] expected: Vec<FieldOriginal> },
Delete  { id: Value, #[serde(default)] expected: Vec<FieldOriginal> },

// Editability
/// Caveats the UI must show before applying (empty for classic SQL tables).
#[serde(default)] pub notes: Vec<EditNote>,

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EditNote {
    /// INSERT overwrites a row with the same key (CQL without LWT).
    UpsertSemantics,
    /// Writes are conditional (CQL LWT `IF EXISTS`/`IF NOT EXISTS`): slower, conflict-safe.
    ConditionalWrites,
    /// UPDATE/DELETE run as mutations (ClickHouse `ALTER TABLE … UPDATE/DELETE`): heavy,
    /// rewrite data parts.
    AsyncMutation,
}
```

### 1.9 Plan, cây, DDL

```rust
// plan.rs — PlanFormat
/// `EXPLAIN json = 1, indexes = 1, actions = 1 PLAN …`; `extra["estimate"]` = JSON rows of
/// `EXPLAIN ESTIMATE`, `extra["pipeline"]` = text of `EXPLAIN PIPELINE`.
ClickHouseJson,
/// CockroachDB `EXPLAIN [ANALYZE] (VERBOSE)` text tree (`• node`, `│ attr: value`).
CockroachText,
/// TiDB `EXPLAIN [ANALYZE] FORMAT='row'` rows packed by the driver as a JSON array of objects
/// (`id`, `estRows`, `actRows`, `task`, `access object`, `execution info`, `operator info`,
/// `memory`, `disk`).
TiDbRows,
/// CQL tracing session (`system_traces.events`) as JSON (`scylla::TracingInfo`).
CqlTrace,

// object.rs
ObjectKind::Dictionary,                 // ClickHouse dictionary
folder::DICTIONARIES = "dictionaries"; folder::AGGREGATES = "aggregates";
// model.rs
DdlLanguage::Cql,
```
YugabyteDB dùng lại `PostgresJson` (thuộc tính riêng như `Storage Rows Scanned` vào `props`).

### 1.10 Cấu hình, setting, store

```rust
// config.rs — ConnectionConfig
/// Custom environment label shown instead of the standard name (≤ 32 chars). Safety rules still
/// follow `environment` (production = confirmations); colour = profile `color`.
#[serde(default)] pub environment_label: Option<String>,

// dbstudio-store settings.rs — AppSettings (bỏ derive Copy)
pub editor_font_family: String,        // "" = font mono mặc định của theme; ≤ 100 ký tự, không ký tự điều khiển
pub editor_font_size: u16,             // 13; 8..=32
pub auto_reconnect: bool,              // true (CONN-12)
pub keepalive_secs: u32,               // 240; 0 = tắt; 30..=3600 (lease keepalive + health check)
pub plan_history_enabled: bool,        // true (PLAN-07)
pub plan_history_max_per_connection: u32, // 200; 10..=5000
pub language: UiLanguage,              // System (thông báo OS, §2.29)

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UiLanguage { #[default] System, Vi, En }

// tabs.rs — EditorTabState
/// Text differs from `file_path` on disk (or the tab was never saved).
#[serde(default)] pub dirty: bool,
```
Đã có sẵn (chỉ còn UI): `editor_max_rows`, `query_timeout_ms`, `history_enabled`, `history_max_per_connection`, `close_to_tray`; autostart đọc/ghi qua `background_get/set` (trạng thái OS, không nằm trong `AppSettings`).

### 1.11 Store migration v7

```sql
-- v7: environment label, editor dirty flag, plan history (CONN-03, PLAN-07)
ALTER TABLE connections ADD COLUMN environment_label TEXT;
ALTER TABLE editor_tabs ADD COLUMN dirty INTEGER NOT NULL DEFAULT 0;
CREATE TABLE plan_history (
    id            TEXT PRIMARY KEY NOT NULL,
    connection_id TEXT REFERENCES connections(id) ON DELETE CASCADE,
    created_at    TEXT NOT NULL,
    database_name TEXT,
    statement     TEXT NOT NULL,          -- đã che bằng dbstudio_sql::redact
    analyze       INTEGER NOT NULL,
    format        TEXT NOT NULL,          -- PlanFormat
    raw_plan      TEXT NOT NULL,          -- RawPlan JSON (≤ 8 MiB, lớn hơn thì không lưu)
    total_cost    REAL,
    total_ms      REAL,
    warnings      INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_plan_history_conn ON plan_history(connection_id, created_at DESC);
```
Danh sách thư mục export gần đây **không cần migration**: khoá `export.recentDirs` trong bảng `settings` (JSON mảng ≤ 20 đường dẫn, mới nhất trước).

### 1.12 Command IPC và event mới

Mọi command có `*_impl` stub ở Bước 0, đăng ký `generate_handler!` + dispatch dev bridge, wrapper TS, mock.

| Command | Chữ ký (TS) | Gói hiện thực |
|---|---|---|
| `session_reconnect` | `(sessionId, secret?: string \| null) → SessionInfo` (giữ `sessionId`) | E |
| `lease_reopen` | `(leaseId) → { lease: LeaseInfo; transactionLost: boolean }` (giữ `leaseId`) | E |
| `script_generate` | `(sessionId, path: ObjectPath, template: 'select' \| 'insert' \| 'update' \| 'delete') → { text: string; dialect: ScriptDialect }` | E (+ S cho `dbstudio_sql::template`) |
| `plan_history_list` | `(connectionId: Uuid \| null, limit: number, before?: string \| null) → PlanHistoryEntry[]` (không kèm `rawPlan`) | E |
| `plan_history_get` | `(id) → PlanHistoryRecord` (cùng hình dạng nội dung `.dbplan.json`) | E |
| `plan_history_delete` / `plan_history_clear` | `(id) → void` / `(connectionId: Uuid \| null) → number` | E |
| `driver_diagnostics` | không đổi tham số; kết quả thêm `checks` | E |
| `backup_inspect` | không đổi tham số; kết quả thêm `objects: BackupObject[]` (rỗng với file `formatVersion < 3`) | B |
| `restore_start` | không đổi; `objects` khác rỗng nay hợp lệ với backup logical v3 | B |
| `export_start` / `import_preview` / `import_start` | không đổi; nhận `ExportSource::Keys`, đích Mongo, `ImportTarget::Keys`, `nullText` | X |

```rust
// src-tauri events.rs
pub const SESSION_STATE: &str = "session://state";
#[derive(Debug, Clone, Serialize)] #[serde(rename_all = "camelCase")]
pub struct SessionStateEvent {
    pub session_id: Uuid, pub connection_id: Uuid,
    pub state: SessionHealth, pub attempt: u32,
    pub error: Option<serde_json::Value>,       // Error serialize
    pub leases_broken: Vec<Uuid>,
}
#[derive(Debug, Clone, Copy, Serialize)] #[serde(rename_all = "camelCase")]
pub enum SessionHealth { Connected, Lost, Reconnecting, Reconnected, GaveUp }

// commands/app.rs
#[serde(rename_all = "camelCase")]
pub struct DriverDiagnostics {
    pub status: DriverStatus, pub message: String, pub help_url: Option<String>,
    #[serde(default)] pub checks: Vec<DiagnosticCheck>,
}
#[serde(rename_all = "camelCase")]
pub struct DiagnosticCheck {
    /// Stable id: `driverRegistered`, `nativeTool:<tool>`, `nativeClient`, `clientVersion`...
    pub id: String, pub ok: bool,
    pub value: Option<String>, pub detail: Option<String>,
}
```
`PlanHistoryEntry = { id, connectionId, createdAt, database, statement, analyze, format, totalCost, totalMs, warnings }`; `PlanHistoryRecord = PlanHistoryEntry & { rawPlan: RawPlan }`.

### 1.13 Frontend tối thiểu ở Bước 0 (không UI mới)

- `src/lib/types/**`: mọi kiểu ở §1.2–1.12 (+ `KeyScanRequest/Page`, `KeyData`, `KeyRecord`, `PutMode`, `FieldOriginal`, `EditNote`, `UiLanguage`, `SessionStateEvent`, `DiagnosticCheck`, `PlanHistory*`, `BackupObject`).
- `src/lib/ipc/**`: wrapper cho command §1.12, `ERROR_KINDS` thêm `invalid`, `ipc.onEvent('session://state')`.
- `src/lib/mock/**`: `plan_history_*` (danh sách rỗng/ghi nhớ trong bộ nhớ), `script_generate` (chuỗi mẫu theo dialect), `session_reconnect` (trả `SessionInfo` cũ), `lease_reopen`, `DIALECTS` thêm 3 kind (Cassandra/Scylla → `{name:'cql'}`, ClickHouse → `{name:'clickHouse'}`, Cockroach/YB → `postgres`, TiDB → `mySql`).
- Bảng bắt buộc theo kiểu: `src/features/connections/kindConfig.ts` (`DATABASE_FIELD`: `database`; `KIND_BADGE`: `CR`, `YB`, `TI`). **Đây là file của vùng UI** → Bước 0 chỉ thêm 6 dòng này, báo session UI trước khi merge (rủi ro R11).
- `contract/fixtures/*.json` + `src/lib/types/contract.test.ts`: mẫu cho mọi kiểu mới.

### 1.14 Phần còn lại của Bước 0

1. Workspace: `rust-version = "1.94"`; dependency mới ghi sẵn (`scylla = { version = "1.10.0", default-features = false, features = ["rustls-023"] }`, `clickhouse = { version = "0.15.3", default-features = false, features = ["lz4", "rustls-tls-ring"] }`, `hyper-util = "0.1.21"`, `hyper-rustls = "0.27.10"` (ring), `sys-locale = "0.3.2"`, `fast-socks5 = "1.0.0"` (cho `dbstudio-ssh`), `mongodb` thêm feature `socks5-proxy`).
2. Crate rỗng `crates/drivers/{cassandra,clickhouse}` (driver đăng ký được, `connect` → `Unsupported`), feature `cassandra`, `clickhouse` trong `src-tauri`/`dbstudio-cli` (bật mặc định khi gói driver merge). PG/MySQL: `register()` thêm `Driver` cho Cockroach/Yugabyte/TiDB trả `Unsupported` tới khi N1/N2 xong.
3. `dbstudio-plan`: module stub `clickhouse.rs`, `cockroach.rs`, `tidb.rs`, `cql_trace.rs` (trả `PlanError::Unsupported`) và nhánh dispatch.
4. `dbstudio-sql`: nhánh `Cql`/`ClickHouse` ở mọi `match` (tạm hành xử như `Opaque`), module stub `template.rs` (`pub fn generate(dialect, &TableDetails, path, ScriptTemplate) -> String`).
5. `dbstudio-store`: migration v7, `plan_history.rs` (chữ ký repository, thân `todo` → gói E), cột `environment_label`, `dirty`.
6. `src-tauri`: command stub, event, `lib.rs` setup gọi `dbstudio_export::startup_cleanup(&store)` (stub, gói X) và `SessionManager::spawn_health(...)` (stub, gói E).
7. `docker/compose.test.yml` + `docker/gen-certs.sh` + thư mục cấu hình service mới (§5), CI matrix (§5.3).

**Xong khi**: `cargo fmt --check`, `cargo clippy --workspace --all-features -D warnings`, `cargo test --workspace` (gồm fixture hợp đồng) xanh; `pnpm lint && pnpm test && pnpm build` xanh; `docker compose -f docker/compose.test.yml --profile wide config` hợp lệ; app chạy với mock không lỗi kiểu. Sau mốc này đổi hợp đồng = PR riêng sửa core + TS + fixture, các gói rebase.

---

## 2. Tồn đọng (mục A) — thiết kế từng việc

Mỗi mục: **Backend** (đợt này) · **IPC/Store** · **Xong khi** (kiểm chứng tự động) · **UI để lại**.

### 2.1 Import CSV `null_text` (gói X)
- Backend: `ImportFormat::Csv { …, #[serde(default)] null_text: Option<String> }`. `None` = hành vi cũ (ô rỗng không ngoặc = NULL). `Some(t)`: ô **không ngoặc** đúng bằng `t` = NULL; ô rỗng = chuỗi rỗng; ô có ngoặc luôn là chuỗi (khớp writer CSV: writer chỉ không bọc ngoặc `null_text`). Preview áp cùng luật.
- Xong khi: test khứ hồi export CSV với `null_text = "NULL"` và `"\\N"` → import → so sánh NULL vs `""` vs `"NULL"` có ngoặc; IT PG.
- UI để lại: ô "Chuỗi NULL" trong wizard import (gợi ý theo preset export).

### 2.2 Export Mongo `flatten_fields` (gói X)
- Backend: engine đọc `ValueFormat.flatten_fields` khi nguồn trả cột `$document` và định dạng CSV/XLSX. Rỗng → cột = hợp khoá cấp 1 của **1000 document đầu** (đệm 1000 dòng rồi mới ghi header; `_id` đầu tiên). Có đường dẫn → trích theo `a.b.c`, chỉ số mảng `items.0.name`; giá trị document/mảng con → relaxed EJSON text; thiếu → ô rỗng/NULL theo `null_text`. Khoá xuất hiện sau 1000 document đầu bị bỏ, ghi `warnings` ("field X first seen at row N").
- Xong khi: unit test với document không đồng nhất + đường dẫn lồng; IT `mongo7` export CSV/XLSX.
- UI để lại: chọn trường/đường dẫn trong wizard export cho nguồn Mongo.

### 2.3 Export nguồn `Keys` (Redis) (gói X; driver gói K)
- Backend: engine bỏ `Unsupported("export_keys")`; lặp `Session::scan_keys` (pattern, count 1000) → với mỗi khoá đọc đủ bằng `read_key` phân trang (string: `max_string_bytes` = 512 MiB, vượt → cảnh báo + bỏ khoá) → `KeyRecord` (TTL đổi thành `expire_at_ms` tuyệt đối). Chỉ JSON (`Array`/`Ndjson`); định dạng khác → `Invalid`. Khoá biến mất giữa chừng → bỏ qua, đếm vào `warnings`. Tiến độ theo số khoá (tổng = `DBSIZE` ước lượng).
- Xong khi: IT `redis7`/`valkey8`: đủ 6 kiểu + TTL + khoá nhị phân → NDJSON → so sánh với dữ liệu nguồn.
- UI để lại: nguồn "Redis keys" trong wizard export (pattern, database).

### 2.4 Import vào MongoDB/Redis (gói X; driver gói K)
- Mongo: cho phép dialect `MongoShell` ở allow-list `import/mod.rs`. CSV/XLSX: mapping cột → field (đường dẫn chấm tạo document lồng), giá trị text qua luật `EditValue::Text` hiện có của Mongo `bulk_insert`. JSON/NDJSON: mỗi bản ghi → **một document nguyên vẹn** qua quy ước `BulkInsertRequest { columns: ["$document"], rows: [[EditValue::Value(Value::Document)]] }` (Mongo `bulk_insert` nhận diện; EJSON relaxed/canonical được parse). `OnConflict`: `Error` (insert, lỗi trùng `_id` → báo dòng), `Skip` (`ordered:false` + đếm lỗi 11000), `Update` (`replaceOne upsert` theo `_id`). `ImportTarget::New` cho Mongo = tạo collection.
- Redis: `ImportTarget::Keys { database: ObjectPath, mode: PutMode }` + `ImportFormat::Json` (bản ghi phải đúng hình `KeyRecord`); áp bằng `ChangeSet::Keys` lô 200 `KeyChange::Put` (không nguyên tử, `atomic=false`); khoá đã hết hạn (`expire_at_ms` quá khứ) bị bỏ, đếm `skipped`.
- Xong khi: IT `mongo7`: CSV có mapping lồng, NDJSON EJSON (ObjectId/Decimal128/Date khứ hồi), `Skip`/`Update`; IT `redis7`: export §2.3 → FLUSHDB → import → khớp từng khoá + TTL ± 2 s.
- UI để lại: đích Mongo/Redis trong wizard import, chọn `PutMode`.

### 2.5 `KeyChange` list có giá trị kỳ vọng (gói K)
- Backend: `ListSet`/`ListRemoveAt` có `expected` → `WATCH key; LINDEX key i` so với `expected` (khác → `ChangeFailed { message: "list item changed" }`), rồi `MULTI; LSET …; EXEC` (EXEC null → `ChangeFailed`). `expected = None` giữ hành vi cũ. Preview hiển thị điều kiện.
- Xong khi: IT: client thứ hai đổi phần tử giữa lúc đọc và áp → lỗi, dữ liệu client hai giữ nguyên.
- UI để lại: key viewer gửi `expected` từ giá trị đã tải; hiển thị lỗi xung đột + nút tải lại.

### 2.6 Phát hiện sửa đồng thời document Mongo (gói K)
- Backend: `expected: Vec<FieldOriginal>` → filter `{_id, "<path>": <bson gốc>}` hoặc `{"<path>": {$exists: false}}`. Khớp 0 → kiểm `_id` còn tồn tại: còn → `ChangeFailed { message: "document was modified by another client" }`, mất → "document no longer exists". Transaction (replica set) rollback như hiện tại; standalone dừng ở lỗi đầu. Đã cân nhắc trường phiên bản (`__v`) — loại vì phụ thuộc quy ước của ứng dụng; ghi trong §8.
- Xong khi: IT `mongo7` + `mongo44`: Update/Replace/Delete với giá trị gốc lệch → lỗi đúng; không lệch → áp.
- UI để lại: document editor gửi giá trị gốc của field đã sửa (Replace: mọi field cấp 1), hiển thị xung đột.

### 2.7 `Error::Invalid` (Bước 0 + mọi gói + Z) — §1.3.

### 2.8 Lý do cho câu ghi thường (gói E, S)
- Backend: `commands/plan.rs` dùng `stmt.destructive.unwrap_or(DestructiveReason::Write)`; guard quét từ khoá dùng `KeywordScan` (§1.4).
- Xong khi: test command: `EXPLAIN ANALYZE UPDATE t SET a=1 WHERE id=1` trên prod → `confirmation.statements[0].reason == "write"`; câu `Unknown` chứa `DELETE` → `keywordScan`.
- UI để lại: nhãn i18n cho 2 lý do mới.

### 2.9 `DriverDiagnostics` có cấu trúc (gói E)
- Backend: §1.12. Mọi kind: `driverRegistered`; thêm `nativeTool:<tool>` từ `backup_tools_detect` (đường dẫn + phiên bản). Oracle giữ thân hiện tại (đợt Oracle sẽ điền `nativeClient`, `clientVersion`, `searchPath`).
- Xong khi: test command cho kind có/không đăng ký; JSON khớp fixture.
- UI để lại: trang chẩn đoán dạng danh sách check.

### 2.10 `EditorTabState.dirty` (gói E) — §1.10; store đọc/ghi cột; test khứ hồi. UI để lại: chấm "chưa lưu" và hỏi khi đóng dựa vào cờ sau khôi phục.

### 2.11 Dọn file tạm export mồ côi khi khởi động (gói X)
- Backend: mỗi export ghi thư mục đích vào `export.recentDirs` (dedupe, ≤ 20). `startup_cleanup(store)` chạy nền lúc khởi động (sau 10 s, không chặn UI): với mỗi thư mục còn tồn tại gọi `cleanup_orphans(dir, 1 giờ)`; thư mục không còn → bỏ khỏi danh sách. Chỉ xoá tệp đúng mẫu `.<name>.dbstudio-tmp-<id>`.
- Xong khi: unit test với store tạm + thư mục tạm có file mồ côi cũ/mới.
- UI để lại: không.

### 2.12 MariaDB preview dialect (gói E + X)
- Backend: `SessionManager` nhớ `last_dialect: HashMap<connection_id, ScriptDialect>` (cập nhật mỗi `session_open`, không lưu đĩa). `import_preview` khi không có session: dùng `last_dialect`, không có thì suy từ `DbKind` và trả `ImportPreview.dialect_guessed = true` (field mới `#[serde(default)]`).
- Xong khi: test command: mở session MariaDB, đóng, preview → dialect `mariadb: true`.
- UI để lại: ghi chú "dialect đoán" trong preview.

### 2.13 Lease keepalive (gói E)
- Backend: `LeaseManager::spawn_keepalive(interval = keepalive_secs)`: mỗi chu kỳ, lease không chạy và nhàn rỗi ≥ interval → `Lease::ping()`. `lease_kit`: ping keepalive **không** gia hạn `idle_ttl` (thêm cờ nội bộ `Activity::Keepalive`); driver tự hiện thực (MSSQL/Mongo/Redis/Cassandra/ClickHouse) đảm bảo cùng ngữ nghĩa trong vùng của mình (ClickHouse: ping trong `session_id` để server không hết `session_timeout`). Ping lỗi → lease `broken = true`, phát `lease://state`. `keepalive_secs = 0` tắt.
- Xong khi: unit test với lease giả + `tokio::time::pause`: ping đúng chu kỳ, không ping khi đang chạy, `idle_ttl` vẫn đóng lease đúng hạn; IT PG: kill backend của lease từ kết nối khác → lease `broken` trong ≤ 2 chu kỳ.
- UI để lại: không (ô cài đặt ở §2.16).

### 2.14 Tự kết nối lại (CONN-12) (gói E)
- Backend:
  - `SessionManager::spawn_health`: mỗi `keepalive_secs` ping từng session; lỗi `Connection`/`Timeout`/`Tls` → event `Lost`. Nếu `auto_reconnect`: thử lại backoff 1, 2, 4, 8, 16 s (5 lần) → `Reconnecting{attempt}` → `Reconnected` hoặc `GaveUp`.
  - Kết nối lại = gọi lại `Driver::connect` với `config` đã lưu trong `Entry` + secret (keychain nếu đã lưu; nếu không, `Entry` giữ `Option<Secret>` trong bộ nhớ suốt đời session — không serialize, không log; Q9) + SSH tunnel mới; thay `Arc<dyn Session>` trong `Entry`, **giữ `session_id`**.
  - Lease của session cũ: lease `auto_commit` và `transaction == Idle` → tự mở lại trên session mới (giữ `lease_id`, database/schema, auto-commit); lease có transaction đang mở → đánh dấu `broken`, liệt kê trong `leases_broken`, chờ `lease_reopen` (UI hỏi người dùng — đúng yêu cầu "hỏi trước nếu đang có transaction mở"); `transactionLost = true`.
  - `session_reconnect` (thủ công, nhận `secret` khi chưa lưu).
- Xong khi: test với `MockDriver` (ping lỗi theo kịch bản): chuỗi event đúng, `session_id` giữ nguyên, lease idle mở lại, lease có transaction không tự mở; IT PG: `pg_terminate_backend` mọi kết nối của session → `Reconnected` và `table_fetch` chạy được.
- UI để lại: banner "mất kết nối / đang kết nối lại", hộp thoại mở lại lease có transaction, nhập mật khẩu khi cần.

### 2.15 Nhãn môi trường tuỳ chỉnh (CONN-03) (gói E)
- Backend: §1.10 + migration v7; validate ≤ 32 ký tự, trim, rỗng → `None` (`Invalid` nếu vượt); export/import profile (`connections_export/import`) mang theo field. Quy tắc an toàn vẫn theo `environment`.
- Xong khi: test store khứ hồi + test command `connection_save` từ chối nhãn 33 ký tự.
- UI để lại: ô nhãn + màu trong dialog kết nối; hiển thị ở cây/tab/status bar.

### 2.16 Settings backend (APP-05) (gói E)
- Backend: §1.10; `settings_set` validate khoảng → `Invalid`; `query_timeout_ms`/`editor_max_rows` đã được command editor đọc (kiểm lại và thêm test nếu thiếu); `editor_font_*` chỉ lưu (frontend áp).
- Xong khi: test `from_value_lenient` với JSON cũ (thiếu field mới) và giá trị hỏng; test command.
- UI để lại: trang Cài đặt — font editor, cỡ chữ, `editorMaxRows`, `queryTimeoutMs`, `historyEnabled`, `autoReconnect`, `keepaliveSecs`, lịch sử plan, ngôn ngữ (đồng bộ với i18n), hành vi đóng cửa sổ + autostart (chuyển từ tab Backup).

### 2.17 Đổi database/schema của lease (gói N1 cho PG; driver khác đã có)
- Backend PG (`postgres/src/lease.rs`): `set_context{database}` khác DB hiện tại → nếu có transaction mở hoặc kết quả treo → `Busy`; ngược lại mở kết nối mới tới DB đích (cùng TLS/SSH/read-only, cùng `search_path` nếu đặt), đóng kết nối cũ, cập nhật `LeaseState.database`. Cockroach: `SET database = x` trên cùng kết nối (hỗ trợ sẵn). YB: như PG.
- Xong khi: IT PG16: lease đổi DB rồi `SELECT current_database()`; đổi khi transaction mở → `busy`.
- UI để lại: không (bộ chọn DB của tab đã gọi `lease_set_context`).

### 2.18 Đổi tên thư mục saved query (gói E)
- Backend: **đã có** — `saved_folder_save` là upsert theo `id`. Chỉ thêm test đổi tên + kiểm tên trùng trong cùng thư mục cha (nếu trùng → `Invalid`).
- UI để lại: thao tác đổi tên trong panel saved queries.

### 2.19 Sinh SELECT/INSERT mẫu & mở DDL (BROWSE-05/07) (gói S + E)
- Backend: `dbstudio_sql::template::generate(dialect, details, path, template)` thuần: SELECT liệt kê cột + giới hạn theo dialect (`LIMIT 100` / `TOP 100` / `FETCH FIRST`; CQL `LIMIT 100`; ClickHouse `LIMIT 100`), INSERT/UPDATE/DELETE dùng tham số `:col` khi dialect có `query_params`, khoá theo PK/unique; Mongo: `db.c.find({}).limit(100)`, `insertOne({...})` theo field suy luận. Command `script_generate` lấy `table_details` (hoặc `schema_model` cho Mongo) rồi gọi hàm. "Mở DDL" dùng `object_ddl` có sẵn.
- Xong khi: unit test snapshot cho 8 dialect × 4 mẫu (quoting tên đặc biệt); test command với `MockSession`.
- UI để lại: mục menu ngữ cảnh "Sinh script" / "Mở DDL trong editor".

### 2.20 Lịch sử plan (PLAN-07) (gói E)
- Backend: `plan_explain` thành công + `plan_history_enabled` → ghi `plan_history` (statement đã redact, `raw_plan` ≤ 8 MiB), cắt bớt theo `plan_history_max_per_connection`. Command §1.12.
- Xong khi: test store (ghi, cắt, xoá theo connection cascade) + test command `plan_explain` → `plan_history_list` thấy bản ghi.
- UI để lại: panel lịch sử plan, mở lại thành plan tab.

### 2.21 ERD comment + quan hệ suy luận (ERD-07/08)
- Backend: **đã có** — `SchemaModelRequest.include_comments`, `TableModel.comment`, `ColumnDetail.comment`, `Relation.inferred` (Mongo `infer_relations`). Driver mới (C, H, N) phải điền comment khi `include_comments`.
- UI để lại: công tắc "hiện comment" trên ERD, nét đứt cho quan hệ `inferred`.

### 2.22 Backup logical MongoDB (EJSON) + Redis (gói B; driver gói K)
- Mongo: engine `logical-mongo`, preamble `formatVersion: 3`, `dialect: "mongo"`. Mỗi collection: `schema/<db>/<coll>.json` (options, validator, collation, index specs từ `listIndexes`, view pipeline) + `data/<db>/<coll>.ndjson` (canonical EJSON, stream qua `Lease::stream_table` trong snapshot transaction khi replica set; standalone ghi cảnh báo "không snapshot"). Restore: tạo collection (options/validator) → `bulk_insert` `$document` lô 1000 → tạo index → view. `clean` = drop collection trước. Partial restore theo tên collection.
- Redis: engine `logical-redis`: `data/db<N>.ndjson` là `KeyRecord` (đọc như §2.3). Restore: `KeyChange::Put` lô 200, `PutMode::Replace` khi `clean` (FLUSHDB trước, cần xác nhận) hoặc `IfAbsent`; khoá hết hạn bỏ qua. Không có snapshot (ghi rõ: dữ liệu ghi trong lúc dump có thể lẫn).
- Xong khi: IT `mongo7` (replica set) + `mongo44`: backup → restore DB trống → số document, index, validator, kiểu BSON khớp; IT `redis7`/`valkey8`: backup → FLUSHDB → restore → khớp + TTL.
- UI để lại: chọn engine logical cho Mongo/Redis trong form job (đã ẩn theo `logical_backup`).

### 2.23 Test backup/restore MSSQL/Mongo/Redis (gói B, hỗ trợ M/K)
- MSSQL logical: IT `mssql2022` backup → restore DB mới (identity, schema, FK PostData, `money`, `datetimeoffset`); native `MsSqlServerBackup` vào thư mục server (`/var/opt/mssql/backup`). Mongo native: `mongodump`/`mongorestore` qua shim trong container (như PG/MySQL). Redis: `redis-cli --rdb` backup (restore RDB vẫn ngoài phạm vi).
- Xong khi: các IT trên chạy xanh cục bộ; lỗi phát sinh trong driver do M/K sửa trong vùng của họ.

### 2.24 Restore một phần backup logical (gói X + B)
- X: SQL writer chèn **dòng đánh dấu** trước mỗi khối đối tượng: `-- dbstudio:object kind=table schema="public" name="t" phase=pre|data|post` (comment `--` hợp lệ ở mọi dialect SQL, CQL, ClickHouse). Preamble logical `formatVersion: 3` liệt kê `objects: [{kind, schema, name, phases, rows}]`.
- B: `restore_start{objects}` với v3: splitter đọc dấu, chỉ chạy khối của đối tượng được chọn; câu PostData tham chiếu đối tượng không chọn (FK) bị bỏ + ghi `warnings`. v2 → `Unsupported` như hiện tại. `backup_inspect` trả `objects`.
- Xong khi: IT PG/MySQL: backup 3 bảng có FK → restore 1 bảng → chỉ bảng đó tồn tại, cảnh báo FK; Mongo (theo collection).
- UI để lại: chọn đối tượng khi restore.

### 2.25 R6 — kiểm tên host TLS qua SSH (gói K cho Mongo/Redis/SSH, N2 cho MySQL, C, H)
Đánh giá khả thi (đã đọc mã nguồn client):

| DB | Cơ chế | Kết luận |
|---|---|---|
| MySQL (`mysql_async` 0.37.1) | `SslOpts::with_danger_tls_hostname_override(Some(server_name))` (opts/mod.rs:307) | **Khả thi, đơn giản**: đặt `tls.server_name` (mặc định = host gốc khi có SSH). |
| Redis (`redis` 1.8.1) | `AsyncConnectionConfig::set_dns_resolver(impl AsyncDNSResolver)`: kết nối `ConnectionAddr::TcpTls { host: <host gốc> }`, resolver trả `127.0.0.1:<cổng tunnel>` cho host đó | **Khả thi**: SNI + kiểm tên = host gốc, TCP vào tunnel. |
| MongoDB (`mongodb` 3.10.1) | Không có hook ServerName/resolver A-record. Dùng feature **`socks5-proxy`**: `dbstudio-ssh` mở listener SOCKS5 cục bộ (127.0.0.1, không auth, chỉ cho phép đích của profile) chuyển qua kênh `direct-tcpip`; driver đặt `proxy_host/port` | **Khả thi, công vừa**; lợi thêm: replica set nhiều host qua SSH (hiện chỉ `directConnection`). |
| Cassandra/Scylla (`scylla` 1.10) | Driver dùng `ServerName::IpAddress(node_ip)` (connection.rs:1600) → `ServerCertVerifier` tuỳ chỉnh bọc `WebPkiServerVerifier`, bỏ tên driver đưa, kiểm theo `tls.server_name`/host gốc | **Khả thi** (áp cho mọi kết nối, kể cả không SSH). |
| ClickHouse (HTTP) | URI dùng host gốc + cổng tunnel; connector hyper-util với resolver map host gốc → 127.0.0.1; SNI = host gốc | **Khả thi**. |

- Xong khi: IT qua `sshd` với `verifyFull`: MySQL `mysql8`, Redis `redis7`, Mongo `mongo7` (TLS), Scylla (TLS), ClickHouse (HTTPS) kết nối thành công; cert sai tên → `tls`.
- UI để lại: bỏ cảnh báo R6 trong dialog kết nối.

### 2.26 MySQL `ANSI_QUOTES` (gói S + N2)
- S: lexer `MySql { ansi_quotes: true }` coi `"x"` là `QuotedIdent`; `mysql_dialect_from_sql_mode` nhận `ANSI_QUOTES` và chế độ gộp `ANSI`; `Dialect::quote_ident` vẫn dùng backtick (đúng ở cả hai chế độ). Guard: script chứa `SET … sql_mode` mà ranh giới câu khác nhau giữa hai chế độ → từ chối (giống luật `NO_BACKSLASH_ESCAPES`).
- N2: lease/session MySQL đọc `@@sql_mode` khi mở và sau `refresh_context` → dialect đúng.
- Xong khi: unit test lexer/split/classify với `"` hai chế độ; IT MySQL8: `SET SESSION sql_mode='ANSI_QUOTES'` → `script_analyze` trả `ansiQuotes: true`.

### 2.27 MSSQL đọc LOB của query tự do theo giới hạn (gói M)
- Backend: bản vá thứ 5 trong `vendor/tiberius-patch` (đánh dấu `DBStudio patch`): giới hạn byte cho cột PLP (`varchar(max)`, `nvarchar(max)`, `varbinary(max)`, `xml`) đặt theo request (`Client::set_plp_limit(Option<usize>)`); decoder đọc tối đa N byte, bỏ phần còn lại trên dây, báo `QueryItem::Truncated { column, total_bytes }`. Driver dùng `max_lob_bytes` (setting) cho query tự do; mỗi cột bị cắt → `on_message("column X truncated at N bytes")`. **Khả thi**: PLP vốn đọc theo chunk trong `column_data/{string,binary}.rs`.
- Xong khi: IT `mssql2022`: `SELECT REPLICATE(CAST('a' AS varchar(max)), 50e6)` với giới hạn 1 MiB → ô 1 MiB + thông báo, kết nối dùng tiếp được; RSS không tăng > 64 MiB.

### 2.28 Test nhỏ
- `tunnel_reaches_mssql_when_it_runs` (gói K, `dbstudio-ssh/tests`): probe TCP `127.0.0.1:$DBSTUDIO_IT_MSSQL_PORT` (1 s); không mở → in "skip" và return, trừ khi `DBSTUDIO_IT_STRICT=1`.
- `it_erd` MySQL (gói N2): dùng guard `Drop` dọn database như commit `96234d3` (kể cả khi test fail).

### 2.29 Thông báo OS theo ngôn ngữ (gói B)
- Backend: bảng chuỗi vi/en trong `src-tauri` (module thông báo của scheduler/tray, không dùng file i18n frontend); ngôn ngữ = `AppSettings.language`, `System` → `sys-locale` (`vi*` → vi, còn lại en). CLI dùng cùng bảng. Nội dung: tên job, thành công/thất bại, thời lượng, lỗi rút gọn.
- Xong khi: unit test chọn chuỗi theo `language` và locale giả lập; test scheduler với `FakeHost` ghi nhận nội dung thông báo tiếng Việt.
- UI để lại: trang Cài đặt ghi `language` khi người dùng đổi ngôn ngữ (§2.16).

---

## 3. Driver mới (mục B)

### 3.1 Cassandra + ScyllaDB — `crates/drivers/cassandra` (gói C)

**3.1.1 Crate**: `scylla = "1.10.0"` (MSRV 1.94), feature `rustls-023`; không bật `chrono-04`/`bigdecimal-04` (tự chuyển `CqlTimestamp`, `CqlDecimal`, `CqlVarint`). Một `Session` scylla = cluster metadata + pool mọi node; bọc trong `CassSession`.

**3.1.2 Ánh xạ kiểu → `Value`**

| CQL | `Value` |
|---|---|
| `ascii`, `text`/`varchar` | `Text` |
| `boolean` | `Bool` |
| `tinyint`, `smallint`, `int`, `bigint`, `counter` | `Int` |
| `varint` | `Decimal` (chuỗi) |
| `float`, `double` | `Float` |
| `decimal` | `Decimal` |
| `blob` | `Bytes` |
| `uuid`, `timeuuid` | `Uuid` |
| `timestamp` | `Timestamp` (UTC, ms) |
| `date` / `time` | `Date` / `Time` (ns) |
| `duration` | `Ext("duration", "1mo2d3ns")` |
| `inet` | `Text` |
| `list`, `set`, `vector` (C* 5) | `Array` |
| `map<text\|ascii, …>` | `Document` |
| `map<khác, …>` | `Array` các cặp `[k, v]` (writer dùng `ColumnMeta.data_type` để sinh `{k: v}`) |
| `tuple` | `Array` |
| UDT | `Document` (giữ thứ tự field) |
| giá trị "empty" (int rỗng) | `Ext("empty", "")` |

**3.1.3 Cây đối tượng**: `Database` = keyspace (keyspace hệ thống `system*` gom vào folder `system`, ẩn mặc định qua option `cql.showSystem`) → folder `tables` (→ `columns`, `indexes`), `materializedViews`, `types` (UDT, `ObjectKind::Type`), `functions`, `aggregates` (`ObjectKind::Function`), `triggers` (chỉ Cassandra). Nguồn: `system_schema.{keyspaces,tables,columns,views,types,functions,aggregates,indexes,triggers}`; Scylla thêm `system_schema.scylla_tables`. `search_objects` lọc phía client trên các bảng này (cache 60 s).

**3.1.4 Query/stream/cancel**: tách câu bằng `dbstudio_sql::split(Cql)` (`BEGIN [UNLOGGED|COUNTER] BATCH … APPLY BATCH` là một câu). Mỗi câu chạy `query_single_page` (unprepared) với `page_size = batch_size`; giữ `PagingState` cho `fetch_more`/kết quả treo. Lệnh kiểu cqlsh xử lý trong driver: `CONSISTENCY <level>`, `SERIAL CONSISTENCY`, `TRACING ON|OFF` (class `Session`); `DESCRIBE …` gửi server (server-side DESCRIBE, Cassandra ≥ 4.0/Scylla ≥ 5.2; cũ hơn → `Unsupported`); `COPY`/`SOURCE` → `Unsupported` kèm gợi ý dùng Export. Kết quả: SELECT → Rows; ghi → `Done` (CQL không trả số dòng); LWT → Rows có `[applied]`. Cancel: CQL không có lệnh huỷ phía server → huỷ future, ngừng phân trang (`cancel_query` = phía client, ghi rõ); `timeout_ms` → `set_request_timeout`.
`fetch_table`: không có OFFSET → **cache paging state** theo `(bảng, filter, offset)` (LRU 64): lưới đọc tuần tự nên offset N luôn có state; nhảy cóc → đọc bỏ qua từ đầu tới tối đa 100 000 dòng, quá thì `Unsupported("offset")`. Sort → `Unsupported("table_sort")` (cap false). Filter trên cột không thuộc khoá → tự thêm `ALLOW FILTERING` + `on_message` cảnh báo quét. `count_table` → `Unsupported("exact_count")`.

**3.1.5 Lease**: không transaction (`begin` → `Unsupported`). Lease = `{ keyspace, consistency, serial_consistency, tracing, pending: Option<PagingState> }` chạy trên **pool scylla Session theo keyspace** (map `keyspace → Arc<scylla::Session>` LRU tối đa 8, tạo lười bằng `use_keyspace`) vì `USE` của scylla áp cho cả session. Câu `USE ks` bị driver chặn và chuyển thành `set_context`. `Busy`/kết quả treo theo hợp đồng §1.6.3 của GĐ2.

**3.1.6 Editability**: khoá = partition + clustering columns (luôn có); `read_only_columns` = khoá; không sửa được: bảng counter, MV (`NotATable`), virtual table (`system_views`). Mặc định dùng LWT: Insert `IF NOT EXISTS`, Update/Delete `… WHERE <khoá> IF EXISTS`; `[applied] = false` → `ChangeFailed`; `notes = [ConditionalWrites]`. Option profile `cql.lwtEdits = false` → câu thường, `notes = [UpsertSemantics]`. `atomic = false`. Preview = CQL.

**3.1.7 Schema model / DDL**: `schema_model` = bảng + cột + PK (partition + clustering), không relation, comment từ option `comment`. `generate_ddl`/`object_ddl`: ưu tiên server-side `DESCRIBE KEYSPACE ks` (cột `create_statement`, thứ tự server); fallback tự sinh từ `system_schema` (keyspace → UDT theo phụ thuộc lồng → table `WITH CLUSTERING ORDER BY` + option → index → MV/function/aggregate ở PostData). `DdlLanguage::Cql`.

**3.1.8 Plan**: `explain` → `Unsupported`. `explain_analyze` (Q4) = chạy **chỉ câu SELECT** với `set_tracing(true)`, lấy `get_tracing_info(id)` → `RawPlan { format: CqlTrace }`; câu ghi → `Unsupported` (`explain_analyze_writes=false`). Parser `cql_trace.rs`: gốc = request (duration), con = sự kiện theo thời gian (`activity`, `source`, `source_elapsed`), cảnh báo khi có "tombstone" hoặc "read N live rows and M tombstone".

**3.1.9 Export/bulk/backup**: `stream_table` = `SELECT … FROM ks.t` phân trang (thứ tự token, `order_by_key` bỏ qua). `bulk_insert`: prepared INSERT, 64 request đồng thời; `OnConflict::Update` = INSERT thường (upsert), `Error`/`Skip` = `IF NOT EXISTS` (LWT, chậm, cảnh báo). Backup: chỉ **logical tích hợp** (DESCRIBE + `INSERT` CQL từ SQL writer dialect `Cql`, một dòng/câu, không transaction). `nodetool snapshot` cần truy cập file trên node → **ngoài phạm vi** (§8).

**3.1.10 TLS/auth/SSH**: `PasswordAuthenticator` (`SessionBuilder::user`); TLS rustls với verifier §2.25 (CA, mTLS từ PEM). SSH: `AddressTranslator` map mọi node → endpoint tunnel + `HostFilter` chỉ giữ node liên hệ + `disallow_shard_aware_port(true)` → **chỉ một node** khi qua SSH (cảnh báo trong `on_message` lúc mở). Option profile: `cql.localDc` (load balancing), `cql.consistency` (mặc định `LOCAL_ONE`, như driver).

**3.1.11 IT**: `scylla` (2026.1) và `cassandra5` (5.0.9) mặc định, `cassandra41`/`scylla62` profile `extra` (§5). Kịch bản: kết nối (mật khẩu, TLS trên Scylla), cây, mọi kiểu §3.1.2 khứ hồi (gồm UDT lồng, map khoá int, vector), query + phân trang `fetch_more`, timeout, LWT edit + xung đột, DDL chạy lại vào keyspace trống, export → import, backup logical → restore, tracing plan parse. Keyspace test Scylla tạo với `tablets = {'enabled': false}` (LWT/counter hạn chế trên tablets — R14).

### 3.2 ClickHouse — `crates/drivers/clickhouse` (gói H)

**3.2.1 Crate & giao thức**: `clickhouse = "0.15.3"` (`lz4`, `rustls-tls-ring`), client tạo bằng `Client::with_http_client(...)` dùng `hyper-util` + `hyper-rustls` với `rustls::ClientConfig` của mình (CA, mTLS, verifier). Cổng 8123 (HTTP) / 8443 (HTTPS). Đã kiểm: `Query::fetch_bytes(format) -> BytesCursor`, `with_setting(name, value)`, `with_option`. Mỗi câu một request HTTP; `query_id` = `QueryRequest.query_id`.

**3.2.2 Ánh xạ kiểu** (decoder `rowbinary.rs` trên `RowBinaryWithNamesAndTypes`, setting `output_format_binary_write_json_as_string=1`):

| ClickHouse | `Value` |
|---|---|
| `Int8..Int64`, `UInt8..UInt32` | `Int` |
| `UInt64` | `UInt` |
| `Int128/256`, `UInt128/256`, `Decimal*(P,S)` | `Decimal` (chuỗi chính xác) |
| `Float32/64`, `BFloat16` | `Float` |
| `Bool` | `Bool` |
| `String`, `FixedString(n)` | `Text` nếu UTF-8, ngược lại `Bytes` |
| `UUID` | `Uuid` |
| `Date`, `Date32` | `Date` |
| `DateTime[(tz)]`, `DateTime64(p[,tz])` | `Timestamp` (UTC ISO, độ chính xác p) |
| `Enum8/16` | `Text` |
| `IPv4/IPv6` | `Text` |
| `Nullable(T)`, `LowCardinality(T)` | theo `T` / `Null` |
| `Array(T)`, `Tuple(...)`, `Nested` | `Array` (tuple có tên → `Document`) |
| `Map(String, T)` | `Document`; `Map(khác, T)` → `Array` cặp |
| `JSON` (as string) | `Json` |
| `Variant`, `Dynamic` | theo kiểu con (discriminator) |
| `Point/Ring/Polygon/MultiPolygon` | `Array` |
| `AggregateFunction`, kiểu chưa biết | không giải mã được → nếu câu `Read`: chạy lại với `JSONCompactEachRowWithNamesAndTypes` (+ `output_format_json_quote_64bit_integers=1`, `output_format_json_quote_decimals=1`); câu khác → `Unsupported` kèm gợi ý `CAST` |

Lỗi giữa luồng (HTTP 200 đã gửi rồi server ném exception): decoder gặp dữ liệu không hợp lệ → đọc phần còn lại như text, tìm `Code: NNN. DB::Exception` → `Error::Query` (R13).

**3.2.3 Cây**: `Database` (ẩn `system`, `INFORMATION_SCHEMA`, `information_schema` mặc định) → `tables` (engine hiển thị trong chi tiết; → `columns`, `indexes` = data skipping index), `views`, `materializedViews`, `dictionaries` (`ObjectKind::Dictionary`). Nguồn `system.{databases,tables,columns,data_skipping_indices,dictionaries}`. `search_objects` = `system.tables`/`system.columns` `ILIKE`.

**3.2.4 Query/stream/cancel**: tách câu `dbstudio_sql::split(ClickHouse)`; `max_rows` → đọc đến giới hạn rồi bỏ stream (kết quả treo chỉ giữ ở lease bằng cách giữ stream mở — tối đa 10 phút, R4 GĐ2). Timeout → setting `max_execution_time`. Cancel: request riêng `KILL QUERY WHERE query_id = '<id>' ASYNC` + drop stream. Read-only profile → setting `readonly=2` mỗi request (server từ chối ghi, `read_only_session = true`). Mã lỗi: 516 → `Authentication`, 497 → `Permission`, 159 → `Timeout`, 394 → `Cancelled`, 164 → `ReadOnlyViolation`, còn lại `Query` (vị trí từ "at position N").

**3.2.5 Lease**: HTTP session (`session_id` = lease id, `session_timeout = 3600`, keepalive §2.13) để `SET`, bảng tạm, `USE` (driver chặn `USE x` → `set_context`, truyền `database` mỗi request) được giữ. Không transaction (`begin` → `Unsupported`; transaction thử nghiệm của ClickHouse không dùng).

**3.2.6 Editability** (thận trọng, Q6): chỉ engine họ `*MergeTree` (không `Replacing/Collapsing/Aggregating/Summing` → chỉ Insert), không phải view/dictionary/Distributed/Log/Memory. `key_columns` = cột sorting key; `read_only_columns` = sorting key + partition key + `MATERIALIZED`/`ALIAS`. Áp: trước mỗi Update/Delete `SELECT count() WHERE <khoá>` phải = 1, khác → `ChangeFailed("key matches N rows")`. Insert = `INSERT VALUES`; Delete = lightweight `DELETE FROM … WHERE` (≥ 23.3) hoặc `ALTER TABLE … DELETE … SETTINGS mutations_sync = 2`; Update = `ALTER TABLE … UPDATE … SETTINGS mutations_sync = 2`. `notes = [AsyncMutation]`, `atomic = false`. Option profile `clickhouse.editing = "insertOnly"` để tắt Update/Delete.

**3.2.7 Plan**: `EXPLAIN json = 1, indexes = 1, actions = 1 PLAN <stmt>` → `ClickHouseJson`; kèm `EXPLAIN ESTIMATE` (rows/parts/marks) và `EXPLAIN PIPELINE` vào `extra`. Parser: node từ `"Node Type"`/`"Description"`/`"Plans"`; chỉ số từ `"Indexes"` (`Initial/Selected Parts/Granules`) → props; cảnh báo "full scan" khi `Selected Granules == Initial Granules` và rows ước tính lớn. `explain_analyze` = false (ClickHouse không có thống kê thực tế trong EXPLAIN).

**3.2.8 Schema model / DDL**: `schema_model`: bảng + cột + comment; `primary_key = []` (khoá của ClickHouse không duy nhất; sorting key ghi vào chi tiết bảng), không relation. `generate_ddl`/`object_ddl`: `SHOW CREATE DATABASE|TABLE|VIEW|DICTIONARY`; thứ tự: database → bảng → dictionary (sau nguồn) → view/MV ở PostData (MV `TO` sau bảng đích).

**3.2.9 Export/bulk/backup**: `stream_table` = `SELECT` RowBinary (`ORDER BY <sorting key>` khi `order_by_key`). `bulk_insert`: POST `INSERT INTO db.t (cols) FORMAT RowBinary` (encoder theo kiểu cột lấy từ `system.columns`), lô 100 000 dòng; `OnConflict::Error` = insert thường; `Skip`/`Update` → `Unsupported` (không có ràng buộc duy nhất). Backup: logical tích hợp (SHOW CREATE + `INSERT … VALUES` nhiều dòng từ SQL writer `ClickHouse`, preamble `SET session_timezone = 'UTC'` khi ≥ 23.6) + native `ClickHouseServerBackup` = `BACKUP DATABASE db TO Disk('backups', '<file>.zip')` / `RESTORE …` (file nằm phía server, cần disk `backups` cấu hình — kiểm `system.disks`, không có → `problem`).

**3.2.10 TLS/auth/SSH**: user/password (header của crate); HTTPS với CA/mTLS; SSH theo §2.25. Option `clickhouse.compression` (lz4 mặc định).

**3.2.11 IT**: `clickhouse26` (26.3 LTS, HTTP + HTTPS), `clickhouse23` (23.8, profile `extra`, kiểm phiên bản tối thiểu). Kịch bản: mọi kiểu §3.2.2 khứ hồi (bảng fixture chứa đủ kiểu), query lớn huỷ < 2 s (`SELECT … FROM numbers(1e12)`), read-only chặn INSERT ở server, sửa dữ liệu MergeTree (count = 1, ≠ 1), DDL chạy lại, export → import RowBinary, backup logical → restore, explain parse.

### 3.3 NewSQL (gói N1 PG-flavor, N2 MySQL-flavor)

Chung: driver nhận diện flavor sau khi kết nối (`version()`), lưu `Flavor` trong session; `DbKind` người dùng chọn chỉ quyết định mặc định (cổng, tên). Lệch kind/flavor → dùng flavor của server + `on_message`/`ServerInfo.extra["flavor"]`. Capabilities trả qua `Session::capabilities()`.

**3.3.1 CockroachDB** (N1; `version()` chứa `CockroachDB`):
- Kết nối `tokio-postgres` như PG; cổng 26257; option profile `cockroach.cluster` → tham số startup `options=--cluster=<id>` (Cockroach Cloud). Lỗi 40001 → `Query` kèm gợi ý "retry transaction".
- Catalog: dùng `information_schema` + `pg_catalog` (đã có nhưng thiếu vài view) + `crdb_internal` khi cần; mỗi truy vấn catalog của PG chạy thử trong IT, chỗ không chạy có nhánh flavor. Bảng không PK có cột ẩn `rowid` → dùng làm khoá sửa (`NOT VISIBLE`).
- Cancel: thử cancel request pgwire; IT chứng minh, nếu không được dùng `CANCEL QUERY (SELECT query_id FROM [SHOW QUERIES] WHERE …)` qua kết nối khác.
- EXPLAIN: `EXPLAIN (VERBOSE)` / `EXPLAIN ANALYZE (VERBOSE)` → `CockroachText`; parser `cockroach.rs` đọc cây `•`/`│`/`└──`, thuộc tính `estimated row count`, `actual row count`, `execution time`, `spans: FULL SCAN` (cảnh báo full scan), `KV rows decoded`. EXPLAIN ANALYZE câu ghi trong transaction rollback như PG.
- DDL: `SHOW CREATE ALL TABLES` (FK ở cuối dạng `ALTER TABLE … ADD CONSTRAINT` → PostData) / `SHOW CREATE <obj>`.
- Đổi database trong lease: `SET database = x` (không cần kết nối mới).
- Backup native: `CockroachServerBackup` = `BACKUP DATABASE db INTO 'nodelocal://1/dbstudio/<job>'` / `RESTORE DATABASE … FROM LATEST IN …` (file phía server, cần quyền admin). `pg_dump` không hỗ trợ Cockroach → không dùng.

**3.3.2 YugabyteDB YSQL** (N1; `version()` chứa `-YB-`):
- Như PG (YB 2024.x nền PG 11, 2025.1+ nền PG 15); cổng 5433, user mặc định `yugabyte`. EXPLAIN JSON PG (+ `DIST` khi ANALYZE → props `Storage Read Requests`…). Catalog chậm hơn → timeout catalog nới 2×.
- Backup native: `YsqlDump` (`ysql_dump`, đi kèm YB) → plain SQL, restore bằng `Ysqlsh`; `pg_dump` không đảm bảo tương thích → không tự chọn.

**3.3.3 TiDB** (N2; `version()` chứa `TiDB`):
- `mysql_async` như MySQL; cổng 4000. Không procedure/trigger/event → ẩn folder; có `SEQUENCE`; FK (≥ 6.6).
- Cancel: `KILL QUERY <id>` (global kill bật mặc định từ v6.1) — IT chứng minh.
- Read-only: chưa chắc `SET SESSION TRANSACTION READ ONLY` được thực thi → `read_only_session = false` (chỉ guard của app) cho tới khi IT xác nhận.
- Lexer: comment `/*T![feature] … */` coi như code (giống `/*! */`); classify `BATCH ON col LIMIT n <DML>` = Write (phá huỷ nếu DML bên trong không WHERE), `FLASHBACK`, `ADMIN`, `SPLIT TABLE` = Admin (gói S).
- EXPLAIN: `EXPLAIN [ANALYZE] FORMAT='row'` → driver gói các dòng thành JSON → `TiDbRows`; parser `tidb.rs` dựng cây từ tiền tố `├─`/`└─`/`│`, cảnh báo `TableFullScan` (estRows lớn), lệch `estRows`/`actRows` ≥ 10×.
- Backup native: `mysqldump` với cờ TiDB (`--single-transaction --skip-routines --skip-triggers --skip-events --no-tablespaces`) nếu IT chạy được; không thì chỉ logical. Dumpling/BR ngoài phạm vi.

**3.3.4 IT**: `cockroach` (v25.4.20, secure với CA của `gen-certs.sh`, password + TLS verify-full), `tidb` (v8.5.7, unistore), `yugabyte` (2025.2.8.1-b8, profile `heavy`). Kịch bản = bộ IT PG/MySQL hiện có chạy lại với flavor (connect, browse, data, edit, lease, explain, model/DDL, export/import, backup logical) + test riêng (rowid, SHOW CREATE ALL TABLES, BATCH ON, global kill).

### 3.4 `dbstudio-sql` cho dialect mới (gói S)

| Thành phần | CQL | ClickHouse |
|---|---|---|
| Lexer | `'…'` (nhân đôi `'`), `$$…$$`, `"ident"`, comment `--`, `//`, `/* */`; số, UUID literal, blob `0x…` | `'…'` có `\` escape, `"ident"` và `` `ident` ``, comment `--`, `#`, `/* */`; heredoc `$tag$…$tag$` |
| Split | `;`; `BEGIN … APPLY BATCH` là một khối | `;` |
| Classify | Read: SELECT, DESCRIBE/DESC, LIST; Write: INSERT/UPDATE/DELETE/BATCH; Ddl: CREATE/ALTER/DROP/TRUNCATE; Session: USE, CONSISTENCY, SERIAL CONSISTENCY, TRACING; Admin: GRANT/REVOKE/CREATE ROLE/… | Read: SELECT/WITH/SHOW/DESCRIBE/EXISTS/EXPLAIN; Write: INSERT, `DELETE FROM`, `UPDATE` (lightweight), `ALTER TABLE … UPDATE/DELETE` (mutation); Ddl: CREATE/ALTER/DROP/TRUNCATE/RENAME/ATTACH/DETACH/OPTIMIZE; Session: USE/SET; Admin: SYSTEM/KILL/GRANT/BACKUP/RESTORE |
| Phá huỷ | DROP (keyspace/table/…) → `Drop`; TRUNCATE → `Truncate`; `DELETE` không WHERE không hợp lệ trong CQL (không cần) | DROP/`DETACH` → `Drop`; TRUNCATE → `Truncate`; `ALTER … DROP COLUMN/PARTITION` → `AlterDrop`; `ALTER … DELETE WHERE 1`/`DELETE FROM t WHERE 1` → `DeleteWithoutWhere`; `ALTER … UPDATE … WHERE 1` → `UpdateWithoutWhere` |
| Params | `:name`, `?` (thay bằng literal CQL) | `:name`, `?`; `{name:Type}` của ClickHouse được **giữ nguyên** (không bắt) |
| Literal | `Dialect::literal_for_type(&Value, Option<&str>)` mới: map `{'k': v}` vs UDT `{f: v}`, set `{…}`, list `[…]`, tuple `(…)`, blob `0x…`, timestamp `'…Z'` | string có `\`, `Array` `[…]`, `Tuple` `(…)`, `Map` `map('k', v)`, `Decimal` thẳng, DateTime `'YYYY-MM-DD hh:mm:ss[.fff]'` (UTC theo `session_timezone`) |
| Redact, vocabulary | `PASSWORD '…'` trong `CREATE/ALTER ROLE|USER`; từ khoá + hàm CQL | `IDENTIFIED BY '…'`, `PASSWORD`; từ khoá + hàm phổ biến |

Ngoài ra: ANSI_QUOTES (§2.26), từ khoá TiDB/Cockroach (`UPSERT` = Write, `SHOW RANGES`/`BACKUP`/`RESTORE`/`IMPORT INTO`/`CANCEL QUERY` = Admin), `template.rs` (§2.19), guard `KeywordScan`. Mỗi dialect ≥ 80 ca unit test + fuzz không panic.

### 3.5 Export/backup cho DB mới
- SQL writer (X): dialect `Cql` — một `INSERT` một dòng, không wrap transaction, không FK; `ClickHouse` — `INSERT … VALUES` nhiều dòng (1000/lô), không transaction; Cockroach/YB dùng writer PG (bỏ `session_replication_role`), TiDB writer MySQL. Allow-list import thêm `Cql`, `ClickHouse`.
- Backup (B, đợt 2): `logical.rs::dialect_name` thêm `cql`, `clickhouse` (+ flavor PG/MySQL đi theo dialect gốc); `NativeTool` mới hiện thực trong `native.rs` (§1.1). IT backup → restore cho mỗi DB mới.

---

## 4. Plan parser mới (`dbstudio-plan`)

| Module | Gói | Fixture (≥ 5, tự chụp từ container) | Cảnh báo |
|---|---|---|---|
| `clickhouse.rs` | H | SELECT có/không dùng index, JOIN, GROUP BY, MV, `FINAL` | full scan granule, `FINAL` trên bảng lớn |
| `cockroach.rs` | N1 | EXPLAIN + ANALYZE: scan, lookup join, hash join, sort, insert | `FULL SCAN`, lệch ≥ 10×, `distribution: full` |
| `tidb.rs` | N2 | row/analyze: TableFullScan, IndexLookUp, HashJoin, cop task, MPP | `TableFullScan`, lệch ≥ 10×, `disk` > 0 (spill) |
| `cql_trace.rs` | C | SELECT theo PK, quét `ALLOW FILTERING`, tombstone | tombstone, thời gian > 100 ms |

Giữ API `parse(&RawPlan) -> PlanTree` và `analyze` hiện có; totals (cost/rows/time) điền khi có.

---

## 5. Docker và CI

### 5.1 Service mới (`docker/compose.test.yml`)

| Service | Image (đã kiểm tag) | Cổng host → container | Đăng nhập | `mem_limit` / tham số RAM | Profile |
|---|---|---|---|---|---|
| `scylla` | `scylladb/scylla:2026.1` (amd64, arm64) | `19060:9042`, `19062:9142` (TLS) | `cassandra`/`cassandra` → IT tạo `dbstudio`/`dbstudio_test` | `1g`; `--smp 1 --memory 750M --overprovisioned 1 --developer-mode 1 --authenticator PasswordAuthenticator`; TLS PEM từ `docker/certs` qua `docker/scylla/scylla.yaml` | `wide` |
| `cassandra5` | `cassandra:5.0.9` | `19050:9042` | như trên | `1g`; `MAX_HEAP_SIZE=512M HEAP_NEWSIZE=128M`; script `docker/cassandra/enable-auth.sh` bật `PasswordAuthenticator` | `wide` |
| `cassandra41` | `cassandra:4.1.12` | `19041:9042` | như trên | `1g` (như trên) | `extra` |
| `scylla62` | `scylladb/scylla:6.2` (bản AGPL cuối) | `19061:9042` | như trên | `1g` | `extra` |
| `clickhouse26` | `clickhouse/clickhouse-server:26.3` (LTS) | `18123:8123`, `18443:8443` (HTTPS) | `CLICKHOUSE_USER=dbstudio`, `CLICKHOUSE_PASSWORD=dbstudio_test`, `CLICKHOUSE_DB=dbstudio`, `CLICKHOUSE_DEFAULT_ACCESS_MANAGEMENT=1` | `1g`; `config.d`: `max_server_memory_usage` 700 MB, `mark_cache_size` nhỏ, disk `backups`, `https_port` + cert | `analytics` |
| `clickhouse23` | `clickhouse/clickhouse-server:23.8` | `18023:8123` | như trên | `1g` | `extra` |
| `cockroach` | `cockroachdb/cockroach:v25.4.20` | `26258:26257` | `root` (cert) → IT tạo `dbstudio`/`dbstudio_test` | `768m`; `start-single-node --certs-dir=/certs --cache=64MiB --max-sql-memory=128MiB --accept-sql-without-tls` | `newsql` |
| `tidb` | `pingcap/tidb:v8.5.7` | `34000:4000` | `root` (không mật khẩu) → IT tạo `dbstudio` | `768m`; `--store=unistore --path=""` (không cần PD/TiKV) | `newsql` |
| `yugabyte` | `yugabytedb/yugabyte:2025.2.8.1-b8` | `54333:5433` | `yugabyte`/`yugabyte` → IT tạo `dbstudio` | `2g`; `bin/yugabyted start --background=false --tserver_flags="memory_limit_hard_bytes=1073741824,yb_num_shards_per_tserver=1,ysql_num_shards_per_tserver=1"` | `heavy` |

Cổng không trùng các cổng hiện có (54316, 54312, 33068, 33106, 33104, 33057, 14333, 15213, 27044, 27077, 63796–63798, 22022). Biến môi trường IT: `DBSTUDIO_IT_SCYLLA_PORT`, `DBSTUDIO_IT_SCYLLA_TLS_PORT`, `DBSTUDIO_IT_CASSANDRA5_PORT`, `DBSTUDIO_IT_CASSANDRA41_PORT`, `DBSTUDIO_IT_SCYLLA62_PORT`, `DBSTUDIO_IT_CLICKHOUSE_PORT`, `DBSTUDIO_IT_CLICKHOUSE_TLS_PORT`, `DBSTUDIO_IT_CLICKHOUSE23_PORT`, `DBSTUDIO_IT_COCKROACH_PORT`, `DBSTUDIO_IT_TIDB_PORT`, `DBSTUDIO_IT_YUGABYTE_PORT`. Healthcheck: `cqlsh -u cassandra -p cassandra -e 'SELECT now() FROM system.local'` (Cassandra chậm, `start_period` 90 s), `clickhouse-client -q 'SELECT 1'`, `cockroach sql --certs-dir … -e 'SELECT 1'`, `mysql -h127.0.0.1 -P4000 -uroot -e 'SELECT 1'` (nếu image không có client: `wget -qO- localhost:10080/status`), `ysqlsh -c 'SELECT 1'`. `sshd` thấy service mới qua mạng compose (IT R6/SSH).

**Ngân sách RAM cục bộ** (Docker VM ~8 GB dùng chung): mỗi lần chỉ bật một profile: `wide` ≈ 2 GB, `analytics` ≈ 1 GB, `newsql` ≈ 1,5 GB, `heavy` ≈ 2 GB. Không bật cùng MSSQL/Oracle. Ghi vào đầu compose như các service GĐ2.

### 5.2 Chứng chỉ
`docker/gen-certs.sh` sinh thêm: PEM cho Scylla, `server.crt/key` cho ClickHouse (SAN `localhost`, `clickhouse26`), bộ cert Cockroach (`ca.crt`, `node.crt/key` với SAN `localhost,cockroach,127.0.0.1`, `client.root.crt/key`) ký bằng CA hiện có.

### 5.3 CI
Job `integration` tách thành matrix `group ∈ {core (hiện có), wide, analytics, newsql}` (`heavy` và `extra` chạy theo lịch tuần, cho phép lỗi lúc đầu). Mỗi nhóm `docker compose --profile <group> up -d --wait` rồi `cargo test` với `DBSTUDIO_IT=1 DBSTUDIO_IT_STRICT=1` và biến cổng của nhóm. Bước build thêm feature `cassandra`, `clickhouse`.

---

## 6. Kế hoạch chia gói song song

### 6.1 Bước 0 — Hợp đồng v2 (1 agent Sonnet, review Opus)
Nhánh `feature/quandh/backlog-contract` → merge `develop` trước mọi gói. Phạm vi §1.14. Ước lượng: lớn nhất ở sửa chỗ dựng literal (`ScriptDialect::MySql` 92, `Editability` 23, `TableDataRequest` không đổi) và frontend tối thiểu.

### 6.2 Các gói (backend; **không gói nào được tạo/sửa component, màn hình, file i18n hiển thị**)
Mỗi gói một worktree `.claude/worktrees/<goi>`, nhánh `feature/quandh/<goi>` từ `develop` mới nhất. Cột "File" là vùng **độc quyền**.

| Gói | Phạm vi | File độc quyền | Phụ thuộc | Tiêu chí xong (kiểm chứng được) |
|---|---|---|---|---|
| **S. dbstudio-sql** | §2.26 (lexer), §3.4, §2.19 (template), guard `KeywordScan`, từ khoá TiDB/Cockroach | `crates/dbstudio-sql/**`, `src-tauri/src/guard.rs` | Bước 0 | **S1 (ưu tiên)**: CQL + ClickHouse lexer/split/classify/literal_for_type ≥ 80 ca mỗi dialect, fuzz không panic. S2: ANSI_QUOTES, template snapshot 8 dialect × 4 mẫu, `/*T!`, `BATCH ON`, guard test `keywordScan`. Toàn bộ test `dbstudio-sql` hiện có vẫn xanh. |
| **E. Editor/phiên/cài đặt** | §2.8 (plan.rs), 2.9, 2.10, 2.12 (phần session), 2.13, 2.14, 2.15, 2.16, 2.18, 2.19 (command), 2.20; scheme URI mới trong `connection_parse_uri` (`cockroachdb://`, `yugabytedb://`, `tidb://`, `clickhouse[s]://`, `cassandra://`, `scylladb://`) | `src-tauri/src/{session,lease,events,state}.rs`, `src-tauri/src/commands/{sessions,editor,history,app,plan,connections,uri}.rs`, `crates/dbstudio-store/src/{settings,tabs,saved,plan_history,db,model,validate,service}.rs`, `crates/dbstudio-core/src/lease_kit.rs` | Bước 0; S2 cho `script_generate` thật | Test §2.8–2.20 xanh; IT PG: keepalive phát hiện kết nối bị kill, reconnect giữ `sessionId` và lease idle; `cargo test -p dbstudio-store` (migration v7 trên DB v6 thật). |
| **X. Export/Import** | §2.1–2.4 (engine), 2.11, 2.12 (preview), 2.24 (writer), §3.5 writer | `crates/dbstudio-export/**`, `src-tauri/src/commands/{export,files,jobs}.rs`, `src-tauri/src/{jobs,grants}.rs`, `crates/dbstudio-store/src/presets.rs` | Bước 0; K (`scan_keys`, `Put`, `$document`) cho IT NoSQL; S1 cho writer CQL/ClickHouse | IT §2.1–2.4, §2.11 unit; writer CQL/ClickHouse unit snapshot + khứ hồi khi C/H đã merge (đợt 2). |
| **B. Backup** | §2.22, 2.23, 2.24 (restore), 2.29 (thông báo theo ngôn ngữ), §3.5 backup (đợt 2) | `crates/dbstudio-backup/**`, `crates/dbstudio-cli/**`, `src-tauri/src/{scheduler,tray,background}.rs`, `src-tauri/src/commands/{backup,background}.rs`, `crates/dbstudio-store/src/backup.rs` | Bước 0; X (writer markers, `KeyRecord` writer) cho 2.22/2.24; K cho Mongo/Redis | IT §2.22–2.24; thông báo: unit test chọn chuỗi vi/en theo `language` (System → `sys-locale` giả lập); CLI chạy backup Mongo/Redis logical. |
| **K. Mongo/Redis + SSH** | §2.5, 2.6, `scan_keys`, `KeyChange::Put`, `$document` trong Mongo `bulk_insert`, R6 Redis/Mongo (SOCKS5), §2.28 test tunnel | `crates/drivers/{mongodb,redis}/**`, `crates/dbstudio-ssh/**`, `src-tauri/src/commands/keyvalue.rs` | Bước 0 | IT `redis6/7`, `valkey8`, `mongo44/7`: xung đột list/document, `Put` Replace/IfAbsent (khoá lớn 50 MB ghi theo chunk + RENAME), `scan_keys` 1 triệu khoá (`DBSTUDIO_IT_BIG=1`); R6 qua `sshd` với `verifyFull`. |
| **N1. PG flavor** | Cockroach, Yugabyte (§3.3.1–3.3.2), §2.17, `cockroach.rs` | `crates/drivers/postgres/**`, `crates/dbstudio-plan/src/cockroach.rs` (+ fixture) | Bước 0 | Bộ IT PG chạy với `cockroach` (và `yugabyte` khi bật `heavy`) xanh hoặc có nhánh `skip` ghi lý do từng tính năng; IT §2.17; parser fixture. PG12/16 không hồi quy. |
| **N2. MySQL flavor** | TiDB (§3.3.3), R6 MySQL, ANSI_QUOTES phía driver, §2.28 `it_erd`, `tidb.rs` | `crates/drivers/mysql/**`, `crates/dbstudio-plan/src/tidb.rs` (+ fixture) | Bước 0; S2 (ANSI_QUOTES) | Bộ IT MySQL với `tidb` xanh/skip có lý do; R6 MySQL qua `sshd`; MySQL 5.7/8, MariaDB không hồi quy. |
| **C. Cassandra/Scylla** | §3.1 toàn bộ | `crates/drivers/cassandra/**`, `crates/dbstudio-plan/src/cql_trace.rs`, `docker/{cassandra,scylla}/**` | Bước 0, S1 | IT §3.1.11 trên `scylla` + `cassandra5`; mọi method của `Session` trong ma trận requirements §4 có test. |
| **H. ClickHouse** | §3.2 toàn bộ | `crates/drivers/clickhouse/**`, `crates/dbstudio-plan/src/clickhouse.rs`, `docker/clickhouse/**` | Bước 0, S1 | IT §3.2.11 trên `clickhouse26` (+ `clickhouse23` khi bật `extra`). |
| **M. MSSQL** | §2.27; hỗ trợ sửa lỗi driver cho B (§2.23) | `vendor/tiberius-patch/**`, `crates/drivers/mssql/**` | Bước 0 | IT §2.27; `DBSTUDIO-PATCH.md` có mục 5; IT MSSQL cũ xanh. |
| **Z. Dọn & tài liệu** (sau cùng) | Quét `Error::Other`/`Permission` lạm dụng còn lại; cập nhật `architecture.md`, `roadmap.md`, `requirements.md` (cột Trạng thái), §9 "khác biệt khi triển khai" của tài liệu này; CI matrix chạy đủ | phần còn lại | mọi gói | `grep` không còn `Error::Other` cho đầu vào; docs khớp code; CI (khi có remote) hoặc chạy cục bộ đủ nhóm. |

Ghi chú vùng:
- `crates/dbstudio-plan/src/lib.rs` (dispatch) và `Cargo.toml` workspace chỉ Bước 0 sửa; gói chỉ sửa module của mình.
- `docker/compose.test.yml`, `docker/gen-certs.sh`, `.github/workflows/ci.yml`: Bước 0 viết đủ; gói C/H/N chỉ được chỉnh **block service của mình** (ghi rõ trong PR) để sửa tham số khởi động.
- `crates/dbstudio-core/**` (trừ `lease_kit.rs` của E) đóng băng sau Bước 0.
- Đợt 2 (sau khi C, H, N1, N2 merge): X2 (ma trận export/import với DB mới), B2 (backup logical + native cho DB mới, IT), rồi Z.

### 6.3 Thứ tự và đồng bộ

```
Bước 0 (hợp đồng v2) ──► merge develop
   ├─ S (S1 trước) ──┬─► C, H (parser thật)
   │                 └─► X (writer CQL/ClickHouse)       S2 ──► E (script_generate), N2 (ANSI_QUOTES)
   ├─ E, M, N1 (độc lập)
   ├─ K ──► X (IT NoSQL) ──► B (Mongo/Redis logical, partial restore)
   └─ N2
Đợt 2: X2 + B2 trên Cassandra/Scylla/ClickHouse/Cockroach/TiDB/YB ──► Z
```
Thứ tự merge khuyến nghị: Bước 0 → S1 → K → E → M → N1 → N2 → S2 → C → H → X → B → X2/B2 → Z. Mỗi merge chạy `cargo test --workspace`, `cargo clippy --all-features`, `pnpm test` (bảo đảm không vỡ frontend của session UI).

### 6.4 Brief chung cho agent gói
- Thêm vào brief: `export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:$PATH"` (cargo không có sẵn trên PATH máy này).
- Bật service bằng profile, tắt sau khi xong; không bật cùng MSSQL/Oracle.
- Không sửa `src/features/**`, `src/app/**`, `src/locales/**` (trừ khi Bước 0 đã quy định).

---

## 7. Để lại cho UI (tổng hợp)

**Thuần UI trong roadmap (không có phần backend đợt này)**: command palette (APP-08); `DocumentTable` ảo hoá cột; đếm ngược hạn token xác nhận (`expiresInMs` đã có); chọn nhiều dòng trong lưới (copy/xoá hàng loạt — `ChangeSet` đã nhận nhiều thay đổi).

**UI cho phần backend làm đợt này**:

| Tính năng | Việc UI | Dữ liệu/command sẵn có |
|---|---|---|
| Cài đặt (APP-05) | Font, cỡ chữ editor, `editorMaxRows`, `queryTimeoutMs`, `historyEnabled`, `autoReconnect`, `keepaliveSecs`, lịch sử plan, ngôn ngữ (đồng bộ i18n → `settings_set{language}`), hành vi đóng cửa sổ + autostart | `settings_get/set`, `background_get/set` |
| CONN-03 | Ô nhãn môi trường tuỳ chỉnh; hiển thị nhãn + màu ở cây/tab/status bar | `ConnectionConfig.environmentLabel`, `color` |
| CONN-12 | Banner mất/đang kết nối lại, hộp thoại mở lại lease có transaction, nhập mật khẩu | event `session://state`, `session_reconnect`, `lease_reopen` |
| BROWSE-05/07 | Menu "Sinh SELECT/INSERT/UPDATE/DELETE", "Mở DDL trong editor" | `script_generate`, `object_ddl` |
| PLAN-07 | Panel lịch sử plan, mở lại | `plan_history_*` |
| ERD-07/08 | Công tắc comment, nét đứt quan hệ suy luận | `includeComments`, `Relation.inferred` |
| Export | Nguồn Redis keys; chọn trường làm phẳng Mongo | `ExportSource::Keys`, `ValueFormat.flattenFields` |
| Import | Đích Mongo/Redis, `PutMode`, ô `nullText`, nhãn "dialect đoán" | `ImportTarget::{Existing,Keys}`, `ImportFormat::Csv.nullText`, `dialectGuessed` |
| Sửa dữ liệu | Gửi `expected` (list Redis, document Mongo), hiển thị xung đột; hiển thị `Editability.notes` (LWT, upsert, mutation ClickHouse); lưới không tổng số khi `exactCount=false`, ẩn sort khi `tableSort=false` | §1.7–1.8, §1.5 |
| Backup | Engine logical Mongo/Redis; chọn đối tượng khi restore; tool native mới trong form job | `backup_inspect.objects`, `NativeTool` mới |
| Chẩn đoán driver | Danh sách check | `DriverDiagnostics.checks` |
| Driver mới | Kind mới trong dialog kết nối (icon, tên i18n, field: Cassandra `localDc`/`consistency`/`lwtEdits`, ClickHouse HTTPS/nén/`editing`, Cockroach `cluster`), highlight CQL/ClickHouse trong CodeMirror (`sqlDialectOf` fallback `StandardSQL` hiện chạy được), formatter, icon `Dictionary`, folder `dictionaries`/`aggregates`, mẫu dán URI (`cockroachdb://`, `tidb://`, `clickhouse://`, `cassandra://` — parse do E) | descriptor, `ScriptDialect`, `ObjectKind` |
| R6 | Bỏ cảnh báo R6 trong dialog kết nối | — |

---

## 8. Rủi ro & quyết định cần chủ dự án

### 8.1 Cần quyết (kèm mặc định để không bị chặn)

| # | Câu hỏi | Mặc định đề xuất |
|---|---|---|
| Q1 | NewSQL có `DbKind` riêng hay chỉ là flavor ẩn? | `DbKind` riêng (G2) + tự nhận diện khi chọn nhầm. Bước 0 phải sửa 6 dòng trong `kindConfig.ts` (file vùng UI). |
| Q2 | ClickHouse: HTTP chính thức (`clickhouse` 0.15.3) hay native (`klickhouse` 0.16.0)? | HTTP + decoder RowBinary riêng (G4). |
| Q3 | Nâng MSRV workspace 1.85 → 1.94 vì `scylla` 1.10? | Đồng ý. |
| Q4 | Plan Cassandra: không có hay dùng tracing? (brief: Unsupported; requirements §4: "◐ tracing") | `explain` = Unsupported; `explain_analyze` = tracing **chỉ cho SELECT**, làm ở cuối gói C, cắt được nếu trễ. |
| Q5 | Sửa dữ liệu Cassandra dùng LWT mặc định? | Có (`IF EXISTS`/`IF NOT EXISTS`), tắt được theo profile. |
| Q6 | Sửa dữ liệu ClickHouse | Insert + Delete (lightweight/mutation) + Update (mutation `mutations_sync=2`) cho họ MergeTree, kiểm `count() = 1` trước; tắt được (`insertOnly`). |
| Q7 | Image ScyllaDB 2026.x theo giấy phép source-available | Dùng 2026.1 để test (miễn phí cho mức dùng nhỏ); 6.2 (AGPL) ở profile `extra` cho phiên bản tối thiểu. |
| Q8 | Backup native cho DB mới | ClickHouse `BACKUP` (phía server), Cockroach `BACKUP` (phía server), YB `ysql_dump`, TiDB `mysqldump` nếu IT đạt; Cassandra/Scylla chỉ logical (`nodetool snapshot` ngoài phạm vi). |
| Q9 | Giữ mật khẩu trong bộ nhớ để tự kết nối lại khi người dùng không lưu vào keychain? | Có, suốt đời session, không ghi đĩa/log; nếu không đồng ý: kết nối lại thủ công phải nhập lại. |
| Q10 | R6 MongoDB qua SOCKS5 (thêm listener SOCKS cục bộ trong `dbstudio-ssh`) | Đồng ý (listener chỉ 127.0.0.1, chỉ cho phép host/port của profile). |
| Q11 | Bỏ `Copy` của `AppSettings` để thêm `editor_font_family: String` | Đồng ý. |
| Q12 | Phát hiện sửa đồng thời Mongo: so giá trị gốc từng field hay trường phiên bản? | So giá trị gốc (không cần quy ước ứng dụng). |
| Q13 | Restore một phần chỉ cho backup logical mới (`formatVersion` 3)? | Đúng; file v2 chỉ restore toàn bộ. |
| Q14 | Phiên bản test tối thiểu ClickHouse | 26.3 mặc định + 23.8 (`extra`); requirements "23+". |

### 8.2 Rủi ro kỹ thuật

| # | Rủi ro | Hướng xử lý |
|---|---|---|
| R11 | Bước 0 phải chạm vài file frontend (`kindConfig.ts`, mock) trong lúc session UI đang sửa | Thay đổi tối thiểu, merge sớm, báo session UI rebase; mọi thứ khác nằm trong `src/lib/{types,ipc,mock}`. |
| R12 | RAM Docker (~8 GB) không đủ chạy nhiều service | Profile theo nhóm, `mem_limit` (§5.1), probe-rồi-skip (G8), CI matrix. |
| R13 | ClickHouse gửi exception giữa luồng HTTP 200; RowBinary không phủ kiểu lạ | Decoder phát hiện + đọc text lỗi; fallback JSON cho câu Read; bảng fixture đủ kiểu trong IT. |
| R14 | ScyllaDB tablets: LWT/counter/MV hạn chế theo phiên bản | Keyspace test `tablets = {'enabled': false}`; editability kiểm `system_schema.scylla_keyspaces` và tắt LWT khi tablets không hỗ trợ. |
| R15 | Cassandra qua SSH chỉ một node (driver khám phá peer) | AddressTranslator + HostFilter; ghi rõ; khuyến nghị kết nối trực tiếp cho cluster nhiều node. |
| R16 | Cockroach `pg_catalog` thiếu view; cancel pgwire chưa chắc | Nhánh flavor dùng `information_schema`/`crdb_internal`; fallback `CANCEL QUERY`. |
| R17 | YugabyteDB image nặng (≥ 1,5 GB), catalog chậm | Profile `heavy`, cờ giảm shard/bộ nhớ, timeout catalog nới. |
| R18 | TiDB read-only session, mysqldump không tương thích hoàn toàn | `read_only_session=false` tới khi IT xác nhận; native chỉ bật nếu IT đạt. |
| R19 | `scylla` 1.x thay API giữa minor | Khoá `=1.10` trong Cargo.toml của driver, nâng có chủ đích. |
| R20 | Tự kết nối lại khi transaction mở có thể làm người dùng tưởng dữ liệu đã commit | Không tự mở lại lease có transaction; event nêu `leasesBroken` + `transactionLost`. |
| R21 | Redis `Put` khoá rất lớn tốn bộ nhớ server khi dựng khoá tạm | Ghi theo chunk vào khoá tạm rồi `RENAME`; cảnh báo khi khoá > 512 MB; khoá tạm có TTL 1 giờ để tự dọn nếu lỗi. |
| R22 | Nhiều gói cùng sửa `dbstudio-plan` | Mỗi gói một module mới; `lib.rs` chỉ Bước 0 sửa. |

---

## 9. Khác biệt khi triển khai

> Gói Z điền sau khi hiện thực (giống `phase2-8.md` §9).

---

## Phụ lục — phiên bản đã kiểm (2026-11-14)

| Thành phần | Phiên bản | Ghi chú |
|---|---|---|
| `scylla` | 1.10.0 | MSRV 1.94; feature `rustls-023`; có `AddressTranslator`, `HostFilter`, `disallow_shard_aware_port`, `get_tracing_info`, `query_single_page` |
| `clickhouse` | 0.15.3 | MSRV 1.89; `fetch_bytes`, `with_http_client`, feature `rustls-tls-ring`, `lz4` |
| `klickhouse` (không chọn) | 0.16.0 | native protocol |
| `cdrs-tokio` (không chọn) | 10.0.0 | |
| `hyper-util` / `hyper-rustls` | 0.1.21 / 0.27.10 | client HTTP tuỳ chỉnh cho ClickHouse |
| `sys-locale` | 0.3.2 | ngôn ngữ OS cho thông báo |
| `fast-socks5` | 1.0.0 | listener SOCKS5 trong `dbstudio-ssh`; `mongodb` feature `socks5-proxy` dùng sẵn crate này |
| `mysql_async` / `redis` / `mongodb` (đang dùng) | 0.37.1 / 1.8.1 / 3.10.1 | hook R6 §2.25 |
| rustc máy dev | 1.98.1 | |
| `scylladb/scylla` | `2026.1`, `6.2` | amd64 + arm64 |
| `cassandra` | `5.0.9`, `4.1.12` | đa kiến trúc |
| `clickhouse/clickhouse-server` | `26.3` (LTS), `23.8` | amd64 + arm64 |
| `cockroachdb/cockroach` | `v25.4.20` | amd64 + arm64 |
| `pingcap/tidb` | `v8.5.7` | amd64 + arm64 |
| `yugabytedb/yugabyte` | `2025.2.8.1-b8` | amd64 + arm64 |
