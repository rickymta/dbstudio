# DBStudio — Thiết kế "Query Advisor" (gợi ý tối ưu cho Explain plan)

> Trạng thái: bản thiết kế v1 (kiến trúc sư/nghiệp vụ), viết trên nhánh `feature/quandh/plan_advisor` (tách từ `develop` @ `9b8ca13`). Chưa có code.
> Yêu cầu chủ dự án (nguyên văn): "Chức năng phân tích query này cần đưa ra các điểm tối ưu kèm chỉ dẫn cụ thể nhé".
> Nguồn đã đọc: `crates/dbstudio-plan/src/*` (lib, analyze, build, 10 parser) và fixture `tests/fixtures*`, `src-tauri/src/commands/{plan,browse,editor}.rs`, `crates/dbstudio-core/src/{driver,metadata,object,browse,plan,config}.rs`, `crates/dbstudio-sql/src/{lib,lexer,dialect,mongo_shell}.rs`, `crates/dbstudio-store/src/settings.rs`, `src/features/plan/*`, `src/features/editor/{EditorTab,open}.tsx|ts`, `src/locales/{vi,en}/plan.json`, `docker/compose.test.yml`.
> Quy ước giống `phase2-8.md`/`backlog-gd9.md`: serde camelCase; mọi field mới có `#[serde(default)]`; "char offset" = chỉ số ký tự Unicode; doc comment trong code viết tiếng Anh.

## 0. Phạm vi và quyết định lớn

**Mục tiêu**: với mỗi plan (Explain/Analyze, mở từ file, lịch sử, so sánh), hiện danh sách **điểm tối ưu** đã xếp hạng; mỗi điểm có *vì sao* (số liệu lấy từ plan) và *làm gì* (câu SQL/lệnh cụ thể để sao chép hoặc chèn vào editor). Không tự chạy bất cứ thứ gì.

Ngoài phạm vi v1: tự đo lại sau khi áp dụng (what-if/hypopg), gợi ý xoá index thừa, tối ưu Redis/CQL trace, gợi ý cấu hình server toàn cục dạng "đặt giúp".

| # | Quyết định | Lý do ngắn |
|---|---|---|
| A1 | **Thêm lớp `PlanAdvice`, giữ nguyên `PlanWarning`.** `PlanTree` thêm `advice: Vec<PlanAdvice>` (`#[serde(default)]`). Warning là *tín hiệu* (đã dùng ở badge diagram, cột `warnings` của plan history, `compareModel`); advice là *khuyến nghị*. Mỗi warning được ít nhất một advice "nhận" (`fromWarnings`). | Không vỡ JSON/fixture/TS hiện có; history chỉ lưu `RawPlan` nên advice luôn tính lại khi xem. |
| A2 | **Hai tầng**: tầng 1 thuần (`dbstudio_plan::advise`) chạy trong `parse_and_analyze`, chỉ dùng plan + câu lệnh; tầng 2 là command mới `plan_advise` lấy metadata (index, số dòng, kiểu cột) qua `Session::search_objects` + `table_details` rồi chạy lại engine với ngữ cảnh. | Explain không chậm thêm; plan mở từ file/history vẫn có gợi ý khi không có kết nối. |
| A3 | Engine nằm trong `crates/dbstudio-plan/src/advice/`; `dbstudio-plan` **thêm phụ thuộc `dbstudio-sql`** (lexer + `Dialect::quote_ident`). Không có AST: phân tích điều kiện và câu SQL bằng token (mini-parser §5.1). | `dbstudio-sql` chỉ phụ thuộc core → không vòng; không kéo `sqlparser`. |
| A4 | Parser bổ sung trường có cấu trúc `PlanNode.predicates: Vec<NodePredicate>` (vai trò + text) thay vì tách lại `condition` (MySQL nối `attached_condition`/`index_condition` không nhãn; PG xoá key điều kiện khỏi `extra`). | Phân biệt Index Cond / Filter / Join chính xác. |
| A5 | Mỗi advice có **mức tin cậy** (`high/medium/low`) + **caveat** có khoá i18n. `low` hoặc có caveat loại `verify` ⇒ UI gắn nhãn "Cần xác minh". Thiếu metadata ⇒ hạ một bậc. | Không hứa suông. |
| A6 | **Tác động** = tỉ lệ thời gian (ANALYZE) hoặc cost (ước tính) của các node liên quan, hiển thị là *trần* ("node này chiếm ~78% thời gian"), không hứa "nhanh gấp N lần". | Ước lượng tăng tốc thật cần what-if, ngoài phạm vi. |
| A7 | An toàn: chỉ **Sao chép / Chèn vào editor / Mở bảng**. Câu chèn vào editor chạy qua guard 5 bước + ConfirmToken như mọi câu khác. Mỗi hành động ghi **phạm vi** (`query/session/object/database/server`); không sinh câu đổi cấu hình server toàn cục, chỉ mô tả kèm phạm vi. | Yêu cầu §4 của brief. |
| A8 | UI: panel bên phải thành **Inspector** có 2 tab "Gợi ý (n)" / "Node", co giãn và thu gọn được; dải tóm tắt dưới header. Sửa kèm 4 lỗi UX (§9) trong cùng đợt. | Bấm gợi ý phải chọn/zoom node trên view đang mở; lỗi (c) đến từ chính panel này. |

---

## 1. Hiện trạng liên quan (đã kiểm trong code)

- `PlanTree { format, analyzed, root, totals, warnings, hot_path, node_count }`; `PlanNode` có `condition` (chuỗi đã nối), `extra: BTreeMap<String,String>` (scalar phẳng của định dạng gốc), `object: PlanObject{schema,name,alias,index}`, số liệu est/actual, `self_cost/self_time_ms` do `analyze` điền.
- `PlanRule` 4 giá trị, `PlanRules{large_table_rows=100k, misestimate_factor=10, misestimate_min_rows=100, nested_loop_rows=10k}` (struct `Copy`, `#[serde(default)]`). Settings chỉ có `plan_misestimate_factor`.
- `plan_explain` → `parse_and_analyze` → `record_history` lưu **`RawPlan`** (đã redact) + `warnings` (số đếm). `plan_parse(raw, rules)` dùng cho file/history. ⇒ advice tính lại mỗi lần parse, không cần migration.
- Thiếu tín hiệu cần cho advisor (phải sửa parser, §3.3):
  - MSSQL: `MissingIndexes` **không được đọc**; `Warnings` cấp QueryPlan chỉ lưu tên (`Plan warnings`), mất thuộc tính `ConvertIssue`/`Expression` của `PlanAffectingConvert`; `OutputList` bị bỏ qua (cần cho INCLUDE của Key Lookup).
  - PG: key điều kiện bị xoá khỏi `extra` sau khi nối vào `condition`; `JIT` (cấp gốc) không đọc; `Plan Width`, `Output` (VERBOSE) không giữ.
  - Cockroach: khối `index recommendations:` ở footer bị bỏ qua.
  - Mongo: `executionStats.totalKeysExamined/totalDocsExamined` không đưa vào cây (stage có `keysExamined/docsExamined`).
- `dbstudio-sql`: chỉ có lexer (`tokenize`, `TokenKind{Word,Number,String,QuotedIdent,Punct,...}`), splitter, classifier; **không có AST**. `Dialect::quote_ident` có sẵn cho mọi dialect.
- Metadata: `Session::table_details(path) -> TableDetails{columns{name,type_name,nullable,..}, indexes{name, columns (thứ tự, tên trần, biểu thức là "(expr)"), unique, primary, method, definition, partial}, estimated_rows, size_bytes}` — có ở PG, MySQL, MSSQL, Oracle, Mongo, ClickHouse, Cassandra. `Session::search_objects{query, scope, kinds, limit}` để định vị `ObjectPath`. `server_info().extra["edition"]` (MSSQL).
- UI: `PlanTab` (header 1 hàng `flex-wrap` với ~7 nút), `PlanDetails` cố định `w-72`, `PlanTable` `min-w-[760px]`, `PlanGraph` ELK `direction RIGHT`, `fitView` + `minZoom 0.1`; chọn node đã tự `setCenter` với zoom ≥ 0.6. `PlanCompare` so 2 plan đang mở. Chưa có UI lịch sử plan (chỉ IPC).

---

## 2. Mô hình dữ liệu

### 2.1 Rust (`crates/dbstudio-plan/src/advice/model.rs`, re-export ở `lib.rs`)

```rust
/// One optimisation opportunity with its evidence and concrete actions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanAdvice {
    /// Stable per plan: `"{rule}:{fingerprint-hash8}"`; React key.
    pub id: String,
    pub rule: AdviceRule,
    pub severity: Severity,                  // reuse
    pub confidence: Confidence,
    /// Nodes concerned, most relevant first (empty for statement-only advice).
    pub node_ids: Vec<u32>,
    /// Table the advice is about (index/statistics advice).
    #[serde(default)] pub table: Option<TableRef>,
    /// Values for `advice.rules.<rule>.title` / `.why` (`table`, `columns`, `index`...).
    pub params: BTreeMap<String, String>,
    /// Figures from the plan shown under "why".
    pub evidence: Vec<Evidence>,
    pub actions: Vec<AdviceAction>,
    pub caveats: Vec<Caveat>,
    pub impact: Impact,
    /// Warnings this advice answers.
    #[serde(default)] pub from_warnings: Vec<PlanRule>,
    /// Character range in `RawPlan.statement` for statement rules.
    #[serde(default)] pub statement_range: Option<TextRange>,
    /// Plan-independent identity for compare: rule + table + columns/expression, lower-cased.
    pub fingerprint: String,
}

#[serde(rename_all = "camelCase")] pub enum Confidence { High, Medium, Low }

pub struct TableRef { pub database: Option<String>, pub schema: Option<String>,
                      pub name: String, pub alias: Option<String>,
                      /// Resolved by `plan_advise` (enables "Open table").
                      #[serde(default)] pub path: Option<ObjectPath> }

pub struct Evidence {
    pub key: String,                         // i18n `advice.evidence.<key>`: rowsRemovedByFilter...
    pub node_id: Option<u32>,
    pub value: Option<f64>, pub text: Option<String>,
    pub unit: Option<EvidenceUnit>,          // rows|ms|bytes|blocks|pct|ratio|count|kb
    pub actual: bool,                        // true = measured (ANALYZE), false = estimate
}

pub struct AdviceAction {
    pub kind: ActionKind,    // createIndex|statistics|sessionSetting|maintenance|rewrite|hint|check|serverSetting
    pub label_key: String,   // i18n `advice.actions.<labelKey>`
    pub sql: Option<String>, // ready text in the connection's language (SQL, mongo shell)
    pub template: bool,      // has placeholders (`:last_id`, `/* columns */`): never "ready to run"
    pub scope: ActionScope,  // query|session|object|database|server
    pub lock: Option<LockRisk>, // none|shortLock|blocksWrites|blocksReadsWrites|heavyMutation
    #[serde(default)] pub params: BTreeMap<String, String>,
}

pub struct Caveat { pub key: String, pub verify: bool, #[serde(default)] pub params: BTreeMap<String,String> }

pub struct Impact {
    pub basis: ImpactBasis,                  // time|cost|rows|none
    /// Share of the whole statement attributed to `node_ids` (0..=1); `None` when unknown.
    pub share: Option<f64>,
    /// Ranking score (§3.4), stored so UI and tests agree.
    pub score: f64,
}

/// Role of a condition piece, filled by the parsers (A4).
pub struct NodePredicate { pub role: PredicateRole, pub text: String }
pub enum PredicateRole { IndexCond, Filter, JoinCond, JoinFilter, Recheck, SortKey, GroupKey, Other }
```

