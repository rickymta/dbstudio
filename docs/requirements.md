# DBStudio — Yêu cầu sản phẩm

> Trạng thái: v0.3 (cập nhật sau khi hiện thực GĐ2–GĐ8: thêm cột **Trạng thái** cho từng yêu cầu, mục 7 "Chưa kiểm chứng", cập nhật câu hỏi mở ở §6; nội dung yêu cầu không đổi). Tên sản phẩm "DBStudio" là tên tạm.
> Tài liệu liên quan: [architecture.md](architecture.md), [roadmap.md](roadmap.md).

## 1. Mục tiêu

DBStudio là ứng dụng desktop (Rust + Tauri 2, frontend React + TypeScript) chạy trên Windows, macOS, Linux, dùng để kết nối, đọc và thao tác dữ liệu trên nhiều hệ quản trị CSDL:

- Quan hệ: PostgreSQL, MySQL/MariaDB, Microsoft SQL Server (MSSQL), Oracle.
- NoSQL: MongoDB, Redis.
- Về sau: Cassandra, ScyllaDB, ClickHouse, NewSQL (CockroachDB, YugabyteDB qua giao thức PostgreSQL; TiDB qua giao thức MySQL).

Năng lực chính: quản lý kết nối, duyệt đối tượng, xem/sửa dữ liệu, query editor, phân tích query thành sơ đồ, ERD, xuất (và có thể nhập) schema/dữ liệu, backup/restore kèm lịch tự động.

### 1.1 Quy ước

- Mã yêu cầu: `<NHÓM>-<số>`, ví dụ `CONN-01`. Mã đã cấp không đổi nghĩa; yêu cầu bị bỏ thì đánh dấu *(huỷ)*, không tái sử dụng mã.
- Mức ưu tiên (MoSCoW):
  - **Must** — bắt buộc để coi tính năng của nhóm là hoàn thành.
  - **Should** — nên có trong giai đoạn tương ứng, có thể lùi nếu bị chặn.
  - **Could** — làm khi có thời gian hoặc ở giai đoạn sau.
- **Đề xuất** — mục do người viết tài liệu thêm ngoài yêu cầu gốc, cần chủ dự án xác nhận trước khi đưa vào phạm vi.
- "SQL DB" = PostgreSQL, MySQL, MSSQL, Oracle và các DB tương thích giao thức của chúng.
- Cột **Trạng thái** (đối chiếu với code trên nhánh `feature/quandh/phase2_complete`): **Đã làm** = có code và test tự động (unit hoặc integration với Docker); **Một phần** = thiếu phần ghi kèm; **Chưa** = không có code. "Đã làm" không có nghĩa đã kiểm tay trên app Tauri thật — xem mục 7. Với Oracle, integration test với DB thật chưa chạy được (mục 7), nên mọi "Đã làm" áp cho Oracle chỉ dựa trên unit test.
- "Lệnh phá huỷ" = `DROP`, `TRUNCATE`, `DELETE`/`UPDATE` không có `WHERE`, `ALTER ... DROP`; Redis `FLUSHDB`/`FLUSHALL`/`DEL` theo pattern; MongoDB `drop`/`deleteMany({})`.

## 2. Yêu cầu chức năng

### 2.1 Quản lý kết nối (CONN)

| Mã | Yêu cầu | Ưu tiên | Trạng thái |
|---|---|---|---|
| CONN-01 | Tạo/sửa/xoá/nhân bản profile kết nối: loại DB, host, port, database mặc định, user, phương thức xác thực, tham số riêng theo driver. | Must | Đã làm — 6 hệ CSDL; tham số riêng qua `options`. |
| CONN-02 | Sắp xếp profile theo nhóm/thư mục lồng nhau; kéo thả để di chuyển. | Should | Đã làm — nhóm lồng nhau, kéo thả profile/nhóm (`dragModel.ts`). |
| CONN-03 | Gắn nhãn môi trường (dev/staging/prod/tuỳ chỉnh) kèm màu; màu hiển thị ở cây kết nối, tab editor và status bar. | Must | Một phần — chỉ 3 nhãn cố định development/staging/production, chưa có nhãn tuỳ chỉnh; màu hiện ở cây, tab và status bar. |
| CONN-04 | Test connection: báo thành công kèm phiên bản server, hoặc lỗi rõ ràng (DNS, timeout, xác thực, TLS). | Must | Đã làm. |
| CONN-05 | SSL/TLS: các mode disable/prefer/require/verify-ca/verify-full; chọn CA cert, client cert/key. | Must | Đã làm — `verify-ca` hành xử như `verify-full` ở MySQL/MongoDB/Redis/MSSQL (architecture §18.3). |
| CONN-06 | SSH tunnel: host, port, user, xác thực bằng mật khẩu hoặc private key (có passphrase), hỗ trợ ssh-agent. | Must | Đã làm — mật khẩu, private key + passphrase, ssh-agent; kiểm host key. MySQL/MongoDB/Redis qua SSH với `verify-full` lỗi tên (R6). |
| CONN-07 | Mật khẩu, passphrase, token lưu trong OS keychain (macOS Keychain, Windows Credential Manager, Secret Service trên Linux). Không bao giờ lưu plaintext trên đĩa. | Must | Đã làm — `KeyringSecretStore`, không dự phòng plaintext; test keychain thật (`DBSTUDIO_KEYCHAIN_IT=1`) chỉ chạy tay; Windows/Linux chưa kiểm. |
| CONN-08 | Tuỳ chọn "không lưu mật khẩu" — hỏi mỗi lần kết nối. | Should | Đã làm. |
| CONN-09 | Import/export danh sách profile ra file JSON; file export **không** chứa secret. | Should | Đã làm. |
| CONN-10 | Chế độ read-only theo kết nối: chặn ở tầng ứng dụng mọi câu lệnh ghi (phân loại bằng parser) và, khi DB hỗ trợ, mở session ở chế độ read-only (vd. `SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY`). | Must | Đã làm — guard ở Rust cho mọi DB; read-only mức session chỉ có ở PG/MySQL, DB khác chặn thêm ở driver (architecture §7.1). |
| CONN-11 | Kết nối gắn nhãn prod: lệnh phá huỷ phải qua hộp thoại xác nhận bổ sung (hiển thị câu lệnh, yêu cầu gõ tên database để xác nhận). | Must | Đã làm — token một lần, gõ đúng tên database bị tác động. |
| CONN-12 | Tự kết nối lại khi mất kết nối (hỏi trước nếu đang có transaction mở); timeout kết nối/query cấu hình được. | Should | Một phần — timeout kết nối theo profile, `queryTimeoutMs` có trong setting nhưng chưa có ô trên trang Cài đặt; không tự kết nối lại (lease hỏng → nút kết nối lại, cảnh báo transaction đã mất). |
| CONN-13 | **Đề xuất:** dán connection string/URI (vd. `postgres://...`, `mongodb+srv://...`) để điền tự động form. | Could | Đã làm — `postgres(ql)`, `mysql`, `sqlserver`, `oracle`, `mongodb(+srv)`, `redis(s)`; mật khẩu trong URI bị bỏ. |

