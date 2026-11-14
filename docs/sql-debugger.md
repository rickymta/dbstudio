# SQL debugger

DBStudio can debug the script of an editor tab the way JetBrains IDEs debug code: breakpoints in
the gutter, Step Over / Step Into / Step Out, Run to Cursor, a Debug tool window with Frames,
Variables, Watches and a Console, inline values and Evaluate Expression.

There are two layers:

- **Script debugger** (every SQL dialect, MongoDB shell, CQL, ClickHouse): the script runs **one
  statement at a time on the lease of the tab**, so the tab's transaction, temporary tables,
  `USE` and session variables are the ones being debugged.
- **Routine debugger** (PostgreSQL, and YugabyteDB when the server has the package): Step Into a
  PL/pgSQL function or procedure the statement calls, through the `pldebugger` extension
  (`pldbgapi`).

Design and rationale: [design/sql-debugger.md](design/sql-debugger.md) (Vietnamese). This page
describes what is implemented.

## Starting a session

1. Open an editor tab on a connection and write the script.
2. Press **Debug** (next to Run) or the Debug shortcut (Shift+F9, macOS ⌃D). With a non-empty
   selection only the statements that intersect the selection are debugged.
3. Without an enabled breakpoint the session stops on the first statement; with one it runs to
   the first breakpoint. "Stop on the first statement" in the Debug options overrides this.

The Debug button's menu holds the start options: transaction mode, what happens at the end,
stop on the first statement, pause on error (on by default: a failing statement pauses at that
statement; off: the session ends), inline values, keymap, and the routine debugging status of the
server.

While a session runs:

- The editor is **read-only** ("Debugging - stop to edit"). Restart reads the current text.
- The lease belongs to the debugger: Run, Begin/Commit/Rollback, Explain and the other editor
  commands on that lease answer `Busy` until the session ends.
- One session per lease, at most 8 sessions in the application.
- Closing the tab stops the session and rolls back the debug transaction (a toast says so).
  Closing the connection, losing the lease or a broken connection also end it.

Statements are split with the same splitter the driver and the guard use (dialect of the lease).
Blocks that cannot be entered are a single step: PostgreSQL `DO` and `CREATE FUNCTION/PROCEDURE`,
MySQL `CREATE PROCEDURE/FUNCTION/TRIGGER/EVENT`, Oracle PL/SQL blocks and units, CQL
`BEGIN BATCH`. `DELIMITER` (MySQL), `GO` (T-SQL) and SQL\*Plus commands (Oracle) are never a stop.

## Stepping

| Command | What it does |
|---|---|
| **Resume** | Runs statements up to the next breakpoint, or to the end of the script. Inside a routine: continue to the next routine breakpoint, then on through the script. |
| **Pause** | While running: stops **before the next statement**. The statement in progress is not interrupted (use Stop to cancel it). Not available inside a routine. |
| **Stop** | Cancels the running statement (aborts the routine), then ends the transaction with the end action. |
| **Rerun** | Stop + start again with the current editor text, the same breakpoints, watches and options. |
| **Step Over** | Runs the current statement and stops at the next one. Inside a routine: next line; after the last line, back to the script. |
| **Step Into** | PostgreSQL with `pldbgapi`: arms the routines the statement calls and stops at the first statement of the routine entered; inside a routine, steps into nested PL/pgSQL calls. When the statement calls nothing debuggable it runs as Step Over with a Console note. Greyed out on servers without a routine debugger. |
| **Smart Step Into** | Lets you pick one routine when the statement calls several (steps directly when there is one). |
| **Force Step Into** | Like Step Into, also arming trigger functions on the target tables of the statement and routines outside `search_path`. |
| **Step Out** | Only inside a routine: steps over until the call stack is shallower (simulated, at most 10 000 steps). |
| **Run to Cursor** | Runs to the statement at the cursor line, stopping at breakpoints on the way. The cursor must be after the current statement (otherwise use Step Over). Script only. |
| **Force Run to Cursor** | The same, ignoring breakpoints. |
| **Show Execution Point** | Scrolls the editor (or the routine source tab) to the current line. |
| **Evaluate Expression** | Opens the Evaluate dialog (see below). |

## Breakpoints

- Click the gutter left of the line numbers to add or remove a breakpoint; Alt+click disables it
  without removing it; right-click opens the breakpoint popover. Toggle Line Breakpoint uses the
  cursor line; View Breakpoints opens the Breakpoints dialog (all breakpoints, "Remove all",
  "Mute").
