# DBStudio

Cross-platform desktop tool for administering many database systems (PostgreSQL, MySQL/MariaDB,
SQL Server, Oracle, MongoDB, Redis/Valkey, Cassandra, ScyllaDB, ClickHouse, CockroachDB,
YugabyteDB, TiDB). Built with Rust + Tauri 2 and a React + TypeScript frontend.

**Status: phases 0–8 implemented, plus the backlog + new-driver round (backend only), not
released.** The six original database families have a driver
(connect, tree, data grid, editing, query editor/console, leases and manual transactions). On top
of them: SQL/Mongo/Redis statement classification as the safety layer (read-only connections,
typed confirmation for destructive statements on production), query history, saved queries and
restored editor tabs, EXPLAIN diagrams (12 plan formats with the newer databases), ERD with DDL generation, export/import
(SQL, CSV, JSON, XLSX) as background jobs, SSH tunnels, and backup/restore: the `.dbsbak` container
(gzip/zstd, optional AES-256-GCM), the integrated engine for SQL databases, native tools
(`pg_dump`, `mysqldump`, `mongodump`, `redis-cli --rdb`, SQL Server `BACKUP DATABASE`), cron
schedules with catch-up and retention, tray/autostart, and a headless CLI.

The **SQL debugger** (JetBrains-style, [docs/sql-debugger.md](docs/sql-debugger.md)) runs the
script of an editor tab one statement at a time on its lease, for every dialect: breakpoints
(condition, hit count, log message, mute), Step Over / Into / Out, Run to Cursor, a Debug tool
window (Frames, Variables, Watches, Console), inline values and read-only Evaluate, a debug
transaction that ends with Commit or Rollback (Rollback suggested) and a paused TTL, and every step
through the same guard as the editor. Step Into stored routines works on PostgreSQL with the
`pldebugger` extension.