### 2.2 Trình duyệt đối tượng (BROWSE)

| Mã | Yêu cầu | Ưu tiên | Trạng thái |
|---|---|---|---|
| BROWSE-01 | Cây đối tượng tải lười (lazy) theo cấp: kết nối → database → schema → nhóm đối tượng → đối tượng → cột/index/constraint. | Must | Đã làm. |
| BROWSE-02 | SQL DB: hiển thị table, view, materialized view (nếu có), index, procedure, function, trigger, sequence, user/role. Nhóm nào DB không có thì ẩn. | Must | Đã làm (PG, MySQL, MSSQL, Oracle). |
| BROWSE-03 | MongoDB: database → collection (kèm view) → index; hiển thị số document ước tính. | Must | Đã làm. |
| BROWSE-04 | Redis: chọn database (db0..dbN), duyệt key theo pattern bằng `SCAN` (không dùng `KEYS`), nhóm theo dấu phân cách (mặc định `:`), hiển thị kiểu và TTL. | Must | Đã làm — cây phân trang theo cursor `SCAN`. |
| BROWSE-05 | Xem DDL của đối tượng (table, view, index, procedure, function, trigger, sequence), copy hoặc mở trong editor. | Must | Một phần — xem/copy DDL; chưa có "mở trong editor". |
| BROWSE-06 | Tìm/lọc đối tượng trong cây theo tên (lọc cục bộ cho phần đã tải, tìm phía server cho phần chưa tải). | Must | Đã làm — tìm phía server ở PG, MySQL, MSSQL, Oracle, MongoDB (Redis: lọc pattern của cây). |
| BROWSE-07 | Làm mới (refresh) từng nhánh; menu ngữ cảnh theo loại đối tượng (mở dữ liệu, xem DDL, sinh SELECT/INSERT mẫu, xuất, ...). | Should | Một phần — refresh, mở editor, ERD, export, backup; chưa có sinh SELECT/INSERT mẫu, mở DDL trong editor. |
| BROWSE-08 | Bảng thuộc tính đối tượng: cột (kiểu, nullable, default), khoá, index, kích thước ước tính, số dòng ước tính. | Should | Đã làm (tab Cấu trúc theo `table_details` của từng driver). |

### 2.3 Xem & sửa dữ liệu (DATA)

| Mã | Yêu cầu | Ưu tiên | Trạng thái |
|---|---|---|---|
| DATA-01 | Lưới dữ liệu với virtual scroll; tải theo trang/lô, hiển thị tổng số dòng khi có thể (đếm theo yêu cầu, không tự động với bảng lớn). | Must | Đã làm. |
| DATA-02 | Lọc (theo cột, điều kiện, hoặc nhập mệnh đề WHERE thô) và sắp xếp nhiều cột; lọc/sắp xếp thực hiện phía server. | Must | Đã làm. |
| DATA-03 | Sửa ô trực tiếp; thay đổi đưa vào danh sách chờ (pending changes), tô màu dòng/ô đã sửa. | Must | Đã làm. |
| DATA-04 | Xem trước câu lệnh SQL sẽ chạy cho danh sách chờ; commit (chạy trong một transaction) hoặc rollback (bỏ thay đổi). | Must | Đã làm. |
| DATA-05 | Thêm, xoá, nhân bản dòng (đi qua danh sách chờ). | Must | Đã làm. |
| DATA-06 | Chỉ cho sửa khi xác định được khoá duy nhất (PK/unique); nếu không có thì lưới ở chế độ chỉ đọc và giải thích lý do. | Must | Đã làm. |
| DATA-07 | Trình xem/sửa ô: JSON (format, tree), text dài, blob (hex, ảnh, tải ra file), giá trị NULL tách biệt với chuỗi rỗng. | Must | Đã làm — JSON tree, text dài, hex, ảnh, lưu ra file, NULL ≠ chuỗi rỗng. |
| DATA-08 | Copy vùng chọn theo nhiều định dạng: TSV, CSV, JSON, Markdown, SQL INSERT, danh sách `IN (...)`. | Should | Đã làm — TSV, CSV, JSON, Markdown, SQL INSERT, `IN (...)`. |
| DATA-09 | Giá trị số nguyên lớn/decimal/thời gian hiển thị đúng, không mất độ chính xác (xem kiểu `Value` trong architecture.md §4). | Must | Đã làm. |
| DATA-10 | MongoDB: xem document dạng tree, JSON (Extended JSON) và bảng (làm phẳng trường cấp 1); sửa/thêm/xoá document. | Must | Đã làm — bảng phẳng / tree / EJSON; sửa theo diff, khoá theo `_id`. |
| DATA-11 | Redis: trình xem theo kiểu key — string, hash, list, set, zset, stream; sửa giá trị, thêm/xoá phần tử, đặt/xoá TTL. | Must | Đã làm — 6 kiểu + RedisJSON (chỉ xem); stream chỉ thêm/xoá entry; Redis không rollback (`atomic=false`). |
| DATA-12 | Ẩn/hiện, đổi thứ tự, ghim cột; ghi nhớ theo bảng. | Could | Đã làm — ẩn/hiện, đổi thứ tự, ghim, độ rộng; nhớ theo kết nối + bảng (lưu ở `localStorage`). |

