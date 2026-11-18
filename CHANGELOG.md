# Changelog

Các thay đổi đáng chú ý của DBStudio. File này do `pnpm release` cập nhật.

## v0.1.9 - 2026-11-18

Thay đổi từ v0.1.8. Bản v0.1.8 không được phát hành (dừng ở bước test), nên bản này gồm cả các
thay đổi của v0.1.8:

- feat(sidebar): ô tìm kiếm ở cuối cột trái như Navicat, chỉ tìm trong kết nối đang mở và tô sáng chỗ khớp
- fix(mongodb): errInfo cả khi lỗi ghi đến từ reply của lệnh raw
- feat(mongo-shell): UUID() không tham số sinh UUID v4 như mongosh
- feat(editor): chế độ Bảng của kết quả MongoDB tách trường thành cột
- feat(editor): hiện thông điệp của câu lệnh ngay dưới số dòng bị ảnh hưởng
- feat(mongodb): hiện errInfo khi server từ chối ghi (lỗi 121)
- fix(explorer): double-click collection MongoDB mở dữ liệu thay vì chỉ thu gọn nút

Thay đổi riêng của v0.1.9:

- test: ổn định hai test chập chờn trên CI (1ec7ec6)

## v0.1.8 - 2026-11-18

Thay đổi từ v0.1.7.

- feat(sidebar): ô tìm kiếm ở cuối cột trái như Navicat, chỉ tìm trong kết nối đang mở và tô sáng chỗ khớp (b7ace0e)
- fix(mongodb): errInfo cả khi lỗi ghi đến từ reply của lệnh raw (c0a3a50)
- feat(mongo-shell): UUID() không tham số sinh UUID v4 như mongosh (6436f1e)
- feat(editor): chế độ Bảng của kết quả MongoDB tách trường thành cột (13f4cc8)
- feat(editor): hiện thông điệp của câu lệnh ngay dưới số dòng bị ảnh hưởng (e1800ef)
- feat(mongodb): hiện errInfo khi server từ chối ghi (lỗi 121) (0bf18ec)
- fix(explorer): double-click collection MongoDB mở dữ liệu thay vì chỉ thu gọn nút (e1ae5f4)

## v0.1.7 - 2026-11-16

Thay đổi từ v0.1.6. Bản v0.1.6 không được phát hành (dừng ở bước test), nên bản này gồm cả các
thay đổi của v0.1.6:

- feat(editor): chọn kết nối và database đang dùng cho tab query (như DBeaver)

Thay đổi riêng của v0.1.7:

- fix(editor): DatabasePicker đọc lease hiện tại lúc chọn database (dde96d10)

## v0.1.6 - 2026-11-16

Thay đổi từ v0.1.5.

- feat(editor): chọn kết nối và database đang dùng cho tab query (như DBeaver) (3bdf5c86)

## v0.1.5 - 2026-11-16

Thay đổi từ v0.1.4.

- test(mssql): smoke test bỏ qua DDL view/routine khi login không có VIEW DEFINITION (6890b561)
- docs(mssql): hỗ trợ từ SQL Server 2008 R2 (620b86ac)
- feat(mssql): kết nối SQL Server 2008/2008 R2 — TLS backend của hệ điều hành, encrypt=none, tự fallback (a14a344c)
- test(mssql): smoke test chỉ cấm DROP … IF EXISTS, không cấm IF EXISTS (SELECT …) (33a45801)
- feat(mssql): chạy được trên SQL Server 2008 R2 (ServerVersion, paging ROW_NUMBER, DDL/IF OBJECT_ID, FOR XML PATH) (2871d708)
- feat(uri): ADO.NET connection strings for SQL Server + read-only MSSQL smoke test (50872ad4)
- feat(connections): tự đặt tên kết nối "<loại> N" khi để trống (như DBeaver) (52ff70f4)
- feat(app): icon mới — icon ứng dụng từ icon1, icon tray/taskbar từ icon2 (f086a945)

## v0.1.4 - 2026-11-15

Thay đổi từ v0.1.3. Bản v0.1.3 không được phát hành (dừng ở bước test), nên bản này gồm cả các
thay đổi của v0.1.3:

- feat(editor): sửa dữ liệu trực tiếp trên lưới kết quả query (giống DBeaver): sửa ô, thêm/xoá/nhân bản dòng, hoàn tác, xem trước SQL, lưu trong transaction của tab; chỉ cho sửa khi kết quả đến từ một bảng có khoá
- feat(connection): nhận chuỗi JDBC ở ô Connection URI (Oracle thin, PostgreSQL, MySQL/MariaDB, SQL Server, ClickHouse)

Thay đổi riêng của v0.1.4:

- test(debugger): chờ lề breakpoint vẽ xong trong mountEditor (CI chậm thì .cm-bp-gutter chưa có) (8c713ec0)

## v0.1.3 - 2026-11-15

Thay đổi từ v0.1.2.

- test(contract): fixture ColumnOrigin.alias (09684216)
- fix(editor): chạy lại đúng statement, khoá NULL/LOB bị cắt không sửa, huỷ cursor sau khi lưu, giữ edit khi huỷ đóng tab (0cb907c4)
- fix(edit): self-join/derived không sửa được (alias + chỉ giữ origin cho SELECT một bảng), ghi lại database lúc chạy, resolve schema SQL Server, kiểm số dòng KHỚP trong lease, validate chung, rollback savepoint báo lỗi, auto-commit áp dụng trên lease (1cb0d934)
- style(test): assert! thay assert_eq! với bool (clippy 1.100) (a3925fc7)
- feat(connection): nhận chuỗi JDBC ở ô Connection URI (Oracle thin, PostgreSQL, MySQL/MariaDB, SQL Server, ClickHouse) (5f31e7b0)
- test(editor): kiểm thử sửa dữ liệu trên lưới kết quả (Vitest) (7b257af6)
- feat(editor): sửa dữ liệu ngay trên lưới kết quả query (Sửa, thêm/xoá/nhân bản dòng, hoàn tác, xem trước, lưu trong transaction của tab) (eadb678b)
- refactor(edit): tách EditToolbar/EditTarget dùng chung cho màn bảng và kết quả query; thêm hoàn tác theo dòng (1c51e334)
- feat(edit): command result_editability, changes_apply nhận lease_id (áp dụng trong transaction của tab) (d8b2814b)
- feat(pg): điền nguồn gốc cột kết quả từ table_oid/column_id (một truy vấn catalog cho cả result) (da722cd1)
- feat(sql): phân tích tĩnh SELECT đơn giản để suy ra nguồn gốc cột (SQL Server, Oracle, CQL, ClickHouse) (d543746f)
- feat(core): thêm ColumnOrigin vào ColumnMeta (nguồn gốc cột kết quả) (f5d1dd0f)

## v0.1.2 - 2026-11-15

Thay đổi từ v0.1.1. Bản v0.1.1 không được phát hành (dừng ở bước test), nên bản này gồm cả các
thay đổi của v0.1.1:

- feat(explorer): menu chuột phải "Tạo script" (SELECT/INSERT/UPDATE/DELETE) và "Sao chép script" cho bảng, view, collection
- feat(connections): chế độ xác thực cho kết nối: có mật khẩu hoặc không cần xác thực
- ci: CI chạy trên master; release chạy toàn bộ test trước khi build

Thay đổi riêng của v0.1.2:

- fix(pgdbg): disarm chờ backend proxy thoát hẳn và lặp lại cancel pldbg_wait_for_target (e7c0a2a4)
- test(scylla): timeout ghi/CAS 10s cho container test (LWT của bulk import bị timeout trên runner chậm) (b90ac7b1)

## v0.1.1 - 2026-11-15

Thay đổi từ v0.1.0.

- ci: CI chạy trên master (và PR) thay vì develop; release chạy toàn bộ CI trước, chỉ build khi test qua, tag phải nằm trên master (491e33e5)
- fix(auth): gợi ý lỗi dùng đúng nhãn "Username and password" (b8507805)
- feat(connections): chế độ xác thực authMode (password | none) cho kết nối (306da6cd)
- feat(explorer): menu chuột phải "Tạo script" (SELECT/INSERT/UPDATE/DELETE) và "Sao chép script" cho bảng, view, collection (ab3761ad)