- `AdviceRule`: enum camelCase, danh sách đầy đủ ở §4 (cột "rule"); biến thể cuối `#[serde(other)] Unknown` để JSON mới hơn vẫn đọc được.
- `PlanNode.predicates`: `#[serde(default, skip_serializing_if = "Vec::is_empty")]` → golden JSON cũ không đổi cho tới khi parser điền; khi parser điền thì cập nhật golden bằng `write_or_check` (ghi rõ trong PR).
- `PlanRules` thêm ngưỡng advisor (đều `#[serde(default)]`, struct vẫn `Copy`):

| Field | Mặc định | Dùng ở |
|---|---|---|
| `filter_min_examined` | 10 000 | full scan/filter: số dòng đọc tối thiểu |
| `filter_max_selectivity` | 0.10 | tỉ lệ dòng giữ lại tối đa để gợi ý index |
| `residual_ratio` | 10.0 | đọc/trả ≥ ratio (index chưa đủ cột, Mongo examined) |
| `heap_fetch_ratio` | 0.10 | PG Heap Fetches / rows |
| `large_offset` | 10 000 | OFFSET lớn |
| `min_share` | 0.05 | dưới mức này hạ severity xuống `info` |
| `max_advice` | 15 | cắt danh sách sau xếp hạng |

### 2.2 Quan hệ với `PlanWarning`
- `analyze()` giữ nguyên (thứ tự, ngưỡng, JSON). `advise()` chạy **sau** `analyze()` và đọc `tree.warnings` làm một nguồn tín hiệu.
- Ánh xạ bắt buộc (test: mọi warning của mọi fixture có advice tương ứng): `FullScanLargeTable → indexForFilter` (hoặc rule dialect/`chPrimaryKeyUnused`/`mongoCollscan`; nếu không trích được cột: `fullScanReview` mức low); `Misestimate → staleStatistics`; `SpillToDisk → sortSpill|hashSpill` (theo dialect); `NestedLoopManyRows → nestedLoopInnerScan` (hoặc `staleStatistics` khi outer bị ước lượng thấp).
- Plan history: cột `warnings` vẫn là số warning. Plan file `.dbplan.json` chỉ chứa `RawPlan` → không đổi.

### 2.3 TS (`src/lib/types/plan.ts`) và IPC
- Thêm interface tương ứng; `PlanTree.advice: PlanAdvice[]` (đọc `?? []` ở mọi chỗ dùng, vì mock cũ có thể thiếu); `PlanNode.predicates?: NodePredicate[]`; `AdviceRule` là union chuỗi; `PlanRules` thêm field optional.
- IPC mới (§7): `planAdvise(sessionId, leaseId, raw, database, rules) → AdviceReport`.
- `contract.test.ts`: thêm shape `PlanAdvice`; fixture `plan` thêm khoá `PlanAdvice`.

---

## 3. Engine

### 3.1 Bố cục module
```
crates/dbstudio-plan/src/advice/
  mod.rs          advise(tree, statement, ctx) -> Vec<PlanAdvice>; xếp hạng, gộp, cắt
  model.rs        kiểu §2.1
  context.rs      AdvisorContext, TableMeta, ServerFacts
  predicate.rs    mini-parser điều kiện → Conjunct (§5.1); Mongo filter JSON
  index.rs        IndexCandidate → DDL theo dialect, đặt tên, đối chiếu index có sẵn (§5)
  common.rs       rule dùng chung theo PlanNodeKind + warning
  pg.rs mysql.rs mssql.rs oracle.rs mongo.rs clickhouse.rs newsql.rs (Cockroach/TiDB/YB)
  statement.rs    rule phân tích tĩnh câu lệnh (§4.8)
tests/advice.rs + tests/advice_fixtures/<fixture>.advice.json   (golden)
```

```rust
pub struct AdvisorContext<'a> {
    pub rules: &'a PlanRules,
    pub dialect: ScriptDialect,          // từ session; mặc định suy từ PlanFormat (dialect_of_format)
    pub server_version: &'a str,         // RawPlan.server_version
    pub server: ServerFacts,             // edition MSSQL/Oracle (Option), từ server_info
    pub tables: BTreeMap<TableKey, TableMeta>,   // rỗng ở tầng 1
    pub metadata: MetadataState,         // NotRequested | Checked | Partial | Unavailable
}
pub struct TableMeta { pub path: ObjectPath, pub estimated_rows: Option<u64>,
                       pub columns: Vec<ColumnDetail>, pub indexes: Vec<IndexDetail> }
```
`parse_and_analyze(raw, rules)` gọi `advise(&tree, &raw.statement, &AdvisorContext::basic(raw, rules))`. Thêm `parse_analyze_advise(raw, rules, ctx)` cho `plan_advise`.

### 3.2 Luồng một rule
1. Duyệt cây (pre-order), mỗi node gọi `common::*` rồi rule dialect theo `tree.format` (`PostgresJson`, `CockroachText`→PG+crdb, YB dùng `PostgresJson`→PG; `MySqlJson*`/`MySqlTree`/`MariaDbJson`/`TiDbRows`→mysql(+tidb); ...).
2. Rule trả `Draft { rule, node_ids, table, evidence, candidate: Option<IndexCandidate>, actions, caveats, confidence, severity }`.
3. `index.rs` biến `candidate` thành action `createIndex` (hoặc đổi thành "index đã có" — §5.4).
4. Sau cùng `statement.rs` chạy trên `RawPlan.statement`, tăng/giảm tin cậy theo plan (vd. hàm trên cột + full scan bảng đó ⇒ high).

### 3.3 Sửa parser cần thiết (mỗi gói làm phần dialect của mình)
| Parser | Thay đổi |
|---|---|
| postgres.rs | Điền `predicates` từ `Index Cond/Recheck Cond/Hash Cond/Merge Cond/Join Filter/Filter/One-Time Filter` + `Sort Key/Group Key/Presorted Key`; giữ `Plan Width` và `Output` (nếu có) vào `extra`; đưa `JIT.Timing.Total` → `root.extra["JIT Total"]`. |
| mysql.rs / mariadb.rs | `predicates`: `index_condition`→IndexCond, `attached_condition`→Filter (JSON); tree text: dòng `Filter:`→Filter, `Index lookup ... (col=...)`→IndexCond, `Sort:`→SortKey. |
| mssql.rs | `MissingIndexes` → `root.extra["missingIndexes"]` = JSON compact `[{impact, database, schema, table, equality[], inequality[], include[]}]` (tên đã bỏ `[]`); `PlanAffectingConvert` → `root.extra["planAffectingConvert"]` = JSON `[{issue, expression}]`; `ColumnsWithNoStatistics`, `NoJoinPredicate`, `MemoryGrantWarning`, `SpillToTempDb/SortSpillDetails/HashSpillDetails` giữ thuộc tính; `OutputList` → `extra["Output"]` (danh sách cột); `predicates` từ `SeekPredicates`(IndexCond)/`Predicate`(Filter)/`HashKeysProbe`(JoinCond). |
| oracle.rs | `predicates` từ `ACCESS_PREDICATES`(IndexCond)/`FILTER_PREDICATES`(Filter); giữ `LAST_STARTS`. |
| mongo.rs | Root: `totalKeysExamined`, `totalDocsExamined`, `nReturned`; stage `$lookup`: `strategy`, `from`, `foreignField`. |
| cockroach.rs | Footer `index recommendations: N` → `root.extra["crdbIndexRecommendations"]` = JSON `[{type, sql}]`. |
| tidb.rs / clickhouse.rs | Đã đủ (`operator info` chứa `stats:pseudo`; granule/parts/keys). Chỉ điền `predicates` (TiDB `Selection` → Filter). |

### 3.4 Tác động và xếp hạng
- `share`: plan ANALYZE có `execution_time_ms` → `Σ self_time_ms(node_ids và con trực tiếp Filter/Fetch/Key Lookup gắn với nó) / execution_time_ms`; không có thời gian → `Σ self_cost / totals.total_cost`; ClickHouse → `granules_selected` của node / tổng granule; static rule không gắn node → `None`.
- Advice thống kê (`staleStatistics`): share = **thời gian inclusive của node cha** (ước lượng sai làm hỏng lựa chọn join phía trên).
- `score = share.unwrap_or(0.02) × w_conf × w_sev`, `w_conf = {high 1.0, medium 0.7, low 0.4}`, `w_sev = {critical 1.3, warning 1.0, info 0.6}`. Sắp giảm dần theo `score`, hoà thì theo `node_ids[0]`.
- Severity: `share ≥ 0.5` và (≥ 1 triệu dòng đọc hoặc ≥ 1 s) ⇒ critical; `share < min_share` ⇒ info; còn lại theo rule.
- Gộp: cùng `fingerprint` ⇒ một advice (hợp `node_ids`, `fromWarnings`, lấy evidence của node có share lớn nhất). Hai đề xuất index trên cùng bảng mà một cái là tiền tố của cái kia ⇒ giữ cái dài, thêm caveat `coversOtherAdvice`. Cắt `max_advice`.

### 3.5 Tin cậy (áp theo thứ tự, không xuống dưới `low`)
1. Khởi điểm do rule đặt (bảng §4).
2. Plan ước tính (không ANALYZE) mà rule dựa vào số dòng ⇒ −1, caveat `estimatedOnly{verify}` ("chạy Analyze để xác nhận").
3. Rule cần metadata nhưng `metadata != Checked` cho bảng đó ⇒ −1, caveat `metadataUnavailable{verify}`.
4. Plan từ history/file (`RawPlan.captured_at` cũ hơn 24 h) ⇒ caveat `planMayBeStale` (không hạ).
5. Tên bảng không định vị được duy nhất ⇒ caveat `ambiguousTable{verify}`, bỏ "Mở bảng".

---

## 4. Danh mục rule

Ký hiệu: **T** = tin cậy khởi điểm; `rows_removed_total = Rows Removed by Filter × Actual Loops` (PG báo trung bình mỗi loop — dễ sai nếu quên nhân); "sel" = dòng giữ lại / dòng đọc. Mọi ngưỡng lấy từ `PlanRules`. Hành động `createIndex` sinh theo §5.