**Ghi chú làm rõ (bổ sung sau GĐ1, không đổi nghĩa yêu cầu):**
- *DATA-02 — WHERE thô:* phần thân mệnh đề WHERE người dùng gõ được coi là SQL tin cậy (không escape), nhưng phải là **một biểu thức duy nhất**; app từ chối mọi nội dung có thể thành câu lệnh thứ hai (`;`, ngoặc không cân, ...) và đọc dữ liệu luôn trong transaction chỉ đọc. Chi tiết theo driver: architecture.md §5.1.
- *DATA-02 — ngữ nghĩa lọc:* các phép "chứa / không chứa / bắt đầu / kết thúc" không phân biệt hoa thường và coi `%`, `_` là ký tự thường; phân biệt dấu và phép so sánh `=`/`<>`/lớn nhỏ trên chuỗi đi theo collation của DB, nên kết quả có thể khác nhau giữa PostgreSQL và MySQL. "Không chứa" loại cả dòng có giá trị NULL (theo `NOT LIKE`).
- *DATA-01 — phân trang:* để các trang không trùng/sót dòng, thứ tự luôn được bổ sung khoá chính (hoặc unique NOT NULL) ở cuối; bảng không có khoá như vậy thì không đảm bảo thứ tự giữa các trang.

### 2.4 Query editor (QUERY)

| Mã | Yêu cầu | Ưu tiên | Trạng thái |
|---|---|---|---|
| QUERY-01 | Nhiều tab editor, mỗi tab gắn một kết nối + database/schema hiện hành; khôi phục tab khi mở lại app. | Must | Đã làm — khôi phục văn bản/ngữ cảnh tab; lease mở lại khi chạy. |
| QUERY-02 | Highlight cú pháp theo dialect (PostgreSQL, MySQL, T-SQL, PL/SQL, CQL, ...). | Must | Đã làm — PostgreSQL, MySQL/MariaDB, T-SQL, PL/SQL; Mongo dùng highlight JavaScript. |
| QUERY-03 | Autocomplete theo schema: keyword, schema, bảng, cột (theo alias), function. | Must | Đã làm — từ `schema_model`, theo alias. |
| QUERY-04 | Chạy: vùng chọn, câu lệnh tại con trỏ, toàn bộ script. | Must | Đã làm. |
| QUERY-05 | Nhiều result set (mỗi câu lệnh một tab kết quả), kèm số dòng ảnh hưởng và thông điệp server (NOTICE, PRINT, warning). | Must | Đã làm — NOTICE (PG), PRINT (MSSQL, qua bản vá tiberius), DBMS_OUTPUT (Oracle). |
| QUERY-06 | Huỷ query đang chạy (dùng cơ chế cancel của giao thức, không chỉ ngắt phía client). | Must | Đã làm — cơ chế huỷ của từng giao thức (architecture §5). |
| QUERY-07 | Lịch sử query theo kết nối: thời điểm, thời gian chạy, trạng thái; tìm kiếm, mở lại. | Must | Đã làm — lưu SQLite, văn bản đã che bí mật. |
| QUERY-08 | Query lưu sẵn và snippet (có placeholder), sắp xếp theo thư mục. | Should | Đã làm — saved query, snippet có placeholder, thư mục lồng. |
| QUERY-09 | Format SQL theo dialect. | Should | Đã làm (`sql-formatter`; không áp dụng cho Mongo/Redis). |
| QUERY-10 | Tham số/biến trong query (`:name`, `$1`, `?`, `@p`): hộp thoại nhập giá trị trước khi chạy. | Should | Đã làm — bind thành literal phía Rust. |
| QUERY-11 | Chế độ transaction: auto-commit / manual commit; hiển thị rõ trạng thái transaction mở, nút commit/rollback, cảnh báo khi đóng tab có transaction mở. | Must | Đã làm (SQL DB; Mongo chỉ trên replica set; Redis không có manual). |
| QUERY-12 | Giới hạn số dòng trả về mặc định (cấu hình được), tải thêm khi cuộn. | Must | Một phần — `editorMaxRows` (mặc định 1000) chưa có ô trên trang Cài đặt; tải thêm bằng nút `fetch_more`. |
| QUERY-13 | Hiển thị thời gian chạy, số dòng; lỗi hiển thị vị trí (dòng/cột) và đánh dấu trong editor khi DB trả về. | Must | Đã làm — MSSQL chỉ trả số dòng trong batch nên vị trí là đầu dòng. |
| QUERY-14 | MongoDB: editor cho find/aggregate (cú pháp kiểu shell, JSON/EJSON), autocomplete tên collection và toán tử. | Must | Đã làm — tập con cú pháp shell (không chạy JavaScript). |
| QUERY-15 | Redis: console lệnh với lịch sử, gợi ý lệnh; xác nhận với lệnh phá huỷ (xem CONN-11). | Must | Đã làm — lịch sử trong phiên, gợi ý từ `script_vocabulary`, xác nhận `FLUSHDB`/`FLUSHALL`. |
| QUERY-16 | Mở/lưu file `.sql` từ đĩa. | Should | Đã làm (≤ 50 MB, qua dialog). |

### 2.5 Phân tích query thành sơ đồ (PLAN)

