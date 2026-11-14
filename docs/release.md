# Phát hành DBStudio

Bản phát hành được CI (`.github/workflows/release.yml`) tự build và đăng lên GitHub Releases khi có tag
`vX.Y.Z` (hoặc `vX.Y.Z-beta.1`) được push. Không có bước nào tự chạy khi merge vào `develop`.

## Quy trình

1. Trên `develop` (đã cập nhật, working tree sạch), chạy:

   ```bash
   pnpm release patch          # hoặc minor | major | 1.2.3 | 1.3.0-beta.1
   pnpm release minor --dry-run  # chỉ xem thay đổi, không ghi gì
   ```

   Script `scripts/release.mjs` sẽ: sửa version ở `src-tauri/tauri.conf.json`, `package.json`,
   `Cargo.toml` (`[workspace.package]`); chạy `cargo update -w --offline` cho `Cargo.lock` (cần `cargo`
   trên PATH, nếu không sẽ cảnh báo); thêm mục mới vào `CHANGELOG.md` từ các commit kể từ tag trước;
   tạo commit `chore(release): vX.Y.Z` và tag annotated `vX.Y.Z`. Script từ chối chạy khi working tree
   có thay đổi chưa commit, hoặc khi không ở nhánh `develop`/`master`/`main` (dùng
   `--allow-branch <tên>` nếu cố ý). Script **không push**.

2. Xem lại commit và `CHANGELOG.md` (sửa tay được, rồi `git commit --amend` và tạo lại tag nếu cần),
   sau đó push:

   ```bash
   git push origin develop
   git push origin vX.Y.Z
   ```

3. Workflow `Release` chạy ba job:
   - `prepare`: kiểm version trong tag khớp cả 3 file (lệch thì fail, ghi rõ file nào), tạo GitHub Release
     ở trạng thái **draft**. Release notes lấy từ mục `## vX.Y.Z` của `CHANGELOG.md`, không có thì
     GitHub tự sinh. Tag có dấu `-` được đánh dấu prerelease.
   - `build` (song song): macOS Apple Silicon (`.dmg`), macOS Intel (`.dmg`), Windows x64 (`.msi`,
     `-setup.exe`), Linux x64 (`.deb`, `.rpm`, `.AppImage`, build trên Ubuntu 22.04 để glibc tương thích
     rộng). Tên file dạng `DBStudio_<version>_<nền-tảng>.<đuôi>`. Hai bản macOS được tách thay vì
     universal vì universal biên dịch mọi crate hai lần trong một job.
   - `publish`: chỉ chạy khi **mọi** build thành công; tạo `SHA256SUMS.txt` cho mọi file rồi chuyển release
     từ draft sang published. Một build lỗi thì release vẫn là draft, không ai thấy bản thiếu file.

4. Kiểm tra release trên trang GitHub. CI cũng có job `version-sync` kiểm 3 file version luôn khớp nhau
   (`pnpm check:version`).

Oracle cần Oracle Instant Client cài sẵn trên máy người dùng lúc chạy (không cần lúc build).

## Secret ký số (tuỳ chọn)

Vào Settings > Secrets and variables > Actions của repo. Không có secret thì build vẫn thành công nhưng
không ký.

| Nền tảng | Secret                                                                                                                  |
| -------- | ----------------------------------------------------------------------------------------------------------------------- |
| macOS    | `APPLE_CERTIFICATE` (file .p12 mã hoá base64), `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY` (vd. `Developer ID Application: Tên (TEAMID)`) |
| macOS (notarize) | `APPLE_ID`, `APPLE_PASSWORD` (app-specific password), `APPLE_TEAM_ID`                                           |
| Windows  | `WINDOWS_CERTIFICATE` (file .pfx mã hoá base64), `WINDOWS_CERTIFICATE_PASSWORD`                                         |
| Updater (chưa bật) | `TAURI_SIGNING_PRIVATE_KEY`, `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`                                             |

- macOS ký khi có đủ nhóm chứng chỉ (3 secret đầu), notarize khi có thêm nhóm Apple ID; thiếu một phần của
  nhóm thì cả nhóm đó bị bỏ qua.
- Mã hoá base64: `base64 -i cert.p12 | pbcopy` (macOS) hoặc `base64 -w0 cert.pfx` (Linux).
- Windows: workflow import pfx vào kho chứng chỉ của runner rồi truyền thumbprint cho Tauri
  (`bundle.windows.certificateThumbprint`, SHA-256, timestamp DigiCert).
- Updater chỉ mới chuẩn bị chỗ truyền khoá; để bật cần thêm plugin updater và `createUpdaterArtifacts`.

## Bản không ký

- **macOS Gatekeeper** báo "đã hỏng" hoặc "không thể xác minh nhà phát triển". Kéo app vào
  `/Applications` rồi chạy:

  ```bash
  xattr -dr com.apple.quarantine /Applications/DBStudio.app
  ```

  Hoặc chuột phải vào app > Open lần đầu (Sequoia trở lên: System Settings > Privacy & Security >
  Open Anyway).

- **Windows SmartScreen** hiện "Windows protected your PC": chọn More info > Run anyway. Độ tin cậy với
  SmartScreen tích luỹ dần theo số lượt tải khi đã có chứng chỉ ký.

- Có thể đối chiếu file tải về với `SHA256SUMS.txt` (`shasum -a 256 -c SHA256SUMS.txt`).

## Phát hành lại một tag

- Build lỗi do hạ tầng (mạng, runner): vào Actions > Release > chạy lại các job lỗi, hoặc **Run workflow**
  và nhập tag đã có (vd. `v0.1.0`). Release được đưa về draft, asset ghi đè (`--clobber`), rồi publish lại.
  Workflow chạy trên chính commit mà tag trỏ tới.
- Cần sửa code: không dời tag đã công khai. Phát hành bản vá mới (`pnpm release patch`). Chỉ khi release
  chưa ai dùng mới xoá release + tag (`git push origin :refs/tags/vX.Y.Z`), sửa, tạo lại tag rồi push.
- Version trong tag lệch file: sửa version (hoặc xoá tag, chạy lại `pnpm release`), không phải lỗi workflow.