### 4.0 Rule chung (mọi dialect có cây đủ số liệu)
| rule | Dấu hiệu & ngưỡng | Bằng chứng | Hành động | T | Giới hạn / false positive |
|---|---|---|---|---|---|
| `indexForFilter` | `FullScan` có predicate Filter trích được ≥1 cột; đọc ≥ `filter_min_examined`; sel ≤ `filter_max_selectivity` (ANALYZE: số đo; ước tính: `est_rows / table.estimated_rows`) | dòng đọc, dòng trả, sel %, self time/share, bảng | `createIndex` (eq → range → include) | high (ANALYZE), medium | Bảng nhỏ/ít trang: scan rẻ hơn; báo cáo chạy 1 lần; cột ít giá trị khác nhau; bảng ghi nhiều (index tốn ghi → caveat `writeOverhead`). |
| `indexResidualFilter` | Node index (seek/scan) có Filter dư: đọc/trả ≥ `residual_ratio` và đọc ≥ 10 000 | index đang dùng, dòng đọc/trả | `createIndex` = cột index hiện tại + cột filter (hoặc mở rộng, §5.4) | medium | Không biết cột index hiện tại khi thiếu metadata ⇒ low. |
| `indexForSort` | `Limit` → `Sort` → một bảng; input ≥ 10× output và ≥ 10 000; hoặc sort spill trên một bảng | sort key, input rows, limit | `createIndex` (eq cột lọc + cột sort theo đúng chiều ASC/DESC) | medium | Sort trên kết quả join nhiều bảng: không gợi ý index. |
| `nestedLoopInnerScan` | NL có `NestedLoopManyRows` và phía inner là FullScan hoặc index với filter dư | loops, rows/loop, thời gian inner | `createIndex` trên cột join của bảng inner (từ JoinFilter/Filter tham chiếu alias outer) | medium | Nếu outer bị ước lượng thấp ≥ 10× ⇒ chuyển sang `staleStatistics` (gốc rễ là thống kê). |
| `staleStatistics` | `Misestimate` trên node scan/join | ước tính vs thực tế, hệ số, hướng | Lệnh cập nhật thống kê bảng (theo dialect, §4.x) | medium | Ước lượng sai do biểu thức/hàm/CTE: thống kê không giúp ⇒ caveat `estimateFromExpression`, T low. |
| `sortSpill` / `hashSpill` | `SpillToDisk` | dung lượng tràn, method, batches | Tăng bộ nhớ **phiên** (theo dialect) + gợi ý index sort nếu một bảng | medium | Không bao giờ sinh câu đặt toàn cục; nhắc bộ nhớ nhân theo số node × worker. |
| `fullScanReview` | `FullScanLargeTable` nhưng không trích được cột (filter phức tạp/không có WHERE) | dòng đọc | `check`: "xem lại điều kiện lọc / có cần đọc toàn bộ?" | low | Truy vấn tổng hợp toàn bảng là hợp lệ. |

### 4.1 PostgreSQL (dùng lại cho YugabyteDB YSQL; Cockroach ở §4.7)
| rule | Dấu hiệu (trường JSON) & ngưỡng | Hành động cụ thể | T | Giới hạn |
|---|---|---|---|---|
| `indexForFilter` | `Seq Scan`/`Parallel Seq Scan` + `Filter`; `Rows Removed by Filter`×loops (+ rows) ≥ 10k, sel ≤ 10% | `CREATE INDEX CONCURRENTLY IF NOT EXISTS ix_orders_customer_id ON public.orders (customer_id);` | high | Parallel Seq Scan nhân `Workers Launched+1` khi tính dòng đọc (parser đã chia loops). |
| `indexResidualFilter` | `Index Scan`/`Bitmap Heap Scan` có `Filter` và `Rows Removed by Filter` ≥ 10× rows | Index tổ hợp (cột Index Cond + cột Filter) | medium | Bitmap: xem thêm `pgBitmapLossy`. |
| `pgBitmapLossy` | `Lossy Heap Blocks` > 0 và `Rows Removed by Index Recheck` ≥ 10k | `SET work_mem = '64MB';` (phiên; giá trị = luỹ thừa 2 ≥ 4× hiện tại, trần 1GB) | medium | — |
| `pgHeapFetches` | `Index Only Scan`, `Heap Fetches` ≥ 1000 và ≥ `heap_fetch_ratio` × rows | `VACUUM (ANALYZE) public.orders;` (object); tham khảo `ALTER TABLE public.orders SET (autovacuum_vacuum_scale_factor = 0.02);` (object, template) | medium | Replica: visibility map theo primary; bảng ghi liên tục. VACUUM không khoá ghi nhưng tốn I/O. |
| `sortSpill` | `Sort Method: external merge`/`Sort Space Type: Disk` | `SET work_mem = '<2×Sort Space Used, làm tròn lên luỹ thừa 2>MB';` (+ `SET LOCAL` nếu trong transaction) | medium | Kích thước đĩa < bộ nhớ cần (~2–3×). |
| `hashSpill` | `Hash Batches` > 1 (hoặc > `Original Hash Batches`), `HashAgg Batches` > 1, `Disk Usage` > 0 | `SET work_mem = '<Peak Memory Usage × Hash Batches>kB';` PG13+: nhắc `hash_mem_multiplier` (phiên) | medium | Ước lượng thấp ở inner ⇒ ưu tiên `staleStatistics`. |
| `staleStatistics` | `Misestimate` | `ANALYZE public.orders;`; nhiều cột cùng bảng trong một điều kiện ⇒ `CREATE STATISTICS st_orders_a_b (dependencies, ndistinct) ON a, b FROM public.orders; ANALYZE public.orders;` (low); cột lệch ⇒ `ALTER TABLE ... ALTER COLUMN c SET STATISTICS 1000;` (low) | medium | Function Scan/CTE: không có thống kê. |
| `pgFunctionOnColumn` | Filter có `lower((email)::text)`, `date_trunc('day', created_at)`, `(created_at)::date`, `abs(x)` bao cột, trên Seq Scan | (1) viết lại thành khoảng: `created_at >= '2024-05-01' AND created_at < '2024-05-02'` (template); (2) expression index `CREATE INDEX CONCURRENTLY ix_users_lower_email ON public.users ((lower(email)));` | high nếu Seq Scan | `(varchar_col)::text` là cast vô hại — **không** báo (§5.1). |
| `implicitConversion` | Cast phía cột khác họ kiểu: `(int_col)::numeric`, `(id)::text = '...'` với cột không phải text (metadata) | Viết lại literal/tham số đúng kiểu cột | medium (cần metadata), low | Thiếu metadata chỉ báo khi cast sang `numeric`/`text` từ cột tên kiểu `*_id`. |
| `pgParallelNotLaunched` | `Workers Launched` < `Workers Planned` | `check`: `SHOW max_parallel_workers; SHOW max_worker_processes;` — mô tả phạm vi **server**, không sinh SET | low | Tải tạm thời. |
| `pgJitOverhead` | `JIT Total` ≥ 20% `Execution Time` và ≥ 50 ms | `SET jit = off;` (phiên); mô tả `jit_above_cost` (server, chỉ chữ) | medium | Truy vấn OLAP lớn có lợi từ JIT. |
| `pgColdCache` | (BUFFERS) `Shared Read Blocks` ≥ 10× `Shared Hit Blocks` và ≥ 10 000 block | Info: "chạy lại để đo khi cache nóng"; không có SQL | low | Không phải điểm tối ưu; severity luôn `info`. |
| `pgNotInSubplan` | `Filter: (NOT (SubPlan N))` không `hashed` (chạy mỗi dòng) | Rewrite `NOT EXISTS` (§4.8 `notInSubquery`) | high | Ngữ nghĩa NULL (caveat `semanticsDiffer`). |

YugabyteDB: cột đầu khoá index mặc định **HASH** ⇒ khi cột range đứng đầu, sinh `(created_at ASC)`; không dùng `CONCURRENTLY` (YB tự backfill online, `CREATE INDEX NONCONCURRENTLY` mới khoá) — nhận diện qua `RawPlan.server_version` chứa `-YB-`.

### 4.2 MySQL / MariaDB (TiDB ở §4.7)
| rule | Dấu hiệu (JSON v1/v2, tree, MariaDB ANALYZE) & ngưỡng | Hành động cụ thể | T | Giới hạn |
|---|---|---|---|---|
| `indexForFilter` | `access_type: ALL` + `attached_condition`; `rows_examined_per_scan` ≥ 10k; `filtered` ≤ 10 (MariaDB `r_filtered`; tree: `Table scan ... rows=` vs `Filter ... actual rows=`) | `` ALTER TABLE `shop`.`orders` ADD INDEX `ix_orders_customer_id` (`customer_id`), ALGORITHM=INPLACE, LOCK=NONE; `` | high (ANALYZE), medium | `filtered` là ước tính thô của optimizer. |
| `mysqlPossibleKeysUnused` | `possible_keys` không rỗng nhưng không có `key` và `access_type: ALL` | `` ANALYZE TABLE `shop`.`orders`; `` rồi chạy lại Explain; template kiểm `SELECT ... FROM t FORCE INDEX (ix)` (hint, chỉ để so sánh) | medium | Optimizer đúng khi sel cao. |
| `indexResidualFilter` | `access_type` `ref/range` với `filtered` ≤ 10 và `attached_condition` có cột ngoài `used_key_parts` | Index tổ hợp = `used_key_parts` + cột filter | medium | — |
| `indexForSort` | `ordering_operation.using_filesort: true` (tree `Sort:`) với ≥ 10k dòng hoặc có LIMIT | Index eq + cột ORDER BY (MySQL 8 hỗ trợ `DESC` trong index; 5.7 bỏ qua DESC → caveat) | medium | — |
| `mysqlTempTableGroup` | `grouping_operation.using_temporary_table: true` / tree `Aggregate using temporary table` với ≥ 10k dòng | Index trên cột GROUP BY (eq trước) | medium | GROUP BY biểu thức/nhiều bảng. |
| `mysqlJoinBuffer` | `using_join_buffer: "hash join"`/`"Block Nested Loop"` trên bảng `ALL` | Index cột join của bảng đó | medium | Hash join 8.0.18+ có thể đã đủ nhanh với bảng nhỏ. |
| `mysqlIndexMerge` | `access_type: index_merge` với `intersect(...)` | Index tổ hợp các cột trong intersect | medium | `union(...)` (OR) ⇒ không gợi ý. |
| `sortSpill` | MariaDB `r_used_priority_queue: false` + nhiều dòng (warning hiện có); MySQL không thấy spill trong plan | `SET SESSION sort_buffer_size = 8388608;` (phiên) + `check`: `SHOW SESSION STATUS LIKE 'Sort_merge_passes';` | low | MySQL: chỉ là giả thuyết. |
| `staleStatistics` | `Misestimate` (tree ANALYZE/MariaDB) | `` ANALYZE TABLE `shop`.`orders`; ``; cột lệch: MySQL 8 `` ANALYZE TABLE ... UPDATE HISTOGRAM ON `status` WITH 64 BUCKETS; ``, MariaDB `` ANALYZE TABLE ... PERSISTENT FOR COLUMNS (`status`) INDEXES (); `` | medium | — |
| `implicitConversion` | (metadata) cột chuỗi so với số (`code = 123`) hoặc ngược lại trong condition | Viết lại literal đúng kiểu (`code = '123'`) | medium | Collation lệch giữa cột join: chưa phát hiện (metadata chưa có collation) — ghi giới hạn. |

DDL MySQL: `ALGORITHM=INPLACE, LOCK=NONE` (MySQL 5.6+/MariaDB 10.0+); nếu server từ chối, thông báo lỗi của server là đủ rõ — caveat `onlineDdlMayFail`. Cột `TEXT/BLOB` cần độ dài tiền tố ⇒ sinh `` (`col`(191)) `` và caveat `prefixIndex`.