| Mã | Yêu cầu | Ưu tiên | Trạng thái |
|---|---|---|---|
| PLAN-01 | Chạy EXPLAIN (ước tính) và EXPLAIN ANALYZE (thực tế) cho câu lệnh tại con trỏ; ANALYZE với lệnh ghi phải được bọc trong transaction rollback và cảnh báo trước. | Must | Đã làm — ANALYZE bọc transaction + rollback (PG, MSSQL, Oracle); MySQL từ chối ANALYZE câu ghi; Mongo `executionStats`; production phải xác nhận. |
| PLAN-02 | Chuẩn hoá plan của từng DB về một cây plan chung (node: loại thao tác, đối tượng, cost, rows ước tính/thực tế, thời gian, loops, thuộc tính thô). | Must | Đã làm — 8 định dạng nguồn. |
| PLAN-03 | Hiển thị sơ đồ node (graph) với auto layout, zoom/pan; độ dày/màu cạnh và node theo cost hoặc thời gian; tô đậm nhánh tốn kém nhất. | Must | Đã làm — thanh tỉ lệ cost/thời gian trong node, nhánh nóng tô đậm (độ dày cạnh chỉ theo nhánh nóng). |
| PLAN-04 | Cảnh báo tự động: full table scan trên bảng lớn, sai lệch rows ước tính/thực tế vượt ngưỡng (mặc định 10×), sort/hash tràn đĩa, nested loop với nhiều dòng. | Should | Đã làm. |
| PLAN-05 | Dạng bảng (cây thụt lề) và dạng raw (JSON/XML/text gốc) cạnh sơ đồ; chọn node đồng bộ giữa các dạng. | Must | Đã làm. |
| PLAN-06 | So sánh 2 plan (trước/sau khi đổi query hoặc index): đặt cạnh nhau, chênh lệch cost/thời gian tổng. | Could | Đã làm. |
| PLAN-07 | Lưu plan vào lịch sử và mở lại từ file. | Could | Một phần — lưu/mở file `.dbplan.json`; chưa có lịch sử plan. |

Nguồn plan theo DB:

| DB | Nguồn |
|---|---|
| PostgreSQL (+ CockroachDB, YugabyteDB) | `EXPLAIN (FORMAT JSON)`, `EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON)` |
| MySQL/MariaDB (+ TiDB) | `EXPLAIN FORMAT=JSON`; `EXPLAIN ANALYZE` (MySQL 8.0.18+, dạng text cây) |
| MSSQL | `SET SHOWPLAN_XML ON` (ước tính), `SET STATISTICS XML ON` (thực tế) |
| Oracle | `EXPLAIN PLAN` + `DBMS_XPLAN.DISPLAY` / `PLAN_TABLE`; thực tế qua `V$SQL_PLAN_STATISTICS_ALL` (cần quyền) |
| MongoDB | `explain("queryPlanner" / "executionStats")` |
| ClickHouse | `EXPLAIN PLAN`, `EXPLAIN PIPELINE` |
| Cassandra/ScyllaDB | Query tracing (không có plan dạng cây; hiển thị timeline sự kiện) |
| Redis | Không áp dụng; có thể hiển thị `SLOWLOG` (Could) |

### 2.6 ERD (ERD)

| Mã | Yêu cầu | Ưu tiên | Trạng thái |
|---|---|---|---|
| ERD-01 | Reverse-engineer từ schema: bảng, cột (kiểu, nullable), PK, FK, unique. | Must | Đã làm (PG, MySQL, MSSQL, Oracle). |
| ERD-02 | Chọn bảng đưa vào sơ đồ (toàn schema, chọn tay, hoặc "bảng X + các bảng liên quan N bậc"). | Must | Đã làm. |
| ERD-03 | Auto layout; kéo thả bảng; zoom/pan; minimap. | Must | Đã làm. |
| ERD-04 | Ký hiệu crow's foot cho quan hệ; phân biệt quan hệ bắt buộc/tuỳ chọn. | Must | Đã làm. |
| ERD-05 | Lưu sơ đồ (danh sách bảng + vị trí) cục bộ, mở lại; phát hiện bảng/cột đã thay đổi so với schema hiện tại. | Should | Đã làm. |
| ERD-06 | Xuất PNG, SVG, PDF. | Must | Đã làm — chưa kiểm trên WebView của app Tauri. |
| ERD-07 | Tuỳ chọn hiển thị: chỉ khoá / tất cả cột, hiện kiểu dữ liệu, ghi chú (comment) cột. | Should | Một phần — chỉ khoá/tất cả cột, hiện kiểu; comment chỉ hiện khi rê chuột. |
| ERD-08 | MongoDB: suy luận schema bằng sampling document để vẽ sơ đồ collection. | Could | Một phần — driver suy luận bằng `$sample`; sơ đồ chưa phân biệt quan hệ suy luận. |

Ngoài phạm vi ban đầu: forward-engineering (sửa ERD → sinh DDL).

### 2.7 Xuất/nhập schema & dữ liệu (EXPORT, IMPORT)

Cách hiểu yêu cầu gốc "tạo schema - data - hoặc cả 2": xuất **chỉ schema**, **chỉ data**, hoặc **cả hai**, cho phạm vi chọn được, ra các định dạng SQL/XLSX/CSV/JSON.

| Mã | Yêu cầu | Ưu tiên | Trạng thái |
|---|---|---|---|
| EXPORT-01 | Chọn nội dung: chỉ schema / chỉ data / cả hai. | Must | Đã làm. |
| EXPORT-02 | Chọn phạm vi: toàn database, một schema, các bảng được chọn, hoặc kết quả một query. | Must | Đã làm cho SQL DB và Mongo (database, schema, bảng chọn, query). |
| EXPORT-03 | SQL: DDL (theo dialect nguồn, thứ tự tạo đúng phụ thuộc FK) + `INSERT` (gộp nhiều dòng, kích thước lô cấu hình được). | Must | Đã làm — DDL PreData/PostData (FK ở cuối). |
| EXPORT-04 | XLSX: mỗi bảng một sheet; khi xuất schema, thêm sheet data dictionary (bảng, cột, kiểu, nullable, default, khoá, comment). Cảnh báo khi vượt giới hạn 1.048.576 dòng/sheet (tự tách sheet). | Must | Đã làm. |
| EXPORT-05 | CSV: mỗi bảng một file, gom thành `.zip` khi nhiều bảng; tuỳ chọn delimiter, quote, encoding (UTF-8 có/không BOM), header. | Must | Đã làm. |
| EXPORT-06 | JSON: dạng array hoặc NDJSON; khi xuất schema, xuất file JSON mô tả cấu trúc (bảng, cột, kiểu, khoá, index). | Must | Đã làm. |
| EXPORT-07 | Chạy nền, stream dữ liệu (không nạp toàn bộ vào RAM), hiển thị tiến độ, huỷ được. | Must | Đã làm — một snapshot qua lease, file tạm + đổi tên. |
| EXPORT-08 | Ánh xạ kiểu khi xuất: NULL, binary (base64/hex), ngày giờ (ISO 8601), số lớn (giữ nguyên dạng chuỗi số). | Must | Đã làm. |
| EXPORT-09 | MongoDB: xuất collection ra JSON/NDJSON (Extended JSON), CSV/XLSX (làm phẳng có chọn trường). Redis: xuất key theo pattern ra JSON. | Should | Một phần — Mongo JSON/NDJSON EJSON; chưa làm phẳng document cho CSV/XLSX, chưa xuất khoá Redis. |
| EXPORT-10 | Lưu cấu hình xuất thành preset để chạy lại. | Could | Đã làm. |
| IMPORT-01 | **Đề xuất (cần xác nhận):** nhập CSV/XLSX/JSON vào bảng có sẵn hoặc bảng mới, có mapping cột, xem trước, chọn kiểu dữ liệu. | Should | Đã làm cho PG, MySQL, MSSQL, Oracle; chưa nhận đích Mongo/Redis. |
| IMPORT-02 | **Đề xuất (cần xác nhận):** chạy file SQL lớn (script) theo stream, báo lỗi theo câu lệnh, tuỳ chọn dừng/tiếp tục khi lỗi. | Should | Đã làm. |
| IMPORT-03 | **Đề xuất (cần xác nhận):** chế độ khi trùng khoá: bỏ qua / ghi đè / dừng; nhập trong transaction hoặc theo lô. | Could | Đã làm — trừ `Skip` trên Oracle. |

