# DBStudio

Cross-platform desktop tool for administering many database systems (PostgreSQL, MySQL, SQL Server,
Oracle, MongoDB, Redis, Cassandra/ScyllaDB, ClickHouse). Built with Rust + Tauri 2 and a React +
TypeScript frontend.

**Status: phase 0 (foundation).** No real database connectivity yet. The workspace builds, the
driver abstraction (`dbstudio-core`) is in place and the UI shell runs. Drivers arrive in later
phases and plug in through `dbstudio_core::Driver`.

## Layout

| Path                   | Purpose                                                                           |
| ---------------------- | --------------------------------------------------------------------------------- |
| `crates/dbstudio-core` | Domain types and `Driver` / `Session` traits. No Tauri dependency.                |
| `src-tauri`            | Thin Tauri app crate: state, IPC commands, window config.                         |
| `src`                  | React app (`app/`, `features/`, `components/ui/`, `lib/`, `stores/`, `locales/`). |

## Requirements

- Rust stable (>= 1.85) and the [Tauri 2 prerequisites](https://tauri.app/start/prerequisites/)
  (Linux: `webkit2gtk-4.1`, `libgtk-3-dev`, ...).
- Node.js 22+ and pnpm 9.
- Linux system packages (Debian/Ubuntu, the same list CI installs):

  ```sh
  sudo apt-get install -y libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev \
    librsvg2-dev libxdo-dev libssl-dev build-essential curl wget file
  ```

## Commands

```sh
pnpm install
pnpm tauri dev          # desktop app with hot reload
pnpm dev                # frontend only in a browser (IPC falls back to mock data)
pnpm tauri build        # production bundle

pnpm lint && pnpm test && pnpm build
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

`cargo build -p dbstudio` needs `dist/` to exist, so run `pnpm build` first.

## Integration tests

The driver and command-layer tests that talk to real servers only run when `DBSTUDIO_IT=1`
(without it they pass immediately; with it, an unreachable server fails the test). They need
Docker:

```sh
sh docker/gen-certs.sh
docker compose -f docker/compose.test.yml up -d --wait
DBSTUDIO_IT=1 cargo test --workspace
docker compose -f docker/compose.test.yml down -v
```

| Service   | Image         | Address           | TLS                            |
| --------- | ------------- | ----------------- | ------------------------------ |
| `pg16`    | PostgreSQL 16 | `127.0.0.1:54316` | yes (CA `docker/certs/ca.crt`) |
| `pg12`    | PostgreSQL 12 | `127.0.0.1:54312` | no                             |
| `mysql8`  | MySQL 8       | `127.0.0.1:33068` | yes (same CA)                  |
| `mariadb` | MariaDB 10.6  | `127.0.0.1:33106` | no                             |

Login on every server: user `dbstudio`, password `dbstudio_test`, database `dbstudio`
(MySQL/MariaDB also have `root` with the same password). These credentials are throw-away and
bound to localhost only. CI runs the same steps in the `integration` job.

## IPC types

`tauri-specta` has no stable 2.x release yet (only `2.0.0-rc.*`), so IPC types are written by hand:
`src/lib/types.ts` mirrors the Rust types and `src/lib/ipc.ts` wraps `invoke`. Keep them in sync
when changing Rust-side types. Outside Tauri (plain browser), `ipc.ts` serves data from
`src/lib/mock/`.

The app icon source is `app-icon.svg`; regenerate with `cargo tauri icon app-icon.svg -o src-tauri/icons`
(delete the generated `android/` and `ios/` folders afterwards, they are not used).