### 4.3 SQL Server
| rule | Dấu hiệu (showplan XML) & ngưỡng | Hành động cụ thể | T | Giới hạn |
|---|---|---|---|---|
| `mssqlMissingIndex` | `QueryPlan/MissingIndexes/MissingIndexGroup@Impact` ≥ 30 | `CREATE NONCLUSTERED INDEX [IX_Orders_CustomerId_OrderDate] ON [dbo].[Orders] ([CustomerId], [OrderDate]) INCLUDE ([Total]) WITH (ONLINE = ON);` — EQUALITY → INEQUALITY → INCLUDE | medium | Gợi ý của optimizer: không xét index có sẵn, thứ tự cột EQUALITY tuỳ ý, INCLUDE phình (> 8 cột ⇒ bỏ INCLUDE, caveat `includeTrimmed`). Đối chiếu metadata bắt buộc. |
| `mssqlKeyLookup` | `Key Lookup`/`RID Lookup` có executions (loops) ≥ 1000 hoặc share ≥ 10% | Tạo lại index đang seek (`object.index` của node anh em) với INCLUDE = `Output` của lookup: `CREATE NONCLUSTERED INDEX [IX_x] ON [dbo].[T] (<cột cũ>) INCLUDE (<cột lookup>) WITH (DROP_EXISTING = ON, ONLINE = ON);` | medium (high khi metadata cho cột cũ) | Thiếu metadata: không biết cột cũ ⇒ sinh index mới thay vì DROP_EXISTING. |
| `implicitConversion` | `PlanAffectingConvert@ConvertIssue="Seek Plan"` (warning), `"Cardinality Estimate"` (info); `Expression` chứa `CONVERT_IMPLICIT(nvarchar(..),[t].[code])` | Đổi kiểu tham số/literal cho khớp cột: bỏ tiền tố `N'..'` khi cột `varchar`, hoặc khai báo tham số `varchar(n)`; ứng dụng: chỉnh kiểu tham số driver | high (Seek Plan) | — |
| `sortSpill`/`hashSpill` | `SpillToTempDb`, `SortSpillDetails`, `HashSpillDetails`, `ExchangeSpillDetails` | `UPDATE STATISTICS [dbo].[T];` + template `OPTION (MIN_GRANT_PERCENT = 10)` (query, hint, low) | medium | 2019+: memory grant feedback tự điều chỉnh sau vài lần chạy. |
| `mssqlNoJoinPredicate` | `Warnings/NoJoinPredicate` | `check`: thiếu điều kiện join (tích Descartes) | high, critical | — |
| `mssqlNoStatistics` | `ColumnsWithNoStatistics` | `CREATE STATISTICS [st_T_col] ON [dbo].[T] ([col]);` (object); mô tả `AUTO_CREATE_STATISTICS` (database, chỉ chữ) | high | — |
| `staleStatistics` | `Misestimate` | `UPDATE STATISTICS [dbo].[T] WITH FULLSCAN;` (bảng > 10 triệu dòng: `WITH SAMPLE 25 PERCENT`) | medium | FULLSCAN tốn I/O. |
| `indexResidualFilter` | Index Seek/Scan có `Predicate` và `ActualRowsRead` ≥ 10× actual rows | Index có thêm cột predicate (key) | medium | Chỉ có `ActualRowsRead` khi actual plan (2016 SP1+). |
| `indexForFilter` | `Clustered Index Scan`/`Table Scan` + `Predicate`, không có MissingIndex cho bảng đó | Index từ `predicates` | medium | — |
| `mssqlExcessiveGrant` | `MemoryGrantWarning@GrantWarningKind="ExcessiveGrant"` | Info + `UPDATE STATISTICS`; template `OPTION (MAX_GRANT_PERCENT = 5)` | low | Ưu tiên thấp, làm nếu kịp. |

`ONLINE = ON` chỉ khi `server.edition` là Enterprise/Developer/Evaluation hoặc Azure (EngineEdition 3, 5, 8); khác ⇒ bỏ mệnh đề + caveat `offlineIndexBuild` ("tạo index sẽ khoá bảng; chạy ngoài giờ cao điểm"). Không biết edition ⇒ giữ `ONLINE = ON` + caveat `onlineDdlMayFail`.

### 4.4 Oracle
| rule | Dấu hiệu (plan rows) & ngưỡng | Hành động cụ thể | T | Giới hạn |
|---|---|---|---|---|
| `indexForFilter` | `TABLE ACCESS FULL` + `FILTER_PREDICATES`; `CARDINALITY` (hoặc `LAST_OUTPUT_ROWS`) / `NUM_ROWS` (metadata `estimated_rows`) ≤ 10% và bảng ≥ 10k | `CREATE INDEX "IX_ORDERS_CUSTOMER_ID" ON "SHOP"."ORDERS" ("CUSTOMER_ID") ONLINE;` | medium (cần metadata), low | Không có số dòng đọc trong plan ⇒ phụ thuộc metadata. |
| `staleStatistics` | E-Rows (`CARDINALITY`) vs A-Rows (`LAST_OUTPUT_ROWS / LAST_STARTS`) ≥ hệ số; hoặc ghi chú `dynamic statistics used` trong `extra.xplan` | `BEGIN DBMS_STATS.GATHER_TABLE_STATS(ownname => 'SHOP', tabname => 'ORDERS', method_opt => 'FOR ALL COLUMNS SIZE AUTO', cascade => TRUE); END;` + dòng `/` | medium | Khối PL/SQL: splitter Oracle cần `/`. |
| `oracleCartesian` | `MERGE JOIN CARTESIAN` mà hai phía CARDINALITY > 1 | `check` điều kiện join | high | Có thể là cố ý với bảng 1 dòng. |
| `oracleIndexSkipScan` | `INDEX SKIP SCAN`/`INDEX FULL SCAN` chiếm share ≥ 20% | Index với cột lọc đứng đầu | medium | — |
| `indexResidualFilter` | `TABLE ACCESS BY INDEX ROWID [BATCHED]` có `FILTER_PREDICATES`, A-Rows con index ≥ 10× A-Rows bảng | Thêm cột filter vào index | medium | — |
| `implicitConversion` | Predicate có `TO_NUMBER("COL")`, `INTERNAL_FUNCTION("COL")`, `SYS_OP_C2C("COL")` | Viết lại literal đúng kiểu/NLS | high | `INTERNAL_FUNCTION` còn gặp với IN-list: chỉ báo khi bao cột đơn. |
| `sortSpill` | `LAST_TEMPSEG_SIZE`/`TEMPSEG_SIZE` > 0, `LAST_EXECUTION` có `PASS` | Info + mô tả `PGA_AGGREGATE_TARGET` (server, chỉ chữ); index cho sort nếu một bảng | low | Không sinh `ALTER SESSION SET workarea_size_policy`. |

`ONLINE` chỉ Enterprise Edition (đọc `server_info`); độ dài tên ≤ 30 nếu version < 12.2, ngược lại ≤ 128.

### 4.5 MongoDB
| rule | Dấu hiệu & ngưỡng | Hành động cụ thể | T | Giới hạn |
|---|---|---|---|---|
| `mongoCollscan` | Stage `COLLSCAN`, `docsExamined` ≥ 10k (planner-only: low) | `db.getSiblingDB("shop").orders.createIndex({ status: 1, createdAt: -1 }, { name: "status_1_createdAt_-1" })` theo **ESR** (Equality → Sort → Range) | high (stats), low | Collection nhỏ; filter `$ne/$nin/$exists:false` chọn lọc kém ⇒ không gợi ý. |
| `mongoExaminedRatio` | `totalKeysExamined/nReturned` hoặc `totalDocsExamined/nReturned` ≥ `residual_ratio`, có `FETCH.filter` | Index tổ hợp: field của IXSCAN `keyPattern` + field filter theo ESR | medium | — |
| `mongoBlockingSort` | Stage `SORT` (không có index cung cấp thứ tự), `usedDisk: true` hoặc ≥ 10k docs | Index có field sort (ESR) | medium | Aggregation `$sort` sau `$group` không index được. |
| `mongoLookupNoIndex` | `$lookup` với `strategy: "NestedLoopJoin"` (6.0+) | `db.getSiblingDB("shop").<from>.createIndex({ <foreignField>: 1 })` | high | Bản < 6.0 không có `strategy` ⇒ low. |
| `mongoUnanchoredRegex` | filter `$regex` không bắt đầu bằng `^` (hoặc có cờ `i`) | Chuyển sang Atlas Search/text index hoặc lưu trường chuẩn hoá | low | — |

Index build Mongo 4.2+ chỉ khoá độc quyền ngắn ở đầu/cuối ⇒ `lock = shortLock`.

### 4.6 ClickHouse
| rule | Dấu hiệu (`EXPLAIN json=1, indexes=1`) & ngưỡng | Hành động cụ thể | T | Giới hạn |
|---|---|---|---|---|
| `chPrimaryKeyUnused` | `ReadFromMergeTree`: stage PrimaryKey chọn hết granule (`granules_selected == granules_initial`) hoặc `Keys` rỗng; ≥ 1000 granule; node có điều kiện | (1) viết lại: thêm điều kiện trên **tiền tố ORDER BY key** (lấy từ metadata index `primary`); (2) `ALTER TABLE db.t ADD INDEX ix_col col TYPE bloom_filter GRANULARITY 4;` + `ALTER TABLE db.t MATERIALIZE INDEX ix_col;` (heavyMutation); (3) projection `ALTER TABLE db.t ADD PROJECTION p_col (SELECT * ORDER BY col);` + `MATERIALIZE PROJECTION` | medium (1), low (2,3) | Ước lượng dòng = granule × 8192 (không biết `index_granularity` thật). |
| `chPartitionNotPruned` | Stage MinMax/Partition: `Selected Parts == Initial Parts` ≥ 100 parts | Thêm điều kiện trên cột partition key | medium | — |
| `chSkipIndexUseless` | Stage Skip có tên, chọn ≥ 90% granule | Info: index bỏ qua dữ liệu không hiệu quả; cân nhắc đổi `TYPE`/`GRANULARITY` (không sinh DROP) | low | — |
| `chFinal` | Read có `FINAL` trên ≥ 1000 granule | `SET do_not_merge_across_partitions_select_final = 1;` (phiên, khi partition theo khoá) | low | Đúng ngữ nghĩa chỉ khi bản ghi trùng không vượt partition. |
| `chJoinRightLarge` | `Join`: phía phải đọc nhiều granule hơn phía trái | Đổi thứ tự (bảng nhỏ bên phải) hoặc `SET join_algorithm = 'grace_hash';` (phiên) | low | Không có số dòng thật. |

### 4.7 CockroachDB, TiDB, YugabyteDB
| Hệ | Dùng lại | Riêng |
|---|---|---|
| CockroachDB | `indexForFilter` (`spans: FULL SCAN` + `filter`, `estimated row count`), `staleStatistics`, `sortSpill` (`spilled to disk`) | `crdbIndexRecommendation`: dùng nguyên `SQL command` từ footer `index recommendations` (high, nguồn server); `crdbMissingStats` (`estimated row count ... (missing stats)`) ⇒ `CREATE STATISTICS s_orders FROM orders;`. DDL: `CREATE INDEX ix ON t (a, b) STORING (c);` (online, không cần CONCURRENTLY). |
| TiDB | `indexForFilter` (`TableFullScan` cop[tikv] dưới `Selection`), `indexResidualFilter` (`IndexLookUp` + `Selection` phía table), `staleStatistics`, `sortSpill` (`disk` > 0 ⇒ `SET tidb_mem_quota_query = ...` phiên) | `tidbPseudoStats`: `operator info` chứa `stats:pseudo` ⇒ `ANALYZE TABLE shop.orders;` (high). DDL: `CREATE INDEX ix ON t (a, b);` (online sẵn, bỏ ALGORITHM/LOCK). |
| YugabyteDB | Toàn bộ §4.1 | HASH/ASC và CONCURRENTLY như ghi chú §4.1. |