- A breakpoint lands on the statement that contains the line; a blank or comment line between
  statements snaps down to the next statement. Lines inside a block, on a meta command or after
  the last statement are **not verified** (hollow marker, with the reason).
- Properties (popover / dialog):
  - **Enabled**.
  - **Suspend**: off = never stop, only log.
  - **Condition**: SQL expression, evaluated read-only as `SELECT CASE WHEN (<cond>) THEN 1 ELSE 0
    END` (Oracle adds `FROM dual`). For MongoDB, Redis and CQL the condition is a read command,
    true when it returns a row/document or a reply other than nil/0/empty. A condition that
    fails **stops** the script, with the error in the Console.
  - **Hit count N**: stops from the N-th time the breakpoint is reached on (a condition that is
    false does not count).
  - **Log message**: text with `{expr}` placeholders, evaluated read-only and written to the
    Console (source `log`) every time the breakpoint is reached (and the hit count is met).
- **Mute Breakpoints** (tool window toolbar) ignores every script breakpoint without removing it.
- Script breakpoints, watches and the mute flag are saved with the editor tab. Routine
  breakpoints are saved per connection.

### Routine breakpoints

When the debugger stops inside a routine it opens a read-only source tab (`f_tax() [debug]`)
showing the routine body (`pg_proc.prosrc`); line numbers are those of the body. Breakpoints set
in that tab are routine breakpoints. Current limits:

- They are armed when you **Step Into** a statement (and stay armed while that statement runs, so
  Resume inside the routine stops at them). Step Over or Resume on a script statement does not
  stop inside the routines it calls.
- Only Enabled is honoured: condition, hit count, log message and suspend are ignored, and Mute
  does not apply.
- A breakpoint records a hash of the routine source; if the routine changed since, it is not
  armed and is reported as not verified ("the routine source has changed").

## Debug tool window

The tool window opens under the editor (a "Debug" tab next to "Results"), JetBrains classic
layout:

- **Vertical toolbar**: Rerun, Resume, Pause, Stop, View Breakpoints, Mute Breakpoints, Settings
  (end action, inline values, keymap).
- **Stepping toolbar**: Show Execution Point, Step Over, Step Into, Smart Step Into, Force Step
  Into, Step Out, Run to Cursor, Evaluate Expression. Tooltips show the shortcut of the active
  keymap. A status line shows the position (`line 8, statement 3/12`), the time of the last step,
  "Debug transaction" when the debugger owns the transaction and the TTL countdown in its last two
  minutes.
- **Frames**: innermost first. A routine frame shows `name(args):line`; the script frame shows the
  statement. Selecting a frame switches Variables, Watches and Evaluate to it.
- **Variables**: a lazy tree (a group is queried when opened):

  | Group | Content | Editable |
  |---|---|---|
  | Parameters | bind parameters of the script | yes, for the following steps |
  | Script variables | MySQL `@x`, T-SQL `DECLARE @x` | yes (`SET`, through the guard) |
  | Session | database, schema, user, pid/SPID, isolation, ... | no |
  | Settings | PostgreSQL session settings, ClickHouse changed settings | yes (`SET`, through the guard) |
  | Temporary tables | where the dialect can list them | no |
  | Transaction | state, owned by the debugger, open time | no |
  | Last result | rows / affected / time of the last step | no |
  | Routine | arguments and locals of the selected routine frame | yes (`pldbg_deposit_value`) |

  Set Value (F2) on editable rows; context menu: Copy Value, Copy Name, Add to Watches, Inspect.
  The input at the top evaluates an expression (Enter) or adds it as a watch (Shift+Enter).
- **Watches**: + (Insert) adds, − (Delete) removes, F2 or double click edits, drag to reorder.
  Watches are re-evaluated every time the session pauses (not between the steps of a Resume).
  A value is a scalar, a small table (up to 50 rows), an error (red) or "skipped" with the reason.
- **Console**: for each step a `▶ line: statement` prompt, the result as a compact grid (cut at
  200 rows by default, copy as TSV), messages (`NOTICE`, `PRINT`, `DBMS_OUTPUT`, breakpoint
  `log`, `debugger` notes), errors. Filters (Results / Messages / Errors / Log) and Clear. The
  input at the bottom evaluates an expression in the paused context: Enter evaluates, Shift+Enter
  adds a line, Up/Down walk the history. The Console keeps the last 2 000 entries.

**Inline values**: after each step the editor shows, in grey italics at the end of the
statement's last line, `3 rows affected`, `1 row: id=5, ...`, `42 rows`, a shortened error, or
the new value of a variable (`@x = 5` for MySQL user variables and T-SQL shim variables). Hovering
a name that matches a variable shows its full value. Turn them off with "Show values inline".

