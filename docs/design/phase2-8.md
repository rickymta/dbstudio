# DBStudio — Hợp đồng GĐ2 → GĐ8 (thiết kế để chia việc song song)

> Trạng thái: bản thiết kế v1 (kiến trúc sư), viết sau GĐ1. Nguồn đã đọc: `docs/{requirements,architecture,roadmap}.md`, `crates/dbstudio-core/src/*`, `crates/dbstudio-store/src/*`, `src-tauri/src/*`, `src/lib/{types,ipc}.ts`, `src/lib/mock/*`, `src/app/tabRegistry.tsx`, `src/stores/tabs.ts`, hai driver `postgres`/`mysql`.
> Phiên bản thư viện kiểm tra ngày 2026-11-13 bằng `cargo search`/`cargo info`/`pnpm view`.
> Quy ước trong tài liệu: "char offset" = chỉ số **ký tự Unicode (scalar)** tính từ 0, giống hợp đồng `Error::Query.position` hiện có; frontend tự đổi sang UTF-16 khi đặt vào CodeMirror (`charToUtf16Offset` trong `src/lib/text.ts`).

## 0. Các quyết định lớn (đọc trước)

| # | Quyết định | Lý do ngắn |
|---|---|---|
| D1 | Tab editor dùng **`Lease`** (một kết nối vật lý dành riêng, lấy từ `Session::open_lease`), không mở session mới. Session vẫn đại diện cả server + pool như GĐ1. | Transaction thủ công, `SET`/bảng tạm/`USE` cần kết nối cố định; mở session mới cho mỗi tab là lặp lại TLS/SSH/xác thực và nhân đôi pool. Tên `Lease` để tránh đụng `tauri::ipc::Channel`. |
| D2 | Phân loại câu lệnh làm ở **crate mới `dbstudio-sql`** (lexer theo dialect + `sqlparser` 0.64 + bảng lệnh Mongo/Redis) và thực thi ở **lớp command** (`src-tauri`), trước khi gọi driver. Driver chỉ khai báo `Session::script_dialect()`. Không hiểu → coi là ghi. | NFR-SEC-03; một nơi duy nhất, CLI dùng lại được. |
| D3 | Xác nhận lệnh phá huỷ trên prod là **`Error::ConfirmationRequired`** (token dùng một lần, gắn với hash văn bản cuối cùng + đích, hết hạn 120 s, kèm chuỗi người dùng phải gõ = tên database). Gọi lại command với `confirm: { token, typed }`. | Dùng được cho mọi command (stream hay không) mà không đổi kiểu trả về. |
| D4 | Tham số query (`:name`, `$1`, `?`, `@p`) được **thay bằng literal phía Rust** (`dbstudio-sql::bind`) trước khi phân loại; vị trí lỗi được ánh xạ ngược. Không dùng prepared statement. | Chạy được cho script nhiều câu, mọi driver, không phải sửa driver. |
| D5 | Mọi method mới của `Session` có mặc định (`Unsupported` hoặc fallback hợp lý); **không thêm field vào struct mà driver GĐ1 đang dựng bằng literal** (`ExecSummary`, `EndOutcome::Rows`, `TableDetails`, `ColumnDetail`, `QueryRequest`). Dữ liệu mới đi bằng kiểu mới (vd. `ExecuteResult`, `LeaseState`). | Driver cũ biên dịch không sửa. |
| D6 | `Value` thêm biến thể **`Ext`** (kiểu riêng của DB: ObjectId, BSON Timestamp, Int64 Mongo, Regex…) và **đổi `Document` sang danh sách cặp có thứ tự** (`[[k,v],...]`). Đây là thay đổi phá vỡ duy nhất về hình dạng JSON; GĐ1 không driver nào sinh `Document` nên chi phí chuyển đổi nhỏ (mục 1.2). | BSON → `Value` không mất mát, giữ thứ tự khoá. |
| D7 | Collection Mongo trả về **một cột `$document`** (`Value::Document`); cột phẳng cấp 1 do frontend suy ra từ hợp các khoá đã tải. | Document không đồng nhất; khoá xuất hiện muộn; không nhân đôi payload. |
| D8 | Redis có API đọc riêng **`Session::read_key`** + sửa qua `ChangeSet::Keys`; cây dùng **`list_children_page`** (phân trang theo cursor `SCAN`) với `ObjectKind::KeyPrefix` cho thư mục ảo. | 6 kiểu dữ liệu với phân trang khác nhau, không ép vào `fetch_table`. |
| D9 | Export/backup chạy như **job nền** (`JobManager` trong `src-tauri`, kiểu trong `dbstudio-core::job`), tiến độ qua event `job://progress`. Engine (`dbstudio-export`, `dbstudio-backup`) không phụ thuộc Tauri. | APP-09, BACKUP-15 (CLI). |
| D10 | MSSQL dùng `tiberius` 0.13 **bản fork tối thiểu** (lộ `INFO`/`DONE` token và gói attention để huỷ). | Bản upstream không có API cho PRINT/row count từng câu và không có cancel (đã kiểm `QueryItem` chỉ có `Row`/`Metadata`). |
| D11 | Oracle dùng crate `oracle` 0.6.3 (ODPI-C, nạp Instant Client lúc chạy) + trạng thái driver `Unavailable`. `oracle-rs` 0.1.7 (thuần Rust) chỉ theo dõi. | Mục 3.2. |
| D12 | Giao kèo chia việc: **một agent "dựng khung"** viết toàn bộ kiểu, trait, stub command, đăng ký handler, migration SQL, kiểu TS, wrapper IPC, stub mock, tab kind, namespace i18n, service Docker — sau đó 10 gói chạy song song chỉ sửa file thuộc vùng của mình (mục 7). | Tránh xung đột trên các file dùng chung (`lib.rs`, `bridge.rs`, `types.ts`, `ipc.ts`, `tabRegistry.tsx`, `Cargo.toml`, migration). |

---

## 1. Mở rộng `dbstudio-core`

### 1.1 Bảng thay đổi so với hợp đồng GĐ1 (tương thích ngược)

| Thay đổi | Tương thích? | Cách chuyển |
|---|---|---|
| `Value::Document(BTreeMap)` → `Value::Document(Vec<(String, Value)>)`, JSON `{"t":"document","v":[["k",{..}],...]}` | **Phá vỡ** (Rust + JSON) | Chỉ test core dựng `Document`. Agent dựng khung sửa test core, kiểu TS `{ t:'document'; v:[string, Value][] }`, `src/features/data/{format.ts,JsonTree.tsx,CellViewer.tsx}`, mock. Thêm helper `Value::doc_get(&self, key)`. |
| `Value::Ext(ExtValue)`, `ValueKind::Ext` | Thêm (JSON mới) | Frontend: hiển thị `text` + nhãn `type`; export dùng `text`. |
| `Error` thêm 8 biến thể (1.3) | Thêm | `ERROR_KINDS` trong `ipc.ts` thêm kind; `toIpcError` giữ field phụ. Không file nào ngoài core `match` hết `Error` (đã grep). |
| `ObjectKind` thêm `KeyPrefix`, `Synonym`, `Package`, `Type`; folder key mới | Thêm (`#[non_exhaustive]` sẵn) | TS union + `FOLDER_KEYS` + `kindIcons.tsx` + i18n `explorer`. |
| `ResultSink::on_statement_start` (mặc định `Ok(())`), `QueryEvent::StatementStart` | Thêm | Frontend bỏ qua nếu không cần. |
| `Capabilities` thêm 15 cờ bool | Thêm (serde mặc định false) | Literal ngoài core (test `src-tauri/src/session.rs`) thêm `..Capabilities::default()`. |
| `DriverStatus::Unavailable { reason }` | Thêm (vẫn `Copy`) | TS union thêm `{ state:'unavailable'; reason }`. |
| `ConnectionConfig.ssh: Option<SshConfig>`, `TlsConfig.server_name: Option<String>` | Thêm (`#[serde(default)]`) | `dbstudio-store/src/db.rs` (đọc/ghi profile) cập nhật; migration v6. |
| `SessionInfo.dialect: ScriptDialect` | Thêm | TS thêm field. |
| `AppSettings` thêm field (`Copy` được giữ) | Thêm (`#[serde(default)]` cấp struct đã có) | TS + `DEFAULT_SETTINGS`. |
| Trait `Session` thêm ~12 method có mặc định; trait mới `Lease` | Thêm | Driver cũ biên dịch nguyên trạng. PG/MySQL chỉ thêm `script_dialect()` ở bước dựng khung. |
| `Host` (src-tauri) thêm `pick_open_path`, `pick_save_path_filtered`, `notify` | Thêm | `FakeHost` cập nhật. |
| Command GĐ1 | **Không đổi**. `connection_save` nhận thêm tham số tuỳ chọn `sshSecret` (thiếu = giữ nguyên). | — |

**Quy tắc serde chung** (áp cho mọi code dưới đây, agent dựng khung phải giữ): mọi struct có `#[serde(rename_all = "camelCase")]`; enum unit → `rename_all = "camelCase"`; **struct variant có field nhiều từ phải thêm `#[serde(rename_all = "camelCase")]` trên chính variant** (serde không truyền `rename_all` của enum xuống field) — vd. `KeyChange::SetString { ttl_ms }` → `"ttlMs"`, `Rename { new_key }`, `Expire { ttl_ms }`, `KeyEntries::String { total_bytes }`, `KeyEntries::Unsupported { type_name }`, `ImportFormat::Sql { continue_on_error }`, `RawRef::XmlNode { node_id }`, `Destination::LocalDir { file_template }`, `Error`-payload struct. Enum có biến thể mặc định là struct variant (vd. `Compression::Zstd { level: 3 }`) → `impl Default` viết tay (derive chỉ hỗ trợ unit variant). Helper fallback trong trait viết dạng generic `fn f<S: Session + ?Sized>(s: &S, ..)` để gọi được từ method mặc định.

Module mới trong core: `script.rs`, `lease.rs`, `edit.rs`, `keyvalue.rs`, `plan.rs` (chỉ `ExplainRequest/RawPlan/PlanFormat`), `model.rs` (schema model, DDL, stream, bulk), `job.rs`, `ssh.rs`, `lease_kit.rs` (helper, mục 1.6.4). Core thêm phụ thuộc `tokio` (`sync`, `rt`, `time`) cho `lease_kit` và `CancelToken`.

### 1.2 `Value`

```rust
// value.rs (bổ sung)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", content = "v", rename_all = "camelCase")]
pub enum Value {
    // ... các biến thể GĐ1 giữ nguyên ...
    /// Nested document; field order preserved. JSON: array of [name, value] pairs.
    Document(Vec<(String, Value)>),
    /// Database-specific scalar without a generic equivalent.
    Ext(ExtValue),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtValue {
    /// Native type key (closed list below). `{"type":"objectId","text":"65f0..."}`
    #[serde(rename = "type")]
    pub ty: String,
    /// Canonical text of the value; round-trips exactly through the driver.
    pub text: String,
}
// ValueKind thêm `Ext`.
```

Danh sách `ExtValue.ty` (đóng; thêm phải cập nhật tài liệu):

| `ty` | `text` | Dùng bởi |
|---|---|---|
| `objectId` | 24 ký tự hex thường | Mongo |
| `int64` | số thập phân | Mongo `Int64` (Int32 → `Value::Int`) |
| `bsonTimestamp` | `"<t>,<i>"` | Mongo |
| `regex` | `/pattern/flags` | Mongo |
| `binary` | `"<subtype 2 hex>:<base64>"` (subtype 0 → `Bytes`, 4 → `Uuid`) | Mongo |
| `javascript`, `javascriptWithScope`, `symbol`, `dbPointer` | Canonical Extended JSON của giá trị | Mongo |
| `minKey`, `maxKey`, `undefined` | `""` | Mongo |
| `dateMillis` | ms từ epoch (ngày ngoài năm 0001–9999) | Mongo |
| `truncatedText`, `truncatedBytes` | tiền tố (bytes → base64) — ô LOB vượt `AppSettings.maxLobBytes`; ô này **không sửa được** | Oracle, MSSQL |
| `rowid` | ROWID Oracle | Oracle |
| `interval` | literal gốc (`+02 03:04:05.000000`) | Oracle, PG có thể dùng `text` như cũ |

Ánh xạ thời gian giữ nguyên quy tắc GĐ1; thêm: múi giờ dạng vùng (Oracle `TIMESTAMP WITH TIME ZONE` theo region) → `Timestamp("2024-05-31T13:45:00.123456789+07:00[Asia/Ho_Chi_Minh]")` (RFC 9557).

### 1.3 `Error` (bổ sung)

```rust
pub enum Error {
    // ... GĐ1 ...
    /// Statement rejected because the connection is read-only (CONN-10).
    #[error("read-only connection: {message}")]
    ReadOnlyViolation { message: String, position: Option<u32> },
    /// Destructive statements on a production connection need confirmation (CONN-11).
    #[error("confirmation required")]
    ConfirmationRequired(Box<ConfirmationRequest>),
    /// The lease is already running a statement.
    #[error("busy: {0}")]
    Busy(String),
    /// Server denied a privilege (e.g. Oracle V$ views for actual plans).
    #[error("permission denied: {0}")]
    Permission(String),
    /// Driver cannot run on this machine (Oracle Instant Client missing...).
    #[error("driver unavailable: {0}")]
    DriverUnavailable(String),
    /// SSH host key unknown or changed.
    #[error("ssh host key: {host}")]
    SshHostKey { host: String, port: u16, key_type: String, fingerprint: String, changed: bool },
    /// Checksum / decryption failure of a backup file (wrong passphrase looks the same).
    #[error("integrity error: {0}")]
    Integrity(String),
    /// Change #index of a ChangeSet failed; everything was rolled back (when atomic).
    #[error("change {index} failed: {message}")]
    ChangeFailed { index: u32, message: String, applied: u32 },
}
```

Serialize (`kind` cố định, `message` không tiền tố như GĐ1):

| Biến thể | `kind` | Field phụ |
|---|---|---|
| ReadOnlyViolation | `readOnly` | `position?` |
| ConfirmationRequired | `confirmationRequired` | `confirmation: ConfirmationRequest` |
| Busy | `busy` | — |
| Permission | `permission` | — |
| DriverUnavailable | `driverUnavailable` | — |
| SshHostKey | `sshHostKey` | `host, port, keyType, fingerprint, changed` |
| Integrity | `integrity` | — |
| ChangeFailed | `changeFailed` | `index, applied` |

### 1.4 Cây đối tượng

```rust
// object.rs — thêm vào ObjectKind (non_exhaustive)
KeyPrefix,   // Redis virtual folder; name = full prefix incl. delimiter, e.g. "user:1:"
Synonym,     // MSSQL / Oracle
Package,     // Oracle package (spec + body shown as one node, DDL gives both)
Type,        // user-defined type (PG/MSSQL/Oracle) — optional for drivers

pub mod folder {
    // ... GĐ1 ...
    pub const COLLECTIONS: &str = "collections"; // Mongo
    pub const SYNONYMS: &str = "synonyms";
    pub const PACKAGES: &str = "packages";
    pub const TYPES: &str = "types";
}
```

Quy ước tên đoạn `Key` (Redis, khoá nhị phân): nếu khoá là UTF-8 hợp lệ và không bắt đầu bằng `"\u{0}b64:"` thì `name` = khoá; ngược lại `name = "\u{0}b64:" + base64(bytes)`. Hàm `dbstudio_core::keyvalue::{encode_key_name, decode_key_name}`; frontend hiển thị bằng `displayKeyName()`.