### 4.8 Phân tích tĩnh câu lệnh (`statement.rs`, mọi dialect SQL; lexer `dbstudio_sql::lexer::tokenize`)
Chỉ xét câu top-level (bỏ qua nội dung chuỗi/comment nhờ lexer). Mỗi rule ghi `statement_range` để UI tô đoạn mã. Nâng tin cậy khi plan xác nhận (cột thuộc bảng bị full scan); không có xác nhận ⇒ `low`, severity `info`.

| rule | Mẫu token | Hành động | T (có/không plan xác nhận) | Giới hạn |
|---|---|---|---|---|
| `nonSargablePredicate` | Trong WHERE/ON: `Word(` bao một colref rồi toán tử so sánh (`lower(email) =`, `YEAR(created_at) =`, `DATE(col) =`, `CAST(col AS ..)`, `col + 1 =`, `ISNULL(col, 0) =`) | Rewrite khoảng/đưa phép tính sang vế hằng; expression index: PG `((lower(email)))`, MySQL 8.0.13+ `((lower(email)))`, MSSQL cột tính toán + index, Oracle function-based index | high / low | Hàm trên vế hằng không sao. |
| `leadingWildcardLike` | `LIKE`/`ILIKE` + String bắt đầu `%` hoặc `_` | PG: `CREATE EXTENSION IF NOT EXISTS pg_trgm;` (database, cần quyền) + `CREATE INDEX CONCURRENTLY ix_t_col_trgm ON t USING gin (col gin_trgm_ops);`; MySQL: FULLTEXT + `MATCH ... AGAINST` (ngữ nghĩa khác); khác: không có index phù hợp | medium / low | — |
| `orAcrossColumns` | WHERE top-level `a = x OR b = y` (khác cột) và plan full scan bảng đó (PG: không có BitmapOr) | Template `UNION ALL` (nhánh 2 thêm `AND NOT (a = x)` hoặc dùng `UNION`) + index từng cột | medium / low | — |
| `selectStar` | `SELECT *`/`t.*` ở danh sách chọn ngoài cùng, và plan có Key Lookup/Heap Fetches nhiều, hoặc spill sort/hash | Liệt kê cột cần (template `/* columns */`) — cho phép index covering, giảm bộ nhớ sort | medium / không báo | Không báo khi plan không có dấu hiệu. |
| `largeOffset` | `OFFSET n` / `OFFSET n ROWS` / MySQL `LIMIT a, b` với n hoặc a ≥ `large_offset` | Keyset pagination: `WHERE (created_at, id) > (:last_created_at, :last_id) ORDER BY created_at, id LIMIT 50` (template) | high | Phân trang nhảy trang tuỳ ý thì không áp dụng được. |
| `notInSubquery` | `NOT IN ( SELECT col` | `NOT EXISTS (SELECT 1 FROM ... WHERE s.col = o.col)`; caveat `semanticsDiffer`: NOT IN trả **0 dòng** nếu subquery có NULL, NOT EXISTS thì không — thường câu gốc mới là lỗi | medium khi metadata nói `nullable`, low khi chưa biết; không báo khi `NOT NULL` | — |
| `typeMismatch` | colref so với literal khác họ kiểu theo metadata (`varchar = 123`, `int = '5'` trên MySQL; `uuid_col = 'x'` không sao ở PG) | Sửa literal | medium (chỉ tầng 2) | Bảng ánh xạ họ kiểu theo `ValueKind` của `ColumnDetail`. |

---

## 5. Sinh `CREATE INDEX`

### 5.1 Trích cột từ điều kiện (`predicate.rs`)
1. Lấy `node.predicates` (vai trò IndexCond/Filter/JoinCond/JoinFilter), tokenize theo `ctx.dialect`, bỏ ngoặc ngoài, tách theo `AND` ở độ sâu 0.
2. Phân loại mỗi conjunct:
   - `colref = (literal|param|colref khác alias)` → `Eq(col)` (vế kia là colref bảng khác ⇒ `Join(col)`); `colref IN (...)`, PG `colref = ANY ('{..}')` → `Eq`; `colref IS NULL` → `Eq`.
   - `<, <=, >, >=, BETWEEN`, `LIKE 'abc%'`, PG `~~ 'abc%'::text` → `Range(col)`.
   - `OR` ở độ sâu 0 → `Or` (chuyển rule `orAcrossColumns`); `Word(` bao colref → `NonSargable{func,col}`; `LIKE '%..'` → `NonSargable`.
   - colref: PG `col`, `alias.col`, `(col)`, `(col)::type`; MSSQL `[db].[dbo].[T].[col]`/`[t].[col]`; MySQL `` `db`.`t`.`col` ``; Oracle `"T"."COL"`; ClickHouse `col`. Cast PG `(col)::text` với cột `varchar/char/text` (metadata) hoặc khi không có metadata coi là **vô hại**; cast khác ⇒ đánh dấu cho `implicitConversion`.
3. Chỉ giữ cột của bảng node (không định danh, hoặc định danh bằng `object.alias`/`object.name`).
4. Mongo: phân tích JSON `filter` — field scalar/`$eq`/`$in` → Eq; `$gt/$gte/$lt/$lte` → Range; `$regex` neo `^` → Range; `$or/$nor/$ne/$nin/$exists` → bỏ; field sort từ `sortPattern` của stage SORT.
5. MSSQL `missingIndexes`: dùng trực tiếp (EQUALITY/INEQUALITY/INCLUDE), bỏ bước 1–3.

### 5.2 Thứ tự cột
`key = Eq... (thứ tự xuất hiện; MSSQL theo thứ tự server)` → `Range` (chỉ **một** cột, cột range đầu tiên) → nếu không có Range và rule là sort: các cột sort kèm chiều → `include` = cột output khi biết (PG `Output` khi VERBOSE, MSSQL `Output` của Key Lookup); MySQL/MariaDB/TiDB không có INCLUDE ⇒ nối vào cuối key **chỉ khi** tổng ≤ 5 cột và không phải TEXT/BLOB, ngược lại bỏ. Trần: 4 cột key, 6 cột include. Mongo: ESR (Equality → Sort → Range).

### 5.3 Cú pháp theo dialect
| Dialect | Mẫu | Ghi chú khoá |
|---|---|---|
| PostgreSQL | `CREATE INDEX CONCURRENTLY IF NOT EXISTS {name} ON {schema}.{table} ({cols}) [INCLUDE ({inc})];` | INCLUDE từ PG 11 (`server_version`); CONCURRENTLY không chạy được trong transaction ⇒ caveat `notInTransaction` khi plan đến từ editor có lease trong transaction; luôn giữ CONCURRENTLY kể cả bảng nhỏ (an toàn hơn, chỉ chậm hơn). |
| YugabyteDB | như PG, bỏ `CONCURRENTLY`, cột range đầu ghi `ASC` | backfill online |
| CockroachDB | `CREATE INDEX IF NOT EXISTS {name} ON {table} ({cols}) [STORING ({inc})];` | online |
| MySQL/MariaDB | `ALTER TABLE {db}.{table} ADD INDEX {name} ({cols}), ALGORITHM=INPLACE, LOCK=NONE;` | MySQL 5.6+ online |
| TiDB | `CREATE INDEX {name} ON {db}.{table} ({cols});` | online |
| SQL Server | `CREATE NONCLUSTERED INDEX {name} ON {schema}.{table} ({cols}) [INCLUDE ({inc})] [WITH (ONLINE = ON)];` | §4.3 edition |
| Oracle | `CREATE INDEX {name} ON {schema}.{table} ({cols}) [ONLINE];` | EE |
| MongoDB | `db.getSiblingDB("{db}").{coll}.createIndex({ {f}: 1, ... }, { name: "{name}" })` (coll không hợp lệ làm định danh JS ⇒ `getCollection("{coll}")`) | shortLock |
| ClickHouse | §4.6 (skip index/projection, không phải B-tree) | heavyMutation |

Quote: MySQL/MSSQL/ClickHouse luôn dùng `Dialect::quote_ident`; PG chỉ quote khi tên không khớp `^[a-z_][a-z0-9_$]*$` hoặc là từ khoá (`dbstudio_sql::vocabulary`); Oracle chỉ quote khi không khớp `^[A-Z][A-Z0-9_$#]*$`. Schema/database: lấy từ `TableRef` (plan) → metadata `path` → bỏ trống (không đoán).

### 5.4 Đặt tên và đối chiếu index có sẵn
- Tên: `ix_{table}_{col1}_{col2}`; MSSQL/Oracle theo kiểu `IX_Table_Col` (PascalCase giữ nguyên tên gốc), Mongo theo quy ước server `f1_1_f2_-1`. Ký tự ngoài `[A-Za-z0-9_]` → `_`. Vượt giới hạn (PG 63, MySQL 64, MSSQL 128, Oracle 30/128) ⇒ cắt và thêm `_{hash6}` (FNV-1a của danh sách cột). Trùng tên với index có sẵn (metadata) ⇒ thêm `_2`, `_3`.
- So khớp (không phân biệt hoa thường với MySQL/MSSQL/Mongo, PG/Oracle so sau khi bỏ quote; bỏ qua index `partial`, `method` khác btree/null, cột biểu thức `(expr)` trừ khi trùng chuỗi chuẩn hoá):
  1. Index có sẵn **bắt đầu bằng** key đề xuất (cho phép hoán vị trong nhóm Eq) ⇒ **không** sinh CREATE; đổi advice thành `existingIndexUnused` (params `index`), hành động: cập nhật thống kê, kiểm cast/collation (`implicitConversion`), kiểm sel; T medium.
  2. Key đề xuất **bắt đầu bằng** toàn bộ cột của một index có sẵn ⇒ sinh CREATE index mới + caveat `extendsIndex{index}` ("sau khi xác minh có thể bỏ index cũ"); không sinh DROP. MSSQL: dùng `WITH (DROP_EXISTING = ON)` cùng tên cũ khi index cũ không unique/không PK.
  3. Chỉ khác INCLUDE ⇒ như 2 (MSSQL DROP_EXISTING, PG tạo mới).
  4. Không khớp ⇒ CREATE bình thường.
- Không có metadata ⇒ vẫn sinh (PG có `IF NOT EXISTS` theo tên), T −1, caveat `metadataUnavailable`.

### 5.5 Metadata: lấy gì, qua API nào, chi phí
| Cần | API | Ghi chú |
|---|---|---|
| `ObjectPath` của bảng | `Session::search_objects{query: name, scope: path database (TableRef.database → `ExplainRequest.database` → root), kinds: [Table] (Mongo: [Collection]), limit: 50}` rồi lọc **trùng tên tuyệt đối** + schema (`TableRef.schema`; thiếu ⇒ ưu tiên schema mặc định: PG `public`, MSSQL `dbo`, Oracle user hiện tại) | ≥ 2 kết quả còn lại ⇒ `ambiguousTable`. |
| Cột, index, số dòng | `Session::table_details(path)` | PG/MSSQL: vài truy vấn catalog (ms). MySQL: `information_schema` có thể chậm trên server nhiều bảng. Mongo: `listIndexes` + đếm ước tính (+ có thể lấy mẫu schema). |
| Edition | `Session::server_info()` (chỉ MSSQL, Oracle) | 1 lần mỗi `plan_advise`. |