**Evaluate Expression** (dialog): prefilled with the selection or the word at the cursor;
Ctrl/Cmd+Enter evaluates; "Add to Watches"; recent expressions. Inside a routine frame a bare
variable name is read from the routine; any other expression is evaluated on the hidden proxy
connection with the routine variables substituted as typed literals (that connection does not see
the uncommitted data or temporary tables of the debugged session).

## Transactions

| Option | Values | Default |
|---|---|---|
| Transaction | **Wrap** (the debugger opens a transaction before the first statement and owns it) / **Lease** (use the tab's mode: autocommit commits each statement, manual uses the tab's transaction) | Wrap |
| When the session ends | **Ask** (dialog Commit / Rollback / Keep open for the tab) / Always roll back / Always commit / Keep the transaction open | Ask, with **Rollback** as the default button |

- Wrap applies only when the lease is in autocommit, has no open transaction and the database has
  manual transactions. Otherwise the session runs as Lease. A transaction the tab already had is
  never committed or rolled back by the debugger.
- A transaction the **script itself** opened (`BEGIN`, Redis `MULTI`) is also ended with the
  session (rolled back, or committed when you chose Commit), unless you chose Keep.
- When the debug transaction disappears (a statement with an implicit commit such as MySQL or
  Oracle DDL, a `COMMIT` in the script, an error that rolled it back) the session **pauses** and
  the Console says that the rest of the script runs in autocommit. Statements known to commit
  implicitly get a Console warning before they run.
- Closing the tab always rolls back. When "Pause on error" is off and a statement fails, the
  session ends with the end action (Ask becomes Rollback).

**Lock protection**

- **Warning**: when the session is paused with a transaction open for `debugTxWarnSecs`
  (default 60 s), a banner says the transaction may be holding locks, with "Roll back and stop".
- **Paused TTL**: a session paused without any command for `debugPausedTtlSecs` (default **900 s**,
  range 60–14 400, no "off" value) ends: the running statement or routine is aborted and the
  debug transaction is rolled back. The status line counts down during the last two minutes.
- **PostgreSQL / YugabyteDB**: with Wrap the debugger sets `idle_in_transaction_session_timeout`
  to TTL + 60 s on the lease and restores the old value at the end, so the server drops the
  connection if the application dies.

These settings (`debugTxWarnSecs` 0 or 10–3 600, `debugPausedTtlSecs` 60–14 400,
`debugWatchTimeoutMs` = 5 000 ms (500–60 000), `debugMaxRowsPerStep` = 200 (10–5 000),
`debugKeymap`, `debugInlineValues`) live in the application settings;
the settings page has no fields for them yet, only the keymap and inline values are in the Debug
menus.

## Safety

- **Every step goes through the same guard as the editor**, with the connection's read-only and
  production flags and the database of the lease *when the step runs*. A step refused on a
  read-only connection pauses as "blocked" without running; Stop ends the session (Jump To, which
  would skip the statement, has no UI yet). A
  destructive statement on a production connection pauses for the usual typed confirmation; the
  one-time token is bound to the statement text and context.
- **Evaluate, watches, breakpoint conditions, `{expr}` in log messages and the Variables probes
  are read-only**: the expression must be exactly one read statement for the guard (checked as if
  the connection were read-only), and it runs inside a frame that is rolled back:

  | Database | No transaction | Transaction open |
  |---|---|---|
  | PostgreSQL, YugabyteDB | `BEGIN READ ONLY` … `ROLLBACK` | `SAVEPOINT __dbg; SET LOCAL transaction_read_only = on` … `ROLLBACK TO SAVEPOINT` |
  | CockroachDB | `BEGIN READ ONLY` … `ROLLBACK` | `SAVEPOINT __dbg` … `ROLLBACK TO SAVEPOINT` |
  | MySQL, MariaDB | `START TRANSACTION READ ONLY` … `ROLLBACK` | `SAVEPOINT __dbg` … `ROLLBACK TO SAVEPOINT` |
  | TiDB | `BEGIN` … `ROLLBACK` | savepoint |
  | SQL Server | `BEGIN TRAN` … `ROLLBACK` | `SAVE TRAN __dbg` … `ROLLBACK TRAN __dbg` when `XACT_STATE() = 1` |
  | Oracle | `SET TRANSACTION READ ONLY` … `ROLLBACK` | `SAVEPOINT dbg` … `ROLLBACK TO dbg` |
  | MongoDB, Redis, Cassandra/ScyllaDB, ClickHouse | guard only (cannot be undone) | — |

  A failed transaction (PostgreSQL aborted, SQL Server `XACT_STATE() = -1`) or an unknown
  transaction state means nothing is evaluated. On PostgreSQL and MySQL, functions with side
  effects a read-only transaction does not stop are refused outright (`nextval`, `setval`,
  `set_config`, advisory locks, `pg_notify`, `dblink*`, large-object functions, `GET_LOCK`,
  `LOAD_FILE`, `@x := ...`, ...). Each evaluation has a timeout (`debugWatchTimeoutMs`) and keeps
  50 rows.
