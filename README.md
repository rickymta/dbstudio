# DBStudio

Cross-platform desktop tool for administering many database systems (PostgreSQL, MySQL/MariaDB,
SQL Server, Oracle, MongoDB, Redis/Valkey; Cassandra/ScyllaDB and ClickHouse later). Built with
Rust + Tauri 2 and a React + TypeScript frontend.

**Status: phases 0–8 implemented, not released.** All six database families have a driver
(connect, tree, data grid, editing, query editor/console, leases and manual transactions). On top
of them: SQL/Mongo/Redis statement classification as the safety layer (read-only connections,
typed confirmation for destructive statements on production), query history, saved queries and
restored editor tabs, EXPLAIN diagrams (8 plan formats), ERD with DDL generation, export/import
(SQL, CSV, JSON, XLSX) as background jobs, and SSH tunnels. Backup/restore (`.dbsbak` container,
scheduler, tray, CLI) is being finished on a separate branch; on this branch `dbstudio-backup` and
`dbstudio-cli` are still the contract scaffold.

Verified so far only on macOS, through unit tests, integration tests against Docker databases and
the UI in a browser (mock IPC / dev bridge). Not verified yet: the real Tauri window for the new
features (tray, autostart, OS notifications, PNG/PDF export on the WebView), Windows and Linux,
SQL Server NTLM against a domain, Azure SQL, the Oracle integration tests against a running
database, and CI on GitHub. Details: [docs/roadmap.md](docs/roadmap.md).

## Layout

| Path                      | Purpose                                                                                    |
| ------------------------- | ------------------------------------------------------------------------------------------ |
| `crates/dbstudio-core`    | Domain types, `Driver` / `Session` / `Lease` traits, `lease_kit` actor helper. No Tauri.   |
| `crates/dbstudio-sql`     | Lexer, splitter, parameters, statement classifier, guard policy, redaction (all dialects). |
| `crates/dbstudio-plan`    | EXPLAIN parsers (PG, MySQL JSON v1/v2/tree, MariaDB, SHOWPLAN XML, Oracle, Mongo) + rules. |
| `crates/dbstudio-export`  | Export engine (SQL, CSV, JSON, XLSX) and import (CSV, XLSX, JSON, SQL script).             |
| `crates/dbstudio-backup`  | `.dbsbak` backup/restore, native tools, scheduler logic (scaffold here, see Status).       |
| `crates/dbstudio-cli`     | `dbstudio-cli` binary: backup/restore/verify/tools without the GUI (scaffold here).        |
| `crates/dbstudio-ssh`     | SSH tunnel (russh): password, private key + passphrase, agent; host key check.             |
| `crates/dbstudio-store`   | SQLite store (profiles, history, saved queries, tabs, ERD layouts, presets) + keychain.    |
| `crates/drivers/postgres` | PostgreSQL (`tokio-postgres`).                                                             |
| `crates/drivers/mysql`    | MySQL / MariaDB (`mysql_async`).                                                           |
| `crates/drivers/mssql`    | SQL Server (`tiberius` 0.13, minimal patch in `vendor/tiberius-patch`).                    |
| `crates/drivers/oracle`   | Oracle (`oracle` crate / ODPI-C; Instant Client loaded at run time).                       |
| `crates/drivers/mongodb`  | MongoDB (official driver; Mongo shell subset parsed by `dbstudio-sql`).                    |
| `crates/drivers/redis`    | Redis / Valkey (`redis` crate).                                                            |
| `vendor/tiberius-patch`   | Patched tiberius: INFO/DONE items, attention cancel, exact money, NTLM on Unix.            |
| `src-tauri`               | Thin Tauri app crate: state, IPC commands, guard, leases, jobs, path grants.               |
| `src`                     | React app (`app/`, `features/`, `components/ui/`, `lib/`, `stores/`, `locales/`).          |
| `contract/fixtures`       | JSON samples shared by the Rust and TypeScript contract tests.                             |
| `docker`                  | Test databases (`compose.test.yml`), certificates, SSH keys, Oracle test image.            |

## Requirements