### 2.8 Backup – Restore (BACKUP)

| Mã | Yêu cầu | Ưu tiên | Trạng thái |
|---|---|---|---|
| BACKUP-01 | Backup thủ công: toàn database hoặc các đối tượng được chọn. | Must | Đã làm — toàn database hoặc đối tượng chọn; IT với PG/MySQL. |
| BACKUP-02 | Engine backup logic tích hợp sẵn (không phụ thuộc tool ngoài): schema + data ra định dạng của DBStudio (SQL theo dialect, kèm manifest). | Must | Một phần — `.dbsbak` (SQL theo dialect từ `generate_ddl` + export engine, preamble + manifest) cho SQL DB; IT với PG 12/16, MySQL/MariaDB; MSSQL/Oracle chưa test; MongoDB/Redis chưa có engine logic. |
| BACKUP-03 | Tuỳ chọn dùng tool native khi có trên máy: `pg_dump`/`pg_restore`, `mysqldump`, `mongodump`/`mongorestore`, MSSQL `BACKUP DATABASE`/`RESTORE` (file nằm phía server), Oracle `expdp`/`impdp`, Redis RDB (`BGSAVE`/tải RDB). Tự dò đường dẫn, cho cấu hình tay. | Should | Một phần — `pg_dump`→`pg_restore`/`psql`, `mysqldump`→`mysql`, `mongodump`→`mongorestore`, `redis-cli --rdb` (chỉ backup), MSSQL `BACKUP DATABASE` vào thư mục trên server; chưa có `expdp`/`impdp`; tự dò + cấu hình tay. IT chỉ với PG/MySQL qua shim. |
| BACKUP-04 | Nén gzip hoặc zstd. | Must | Đã làm. |
| BACKUP-05 | Mã hoá tuỳ chọn (AES-256-GCM, khoá dẫn xuất từ passphrase lưu trong keychain). | Should | Đã làm — AES-256-GCM theo khối, Argon2id; passphrase lưu keychain theo job. |
| BACKUP-06 | Restore: chọn file, xem trước nội dung (manifest: DB nguồn, thời điểm, đối tượng), chọn đích, xác nhận (xác nhận bổ sung nếu đích là prod). | Must | Đã làm — xem preamble, chọn đích, xác nhận gõ tên DB với production/`clean`; không restore một phần backup logical. |
| BACKUP-07 | Lịch backup tự động dạng cron (có trình tạo lịch trực quan: hằng ngày/tuần/tháng + biểu thức cron nâng cao). | Must | Đã làm — trình tạo lịch + cron 5 trường, xem trước các lần chạy. |
| BACKUP-08 | Chính sách giữ lại: giữ N bản gần nhất và/hoặc xoá bản cũ hơn X ngày. | Must | Đã làm — luôn giữ bản mới nhất; chỉ xoá file do job tạo. |
| BACKUP-09 | Đích lưu: thư mục local (gồm ổ mạng đã mount). S3/SFTP về sau. | Must (local) / Could (S3, SFTP) | Đã làm (local); S3/SFTP chưa. |
| BACKUP-10 | Thông báo thành công/thất bại qua thông báo hệ điều hành. | Must | Đã làm — chưa kiểm tay trên OS thật; nội dung chỉ tiếng Anh. |
| BACKUP-11 | Nhật ký lịch sử backup/restore: thời điểm, thời lượng, kích thước, trạng thái, lỗi. | Must | Đã làm (`backup_runs`, lịch sử chạy trong tab Backup). |
| BACKUP-12 | Lần chạy bị lỡ (máy tắt/app đóng): khi app mở lại, chạy bù một lần hoặc bỏ qua theo cấu hình từng lịch. | Must | Đã làm — chạy bù đúng một lần hoặc bỏ qua; kiểm bằng đồng hồ giả. |
| BACKUP-13 | App chạy nền qua tray icon (đóng cửa sổ không thoát app khi có lịch); tuỳ chọn autostart cùng hệ điều hành. | Must | Đã làm — tray, close-to-tray, autostart (`--background`), single-instance; chưa kiểm tay. |
| BACKUP-14 | Kiểm tra tính toàn vẹn file backup (checksum trong manifest). | Should | Đã làm (`backup_verify`, `dbstudio-cli verify`). |
| BACKUP-15 | CLI headless (`dbstudio-cli backup --job <id>`) để gắn vào scheduler của OS (cron, launchd, Task Scheduler). | Could | Đã làm — `backup`/`restore`/`verify`/`tools`, khoá file chung với app. |

### 2.9 Chung (APP)