Giới hạn: tối đa **6 bảng** (theo `score` của advice cần bảng đó), song song 3, timeout **3 s/bảng**, tổng **8 s**; luôn dùng **Session** (pool), không dùng lease (không chen vào transaction của editor; caveat `leaseObjectsInvisible` khi plan chạy trên lease và bảng không tìm thấy). Không cache backend ở v1 (frontend gọi một lần mỗi `runId`). Setting `planAdvisorMetadata` (mặc định `true`) tắt hẳn tầng 2. Lỗi từng bảng ghi vào `AdviceReport.metadata.failed` và hạ tin cậy theo §3.5 — **không** làm hỏng toàn bộ.

---

## 6. An toàn
1. Không command nào của advisor thực thi SQL người dùng; `plan_advise` chỉ đọc catalog bằng API browse có sẵn (giống Explorer), không qua guard, không cần ConfirmToken, được phép trên kết nối production/read-only.
2. Hành động UI duy nhất: **Sao chép**, **Chèn vào editor** (không chạy), **Mở bảng**. Câu đã chèn khi chạy đi qua guard 5 bước như mọi câu khác: DDL trên production ⇒ ConfirmToken; kết nối `read_only` ⇒ guard chặn.
3. UI cảnh báo trước khi chèn: kết nối `readOnly` ⇒ banner "Kết nối chỉ đọc: câu này sẽ bị chặn khi chạy; dùng kết nối có quyền DDL"; `environment = production` ⇒ nhãn đỏ "Production" trên từng action có `kind ∈ {createIndex, maintenance, statistics}` + nhắc chạy ngoài giờ cao điểm.
4. Khoá khi tạo index: `lock` + caveat theo §4.3/§5.3; bảng ≥ 1 triệu dòng (`estimated_rows`) ⇒ caveat `largeTableBuild{rows}` (thời gian build, dung lượng, I/O); MySQL 5.5/không hỗ trợ online ⇒ `blocksWrites`.
5. Phạm vi: mỗi action có `scope`. `server`/`database` scope **không có `sql`** (chỉ mô tả + câu `SHOW/SELECT` kiểm tra); `session` scope ghi rõ "chỉ ảnh hưởng phiên hiện tại; nếu editor dùng pool, đặt trong editor có kết nối riêng (lease)".
6. Template có placeholder (`template = true`) hiện nhãn "Mẫu — cần sửa trước khi chạy"; nút "Chèn" vẫn có nhưng không chọn sẵn toàn bộ câu để tránh chạy nhầm.
7. Câu lệnh trong advice dựng từ tên định danh đã quote và literal lấy nguyên từ plan; không nối chuỗi người dùng khác. Plan history đã redact secret ⇒ advice không chứa secret mới.

---

## 7. Backend IPC

```rust
#[tauri::command]
pub async fn plan_advise(state, session_id: Uuid, lease_id: Option<Uuid>, raw: RawPlan,
                         database: Option<String>, rules: Option<PlanRules>) -> Result<AdviceReport>

#[serde(rename_all = "camelCase")]
pub struct AdviceReport {
    pub advice: Vec<PlanAdvice>,
    pub metadata: AdviceMetadata,
}
pub struct AdviceMetadata {
    pub state: MetadataState,                  // checked | partial | unavailable | disabled
    pub checked: Vec<TableRef>,                // có path
    pub failed: Vec<TableFailure>,             // { table, reason: "timeout"|"notFound"|"ambiguous"|"unsupported"|"error", message }
    pub skipped: u32,                          // vượt 6 bảng
    pub elapsed_ms: u64,
}
```
Luồng `plan_advise_impl`: lấy session (`state.sessions.get_with_config`) → dialect (lease nếu có) → `dbstudio_plan::parse`/`analyze` → `advise` tầng 1 để biết bảng cần → `collect_metadata` (§5.5) → `advise` lại với ngữ cảnh → trả. Lỗi `notFound` session ⇒ trả `Error::NotFound` (UI hiển thị như §9d). `plan_explain`/`plan_parse` không đổi chữ ký (tree đã có `advice` tầng 1). Mock `src/lib/mock/plan.ts`: `planAdvise` trả `tree.advice` + `metadata.state = 'checked'` với 1 bảng giả.

---

## 8. UI/UX

### 8.1 Bố cục PlanTab
```
┌ header (1 hàng; nút phụ gom vào menu ⋯ khi hẹp — §9b) ─────────────────────────────┐
├ dải tóm tắt: [Lightbulb] 3 điểm tối ưu · lớn nhất: Tạo index orders(customer_id) — node chiếm ~78% thời gian  [Xem] ┤
├ Tabs Diagram | Table | Raw ─────────────────────────────┬ Inspector (co giãn 240–480px, thu gọn) ┤
│                                                          │ [Gợi ý (3)] [Node]                   │
│                                                          │ thẻ gợi ý ...                        │
└──────────────────────────────────────────────────────────┴──────────────────────────────────────┘
```
- Dải tóm tắt chỉ hiện khi có advice: `advice.summary` = "{{count}} điểm tối ưu · lớn nhất: {{title}}{{shareText}}"; `shareText` chỉ khi `impact.share` có ("— node chiếm ~{{pct}} thời gian/cost"). Không có advice ⇒ "Không thấy điểm cần tối ưu rõ ràng" (muted, ẩn được). Trạng thái tầng 2 bên phải dải: "Đang kiểm tra index hiện có…" (spinner) / "Đã đối chiếu index của 2 bảng" / "Không lấy được metadata (timeout) — độ tin cậy đã hạ" (tooltip liệt kê `failed`).
- Inspector: tab **Gợi ý** mặc định khi `advice.length > 0`, ngược lại **Node**. Tab Node = nội dung `PlanDetails` hiện tại (warning của node + số liệu). Danh sách tất cả warning (đầu `PlanDetails` hiện nay) chuyển vào cuối tab Gợi ý dạng thu gọn "Cảnh báo thô (n)".
- Thẻ gợi ý (thu gọn/mở): dòng 1 icon severity + tiêu đề + nhãn tin cậy (`Tin cậy cao` xanh / `Trung bình` vàng / `Cần xác minh` xám viền đứt cho `low` hoặc có caveat `verify`) + `~78%`; mở ra: "Vì sao" (câu `why` + bảng evidence 2 cột, số định dạng locale, hậu tố "(ước tính)" khi `actual=false`), các action (khối mã đơn cách, nhãn phạm vi, nhãn khoá, nút **Sao chép**, **Chèn vào editor**), caveat (danh sách gạch đầu dòng), nút **Mở bảng** (khi `table.path`), đoạn mã câu lệnh tô `statement_range` cho rule tĩnh.
- Bấm thẻ ⇒ `select(node_ids[0])`; Diagram: dùng cơ chế `setCenter` hiện có + **zoom ≥ 0.9** và nhấp nháy viền các `node_ids` khác 1,5 s; Table: mở rộng tổ tiên đang thu gọn + `scrollIntoView({block:'center'})`; Raw: tô `locateRaw`. Node có advice hiện icon `Lightbulb` nhỏ (lucide) trên Diagram/Table (cạnh badge warning).
- Phím: `↑/↓` giữa các thẻ, `Enter` mở/đóng, `c` sao chép action đầu.

### 8.2 Hành động
- **Sao chép**: `navigator.clipboard.writeText(sql)`, toast "Đã sao chép".
- **Chèn vào editor**: `PlanEntry` thêm `sourceEditorId: string | null` (đặt khi `EditorTab.explain` gọi `openPlan`). Nếu editor đó còn mở và cùng `sessionId` ⇒ hàm mới `appendToEditor(tabStateId, text)` (`src/features/editor/open.ts`): chèn vào **cuối** tài liệu, cách 1 dòng trống, đặt selection bao đoạn vừa chèn (trừ khi `template`), kích hoạt tab, **không chạy**. Ngược lại ⇒ `openEditor(session, { text, database })`. Mongo ⇒ editor chế độ mongo shell (theo `editorModeOf(session)`).
- **Mở bảng**: `openObjectTab({ kind: Table|Collection, path: table.path, ... }, session)`.
- Plan không có session (mở file khi đã ngắt kết nối): Chèn mở editor yêu cầu chọn kết nối như `openEditorForActiveTab`; Mở bảng tắt.

### 8.3 Lịch sử và So sánh
- Mọi plan mở bằng `openRawPlan` (file, và UI lịch sử plan khi có) đi qua `plan_parse` ⇒ có advice tầng 1; nếu có session cùng `connectionId` đang mở ⇒ gọi `plan_advise` như plan mới, kèm caveat `planMayBeStale` khi `captured_at` > 24 h.
- `PlanCompare`: thêm mục "Gợi ý" trong dialog: ba nhóm theo `fingerprint` — **Đã hết** (có ở A, không ở B), **Mới**, **Còn lại**; `compareModel.ts` thêm `adviceDiff(a, b)` và dòng tổng `advice` (số lượng) trong `TotalDiff`.

### 8.4 i18n (`src/locales/{vi,en}/plan.json`, khoá dưới `advice`)
`title`, `summary`, `summaryNone`, `share.time|cost`, `tabs.advice|node`, `status.checking|checked|partial|unavailable|disabled`, `confidence.high|medium|low|verify`, `scope.query|session|object|database|server`, `lock.none|shortLock|blocksWrites|blocksReadsWrites|heavyMutation`, `actions.copy|insert|openTable|<labelKey>`, `template`, `readOnlyBanner`, `productionBadge`, `rawWarnings`, `rules.<rule>.title|why` (mọi `AdviceRule`), `evidence.<key>`, `caveats.<key>`, `compare.gone|new|kept`. Test Vitest: mọi giá trị của union `AdviceRule` có `title`+`why` ở **cả** vi và en; mọi caveat/evidence key dùng trong golden backend có bản dịch (đọc danh sách khoá từ `crates/dbstudio-plan/tests/advice_fixtures/keys.json` do test Rust sinh).
Ví dụ: `rules.indexForFilter.title` vi "Thêm index cho điều kiện lọc trên {{table}}", en "Add an index for the filter on {{table}}"; `why` vi "Quét toàn bảng {{table}} đọc {{examined}} dòng nhưng chỉ giữ {{kept}} ({{selectivity}}). Index trên {{columns}} cho phép đọc thẳng các dòng cần."

### 8.5 Setting
`AppSettings.plan_advisor_metadata: bool` (mặc định `true`, đọc lenient như field khác) — mục "Plan" trong Settings: "Đối chiếu index hiện có khi gợi ý (đọc catalog)".

---

## 9. Sửa kèm (lỗi UX thấy khi test thật)

