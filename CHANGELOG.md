# Changelog

Các thay đổi đáng chú ý của DBStudio. File này do `pnpm release` cập nhật.

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

