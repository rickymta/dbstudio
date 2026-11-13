//! Native tools (`pg_dump`, `mysqldump`, ...): detection, argument building and execution.
//!
//! Secrets never appear on a command line: PostgreSQL gets `PGPASSWORD`, MySQL a temporary
//! `--defaults-extra-file` (0600, removed afterwards), MongoDB a temporary `--config` YAML,
//! Redis `REDISCLI_AUTH`. Arguments are built from the spec only; the `options` map accepts
//! an allow-list of keys with validated values.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use dbstudio_core::{
    CancelToken, ConnectionConfig, DbKind, Error, JobReporter, Result, Secret, TlsMode,
};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::{NativeTool, NativeToolInfo, ToolSource};

/// Tail of stderr kept for the error message and the run log.
const LOG_TAIL: usize = 16 * 1024;

// ---------------------------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------------------------

fn exe_names(binary: &str) -> Vec<String> {
    if cfg!(windows) {
        vec![format!("{binary}.exe"), binary.to_string()]
    } else {
        vec![binary.to_string()]
    }
}

fn is_executable(p: &Path) -> bool {
    let Ok(meta) = fs::metadata(p) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn find_in(dir: &Path, binary: &str) -> Option<PathBuf> {
    exe_names(binary)
        .into_iter()
        .map(|n| dir.join(n))
        .find(|p| is_executable(p))
}

/// Directories searched after `PATH`. Patterns end in `*` for "every child directory".
fn known_dirs() -> Vec<PathBuf> {
    let mut patterns: Vec<String> = vec![
        "/opt/homebrew/opt/libpq/bin".into(),
        "/usr/local/opt/libpq/bin".into(),
        "/opt/homebrew/bin".into(),
        "/usr/local/bin".into(),
        "/Applications/Postgres.app/Contents/Versions/*/bin".into(),
        "/usr/lib/postgresql/*/bin".into(),
        "C:\\Program Files\\PostgreSQL\\*\\bin".into(),
        "C:\\Program Files\\MySQL\\*\\bin".into(),
        "C:\\Program Files\\MongoDB\\Tools\\*\\bin".into(),
    ];
    if let Ok(home) = std::env::var("HOME") {
        patterns.push(format!("{home}/.local/bin"));
    }
    let mut out = Vec::new();
    for p in patterns {
        match p.split_once('*') {
            None => out.push(PathBuf::from(p)),
            Some((head, tail)) => {
                if let Ok(rd) = fs::read_dir(head) {
                    let mut children: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
                    // Newest version first.
                    children.sort_by(|a, b| b.cmp(a));
                    for c in children {
                        out.push(PathBuf::from(format!("{}{tail}", c.display())));
                    }
                }
            }
        }
    }
    out
}

fn probe(path: &Path, tool: NativeTool) -> (Option<String>, Option<String>) {
    if matches!(tool, NativeTool::Expdp | NativeTool::Impdp) {
        // Data Pump has no `--version`.
        return (None, None);
    }
    let out = std::process::Command::new(path)
        .arg("--version")
        .stdin(Stdio::null())
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout);
            let first = text.lines().next().unwrap_or("").trim().to_string();
            (Some(first).filter(|v| !v.is_empty()), None)
        }
        Ok(o) => (None, Some(format!("`--version` failed with {}", o.status))),
        Err(e) => (None, Some(format!("cannot run {}: {e}", path.display()))),
    }
}

/// Probe the native tools (configured path, then `PATH`, then well-known directories).
pub fn detect_tools(overrides: &BTreeMap<NativeTool, String>) -> Vec<NativeToolInfo> {
    let path_dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    detect_in(overrides, &path_dirs, &known_dirs())
}