| Mã | Yêu cầu | Ưu tiên | Trạng thái |
|---|---|---|---|
| APP-01 | Giao diện đa ngôn ngữ tiếng Việt và tiếng Anh, chuyển không cần khởi động lại. | Must | Đã làm. |
| APP-02 | Theme sáng/tối/theo hệ thống. | Must | Đã làm. |
| APP-03 | Khung giao diện: sidebar (kết nối + cây đối tượng), vùng tab, status bar (kết nối hiện hành, môi trường, trạng thái transaction, tác vụ nền). | Must | Đã làm. |
| APP-04 | Phím tắt cho thao tác chính (chạy query, tab mới, đóng tab, format, tìm đối tượng), có bảng tra phím tắt; tuỳ biến phím tắt là Could. | Must | Đã làm — bảng tra phím tắt; chưa tuỳ biến (Could). |
| APP-05 | Trang cài đặt: ngôn ngữ, theme, font editor, giới hạn dòng mặc định, timeout, hành vi đóng cửa sổ. | Must | Một phần — có ngôn ngữ, theme, trần lưới, hiển thị thời gian, mức log; chưa có font editor, giới hạn dòng editor, timeout query. Hành vi đóng cửa sổ và autostart nằm ở tab Backup, chưa ở trang Cài đặt. |
| APP-06 | Auto-update (kiểm tra, tải, cài bản mới, có chữ ký). | Should | Chưa (GĐ9). |
| APP-07 | Log ứng dụng ra file xoay vòng, mức log cấu hình được; không ghi secret; nút "mở thư mục log". | Must | Đã làm. |
| APP-08 | Command palette (tìm lệnh/đối tượng nhanh). | Could | Chưa. |
| APP-09 | Trung tâm tác vụ nền (export, backup, restore): tiến độ, huỷ, xem lỗi. | Should | Đã làm (export, import, xoá khoá theo pattern, backup, restore, verify). |

## 3. Yêu cầu phi chức năng (NFR)

| Mã | Yêu cầu | Trạng thái |
|---|---|---|
| NFR-PLAT-01 | Chạy trên Windows 10+ (x64; arm64 là Could), macOS 12+ (Intel + Apple Silicon), Linux x64 (Ubuntu 22.04+/Fedora 38+ hoặc tương đương; gói deb, rpm, AppImage). | Chưa kiểm — mới build/chạy trên macOS. |
| NFR-PLAT-02 | Hành vi nhất quán giữa các webview (WebView2, WKWebView, WebKitGTK); kiểm thử giao diện trên cả 3. | Chưa kiểm — chưa thử trên WebView2/WebKitGTK. |
| NFR-PERF-01 | Khởi động đến cửa sổ dùng được < 2 giây trên máy tầm trung (SSD, 8 GB RAM), không tính thời gian kết nối DB. | Chưa đo. |
| NFR-PERF-02 | Kết quả được stream theo lô (mặc định 500–1000 dòng/lô); dòng đầu tiên hiển thị ngay khi lô đầu về. | Đã làm. |
| NFR-PERF-03 | Lưới cuộn mượt (mục tiêu ~60 fps) với 1 triệu dòng đã tải; bộ nhớ có giới hạn (cấu hình được) — vượt ngưỡng thì dừng tải và báo. | Một phần — trần dòng/ô cấu hình được trong Cài đặt; chưa benchmark 1 triệu dòng trên webview thật. |
| NFR-PERF-04 | Không thao tác DB nào chạy trên luồng UI; mọi query huỷ được; UI phản hồi trong khi query chạy. | Đã làm — mọi thao tác DB async/huỷ được; chưa đo trên app thật. |
| NFR-SEC-01 | Secret chỉ nằm trong OS keychain và bộ nhớ tiến trình Rust; không bao giờ gửi sang frontend, không ghi log, không có trong file export profile. | Đã làm — có test quét mật khẩu trong SQLite/WAL/log/file export (`leak_test.rs`). |
| NFR-SEC-02 | Hỗ trợ TLS (rustls) và SSH tunnel cho mọi driver có thể. | Đã làm — TLS mọi driver (Oracle qua TCPS/wallet), SSH tunnel mọi driver; giới hạn R6. |
| NFR-SEC-03 | Read-only mode và xác nhận lệnh phá huỷ (CONN-10, CONN-11) được thực thi ở phía Rust, không chỉ ở UI. | Đã làm. |
| NFR-SEC-04 | Tauri capabilities tối thiểu (chỉ cấp plugin/quyền thật sự dùng); CSP chặt, không tải script từ xa. | Đã làm. |
| NFR-SEC-05 | Không gửi telemetry mặc định. Nếu có về sau: opt-in, mô tả rõ dữ liệu gửi đi. | Đã làm (không có telemetry). |
| NFR-SEC-06 | Dữ liệu cục bộ (SQLite) không chứa secret; file backup mã hoá được (BACKUP-05). | Đã làm — SQLite không chứa secret; file backup mã hoá được (BACKUP-05). |
| NFR-EXT-01 | Thêm một hệ quản trị = thêm một crate driver hiện thực trait chung (`Driver`, `Session`) + khai báo `Capabilities`; không sửa code UI cho từng DB ngoài các view chuyên biệt (document, key-value). | Đã làm — 6 driver, mỗi driver một crate. |
| NFR-EXT-02 | UI tự bật/tắt tính năng theo `Capabilities` của driver (vd. ẩn ERD với Redis). | Đã làm. |
| NFR-REL-01 | **Đề xuất:** sửa dữ liệu không bao giờ tự commit ngầm; mọi lỗi giữa chừng của commit lô phải rollback toàn bộ. | Đã làm (SQL DB); Mongo standalone và Redis không nguyên tử, UI báo `atomic=false`. |
| NFR-REL-02 | **Đề xuất:** dữ liệu cục bộ (profile, lịch sử, lịch backup) có migration schema có phiên bản; nâng cấp app không làm mất dữ liệu. | Đã làm (migration v1–v6). |
| NFR-QUAL-01 | Unit test cho core, parser plan, export; integration test với DB thật chạy bằng Docker (testcontainers hoặc docker compose). | Đã làm. |
| NFR-QUAL-02 | CI chạy build + lint (`cargo clippy -D warnings`, `cargo fmt --check`, ESLint, `tsc --noEmit`) + test trên Windows, macOS, Linux. Integration test DB chạy trên Linux (Docker). | Một phần — workflow có đủ bước (tsc chạy trong `pnpm build`) nhưng chưa chạy trên GitHub. |
| NFR-QUAL-03 | Chuỗi giao diện đi qua i18n, không hard-code; kiểm tra thiếu khoá dịch trong CI. | Đã làm. |