| # | Lỗi | Nguyên nhân (code) | Sửa | Tiêu chí chấp nhận |
|---|---|---|---|---|
| a | Diagram lần đầu fit quá nhỏ, không đọc được node | `PlanGraph`: `fitView` + `minZoom 0.1`; ELK `direction RIGHT` nên plan sâu thành dải ngang dài | Viewport ban đầu: tính zoom fit; nếu < **0.65** ⇒ đặt zoom **0.85** và căn node gốc (cùng hot path) vào góc trên-trái (padding 24px) thay vì fit toàn bộ; thêm nút "Bố cục ngang/dọc" (`elk.direction` `RIGHT`/`DOWN`, nhớ theo người dùng trong UI prefs); bật MiniMap khi `nodeCount > 25`. Nút "Vừa khung" vẫn fit toàn bộ. | Fixture 40+ node (thêm `planTestData` lớn): sau render, `flow.getZoom() ≥ 0.65`; node gốc nằm trong viewport; test Vitest với ReactFlow mock + kiểm tay ở 1024×768. |
| b | Header plan chật, nút xuống dòng ở 1024px | Header `flex-wrap` gồm câu lệnh + 2 badge + 5 stat + 7 nút | Header một hàng không wrap: câu lệnh `flex-1 truncate`; stat ít quan trọng (Planning, Nodes) ẩn dưới container width 900px (Tailwind `@container`); nút chính giữ (Analyze/Estimate, Rerun, Cancel); Save/Open/Compare/Copy raw gom vào menu `⋯` khi container < 1100px, icon-only + tooltip khi 1100–1280px. | Ở cửa sổ 1024px (sidebar mặc định), header cao một hàng (≤ 36px), không nút nào xuống dòng; test RTL đặt width container và kiểm menu `⋯` chứa 4 mục. |
| c | ANALYZE: cột Actual rows/Loops/Self time/Share bị panel chi tiết che, phải cuộn ngang | `PlanDetails` cố định `w-72` + `PlanTable min-w-[760px]` trong vùng còn ~500px | Inspector (§8.1) co giãn bằng thanh kéo (240–480px), nút thu gọn (còn dải 28px có icon `Lightbulb` + số); **tự thu gọn** khi vùng plan < 1100px (nhớ lựa chọn người dùng nếu họ mở lại). Table: cột Object chuyển về cuối và `max-w-[12rem]`, cột Operation sticky trái, Share `w-20`; `min-w` tính theo cột thực có (ước tính 560px, ANALYZE 680px). | 1024×768, plan ANALYZE: không cuộn ngang vẫn thấy đủ Actual rows, Loops, Self time, Share (Inspector tự thu gọn); mở Inspector thì cột Operation vẫn sticky. |
| d | Session backend mất (`notFound`), Run/Explain chỉ báo "Nothing to run" | `EditorTab.start/explain`: `ipc.scriptAnalyze(...).catch(() => null)` nuốt lỗi ⇒ không có statement ⇒ `selectRunText` trả `null` ⇒ toast `run.nothing` | Không nuốt lỗi: lỗi `kind === 'notFound'` ⇒ toast lỗi "Phiên kết nối đã mất" + nút **Kết nối lại** (`ipc.sessionReconnect`) và đánh dấu session mất trong store phiên (như event `session://state`); lỗi khác ⇒ toast thông điệp lỗi thật. Chỉ báo "Không có gì để chạy" khi phân tích thành công mà vùng chọn rỗng. `runPlan`/`explainErrorMessage` xử lý `notFound` tương tự (trạng thái lỗi của PlanTab có nút Kết nối lại). | Test RTL: mock `scriptAnalyze` reject `{kind:'notFound'}` ⇒ thấy toast "Phiên kết nối đã mất" và nút Kết nối lại, **không** thấy "Nothing to run"; i18n vi/en. |

---

## 10. Kế hoạch triển khai (gói song song)

Mỗi gói một worktree `.claude/worktrees/<goi>`, nhánh `feature/quandh/<goi>` từ `develop` mới nhất (theo `~/.claude/CLAUDE.md`). Cột "File" là vùng **độc quyền**. Brief chung: thêm toolchain vào PATH (memory "Rust toolchain PATH"); bật service Docker theo profile và tắt sau khi xong.

### 10.1 Bước 0 — Hợp đồng (1 agent Sonnet, review Opus)
Kiểu §2.1 + `NodePredicate` + field `PlanTree.advice`/`PlanNode.predicates` + ngưỡng `PlanRules`; `advice/mod.rs` với `advise()` trả `vec![]` và khung module rỗng; `dbstudio-sql` vào `dbstudio-plan/Cargo.toml`; `parse_and_analyze` gọi `advise`; command `plan_advise` stub (tầng 1, `metadata.state = disabled`) đăng ký trong `lib.rs`; setting `plan_advisor_metadata`; TS types + IPC wrapper + mock; fixture `plan` thêm `PlanAdvice`; khoá i18n khung (`advice.title`, `summary`, ...). Sửa mọi chỗ dựng literal `PlanTree {`/`PlanNode {`.
**Xong khi**: `cargo test --workspace`, `cargo clippy --all-features`, `pnpm lint && pnpm test && pnpm build` xanh; golden cũ không đổi.

### 10.2 Các gói
| Gói | Phạm vi | File độc quyền | Phụ thuộc | Tiêu chí xong |
|---|---|---|---|---|
| **R1. Lõi engine + PG + tĩnh** | §3.2–3.5, `predicate.rs` (SQL + Mongo JSON), `common.rs`, `pg.rs`, `statement.rs`, parser PG (§3.3) | `advice/{mod,predicate,common,pg,statement}.rs`, `src/postgres.rs`, `tests/advice.rs`, `tests/advice_fixtures/pg_*` | Bước 0 | Golden advice cho 11 fixture PG + 4 fixture mới (`pg_12_index_residual`, `pg_13_heap_fetches`, `pg_14_bitmap_lossy`, `pg_15_function_filter`); ≥ 60 ca `predicate.rs` (proptest không panic); mọi warning có advice. |
| **R2. Index DDL + metadata** | `index.rs` (§5.2–5.4), `context.rs`, `plan_advise` thật (§5.5, §7) | `advice/{index,context}.rs`, `src-tauri/src/commands/plan.rs` (phần `plan_advise`) | Bước 0; dùng R1 qua trait nội bộ `Draft` (đã chốt ở Bước 0) | Snapshot DDL 9 dialect × (cơ bản, include, tên dài, quote, trùng tên); bảng ca §5.4 (1–4); test timeout/ambiguous với session giả (`dbstudio_core::testing`). |
| **R3. MySQL/MariaDB/TiDB** | `mysql.rs`, `newsql.rs` (phần TiDB), parser §3.3 | `advice/mysql.rs`, `src/{mysql,mariadb,tidb}.rs` | R1 (lõi) | Golden cho 23 fixture MySQL/MariaDB + 8 TiDB. |
| **R4. MSSQL + Oracle** | `mssql.rs`, `oracle.rs`, parser §3.3 | `advice/{mssql,oracle}.rs`, `src/{mssql,oracle}.rs` | R1 | Fixture mới `mssql_07_missing_index_convert.xml` (MissingIndexes + PlanAffectingConvert + Key Lookup Output), `oracle_07_skip_scan.json`; golden cho mọi fixture MSSQL/Oracle. |
| **R5. Mongo + ClickHouse + Cockroach/YB** | `mongo.rs`, `clickhouse.rs`, `newsql.rs` (crdb/YB), parser §3.3 | `advice/{mongo,clickhouse}.rs`, `src/{mongo,cockroach,clickhouse}.rs` | R1 | Golden 6 Mongo + 7 ClickHouse + 8 Cockroach; fixture crdb có footer index recommendations. |
| **F1. Panel gợi ý** | §8 toàn bộ (Inspector, dải tóm tắt, thẻ, hành động, compare, i18n, setting) | `src/features/plan/{PlanAdvice*.tsx,adviceModel.ts,PlanDetails.tsx,PlanCompare.tsx,compareModel.ts}`, `src/features/editor/open.ts` (`appendToEditor`), `src/locales/*/plan.json` | Bước 0 (mock đủ dữ liệu) | Vitest: xếp hạng, bấm thẻ chọn node, Copy/Insert/Open, nhãn tin cậy, read-only/production, trạng thái tầng 2, compare diff, i18n đủ khoá. |
| **F2. Sửa kèm UX** | §9 a–d | `src/features/plan/{PlanTab,PlanGraph,PlanTable,elkLayout}.tsx|ts`, `src/features/editor/EditorTab.tsx`, `src/locales/*/editor.json` | Bước 0; phối hợp F1 ở `PlanTab` (F2 sở hữu file, F1 cấp component `<Inspector>`/`<AdviceSummary>`) | Tiêu chí cột cuối §9. |
| **T. IT Docker** | §11.2 | `src-tauri/tests/it_plan_advice.rs` (hoặc `crates/drivers/*/tests/it_advice.rs`), seed SQL trong `docker/seed/advice/*` | R1–R5, R2 | Xanh trên `pg16`, `mysql8`, `mariadb`, `mssql2022`, `mongo7`, `clickhouse26`; `oracle-it`, `cockroach`, `tidb`, `yugabyte` probe-rồi-skip. |

Thứ tự merge: Bước 0 → R1 → (R2, R3, R4, R5, F1, F2 song song) → T → cập nhật §12 "khác biệt khi triển khai". Mỗi merge: `cargo test --workspace`, `cargo clippy --all-features -- -D warnings`, `pnpm test`.

---

## 11. Kiểm thử

### 11.1 Unit (không cần DB)
- `crates/dbstudio-plan/tests/advice.rs`: với mỗi fixture thật trong `tests/fixtures*` và `tests/clickhouse`, chạy `parse_and_analyze` (+ ngữ cảnh metadata giả từ `tests/advice_fixtures/<fixture>.meta.json` khi có) và so golden `<fixture>.advice.json` bằng `write_or_check` (`UPDATE_FIXTURES=1` để ghi lại). Golden gồm rule, severity, confidence, node_ids, params, sql, caveat keys, share làm tròn 3 số.
- Bất biến (chạy trên mọi fixture): mọi `PlanWarning` có advice với `fromWarnings` chứa rule đó; `advise` idempotent; không advice nào có `sql` khi `scope ∈ {server, database}` trừ `check`; mọi `createIndex` parse lại được bằng `dbstudio_sql::split` thành đúng 1 câu và `classify` = DDL; độ dài tên index ≤ giới hạn dialect; `score` giảm dần.
- `predicate.rs`: bảng ca theo dialect (PG cast vô hại/có hại, MSSQL `[..]` 4 phần + `CONVERT_IMPLICIT`, MySQL backtick, Oracle `"..."`, `ANY('{..}')`, `BETWEEN`, `IN`, OR lồng, chuỗi chứa `AND`), proptest chuỗi ngẫu nhiên không panic.
- `statement.rs`: ≥ 5 ca dương + 3 ca âm cho mỗi rule §4.8 (từ khoá trong chuỗi/comment không kích hoạt).
- `src-tauri` `plan.rs`: `plan_advise` với session giả: đủ metadata ⇒ `checked`; một bảng timeout ⇒ `partial` + hạ tin cậy; session mất ⇒ `NotFound`; setting tắt ⇒ `disabled`, không gọi `table_details`.