The backlog + phase 9 round ([docs/design/backlog-gd9.md](docs/design/backlog-gd9.md)) added the
backend for: Cassandra/ScyllaDB and ClickHouse drivers; CockroachDB and YugabyteDB as flavours of
the PostgreSQL driver and TiDB as a flavour of the MySQL driver (detected from `version()`); CQL
and ClickHouse dialects plus MySQL `ANSI_QUOTES` in the statement classifier; automatic reconnect
and lease keepalive; custom environment labels; plan history; script templates; Redis key export,
Mongo flattening and import into MongoDB/Redis; logical backup of MongoDB/Redis, export and
backup/restore on the six new databases, partial restore; OS notifications in Vietnamese or
English; TLS host-name checks through SSH tunnels for MySQL, MongoDB, Redis, Cassandra/ScyllaDB and
ClickHouse (R6). **The UI for these is not
built yet** (new connection kinds, settings page, reconnect banner, ...; list in
[docs/roadmap.md](docs/roadmap.md#việc-ui-để-lại)).

Verified so far only on macOS, through unit tests, integration tests against Docker databases and
the UI in a browser (mock IPC / dev bridge). Not verified yet: the real Tauri window (tray,
autostart, OS notifications, PNG/PDF export on the WebView), Windows and Linux, SQL Server NTLM
against a domain, Azure SQL, the Oracle integration tests against a running database (so also
Oracle backup/restore), ClickHouse 23.8, and CI on GitHub. Details: [docs/roadmap.md](docs/roadmap.md).

## Releases

Tagging `vX.Y.Z` builds installers for macOS, Windows and Linux and publishes a GitHub Release
(`pnpm release patch|minor|major`): [docs/release.md](docs/release.md) (Vietnamese).

## Layout

| Path                        | Purpose                                                                                                                     |
| --------------------------- | --------------------------------------------------------------------------------------------------------------------------- |
| `crates/dbstudio-core`      | Domain types, `Driver` / `Session` / `Lease` traits, `lease_kit` actor helper. No Tauri.                                    |
| `crates/dbstudio-sql`       | Lexer, splitter, parameters, statement classifier, guard policy, redaction (all dialects).                                  |
| `crates/dbstudio-plan`      | EXPLAIN parsers (12 formats: PG, MySQL, MariaDB, MSSQL, Oracle, Mongo, ClickHouse, CockroachDB, TiDB, CQL tracing) + rules. |
| `crates/dbstudio-export`    | Export engine (SQL, CSV, JSON, XLSX) and import (CSV, XLSX, JSON, SQL script).                                              |
| `crates/dbstudio-backup`    | `.dbsbak` backup/restore, native tools, scheduler and retention logic.                                                      |
| `crates/dbstudio-cli`       | `dbstudio-cli` binary: backup/restore/verify/tools without the GUI.                                                         |
| `crates/dbstudio-ssh`       | SSH tunnel (russh): password, private key + passphrase, agent; host key check.                                              |
| `crates/dbstudio-store`     | SQLite store (profiles, history, saved queries, tabs, ERD layouts, presets) + keychain.                                     |
| `crates/dbstudio-debug`     | SQL debugger engine: session actor, step program, read-only evaluation, PostgreSQL routine debugger (`pldbgapi`). No Tauri. |
| `crates/drivers/postgres`   | PostgreSQL (`tokio-postgres`); CockroachDB and YugabyteDB YSQL flavours.                                                    |
| `crates/drivers/mysql`      | MySQL / MariaDB (`mysql_async`); TiDB flavour.                                                                              |
| `crates/drivers/mssql`      | SQL Server (`tiberius` 0.13, minimal patch in `vendor/tiberius-patch`).                                                     |
| `crates/drivers/oracle`     | Oracle (`oracle` crate / ODPI-C; Instant Client loaded at run time).                                                        |
| `crates/drivers/mongodb`    | MongoDB (official driver; Mongo shell subset parsed by `dbstudio-sql`).                                                     |
| `crates/drivers/redis`      | Redis / Valkey (`redis` crate).                                                                                             |
| `crates/drivers/cassandra`  | Cassandra / ScyllaDB (`scylla` 1.10; two registered drivers).                                                               |
| `crates/drivers/clickhouse` | ClickHouse over HTTP(S) (`clickhouse` 0.15 + own RowBinary decoder).                                                        |
| `vendor/tiberius-patch`     | Patched tiberius: INFO/DONE items, attention cancel, exact money, NTLM on Unix.                                             |
| `src-tauri`                 | Thin Tauri app crate: state, IPC commands, guard, leases, jobs, path grants.                                                |
| `src`                       | React app (`app/`, `features/`, `components/ui/`, `lib/`, `stores/`, `locales/`).                                           |
| `contract/fixtures`         | JSON samples shared by the Rust and TypeScript contract tests.                                                              |
| `docker`                    | Test databases (`compose.test.yml`), certificates, SSH keys, Oracle test image.                                             |

## Requirements

- Rust stable (>= 1.94, required by `scylla`) and the [Tauri 2 prerequisites](https://tauri.app/start/prerequisites/)
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

All eight driver crates are cargo features of `src-tauri` and `dbstudio-cli`, enabled by default
(`postgres`, `mysql`, `mssql`, `oracle`, `mongodb`, `redis`, `cassandra`, `clickhouse`); the
CockroachDB, YugabyteDB and TiDB drivers come with `postgres` / `mysql`.

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

`dbstudio-cli` shares the app's SQLite store (`--data-dir`, default: the app data directory), the
keychain and the per-job lock files, so a job run from cron / launchd / Task Scheduler never
overlaps with the same job in the app:

```sh
dbstudio-cli backup --list
dbstudio-cli backup --job <job-id>                       # exit code 3 = job already running
dbstudio-cli verify --file backup.dbsbak                 # passphrase from $DBSTUDIO_PASSPHRASE
dbstudio-cli restore --file backup.dbsbak --connection <id> --database app \
  --yes --confirm-text app                               # production target or --clean
dbstudio-cli tools                                       # native tools that were found
```

`src-tauri` embeds the frontend, so `cargo build -p dbstudio`, `cargo clippy --workspace` and
`cargo test --workspace` need `dist/` to exist: run `pnpm build` first.

### Data directory

Debug builds from several worktrees share the app data directory
(`~/Library/Application Support/dev.dbstudio.desktop` on macOS), so a branch with a newer
migration breaks the others. Set `DBSTUDIO_DATA_DIR=/path/to/dir` to give a build its own
directory (also works in release builds, e.g. portable use or tests). When startup fails (for
example the store was written by a newer version) the app shows an error dialog and exits with
code 1; `DBSTUDIO_STARTUP_NO_DIALOG=1` prints the message to stderr instead (tests).

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

`up -d --wait` with no service starts every service without a profile, which needs a lot of
memory: **SQL Server needs ~2 GB and Oracle Free ~3 GB** (container limits are set), the others are
small. On Apple Silicon SQL Server, MongoDB 4.4, MySQL 5.7 and Oracle run under emulation. Start
only what you test.

The services of the newer databases live in compose **profiles** (`wide`, `analytics`, `newsql`,
`heavy`, `extra`), so a plain `up` never starts them; naming a service on the command line enables
its profile. Start **one profile at a time** and never together with SQL Server / Oracle (the
Docker VM of the development machine has ~8 GB):

```sh
docker compose -f docker/compose.test.yml up -d --wait scylla cassandra5        # profile wide
docker compose -f docker/compose.test.yml --profile newsql up -d --wait         # whole profile
```

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

Services in profiles (the CQL / SQL logins `dbstudio` / `dbstudio_test` are created by the tests
from the bootstrap account listed here):

| Service        | Profile (RAM)        | Image                  | Host port (env var)                                                                             | TLS                            | Login                                       |
| -------------- | -------------------- | ---------------------- | ----------------------------------------------------------------------------------------------- | ------------------------------ | ------------------------------------------- |
| `scylla`       | `wide` (~2 GB)       | ScyllaDB 2026.1        | `19060` (`DBSTUDIO_IT_SCYLLA_PORT`), TLS `19062` (`DBSTUDIO_IT_SCYLLA_TLS_PORT`)                | yes on 19062 (same CA)         | `cassandra` / `cassandra`                   |
| `cassandra5`   | `wide`               | Cassandra 5.0.9        | `19050` (`DBSTUDIO_IT_CASSANDRA5_PORT`)                                                         | no                             | `cassandra` / `cassandra`                   |
| `clickhouse26` | `analytics` (~1 GB)  | ClickHouse 26.3 LTS    | HTTP `18123` (`DBSTUDIO_IT_CLICKHOUSE_PORT`), HTTPS `18443` (`DBSTUDIO_IT_CLICKHOUSE_TLS_PORT`) | HTTPS (same CA)                | `dbstudio` / `dbstudio_test`, db `dbstudio` |
| `cockroach`    | `newsql` (~1.5–2 GB) | CockroachDB v25.4.20   | `26258` (`DBSTUDIO_IT_COCKROACH_PORT`)                                                          | yes (`docker/certs/cockroach`) | `root` (client cert)                        |
| `tidb`         | `newsql`             | TiDB v8.5.7 (unistore) | `34000` (`DBSTUDIO_IT_TIDB_PORT`)                                                               | no                             | `root`, no password                         |
| `yugabyte`     | `heavy` (~2 GB)      | YugabyteDB 2025.2.8.1  | `54333` (`DBSTUDIO_IT_YUGABYTE_PORT`)                                                           | no                             | `yugabyte` / `yugabyte`                     |
| `cassandra41`  | `extra` (1 GB each)  | Cassandra 4.1.12       | `19041` (`DBSTUDIO_IT_CASSANDRA41_PORT`)                                                        | no                             | `cassandra` / `cassandra`                   |
| `scylla62`     | `extra`              | ScyllaDB 6.2           | `19061` (`DBSTUDIO_IT_SCYLLA62_PORT`)                                                           | no                             | `cassandra` / `cassandra`                   |
| `clickhouse23` | `extra`              | ClickHouse 23.8        | `18023` (`DBSTUDIO_IT_CLICKHOUSE23_PORT`)                                                       | no                             | `dbstudio` / `dbstudio_test`                |

Container limits: `scylla`, `clickhouse26`, `cockroach` and the `extra` services 1 GB,
`cassandra5` 1.5 GB, `tidb` 768 MB, `yugabyte` 2 GB.

`sshd` shares the compose network with every database, so tunnels can target the service names
(`pg16:5432`, `mysql8:3306`, `mssql2022:1433`, ...).

Running one group at a time (start the listed services first):

| Group                       | Services                                                                                    | Command                                                                 |
| --------------------------- | ------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------- |
| PostgreSQL driver           | `pg16`, `pg12`                                                                              | `DBSTUDIO_IT=1 cargo test -p dbstudio-driver-postgres`                  |
| MySQL / MariaDB driver      | `mysql8`, `mariadb`, `mariadb104`, `mysql57`                                                | `DBSTUDIO_IT=1 cargo test -p dbstudio-driver-mysql`                     |
| SQL Server driver           | `mssql2022`                                                                                 | `DBSTUDIO_IT=1 cargo test -p dbstudio-driver-mssql`                     |
| MongoDB driver              | `mongo44`, `mongo7`                                                                         | `DBSTUDIO_IT=1 cargo test -p dbstudio-driver-mongodb`                   |
| Redis / Valkey driver       | `redis6`, `redis7`, `valkey8`                                                               | `DBSTUDIO_IT=1 cargo test -p dbstudio-driver-redis`                     |
| SSH tunnel                  | `sshd` + `pg16`, `mysql8`, `mssql2022`                                                      | `DBSTUDIO_IT=1 cargo test -p dbstudio-ssh`                              |
| Export / import             | `pg16`, `pg12`, `mysql8`, `mariadb` (+ `redis7`, `valkey8`, `mongo7`, `mongo44`; newer DBs) | `DBSTUDIO_IT=1 cargo test -p dbstudio-export`                           |
| Backup / restore            | `pg16`, `pg12`, `mysql8`, `mariadb`, `sshd` (+ `mssql2022`, Mongo/Redis; newer DBs)         | `DBSTUDIO_IT=1 cargo test -p dbstudio-backup`                           |
| Cassandra / ScyllaDB driver | `scylla`, `cassandra5` (`extra`: `cassandra41`, `scylla62`)                                 | `DBSTUDIO_IT=1 cargo test -p dbstudio-driver-cassandra`                 |
| ClickHouse driver           | `clickhouse26` (`extra`: `clickhouse23`)                                                    | `DBSTUDIO_IT=1 cargo test -p dbstudio-driver-clickhouse`                |
| CockroachDB / YugabyteDB    | `cockroach` (`heavy`: `yugabyte`)                                                           | `DBSTUDIO_IT=1 cargo test -p dbstudio-driver-postgres --test it_flavor` |
| TiDB                        | `tidb` (+ `sshd` for the TLS-through-SSH test)                                              | `DBSTUDIO_IT=1 cargo test -p dbstudio-driver-mysql --test it_tidb`      |
| Command layer (`src-tauri`) | `pg16`, `mysql8`, `redis6`                                                                  | `DBSTUDIO_IT=1 cargo test -p dbstudio`                                  |
| Query Advisor (`src-tauri`) | `pg16`, `mysql8`, `mariadb` (+ `mssql2022`, `mongo7`, `clickhouse26`)                       | `DBSTUDIO_IT=1 cargo test -p dbstudio --lib plan_advice_it`             |
| Oracle driver               | `oraclefree`                                                                                | `docker/oracle-it/run-tests.sh` (see below)                             |
| SQL debugger                | `pg16dbg` (profile `debug`), `pg16`, `mysql8`                                               | see below                                                               |

The SQL debugger suites (`crates/dbstudio-debug/tests`) need `pg16dbg`, a PostgreSQL 16 image with
`pldebugger` built locally from `docker/pg16dbg` (profile `debug`, ~300 MB, port `54317`,
`DBSTUDIO_IT_PGDBG_PORT`, login `dbstudio` / `dbstudio_test`, superuser), plus `pg16` and `mysql8`
for the script debugger:

```sh
docker compose -f docker/compose.test.yml --profile debug up -d --wait pg16dbg pg16 mysql8
DBSTUDIO_IT=1 DBSTUDIO_IT_PGDBG_PORT=54317 cargo test -p dbstudio-debug -- --test-threads=2
```

The Query Advisor suite (`src-tauri/src/plan_advice_it.rs`, one test per service) seeds a 200 000 row
table in a `dbstudio_it_adv*` schema / database, runs the DDL the advice generates and explains
again; a service that is not running is skipped unless `DBSTUDIO_IT_STRICT` lists it (for example
`DBSTUDIO_IT_STRICT=pg16,mysql8`). Run one service with `... plan_advice_it::pg16`.

The export / backup suites for the newer databases (`it_newdb`) need the profile services of the
database they test (ScyllaDB, Cassandra 5, ClickHouse 26, CockroachDB, YugabyteDB, TiDB).

The original suites fail when their service is not running. The suites added in the backlog +
phase 9 round (Cassandra, ClickHouse, the CockroachDB / YugabyteDB / TiDB flavours, the Mongo/Redis
key-value tests, `it_nosql` / `it_newdb` / `it_mssql` of export and backup, the SSH tunnel to SQL
Server) probe their server first: when it is not reachable they print `skip` and pass, unless
`DBSTUDIO_IT_STRICT=1`, which turns a missing server into a failure (CI sets it). `DBSTUDIO_IT_BIG=1`
turns on the heavy cases (large tables / one million Redis keys).

The Oracle driver needs Instant Client, so its tests run in Linux inside the `oracle-it` image:
build it once with `docker compose -f docker/compose.test.yml build oracle-it`, start `oraclefree`,
then run `docker/oracle-it/run-tests.sh` (extra arguments go to `cargo test`). The script mounts
the repository it is run from, so it also works from a git worktree (unlike
`docker compose -f docker/compose.test.yml run --rm oracle-it`, which mounts the directory of the
compose file). Alternatively install Instant Client locally and set
`DBSTUDIO_ORACLE_CLIENT_LIB_DIR`.

The Keychain test of the secret store runs only with `DBSTUDIO_KEYCHAIN_IT=1 cargo test -p dbstudio-store real_keychain` and uses a separate test service name. These credentials are throw-away and
bound to localhost only. CI runs the same steps in the `integration` job, one matrix entry per
group (`core` = every service without a profile, `wide`, `analytics`, `newsql`, `debug`) with
`DBSTUDIO_IT_STRICT=1`; `heavy` and `extra` run weekly in `integration-extra`.

## IPC types

`tauri-specta` has no stable 2.x release yet (only `2.0.0-rc.*`), so IPC types are written by hand:
`src/lib/types/` mirrors the Rust types and `src/lib/ipc/` wraps `invoke` (`src/lib/types.ts` and
`src/lib/ipc.ts` re-export them). Keep them in sync when changing Rust-side types; the contract
tests (`crates/dbstudio-core/tests/contract.rs`, `src/lib/types/contract.test.ts`) compare both
sides against `contract/fixtures`. Outside Tauri (plain browser), the IPC layer serves data from
`src/lib/mock/`.

The app icon source is `app-icon.svg`; regenerate with `cargo tauri icon app-icon.svg -o src-tauri/icons`
(delete the generated `android/` and `ios/` folders afterwards, they are not used).