### 3.1 Phiên bản DB mục tiêu (đề xuất)

| DB | Phiên bản tối thiểu |
|---|---|
| PostgreSQL | 12+ |
| MySQL / MariaDB | MySQL 5.7+, MariaDB 10.4+ |
| SQL Server | 2016+ (kể cả Azure SQL) |
| Oracle | 12c+ |
| MongoDB | 4.4+ |
| Redis / Valkey | Redis 6+, Valkey 7.2+ |
| Cassandra / ScyllaDB | Cassandra 4+, ScyllaDB 5+ |
| ClickHouse | 23+ |
| NewSQL | CockroachDB, YugabyteDB (giao thức PostgreSQL), TiDB (giao thức MySQL) — bản stable hiện hành |

## 4. Ma trận tính năng × hệ quản trị

Ký hiệu: ✓ áp dụng · ◐ áp dụng một phần / dạng riêng · — không áp dụng.

| Tính năng | PostgreSQL | MySQL | MSSQL | Oracle | MongoDB | Redis | Cassandra/Scylla | ClickHouse | NewSQL |
|---|---|---|---|---|---|---|---|---|---|
| Kết nối, TLS, SSH (CONN) | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| Cây đối tượng (BROWSE) | ✓ | ✓ | ✓ | ✓ | ◐ db/collection/index | ◐ db/key | ◐ keyspace/table | ✓ | ✓ |
| Xem DDL (BROWSE-05) | ✓ | ✓ | ✓ | ✓ | ◐ index/validator | — | ✓ | ✓ | ✓ |
| Lưới dữ liệu (DATA) | ✓ | ✓ | ✓ | ✓ | ◐ document | ◐ theo kiểu key | ✓ | ✓ | ✓ |
| Sửa dữ liệu (DATA-03..06) | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ◐ theo primary key | ◐ hạn chế | ✓ |
| Query editor (QUERY) | ✓ SQL | ✓ SQL | ✓ T-SQL | ✓ PL/SQL | ◐ MQL | ◐ console | ✓ CQL | ✓ SQL | ✓ SQL |
| Transaction thủ công (QUERY-11) | ✓ | ✓ | ✓ | ✓ | ◐ replica set | ◐ MULTI/EXEC | — | — | ✓ |
| Plan diagram (PLAN) | ✓ | ✓ | ✓ | ✓ | ✓ | — (SLOWLOG) | ◐ tracing | ✓ | ✓ |
| ERD (ERD) | ✓ | ✓ | ✓ | ✓ | ◐ sampling (Could) | — | ◐ không có FK | ◐ không có FK | ✓ |
| Export schema | ✓ | ✓ | ✓ | ✓ | ◐ suy luận | — | ✓ | ✓ | ✓ |
| Export data | ✓ | ✓ | ✓ | ✓ | ✓ | ◐ JSON | ✓ | ✓ | ✓ |
| Backup tích hợp | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ◐ | ◐ | ✓ |
| Backup native | pg_dump | mysqldump | BACKUP DATABASE | expdp | mongodump | RDB | nodetool snapshot | BACKUP | theo giao thức gốc* |

\* CockroachDB có `BACKUP` riêng; TiDB có BR/Dumpling; YugabyteDB có `ysql_dump`. Chỉ hỗ trợ khi tới GĐ10+.

## 5. Ngoài phạm vi (bản đầu)

- Forward-engineering ERD (sinh DDL từ sơ đồ), so sánh/đồng bộ schema giữa 2 DB.
- Trình thiết kế bảng dạng form (tạo/sửa bảng qua UI) — có thể xem xét sau GĐ6.
- Quản trị server (quản lý user/quyền qua UI, monitoring, replication).
- Làm việc nhóm, đồng bộ profile qua cloud, AI hỗ trợ viết query.
- Ứng dụng web/mobile.

## 6. Rủi ro & câu hỏi mở

Câu hỏi cần chủ dự án trả lời (gắn với giai đoạn bị ảnh hưởng). GĐ2–GĐ8 đã làm theo mặc định đề xuất ở [design/phase2-8.md](design/phase2-8.md) §8.1 khi chưa có trả lời; các mặc định đó ghi trong ngoặc *(GĐ2–8: ...)* — chủ dự án vẫn cần xác nhận.

1. **Import có nằm trong phạm vi không?** (IMPORT-01..03, GĐ5) — yêu cầu gốc chỉ nói "tạo", nhưng công cụ DB thường cần cả nhập. *(GĐ2–8: đã hiện thực import CSV/XLSX/JSON/SQL cho SQL DB.)*
2. **Backup: ưu tiên engine tích hợp hay tool native?** Engine tích hợp chạy mọi nơi nhưng chậm hơn và không đảm bảo tương đương 100% với `pg_dump`/`expdp` (đối tượng đặc thù, quyền, extension). Đích cloud (S3/SFTP) có cần ở bản đầu không? (GĐ8) *(GĐ2–8: làm cả hai; chỉ đích local.)*
3. **Backup tự động chỉ chạy khi app đang mở (tray) có đủ không**, hay cần service/daemon độc lập chạy cả khi user chưa đăng nhập? (GĐ8, liên quan BACKUP-15) *(GĐ2–8: tray + autostart + CLI cho scheduler của OS, không làm service.)*
4. **Phân phối nội bộ hay công khai?** Quyết định chi phí ký số (Apple Developer ID + notarize, chứng thư code signing Windows), kênh auto-update, và việc có cần cửa hàng ứng dụng không. (GĐ9)
5. **Giấy phép sản phẩm** (mã nguồn mở hay thương mại)? Ảnh hưởng việc chọn dependency (vd. giấy phép của driver, Oracle Instant Client) và khả năng phân phối kèm. *(bổ sung)*
6. **Xác thực MSSQL**: có cần Windows Integrated Authentication (Kerberos/NTLM) và Azure AD không? `tiberius` hỗ trợ hạn chế trên macOS/Linux. *(bổ sung, GĐ3)* *(GĐ2–8: SQL login + NTLM trên mọi OS qua bản vá tiberius, Integrated chỉ Windows, chưa có Azure AD; NTLM chưa thử với domain thật.)*
7. Tên sản phẩm và bundle identifier hiện tạm là "DBStudio" — cần chốt trước GĐ9 (đổi identifier sau khi phát hành sẽ làm mất dữ liệu cục bộ/keychain entry và đứt chuỗi auto-update).
8. **Chế độ TLS mặc định cho profile mới** *(bổ sung sau GĐ1, CONN-05)*. Hiện là `prefer` (giống libpq và đa số công cụ). Review bảo mật chỉ ra `prefer` và `require` không chống được tấn công chặn giữa: `prefer` có thể rơi về kết nối không mã hoá, `require` không kiểm chứng thư — mật khẩu có thể lộ. Giao diện đã cảnh báo khi chọn `disable`/`prefer`/`require` với host không phải loopback. Có nên đổi mặc định sang `verify-full` (hoặc `verify-full` cho host từ xa và `require`/`prefer` cho localhost) không? Đánh đổi: an toàn hơn nhưng nhiều server nội bộ dùng chứng thư tự ký sẽ kết nối thất bại cho tới khi người dùng chỉ định CA.