- Each step is recorded in the query history (redacted, without parameter values); evaluations
  are not.
- The routine debugger only ever sets breakpoints restricted to the backend pid of the debugged
  connection, so other sessions calling the same routine are never stopped.

## Support by database

| Database | Script debugging | Routine debugging (Step Into) | Notes |
|---|---|---|---|
| PostgreSQL | yes | **yes**, PL/pgSQL through `pldebugger` (see below) | `DO` is one step |
| YugabyteDB (YSQL) | yes | same code path as PostgreSQL when the server has `pldbgapi`; not tested | |
| CockroachDB | yes | no (no debugger API) | |
| MySQL, MariaDB | yes | no (no server API) | user variables `@x` editable; `ROW_COUNT()` kept (below) |
| TiDB | yes | no | read-only frame is `BEGIN … ROLLBACK` |
| SQL Server | yes (T-SQL variable shim) | no | `PRINT` / `RAISERROR` go to the Console |
| Oracle | yes | no (a `DBMS_DEBUG` spike is planned behind the cargo feature `oracle-debug-spike` of `dbstudio-debug`; it contains no debugger yet) | `DBMS_OUTPUT` goes to the Console |
| MongoDB | yes | no | no script variables |
| Redis / Valkey | engine only; the Redis tab is a console, so the Debug button is not shown | no | `MULTI` tracking |
| Cassandra / ScyllaDB | yes | no | no transactions: nothing can be undone |
| ClickHouse | yes | no | no transactions |

On servers without a routine debugger, Step Into / Force Step Into / Smart Step Into are greyed
out; Step Over is the way forward.

Dialect specific behaviour:

- **SQL Server (T-SQL)**. Variables live inside one batch, so stepping statement by statement uses
  a *shim*: each step re-declares the scalar variables declared before it, restores their values,
  runs the statement and reads the values back (that read-out is hidden). A batch that the shim
  cannot model runs as **one step**: table variables, cursors, `WHILE` / `TRY…CATCH` / `GOTO` /
  `RETURN` / `WAITFOR`, `@@ROWCOUNT`, `@@ERROR`, `@@IDENTITY`, `@@FETCH_STATUS`,
  `SCOPE_IDENTITY()`, or a statement that must start a batch (`CREATE PROCEDURE/VIEW/TRIGGER/
  FUNCTION`, ...). `GO n` repeats the steps of the batch n times (hit counts are meaningful there).
  The start option `tsqlStepUnit: batch` (not in the menus yet) steps one batch at a time without
  the shim.
- **MySQL / MariaDB**. Evaluations reset `ROW_COUNT()`, `FOUND_ROWS()` and the warning list. When
  the **next** statement reads `ROW_COUNT()`, `FOUND_ROWS()`, `LAST_INSERT_ID()`,
  `@@warning_count`, `@@error_count`, `SHOW WARNINGS/ERRORS` or `GET DIAGNOSTICS`, watches and
  probes are skipped at that pause ("keeping ROW_COUNT() for the next statement"). A breakpoint
  condition on that statement still runs, with a Console warning. A statement containing `;`
  (stored program body) is sent with a private `DELIMITER`.
- **Redis**. Each line is a step. Between `MULTI` and `EXEC`/`DISCARD` commands are queued, so
  watches, conditions and probes are skipped. A `MULTI` left open is discarded when the session
  ends (unless Keep).
- **Implicit commits**. MySQL and Oracle DDL commit the debug transaction: the Console warns
  before the statement and the session pauses after it, since the rest runs in autocommit.
- **Oracle**. `SET SERVEROUTPUT` is still sent (it enables `DBMS_OUTPUT`); other SQL\*Plus
  commands are skipped.

## Default keyboard shortcuts

Shortcuts work while the tab has a debug session, except Debug (only without a session) and
Toggle / View Breakpoints (always). The keymap (JetBrains or VS Code) is chosen in the Debug
menus. On macOS the F keys may need `fn` unless "Use F1, F2, etc. keys as standard function keys"
is on.