/// [`detect_tools`] with explicit search directories (tests use empty ones).
pub fn detect_in(
    overrides: &BTreeMap<NativeTool, String>,
    path_dirs: &[PathBuf],
    known: &[PathBuf],
) -> Vec<NativeToolInfo> {
    NativeTool::ALL
        .into_iter()
        .filter_map(|tool| {
            let binary = tool.binary()?;
            let mut found: Option<(PathBuf, ToolSource)> = None;
            let mut problem = None;
            if let Some(configured) = overrides.get(&tool) {
                let p = PathBuf::from(configured);
                if is_executable(&p) {
                    found = Some((p, ToolSource::Configured));
                } else {
                    problem = Some(format!("configured path {configured} is not an executable"));
                }
            }
            if found.is_none() {
                found = path_dirs
                    .iter()
                    .find_map(|d| find_in(d, binary))
                    .map(|p| (p, ToolSource::Path))
                    .or_else(|| {
                        known
                            .iter()
                            .find_map(|d| find_in(d, binary))
                            .map(|p| (p, ToolSource::KnownDir))
                    });
            }
            Some(match found {
                Some((path, source)) => {
                    let (version, probe_problem) = probe(&path, tool);
                    NativeToolInfo {
                        tool,
                        path: Some(path.to_string_lossy().into_owned()),
                        version,
                        source,
                        problem: probe_problem,
                    }
                }
                None => NativeToolInfo {
                    tool,
                    path: None,
                    version: None,
                    source: ToolSource::Path,
                    problem: Some(problem.unwrap_or_else(|| {
                        format!("{binary} not found in PATH or the usual install directories")
                    })),
                },
            })
        })
        .collect()
}

/// Path of a usable tool, or an error carrying the `problem`.
pub fn require_tool(
    tool: NativeTool,
    overrides: &BTreeMap<NativeTool, String>,
) -> Result<NativeToolInfo> {
    require_info(tool, detect_tools(overrides))
}

/// Pick `tool` from probe results; a missing or broken tool becomes an error with the problem.
pub fn require_info(tool: NativeTool, infos: Vec<NativeToolInfo>) -> Result<NativeToolInfo> {
    let info = infos
        .into_iter()
        .find(|i| i.tool == tool)
        .ok_or_else(|| Error::Unsupported(format!("native tool {tool:?} has no executable")))?;
    match (&info.path, &info.problem) {
        (Some(_), None) => Ok(info),
        (_, Some(p)) => Err(Error::Other(format!(
            "{}: {p}",
            tool.binary().unwrap_or("tool")
        ))),
        (None, None) => Err(Error::Other("tool not found".into())),
    }
}

/// Major version in `"pg_dump (PostgreSQL) 16.4"` style output.
pub fn major_version(text: &str) -> Option<u32> {
    text.split_whitespace()
        .find(|w| w.starts_with(|c: char| c.is_ascii_digit()))
        .and_then(|w| w.split('.').next())
        .and_then(|w| w.parse().ok())
}

