# Changelog

Các thay đổi đáng chú ý của DBStudio. File này do `pnpm release` cập nhật.

## v0.1.1 - 2026-11-15

Thay đổi từ v0.1.0.

- ci: CI chạy trên master (và PR) thay vì develop; release chạy toàn bộ CI trước, chỉ build khi test qua, tag phải nằm trên master (491e33e5)
- fix(auth): gợi ý lỗi dùng đúng nhãn "Username and password" (b8507805)
- feat(connections): chế độ xác thực authMode (password | none) cho kết nối (306da6cd)
- feat(explorer): menu chuột phải "Tạo script" (SELECT/INSERT/UPDATE/DELETE) và "Sao chép script" cho bảng, view, collection (ab3761ad)