### 11.2 IT với Docker (`docker/compose.test.yml`)
Mẫu probe-rồi-skip (`DBSTUDIO_IT_STRICT=1` ở CI). Mỗi DB: seed bảng `orders` ~200k dòng (`customer_id`, `status`, `created_at`, `total`, `code varchar`) bằng lệnh sinh dữ liệu phía server (`generate_series`, recursive CTE MySQL 8, `numbers()` ClickHouse, `insertMany` lô 10k Mongo).
| Ca | DB | Kỳ vọng |
|---|---|---|
| Lọc không index | pg16, mysql8, mariadb, mssql2022, mongo7 | có `indexForFilter`/`mssqlMissingIndex`/`mongoCollscan`; **chạy câu `createIndex` sinh ra trên container**, Explain lại ⇒ không còn advice đó và node thành index scan/seek/IXSCAN (kiểm chứng đầu-cuối DDL đúng cú pháp và đúng cột). |
| Index đã có | pg16, mysql8 | sau bước trên, đổi điều kiện thành `lower(code) = ...` ⇒ `pgFunctionOnColumn`/`nonSargablePredicate`, không đề xuất trùng index. |
| Sort spill | pg16 (`SET work_mem='64kB'`), mssql2022 | `sortSpill`, action `SET work_mem` phạm vi session. |
| Chuyển kiểu ngầm | mssql2022 (`WHERE code = N'123'` với `varchar`), mysql8 (`code = 123`) | `implicitConversion` high/medium. |
| Thống kê cũ | pg16 (tắt autovacuum, nạp thêm 1 triệu dòng lệch) | `staleStatistics` với `ANALYZE public.orders;`. |
| ClickHouse | clickhouse26 (ORDER BY `(customer_id, created_at)`, lọc theo `status`) | `chPrimaryKeyUnused` có rewrite + skip index. |
| Metadata | pg16 | `plan_advise` trả `checked` với `path` đúng; tên bảng trùng ở 2 schema ⇒ `ambiguousTable`. |

### 11.3 Frontend
Vitest/RTL theo §8 và §9; kiểm tay trong cửa sổ Tauri ở 1024×768 và 1440×900 (light/dark) trước khi đóng gói.

---

## 12. Câu hỏi mở / khác biệt khi triển khai
- Có cần cho phép người dùng chỉnh ngưỡng advisor trong Settings không (v1: chỉ qua `PlanRules` mặc định)?
- Chọn `share` cho plan song song PG (thời gian worker chồng nhau) — v1 dùng thời gian đã chia `factor` như `analyze`.
- **Triển khai Bước 0 + R1 (lệch/chốt so với thiết kế):**
  - Hợp đồng nội bộ cho R2–R5: `advice::Cx` (đọc cây: `nodes`, `parent_of`), `advice::Draft` (+ `ShareSpec`) và mỗi họ dialect export `node_rules(cx, node, out)` / `tree_rules(cx, out)` (khung rỗng ở `mysql/mssql/oracle/mongo/clickhouse/newsql.rs`, `mod.rs` đã nối sẵn theo `PlanFormat`; TiDB gọi `mysql` rồi `newsql`, Cockroach chỉ `newsql`). Rule không tự viết DDL: gán `Draft.candidate = IndexCandidate` rồi `finalize` gọi `index::render` (trả `Create | Existing | Unsupported`). R1 đã cài `render` cho PostgreSQL/YugabyteDB (tên, quote, INCLUDE, CONCURRENTLY, đối chiếu index có sẵn ca 1–2 của §5.4); R2 mở rộng 8 dialect còn lại và giữ nguyên golden PG. Helper dùng chung ở `common.rs` (`ev`, `table_ref`, `conjuncts`, `index_columns`, `candidate`, `row_stats`, `statistics_action`, `check_action`, `stale_statistics`).
  - Golden không dùng `write_or_check` của core (nó chỉ ghi vào `contract/fixtures/<group>.json`): `tests/advice.rs` có helper riêng cùng cơ chế `UPDATE_FIXTURES=1`, ghi `tests/advice_fixtures/<fixture>.advice.json` (+ `keys.json` = rule/evidence/caveat/action key của cả kho fixture). Câu lệnh của fixture lấy từ file cạnh bên `advice_fixtures/<fixture>.sql` (RawPlan fixture không có statement). `.meta.json` (metadata giả) chưa làm — thuộc R2. Golden mới hiện chỉ cho PG; các gói R3–R5 thêm phần của mình (bất biến chạy trên toàn bộ kho fixture, kể cả MySQL/MSSQL/Oracle/Mongo/Cockroach/TiDB/ClickHouse).
  - Với dialect chưa có rule riêng, `common::warning_answers` vẫn trả advice tối thiểu cho mọi `PlanWarning` (`fullScanReview`, `sortSpill`/`hashSpill`, `staleStatistics`, `nestedLoopInnerScan`, tin cậy `low`, hành động `check`); gói dialect thay bằng rule riêng bằng cách gán `from_warnings` cùng node thì bản tối thiểu không sinh ra nữa.
  - `Draft.needs_actual`/`needs_metadata` thực hiện §3.5 (hạ một bậc + caveat `estimatedOnly`/`metadataUnavailable`); `planMayBeStale` chỉ xét khi `AdvisorContext.now` được điền (tầng 1 để trống để kết quả tất định).
  - `CREATE EXTENSION pg_trgm` **không** sinh thành action (scope `database` không được có `sql`, §6.5): thay bằng caveat `requiresExtension{extension}` (verify) trên advice `leadingWildcardLike`.
  - Parser PG: `PlanNode.predicates` một phần tử cho mỗi key của `Sort/Group/Presorted Key`; `Plan Width` và `Output` nay nằm trong `extra` (các cây trong `src/lib/mock/planTrees.json` chưa tái tạo — mock chỉ cần advice mẫu ở `planAdvice.ts`); `JIT.Timing.Total` → `root.extra["JIT Total"]`.
  - `indexForFilter` khi chỉ có ước tính và không biết cỡ bảng: dùng `est_rows ≥ filter_min_examined` làm điều kiện tối thiểu (không biết độ chọn lọc ⇒ hạ xuống `low` nhờ `needs_metadata`). Misestimate trên node có hàm bao cột hoặc CTE/Function Scan ⇒ `low` + caveat `estimateFromExpression`.
  - `pgBitmapLossy`: bộ nhớ cần ước bằng (exact+lossy blocks) × 320 B × 2, làm tròn lên luỹ thừa 2 MB (≥ 8MB, trần 1GB), kèm caveat `workMemEstimate`. `sortSpill`: 2 × `Sort Space Used`, làm tròn lên luỹ thừa 2 MB.
  - Chưa làm trong R1: ca 3 của §5.4 (chỉ khác INCLUDE — `IndexDetail.columns` không cho biết INCLUDE), cột range ghi `ASC` cho YugabyteDB, `typeMismatch` (cần metadata, thuộc R2), `implicitConversion` MySQL/MSSQL/Oracle (R3/R4).
  - Hợp đồng `plan_advise` đã cố định ở Bước 0 (stub trả advice tầng 1 + `metadata.state = disabled`); R2 thay thân hàm `advise_impl`.
  - (Câu hỏi mở cũ vẫn giữ nguyên ở hai dòng đầu mục này.)
- **Triển khai R2 (Index DDL + metadata; lệch/chốt so với thiết kế):**
  - `AdvisorContext` thêm `format: PlanFormat` + `flavor()` (`Flavor`: Postgres/Yugabyte/Cockroach/MySql/MariaDb/TiDb/MsSql/Oracle/Mongo/ClickHouse/Other) vì `ScriptDialect` không phân biệt PG/YB/Cockroach hay MySQL/TiDB; `flavor()` đi từ `dialect`, `format` chỉ để tách trong cùng họ. `context.rs` còn có hàm thuần `search_scope`/`search_kind`/`resolve_table` (§5.5) để test không cần session. `advice/mod.rs` chỉ sửa: re-export các tên trên và `IndexOutcome::Create` thêm `follow_up` (hành động sau `createIndex`).
  - ClickHouse: `createIndex` là một câu `ALTER TABLE .. ADD INDEX IF NOT EXISTS ix_{col} {col} TYPE bloom_filter|minmax GRANULARITY 4;` (minmax khi cột đầu là range) và câu `MATERIALIZE INDEX` là action thứ hai (`kind=maintenance`, `labelKey=materializeIndex`, `heavyMutation`) để giữ bất biến "mỗi createIndex đúng một câu"; frontend cần khoá i18n `advice.actions.materializeIndex`. Chỉ cột key đầu được dùng; include bị bỏ (caveat `includeDropped`). Chưa sinh projection.
  - MySQL/MariaDB/TiDB: INCLUDE chỉ nối vào key khi biết kiểu cột (metadata), tổng ≤ 5 cột và không TEXT/BLOB, ngược lại bỏ + `includeDropped`; cột key TEXT/BLOB thành `` `c`(191) `` + `prefixIndex`; MySQL/MariaDB luôn thêm caveat `onlineDdlMayFail`. Oracle/Mongo cũng bỏ include (`includeDropped`).
  - Oracle chỉ quote khi tên không khớp `^[A-Z][A-Z0-9_$#]*$` (theo §5.3) nên DDL là `CREATE INDEX IX_ORDERS_CUSTOMER_ID ON SHOP.ORDERS (CUSTOMER_ID);`, khác ví dụ có quote ở §4.4; `quote_ident` dùng chung không đổi. `ONLINE` chỉ khi `server.edition` (Oracle: `edition` hoặc `banner`) chứa Enterprise; chưa biết ⇒ bỏ `ONLINE` + `offlineIndexBuild{verify}`. SQL Server: `ONLINE = ON` theo `engineEdition`/`edition`, chưa biết ⇒ giữ + `onlineDdlMayFail{verify}`, biết là không hỗ trợ ⇒ bỏ + `offlineIndexBuild`. `WITH (DROP_EXISTING = ON, ..)` (cùng tên cũ, INCLUDE = hợp include cũ + mới đọc từ `definition`) cho ca 2 và 3 khi index cũ là NONCLUSTERED, không unique/PK; ngược lại tạo mới + `extendsIndex`.
  - Ca 3 §5.4 nhận biết INCLUDE bằng cách đọc mệnh đề `INCLUDE (..)`/`STORING (..)` trong `IndexDetail.definition` (PG, MSSQL, Cockroach); driver nào không trả definition thì coi như index đã đủ (ca 1). YugabyteDB: cột key đầu ghi `ASC` khi nó không phải cột bằng (eq_count = 0). Tên index Mongo theo server (`f_1_g_-1`, tối đa 127), ClickHouse `ix_{col}`; Oracle < 12.2 hoặc không rõ version ⇒ 30 ký tự.
  - `plan_advise`: tầng 1 chạy trước, bảng lấy theo thứ tự advice đã xếp hạng (≤ 6, phần dư tính vào `skipped`), 3 bảng song song (`JoinSet` + `Semaphore`), 3 s/bảng, 8 s tổng (ngân sách chung cắt cả bảng đang chờ). Không có bảng nào cần tra ⇒ `metadata.state = notRequested`; setting tắt ⇒ `disabled` và không gọi `search_objects`/`table_details`; mọi bảng lỗi ⇒ `unavailable`; một phần (hoặc có `skipped`) ⇒ `partial`. Bảng mơ hồ (≥ 2 kết quả, schema mặc định không phân xử được: PG/Yugabyte/Cockroach `public`, MSSQL `dbo`, Oracle `user` của `server_info`) ⇒ failed `ambiguous` + caveat `ambiguousTable{verify}`; không thấy bảng khi plan chạy trên lease ⇒ `leaseObjectsInvisible{verify}`. `TableRef.path` được điền cho advice có metadata. `in_transaction` lấy từ `lease.state().transaction == Active`. Session mất hoặc lease mất ⇒ `NotFound`. Hạn mức nằm trong `AdviseLimits` để test dùng thời gian ngắn.