Rủi ro kỹ thuật:

| Rủi ro | Ảnh hưởng | Hướng xử lý |
|---|---|---|
| Crate `oracle` cần Oracle Instant Client (ODPI-C) | Đóng gói phức tạp, giấy phép phân phối lại, người dùng phải cài thêm | Cập nhật GĐ3: không đóng gói; nạp động lúc chạy (setting `oracle_client` hoặc `DBSTUDIO_ORACLE_CLIENT_LIB_DIR`), thiếu → driver `unavailable` + chẩn đoán. `oracle-rs` thuần Rust chỉ theo dõi, đánh giá lại ở GĐ9 |
| Hiệu năng WebKitGTK (Linux) với lưới lớn và sơ đồ lớn | Giật/lag trên Linux | Benchmark sớm; dự phòng canvas grid *(bổ sung)*. Cập nhật sau GĐ1: benchmark **chưa làm** (chưa có bản build Linux), quyết định grid còn mở |
| Khác biệt dialect lớn (DDL, kiểu dữ liệu, plan) | Tốn công cho mỗi DB, dễ sai ở export/backup | Test integration theo từng DB + bộ dữ liệu mẫu nhiều kiểu |
| Backup logic không đầy đủ so với tool native | Restore thiếu đối tượng | Manifest liệt kê rõ phạm vi hỗ trợ; khuyến nghị tool native cho production |
| Keychain trên Linux phụ thuộc Secret Service đang chạy | Không lưu được mật khẩu trên một số môi trường | Phát hiện và báo rõ; fallback "hỏi mỗi lần" (CONN-08), không fallback plaintext |
| Keychain thật chưa được kiểm thử đủ *(bổ sung sau GĐ1)* | Hành vi thật (quyền truy cập Keychain macOS, Credential Manager, Secret Service, lỗi khi bị từ chối) chưa biết hết | Có test keychain thật chạy tay (`DBSTUDIO_KEYCHAIN_IT=1`) và test quét mật khẩu (`leak_test.rs`); còn Windows/Linux khi có bản build |
| Hạn chế TLS của `mysql_async` 0.37 *(bổ sung sau GĐ1)* | `verify-ca` hoạt động như `verify-full` (không cho thay verifier); không dùng kho chứng thư của OS khi thiếu file CA (dùng `webpki-roots`); không đăng nhập được tài khoản `sha256_password` | Ghi rõ trong tài liệu và thông báo lỗi; theo dõi bản `mysql_async` mới hoặc đóng góp upstream; cân nhắc thay lớp TLS nếu người dùng cần CA nội bộ từ kho OS |
| Chưa kiểm thử trên Windows và Linux *(bổ sung sau GĐ1)* | Lỗi build, khác biệt webview (WebView2, WebKitGTK), keychain, đường dẫn file chỉ lộ ra muộn | Cho CI matrix 3 OS chạy ngay khi repo có remote; benchmark lưới và kiểm thử giao diện trên cả 3 webview trước khi chốt grid (chậm nhất GĐ9) |
| Phân loại câu lệnh bằng heuristic *(bổ sung sau GĐ2–8)* | Câu lệnh lạ có thể bị phân loại sai (SQL động, `ANSI_QUOTES` của MySQL) | Bảo thủ: `Unknown`/`Procedural` coi là ghi, production quét thêm từ khoá; read-only còn lớp driver (architecture §7.1, §18.3) |

## 7. Chưa kiểm chứng (sau GĐ2–GĐ8)

Những phần dưới đây có code (và thường có test tự động) nhưng **chưa được kiểm chứng trong môi trường thật**:

- **GUI thật trong cửa sổ Tauri**: giao diện GĐ2–GĐ8 mới được thao tác trong trình duyệt với mock IPC hoặc dev bridge. Chưa kiểm: tray icon, close-to-tray, autostart, thông báo OS (BACKUP-10/13), xuất PNG/PDF của ERD và plan trên WebView (`html-to-image`/`jspdf`, rủi ro R7 của thiết kế).
- **Windows và Linux**: chưa build/chạy lần nào; chưa kiểm WebView2, WebKitGTK, Credential Manager, Secret Service, ssh-agent qua named pipe.
- **SQL Server**: NTLM với domain thật, Integrated (Windows), Azure SQL.
- **Oracle**: integration test với DB thật chưa chạy (container `oraclefree` cần ~3 GB RAM cho Docker, đang chờ); hiện chỉ có unit test và test vi sai splitter.
- **CI GitHub**: workflow (matrix 3 OS + job `integration`) chưa chạy lần nào (repo chưa có remote).
- **Backup/restore**: mới có IT cho PG và MySQL/MariaDB (tool native qua shim, qua SSH tunnel); MSSQL, Oracle, MongoDB, Redis chưa test. Tray, close-to-tray, autostart, single-instance và thông báo OS chưa kiểm tay.
- **Hiệu năng**: chưa benchmark lưới 1 triệu dòng trên 3 webview, chưa đo thời gian khởi động.
