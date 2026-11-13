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

## Dev bridge

To exercise the UI end to end against the real backend (real databases and keychain) from a
normal browser, run the app with the debug-only `dev-bridge` feature. It starts an HTTP server on
`127.0.0.1:1421` (change with `DBSTUDIO_DEV_BRIDGE_PORT`) inside the app process, only when a
token is set:

```sh
DBSTUDIO_DEV_BRIDGE_TOKEN=<token> pnpm tauri dev --features dev-bridge
# in another shell, point the frontend at it:
VITE_IPC_BRIDGE_URL=http://127.0.0.1:1421 VITE_IPC_BRIDGE_TOKEN=<token> pnpm dev   # http://localhost:1420
```

`POST /ipc/<command>` takes the same camelCase arguments as Tauri's `invoke` and needs the
`x-dbstudio-bridge-token` header; `table_fetch` streams NDJSON. **Development only**: the bridge
exposes every command (including saved connections) to anything that knows the token. It is not
compiled in release builds (enabling the feature there is a compile error) and the default build
does not include it.

## Integration tests

The driver and command-layer tests that talk to real servers only run when `DBSTUDIO_IT=1`
(without it they pass immediately; with it, an unreachable server fails the test). They need
Docker:

```sh
sh docker/gen-certs.sh && sh docker/gen-ssh-keys.sh      # test CA/certs and the SSH test key (git-ignored)
docker compose -f docker/compose.test.yml up -d --wait pg16 mysql8 # start only what you test
DBSTUDIO_IT=1 cargo test --workspace
docker compose -f docker/compose.test.yml stop                     # or `down -v` to remove everything
```

Start individual services with `docker compose -f docker/compose.test.yml up -d --wait <service>`.
`up -d --wait` with no service starts all of them, which needs a lot of memory: **SQL Server needs
~2 GB and Oracle Free ~3 GB** (container limits are set), the others are small. On Apple Silicon
SQL Server, MongoDB 4.4, MySQL 5.7 and Oracle run under emulation. Start only what you test.

| Service      | Image                                 | Host port (env var)                            | TLS                                  | Login                                                                                        |
| ------------ | ------------------------------------- | ---------------------------------------------- | ------------------------------------ | -------------------------------------------------------------------------------------------- |
| `pg16`       | PostgreSQL 16                         | `127.0.0.1:54316`                              | yes (CA `docker/certs/ca.crt`)       | `dbstudio` / `dbstudio_test`, db `dbstudio`                                                  |
| `pg12`       | PostgreSQL 12                         | `127.0.0.1:54312`                              | no                                   | same                                                                                         |
| `mysql8`     | MySQL 8                               | `127.0.0.1:33068`                              | yes (same CA)                        | same (+ `root`)                                                                              |
| `mariadb`    | MariaDB 10.6                          | `127.0.0.1:33106`                              | no                                   | same (+ `root`)                                                                              |
| `mariadb104` | MariaDB 10.4                          | `127.0.0.1:33104`                              | no                                   | same (+ `root`)                                                                              |
| `mysql57`    | MySQL 5.7 (amd64)                     | `127.0.0.1:33057`                              | no                                   | same (+ `root`)                                                                              |
| `mssql2022`  | SQL Server 2022 (amd64)               | `127.0.0.1:14333` (`DBSTUDIO_IT_MSSQL_PORT`)   | yes (self-signed, trust cert)        | `sa` / `Dbstudio_test1!`                                                                     |
| `oraclefree` | Oracle Database 23ai Free             | `127.0.0.1:15213` (`DBSTUDIO_IT_ORACLE_PORT`)  | no                                   | `dbstudio` / `dbstudio_test`, service `FREEPDB1` (`SYSTEM` / `dbstudio_test`)                |
| `mongo44`    | MongoDB 4.4 (amd64, standalone)       | `127.0.0.1:27044` (`DBSTUDIO_IT_MONGO44_PORT`) | no                                   | `dbstudio` / `dbstudio_test`, `authSource=admin`                                             |
| `mongo7`     | MongoDB 7, replica set `rs0` (1 node) | `127.0.0.1:27077` (`DBSTUDIO_IT_MONGO7_PORT`)  | preferTLS (CA `docker/certs/ca.crt`) | same; use `directConnection=true`                                                            |
| `redis6`     | Redis 6.2                             | `127.0.0.1:63796` (`DBSTUDIO_IT_REDIS6_PORT`)  | no                                   | ACL user `dbstudio` / `dbstudio_test`; `KEYS` disabled                                       |
| `redis7`     | Redis 7.4                             | `127.0.0.1:63797` (`DBSTUDIO_IT_REDIS7_PORT`)  | TLS only (same CA)                   | user `dbstudio` / `dbstudio_test`                                                            |
| `valkey8`    | Valkey 8                              | `127.0.0.1:63798` (`DBSTUDIO_IT_VALKEY8_PORT`) | no                                   | password `dbstudio_test` (default user or `dbstudio`)                                        |
| `sshd`       | OpenSSH (linuxserver)                 | `127.0.0.1:22022` (`DBSTUDIO_IT_SSH_PORT`)     | n/a                                  | `dbstudio` / `dbstudio_ssh`, or key `docker/ssh/id_ed25519` (passphrase `dbstudio_key_pass`) |

`sshd` shares the compose network with every database, so tunnels can target the service names
(`pg16:5432`, `mysql8:3306`, `mssql2022:1433`, ...). The Oracle driver needs Oracle Instant Client:
run its tests in Linux with
`docker compose -f docker/compose.test.yml run --rm oracle-it` (profile `tools`, not started by
`up`; uses `oraclefree`), or install Instant Client locally and set `ORACLE_CLIENT_LIB_DIR`.

The Keychain test of the secret store runs only with `DBSTUDIO_KEYCHAIN_IT=1 cargo test -p dbstudio-store real_keychain` and uses a separate test service name. These credentials are throw-away and
bound to localhost only. CI runs the same steps in the `integration` job.

## IPC types

`tauri-specta` has no stable 2.x release yet (only `2.0.0-rc.*`), so IPC types are written by hand:
`src/lib/types.ts` mirrors the Rust types and `src/lib/ipc.ts` wraps `invoke`. Keep them in sync
when changing Rust-side types. Outside Tauri (plain browser), `ipc.ts` serves data from
`src/lib/mock/`.

The app icon source is `app-icon.svg`; regenerate with `cargo tauri icon app-icon.svg -o src-tauri/icons`
(delete the generated `android/` and `ios/` folders afterwards, they are not used).