/// `pg_dump` older than the server cannot dump it.
pub fn check_pg_version(info: &NativeToolInfo, server_version: &str) -> Result<()> {
    let tool = info.version.as_deref().and_then(major_version);
    let server = major_version(server_version);
    if let (Some(t), Some(s)) = (tool, server) {
        if t < s {
            return Err(Error::Other(format!(
                "{} is version {t} but the server is version {s}; install a newer client",
                info.tool.binary().unwrap_or("tool")
            )));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Invocation
// ---------------------------------------------------------------------------------------------

/// One tool run.
#[derive(Debug)]
pub struct ToolRun {
    /// Executable.
    pub program: PathBuf,
    /// Arguments (never contain secrets).
    pub args: Vec<OsString>,
    /// Environment (may contain secrets).
    pub env: Vec<(String, String)>,
    /// File fed to stdin.
    pub stdin: Option<PathBuf>,
    /// File receiving stdout.
    pub stdout: Option<PathBuf>,
}

/// Temporary file removed on drop.
#[derive(Debug)]
pub struct TempSecretFile(PathBuf);

impl TempSecretFile {
    /// Write `content` to a new 0600 file in `dir`.
    pub fn create(dir: &Path, content: &str) -> Result<Self> {
        let path = dir.join(format!("dbstudio-{}.tmp", uuid::Uuid::new_v4().simple()));
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&path)?;
        f.write_all(content.as_bytes())?;
        Ok(TempSecretFile(path))
    }

    /// Location.
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempSecretFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Run `run`, streaming stderr lines to the reporter. Cancelling kills the process.
pub async fn run_tool(run: ToolRun, reporter: &dyn JobReporter) -> Result<String> {
    let name = run
        .program
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut cmd = Command::new(&run.program);
    cmd.args(&run.args)
        .envs(run.env.iter().map(|(k, v)| (k, v)))
        .kill_on_drop(true)
        .stderr(Stdio::piped());
    cmd.stdin(match &run.stdin {
        Some(p) => Stdio::from(File::open(p)?),
        None => Stdio::null(),
    });
    cmd.stdout(match &run.stdout {
        Some(p) => Stdio::from(File::create(p)?),
        None => Stdio::null(),
    });
    let mut child = cmd
        .spawn()
        .map_err(|e| Error::Other(format!("cannot start {name}: {e}")))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| Error::Other("no stderr".into()))?;
    let cancel: CancelToken = reporter.cancel_token().clone();
    let mut tail = String::new();
    let mut lines = BufReader::new(stderr).lines();
    let status = {
        let reading = async {
            while let Ok(Some(line)) = lines.next_line().await {
                reporter.log(&line);
                tail.push_str(&line);
                tail.push('\n');
                if tail.len() > LOG_TAIL {
                    let cut = tail.len() - LOG_TAIL;
                    let cut = (cut..tail.len())
                        .find(|i| tail.is_char_boundary(*i))
                        .unwrap_or(0);
                    tail.drain(..cut);
                }
            }
            child.wait().await
        };
        tokio::select! {
            r = reading => r.map_err(|e| Error::Other(format!("{name}: {e}")))?,
            () = cancel.cancelled() => {
                return Err(Error::Cancelled);
            }
        }
    };
    if !status.success() {
        let detail = tail.trim();
        return Err(Error::Other(format!(
            "{name} failed ({status}){}{detail}",
            if detail.is_empty() { "" } else { ": " }
        )));
    }
    Ok(tail)
}

// ---------------------------------------------------------------------------------------------
// Per-product argument builders
// ---------------------------------------------------------------------------------------------

/// Host and port a tool connects to (the SSH tunnel endpoint when there is one).
fn endpoint(config: &ConnectionConfig, tunnel: Option<std::net::SocketAddr>) -> (String, u16) {
    match tunnel {
        Some(a) => (a.ip().to_string(), a.port()),
        None => (config.host.clone(), config.port),
    }
}

fn truthy(options: &BTreeMap<String, String>, key: &str) -> bool {
    options.get(key).is_some_and(|v| v == "true")
}

/// Reject option keys and values outside the allow-list.
pub fn validate_options(tool: NativeTool, options: &BTreeMap<String, String>) -> Result<()> {
    let (allowed, choices): (&[&str], &[(&str, &[&str])]) = match tool {
        NativeTool::PgDump => (
            &[
                "schemaOnly",
                "dataOnly",
                "noOwner",
                "noPrivileges",
                "format",
            ],
            &[("format", &["custom", "plain"])],
        ),
        NativeTool::MySqlDump => (
            &[
                "schemaOnly",
                "dataOnly",
                "routines",
                "triggers",
                "events",
                "singleTransaction",
            ],
            &[],
        ),
        NativeTool::MongoDump => (&["gzip"], &[]),
        _ => (&[], &[]),
    };
    for (k, v) in options {
        if !allowed.contains(&k.as_str()) {
            return Err(Error::Other(format!(
                "option {k:?} is not allowed for {tool:?}"
            )));
        }
        match choices.iter().find(|(key, _)| key == k) {
            Some((_, values)) if !values.contains(&v.as_str()) => {
                return Err(Error::Other(format!(
                    "option {k} must be one of {values:?}"
                )));
            }
            None if v != "true" && v != "false" => {
                return Err(Error::Other(format!("option {k} must be true or false")));
            }
            _ => {}
        }
    }
    Ok(())
}

fn pg_env(config: &ConnectionConfig, secret: Option<&Secret>) -> Vec<(String, String)> {
    let mut env = Vec::new();
    if let Some(s) = secret {
        env.push(("PGPASSWORD".to_string(), s.expose().to_string()));
    }
    let mode = match config.tls.mode {
        TlsMode::Disable => "disable",
        TlsMode::Prefer => "prefer",
        TlsMode::Require => "require",
        TlsMode::VerifyCa => "verify-ca",
        TlsMode::VerifyFull => "verify-full",
    };
    env.push(("PGSSLMODE".to_string(), mode.to_string()));
    if let Some(ca) = &config.tls.ca_cert_path {
        env.push(("PGSSLROOTCERT".to_string(), ca.clone()));
    }
    if let Some(c) = &config.tls.client_cert_path {
        env.push(("PGSSLCERT".to_string(), c.clone()));
    }
    if let Some(k) = &config.tls.client_key_path {
        env.push(("PGSSLKEY".to_string(), k.clone()));
    }
    env
}

fn pg_conn_args(
    config: &ConnectionConfig,
    tunnel: Option<std::net::SocketAddr>,
    database: &str,
) -> Vec<OsString> {
    let (host, port) = endpoint(config, tunnel);
    let mut a: Vec<OsString> = vec![
        "-h".into(),
        host.into(),
        "-p".into(),
        port.to_string().into(),
    ];
    if let Some(u) = &config.username {
        a.push("-U".into());
        a.push(u.into());
    }
    a.push("-w".into());
    a.push("-d".into());
    a.push(database.into());
    a
}

fn mysql_ssl_arg(config: &ConnectionConfig) -> Option<OsString> {
    let mode = match config.tls.mode {
        TlsMode::Disable => "DISABLED",
        TlsMode::Prefer => return None,
        TlsMode::Require => "REQUIRED",
        TlsMode::VerifyCa => "VERIFY_CA",
        TlsMode::VerifyFull => "VERIFY_IDENTITY",
    };
    Some(format!("--ssl-mode={mode}").into())
}

fn mysql_conn_args(
    defaults: &Path,
    config: &ConnectionConfig,
    tunnel: Option<std::net::SocketAddr>,
) -> Vec<OsString> {
    let (host, port) = endpoint(config, tunnel);
    // `--defaults-extra-file` must come first.
    let mut a: Vec<OsString> = vec![
        format!("--defaults-extra-file={}", defaults.display()).into(),
        "-h".into(),
        host.into(),
        "-P".into(),
        port.to_string().into(),
    ];
    if let Some(u) = &config.username {
        a.push("-u".into());
        a.push(u.into());
    }
    if let Some(ca) = &config.tls.ca_cert_path {
        a.push(format!("--ssl-ca={ca}").into());
    }
    a.extend(mysql_ssl_arg(config));
    a
}

fn mysql_defaults_content(secret: Option<&Secret>) -> String {
    let pw = secret.map_or(String::new(), |s| {
        format!(
            "password=\"{}\"\n",
            s.expose().replace('\\', "\\\\").replace('"', "\\\"")
        )
    });
    format!("[client]\n{pw}")
}

/// Everything needed to run a native dump of one database.
#[derive(Debug)]
pub struct DumpCommand {
    /// The run.
    pub run: ToolRun,
    /// Secret files kept alive while the tool runs.
    pub guards: Vec<TempSecretFile>,
    /// Extension of the dump inside the container.
    pub extension: &'static str,
    /// Format label stored in the preamble.
    pub format: &'static str,
}

/// Build the dump command for `tool`.
#[allow(clippy::too_many_arguments)]
pub fn build_dump(
    tool: NativeTool,
    info: &NativeToolInfo,
    config: &ConnectionConfig,
    secret: Option<&Secret>,
    tunnel: Option<std::net::SocketAddr>,
    database: &str,
    options: &BTreeMap<String, String>,
    tmp: &Path,
    out: &Path,
) -> Result<DumpCommand> {
    validate_options(tool, options)?;
    let program = PathBuf::from(info.path.clone().unwrap_or_default());
    match (tool, config.kind) {
        (NativeTool::PgDump, DbKind::Postgres) => {
            let plain = options.get("format").is_some_and(|f| f == "plain");
            let mut args = pg_conn_args(config, tunnel, database);
            args.push(format!("--format={}", if plain { "plain" } else { "custom" }).into());
            for (key, flag) in [
                ("schemaOnly", "--schema-only"),
                ("dataOnly", "--data-only"),
                ("noOwner", "--no-owner"),
                ("noPrivileges", "--no-privileges"),
            ] {
                if truthy(options, key) {
                    args.push(flag.into());
                }
            }
            Ok(DumpCommand {
                run: ToolRun {
                    program,
                    args,
                    env: pg_env(config, secret),
                    stdin: None,
                    stdout: Some(out.to_path_buf()),
                },
                guards: vec![],
                extension: if plain { "sql" } else { "pgdump" },
                format: if plain { "plain" } else { "custom" },
            })
        }
        (NativeTool::MySqlDump, DbKind::MySql) => {
            let defaults = TempSecretFile::create(tmp, &mysql_defaults_content(secret))?;
            let mut args = mysql_conn_args(defaults.path(), config, tunnel);
            if options.get("singleTransaction").is_none_or(|v| v == "true") {
                args.push("--single-transaction".into());
            }
            for (key, flag) in [
                ("schemaOnly", "--no-data"),
                ("dataOnly", "--no-create-info"),
                ("routines", "--routines"),
                ("triggers", "--triggers"),
                ("events", "--events"),
            ] {
                if truthy(options, key) {
                    args.push(flag.into());
                }
            }
            args.push("--default-character-set=utf8mb4".into());
            args.push(database.into());
            Ok(DumpCommand {
                run: ToolRun {
                    program,
                    args,
                    env: vec![],
                    stdin: None,
                    stdout: Some(out.to_path_buf()),
                },
                guards: vec![defaults],
                extension: "sql",
                format: "plain",
            })
        }
        (NativeTool::MongoDump, DbKind::MongoDb) => {
            let (host, port) = endpoint(config, tunnel);
            let yaml = format!(
                "password: \"{}\"\n",
                secret.map_or(String::new(), |s| s
                    .expose()
                    .replace('\\', "\\\\")
                    .replace('"', "\\\""))
            );
            let cfg = TempSecretFile::create(tmp, &yaml)?;
            let mut args: Vec<OsString> = vec![
                "--host".into(),
                host.into(),
                "--port".into(),
                port.to_string().into(),
                format!("--config={}", cfg.path().display()).into(),
                "--db".into(),
                database.into(),
                "--archive".into(),
            ];
            if let Some(u) = &config.username {
                args.push("--username".into());
                args.push(u.into());
                args.push("--authenticationDatabase".into());
                args.push(
                    config
                        .options
                        .get("authSource")
                        .cloned()
                        .unwrap_or_else(|| "admin".into())
                        .into(),
                );
            }
            if truthy(options, "gzip") {
                args.push("--gzip".into());
            }
            Ok(DumpCommand {
                run: ToolRun {
                    program,
                    args,
                    env: vec![],
                    stdin: None,
                    stdout: Some(out.to_path_buf()),
                },
                guards: vec![cfg],
                extension: "archive",
                format: "archive",
            })
        }
        (NativeTool::RedisCli, DbKind::Redis) => {
            let (host, port) = endpoint(config, tunnel);
            let mut args: Vec<OsString> = vec![
                "-h".into(),
                host.into(),
                "-p".into(),
                port.to_string().into(),
            ];
            if let Some(u) = &config.username {
                args.push("--user".into());
                args.push(u.into());
            }
            args.push("--rdb".into());
            args.push(out.into());
            let env = secret
                .map(|s| vec![("REDISCLI_AUTH".to_string(), s.expose().to_string())])
                .unwrap_or_default();
            Ok(DumpCommand {
                run: ToolRun {
                    program,
                    args,
                    env,
                    stdin: None,
                    stdout: None,
                },
                guards: vec![],
                extension: "rdb",
                format: "rdb",
            })
        }
        (t, k) => Err(Error::Unsupported(format!(
            "{t:?} cannot back up a {} connection",
            k.display_name()
        ))),
    }
}

/// Restore command for a native dump (`format` is the preamble's label).
#[allow(clippy::too_many_arguments)]
pub fn build_restore(
    tool: NativeTool,
    format: &str,
    info: &NativeToolInfo,
    config: &ConnectionConfig,
    secret: Option<&Secret>,
    tunnel: Option<std::net::SocketAddr>,
    database: &str,
    clean: bool,
    continue_on_error: bool,
    tmp: &Path,
    input: &Path,
) -> Result<DumpCommand> {
    let program = PathBuf::from(info.path.clone().unwrap_or_default());
    match (tool, config.kind, format) {
        (NativeTool::PgRestore, DbKind::Postgres, "custom") => {
            let mut args = pg_conn_args(config, tunnel, database);
            args.push("--no-owner".into());
            if clean {
                args.push("--clean".into());
                args.push("--if-exists".into());
            }
            if !continue_on_error {
                args.push("--exit-on-error".into());
            }
            Ok(DumpCommand {
                run: ToolRun {
                    program,
                    args,
                    env: pg_env(config, secret),
                    stdin: Some(input.to_path_buf()),
                    stdout: None,
                },
                guards: vec![],
                extension: "",
                format: "custom",
            })
        }
        (NativeTool::Psql, DbKind::Postgres, "plain") => {
            let mut args = pg_conn_args(config, tunnel, database);
            args.extend(["-X", "-q"].map(OsString::from));
            if !continue_on_error {
                args.extend(["-v", "ON_ERROR_STOP=1"].map(OsString::from));
            }
            Ok(DumpCommand {
                run: ToolRun {
                    program,
                    args,
                    env: pg_env(config, secret),
                    stdin: Some(input.to_path_buf()),
                    stdout: None,
                },
                guards: vec![],
                extension: "",
                format: "plain",
            })
        }
        (NativeTool::MySqlClient, DbKind::MySql, "plain") => {
            let defaults = TempSecretFile::create(tmp, &mysql_defaults_content(secret))?;
            let mut args = mysql_conn_args(defaults.path(), config, tunnel);
            if continue_on_error {
                args.push("--force".into());
            }
            args.push("--default-character-set=utf8mb4".into());
            args.push(database.into());
            Ok(DumpCommand {
                run: ToolRun {
                    program,
                    args,
                    env: vec![],
                    stdin: Some(input.to_path_buf()),
                    stdout: None,
                },
                guards: vec![defaults],
                extension: "",
                format: "plain",
            })
        }
        (NativeTool::MongoRestore, DbKind::MongoDb, "archive") => {
            let (host, port) = endpoint(config, tunnel);
            let yaml = format!(
                "password: \"{}\"\n",
                secret.map_or(String::new(), |s| s
                    .expose()
                    .replace('\\', "\\\\")
                    .replace('"', "\\\""))
            );
            let cfg = TempSecretFile::create(tmp, &yaml)?;
            let mut args: Vec<OsString> = vec![
                "--host".into(),
                host.into(),
                "--port".into(),
                port.to_string().into(),
                format!("--config={}", cfg.path().display()).into(),
                "--archive".into(),
                "--nsInclude".into(),
                "*.*".into(),
            ];
            if let Some(u) = &config.username {
                args.extend(["--username".into(), u.into()]);
                args.extend([
                    "--authenticationDatabase".into(),
                    config
                        .options
                        .get("authSource")
                        .cloned()
                        .unwrap_or_else(|| "admin".into())
                        .into(),
                ]);
            }
            if clean {
                args.push("--drop".into());
            }
            let _ = database;
            Ok(DumpCommand {
                run: ToolRun {
                    program,
                    args,
                    env: vec![],
                    stdin: Some(input.to_path_buf()),
                    stdout: None,
                },
                guards: vec![cfg],
                extension: "",
                format: "archive",
            })
        }
        (t, k, f) => Err(Error::Unsupported(format!(
            "{t:?} cannot restore a {f} dump into a {} connection",
            k.display_name()
        ))),
    }
}

/// Restore tool matching a dump made by `tool` in `format`.
pub fn restore_tool_for(tool: NativeTool, format: &str) -> Result<NativeTool> {
    match (tool, format) {
        (NativeTool::PgDump, "custom") => Ok(NativeTool::PgRestore),
        (NativeTool::PgDump, "plain") => Ok(NativeTool::Psql),
        (NativeTool::MySqlDump, _) => Ok(NativeTool::MySqlClient),
        (NativeTool::MongoDump, _) => Ok(NativeTool::MongoRestore),
        (NativeTool::RedisCli, _) => Err(Error::Unsupported(
            "an RDB backup is restored by placing it in the Redis server's data directory".into(),
        )),
        (t, _) => Err(Error::Unsupported(format!("restore of {t:?} backups"))),
    }
}

/// Give a hung tool a moment to exit (used by tests).
#[allow(dead_code)]
pub(crate) const GRACE: Duration = Duration::from_secs(2);

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(kind: DbKind) -> ConnectionConfig {
        let mut c = ConnectionConfig::new(kind, "c", "db.example");
        c.username = Some("bob".into());
        c
    }

    fn info(tool: NativeTool) -> NativeToolInfo {
        NativeToolInfo {
            tool,
            path: Some("/bin/true".into()),
            version: None,
            source: ToolSource::Path,
            problem: None,
        }
    }

    fn args_text(c: &DumpCommand) -> String {
        c.run
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn secrets_stay_off_the_command_line() {
        let tmp = tempfile::tempdir().unwrap();
        let secret = Secret::new("s3cr3t\"x");
        for tool in [NativeTool::PgDump, NativeTool::MySqlDump] {
            let kind = if tool == NativeTool::PgDump {
                DbKind::Postgres
            } else {
                DbKind::MySql
            };
            let c = build_dump(
                tool,
                &info(tool),
                &cfg(kind),
                Some(&secret),
                None,
                "app",
                &BTreeMap::new(),
                tmp.path(),
                Path::new("/o"),
            )
            .unwrap();
            assert!(!args_text(&c).contains("s3cr3t"), "{}", args_text(&c));
            if tool == NativeTool::PgDump {
                assert!(
                    c.run
                        .env
                        .iter()
                        .any(|(k, v)| k == "PGPASSWORD" && v == "s3cr3t\"x")
                );
                assert!(args_text(&c).contains("-h db.example"));
            } else {
                let file = c.guards[0].path().to_path_buf();
                let text = fs::read_to_string(&file).unwrap();
                assert!(text.contains("password=\"s3cr3t\\\"x\""));
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    assert_eq!(
                        fs::metadata(&file).unwrap().permissions().mode() & 0o777,
                        0o600
                    );
                }
                assert!(args_text(&c).starts_with("--defaults-extra-file="));
                drop(c);
                assert!(!file.exists(), "secret file removed on drop");
            }
        }
    }

    #[test]
    fn options_are_allow_listed() {
        let o = |k: &str, v: &str| BTreeMap::from([(k.to_string(), v.to_string())]);
        assert!(validate_options(NativeTool::PgDump, &o("format", "custom")).is_ok());
        assert!(validate_options(NativeTool::PgDump, &o("format", "tar; rm -rf")).is_err());
        assert!(validate_options(NativeTool::PgDump, &o("exec", "true")).is_err());
        assert!(validate_options(NativeTool::PgDump, &o("noOwner", "yes")).is_err());
        assert!(validate_options(NativeTool::MySqlDump, &o("routines", "true")).is_ok());
    }

    #[test]
    fn tunnel_endpoint_replaces_host() {
        let tmp = tempfile::tempdir().unwrap();
        let addr: std::net::SocketAddr = "127.0.0.1:5555".parse().unwrap();
        let c = build_dump(
            NativeTool::PgDump,
            &info(NativeTool::PgDump),
            &cfg(DbKind::Postgres),
            None,
            Some(addr),
            "app",
            &BTreeMap::new(),
            tmp.path(),
            Path::new("/o"),
        )
        .unwrap();
        assert!(args_text(&c).contains("-h 127.0.0.1 -p 5555"));
    }

    #[test]
    fn wrong_product_is_unsupported_and_versions_parse() {
        let tmp = tempfile::tempdir().unwrap();
        let e = build_dump(
            NativeTool::PgDump,
            &info(NativeTool::PgDump),
            &cfg(DbKind::MySql),
            None,
            None,
            "x",
            &BTreeMap::new(),
            tmp.path(),
            Path::new("/o"),
        );
        assert!(e.is_err());
        assert_eq!(major_version("pg_dump (PostgreSQL) 16.4"), Some(16));
        assert_eq!(major_version("mysqldump  Ver 8.4.0 for macos14"), Some(8));
        let mut i = info(NativeTool::PgDump);
        i.version = Some("pg_dump (PostgreSQL) 14.2".into());
        assert!(check_pg_version(&i, "16.1").is_err());
        assert!(check_pg_version(&i, "13.4").is_ok());
        assert_eq!(
            restore_tool_for(NativeTool::PgDump, "custom").unwrap(),
            NativeTool::PgRestore
        );
        assert!(restore_tool_for(NativeTool::RedisCli, "rdb").is_err());
    }

    #[test]
    fn no_tool_anywhere_is_a_visible_problem() {
        let infos = detect_in(&BTreeMap::new(), &[], &[]);
        assert_eq!(infos.len(), 10);
        for i in &infos {
            assert!(i.path.is_none());
            assert!(i.problem.as_deref().unwrap().contains("not found"), "{i:?}");
        }
        let e = require_info(NativeTool::PgDump, infos).unwrap_err();
        assert!(e.to_string().contains("pg_dump"), "{e}");
        // A configured path that is not executable is named in the problem.
        let ov = BTreeMap::from([(NativeTool::MySqlDump, "/no/such/mysqldump".to_string())]);
        let i = detect_in(&ov, &[], &[])
            .into_iter()
            .find(|i| i.tool == NativeTool::MySqlDump)
            .unwrap();
        assert!(i.problem.unwrap().contains("/no/such/mysqldump"));
    }

    #[test]
    fn missing_tool_reports_a_problem_and_configured_path_wins() {
        // A configured path that does not exist is reported, not silently ignored.
        let ov = BTreeMap::from([(NativeTool::PgDump, "/nonexistent/pg_dump".to_string())]);
        let infos = detect_tools(&ov);
        let pg = infos.iter().find(|i| i.tool == NativeTool::PgDump).unwrap();
        if pg.path.is_none() {
            assert!(
                pg.problem
                    .as_deref()
                    .unwrap()
                    .contains("/nonexistent/pg_dump")
            );
        }
        // An executable that does exist and is configured is used.
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("mysqldump");
        fs::write(&fake, "#!/bin/sh\necho 'mysqldump  Ver 9.9.9'\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
            let ov = BTreeMap::from([(NativeTool::MySqlDump, fake.to_string_lossy().into_owned())]);
            let i = detect_tools(&ov)
                .into_iter()
                .find(|i| i.tool == NativeTool::MySqlDump)
                .unwrap();
            assert_eq!(i.source, ToolSource::Configured);
            assert_eq!(i.version.as_deref(), Some("mysqldump  Ver 9.9.9"));
            assert!(i.problem.is_none());
        }
        // T-SQL backup has no binary and is not listed.
        assert!(
            infos
                .iter()
                .all(|i| i.tool != NativeTool::MsSqlServerBackup)
        );
    }
}