- Rust stable (>= 1.85) and the [Tauri 2 prerequisites](https://tauri.app/start/prerequisites/)
  (Linux: `webkit2gtk-4.1`, `libgtk-3-dev`, ...).
- Node.js 22+ and pnpm 9.
- Linux system packages (Debian/Ubuntu, the same list CI installs):

  ```sh
  sudo apt-get install -y libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev \
    librsvg2-dev libxdo-dev libssl-dev build-essential curl wget file
  ```

- Optional, at run time only: [Oracle Instant Client](https://www.oracle.com/database/technologies/instant-client.html)
  for Oracle connections. The driver builds without it (ODPI-C loads `libclntsh` lazily); when it
  is missing the Oracle driver shows as unavailable with instructions. Point the app at it with
  `DBSTUDIO_ORACLE_CLIENT_LIB_DIR` (an `oracle_client` setting in the store is also read at
  startup; there is no UI for it yet).

All six drivers are cargo features of `src-tauri`, enabled by default (`postgres`, `mysql`, `mssql`,
`oracle`, `mongodb`, `redis`).

## Commands

```sh
pnpm install
pnpm tauri dev          # desktop app with hot reload
pnpm dev                # frontend only in a browser (IPC falls back to mock data)
pnpm tauri build        # production bundle

pnpm lint && pnpm test && pnpm build && pnpm format:check
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

cargo run -p dbstudio-cli -- --help   # headless CLI (backup/restore/verify/tools)
```

`src-tauri` embeds the frontend, so `cargo build -p dbstudio`, `cargo clippy --workspace` and
`cargo test --workspace` need `dist/` to exist: run `pnpm build` first.

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
`x-dbstudio-bridge-token` header; commands with a result channel (`table_fetch`, `query_execute`,
`lease_execute`, `lease_fetch_more`) stream NDJSON, and `GET /events` streams the app events
(`job://progress`, `lease://state`, ...). **Development only**: the bridge exposes every command
(including saved connections) to anything that knows the token. It is not compiled in release
builds (enabling the feature there is a compile error) and the default build does not include it.

## Integration tests

The driver and command-layer tests that talk to real servers only run when `DBSTUDIO_IT=1`
(without it they pass immediately; with it, an unreachable server fails the test). They need
Docker:

```sh
sh docker/gen-certs.sh && sh docker/gen-ssh-keys.sh      # test CA/certs and the SSH test key (git-ignored)
docker compose -f docker/compose.test.yml up -d --wait pg16 mysql8 # start only what you test
DBSTUDIO_IT=1 cargo test -p dbstudio-driver-postgres
docker compose -f docker/compose.test.yml stop                     # or `down -v` to remove everything
```

`up -d --wait` with no service starts all of them, which needs a lot of memory: **SQL Server needs
~2 GB and Oracle Free ~3 GB** (container limits are set), the others are small. On Apple Silicon
SQL Server, MongoDB 4.4, MySQL 5.7 and Oracle run under emulation. Start only what you test.

| Service      | Image                                  | Host port (env var)                            | TLS                                  | Login                                                                                        |
| ------------ | -------------------------------------- | ---------------------------------------------- | ------------------------------------ | -------------------------------------------------------------------------------------------- |
| `pg16`       | PostgreSQL 16                          | `127.0.0.1:54316`                              | yes (CA `docker/certs/ca.crt`)       | `dbstudio` / `dbstudio_test`, db `dbstudio`                                                  |
| `pg12`       | PostgreSQL 12                          | `127.0.0.1:54312`                              | no                                   | same                                                                                         |
| `mysql8`     | MySQL 8                                | `127.0.0.1:33068`                              | yes (same CA)                        | same (+ `root`)                                                                              |
| `mariadb`    | MariaDB 10.6                           | `127.0.0.1:33106`                              | no                                   | same (+ `root`)                                                                              |
| `mariadb104` | MariaDB 10.4                           | `127.0.0.1:33104`                              | no                                   | same (+ `root`)                                                                              |
| `mysql57`    | MySQL 5.7 (amd64)                      | `127.0.0.1:33057`                              | no                                   | same (+ `root`)                                                                              |
| `mssql2022`  | SQL Server 2022 (amd64, ~2 GB)         | `127.0.0.1:14333` (`DBSTUDIO_IT_MSSQL_PORT`)   | yes (self-signed, trust cert)        | `sa` / `Dbstudio_test1!`                                                                     |
| `oraclefree` | Oracle Database 23ai Free (~3 GB)      | `127.0.0.1:15213` (`DBSTUDIO_IT_ORACLE_PORT`)  | no                                   | `dbstudio` / `dbstudio_test`, service `FREEPDB1` (`SYSTEM` / `dbstudio_test`)                |
| `oracle-it`  | Oracle Linux 9 + Instant Client + Rust | — (profile `tools`, not started by `up`)       | n/a                                  | runs the Oracle driver tests against `oraclefree`                                            |
| `mongo44`    | MongoDB 4.4 (amd64, standalone)        | `127.0.0.1:27044` (`DBSTUDIO_IT_MONGO44_PORT`) | no                                   | `dbstudio` / `dbstudio_test`, `authSource=admin`                                             |
| `mongo7`     | MongoDB 7, replica set `rs0` (1 node)  | `127.0.0.1:27077` (`DBSTUDIO_IT_MONGO7_PORT`)  | preferTLS (CA `docker/certs/ca.crt`) | same; use `directConnection=true`                                                            |
| `redis6`     | Redis 6.2                              | `127.0.0.1:63796` (`DBSTUDIO_IT_REDIS6_PORT`)  | no                                   | ACL user `dbstudio` / `dbstudio_test`; `KEYS` disabled                                       |
| `redis7`     | Redis 7.4                              | `127.0.0.1:63797` (`DBSTUDIO_IT_REDIS7_PORT`)  | TLS only (same CA)                   | user `dbstudio` / `dbstudio_test`                                                            |
| `valkey8`    | Valkey 8                               | `127.0.0.1:63798` (`DBSTUDIO_IT_VALKEY8_PORT`) | no                                   | password `dbstudio_test` (default user or `dbstudio`)                                        |
| `sshd`       | OpenSSH (linuxserver)                  | `127.0.0.1:22022` (`DBSTUDIO_IT_SSH_PORT`)     | n/a                                  | `dbstudio` / `dbstudio_ssh`, or key `docker/ssh/id_ed25519` (passphrase `dbstudio_key_pass`) |

`sshd` shares the compose network with every database, so tunnels can target the service names
(`pg16:5432`, `mysql8:3306`, `mssql2022:1433`, ...).

Running one group at a time (start the listed services first):

| Group                       | Services                                     | Command                                                |
| --------------------------- | -------------------------------------------- | ------------------------------------------------------ |
| PostgreSQL driver           | `pg16`, `pg12`                               | `DBSTUDIO_IT=1 cargo test -p dbstudio-driver-postgres` |
| MySQL / MariaDB driver      | `mysql8`, `mariadb`, `mariadb104`, `mysql57` | `DBSTUDIO_IT=1 cargo test -p dbstudio-driver-mysql`    |
| SQL Server driver           | `mssql2022`                                  | `DBSTUDIO_IT=1 cargo test -p dbstudio-driver-mssql`    |
| MongoDB driver              | `mongo44`, `mongo7`                          | `DBSTUDIO_IT=1 cargo test -p dbstudio-driver-mongodb`  |
| Redis / Valkey driver       | `redis6`, `redis7`, `valkey8`                | `DBSTUDIO_IT=1 cargo test -p dbstudio-driver-redis`    |
| SSH tunnel                  | `sshd` + `pg16`, `mysql8`, `mssql2022`       | `DBSTUDIO_IT=1 cargo test -p dbstudio-ssh`             |
| Export / import             | `pg16`, `pg12`, `mysql8`, `mariadb`          | `DBSTUDIO_IT=1 cargo test -p dbstudio-export`          |
| Command layer (`src-tauri`) | `pg16`, `mysql8`, `redis6`                   | `DBSTUDIO_IT=1 cargo test -p dbstudio`                 |
| Oracle driver               | `oraclefree`                                 | `docker/oracle-it/run-tests.sh` (see below)            |

A test whose service is not running fails; `DBSTUDIO_IT_BIG=1` turns on the heavy cases (large
tables / one million Redis keys).

The Oracle driver needs Instant Client, so its tests run in Linux inside the `oracle-it` image:
build it once with `docker compose -f docker/compose.test.yml build oracle-it`, start `oraclefree`,
then run `docker/oracle-it/run-tests.sh` (extra arguments go to `cargo test`). The script mounts
the repository it is run from, so it also works from a git worktree (unlike
`docker compose -f docker/compose.test.yml run --rm oracle-it`, which mounts the directory of the
compose file). Alternatively install Instant Client locally and set
`DBSTUDIO_ORACLE_CLIENT_LIB_DIR`.

The Keychain test of the secret store runs only with `DBSTUDIO_KEYCHAIN_IT=1 cargo test -p dbstudio-store real_keychain` and uses a separate test service name. These credentials are throw-away and
bound to localhost only. CI runs the same steps in the `integration` job.

## IPC types

`tauri-specta` has no stable 2.x release yet (only `2.0.0-rc.*`), so IPC types are written by hand:
`src/lib/types/` mirrors the Rust types and `src/lib/ipc/` wraps `invoke` (`src/lib/types.ts` and
`src/lib/ipc.ts` re-export them). Keep them in sync when changing Rust-side types; the contract
tests (`crates/dbstudio-core/tests/contract.rs`, `src/lib/types/contract.test.ts`) compare both
sides against `contract/fixtures`. Outside Tauri (plain browser), the IPC layer serves data from
`src/lib/mock/`.

The app icon source is `app-icon.svg`; regenerate with `cargo tauri icon app-icon.svg -o src-tauri/icons`
(delete the generated `android/` and `ios/` folders afterwards, they are not used).