Phân trang cây (dùng cho Redis, mặc định cho mọi driver):

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChildrenPageRequest {
    pub parent: ObjectPath,
    #[serde(default)] pub cursor: Option<String>,
    pub limit: u32,                       // clamp 1..=5000
    /// Glob (`*`, `?`, `[...]`) on the child name; Redis passes it to SCAN MATCH.
    #[serde(default)] pub pattern: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChildrenPage {
    pub nodes: Vec<ObjectNode>,
    pub next_cursor: Option<String>,      // None = done
    pub approximate_total: Option<u64>,
}
```
Mặc định trong trait: gọi `list_children`, lọc `pattern` (glob không phân biệt hoa thường), cắt theo `cursor` = offset thập phân. Redis: một trang có thể chứa lại một `KeyPrefix` đã trả ở trang trước — frontend khử trùng theo `pathKey`.

`ObjectNode.detail` cho `Key`: `"<type>"` hoặc `"<type> ttl:<seconds>"` (type ∈ `string|hash|list|set|zset|stream|ReJSON-RL|...`); frontend parse bằng `parseKeyDetail`.

### 1.5 Script: dialect, phân tích, tham số, xác nhận (`script.rs`)

```rust
/// Language of the editor / console for a session. Decides lexer, splitter, params, classifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "name", rename_all = "camelCase")]
pub enum ScriptDialect {
    Postgres,
    #[serde(rename_all = "camelCase")]
    MySql { mariadb: bool, backslash_escapes: bool },
    MsSql,
    Oracle,
    MongoShell,
    RedisCli,
    /// Unknown language: one statement, classified `Unknown` (treated as a write).
    Opaque,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextRange { pub start: u32, pub end: u32 } // char offsets, end exclusive

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScriptAnalysis {
    pub dialect: ScriptDialect,
    pub statements: Vec<StatementInfo>,
    pub params: Vec<ParamRef>,
    /// Lexer problems (unterminated string/comment...). Non-empty => every statement Unknown.
    pub problems: Vec<ScriptProblem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatementInfo {
    pub index: u32,
    pub range: TextRange,           // without trailing terminator / GO line
    pub class: StatementClass,
    /// Leading verb for display: "SELECT", "ALTER TABLE", "db.users.find", "FLUSHDB".
    pub verb: String,
    pub destructive: Option<DestructiveReason>,
    /// MySQL/Oracle DDL commits the open transaction implicitly.
    pub implicit_commit: bool,
    /// Best-effort referenced object names (for history and confirmation text).
    pub targets: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum StatementClass {
    Read,                                   // SELECT, SHOW, find, GET...
    Write,                                  // INSERT/UPDATE/DELETE/MERGE, insertOne, SET key...
    Ddl,                                    // CREATE/ALTER/DROP/TRUNCATE, createIndex, drop
    Transaction { op: TxControl },
    Session,                                // SET search_path, USE db, SELECT db (Redis), ALTER SESSION
    Procedural,                             // CALL/EXEC/anonymous PL/SQL, runCommand, EVAL
    Explain { analyze: bool, writes: bool },
    Admin,                                  // GRANT, SET GLOBAL, CONFIG SET, KILL, SHUTDOWN
    Empty,                                  // comments only
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TxControl { Begin, Commit, Rollback, Savepoint, Release, RollbackToSavepoint }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DestructiveReason {
    Drop, Truncate, DeleteWithoutWhere, UpdateWithoutWhere, AlterDrop,
    FlushDb, FlushAll, DeleteByPattern,           // Redis
    DropCollection, DropDatabase, DeleteManyAll, UpdateManyAll, // Mongo
    Restore, ImportReplace,                        // app operations (backup/import)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScriptProblem { pub range: TextRange, pub message: String }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ParamStyle { Named, Dollar, Question, At } // :name  $1  ?  @p

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParamRef {
    /// Key used in ParamValue.name: ":id", "$1", "?3" (3rd question mark), "@p".
    pub name: String,
    pub style: ParamStyle,
    pub ranges: Vec<TextRange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParamValue { pub name: String, pub value: ParamInput }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "v", rename_all = "camelCase")]
pub enum ParamInput {
    Null,
    Text(String),      // rendered as a dialect string literal (escaped)
    Number(String),    // validated against /^-?\d+(\.\d+)?([eE][-+]?\d+)?$/
    Bool(bool),
    Raw(String),       // SQL expression typed by the user, inserted verbatim (trusted)
}

/// Payload of Error::ConfirmationRequired.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfirmationRequest {
    pub token: String,                 // 32 hex chars, single use
    /// Text the user must type exactly (database name, "db0" for Redis).
    pub confirm_text: String,
    pub statements: Vec<DestructiveStatement>,
    pub expires_in_ms: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DestructiveStatement {
    pub index: u32,
    pub range: TextRange,              // in the ORIGINAL (pre-binding) text
    pub text: String,                  // statement text, truncated to 2000 chars
    pub reason: DestructiveReason,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfirmationGrant { pub token: String, pub typed: String }

/// Request body of `query_execute` / `lease_execute`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecuteRequest {
    #[serde(flatten)]
    pub query: QueryRequest,
    #[serde(default)] pub params: Vec<ParamValue>,
    #[serde(default)] pub confirm: Option<ConfirmationGrant>,
    /// Record in query history (default true).
    #[serde(default = "crate::script::yes")] pub record_history: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecuteResult {
    pub summary: ExecSummary,
    pub state: Option<LeaseState>,     // None for query_execute
    pub history_id: Option<i64>,
}
```

**Luồng bảo vệ ở lớp command** (`src-tauri/src/guard.rs`, gọi `dbstudio_sql::guard`):
1. `bind(dialect, text, params)` → `BoundScript { text, map }` (nếu `params` rỗng, giữ nguyên).
2. `analyze(dialect, &bound.text)`.
3. `Policy { read_only: config.read_only, production: config.environment == Some(Production), confirm_text }` với `confirm_text` = database hiện hành của lease, hoặc `config.database`, hoặc `"db<N>"` (Redis).
4. `check(&policy, &analysis) -> Verdict`:
   - `read_only`: chỉ cho `Read`, `Explain{writes:false}` (cả analyze), `Transaction`, `Session`, `Empty`. Còn lại → `Error::ReadOnlyViolation { position: range.start ánh xạ ngược }`.
   - `production` và có `destructive` → nếu `confirm` hợp lệ (token tồn tại, chưa hết hạn, chưa dùng, khớp `sha256(target_id ‖ bound.text)`, `typed == confirm_text` so khớp tuyệt đối) → cho chạy và huỷ token; ngược lại tạo token mới → `Error::ConfirmationRequired`.
5. Ghi lịch sử, gọi driver, ánh xạ ngược `position` trong lỗi và `StatementStart.range` bằng `bound.map`.

Kho token: `ConfirmTokens` trong `AppState` (`Mutex<HashMap<String, TokenEntry{digest, target, expires}>>`), dọn khi tra. Cùng cơ chế dùng cho `restore_start`, `import_start` (chế độ ghi đè), `keys_delete_pattern` với `digest = sha256(serde_json(spec))`.

### 1.6 `Lease` — kết nối riêng cho tab editor / console (`lease.rs`)

#### 1.6.1 Kiểu

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaseOptions {
    #[serde(default)] pub database: Option<String>,
    #[serde(default)] pub schema: Option<String>,
    #[serde(default = "crate::script::yes")] pub auto_commit: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum IsolationLevel { ReadUncommitted, ReadCommitted, RepeatableRead, Serializable, Snapshot }

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TxOptions {
    #[serde(default)] pub read_only: bool,
    #[serde(default)] pub isolation: Option<IsolationLevel>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TxStatus { Idle, Active, Failed, Unknown }   // Failed = PG "aborted", only ROLLBACK works

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaseState {
    pub auto_commit: bool,
    pub transaction: TxStatus,
    pub running: Option<Uuid>,          // query id currently executing
    pub pending_result: Option<Uuid>,   // query id whose last result set can fetch_more
    pub database: Option<String>,
    pub schema: Option<String>,
    pub broken: bool,                   // connection lost; only close() is valid
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaseContext { pub database: Option<String>, pub schema: Option<String> }
```

#### 1.6.2 Trait

```rust
#[async_trait]
pub trait Lease: Send + Sync {
    /// Run `req` on this connection. `req.database` is ignored (use set_context).
    /// One statement batch at a time: a concurrent call fails with Error::Busy.
    /// When the LAST result set of the request hits `max_rows` and more rows exist, the
    /// result stays open (state.pending_result = Some(query_id)) until fetch_more,
    /// discard_result, the next execute/begin/commit/rollback/set_*, or close.
    async fn execute(&self, req: QueryRequest, sink: &mut dyn ResultSink) -> Result<ExecSummary>;
    /// Continue the pending result: on_rows* then on_statement_end (row_count = rows of this
    /// call, truncated = more remain). No on_columns. Wrong id => Error::NotFound.
    async fn fetch_more(&self, query_id: Uuid, max_rows: u64, batch_size: Option<u32>,
                        sink: &mut dyn ResultSink) -> Result<ExecSummary>;
    async fn discard_result(&self) -> Result<()>;
    /// Cancel only if `query_id` is the running statement (late cancel never hits the next one).
    /// Unknown/finished id => Ok(()).
    async fn cancel(&self, query_id: Uuid) -> Result<()>;
    async fn begin(&self, opts: TxOptions) -> Result<()>;
    async fn commit(&self) -> Result<()>;
    async fn rollback(&self) -> Result<()>;
    /// Switching to auto-commit with an open transaction => Error::Busy (UI asks first).
    async fn set_auto_commit(&self, on: bool) -> Result<()>;
    /// USE db / SET search_path / ALTER SESSION SET CURRENT_SCHEMA / SELECT n (Redis).
    async fn set_context(&self, ctx: LeaseContext) -> Result<()>;
    /// Cheap cached snapshot, never blocks.
    fn state(&self) -> LeaseState;
    async fn ping(&self) -> Result<()>;
    async fn explain(&self, req: ExplainRequest) -> Result<RawPlan> {
        let _ = req; Err(Error::Unsupported("explain".into()))
    }
    /// Stream a whole table (export/backup) on this connection, within its transaction.
    async fn stream_table(&self, req: TableStreamRequest, sink: &mut dyn ResultSink)
        -> Result<ExecSummary> { let _ = (req, sink); Err(Error::Unsupported("table_stream".into())) }
    async fn bulk_insert(&self, req: BulkInsertRequest) -> Result<BulkInsertResult> {
        let _ = req; Err(Error::Unsupported("bulk_insert".into()))
    }
    /// Cancel running work, roll back an open transaction, close the connection.
    /// Returns whether a transaction was rolled back. Idempotent.
    async fn close(&self) -> Result<LeaseCloseReport>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaseCloseReport { pub rolled_back: bool, pub forced: bool }
```

#### 1.6.3 Ngữ nghĩa bắt buộc

- **Kết nối**: lease mở một kết nối vật lý **ngoài pool** (không tính vào giới hạn pool), áp cùng TLS/SSH/read-only của session. `SessionManager` giới hạn 32 lease/session.
- **Auto-commit** (`auto_commit=true`): mỗi câu lệnh tự commit; người dùng gõ `BEGIN` vẫn hợp lệ — driver cập nhật `transaction` theo phân loại `Transaction{op}` (dùng `dbstudio_sql::analyze` trên văn bản đã chạy) và, nếu rẻ, hỏi server: MSSQL `SELECT @@TRANCOUNT`, Oracle `DBMS_TRANSACTION.LOCAL_TRANSACTION_ID`, Mongo theo `ClientSession`. PG không có cách rẻ → dựa vào phân loại + mã lỗi `25P02` (→ `Failed`). MySQL/Oracle: câu `implicit_commit` đưa về `Idle`.
- **Manual** (`auto_commit=false`): transaction mở **lười** trước câu ghi/đọc đầu tiên (PG `BEGIN`, MySQL `SET autocommit=0`, MSSQL `SET IMPLICIT_TRANSACTIONS ON`, Oracle mặc định, Mongo `startTransaction` — chỉ replica set, standalone → `Unsupported("transactions_standalone")`).
- **Huỷ**: `cancel(query_id)` dùng cơ chế giao thức (PG CancelToken, MySQL `KILL QUERY` qua kết nối ngắn hạn, MSSQL attention (fork), Oracle `break_execution`, Mongo `killOp` theo `comment = query_id`, Redis `CLIENT KILL ID` — lease Redis thành `broken`). Huỷ không đóng transaction (trừ PG → `Failed`). `timeout_ms` → cùng đường huỷ, trả `Error::Timeout`.
- **Sink dừng** (Channel đóng): driver huỷ/xả như GĐ1 để kết nối dùng lại được; nếu không xả được → `broken`.
- **Đóng tab**: frontend kiểm `state.transaction != idle` → hỏi người dùng (QUERY-11) → gọi `lease_close`. `close()` = huỷ câu đang chạy (chờ tối đa 5 s) → `ROLLBACK` nếu có transaction (tối đa 5 s) → đóng socket; bước nào quá hạn thì đóng socket luôn (`forced=true`, server tự rollback).
- **Đóng session / thoát app**: `SessionManager.close*` đóng mọi lease của session trước, rồi `Session::close`.
- **Mất kết nối** (CONN-12): method trả `Error::Connection`, `broken=true`, `src-tauri` phát event `lease://state`; frontend đề nghị mở lease mới, cảnh báo nếu transaction đang mở đã mất.
- **Drop không gọi close**: kết nối bị đóng (server rollback). Driver phải đảm bảo không trả kết nối lease vào pool.

#### 1.6.4 `lease_kit` (helper tuỳ chọn trong core)

Mô hình actor: một tokio task sở hữu kết nối, nhận lệnh qua `mpsc`; `ActorLease<B: LeaseBackend>` hiện thực `Lease` (busy, khớp `query_id`, cache `LeaseState`, thứ tự close). Driver chỉ hiện thực:

```rust
#[async_trait]
pub trait LeaseBackend: Send + 'static {
    async fn execute(&mut self, req: QueryRequest, sink: &mut dyn ResultSink, keep_open: bool) -> Result<ExecSummary>;
    async fn fetch_more(&mut self, max_rows: u64, batch: usize, sink: &mut dyn ResultSink) -> Result<ExecSummary>;
    async fn discard_result(&mut self) -> Result<()>;
    async fn tx(&mut self, op: TxControl, opts: TxOptions) -> Result<()>;
    async fn set_auto_commit(&mut self, on: bool) -> Result<()>;
    async fn set_context(&mut self, ctx: LeaseContext) -> Result<()>;
    async fn probe_tx(&mut self) -> Result<TxStatus>;
    async fn ping(&mut self) -> Result<()>;
    async fn close(&mut self) -> Result<bool>;
    /// Handle usable concurrently from another task to interrupt the running call.
    fn canceller(&self) -> Arc<dyn Canceller>;
}
#[async_trait] pub trait Canceller: Send + Sync { async fn cancel(&self) -> Result<()>; }
```
Kết quả treo (`pending_result`) = actor dừng giữa vòng đọc dòng, chờ lệnh `fetch_more`/`discard`. Gói B viết helper này đầu tiên (milestone B1); các driver khác dùng hoặc tự hiện thực `Lease`.

### 1.7 Bổ sung stream kết quả

```rust
pub trait ResultSink: Send {
    // ... GĐ1 ...
    /// A statement begins (char offsets into the request text). Optional for drivers.
    fn on_statement_start(&mut self, start: StatementStart) -> Result<()> { let _ = start; Ok(()) }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatementStart { pub index: u32, pub range: TextRange }

pub enum QueryEvent { /* ... */  StatementStart { start: StatementStart } }
// JSON: {"type":"statementStart","start":{"index":0,"range":{"start":0,"end":17}}}
```
MSSQL không tách được câu trong một batch T-SQL → `range` có thể là cả batch. Quy tắc kết thúc stream của GĐ1 (`statementEnd` cuối cùng, chờ tối đa 5 s) áp dụng cho `query_execute`, `lease_execute`, `lease_fetch_more`.

### 1.8 Sửa dữ liệu (`edit.rs`)

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Editability {
    pub editable: bool,
    pub key_columns: Vec<String>,          // PK, else first unique NOT NULL index; Mongo ["_id"]
    pub read_only_columns: Vec<String>,    // identity/generated/computed/rowversion
    pub reason: Option<NotEditableReason>,
    /// apply_changes is all-or-nothing (false: Mongo standalone, Redis).
    pub atomic: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NotEditableReason { NoUniqueKey, NotATable, ReadOnlyConnection, Unsupported }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "model", rename_all = "camelCase")]
pub enum ChangeSet {
    Rows(RowChangeSet),
    Documents(DocumentChangeSet),
    Keys(KeyChangeSet),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RowChangeSet {
    pub table: ObjectPath,
    pub key_columns: Vec<String>,
    pub changes: Vec<RowChange>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum RowChange {
    /// `key` aligned with key_columns, values as fetched.
    Update { key: Vec<Value>, set: Vec<CellEdit> },
    Insert { values: Vec<CellEdit> },
    Delete { key: Vec<Value> },
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CellEdit { pub column: String, pub value: EditValue }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "v", rename_all = "camelCase")]
pub enum EditValue {
    Value(Value),      // exact typed value (NULL = Value::Null, bytes from file...)
    Text(String),      // user-typed text; rendered as string literal, DB coerces to column type
    Default,           // DEFAULT keyword
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentChangeSet { pub collection: ObjectPath, pub changes: Vec<DocumentChange> }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum DocumentChange {
    Insert { document: DocInput },
    Replace { id: Value, document: DocInput },
    Update { id: Value, set: Vec<FieldEdit>, #[serde(default)] unset: Vec<Vec<String>> },
    Delete { id: Value },
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum DocInput { Value { value: Value }, Ejson { text: String } } // Ejson: relaxed/canonical EJSON typed by user
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FieldEdit { pub path: Vec<String>, pub value: Value }   // ["address","city"]

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyChangeSet { pub database: ObjectPath, pub changes: Vec<KeyChange> }
/// `key` uses the Key segment name encoding (1.4). Member/field/value: Text or Bytes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum KeyChange {
    SetString { key: String, value: Value, #[serde(default)] ttl_ms: Option<u64> },
    HashSet { key: String, entries: Vec<(Value, Value)> },
    HashDelete { key: String, fields: Vec<Value> },
    ListSet { key: String, index: i64, value: Value },
    ListPush { key: String, values: Vec<Value>, head: bool },
    /// Remove by index: LSET to a random sentinel + LREM 1 sentinel (inside MULTI).
    ListRemoveAt { key: String, index: i64 },
    SetAdd { key: String, members: Vec<Value> },
    SetRemove { key: String, members: Vec<Value> },
    ZAdd { key: String, members: Vec<(Value, String)> },   // (member, score text)
    ZRemove { key: String, members: Vec<Value> },
    StreamAdd { key: String, id: Option<String>, fields: Vec<(Value, Value)> },
    StreamDelete { key: String, ids: Vec<String> },
    Expire { key: String, ttl_ms: Option<u64> },            // None = PERSIST
    Rename { key: String, new_key: String },                // RENAMENX; exists => ChangeFailed
    Delete { keys: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyResult { pub applied: u32, pub affected: Vec<u64>, pub atomic: bool }
```

Ngữ nghĩa:
- `preview_changes` trả văn bản sẽ chạy (SQL theo dialect; lệnh shell Mongo `db.c.updateOne({...}, {$set:{...}})`; lệnh Redis kiểu redis-cli), một phần tử/thay đổi, đúng thứ tự áp dụng: tất cả `Delete`, rồi `Update`, rồi `Insert` (giảm xung đột khoá). Literal dùng `dbstudio_sql::Dialect`.
- `apply_changes` (Rows): một transaction trên kết nối pool; mỗi `Update`/`Delete` phải ảnh hưởng **đúng 1 dòng**, khác → rollback, `Error::ChangeFailed { index, applied: 0 }`. Lỗi DB ở thay đổi `i` → rollback toàn bộ, `ChangeFailed { index: i }` (NFR-REL-01). Cột trong `read_only_columns` bị từ chối trước khi chạy.
- Documents: replica set → transaction; standalone → tuần tự, dừng ở lỗi đầu, `ChangeFailed { applied }`, `atomic=false`. `Update` dùng `$set`/`$unset` theo path; `Replace` dùng `replaceOne({_id})`; `matchedCount != 1` → `ChangeFailed`.
- Keys: `WATCH` các khoá + kiểm `TYPE` trước, rồi `MULTI/EXEC`; `EXEC` trả nil (khoá đổi) → `ChangeFailed`. Redis không rollback lỗi thời gian chạy → `atomic=false`, UI ghi rõ.
- Read-only connection → lớp command trả `ReadOnlyViolation` trước khi gọi driver. Không cần xác nhận prod (mọi thay đổi có khoá).
- Copy "SQL INSERT" (DATA-08) = `preview_changes` của một `RowChangeSet` toàn `Insert`.

### 1.9 Key-value (`keyvalue.rs`)

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyReadRequest {
    pub key: ObjectPath,                     // [Database "0", (KeyPrefix...), Key k]
    #[serde(default)] pub cursor: Option<String>,
    pub count: u32,                          // clamp 1..=10_000
    #[serde(default)] pub pattern: Option<String>,   // HSCAN/SSCAN/ZSCAN MATCH
    #[serde(default)] pub max_string_bytes: Option<u64>, // default 1 MiB (GETRANGE)
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyValuePage {
    pub key: ObjectPath,
    pub key_type: String,                    // string|hash|list|set|zset|stream|ReJSON-RL|...
    pub ttl_ms: Option<i64>,                 // None = persistent
    pub length: Option<u64>,                 // STRLEN/HLEN/LLEN/SCARD/ZCARD/XLEN
    pub memory_bytes: Option<u64>,           // MEMORY USAGE (may be denied => None)
    pub encoding: Option<String>,            // OBJECT ENCODING
    pub entries: KeyEntries,
    pub next_cursor: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum KeyEntries {
    String { value: Value, total_bytes: u64, truncated: bool },
    Hash { fields: Vec<(Value, Value)> },
    List { start: u64, items: Vec<Value> },          // cursor = next start index
    Set { members: Vec<Value> },
    ZSet { members: Vec<(Value, String)> },          // score as Redis text ("inf" ok)
    Stream { entries: Vec<StreamEntry>, groups: u64 },  // cursor = next id (exclusive)
    Json { text: String },                           // RedisJSON JSON.GET
    Unsupported { type_name: String },
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamEntry { pub id: String, pub fields: Vec<(Value, Value)> }
```
Giá trị: UTF-8 hợp lệ → `Text`, ngược lại `Bytes`.

### 1.10 EXPLAIN — phía driver (`plan.rs` trong core)

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExplainRequest {
    pub query_id: Uuid,
    pub text: String,                 // exactly one statement (command layer checks)
    #[serde(default)] pub database: Option<String>,
    #[serde(default)] pub analyze: bool,
    #[serde(default)] pub options: ExplainOptions,
    #[serde(default)] pub timeout_ms: Option<u64>,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ExplainOptions { pub buffers: bool, pub verbose: bool, pub settings: bool }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawPlan {
    pub format: PlanFormat,
    pub text: String,                 // unparsed source exactly as received
    pub statement: String,
    pub analyzed: bool,
    pub rolled_back: bool,            // ANALYZE ran inside a transaction that was rolled back
    pub server_version: String,
    pub captured_at: String,          // RFC 3339 UTC
    #[serde(default)] pub extra: BTreeMap<String, String>, // e.g. "xplan" (Oracle DBMS_XPLAN text)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PlanFormat {
    PostgresJson,        // EXPLAIN (FORMAT JSON[, ANALYZE, BUFFERS, VERBOSE, SETTINGS])
    MySqlJson,           // EXPLAIN FORMAT=JSON (v1)
    MySqlJsonV2,         // 8.3+ explain_json_format_version=2 (EXPLAIN [ANALYZE] FORMAT=JSON)
    MySqlTree,           // EXPLAIN ANALYZE / FORMAT=TREE text
    MariaDbJson,         // EXPLAIN FORMAT=JSON / ANALYZE FORMAT=JSON (r_rows, r_total_time_ms)
    MsSqlShowplanXml,    // SET SHOWPLAN_XML ON / SET STATISTICS XML ON
    OraclePlanRows,      // JSON array of PLAN_TABLE / V$SQL_PLAN_STATISTICS_ALL rows
    MongoExplain,        // canonical EJSON of explain("queryPlanner"|"executionStats")
}
```

Quy tắc ANALYZE (PLAN-01): driver **luôn** bọc ANALYZE trong transaction rồi rollback, kể cả SELECT (`rolled_back=true`): PG `BEGIN; EXPLAIN (ANALYZE...) ...; ROLLBACK`; MSSQL `BEGIN TRAN; SET STATISTICS XML ON; <stmt>; SET STATISTICS XML OFF; ROLLBACK`; Oracle chạy `/*+ GATHER_PLAN_STATISTICS */` trong transaction rồi `ROLLBACK`, đọc `V$SQL_PLAN_STATISTICS_ALL` (thiếu quyền → `Error::Permission`). Câu `Ddl` hoặc `implicit_commit` → `Unsupported("explain_analyze_ddl")`. MySQL: ANALYZE câu ghi → `Unsupported("explain_analyze_writes")` (bảng MyISAM không rollback được). Mongo: `executionStats` không ghi dữ liệu với update/delete. UI luôn cảnh báo trước khi ANALYZE câu ghi; read-only connection chặn ANALYZE câu ghi ở guard.

### 1.11 ERD, DDL, stream bảng, nạp hàng loạt (`model.rs`)

```rust
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ModelDetail { Keys, #[default] Full }  // Keys: PK/FK/unique cols only (autocomplete uses Full w/o comments)

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SchemaModelRequest {
    pub scope: ObjectPath,                         // Database or Schema node
    #[serde(default)] pub tables: Vec<ObjectPath>, // empty = all tables in scope
    #[serde(default)] pub include_views: bool,
    #[serde(default)] pub include_comments: bool,
    #[serde(default)] pub detail: ModelDetail,
    #[serde(default)] pub sample_size: Option<u32>, // Mongo inference (default 1000)
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SchemaModel {
    pub scope: ObjectPath,
    pub tables: Vec<TableModel>,
    pub relations: Vec<Relation>,
    pub inferred: bool,          // Mongo sampling
    pub truncated: bool,         // more than 2000 tables: first 2000 by name
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TableModel {
    pub path: ObjectPath,        // full tree path incl. Folder segments
    pub kind: ObjectKind,
    pub schema: Option<String>,
    pub name: String,
    pub comment: Option<String>,
    pub columns: Vec<ColumnDetail>,   // reuse GĐ1 type; Mongo: typeName "string|int32", extra "seen 87%"
    pub primary_key: Vec<String>,
    pub unique_keys: Vec<Vec<String>>,
    pub estimated_rows: Option<u64>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Relation {
    pub name: String,
    pub from_table: ObjectPath,
    pub from_columns: Vec<String>,
    pub to_table: ObjectPath,
    pub to_columns: Vec<String>,
    pub on_update: Option<String>,
    pub on_delete: Option<String>,
    pub from_optional: bool,      // any FK column nullable => "zero or one"
    pub one_to_one: bool,         // FK columns are themselves PK/unique
    pub inferred: bool,           // Mongo heuristic (fooId -> foos._id)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DdlRequest {
    pub scope: ObjectPath,                         // Database, Schema or one object
    #[serde(default)] pub objects: Vec<ObjectPath>,// restrict (tables chosen in export)
    #[serde(default)] pub options: DdlOptions,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DdlOptions {
    pub include_drop: bool, pub if_not_exists: bool,
    pub include_indexes: bool, pub include_foreign_keys: bool, pub include_triggers: bool,
    pub include_views: bool, pub include_routines: bool, pub include_sequences: bool,
    pub include_comments: bool, pub include_grants: bool, pub qualify_schema: bool,
} // Default: everything true except include_drop, if_not_exists, include_grants
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DdlSection { PreData, PostData }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DdlLanguage { Sql, MongoShell }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DdlItem {
    pub path: ObjectPath,
    pub kind: ObjectKind,
    pub section: DdlSection,
    /// One executable unit including its terminator (";", MSSQL: one batch without GO,
    /// Oracle PL/SQL: ends with "\n/").
    pub sql: String,
    pub language: DdlLanguage,
    pub depends_on: Vec<ObjectPath>,
}
```
Thứ tự `Vec<DdlItem>` là thứ tự tạo: toàn bộ `PreData` (schema → type → sequence → table *không có FK* nhưng có PK/unique/check → view/mview theo phụ thuộc → function/procedure/package) rồi `PostData` (index thường → FK → trigger → `setval`/identity reseed → comment/grant). Export chèn dữ liệu giữa hai phần. FK vòng không còn là vấn đề vì FK nằm ở PostData.

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TableStreamRequest {
    pub query_id: Uuid,
    pub table: ObjectPath,
    #[serde(default)] pub columns: Vec<String>,     // empty = all, ordinal order
    #[serde(default)] pub filters: Vec<ColumnFilter>,
    #[serde(default)] pub raw_where: Option<String>,
    #[serde(default)] pub order_by_key: bool,
    #[serde(default)] pub batch_size: Option<u32>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OnConflict { Error, Skip, Update }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BulkInsertRequest {
    pub table: ObjectPath,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<EditValue>>,
    pub on_conflict: OnConflict,
    #[serde(default)] pub conflict_columns: Vec<String>, // default: primary key
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BulkInsertResult { pub inserted: u64, pub updated: u64, pub skipped: u64 }
```
`bulk_insert` chạy **trong transaction hiện hành của lease** (không tự commit); lỗi ở một dòng → `Err` cho cả lô. Cách chọn: PG `COPY ... FROM STDIN` (text) khi `on_conflict=Error`, ngược lại `INSERT ... ON CONFLICT`; MySQL `INSERT [IGNORE] ... [ON DUPLICATE KEY UPDATE]` nhiều dòng (giới hạn `max_allowed_packet`); MSSQL TDS bulk load (`Error`) hoặc `MERGE`; Oracle array DML (`Batch`) / `MERGE`; Mongo `insertMany(ordered)` / `bulkWrite` upsert.

### 1.12 Job nền (`job.rs`)

```rust
pub type JobId = Uuid;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum JobKind { Export, Import, SqlScript, Backup, Restore, Verify, KeyDelete }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum JobStatus { Queued, Running, Succeeded, Failed, Cancelled }
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobProgress {
    pub phase: String,                 // stable i18n key: "schema","data","compress","encrypt","verify","restore"
    pub object: Option<String>,        // "public.orders"
    pub objects_done: u32,
    pub objects_total: Option<u32>,
    pub rows_done: u64,
    pub rows_total: Option<u64>,       // estimate
    pub bytes_done: u64,
    pub bytes_total: Option<u64>,
    pub message: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobError { pub kind: String, pub message: String }   // from Error::kind()/message()
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobInfo {
    pub id: JobId,
    pub kind: JobKind,
    pub title: String,                 // already human readable: "Export app → orders.xlsx"
    pub connection_id: Option<Uuid>,
    pub status: JobStatus,
    pub progress: JobProgress,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub error: Option<JobError>,
    /// Kind-specific outcome, e.g. {"path": "...", "sizeBytes": 123, "rows": 10, "warnings": [...]}.
    pub result: Option<serde_json::Value>,
    pub cancellable: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum JobEvent {
    Started { job: JobInfo },
    #[serde(rename_all = "camelCase")]
    Progress { job_id: JobId, progress: JobProgress },
    Finished { job: JobInfo },
}
/// Cooperative cancellation flag shared by a job and the engine.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>, Arc<tokio::sync::Notify>);
impl CancelToken { pub fn cancel(&self); pub fn is_cancelled(&self) -> bool; pub async fn cancelled(&self); }
/// What engines (export/backup) use to report.
pub trait JobReporter: Send + Sync {
    fn progress(&self, p: &JobProgress);          // throttled by the manager (≤ 4/s per job)
    fn log(&self, line: &str) { let _ = line; }   // tail kept for backup_runs.log
    fn cancel_token(&self) -> &CancelToken;
}
```
Event Tauri `job://progress`, payload là `JobEvent`, ví dụ:
```json
{"type":"progress","jobId":"0b7e...","progress":{"phase":"data","object":"public.orders","objectsDone":3,"objectsTotal":12,"rowsDone":150000,"rowsTotal":1000000,"bytesDone":20971520,"bytesTotal":null,"message":null}}
```
Huỷ: `job_cancel` → `CancelToken::cancel` + `Session/Lease::cancel(query_id)` của truy vấn đang chạy; engine xoá file tạm; status `Cancelled`. Job không lưu SQLite (chỉ trong bộ nhớ, giữ 100 job xong gần nhất); lịch sử backup/restore lưu ở `backup_runs`.

### 1.13 SSH (`ssh.rs`) — CONN-06

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SshConfig {
    pub host: String,
    #[serde(default = "ssh_port")] pub port: u16,              // 22
    pub username: String,
    pub auth: SshAuth,
    #[serde(default)] pub keepalive_secs: Option<u32>,
    #[serde(default)] pub connect_timeout_ms: Option<u64>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "camelCase")]
pub enum SshAuth {
    Password,                                                  // secret: keychain "<connId>#ssh"
    #[serde(rename_all = "camelCase")]
    PrivateKey { key_path: String, has_passphrase: bool },     // passphrase: "<connId>#ssh"
    Agent,                                                     // SSH_AUTH_SOCK / Windows OpenSSH pipe
}
// TlsConfig thêm: #[serde(default)] pub server_name: Option<String>  // SNI + verify name override
```
`SessionManager::connect`: nếu `config.ssh` có → `dbstudio_ssh::Tunnel::open(...)` (listener `127.0.0.1:0`, nhận nhiều kết nối) → clone config với `host=127.0.0.1`, `port=local`, `tls.server_name=Some(host gốc)` → `Driver::connect`. Tunnel sống cùng session (đóng khi session đóng). Host key: tra bảng `ssh_known_hosts`; chưa biết/đổi → `Error::SshHostKey`; UI hỏi → `ssh_trust_host` → mở lại. Driver phải tôn trọng `tls.server_name` (PG: `tokio-postgres-rustls` dùng tên trong `ServerName`; MySQL: xem rủi ro R6).

### 1.14 Trait `Session` — danh sách method mới (đều có mặc định)

```rust
#[async_trait]
pub trait Session: Send + Sync {
    // ... GĐ1 giữ nguyên ...
    fn script_dialect(&self) -> ScriptDialect { ScriptDialect::Opaque }
    async fn open_lease(&self, opts: LeaseOptions) -> Result<Box<dyn Lease>> {
        let _ = opts; Err(Error::Unsupported("leases".into()))
    }
    async fn list_children_page(&self, req: ChildrenPageRequest) -> Result<ChildrenPage> {
        crate::fallback::children_page(self, req).await      // list_children + glob + offset cursor
    }
    async fn editability(&self, path: &ObjectPath) -> Result<Editability> {
        crate::fallback::editability_from_details(self, path).await // PK, else unique NOT NULL index
    }
    async fn preview_changes(&self, changes: &ChangeSet) -> Result<Vec<String>> {
        let _ = changes; Err(Error::Unsupported("data_editing".into()))
    }
    async fn apply_changes(&self, changes: ChangeSet) -> Result<ApplyResult> {
        let _ = changes; Err(Error::Unsupported("data_editing".into()))
    }
    async fn explain(&self, req: ExplainRequest) -> Result<RawPlan> {
        let _ = req; Err(Error::Unsupported("explain".into()))
    }
    async fn schema_model(&self, req: SchemaModelRequest) -> Result<SchemaModel> {
        crate::fallback::schema_model_via_details(self, req).await // ≤ 300 tables, else Unsupported
    }
    async fn generate_ddl(&self, req: DdlRequest) -> Result<Vec<DdlItem>> {
        let _ = req; Err(Error::Unsupported("ddl_generation".into()))
    }
    async fn read_key(&self, req: KeyReadRequest) -> Result<KeyValuePage> {
        let _ = req; Err(Error::Unsupported("key_value".into()))
    }
}
```
`crate::fallback` (module mới) chứa ba fallback trên; chúng chỉ dùng method GĐ1 nên driver cũ có ngay tính năng (chậm hơn). Driver nên ghi đè `schema_model` bằng truy vấn catalog hàng loạt (1–4 truy vấn cho cả schema).

### 1.14b Kiểu phụ cho IPC (nằm ở crate ghi bên cạnh; mọi struct camelCase)

```rust
// src-tauri (lease.rs / commands)
pub struct LeaseInfo { pub lease_id: Uuid, pub session_id: Uuid, pub state: LeaseState }
pub struct PlanResult { pub raw: RawPlan, pub tree: Option<PlanTree>, pub parse_error: Option<String> }
pub struct FileFilter { pub name: String, pub extensions: Vec<String> }            // {"name":"CSV","extensions":["csv"]}
pub struct BackgroundSettings { pub close_to_tray: CloseBehavior, pub autostart: bool }
// dbstudio-store settings.rs
#[serde(rename_all = "camelCase")] pub enum CloseBehavior { Auto, Always, Never } // Auto = khi có backup job bật lịch
// dbstudio-sql vocabulary.rs
pub struct Vocabulary { pub keywords: Vec<String>, pub functions: Vec<String>, pub commands: Vec<CommandHint> }
pub struct CommandHint { pub name: String, pub syntax: String, pub summary: String, pub flags: Vec<String> } // Redis/Mongo
// dbstudio-store history.rs / saved.rs / tabs.rs
pub struct HistoryEntry { pub id: i64, pub connection_id: Uuid, pub database: Option<String>, pub kind: String,
    pub text: String, pub text_truncated: bool, pub started_at: String, pub elapsed_ms: Option<u64>,
    pub status: String, pub statements: Option<u32>, pub rows: Option<u64>, pub error_message: Option<String> }
pub struct HistoryQuery { pub connection_id: Option<Uuid>, pub search: Option<String>, pub status: Option<String>,
    pub before_id: Option<i64>, pub limit: u32 /* clamp 1..=500 */ }
pub struct SavedFolder { pub id: Uuid, pub name: String, pub parent_id: Option<Uuid>, pub sort_order: i64 }
pub struct SavedQuery { pub id: Uuid, pub folder_id: Option<Uuid>, pub name: String, pub text: String,
    pub is_snippet: bool, pub db_kind: Option<DbKind>, pub connection_id: Option<Uuid>, pub description: Option<String>,
    pub sort_order: i64, pub created_at: String, pub updated_at: String }
pub struct SavedQueriesSnapshot { pub folders: Vec<SavedFolder>, pub queries: Vec<SavedQuery> }
pub struct EditorTabState { pub id: Uuid, pub position: u32, pub title: String, pub connection_id: Option<Uuid>,
    pub database: Option<String>, pub schema: Option<String>, pub text: String, pub file_path: Option<String>,
    pub cursor: u32, pub auto_commit: bool }
// dbstudio-store erd.rs / presets.rs
pub struct ErdLayout { pub id: Uuid, pub connection_id: Uuid, pub name: String, pub scope: ObjectPath,
    pub tables: serde_json::Value, pub options: serde_json::Value, pub viewport: Option<serde_json::Value>,
    pub snapshot: serde_json::Value, pub created_at: String, pub updated_at: String }
pub struct ErdLayoutSummary { pub id: Uuid, pub name: String, pub scope: ObjectPath, pub table_count: u32, pub updated_at: String }
pub struct ExportPreset { pub id: Uuid, pub name: String, pub connection_id: Option<Uuid>, pub spec: serde_json::Value,
    pub created_at: String, pub updated_at: String }
// dbstudio-export
pub struct ObjectMeta { pub path: Option<ObjectPath>, pub name: String, pub columns: Vec<ColumnMeta>,
    pub details: Option<TableModel>, pub estimated_rows: Option<u64> }
// dbstudio-backup
#[serde(rename_all = "camelCase")] pub enum RunKind { Backup, Restore, Verify }
#[serde(rename_all = "camelCase")] pub enum RunTrigger { Manual, Schedule, CatchUp, Cli }
#[serde(rename_all = "camelCase")] pub enum RunStatus { Running, Succeeded, Failed, Cancelled, Interrupted }
#[serde(rename_all = "camelCase")] pub enum ToolSource { Configured, Path, KnownDir }
pub struct BackupOutcome { pub files: Vec<String>, pub size_bytes: u64, pub checksum: String, pub objects: u32, pub rows: u64 }
pub struct RestoreOutcome { pub objects: u32, pub rows: u64, pub errors: Vec<RowError> }
```

### 1.15 `Capabilities` — cờ mới và giá trị

Cờ thêm (bool, `#[serde(default)]`): `leases`, `manual_transactions`, `query_params`, `explain_analyze_writes`, `document_store`, `key_value_store`, `ddl_generation`, `schema_model`, `schema_inference`, `table_stream`, `bulk_insert`, `native_backup`, `paged_tree`, `read_only_session`, `ssh_tunnel`.

| Cờ | PG | MySQL/MariaDB | MSSQL | Oracle | MongoDB | Redis |
|---|---|---|---|---|---|---|
| sql | ✓ | ✓ | ✓ | ✓ | — | — |
| transactions | ✓ | ✓ | ✓ | ✓ | ✓ (replica set) | ✓ (MULTI) |
| leases | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| manual_transactions | ✓ | ✓ | ✓ | ✓ | ✓ (kiểm lúc chạy) | — |
| query_params | ✓ (`:n`,`$1`) | ✓ (`:n`,`?`) | ✓ (`:n`,`@p`) | ✓ (`:n`,`:1`) | — | — |
| explain | ✓ | ✓ | ✓ | ✓ | ✓ | — |
| explain_analyze | ✓ | ✓ (8.0.18+/MariaDB) | ✓ | ✓ (cần quyền V$) | ✓ | — |
| explain_analyze_writes | ✓ | — | ✓ | ✓ | ✓ | — |
| foreign_keys | ✓ | ✓ | ✓ | ✓ | — | — |
| schema_model | ✓ | ✓ | ✓ | ✓ | ✓ | — |
| schema_inference | — | — | — | — | ✓ | — |
| ddl_generation | ✓ | ✓ | ✓ | ✓ | ✓ (index/validator) | — |
| table_stream | ✓ | ✓ | ✓ | ✓ | ✓ | — |
| bulk_insert | ✓ | ✓ | ✓ | ✓ | ✓ | — |
| logical_backup | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| native_backup | ✓ pg_dump | ✓ mysqldump | ✓ BACKUP DATABASE | ✓ expdp | ✓ mongodump | ✓ redis-cli --rdb |
| data_editing | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| document_store | — | — | — | — | ✓ | — |
| key_value_store | — | — | — | — | — | ✓ |
| paged_tree | — | — | — | — | — | ✓ |
| read_only_session | ✓ | ✓ | — | — | — | — |
| cancel_query | ✓ | ✓ | ✓ (fork) | ✓ | ✓ | ✓ (CLIENT KILL) |
| ssh_tunnel | ✓ | ✓ | ✓ | ✓ | ✓ (`directConnection`) | ✓ |
| multiple_databases / schemas | ✓/✓ | ✓/— | ✓/✓ | —/✓ | ✓/— | ✓/— |
| object_search | ✓ | ✓ | ✓ | ✓ | ✓ | — |

Cập nhật `planned_for`: Oracle `explain_analyze=true`, `logical_backup=true`; MSSQL `logical_backup=true`; Redis `logical_backup=true`, `cancel_query=true`. Nơi nào `read_only_session=false` → UI hiển thị "read-only chỉ được chặn ở ứng dụng".

`DriverStatus::Unavailable { reason: UnavailableReason }` với `enum UnavailableReason { NativeClientMissing, NativeClientTooOld }` (`Copy`); chi tiết/hướng dẫn qua command `driver_diagnostics`.

---

## 2. Crate mới

### 2.1 `dbstudio-sql` (thuần Rust; deps: `dbstudio-core`, `sqlparser = "0.64"`)

```rust
pub mod lexer;        // Token { kind, range } per dialect: strings ('..', E'..', N'..', q'[..]'), $tag$, `..`, [..],
                      // "..", --, #, /* */ (nested for PG), DELIMITER (MySQL), GO lines (MSSQL), "/" lines (Oracle)
pub mod split;        // fn split(d: ScriptDialect, text: &str) -> Vec<TextRange>
                      // streaming: struct StreamSplitter { fn push(&mut self, chunk: &str) -> Vec<String>; fn finish() }
pub mod params;       // fn find_params(d, text) -> Vec<ParamRef>; fn bind(d, text, &[ParamValue]) -> Result<BoundScript>
pub mod classify;     // fn analyze(d, text) -> ScriptAnalysis  (sqlparser first, keyword fallback)
pub mod guard;        // Policy, Verdict, fn check(&Policy, &ScriptAnalysis) -> Verdict
pub mod dialect;      // struct Dialect: quote_ident, literal(&Value), literal_text(&str), type_for(ValueKind)
pub mod ddl;          // fn create_table(d, name, &[NewColumn]) -> String (import "bảng mới")
pub mod mongo_shell;  // parse(text) -> Vec<ShellStatement> (AST, no bson dep); classify
pub mod redis_cli;    // tokenize_line(line) -> Vec<Vec<u8>>; COMMANDS table; classify
pub mod vocabulary;   // fn vocabulary(d) -> Vocabulary { keywords, functions, commands } (editor hints)

pub struct BoundScript { pub text: String, pub map: OffsetMap }
impl OffsetMap { pub fn to_original(&self, pos: u32) -> u32; }
pub enum Verdict { Allow, Deny(Error), Confirm(Vec<DestructiveStatement>) }
pub struct Policy { pub read_only: bool, pub production: bool, pub confirm_text: String }
```

Quy tắc chính:
- **Tách câu**: PG (giữ tương đương splitter `crates/drivers/postgres/src/sql.rs`, kể cả `BEGIN ATOMIC`), MySQL (`;`, `DELIMITER xx`), MSSQL (batch theo dòng `GO [n]`; trong batch chia câu bằng `sqlparser`, thất bại → cả batch là một câu), Oracle (`;` cho SQL; khối `DECLARE|BEGIN|CREATE [OR REPLACE] (PROCEDURE|FUNCTION|PACKAGE [BODY]|TRIGGER|TYPE [BODY])` kết thúc ở dòng chỉ có `/` hoặc hết văn bản; bỏ lệnh SQL*Plus `SET SERVEROUTPUT`, `PROMPT`, `SPOOL` → `Session` class).
- **Tham số**: PG `:name` (không phải `::`, `a[1:2]`), `$n` (ngoài dollar-quote); MySQL `:name`, `?` (không trong `:=`); MSSQL `:name`, `@p` chỉ khi không có `DECLARE @p` trong văn bản và không phải `@@`; Oracle `:name`/`:1`, **bỏ qua** `:new`/`:old`/`:parent` và mọi bind trong câu `CREATE ...`. Không bao giờ trong chuỗi/comment/định danh có ngoặc. Literal: PG `'..'` (nhân đôi `'`, `standard_conforming_strings=on`), MySQL `'..'` + escape `\` khi `backslash_escapes`, MSSQL `N'..'`, Oracle `'..'` (dài > 4000 → `TO_CLOB(...)||...`).
- **Phân loại phá huỷ**: `DROP *`, `TRUNCATE`, `DELETE`/`UPDATE` không `WHERE` (cả `WHERE 1=1`/`WHERE TRUE` → coi là không có), `ALTER ... DROP`, MySQL `DROP DATABASE`; Mongo `drop`, `dropDatabase`, `deleteMany({})`, `remove({})`, `updateMany({}, ...)`; Redis `FLUSHDB`, `FLUSHALL`. Không parse được → `Unknown` (guard coi là ghi; với prod coi `Unknown` không phá huỷ nhưng `Procedural`/`Unknown` có từ khoá `DROP|TRUNCATE` ở bất kỳ đâu → `Drop`/`Truncate` để an toàn).
- **Ngữ pháp Mongo shell** (hỗ trợ — không chạy JavaScript):
```
script   := stmt ((';' | NEWLINE)+ stmt)*
stmt     := 'use' IDENT | 'show' ('dbs'|'databases'|'collections'|'tables')
          | 'db' coll '.' cmethod '(' args? ')' chain*
          | 'db' '.' dbmethod '(' args? ')'
coll     := '.' IDENT | '[' STRING ']' | '.getCollection(' STRING ')'
cmethod  := find|findOne|aggregate|countDocuments|estimatedDocumentCount|distinct
          | insertOne|insertMany|updateOne|updateMany|replaceOne|deleteOne|deleteMany
          | findOneAndUpdate|findOneAndReplace|findOneAndDelete|bulkWrite
          | createIndex|createIndexes|dropIndex|dropIndexes|getIndexes|drop|renameCollection|stats
dbmethod := runCommand|adminCommand|getCollectionNames|createCollection|createView|dropDatabase|stats
chain    := '.' (sort|limit|skip|project|projection|hint|collation|maxTimeMS|batchSize|comment
          | count|toArray|pretty|explain) '(' args? ')'
args     := value (',' value)* ','?
value    := relaxed EJSON | shell literal:
            ObjectId("hex") ISODate("..") new Date(..) NumberInt(..) NumberLong(..) NumberDecimal("..")
            UUID("..") BinData(n,"b64") Timestamp(t,i) MinKey() MaxKey() /re/flags
            unquoted keys, 'single' or "double" strings, true false null, trailing commas, // and /* */ comments
```
  AST: `ShellStatement { range, kind: ShellKind, collection: Option<String>, method: String, args: Vec<ShellValue>, chain: Vec<(String, Vec<ShellValue>)> }`, `ShellValue` = cây JSON có biến thể cho literal shell. Driver Mongo đổi `ShellValue → bson::Bson`.
- **Redis**: tách theo dòng; token như redis-cli (`"..."` có escape `\n \t \" \\ \xHH`, `'...'` nguyên văn); bảng `COMMANDS: &[CommandSpec { name, arity, flags: READ|WRITE|ADMIN|DANGEROUS|BLOCKING|PUBSUB, summary }]` (từ tài liệu Redis 7.4); lệnh lạ → `Unknown`. `SELECT n` → `Session`; `SUBSCRIBE/PSUBSCRIBE/MONITOR` → driver từ chối (`Unsupported`).

### 2.2 `dbstudio-plan` (thuần, deps: `dbstudio-core`, `serde_json`, `quick-xml = "0.42"`)

```rust
pub fn parse(raw: &RawPlan) -> Result<PlanTree, PlanError>;
pub fn analyze(tree: &mut PlanTree, rules: &PlanRules);      // fills warnings + hot_path + self_*
pub fn parse_and_analyze(raw: &RawPlan, rules: &PlanRules) -> Result<PlanTree, PlanError>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanTree {
    pub format: PlanFormat,
    pub analyzed: bool,
    pub root: PlanNode,          // multi-root sources (MySQL union, Oracle multi-stmt) get a synthetic "Result" root
    pub totals: PlanTotals,
    pub warnings: Vec<PlanWarning>,
    pub hot_path: Vec<u32>,      // node ids from root following max inclusive time (else cost)
    pub node_count: u32,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanNode {
    pub id: u32,                         // pre-order index, root = 0
    pub op: String,                      // native label: "Seq Scan", "Clustered Index Seek", "TABLE ACCESS FULL", "IXSCAN"
    pub kind: PlanNodeKind,
    pub object: Option<PlanObject>,
    pub condition: Option<String>,       // filter / index cond / join cond summary
    pub est_startup_cost: Option<f64>,
    pub est_cost: Option<f64>,           // inclusive (subtree) total cost
    pub est_rows: Option<f64>,           // per execution (per loop)
    pub actual_rows: Option<f64>,        // per loop average
    pub actual_rows_total: Option<f64>,  // actual_rows * loops
    pub actual_time_ms: Option<f64>,     // inclusive, total over loops
    pub loops: Option<u64>,
    pub self_cost: Option<f64>,          // computed by analyze()
    pub self_time_ms: Option<f64>,
    pub children: Vec<PlanNode>,
    pub extra: BTreeMap<String, String>, // raw attributes flattened to text ("Sort Method": "external merge")
    pub raw: RawRef,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PlanNodeKind {
    FullScan, IndexScan, IndexOnlyScan, IndexSeek, KeyLookup,
    NestedLoopJoin, HashJoin, MergeJoin, Sort, Aggregate, HashAggregate, Window, Limit, Filter,
    Materialize, Subquery, Union, Gather, Modify, Fetch, Result, Other,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanObject { pub schema: Option<String>, pub name: String, pub alias: Option<String>, pub index: Option<String> }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum RawRef { JsonPointer { pointer: String }, XmlNode { node_id: String }, Lines { start: u32, end: u32 }, Row { id: u32 }, None }
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanTotals { pub total_cost: Option<f64>, pub planning_time_ms: Option<f64>, pub execution_time_ms: Option<f64>, pub rows: Option<f64> }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanWarning { pub node_id: u32, pub rule: PlanRule, pub severity: Severity, pub params: BTreeMap<String, String> }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PlanRule { FullScanLargeTable, Misestimate, SpillToDisk, NestedLoopManyRows }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Severity { Info, Warning, Critical }
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PlanRules { pub large_table_rows: f64, pub misestimate_factor: f64, pub misestimate_min_rows: f64, pub nested_loop_rows: f64 }
// default: 100_000, 10.0, 100.0, 10_000
```

Quy tắc PLAN-04: `FullScanLargeTable` khi `kind == FullScan` và max(`actual_rows_total`, `est_rows`) ≥ `large_table_rows`; `Misestimate` khi có `actual_rows` và max(a,e)/max(min(a,e),1) ≥ `misestimate_factor` và max(a,e) ≥ `misestimate_min_rows` (Critical khi ≥ 100×); `SpillToDisk` theo dấu hiệu nguồn (PG `Sort Method: external*`, `Disk:`, Hash `Batches > 1`; MSSQL `SpillToTempDb`/`SortSpillDetails`/`HashSpillDetails`; Oracle `TEMPSEG_SIZE`/`LAST_TEMPSEG_SIZE > 0`; Mongo `usedDisk:true`; MariaDB `r_temp_table`/`filesort` có `r_used_priority_queue=false` và `r_output_rows` lớn → Info); `NestedLoopManyRows` khi `kind == NestedLoopJoin` và `loops` của con trong ≥ `nested_loop_rows`.
Chuẩn hoá theo nguồn: PG theo JSON chuẩn; MSSQL `RelOp` (`EstimateRows` mỗi lần thực thi, `RunTimeCountersPerThread` cộng các thread, `ActualExecutions` → loops, `EstimatedTotalSubtreeCost` → `est_cost`); Oracle `ID/PARENT_ID` dựng cây, `CARDINALITY` → est_rows, `LAST_OUTPUT_ROWS/LAST_STARTS`, `LAST_ELAPSED_TIME` (µs) → ms; Mongo `winningPlan`/`executionStages` (`nReturned`, `executionTimeMillisEstimate`, `works`, `docsExamined` vào `extra`); MySQL JSON v1 `query_block` lồng (`nested_loop`, `table`, `cost_info.prefix_cost` → `est_cost`), tree text parse theo thụt lề (`-> ... (cost=.. rows=..) (actual time=a..b rows=r loops=l)`).

### 2.3 `dbstudio-export` (deps: core, `dbstudio-sql`, `rust_xlsxwriter = "0.102"`, `calamine = "0.38"`, `csv = "1.4"`, `zip = "9.5"`, `tokio`, `serde_json`)

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportSpec {
    pub connection_id: Uuid,
    pub source: ExportSource,
    pub content: ExportContent,
    pub format: ExportFormat,
    pub destination: ExportDestination,
    #[serde(default)] pub values: ValueFormat,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ExportSource {
    Database { path: ObjectPath },
    Schema { path: ObjectPath },
    Tables { paths: Vec<ObjectPath> },
    Query { text: String, #[serde(default)] database: Option<String> },
    Keys { database: ObjectPath, pattern: String },          // Redis → JSON only
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ExportContent { Schema, Data, Both }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ExportFormat {
    Sql(SqlOptions), Xlsx(XlsxOptions), Csv(CsvOptions), Json(JsonOptions),
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SqlOptions {
    pub ddl: DdlOptions, pub insert_batch_rows: u32 /*100*/, pub max_statement_bytes: u32 /*1 MiB*/,
    pub wrap_in_transaction: bool, pub disable_fk_checks: bool, pub include_column_names: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct XlsxOptions { pub data_dictionary: bool, pub header: bool, pub freeze_header: bool, pub max_cell_chars: u32 /*32767*/ }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CsvOptions {
    pub delimiter: char /*','*/, pub quote: char /*'"'*/, pub quote_all: bool, pub header: bool,
    pub encoding: TextEncoding /*Utf8Bom*/, pub line_ending: LineEnding /*Crlf*/, pub null_text: String /*""*/,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)] #[serde(rename_all = "camelCase")]
pub enum TextEncoding { Utf8, Utf8Bom }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)] #[serde(rename_all = "camelCase")]
pub enum LineEnding { Lf, Crlf }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct JsonOptions { pub layout: JsonLayout /*Array*/, pub extended_json: ExtendedJson /*Off*/, pub pretty: bool, pub fields: Vec<String> }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)] #[serde(rename_all = "camelCase")]
pub enum JsonLayout { Array, Ndjson }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)] #[serde(rename_all = "camelCase")]
pub enum ExtendedJson { Off, Relaxed, Canonical }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportDestination {
    pub path: String,                  // file, or .zip when several files are produced
    #[serde(default)] pub overwrite: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ValueFormat { pub binary: BinaryFormat /*Base64*/, pub flatten_fields: Vec<String> /* Mongo CSV/XLSX dotted paths; empty = top-level keys of first 1000 docs */ }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)] #[serde(rename_all = "camelCase")]
pub enum BinaryFormat { Base64, Hex }

pub struct ExportContext {
    pub session: Arc<dyn Session>,
    pub kind: DbKind,
    pub dialect: ScriptDialect,
    pub reporter: Arc<dyn JobReporter>,
}
pub async fn run_export(ctx: ExportContext, spec: ExportSpec) -> Result<ExportReport>;
#[derive(Debug, Clone, Serialize)] #[serde(rename_all = "camelCase")]
pub struct ExportReport { pub files: Vec<String>, pub objects: u32, pub rows: u64, pub bytes: u64, pub warnings: Vec<String> }

/// Writers are ResultSinks so query results stream straight to disk.
pub trait TableWriter: ResultSink { fn begin_object(&mut self, meta: &ObjectMeta) -> Result<()>; fn end_object(&mut self) -> Result<()>; fn finish(self: Box<Self>) -> Result<Vec<PathBuf>>; }

// ---- Import ----
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportSpec {
    pub connection_id: Uuid,
    pub file: String,
    pub format: ImportFormat,
    pub target: ImportTarget,
    #[serde(default)] pub mapping: Vec<ColumnMapping>,   // ignored for Sql
    #[serde(default)] pub on_conflict: Option<OnConflict>,
    pub mode: ImportMode,
    #[serde(default)] pub max_errors: u32,               // 0 = stop at first error
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ImportFormat {
    Csv { delimiter: char, quote: char, header: bool, encoding: TextEncoding },
    Xlsx { sheet: String, header: bool },
    Json { layout: JsonLayout },
    Sql { continue_on_error: bool },
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ImportTarget {
    Existing { table: ObjectPath },
    New { parent: ObjectPath, name: String, columns: Vec<NewColumn> },
    Script { #[serde(default)] database: Option<String> },   // Sql format
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewColumn { pub name: String, pub type_name: String, pub nullable: bool, pub primary_key: bool }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ColumnMapping { pub source: String /* header name or "#3" */, pub target: String, #[serde(default)] pub transform: Transform }
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Transform { #[default] None, Trim, EmptyAsNull, TrimEmptyAsNull }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ImportMode { SingleTransaction, Batches { rows: u32 } }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportPreviewRequest { pub file: String, pub format: ImportFormat, #[serde(default)] pub rows: u32 /*50*/ }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportPreview {
    pub columns: Vec<String>, pub rows: Vec<Vec<Value>>, pub sheets: Vec<String>,
    pub detected_delimiter: Option<char>, pub suggested_kinds: Vec<ValueKind>,
    pub estimated_rows: Option<u64>, pub statements_preview: Vec<StatementInfo>, // Sql: first 50
}
pub async fn preview_import(req: ImportPreviewRequest) -> Result<ImportPreview>;
pub async fn run_import(ctx: ExportContext, spec: ImportSpec) -> Result<ImportReport>;
#[derive(Debug, Clone, Serialize)] #[serde(rename_all = "camelCase")]
pub struct ImportReport { pub rows_read: u64, pub inserted: u64, pub updated: u64, pub skipped: u64, pub errors: Vec<RowError> }
#[derive(Debug, Clone, Serialize)] #[serde(rename_all = "camelCase")]
pub struct RowError { pub line: u64, pub message: String }   // 1-based line/row/statement number in the file
```

Quy tắc:
- **Phân việc driver ↔ export**: driver cung cấp `generate_ddl`, `Lease::stream_table`, `Lease::bulk_insert`, `schema_model` (data dictionary); export lo định dạng, nén zip, thứ tự, tiến độ, file tạm. Literal INSERT dùng `dbstudio_sql::Dialect::literal` (cùng dialect nguồn — không chuyển dialect).
- **Một snapshot**: export nhiều bảng mở một lease, `begin(TxOptions{read_only:true, isolation: RepeatableRead | Snapshot (MSSQL nếu bật) | Serializable read-only (Oracle)})`, stream lần lượt; driver không có `stream_table` → fallback `fetch_table` từng trang 50.000 dòng (có khoá).
- **File**: ghi vào `<dir>/.<name>.dbstudio-tmp-<jobid>`, `fsync`, đổi tên khi xong; huỷ/lỗi → xoá. Nhiều bảng CSV/JSON → zip (`ZipWriter`, mỗi bảng một entry, stream). XLSX dùng `Workbook::add_worksheet_with_constant_memory()`; tách sheet ở 1.048.575 dòng dữ liệu (`orders`, `orders (2)`…), tên sheet ≤ 31 ký tự và duy nhất; sheet `Data dictionary` khi `content != Data`.
- **Giá trị** (EXPORT-08): NULL → SQL `NULL`/CSV `null_text`/JSON `null`/XLSX ô trống; bytes → base64/hex (SQL dùng literal nhị phân của dialect); ngày giờ ISO 8601 như `Value`; số lớn/decimal → chuỗi trong JSON, ô text trong XLSX; `Ext` → `text` (JSON EJSON khi bật).
- **Import**: SQL script chạy qua `StreamSplitter` trên một lease, mỗi câu qua guard (read-only chặn; prod: quét trước cả file bằng lexer, có câu phá huỷ → `ConfirmationRequired` trước khi tạo job); lỗi câu `n` → dừng hoặc ghi `RowError{line}` rồi tiếp (`continue_on_error`). CSV/XLSX/JSON: đọc stream, ánh xạ cột, giá trị đi dạng `EditValue::Text` (DB tự ép kiểu) trừ JSON có kiểu; `Batches{rows}` = commit mỗi lô; lô lỗi và `max_errors>0` → chạy lại lô từng dòng để tìm dòng hỏng (ghi `RowError`). `ImportTarget::New` sinh `CREATE TABLE` bằng `dbstudio_sql::ddl::create_table` và chạy trước.

### 2.4 `dbstudio-backup` (deps: core, export, `dbstudio-store`, `dbstudio-sql`, `tar = "0.4"`, `flate2 = "1.1"`, `zstd = "0.14"`, `aes-gcm = "0.11"`, `argon2 = "0.6"`, `sha2 = "0.11"`, `croner = "4"`, `chrono`, `fd-lock = "4"`, `tokio` (process))

**Định dạng file tích hợp `.dbsbak` (v1)**:
```
"DBSBAK1\0" | u32 LE header_len | header JSON (clear) | payload
header = { formatVersion:1, compression:"none"|"gzip"|"zstd", encryption:null | { alg:"aes-256-gcm-chunked",
           kdf:"argon2id", salt, m:65536, t:3, p:1, noncePrefix, chunkSize:1048576 }, summary?: Preamble /* only when NOT encrypted */ }
payload = encrypt?( compress( tar( preamble.json, schema/pre.sql, data/<obj>.sql|.ndjson ..., schema/post.sql, manifest.json ) ) )
```
- Mã hoá theo khối 1 MiB: nonce = `noncePrefix(7B) ‖ counter u32 BE ‖ last(1B)`, AAD = SHA-256(header). Sai passphrase hay file bị sửa → `Error::Integrity`.
- `preamble.json` (đầu): nguồn (kind, host, database, server version), thời điểm, engine, danh sách đối tượng, phạm vi hỗ trợ ("không gồm: role/grant, extension…"). `manifest.json` (cuối): từng entry `{path, sha256, bytes, rows}`, tổng, `complete: true`. Kiểm toàn vẹn = đọc tuần tự, so hash từng entry với manifest (BACKUP-14). Xem trước restore chỉ cần giải mã/giải nén tới hết `preamble.json`.
- Dữ liệu: SQL DB → INSERT theo dialect nguồn (dùng lại `dbstudio-export` writer SQL); Mongo → canonical EJSON NDJSON + `indexes.json`; Redis → NDJSON `{key, type, ttlMs, value}` theo kiểu (không dùng `DUMP` vì phụ thuộc phiên bản RDB).

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupSpec {
    pub connection_id: Uuid,
    pub targets: Vec<ObjectPath>,                    // databases or objects (BACKUP-01)
    pub engine: BackupEngine,
    #[serde(default)] pub compression: Compression,
    #[serde(default)] pub encrypted: bool,           // passphrase from keychain (job) or dialog (manual)
    pub destination: Destination,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum BackupEngine {
    Logical { #[serde(default = "content_both")] content: ExportContent, #[serde(default)] ddl: DdlOptions },
    Native { tool: NativeTool, #[serde(default)] options: BTreeMap<String, String> /* allow-listed keys per tool */ },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NativeTool { PgDump, PgRestore, Psql, MySqlDump, MySqlClient, MongoDump, MongoRestore,
                      Expdp, Impdp, RedisCli, MsSqlServerBackup /* T-SQL, no binary */ }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Compression { None, Gzip { level: u32 }, Zstd { level: i32 } } // impl Default => Zstd { level: 3 }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Destination {
    /// `file_template` placeholders: {connection} {database} {date:%Y%m%d-%H%M%S} {job}
    LocalDir { path: String, #[serde(default = "default_template")] file_template: String },
    /// MSSQL/Oracle native: directory on the SERVER (BACKUP DATABASE / DIRECTORY object).
    ServerDir { path: String },
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupJob {
    pub id: Uuid,
    pub name: String,
    pub enabled: bool,
    pub spec: BackupSpec,
    pub schedule: Option<Schedule>,
    #[serde(default)] pub retention: Retention,
    #[serde(default)] pub missed_run: MissedRunPolicy,
    #[serde(default)] pub notify: NotifyPolicy,
    pub last_run_at: Option<String>,
    pub next_run_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Schedule { pub cron: String /* 5 fields, croner */, pub timezone: ScheduleTz }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)] #[serde(rename_all = "camelCase")]
pub enum ScheduleTz { Local, Utc }
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Retention { pub keep_last: Option<u32>, pub max_age_days: Option<u32> }
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MissedRunPolicy { #[default] RunOnce, Skip }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NotifyPolicy { pub on_success: bool /*true*/, pub on_failure: bool /*true*/ }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupRun {
    pub id: Uuid, pub job_id: Option<Uuid>, pub job_name: String, pub connection_id: Option<Uuid>,
    pub kind: RunKind /* backup|restore|verify */, pub trigger: RunTrigger /* manual|schedule|catchUp|cli */,
    pub engine: String, pub status: RunStatus /* running|succeeded|failed|cancelled|interrupted */,
    pub started_at: String, pub finished_at: Option<String>, pub duration_ms: Option<u64>,
    pub file_path: Option<String>, pub size_bytes: Option<u64>, pub checksum: Option<String>,
    pub objects: Option<u32>, pub rows: Option<u64>, pub error: Option<JobError>, pub log_tail: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreSpec {
    pub file: String,
    pub target_connection_id: Uuid,
    pub target_database: Option<String>,             // None = same name as source
    #[serde(default)] pub create_database: bool,
    #[serde(default)] pub clean: bool,               // drop existing objects first (destructive => confirm)
    #[serde(default)] pub objects: Vec<String>,      // subset (preamble names); empty = all
    #[serde(default)] pub continue_on_error: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupInspection { pub header: serde_json::Value, pub preamble: Option<serde_json::Value>, pub encrypted: bool, pub native: Option<NativeTool> }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeToolInfo { pub tool: NativeTool, pub path: Option<String>, pub version: Option<String>, pub source: ToolSource /* configured|path|knownDir */, pub problem: Option<String> }

pub async fn run_backup(ctx: BackupContext, spec: &BackupSpec, passphrase: Option<&Secret>) -> Result<BackupOutcome>;
pub async fn inspect(path: &Path, passphrase: Option<&Secret>) -> Result<BackupInspection>;
pub async fn verify(path: &Path, passphrase: Option<&Secret>, reporter: &dyn JobReporter) -> Result<()>;
pub async fn run_restore(ctx: BackupContext, spec: &RestoreSpec, passphrase: Option<&Secret>) -> Result<RestoreOutcome>;
pub fn detect_tools(overrides: &BTreeMap<NativeTool, String>) -> Vec<NativeToolInfo>;
pub fn apply_retention(job: &BackupJob, runs: &[BackupRun], now: DateTime<Utc>) -> Vec<PathBuf>; // files to delete
pub struct Scheduler<C: Clock> { /* pure; next_due(job, last_scheduled_for, now) ; due_jobs(...) ; catch_up(...) */ }
pub struct BackupContext {
    pub session: Arc<dyn Session>, pub config: ConnectionConfig, pub secret: Option<Secret>,
    pub tunnel_addr: Option<SocketAddr>, pub reporter: Arc<dyn JobReporter>,
}
```

Native tool: dò theo thứ tự đường dẫn cấu hình (`settings` key `native_tools`) → `PATH` → thư mục biết trước (`/opt/homebrew/opt/libpq/bin`, `/Applications/Postgres.app/Contents/Versions/*/bin`, `/usr/lib/postgresql/*/bin`, `C:\Program Files\PostgreSQL\*\bin`, `C:\Program Files\MySQL\*\bin`, MongoDB Database Tools, Instant Client dir…), đọc `--version`; `pg_dump` major < server major → `problem`. **Secret không bao giờ nằm trên dòng lệnh**: PG `PGPASSWORD` env + `PGSSLMODE/PGSSLROOTCERT`; MySQL file `--defaults-extra-file` tạm quyền 0600 (xoá sau); mongodump `--config` YAML tạm 0600; expdp `userid` qua stdin/parfile tạm 0600; Redis `REDISCLI_AUTH` env. Tham số chỉ dựng bằng code từ spec (không nhận chuỗi tham số tự do). Qua SSH: tool nối tới `tunnel_addr`. MSSQL: `BACKUP DATABASE [db] TO DISK=N'<ServerDir>/<file>.bak' WITH COPY_ONLY, COMPRESSION, CHECKSUM, INIT` qua session (file ở server; retention chỉ áp được khi thư mục là ổ mount chung — UI báo). Redis: `redis-cli --rdb <file>` (tải RDB qua replication; cần quyền `SYNC`).

**Scheduler** (chạy trong `src-tauri/src/scheduler.rs`, logic trong crate): tick mỗi 30 s; khi khởi động: `backup_runs` còn `running` → `interrupted`; với mỗi job bật, nếu có mốc lịch trong `(last_scheduled_for, now]` → `RunOnce` chạy một lần (`trigger=catchUp`) hoặc `Skip`; mỗi job tối đa 1 lần chạy đồng thời, khoá file `<data_dir>/locks/backup-<jobId>.lock` (`fd-lock`, chia với CLI); xong → retention (chỉ xoá file nằm trong `backup_runs` của job đó, không glob), thông báo OS theo `notify`.

### 2.5 `dbstudio-cli` (binary, deps: core, store, backup, export, drivers (feature), `clap = "4.6"`)

`dbstudio-cli backup --job <id>` · `backup --list` · `restore --file F --connection ID [--database D] [--yes]` · `verify --file F` · `tools` · tuỳ chọn chung `--data-dir` (mặc định = `app_data_dir` của app: `<OS data dir>/dev.dbstudio.desktop`). Đọc secret từ cùng keychain (service `dev.dbstudio.desktop`); restore vào prod bắt buộc `--yes --confirm-text <db>`. Mã thoát: 0 ok, 1 lỗi, 2 sai tham số, 3 đang chạy (lock).

### 2.6 `dbstudio-ssh` (deps: core, `russh = "0.65"` (gồm `russh::keys`), tokio)

`Tunnel::open(cfg: &SshConfig, secret: Option<&Secret>, target: (String, u16), known: &dyn KnownHosts) -> Result<Tunnel>`; `Tunnel::local_addr()`; drop → đóng. `trait KnownHosts { fn check(&self, host, port, key_type, fingerprint) -> HostKeyStatus; }` hiện thực bởi store.

---

## 3. Driver mới

Quy ước chung cho mọi driver mới: crate `crates/drivers/<db>` tên `dbstudio-driver-<db>`, feature `<db>` trong `src-tauri` (mặc định bật, trừ `oracle` — xem R2), integration test dưới `tests/` chỉ chạy khi `DBSTUDIO_IT=1`, biến môi trường cổng/mật khẩu theo `docker/compose.test.yml`. Driver phải: chấp nhận path có/không Folder, `fetch_table` thêm khoá vào ORDER BY, `raw_where` một biểu thức (chặn bằng lexer + kiểm phía server khi có), `cancel` id lạ → `Ok`, lỗi `Query.position` = char offset trong toàn văn bản, `Value` không mất mát.

Service Docker mới (cổng không trùng 54312/54316/33057/33068/33104/33106):

| Service | Image | Cổng host | Ghi chú |
|---|---|---|---|
| `mssql2022` | `mcr.microsoft.com/mssql/server:2022-latest` (`platform: linux/amd64`) | 14333 | `ACCEPT_EULA=Y`, `MSSQL_SA_PASSWORD=Dbstudio_test1!`; Apple Silicon chạy giả lập Rosetta (R1) |
| `oraclefree` | `gvenzl/oracle-free:23-slim-faststart` | 15213 | `ORACLE_PASSWORD=dbstudio_test`, `APP_USER=dbstudio`, `APP_USER_PASSWORD=dbstudio_test` |
| `oracle-it` | build `docker/oracle-it/Dockerfile` từ `ghcr.io/oracle/oraclelinux9-instantclient:23` + rustup | — | `docker compose run --rm oracle-it cargo test -p dbstudio-driver-oracle` (volume repo + `target-oracle`) |
| `mongo44` | `mongo:4.4` | 27044 | standalone, auth `dbstudio/dbstudio_test` |
| `mongo7` | `mongo:7` | 27077 | replica set `rs0` 1 node (keyFile) + TLS bằng `docker/certs`; test dùng `directConnection=true` |
| `redis6` | `redis:6.2` | 63796 | ACL user `dbstudio` |
| `redis7` | `redis:7.4` | 63797 | TLS (`--tls-port`, cert `docker/certs`) |
| `valkey8` | `valkey/valkey:8` | 63798 | không TLS |
| `sshd` | `lscr.io/linuxserver/openssh-server` | 22022 | mật khẩu + key có passphrase; cùng network với các DB |

### 3.1 MSSQL — `crates/drivers/mssql`

- **Crate**: `tiberius = { version = "0.13", default-features = false, features = ["tds80", "rustls", "winauth", "tokio-util", "time"] }` qua **fork tối thiểu** (`[patch.crates-io] tiberius = { git = "<fork>", rev = "..." }`) bổ sung: `QueryItem::Info(Message)`/`QueryItem::Done{rows}` (token `INFO`, `DONE/DONEPROC/DONEINPROC`) và `Client::cancel_handle()` gửi gói attention. Kèm `tokio-util` (compat), `rustls 0.23`.
- **Xác thực**: SQL login (mặc định); `windows(DOMAIN\user, pass)` NTLM qua feature `winauth` (mọi OS); Integrated SSPI chỉ trên Windows (`AuthMethod::Integrated`, option `auth=integrated`); Azure AD token để sau (D-Q2). Option: `encrypt` theo `TlsMode`, `applicationName`, `pool_max`.
- **TLS**: `disable` → `EncryptionLevel::NotSupported`; `prefer` → `On` + `trust_cert()` (TDS không có fallback; nếu server yêu cầu thì vẫn mã hoá); `require` → `Required` + `trust_cert()`; `verifyCa`/`verifyFull` → `Required` + `trust_cert_ca(ca)` (không có CA → kho OS qua `rustls-native-certs`); `server_name` override cho SSH. `verifyCa` hành xử như `verifyFull` (ghi rõ).
- **Pool**: pool lười theo database (tối đa 4 + 2 catalog) như PG, kết nối bằng `config.database(db)`.
- **Cây**: Root → `Database` (`sys.databases` có `HAS_DBACCESS`) + `Folder roles` (server principals, khi đủ quyền) → `Database` → `Folder schemas` → `Schema` → `Folder tables|views|functions|procedures|sequences|synonyms|types` → `Table`/`View` → `Folder columns|indexes|triggers`. Path ví dụ `[Database app, Folder schemas, Schema dbo, Folder tables, Table orders, Folder columns, Column id]`.
- **Method**: `execute` tách batch `GO [n]` bằng `dbstudio_sql`, mỗi batch `simple_query`; nhiều result set → mỗi `Metadata` mở statement mới; `Info` → `on_message` (gồm PRINT, RAISERROR mức < 11); `Done{rows}` → `Affected`. Vị trí lỗi: TDS trả số dòng trong batch → đổi sang char offset của dòng đầu (batch offset + line). `fetch_table` `ORDER BY ... OFFSET n ROWS FETCH NEXT m ROWS ONLY` trong `SET TRANSACTION ISOLATION LEVEL READ COMMITTED` + `BEGIN TRAN ... ROLLBACK`; `raw_where` → lexer + `SET PARSEONLY ON` kiểm một câu. `object_ddl`/`generate_ddl` dựng từ catalog (`sys.columns/types/default_constraints/check_constraints/indexes/foreign_keys/identity_columns/computed_columns`) + `OBJECT_DEFINITION()` cho view/proc/func/trigger. `explain` dùng lease nội bộ: batch riêng `SET SHOWPLAN_XML ON`, batch câu lệnh, `SET SHOWPLAN_XML OFF`; ANALYZE như 1.10. `open_lease` (lease_kit; auto-commit mặc định; manual = `SET IMPLICIT_TRANSACTIONS ON`; trạng thái từ `@@TRANCOUNT`). `bulk_insert` dùng `Client::bulk_insert`. `schema_model` 4 truy vấn catalog. `search_objects` qua `sys.objects`. `read_only_session` không có (guard ứng dụng).
- **Kiểu → Value**: `bit`→Bool; `tinyint/smallint/int/bigint`→Int; `decimal/numeric/money/smallmoney`→Decimal (chuỗi đầy đủ thang); `real/float`→Float; `char/varchar/nchar/nvarchar/text/ntext/xml`→Text; `binary/varbinary/image/rowversion/timestamp`→Bytes; `uniqueidentifier`→Uuid; `date`→Date; `time(n)`→Time (đủ 7 chữ số); `datetime/datetime2/smalldatetime`→DateTime (đủ độ chính xác); `datetimeoffset`→Timestamp giữ offset gốc; `sql_variant`→theo kiểu bên trong; `geography/geometry/hierarchyid`→Bytes; `json` (2025)→Json; LOB > `maxLobBytes` → `Ext truncated*`.
- **Huỷ**: attention (fork); fallback khi chưa có fork: đóng socket kết nối đó (lease → `broken`).
- **Test**: `mssql2022`; tạo DB `dbstudio` trong test setup. Kiểm nhiều result set + PRINT, `GO 2`, huỷ `WAITFOR DELAY '00:01:00'` < 2 s, transaction thủ công, kiểu dữ liệu biên.
- **Rủi ro**: R1 (image amd64), R3 (fork tiberius), Windows auth ngoài Windows chỉ NTLM.

### 3.2 Oracle — `crates/drivers/oracle`

- **Crate**: `oracle = "0.6.3"` (ODPI-C biên dịch tĩnh trong crate, **nạp `libclntsh` động lúc chạy** → thiếu Instant Client không làm app crash). API đồng bộ → mỗi kết nối chạy trên một thread `spawn_blocking` riêng/actor; pool tự viết (tối đa 4) hoặc `oracle::pool::Pool`.
- **Khả dụng**: `OracleDriver::descriptor()` probe một lần (`OnceLock`) bằng `oracle::Version::client()` (sau `InitParams::oracle_client_lib_dir(dir)` nếu `AppSettings.oracleClientLibDir` có); lỗi `DPI-1047` → `DriverStatus::Unavailable { reason: NativeClientMissing }`; `connect` trả `Error::DriverUnavailable` kèm hướng dẫn; `driver_diagnostics("oracle")` trả thông điệp + link tải Instant Client theo OS/arch (macOS arm64 có từ 23ai).
- **Xác thực**: user/password; `database` = service name hoặc chuỗi Easy Connect (`host:port/service`); option `connect_as=sysdba|sysoper` (allow-list), `wallet_dir` (TLS/mTLS Autonomous). Kerberos/OS auth để sau.
- **TLS**: TCPS qua Easy Connect Plus (`tcps://host:port/service?ssl_server_dn_match=...&wallet_location=...`); CA PEM → wallet tự sinh không được (ODPI-C 23 hỗ trợ `ssl_server_cert_dn` và PEM wallet `ewallet.pem` trong `wallet_location`) → driver ghi `ewallet.pem` tạm từ `ca_cert_path`. `disable/prefer` = TCP; `require` = TCPS không kiểm DN (`ssl_server_dn_match=off`); `verifyCa`/`verifyFull` = TCPS + DN match.
- **Cây** (không có cấp Database; `multiple_databases=false`): Root → `Schema` (user hiện tại trước, sau đó các owner có đối tượng trong `ALL_OBJECTS`; cờ `options.show_all_schemas`) + `Folder roles` (khi đủ quyền) → `Schema` → `Folder tables|views|materializedViews|sequences|synonyms|functions|procedures|packages|types|triggers` → `Table` → `Folder columns|indexes|triggers`. Path `[Schema HR, Folder tables, Table EMPLOYEES, ...]`; tên giữ đúng hoa/thường, luôn quote `"..."`.
- **Method**: `execute` tách bằng `dbstudio_sql` (PL/SQL block giữ nguyên `;` bên trong, bỏ `/`); SQL bỏ `;` cuối; DBMS_OUTPUT luôn bật trên lease (`DBMS_OUTPUT.ENABLE(NULL)`), sau mỗi câu `GET_LINES` → `on_message`; `SET SERVEROUTPUT` bị bỏ qua có thông báo. Lỗi `ORA-xxxxx` có `offset` (ODPI `DPI error offset`) → char offset tuyệt đối. `fetch_table` `OFFSET n ROWS FETCH NEXT m ROWS ONLY` (12c+) trong `SET TRANSACTION READ ONLY`; `raw_where` lexer + `DBMS_SQL.PARSE` không có (DDL thực thi khi parse) → dùng lexer + bọc `SELECT ... WHERE (raw)` qua `prepare` (OCI parse lười; `stmt.describe` không thực thi). `object_ddl`/`generate_ddl` = `DBMS_METADATA.GET_DDL` với `SET_TRANSFORM_PARAM(SQLTERMINATOR=true, SEGMENT_ATTRIBUTES=false, STORAGE=false, CONSTRAINTS_AS_ALTER=true, REF_CONSTRAINTS=false)` + `GET_DEPENDENT_DDL('REF_CONSTRAINT'|'INDEX'|'COMMENT')` cho PostData; thứ tự theo `ALL_DEPENDENCIES`. `explain`: `EXPLAIN PLAN SET STATEMENT_ID=:id FOR ...` → đọc `PLAN_TABLE` thành JSON (`OraclePlanRows`), `extra.xplan` = `DBMS_XPLAN.DISPLAY(NULL, :id, 'ALL')`; ANALYZE như 1.10 (`DISPLAY_CURSOR(sql_id, child, 'ALLSTATS LAST')`). `cancel` = `Connection::break_execution()` từ thread khác. Lease: manual mặc định của Oracle; auto-commit = `conn.set_autocommit(true)`. `bulk_insert` = `Batch` (array DML). `schema_model` qua `ALL_TAB_COLUMNS`, `ALL_CONSTRAINTS`, `ALL_CONS_COLUMNS`, `ALL_COL_COMMENTS` (lọc owner).
- **Kiểu → Value** (lấy chuỗi từ OCI để không mất mát): `NUMBER(p,0)` p ≤ 18 → Int, còn lại `NUMBER` → Decimal; `BINARY_FLOAT/DOUBLE`→Float (NaN/Inf); `DATE`→DateTime (giây); `TIMESTAMP(n)`→DateTime (n chữ số); `TIMESTAMP WITH TIME ZONE`→Timestamp (offset hoặc `[Region]`); `WITH LOCAL TIME ZONE`→Timestamp (session `TIME_ZONE='UTC'`); `INTERVAL`→`Ext interval`; `CHAR/VARCHAR2/NCHAR/NVARCHAR2/LONG`→Text; `CLOB/NCLOB`→Text (≤ `maxLobBytes`, vượt → `Ext truncatedText`); `BLOB/RAW/LONG RAW`→Bytes; `ROWID/UROWID`→`Ext rowid`; `JSON` (21c)→Json; `BOOLEAN` (23ai)→Bool; `XMLTYPE`→Text; `VECTOR`→Text.
- **Test**: `oraclefree` + `oracle-it` (Linux có Instant Client; macOS có thể cài Instant Client arm64 và đặt `ORACLE_CLIENT_LIB_DIR`). Test thiếu client: chạy test ở host không có client, kỳ vọng `Unavailable` và không panic. Kiểm block `BEGIN..END;`, `CREATE OR REPLACE PROCEDURE ... /`, DBMS_OUTPUT, kiểu biên.
- **Phương án thuần Rust**: `oracle-rs` 0.1.7 (MIT/Apache, thin TNS, async) — mới, một người duy trì, chưa có bằng chứng về TLS/LOB/kiểu đầy đủ/cancel. **Khuyến nghị**: dùng `oracle` (ODPI-C) cho GĐ này; giữ toàn bộ code SQL/catalog trong module tách khỏi lớp kết nối (`conn.rs` sau trait nội bộ `OraConn`) để có thể thử `oracle-rs` sau feature flag mà không viết lại; đánh giá lại ở GĐ9.

### 3.3 MongoDB — `crates/drivers/mongodb`

- **Crate**: `mongodb = { version = "3.10", default-features = false, features = ["compat-3-3-0", "bson-3", "rustls-tls", "dns-resolver"] }`, `bson = "3.1"` (EJSON relaxed/canonical, Decimal128).
- **Kết nối/xác thực**: `ClientOptions` dựng từ profile: host/port, `options.srv=true` → `mongodb+srv://` (dùng DNS), `authSource` (mặc định `database` hoặc `admin`), `authMechanism` (SCRAM-SHA-256/1; X.509 khi có client cert; AWS/Kerberos để sau), `replicaSet`, `directConnection` (bắt buộc true khi SSH), `appName`. `CONN-13` dán URI: parse ở frontend? → **không**, parse ở Rust qua command `connection_parse_uri(uri) -> ConnectionProfile` (gói K) để không đưa mật khẩu sang frontend (mật khẩu trong URI chuyển thành `SecretChange::Set`).
- **TLS**: `disable` → tắt; `prefer` → thử TLS `allow_invalid_certificates=true`, lỗi bắt tay → thử plain; `require` → TLS + `allow_invalid_certificates`; `verifyCa` → CA + `allow_invalid_hostnames`; `verifyFull` → CA đầy đủ; client cert → `cert_key_file_path` (PEM gộp cert+key; driver ghi file tạm 0600 nếu hai file tách).
- **Cây**: Root → `Database` (`listDatabases nameOnly`; không có quyền → chỉ `config.database`) → `Folder collections` → `Collection` (detail `~12.3k docs` từ `estimatedDocumentCount`, lấy lười theo lô) → `Folder indexes` → `Index`; `Folder views` → `View`. `Folder` key mới `collections`. System collections ẩn trừ option.
- **`fetch_table`** (collection/view): `find(filter).sort(sort + {_id:1}).skip(offset).limit(limit).batch_size(n)`; một cột `ColumnMeta { name: "$document", type_name: "document", value_kind: Document }`; filters: `column` = đường dẫn chấm, toán tử → `$eq/$ne/$lt.../$regex` (escape, `i`), `IsNull` → `{f: null}` (khớp cả thiếu), giá trị: thử parse literal shell/EJSON (`123`, `true`, `ObjectId("..")`, `"x"`) rồi rơi về chuỗi; `raw_where` = văn bản filter EJSON relaxed / literal shell (một object). `count_table` = `countDocuments` (không filter → `estimatedDocumentCount`).
- **`execute`**: parse bằng `dbstudio_sql::mongo_shell`, chạy từng câu: `find/aggregate` stream cursor (`$document`), áp `max_rows`; ghi → `Affected` (matched/modified/inserted/deleted) + `on_message` chi tiết (insertedIds); `runCommand` → 1 dòng document; `use db` → đổi DB của lease; `show ...` → bảng 1 cột `name`. `comment` = query id để `killOp`.
- **BSON → Value**: Double→Float; String→Text; Document→Document (giữ thứ tự); Array→Array; Binary sub 0→Bytes, sub 4→Uuid, khác→`Ext binary`; ObjectId→`Ext objectId`; Bool→Bool; DateTime→Timestamp (`YYYY-MM-DDTHH:MM:SS.mmmZ`, ngoài phạm vi → `Ext dateMillis`); Null→Null; Regex→`Ext regex`; JS→`Ext javascript`; Int32→Int; Timestamp→`Ext bsonTimestamp`; Int64→`Ext int64`; Decimal128→Decimal; MinKey/MaxKey/Undefined/Symbol/DbPointer→`Ext`. **Value → BSON** ngược lại: `Int` → Int32 nếu vừa, ngược lại Int64; `Ext int64` → Int64; `Timestamp` → DateTime (quá ms → lỗi); `Decimal` → Decimal128; `Text` → String.
- **Khác**: `editability` = `_id` (view → `NotATable`); `apply_changes` Documents (1.8); `explain` = `explain` command với verbosity `queryPlanner`/`executionStats` cho find/aggregate/count/distinct/update/delete/findAndModify → `MongoExplain` (canonical EJSON); `schema_model` = `$sample` N (mặc định 1000) mỗi collection, cột = khoá cấp 1 (type hợp, tỉ lệ xuất hiện, `nullable` nếu thiếu ở một số doc), quan hệ suy luận `xxxId|xxx_id` kiểu ObjectId → collection `xxx`/`xxxs` (`inferred=true`); `generate_ddl` = `db.createCollection(name, {validator, capped...})` + `createIndex` (`language: MongoShell`); `open_lease` = `ClientSession` + DB hiện hành, transaction chỉ trên replica set/mongos; `stream_table`/`bulk_insert` (insertMany ordered / bulkWrite upsert theo `_id`).
- **Huỷ**: drop cursor + `killOp` theo `currentOp({comment: query_id})` qua kết nối admin.
- **Test**: `mongo44` (standalone: không transaction), `mongo7` (replica set + TLS); kiểm `ObjectId/Decimal128/Date/Int64` khứ hồi, find có filter/projection/sort, aggregate nhiều stage, sửa/thêm/xoá document, explain.
- **Rủi ro**: `$sample` trên collection lớn tốn; replica set thiếu → transaction/`atomic=false`.

### 3.4 Redis / Valkey — `crates/drivers/redis`

- **Crate**: `redis = { version = "1.8", default-features = false, features = ["tokio-comp", "tokio-rustls-comp", "tls-rustls-insecure", "streams", "acl"] }` (2.0 còn rc).
- **Kết nối**: `Client::build_with_tls` (CA/client cert từ file) hoặc `redis://`; username (ACL Redis 6+) + password; `database` = số db mặc định. Browse/đọc: một `MultiplexedConnection` mỗi db (tạo lười; không dùng `SELECT` trên kết nối chung). Lease = một `MultiplexedConnection` riêng (SELECT/MULTI chỉ ảnh hưởng lease đó).
- **TLS**: `disable` → `redis://`; `prefer` → thử `rediss://#insecure` rồi plain; `require` → `rediss://...#insecure`; `verifyCa`/`verifyFull` → `rediss://` + CA (không tách được kiểm tên host → `verifyCa` như `verifyFull`).
- **Cây** (`paged_tree`): Root → `Database` "0".."N-1" (`CONFIG GET databases`, bị cấm → 16; detail số khoá từ `INFO keyspace`; ẩn db rỗng trừ db hiện hành, option `show_empty_dbs`) → con lấy bằng `list_children_page`: `SCAN cursor MATCH <prefix><pattern|*> COUNT 1000` lặp tới khi đủ `limit` con khác nhau hoặc cursor 0; khoá có dấu phân cách sau prefix → `KeyPrefix` (name = prefix đầy đủ kèm dấu phân cách, `option delimiter` mặc định `:`), còn lại → `Key` (detail type + TTL lấy bằng pipeline `TYPE`/`PTTL`). `next_cursor` = `"<scan cursor>"` (UI khử trùng). `list_children` (không trang) trả tối đa 1000 con đầu.
- **`read_key`** (1.9): `TYPE`, `PTTL`, độ dài, `OBJECT ENCODING`, `MEMORY USAGE` (lỗi quyền → None); string `GETRANGE 0 max-1`; hash/set/zset `HSCAN/SSCAN/ZSCAN` (zset theo hạng: `ZRANGE start stop WITHSCORES` khi không có pattern); list `LRANGE`; stream `XRANGE (id + COUNT n` + `XINFO STREAM` (groups); RedisJSON `JSON.GET`.
- **`execute`** (console QUERY-15): mỗi dòng một lệnh (token redis-cli); kết quả: bulk/simple → cột `value` 1 dòng; mảng → cột `#`,`value`; lồng → `Value::Array`; nil → Null; integer → Int; lỗi → dừng, `Error::Query{position: đầu dòng}`. Từ chối `SUBSCRIBE/PSUBSCRIBE/SSUBSCRIBE/MONITOR` (`Unsupported`). `SELECT n` trên lease → cập nhật `LeaseState.database`.
- **Ghi**: `apply_changes` Keys (1.8); xoá theo pattern = job `KeyDelete` (`SCAN MATCH` + `UNLINK` lô 500) qua `keys_delete_pattern` (luôn phân loại `DeleteByPattern` → prod phải xác nhận).
- **Không hỗ trợ**: `object_ddl`, `table_details`, `fetch_table`, `explain`, `schema_model`, `generate_ddl` (mặc định `Unsupported`). Cluster/Sentinel để sau (descriptor không nhận option `cluster`).
- **Huỷ**: `CLIENT ID` khi mở lease; `cancel` → `CLIENT KILL ID <id>` qua kết nối khác; lease → `broken` (frontend tự mở lease mới, ghi chú SELECT db).
- **Test**: `redis6`, `redis7` (TLS), `valkey8`; tạo 1.000.000 khoá bằng pipeline trong test (`DBSTUDIO_IT_BIG=1` cho bài nặng), kiểm SCAN không dùng `KEYS` (bật `rename-command KEYS ""` trên `redis6` để chứng minh), đủ 6 kiểu, TTL, FLUSHDB cần xác nhận.

### 3.5 Bổ sung cho PG/MySQL (thuộc các gói B, D, E, F, G)

| Method | PostgreSQL | MySQL/MariaDB |
|---|---|---|
| `script_dialect` | `Postgres` | `MySql { mariadb, backslash_escapes = !sql_mode.contains("NO_BACKSLASH_ESCAPES") }` |
| `open_lease` | kết nối riêng (không qua pool). `pending_result` = cursor `DECLARE`; ở auto-commit driver mở `BEGIN` ngầm cho câu SELECT cuối bị cắt và `COMMIT` khi `discard`/lệnh kế tiếp (`state.transaction` vẫn báo `Idle`); ở manual, cursor nằm trong transaction của người dùng | actor giữ `query_iter` treo (TCP backpressure) |
| `explain` | `PostgresJson` | `MySqlJson`/`MySqlTree` (ANALYZE 8.0.18+), MariaDB `MariaDbJson` |
| `schema_model` | 3 truy vấn `pg_catalog` | 3 truy vấn `information_schema` |
| `generate_ddl` | tái dùng `ddl.rs` + tách FK/index ra PostData | `SHOW CREATE TABLE` rồi tách `CONSTRAINT ... FOREIGN KEY` thành `ALTER TABLE ADD` (PostData) |
| `stream_table`/`bulk_insert` | cursor / `COPY FROM STDIN` | `query_iter` / INSERT nhiều dòng |
| native | `pg_dump -Fc`/`pg_restore` | `mysqldump --single-transaction --routines --triggers` / `mysql` |

---

## 4. Command IPC mới (`src-tauri`)

Quy ước: tham số camelCase phía JS; mỗi command có `*_impl(&AppState, ...)`; lỗi là `Error` serialize như GĐ1. "Ch" = `Channel<QueryEvent>` theo quy tắc GĐ1 (resolve khi có cả kết quả và `statementEnd`). File Rust: `commands/{editor,changes,plan,erd,files,jobs,export,backup,keyvalue,history,background,ssh}.rs`.

| Command | Tham số (JS) | Trả về | Stream/Event | Gói |
|---|---|---|---|---|
| `script_analyze` | `sessionId, text` | `ScriptAnalysis` | — | B |
| `script_vocabulary` | `sessionId` | `Vocabulary` | — | B |
| `query_execute` | `sessionId, request: ExecuteRequest, onEvent` | `ExecuteResult` (`state=null`) | Ch | B |
| `lease_open` | `sessionId, options: LeaseOptions` | `LeaseInfo { leaseId, sessionId, state }` | — | B |
| `lease_close` | `leaseId` | `LeaseCloseReport` | — | B |
| `lease_execute` | `leaseId, request: ExecuteRequest, onEvent` | `ExecuteResult` | Ch | B |
| `lease_fetch_more` | `leaseId, queryId, maxRows, batchSize?, onEvent` | `ExecuteResult` | Ch | B |
| `lease_discard_result` | `leaseId` | `LeaseState` | — | B |
| `lease_cancel` | `leaseId, queryId` | `()` | — | B |
| `lease_begin` | `leaseId, options: TxOptions` | `LeaseState` | — | B |
| `lease_commit` / `lease_rollback` | `leaseId` | `LeaseState` | — | B |
| `lease_set_auto_commit` | `leaseId, autoCommit` | `LeaseState` | — | B |
| `lease_set_context` | `leaseId, context: LeaseContext` | `LeaseState` | — | B |
| `lease_state` | `leaseId` | `LeaseState` | event `lease://state` `{leaseId, state}` khi broken | B |
| `history_list` | `query: HistoryQuery` | `HistoryEntry[]` | — | B |
| `history_delete` | `ids: number[]` | `()` | — | B |
| `history_clear` | `connectionId?` | `number` (đã xoá) | — | B |
| `saved_queries_list` | — | `SavedQueriesSnapshot { folders, queries }` | — | B |
| `saved_query_save` / `saved_query_delete` | `query` / `id` | `SavedQuery` / `()` | — | B |
| `saved_folder_save` / `saved_folder_delete` | `folder` / `id` | `SavedFolder` / `()` | — | B |
| `editor_tabs_get` / `editor_tabs_set` | — / `tabs: EditorTabState[]` | `EditorTabState[]` / `()` | — | B |
| `sql_file_open` | — | `{ path, text } \| null` (dialog, ≤ 50 MB, UTF-8/BOM) | — | B |
| `sql_file_save` | `path?: string, text` | `string \| null` (path; null = huỷ dialog) | — | B |
| `table_editability` | `sessionId, path` | `Editability` | — | D |
| `changes_preview` | `sessionId, changes: ChangeSet` | `string[]` | — | D |
| `changes_apply` | `sessionId, changes: ChangeSet` | `ApplyResult` | — | D |
| `plan_explain` | `sessionId, leaseId?, request: ExplainRequest, rules?: PlanRules` | `PlanResult { raw: RawPlan, tree: PlanTree \| null, parseError: string \| null }` | — | E |
| `plan_parse` | `raw: RawPlan, rules?` | `PlanTree` | — | E |
| `plan_file_save` / `plan_file_open` | `raw` / — | `string \| null` / `RawPlan \| null` (`.dbplan.json`) | — | E |
| `schema_model` | `sessionId, request: SchemaModelRequest` | `SchemaModel` | — | F |
| `erd_layouts_list` | `connectionId` | `ErdLayoutSummary[]` | — | F |
| `erd_layout_get` / `erd_layout_save` / `erd_layout_delete` | `id` / `layout: ErdLayout` / `id` | `ErdLayout` / `ErdLayout` / `()` | — | F |
| `ddl_generate` | `sessionId, request: DdlRequest` | `DdlItem[]` | — | F |
| `file_pick_save` | `suggestedName, filters: FileFilter[]` | `string \| null` (ghi vào `PathGrants`) | — | G |
| `file_pick_open` | `filters, directory: boolean` | `string \| null` | — | G |
| `file_save_bytes` | `suggestedName, dataBase64, filters` | `string \| null` (ERD PNG/SVG/PDF…) | — | G |
| `export_start` | `spec: ExportSpec` | `JobId` | `job://progress` | G |
| `export_presets_list` / `export_preset_save` / `export_preset_delete` | `connectionId?` / `preset` / `id` | `ExportPreset[]` / `ExportPreset` / `()` | — | G |
| `import_preview` | `request: ImportPreviewRequest` | `ImportPreview` | — | G |
| `import_start` | `spec: ImportSpec, confirm?: ConfirmationGrant` | `JobId` | `job://progress` | G |
| `jobs_list` / `job_get` | — / `jobId` | `JobInfo[]` / `JobInfo` | — | G |
| `job_cancel` | `jobId` | `()` | — | G |
| `jobs_clear_finished` | — | `number` | — | G |
| `backup_jobs_list` | — | `BackupJob[]` | — | H |
| `backup_job_save` | `job: BackupJob, passphrase: SecretChange` | `BackupJob` | `backup://changed` | H |
| `backup_job_delete` | `id` | `()` (file đã tạo giữ nguyên) | `backup://changed` | H |
| `backup_job_run_now` | `id` | `JobId` | `job://progress` | H |
| `backup_run_manual` | `spec: BackupSpec, passphrase?: Secret, savePassphrase?: boolean` | `JobId` | `job://progress` | H |
| `backup_runs_list` | `jobId?, limit, before?` | `BackupRun[]` | — | H |
| `backup_inspect` | `path, passphrase?: Secret` | `BackupInspection` | — | H |
| `backup_verify` | `path, passphrase?` | `JobId` | `job://progress` | H |
| `restore_start` | `spec: RestoreSpec, passphrase?, confirm?` | `JobId` (`ConfirmationRequired` nếu đích prod, `clean`) | `job://progress` | H |
| `backup_tools_detect` | — | `NativeToolInfo[]` | — | H |
| `backup_tool_set_path` | `tool, path: string \| null` | `NativeToolInfo` | — | H |
| `schedule_preview` | `cron, timezone, count` | `string[]` (RFC 3339) hoặc `Error::Other` khi cron sai | — | H |
| `background_get` / `background_set` | — / `settings: BackgroundSettings { closeToTray: 'auto'\|'always'\|'never', autostart }` | `BackgroundSettings` | — | H |
| `object_children_page` | `sessionId, request: ChildrenPageRequest` | `ChildrenPage` | — | L |
| `key_read` | `sessionId, request: KeyReadRequest` | `KeyValuePage` | — | L |
| `keys_delete_pattern` | `sessionId, database: ObjectPath, pattern, confirm?` | `JobId` | `job://progress` | L |
| `connection_parse_uri` | `uri` | `{ profile: ConnectionProfile, hasPassword: boolean }` (mật khẩu giữ trong `PendingSecrets`, trả `pendingSecretId`) | — | K |
| `driver_diagnostics` | `kind` | `{ status, message, helpUrl? }` | — | J |
| `ssh_trust_host` | `host, port, keyType, fingerprint` | `()` | — | A |
| `connection_save` (GĐ1, thêm) | `+ sshSecret?: SecretChange` | như cũ | — | A |

Bridge (dev): scaffold tổng quát hoá `fetch_stream` cho mọi command có Channel (`table_fetch`, `query_execute`, `lease_execute`, `lease_fetch_more`) và thêm `GET /events` (NDJSON mọi event Tauri app phát: `job://progress`, `lease://state`, `backup://changed`). `ipc.ts` thêm `ipc.onEvent(name, handler): Promise<() => void>` (Tauri `listen`, bridge `EventSource`/fetch stream, mock emitter).

`PathGrants` (AppState): mọi đường dẫn đến từ `file_pick_*`/`sql_file_*`/`plan_file_*` được ghi (canonical, mục đích read/write/dir, hết hạn 1 giờ); `export_start`, `import_start`, `import_preview`, `backup_*`, `restore_start` chỉ nhận path đã cấp hoặc thư mục đích đã lưu trong `backup_jobs` (đã cấp lúc tạo job). Ngăn webview bị chiếm quyền ghi file tuỳ ý.

---

## 5. Store (SQLite) — migration v3+

Agent dựng khung **nối thêm cả 4 migration một lần** (các gói chỉ viết repository Rust). `MIGRATIONS[2..]`:

```sql
-- v3: editor (QUERY-01, 07, 08)
CREATE TABLE query_history (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    connection_id  TEXT NOT NULL REFERENCES connections(id) ON DELETE CASCADE,
    database_name  TEXT,
    kind           TEXT NOT NULL,          -- query | explain | script | changes
    text           TEXT NOT NULL,          -- ≤ 100 000 chars; secrets redacted (see below)
    text_truncated INTEGER NOT NULL DEFAULT 0,
    started_at     TEXT NOT NULL,
    elapsed_ms     INTEGER,
    status         TEXT NOT NULL,          -- running | ok | error | cancelled
    statements     INTEGER,
    rows           INTEGER,
    error_message  TEXT
);
CREATE INDEX idx_history_conn_time ON query_history(connection_id, started_at DESC);
CREATE INDEX idx_history_time ON query_history(started_at DESC);
CREATE TABLE saved_query_folders (
    id         TEXT PRIMARY KEY NOT NULL,
    name       TEXT NOT NULL,
    parent_id  TEXT REFERENCES saved_query_folders(id) ON DELETE CASCADE,
    sort_order INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE saved_queries (
    id            TEXT PRIMARY KEY NOT NULL,
    folder_id     TEXT REFERENCES saved_query_folders(id) ON DELETE SET NULL,
    name          TEXT NOT NULL,
    text          TEXT NOT NULL,
    is_snippet    INTEGER NOT NULL DEFAULT 0,  -- snippet placeholders ${1:name}
    db_kind       TEXT,                        -- NULL = any
    connection_id TEXT REFERENCES connections(id) ON DELETE SET NULL,
    description   TEXT,
    sort_order    INTEGER NOT NULL DEFAULT 0,
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL
);
CREATE INDEX idx_saved_folder ON saved_queries(folder_id);
CREATE TABLE editor_tabs (
    id            TEXT PRIMARY KEY NOT NULL,
    position      INTEGER NOT NULL,
    title         TEXT NOT NULL,
    connection_id TEXT REFERENCES connections(id) ON DELETE SET NULL,
    database_name TEXT,
    schema_name   TEXT,
    text          TEXT NOT NULL,
    file_path     TEXT,
    cursor        INTEGER NOT NULL DEFAULT 0,
    auto_commit   INTEGER NOT NULL DEFAULT 1,
    updated_at    TEXT NOT NULL
);
```
```sql
-- v4: ERD + export presets (ERD-05, EXPORT-10)
CREATE TABLE erd_layouts (
    id            TEXT PRIMARY KEY NOT NULL,
    connection_id TEXT NOT NULL REFERENCES connections(id) ON DELETE CASCADE,
    name          TEXT NOT NULL,
    scope         TEXT NOT NULL,   -- ObjectPath JSON
    tables        TEXT NOT NULL,   -- JSON [{path, x, y, collapsed, color?}]
    options       TEXT NOT NULL,   -- JSON {columns:"all"|"keys", showTypes, showComments, notation:"crowsFoot"}
    viewport      TEXT,            -- JSON {x, y, zoom}
    snapshot      TEXT NOT NULL,   -- JSON {tablePathKey: [[column, typeName], ...]} for change detection
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL,
    UNIQUE(connection_id, name)
);
CREATE INDEX idx_erd_conn ON erd_layouts(connection_id);
CREATE TABLE export_presets (
    id            TEXT PRIMARY KEY NOT NULL,
    name          TEXT NOT NULL,
    connection_id TEXT REFERENCES connections(id) ON DELETE CASCADE,
    spec          TEXT NOT NULL,   -- ExportSpec JSON (destination path kept, re-granted on run)
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL
);
```
```sql
-- v5: backup (BACKUP-07, 08, 11, 12)
CREATE TABLE backup_jobs (
    id                 TEXT PRIMARY KEY NOT NULL,
    name               TEXT NOT NULL,
    connection_id      TEXT NOT NULL REFERENCES connections(id) ON DELETE CASCADE,
    enabled            INTEGER NOT NULL DEFAULT 1,
    spec               TEXT NOT NULL,     -- BackupSpec JSON
    cron               TEXT,              -- NULL = manual only
    timezone           TEXT NOT NULL DEFAULT 'local',
    retention          TEXT NOT NULL,     -- Retention JSON
    missed_policy      TEXT NOT NULL DEFAULT 'runOnce',
    notify             TEXT NOT NULL,     -- NotifyPolicy JSON
    encrypted          INTEGER NOT NULL DEFAULT 0,  -- passphrase in keychain account "backup:<id>"
    last_scheduled_for TEXT,
    last_run_at        TEXT,
    next_run_at        TEXT,
    created_at         TEXT NOT NULL,
    updated_at         TEXT NOT NULL
);
CREATE INDEX idx_backup_jobs_due ON backup_jobs(enabled, next_run_at);
CREATE TABLE backup_runs (
    id            TEXT PRIMARY KEY NOT NULL,
    job_id        TEXT REFERENCES backup_jobs(id) ON DELETE SET NULL,
    job_name      TEXT NOT NULL,
    connection_id TEXT REFERENCES connections(id) ON DELETE SET NULL,
    kind          TEXT NOT NULL,          -- backup | restore | verify
    trigger       TEXT NOT NULL,          -- manual | schedule | catchUp | cli
    engine        TEXT NOT NULL,
    status        TEXT NOT NULL,          -- running | succeeded | failed | cancelled | interrupted
    started_at    TEXT NOT NULL,
    finished_at   TEXT,
    duration_ms   INTEGER,
    file_path     TEXT,
    size_bytes    INTEGER,
    checksum      TEXT,
    objects       INTEGER,
    rows          INTEGER,
    error_kind    TEXT,
    error_message TEXT,
    log_tail      TEXT                    -- last 16 KB
);
CREATE INDEX idx_backup_runs_job ON backup_runs(job_id, started_at DESC);
CREATE INDEX idx_backup_runs_time ON backup_runs(started_at DESC);
```
```sql
-- v6: SSH (CONN-06)
ALTER TABLE connections ADD COLUMN ssh TEXT;   -- SshConfig JSON or NULL
CREATE TABLE ssh_known_hosts (
    host        TEXT NOT NULL,
    port        INTEGER NOT NULL,
    key_type    TEXT NOT NULL,
    fingerprint TEXT NOT NULL,              -- SHA256:base64
    added_at    TEXT NOT NULL,
    PRIMARY KEY (host, port, key_type)
);
```
Không có bảng `jobs` (job export/import chỉ trong bộ nhớ; backup/restore/verify ghi `backup_runs`). Lịch sử: giữ tối đa `AppSettings.historyMaxPerConnection` (mặc định 5.000) — cắt sau mỗi lần ghi; tìm kiếm `LIKE` (FTS5 để sau). Văn bản lịch sử che bí mật: `dbstudio_sql::redact` thay literal sau `PASSWORD`/`IDENTIFIED BY`/`AUTH`/`CONFIG SET requirepass` bằng `'***'`.

Settings (`settings` key-value, GĐ1): `AppSettings` thêm `editorMaxRows: u32 (1000)`, `queryTimeoutMs: u64 (0 = không)`, `historyEnabled: bool (true)`, `historyMaxPerConnection: u32 (5000)`, `maxLobBytes: u64 (16 MiB)`, `planMisestimateFactor: u32 (10)`, `closeToTray: CloseBehavior (auto)`; key riêng `native_tools` (JSON map tool → path), `oracle_client` (JSON `{libDir}`). Keychain account mới: `"<connId>#ssh"`, `"backup:<jobId>"`.

Rust model trong store (dùng bởi command và backup/CLI): `HistoryEntry`, `NewHistoryEntry`, `HistoryQuery { connection_id, search, status, before_id, limit }`, `SavedQuery`, `SavedFolder`, `EditorTabState`, `ErdLayout` (field JSON là `serde_json::Value`), `ErdLayoutSummary`, `ExportPreset { spec: serde_json::Value }`, `BackupJobRow`, `BackupRunRow`, `KnownHost`. Mỗi nhóm một file (`history.rs`, `saved.rs`, `erd.rs`, `presets.rs`, `backup.rs`, `known_hosts.rs`) — scaffold tạo struct + chữ ký, gói tương ứng hiện thực.

---

## 6. Frontend

### 6.1 Thư viện (đã kiểm `pnpm view`)

| Gói | Phiên bản | Dùng cho |
|---|---|---|
| `@codemirror/autocomplete` | 6.20.4 | autocomplete schema (QUERY-03), snippet |
| `@codemirror/commands`, `@codemirror/search`, `@codemirror/lint` | 6.11.1 / 6.7.2 / 6.9.8 | phím tắt, tìm, đánh dấu lỗi (QUERY-13) |
| `@codemirror/lang-sql` (đã có) | 6.10.0 | `PostgreSQL`, `MySQL`, `MariaSQL`, `MSSQL`, `PLSQL` |
| `@codemirror/lang-javascript`, `@codemirror/lang-json` | 6.2.5 / 6.0.2 | Mongo shell, editor EJSON |
| `sql-formatter` | 15.9.0 | QUERY-09 (`postgresql`, `mysql`, `mariadb`, `transactsql`, `plsql`) |
| `@xyflow/react` | 12.12.1 | ERD, plan graph |
| `elkjs` | 0.12.1 | auto layout (chạy trong Web Worker `elk-worker.min.js`) |
| `html-to-image` | 1.11.13 | PNG/SVG ERD & plan |
| `jspdf` + `svg2pdf.js` | 4.2.1 / 2.9.0 | PDF (ERD-06) |
| `cronstrue` | 3.39.0 | mô tả lịch cron (vi/en) |

Không cần plugin JS notification/autostart (mọi thứ qua command Rust). Tauri Rust thêm: `tauri` feature `tray-icon`, `tauri-plugin-notification = "2.8"`, `tauri-plugin-autostart = "2.7"`, `tauri-plugin-single-instance = "2.5"` (bản 3.x còn alpha). Capability webview **không** thêm quyền mới.

### 6.2 Cấu trúc `src/`

```
src/lib/types/           # tách types.ts thành module; types.ts chỉ còn `export * from './types/...'`
  core.ts (GĐ1) script.ts lease.ts edit.ts keyvalue.ts plan.ts model.ts jobs.ts export.ts backup.ts history.ts ssh.ts
src/lib/ipc/             # ipc.ts giữ API `ipc`, gộp từ: core.ts editor.ts changes.ts plan.ts erd.ts files.ts jobs.ts export.ts backup.ts keyvalue.ts events.ts
src/lib/mock/            # thêm leases.ts history.ts changes.ts plan.ts erd.ts jobs.ts export.ts backup.ts keyvalue.ts
                         # catalogMssql.ts catalogOracle.ts catalogMongo.ts catalogRedis.ts (catalog.ts dispatch theo kind)
src/lib/text.ts          # charToUtf16Offset / utf16ToCharOffset
src/features/editor/     # EditorTab, editorStore (per tab: leaseId, text, results, txState), useLease, ResultsPanel (multi result tabs, dùng lại DataGrid chế độ đọc),
                         # ParamsDialog, TxBar (commit/rollback/auto), completion.ts (từ schema_model detail=Full không comment, cache TanStack Query),
                         # dialects.ts (ScriptDialect → CM6 language), format.ts, HistoryPanel, SavedQueriesPanel, sqlFile.ts, RedisConsole (chế độ console của EditorTab)
src/features/safety/     # ConfirmDestructiveDialog (gõ confirmText), ReadOnlyBadge, useGuardedCall (bắt confirmationRequired → hỏi → gọi lại kèm confirm)
src/features/data/edit/  # pendingChanges store, EditableCell, ChangesPreviewDialog, rowOps (thêm/xoá/nhân bản), copyFormats (DATA-08)
src/features/plan/       # PlanTab, PlanGraph (xyflow+elk), PlanTable (cây thụt lề), PlanRaw (CM6 json/xml/text), PlanCompare, planStore (node chọn đồng bộ)
src/features/erd/        # ErdTab, ErdCanvas, TableNode, CrowsFootEdge (markers SVG), selection.ts (N bậc), erdLayout.ts (elk), diff.ts (snapshot), exportImage.ts
src/features/export/     # ExportWizard (dialog), ImportWizard, specForm.ts, presets
src/features/jobs/       # jobsStore (onEvent job://progress), JobsIndicator (status bar), JobsPanel
src/features/backup/     # BackupTab (singleton), BackupJobDialog, CronBuilder (daily/weekly/monthly + advanced; schedule_preview), RestoreDialog, RunsHistory, NativeToolsPanel, BackgroundSettings
src/features/document/   # DocumentView (bảng phẳng | tree | EJSON), flatten.ts, DocumentEditor, documentChanges.ts
src/features/keyvalue/   # KeyTab, viewers/{String,Hash,List,Set,ZSet,Stream,Json}.tsx, TtlEditor, keyChanges.ts, keyName.ts
src/features/connections/ # (gói A) SshSection trong ConnectionDialog, HostKeyDialog
src/locales/{vi,en}/     # namespace mới: editor plan erd export jobs backup document keyvalue safety (scaffold tạo `{}` + đăng ký i18n.ts)
```

### 6.3 Tab mới và đăng ký

`src/stores/tabs.ts` thêm vào union `TabItem` (scaffold):
```ts
| (TabBase & { kind: 'editor'; payload: EditorTabPayload })   // { tabStateId, connectionId, sessionId|null, mode:'sql'|'mongo'|'redis' }
| (TabBase & { kind: 'plan'; payload: PlanTabPayload })       // { sessionId, connectionId, planId }  (plan giữ trong planStore)
| (TabBase & { kind: 'erd'; payload: ErdTabPayload })         // { sessionId, connectionId, scope: ObjectPath, layoutId|null }
| (TabBase & { kind: 'key'; payload: ObjectTabPayload })      // Redis key
| (TabBase & { kind: 'backup'; payload?: undefined })         // singleton id 'backup'
```
`closeSessionTabs` không đóng tab `editor` (chỉ tháo `sessionId`, giữ văn bản). `TabBase` thêm `beforeClose?: () => Promise<boolean>` (không lưu trong store, đăng ký qua `useTabGuard(tabId, fn)`) để editor hỏi khi có transaction mở. `tabRegistry.tsx` thêm renderer trỏ tới component giữ chỗ trong từng feature (scaffold), gói tính năng chỉ thay nội dung file của mình. Tab `table` chọn `DocumentView` khi `descriptor.capabilities.documentStore`.

### 6.4 Mock (chạy ngoài Tauri)

Mỗi file mock mới cung cấp đúng hàm của `mock` cho command tương ứng; scaffold tạo bản tối thiểu (trả dữ liệu rỗng hợp lệ hoặc ném `unsupported`), gói tính năng làm phong phú: lease giả lập transaction và `pending_result`; `ConfirmationRequired` khi profile mock `environment: production` và văn bản chứa `DROP`/`DELETE FROM x` không WHERE; plan trả fixture JSON PG/MSSQL từ `crates/dbstudio-plan/tests/fixtures` (import qua `?raw`); job giả phát `job://progress` qua emitter; Redis mock 50.000 khoá sinh lười; Mongo mock document có `Ext objectId`/`int64`.

### 6.5 Hợp đồng TS ↔ Rust

Thư mục `contract/fixtures/*.json` (mẫu JSON cho mọi kiểu mới, do scaffold sinh từ test Rust `crates/dbstudio-core/tests/contract.rs` với `UPDATE_FIXTURES=1`); `src/lib/types/contract.test.ts` đọc cùng fixture và kiểm type guard. Lệch hình dạng → test đỏ ở một trong hai phía.

---

## 7. Kế hoạch chia việc song song

### 7.1 Bước 0 — Dựng khung hợp đồng (1 agent, Sonnet; review bởi Opus)

Làm trên nhánh `feature/quandh/phase2-contract`, merge vào `develop` trước khi mở các gói khác. Phạm vi:
1. `dbstudio-core`: toàn bộ kiểu/trait mục 1 (đúng serde), `fallback.rs` (3 hàm, có test với mock session), `Capabilities` mới + `planned_for`, `Value` đổi `Document`/thêm `Ext`, `Error` mới + serialize, `ObjectKind`/folder; `lease_kit.rs` **chỉ chữ ký** (`todo` → gói B). Test JSON cho mọi kiểu + sinh `contract/fixtures`.
2. Crate rỗng có API công khai đúng chữ ký (thân trả `Err(Error::Unsupported(..))` hoặc kết quả tối thiểu): `dbstudio-sql` (`analyze` tối thiểu: cả văn bản = 1 câu `Unknown`; `bind` khi không có tham số trả nguyên văn), `dbstudio-plan`, `dbstudio-export`, `dbstudio-backup`, `dbstudio-cli` (clap khung), `dbstudio-ssh`, `drivers/{mssql,oracle,mongodb,redis}` (driver đăng ký được, `connect` → `Unsupported`, descriptor `Available` chỉ khi feature bật). Mọi dependency ngoài ghi sẵn trong `Cargo.toml` của từng crate + workspace.
3. PG/MySQL: thêm `script_dialect()`; trong `impl Session` thêm các method mới **ủy quyền** sang module rỗng theo gói: `lease.rs` (B), `edit.rs` (D), `explain.rs` (E), `model.rs` + `ddl_gen.rs` (F), `stream.rs` + `bulk.rs` (G) — module trả `Unsupported`.
4. `dbstudio-store`: migration v3–v6, struct model + chữ ký repository trong file riêng, `AppSettings` mới, đọc/ghi cột `ssh`.
5. `src-tauri`: `AppState` thêm `leases: LeaseManager`, `jobs: JobManager` (khung), `confirm: ConfirmTokens`, `grants: PathGrants`; `SessionManager.Entry` thêm `config: ConnectionConfig`; mọi command mục 4 với `*_impl` stub, đăng ký `generate_handler!`, dispatch bridge + stream tổng quát + `/events`; `guard.rs` gọi `dbstudio_sql` (đã nối luồng đủ 5 bước với stub); `Host` mới + `FakeHost`; Cargo feature `mssql`, `oracle`, `mongodb`, `redis`; plugin Tauri đã thêm nhưng chưa dùng.
6. Frontend: tách `types/`, `ipc/` (mọi wrapper), mock stub, `ERROR_KINDS`, `Value` mới trong `format.ts`/`JsonTree`, tab kind + component giữ chỗ, namespace i18n, `ipc.onEvent`, thêm dependency npm mục 6.1.
7. `docker/compose.test.yml` thêm service mục 3, `docker/gen-certs.sh` sinh PEM gộp cho Mongo/Redis, `docker/oracle-it/Dockerfile`; CI job `integration` thêm các service (MSSQL chạy trên runner x86_64).

**Xong khi**: `cargo fmt --check`, `cargo clippy --workspace --all-features -D warnings`, `cargo test --workspace` xanh (gồm test hợp đồng JSON); `pnpm lint && pnpm test && pnpm build` xanh; app chạy được với mock, mọi tab mới mở ra placeholder; `docker compose -f docker/compose.test.yml up -d --wait` lên đủ service (MSSQL chấp nhận chậm trên Apple Silicon). Sau mốc này, **đổi hợp đồng** = PR riêng sửa core/TS/fixture + ghi chú trong tài liệu, các gói rebase.

### 7.2 Các gói song song (sau Bước 0)

Mỗi gói một worktree `feature/quandh/<goi>` từ `develop` mới nhất. Cột "File" là vùng **độc quyền**; ngoài vùng chỉ được sửa nếu ghi trong gói.

| Gói | Phạm vi | File độc quyền | Phụ thuộc | Tiêu chí xong (kiểm chứng được) |
|---|---|---|---|---|
| **A. dbstudio-sql + SSH** | Lexer/split/params/classify/guard/dialect literal/ddl/mongo_shell/redis_cli/vocabulary/redact; sau đó SSH tunnel | `crates/dbstudio-sql/**`, `crates/dbstudio-ssh/**`, `src-tauri/src/commands/ssh.rs`, `crates/dbstudio-store/src/known_hosts.rs`, `src/features/connections/{SshSection,HostKeyDialog}.tsx` (+ dòng nối trong `ConnectionDialog.tsx`) | Bước 0 | Unit test ≥ 300 ca: tách câu tương đương corpus splitter PG hiện có (test vi sai), `GO n`, PL/SQL `/`, `DELIMITER`; tham số không bắt nhầm `::`, `$$`, `:new`, `@@`; phân loại 4 dialect + Mongo + Redis đúng bảng phá huỷ; `bind` + `OffsetMap` khứ hồi; fuzz (`cargo test` với proptest) không panic. **Milestone A1 (ưu tiên, ~đầu tiên)**: `mongo_shell` + `redis_cli` + `split` cho K, L, I, J. SSH: IT với `sshd` tới `pg16`/`mysql8`/`mssql2022` bằng mật khẩu và key có passphrase; host key lạ → `sshHostKey`. |
| **B. Editor backend** | `lease_kit`, lease PG/MySQL, commands editor/history/saved/tabs/sql_file, guard hoàn chỉnh, lịch sử | `crates/dbstudio-core/src/lease_kit.rs`, `crates/drivers/{postgres,mysql}/src/{lease.rs,conn.rs,exec.rs}` (exec: chuyển sang splitter `dbstudio-sql` sau A1), `src-tauri/src/{lease.rs,guard.rs,history.rs}`, `src-tauri/src/commands/{editor,history}.rs`, `crates/dbstudio-store/src/{history,saved,tabs}.rs` | Bước 0, A (thật) | IT (PG16, MySQL8, MariaDB): script 3 câu → 3 result + affected + NOTICE; `pg_sleep(60)`/`SLEEP(60)` huỷ < 2 s và server xác nhận; manual commit: kết nối khác không thấy UPDATE, rollback hoàn tác; `fetch_more` liên tục 3 lần; đóng lease khi transaction mở → rolled_back; test command trực tiếp: read-only chặn INSERT/UPDATE/DELETE/DDL; prod `DELETE FROM t` và `DROP TABLE` → `confirmationRequired`, token sai/hết hạn/dùng lại bị từ chối, gõ sai tên DB bị từ chối; lịch sử còn sau khi mở lại store. |
| **C. Editor frontend** | Editor CM6, nhiều tab, result tabs, params dialog, tx bar, autocomplete alias, format, lịch sử/saved, mở/lưu file, xác nhận phá huỷ, phím tắt APP-04, khôi phục tab | `src/features/{editor,safety}/**`, `src/lib/mock/{leases,history}.ts`, `src/locales/*/{editor,safety}.json`, `src/app/StatusBar.tsx` (ô transaction) | Bước 0 (mock); chạy thật khi B xong | Vitest: autocomplete `SELECT u.| FROM users u` gợi ý cột; lỗi đánh dấu đúng dòng/cột (kể cả emoji trước vị trí); đóng tab có transaction mở hỏi xác nhận; dialog phá huỷ chỉ bật nút khi gõ đúng; khôi phục tab sau reload (mock store); kiểm tay với DB thật qua dev bridge. |
| **D. Sửa dữ liệu** | Editability, preview/apply PG/MySQL, lưới chế độ sửa, thêm/xoá/nhân bản dòng, trình sửa ô, copy định dạng | `crates/drivers/{postgres,mysql}/src/edit.rs`, `src-tauri/src/commands/changes.rs`, `src/features/data/**` (độc quyền cả thư mục), `src/lib/mock/changes.ts` | Bước 0 | IT: sửa 3 ô + thêm 1 + xoá 1 → preview SQL đúng thứ tự → apply một transaction; lỗi ở thay đổi 2 → `changeFailed{index:1}` và DB không đổi; UPDATE trúng 0 dòng → rollback; bảng không PK/unique → `editable=false, reason=noUniqueKey`, lưới hiện lý do; read-only → `readOnly`. Vitest: pending changes tô màu, NULL ≠ chuỗi rỗng. |
| **E. Plan** | `dbstudio-plan` (8 format), explain PG/MySQL, plan tab | `crates/dbstudio-plan/**` (+ `tests/fixtures/*` ≥ 5/format, MSSQL/Oracle/Mongo lấy từ tài liệu chính thức hoặc tự chụp từ container), `crates/drivers/{postgres,mysql}/src/explain.rs`, `src-tauri/src/commands/plan.rs`, `src/features/plan/**`, `src/lib/mock/plan.ts` | Bước 0 | Unit: mọi fixture parse thành cây, totals đúng, cảnh báo full scan / lệch ≥ 10× / spill / NL; IT PG: `EXPLAIN ANALYZE UPDATE` trong transaction rollback, dữ liệu không đổi; MySQL ANALYZE UPDATE → `unsupported`. Vitest: chọn node đồng bộ graph/table/raw; hot path tô đậm. |
| **F. ERD + DDL** | `schema_model` + `generate_ddl` PG/MySQL, store ERD, ERD tab, xuất PNG/SVG/PDF | `crates/drivers/{postgres,mysql}/src/{model.rs,ddl_gen.rs}`, `crates/dbstudio-store/src/erd.rs`, `src-tauri/src/commands/erd.rs`, `src/features/erd/**`, `src/lib/mock/erd.ts` | Bước 0 | IT: Pagila (PG) / Sakila (MySQL) — đủ bảng, PK, FK, `from_optional`/`one_to_one` đúng; `generate_ddl` toàn DB chạy lại vào DB trống tạo đủ đối tượng (FK ở PostData); `schema_model` 100 bảng ≤ 4 truy vấn (đếm qua log). Vitest/tay: auto layout 100 bảng < 3 s; "bảng X + 1 bậc"; lưu/mở lại giữ vị trí, bảng mất được đánh dấu; PNG/SVG/PDF đọc được cả theme sáng/tối. |
| **G. Export/Import + Jobs** | `dbstudio-export` đầy đủ, stream/bulk PG/MySQL, `JobManager`, file dialog/grants, presets, wizard UI, trung tâm job | `crates/dbstudio-export/**`, `crates/drivers/{postgres,mysql}/src/{stream.rs,bulk.rs}`, `src-tauri/src/{jobs.rs,grants.rs}`, `src-tauri/src/commands/{jobs,export,files}.rs`, `crates/dbstudio-store/src/presets.rs`, `src/features/{export,jobs}/**`, `src/lib/mock/{jobs,export}.ts` | Bước 0; schema export cần F (`generate_ddl`) — làm data trước | IT PG/MySQL: "cả hai" ra SQL → chạy vào DB trống → số bảng/dòng/checksum (`md5` từng bảng theo khoá) khớp; XLSX tách sheet ở > 1.048.575 dòng (test với giới hạn hạ qua hằng số test), sheet dictionary; CSV zip + BOM; JSON array/NDJSON + schema.json; bảng 10 triệu dòng (tuỳ chọn `DBSTUDIO_IT_BIG=1`): RSS không tăng tuyến tính, huỷ giữa chừng không để file; export từ query cho mọi định dạng; import CSV 1 triệu dòng có mapping, lỗi báo số dòng. |
| **H. Backup** | `dbstudio-backup` (container, crypto, native tools, scheduler, retention), CLI, tray, autostart, single-instance, notification, backup UI | `crates/dbstudio-backup/**`, `crates/dbstudio-cli/**`, `src-tauri/src/{scheduler.rs,tray.rs,background.rs}`, `src-tauri/src/commands/{backup,background}.rs`, `crates/dbstudio-store/src/backup.rs`, `src/features/backup/**`, `src/lib/mock/backup.ts` | Bước 0; engine logic cần G (writer SQL + stream) — làm container/crypto/native/scheduler/UI trước | Unit: sai passphrase/file bị sửa → `integrity`; scheduler với đồng hồ giả: `*/5 * * * *` đúng giờ, chạy bù đúng 1 lần, retention "giữ 3" xoá đúng file; IT: backup tích hợp → restore DB trống khớp (PG/MySQL); `pg_dump`/`mysqldump` khi có trong PATH thành công, không có → `problem` hiển thị; restore vào prod cần xác nhận; CLI `backup --job` chạy với keychain giả (`--secret-store memory` chỉ cho test). Tay: cửa sổ đóng → tray, lịch vẫn chạy; thông báo OS; autostart bật/tắt (macOS; Windows/Linux khi có máy). |
| **I. MSSQL** | Driver đầy đủ (mọi method mục 3.1) + fork tiberius | `crates/drivers/mssql/**`, `vendor/tiberius-patch/` (hoặc fork git), dòng `[patch]` trong workspace `Cargo.toml` (được phép sửa duy nhất mục này) | Bước 0, A1 (split `GO`) | IT `mssql2022`: tiêu chí GĐ3 (kết nối TLS, cây, dữ liệu, query + huỷ, nhiều result set + PRINT, `GO`, sửa dữ liệu, explain XML parse bằng `dbstudio-plan` nếu E đã merge, schema_model, generate_ddl chạy lại được, bulk). |
| **J. Oracle** | Driver đầy đủ (3.2) + trạng thái unavailable + chẩn đoán | `crates/drivers/oracle/**`, `docker/oracle-it/**`, `src-tauri/src/commands/app.rs` phần `driver_diagnostics` (hàm mới, không sửa hàm cũ) | Bước 0, A1 (split PL/SQL) | IT trong `oracle-it`: tiêu chí GĐ3 + block PL/SQL, procedure không tách sai, DBMS_OUTPUT, kiểu biên; host không có Instant Client: `list_drivers` báo `unavailable`, không panic. |
| **K. MongoDB + document UI** | Driver (3.3), `connection_parse_uri`, document view/edit | `crates/drivers/mongodb/**`, `src-tauri/src/commands/uri.rs`, `src/features/document/**`, `src/lib/mock/catalogMongo.ts` | Bước 0, A1 (mongo_shell) | IT `mongo44` + `mongo7`: tiêu chí GĐ4 Mongo (ObjectId/Decimal128/Date/Int64 khứ hồi đúng; find/aggregate; sửa/thêm/xoá; transaction chỉ trên `mongo7`); ERD sampling. Vitest: bảng phẳng từ document không đồng nhất, editor EJSON. |
| **L. Redis + key-value UI** | Driver (3.4), page tree, key viewer/editor, console mode, xoá theo pattern | `crates/drivers/redis/**`, `src-tauri/src/commands/keyvalue.rs`, `src/features/keyvalue/**`, `src/features/explorer/**` (phân trang cây — độc quyền thư mục), `src/lib/mock/{keyvalue,catalogRedis}.ts` | Bước 0, A1 (redis_cli) | IT `redis6`/`redis7`/`valkey8`: duyệt 1 triệu khoá bằng SCAN (KEYS bị đổi tên vẫn chạy), đủ 6 kiểu xem/sửa, TTL đặt/xoá, `FLUSHDB` prod cần xác nhận, read-only chặn lệnh ghi; console lịch sử + gợi ý (C cung cấp khung console, L cung cấp vocabulary/viewer). |

Ghi chú phân vùng:
- Thư mục `src/features/explorer/**` thuộc L (cây phân trang dùng chung); các gói khác cần mục menu ngữ cảnh mới (mở editor, ERD, export, backup) thì scaffold đã thêm sẵn mục gọi tới hàm `openEditor/openErd/openExport/openBackup` trong `src/features/explorer/actions.ts` (stub) — gói C/F/G/H hiện thực hàm đó trong thư mục của mình (`src/features/<x>/open.ts`), `actions.ts` chỉ import.
- `src/app/StatusBar.tsx`: C sở hữu; G thêm `JobsIndicator` qua slot scaffold đã để sẵn (`<StatusBarSlot name="jobs" />`).
- Locale: mỗi gói chỉ sửa namespace của mình; khoá dùng chung (`common.json`) chỉ scaffold.

### 7.3 Thứ tự và đồng bộ

```
Bước 0 (scaffold) ──► merge develop
   ├─ A (A1 trước: split/mongo_shell/redis_cli) ─┬─► I, J, K, L dùng parser thật
   │                                             └─► B chuyển exec PG/MySQL sang splitter chung
   ├─ B ──► C chạy thật
   ├─ D, E (độc lập)
   ├─ F ──► G (phần schema export)
   └─ G ──► H (engine logic)
Wave 2 (sau khi I–L merge): ma trận tích hợp — export/backup/ERD/plan trên MSSQL/Oracle/Mongo/Redis;
   cập nhật docs (Opus review), CI integration đủ DB.
```
Thứ tự merge vào `develop` khuyến nghị: A1 → B → D → E → F → C → G → I/J/K/L → A (SSH) → H. Mỗi merge chạy toàn bộ test workspace + frontend.

---

## 8. Rủi ro & quyết định cần chủ dự án

### 8.1 Cần quyết (kèm mặc định để không bị chặn)

| # | Câu hỏi | Mặc định đề xuất (dùng nếu chưa có trả lời) |
|---|---|---|
| Q1 | **Phân phối Oracle Instant Client** (giấy phép cho phép đóng gói kèm theo điều khoản OTN/redistribution, nhưng tăng ~80–120 MB và phụ thuộc OS/arch). | Không đóng gói; app báo `unavailable` + hướng dẫn tải, cho chọn thư mục client trong Cài đặt. Feature `oracle` bật trong bản build. |
| Q2 | **Xác thực MSSQL**: cần Windows Integrated (Kerberos) trên macOS/Linux và Azure AD (Entra ID) không? | SQL login + NTLM (user/password miền) mọi OS, Integrated chỉ trên Windows; Azure AD để sau. |
| Q3 | **Fork `tiberius`** (D10) để có PRINT, row count từng câu, cancel. | Đồng ý fork tối thiểu, gửi PR upstream; nếu không đồng ý: MSSQL không có PRINT/affected chi tiết và huỷ = đóng kết nối. |
| Q4 | **Mã hoá backup mặc định**. | Tắt mặc định; khi đích là kết nối `production` UI đề nghị bật; passphrase lưu keychain theo job; mất passphrase = mất backup (hiển thị cảnh báo). |
| Q5 | **Backup tự động khi app không chạy** (service/daemon) hay tray + CLI là đủ? | Tray + autostart + CLI cho OS scheduler; không làm service. |
| Q6 | **Đổi hình dạng JSON `Value::Document`** (D6). | Đổi ngay ở Bước 0 (chưa có dữ liệu thật dùng). |
| Q7 | **Đích S3/SFTP cho backup**. | Chỉ local/ổ mount ở đợt này. |
| Q8 | **Lưu văn bản query vào lịch sử** (có thể chứa dữ liệu nhạy cảm). | Bật, có che `PASSWORD/IDENTIFIED BY`, nút tắt và xoá lịch sử; giới hạn 5.000/kết nối. |

### 8.2 Rủi ro kỹ thuật

| # | Rủi ro | Hướng xử lý |
|---|---|---|
| R1 | Image SQL Server chỉ amd64; Apple Silicon chạy giả lập (khởi động chậm, đôi khi crash với Rosetta tắt). | Bật "Use Rosetta" trong Docker Desktop; CI chạy trên runner x86_64; healthcheck chờ 120 s. `azure-sql-edge` (arm64) đã ngừng hỗ trợ — không dùng. |
| R2 | Oracle cần Instant Client cả khi build test; macOS dev phải cài thủ công. | Test trong container `oracle-it`; feature `oracle` biên dịch không cần client (ODPI-C nạp động). |
| R3 | Fork `tiberius` phải duy trì. | Patch nhỏ, cô lập; theo dõi upstream mỗi quý. |
| R4 | Giữ kết quả treo (`pending_result`) giữ snapshot/khoá trên server (PG transaction ngầm, MySQL kết nối bận). | Tự huỷ kết quả treo sau 10 phút không dùng hoặc khi tab mất focus lâu; UI hiển thị "đang giữ kết quả". |
| R5 | Phân loại bằng parser không hoàn hảo (T-SQL không cần `;`, PL/SQL động `EXECUTE IMMEDIATE`). | Bảo thủ: `Procedural`/`Unknown` = ghi; với prod quét từ khoá `DROP/TRUNCATE` trong toàn câu; read-only còn lớp driver (PG/MySQL). |
| R6 | MySQL qua SSH với `verify-full`: `mysql_async` không cho đổi tên kiểm chứng (server name) → lỗi tên host. | Ghi rõ; với SSH khuyến nghị `verify-ca`/`require`, hoặc dùng tên host trong `/etc/hosts`; theo dõi upstream. |
| R7 | `html-to-image` trên WKWebView/WebKitGTK có thể sai font/foreignObject. | Xuất SVG bằng serialize trực tiếp node React Flow (không qua foreignObject cho chữ), PNG từ SVG đó; kiểm tay trên 3 webview ở GĐ9. |
| R8 | Redis `apply_changes` không rollback; Mongo standalone không transaction. | `Editability.atomic=false` hiển thị rõ; WATCH + kiểm kiểu trước; dừng ở lỗi đầu và báo số đã áp. |
| R9 | CLI đọc keychain trên macOS bị hỏi quyền cho binary khác app. | Tài liệu hướng dẫn "Always Allow"; CLI báo lỗi rõ khi bị từ chối. |
| R10 | Mười gói chạy song song đụng file dùng chung. | Bước 0 làm hết file dùng chung; bảng vùng độc quyền 7.2; đổi hợp đồng qua PR riêng. |