| Action | JetBrains (Windows/Linux) | JetBrains (macOS) | VS Code |
|---|---|---|---|
| Debug (start) | Shift+F9 | ⌃D | F5 |
| Resume | F9 | ⌥⌘R | F5 |
| Pause | — | — | F6 |
| Stop | Ctrl+F2 | ⌘F2 | Shift+F5 |
| Rerun | Ctrl+F5 | ⌘R | Ctrl+Shift+F5 (macOS ⇧⌘F5) |
| Step Over | F8 | F8 | F10 |
| Step Into | F7 | F7 | F11 |
| Smart Step Into | Shift+F7 | ⇧F7 | — |
| Force Step Into | Alt+Shift+F7 | ⌥⇧F7 | — |
| Step Out | Shift+F8 | ⇧F8 | Shift+F11 |
| Run to Cursor | Alt+F9 | ⌥F9 | — |
| Force Run to Cursor | Ctrl+Alt+F9 | ⌥⌘F9 | — |
| Evaluate Expression | Alt+F8 | ⌥F8 | — |
| Show Execution Point | Alt+F10 | ⌥F10 | — |
| Toggle Line Breakpoint | Ctrl+F8 | ⌘F8 | F9 |
| View Breakpoints | Ctrl+Shift+F8 | ⇧⌘F8 | — |
| Mute Breakpoints | — (toolbar) | — | — |

## Setting up routine debugging on PostgreSQL

Routine debugging needs, on the server:

1. The **pldebugger** package. On Debian/Ubuntu with the PGDG repository (apt.postgresql.org):
   `apt-get install postgresql-<major>-pldebugger` (for example `postgresql-16-pldebugger`).
   Elsewhere use your distribution's pldebugger package or build it from source.
2. `plugin_debugger` in **`shared_preload_libraries`** (postgresql.conf, appended to what is
   already there), then a **server restart**:

   ```
   shared_preload_libraries = 'plugin_debugger'
   ```

3. The **`pldbgapi` extension** in every database you debug: `CREATE EXTENSION pldbgapi;`.
4. A **superuser** role for the debugging connection: `pldbg_set_global_breakpoint` refuses other
   roles (`must be a superuser to create a breakpoint`). The role must also be able to execute the
   `pldbg_*` functions.

Managed PostgreSQL services often do not allow loading `plugin_debugger`; script debugging still
works there.

DBStudio checks this when a session starts and in "Routine debugging status..." of the Debug menu:

| Status | Meaning / what to do |
|---|---|
| Available | Step Into works. |
| Needs extension | The package is installed but the extension is not created in this database. The dialog shows `CREATE EXTENSION pldbgapi;` with "Insert statement into editor" (DBStudio never runs it for you). |
| Needs server config | Add `plugin_debugger` to `shared_preload_libraries` and restart the server. |
| No permission | The role cannot execute the `pldbg_*` functions, or is not a superuser. |
| Unsupported | The package is not installed (or the server is CockroachDB). |

The test image does exactly this ([docker/pg16dbg](../docker/pg16dbg)): `FROM postgres:16`,
`apt-get install postgresql-16-pldebugger`, start with
`postgres -c shared_preload_libraries=plugin_debugger`, and an init script running
`CREATE EXTENSION IF NOT EXISTS pldbgapi;`:

```sh
docker compose -f docker/compose.test.yml --profile debug up -d --wait pg16dbg
# 127.0.0.1:54317, dbstudio / dbstudio_test, database dbstudio (superuser)
```

How it works: the tab's connection (the *target*) runs the statement; a second, hidden connection
of the same session (the *proxy*, same TLS/SSH) drives the `pldbg_*` functions. A global
breakpoint restricted to the target's backend pid makes the target stop and attach to the proxy's
listener; the proxy is closed after every statement, which removes any breakpoint left behind. If
DBStudio dies while a routine is stopped, the target simply runs on.

## Known limits

- Routine breakpoints only take effect after Step Into, and ignore condition, hit count, log
  message, suspend and Mute (see above).
- Inside a routine: no Run to Cursor and no Pause.
- Inline values are shown for script statements only, not in routine sources.
- There is no UI for Jump To (set next statement), for opening a routine source before a session
  ("Open routine..."), or for the debug settings in the settings page.
- Force Step Into does not turn a PostgreSQL `DO` block into a function: `DO` stays one step.
- The debugger has been verified with unit tests, integration tests (PostgreSQL 16 with
  pldebugger, PostgreSQL 16 and MySQL 8 for script debugging) and the UI with mock IPC; not yet in
  the real Tauri window, nor against the other databases' servers.
