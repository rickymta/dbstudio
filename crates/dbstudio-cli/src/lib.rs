//! Command line interface of DBStudio (backup, restore, verify, tools).
//!
//! Exit codes: [`EXIT_OK`] success, [`EXIT_ERROR`] failure, [`EXIT_USAGE`] bad arguments,
//! [`EXIT_LOCKED`] the job is already running (file lock held by the app or another CLI).
//!
//! The CLI shares the SQLite store, the keychain (service `dev.dbstudio.desktop`) and the
//! per-job lock files with the desktop app, so a job started by cron / Task Scheduler / launchd
//! never overlaps with the same job running inside the app.

use std::ffi::OsString;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use clap::{ArgGroup, Parser, Subcommand, ValueEnum};
use dbstudio_backup::runner::{
    RunEnv, RunMeta, RunOutput, execute_restore, explicit_target_required, record_run,
    restore_confirmation_text, restore_is_guarded, restore_names_its_target, row_to_job, run_job,
    tool_overrides,
};
use dbstudio_backup::{
    BackupRun, RestoreSpec, RunKind, RunStatus, RunTrigger, detect_tools, inspect, verify,
};
use dbstudio_core::{CancelToken, DriverRegistry, Error, JobProgress, JobReporter, Secret};
use dbstudio_store::{
    ConnectionService, KeyringSecretStore, MemorySecretStore, SecretStore, Store,
};
use uuid::Uuid;

/// Everything went fine.
pub const EXIT_OK: u8 = 0;
/// The operation failed.
pub const EXIT_ERROR: u8 = 1;
/// Wrong arguments.
pub const EXIT_USAGE: u8 = 2;
/// The job is already running (lock held).
pub const EXIT_LOCKED: u8 = 3;

/// Folder name of the application data directory.
const APP_DIR: &str = "dev.dbstudio.desktop";
/// File name of the SQLite database inside the data directory.
const DATABASE_FILE: &str = "dbstudio.sqlite";
/// Default environment variable holding the passphrase of an encrypted file.
const PASSPHRASE_ENV: &str = "DBSTUDIO_PASSPHRASE";

const AFTER_HELP: &str = "\
Scheduling without the app
  The desktop app runs schedules itself while it is open (it keeps running in the tray when
  its window is closed). To run a job when the app is not running, call this program from your
  operating system's scheduler; the job id is shown by `dbstudio-cli backup --list`.
  A job never runs twice at once: the app and the CLI share a lock file per job and the CLI
  exits with code 3 when the job is already running.

  cron (Linux/macOS), every night at 02:30:
    30 2 * * *  /usr/local/bin/dbstudio-cli backup --job <JOB-ID>

  launchd (macOS): a LaunchAgent plist with
    ProgramArguments = [ /usr/local/bin/dbstudio-cli, backup, --job, <JOB-ID> ]
    StartCalendarInterval = { Hour = 2, Minute = 30 }
  The first run asks for access to the keychain item of the connection: choose \"Always Allow\".

  Windows Task Scheduler:
    schtasks /Create /SC DAILY /ST 02:30 /TN \"DBStudio backup\" ^
      /TR \"\\\"C:\\Program Files\\DBStudio\\dbstudio-cli.exe\\\" backup --job <JOB-ID>\"

Exit codes: 0 ok, 1 failure, 2 bad arguments, 3 the job is already running.
Passwords and passphrases are read from the OS keychain; an encrypted file given to `restore`
or `verify` needs its passphrase in the environment variable named by --passphrase-env.";

/// Where secrets are read from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum SecretStoreKind {
    /// The operating system keychain.
    #[default]
    Keychain,
    /// An empty in-memory store. For tests only: nothing is ever found in it.
    Memory,
}

/// `dbstudio-cli`.
#[derive(Debug, Parser)]
#[command(
    name = "dbstudio-cli",
    version,
    about = "DBStudio command line",
    after_long_help = AFTER_HELP
)]
pub struct Cli {
    /// Application data directory (default: `<OS data dir>/dev.dbstudio.desktop`).
    #[arg(long, global = true, value_name = "DIR")]
    pub data_dir: Option<PathBuf>,

    /// Where passwords and passphrases come from. `memory` is for tests only.
    #[arg(long, global = true, value_enum, default_value_t = SecretStoreKind::Keychain, hide = true)]
    pub secret_store: SecretStoreKind,

    /// What to do.
    #[command(subcommand)]
    pub command: Command,
}

/// Subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run a saved backup job, or list the jobs.
    #[command(group(ArgGroup::new("what").required(true).args(["job", "list"])))]
    Backup {
        /// Id of the job to run.
        #[arg(long, value_name = "ID")]
        job: Option<Uuid>,
        /// List the saved jobs.
        #[arg(long)]
        list: bool,
    },
    /// Restore a backup file into a connection.
    Restore {
        /// Backup file.
        #[arg(long, value_name = "FILE")]
        file: PathBuf,
        /// Id of the target connection.
        #[arg(long, value_name = "ID")]
        connection: Uuid,
        /// Target database (default: same name as the source).
        #[arg(long, value_name = "NAME")]
        database: Option<String>,
        /// Create the target database first.
        #[arg(long)]
        create_database: bool,
        /// Drop the existing objects of the backup first (destructive).
        #[arg(long)]
        clean: bool,
        /// Keep going after a failed statement.
        #[arg(long)]
        continue_on_error: bool,
        /// Do not ask for confirmation.
        #[arg(long)]
        yes: bool,
        /// Database name typed to confirm a restore into a production connection.
        #[arg(long, value_name = "TEXT")]
        confirm_text: Option<String>,
        /// Environment variable holding the passphrase of an encrypted file.
        #[arg(long, value_name = "VAR", default_value = PASSPHRASE_ENV)]
        passphrase_env: String,
    },
    /// Check the integrity of a backup file.
    Verify {
        /// Backup file.
        #[arg(long, value_name = "FILE")]
        file: PathBuf,
        /// Environment variable holding the passphrase of an encrypted file.
        #[arg(long, value_name = "VAR", default_value = PASSPHRASE_ENV)]
        passphrase_env: String,
    },
    /// Show the native backup tools that were found.
    Tools,
}

/// Looks up a passphrase by the name of an environment variable.
pub type PassphraseSource = Box<dyn Fn(&str) -> Option<Secret> + Send + Sync>;

/// Services the commands run against (replaceable in tests).
pub struct CliDeps {
    /// Run environment: store, keychain, drivers and the lock directory.
    pub env: RunEnv,
    /// Source of the passphrase of encrypted files.
    pub passphrase: PassphraseSource,
    /// Answer to the interactive confirmation (`None` = ask on the terminal).
    pub assume_yes: Option<bool>,
}

impl std::fmt::Debug for CliDeps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CliDeps")
            .field("env", &self.env)
            .finish_non_exhaustive()
    }
}

/// Default data directory of the desktop app.
pub fn default_data_dir() -> Option<PathBuf> {
    let base = if cfg!(target_os = "macos") {
        PathBuf::from(std::env::var_os("HOME")?).join("Library/Application Support")
    } else if cfg!(windows) {
        PathBuf::from(std::env::var_os("APPDATA")?)
    } else {
        match std::env::var_os("XDG_DATA_HOME") {
            Some(x) if !x.is_empty() => PathBuf::from(x),
            _ => PathBuf::from(std::env::var_os("HOME")?).join(".local/share"),
        }
    };
    Some(base.join(APP_DIR))
}

fn build_registry() -> DriverRegistry {
    #[allow(unused_mut)]
    let mut registry = DriverRegistry::new();
    #[cfg(feature = "postgres")]
    registry.register(Arc::new(dbstudio_driver_postgres::PostgresDriver::new()));
    #[cfg(feature = "mysql")]
    registry.register(Arc::new(dbstudio_driver_mysql::MySqlDriver::new()));
    #[cfg(feature = "mssql")]
    dbstudio_driver_mssql::register(&mut registry);
    #[cfg(feature = "oracle")]
    dbstudio_driver_oracle::register(&mut registry);
    #[cfg(feature = "mongodb")]
    dbstudio_driver_mongodb::register(&mut registry);
    #[cfg(feature = "redis")]
    dbstudio_driver_redis::register(&mut registry);
    registry
}

fn open_deps(cli: &Cli) -> Result<CliDeps, Error> {
    let dir = cli
        .data_dir
        .clone()
        .or_else(default_data_dir)
        .ok_or_else(|| {
            Error::Other("cannot determine the data directory; use --data-dir".into())
        })?;
    let db = dir.join(DATABASE_FILE);
    if !db.exists() {
        return Err(Error::NotFound(format!(
            "no DBStudio database at {} (run the app once or pass --data-dir)",
            db.display()
        )));
    }
    let store = Store::open(&db)?;
    let secrets: Arc<dyn SecretStore> = match cli.secret_store {
        SecretStoreKind::Keychain => Arc::new(KeyringSecretStore::new()),
        SecretStoreKind::Memory => Arc::new(MemorySecretStore::new()),
    };
    let service = Arc::new(ConnectionService::new(store, secrets.clone()));
    let deps = CliDeps {
        env: RunEnv {
            service,
            secrets,
            registry: Arc::new(build_registry()),
            locks_dir: dir.join("locks"),
            work_dir: dir.clone(),
        },
        passphrase: Box::new(|var| std::env::var(var).ok().map(Secret::new)),
        assume_yes: None,
    };
    // What a crashed earlier run left behind (scratch directories, half-written files).
    dbstudio_backup::runner::cleanup_orphans(&deps.env);
    Ok(deps)
}

/// Parse `args` and run; returns the process exit code.
pub fn run<I, T>(args: I) -> u8
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(e) => {
            let code = if e.use_stderr() { EXIT_USAGE } else { EXIT_OK };
            // Printing can only fail when the stream is closed; nothing to do then.
            let _ = e.print();
            return code;
        }
    };
    execute(&cli)
}

/// Run a parsed command line on its own runtime.
pub fn execute(cli: &Cli) -> u8 {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("dbstudio-cli: cannot start the runtime: {e}");
            return EXIT_ERROR;
        }
    };
    rt.block_on(async {
        // `tools` does not need the store.
        if matches!(cli.command, Command::Tools) {
            return print_tools(&mut std::io::stdout(), &Default::default());
        }
        if let Command::Verify {
            file,
            passphrase_env,
        } = &cli.command
        {
            if cli.data_dir.is_none()
                && default_data_dir().is_none_or(|d| !d.join(DATABASE_FILE).exists())
            {
                // Verifying needs no store: do it without recording a run.
                let pass = std::env::var(passphrase_env).ok().map(Secret::new);
                return verify_only(file, pass.as_ref()).await;
            }
        }
        match open_deps(cli) {
            Ok(deps) => execute_async(cli, &deps, &mut std::io::stdout()).await,
            Err(e) => {
                eprintln!("dbstudio-cli: {}", e.message());
                EXIT_ERROR
            }
        }
    })
}

/// Reporter printing phase changes and log lines to stderr; Ctrl-C cancels.
struct CliReporter {
    cancel: CancelToken,
    phase: Mutex<String>,
    quiet: bool,
}

impl CliReporter {
    fn new(quiet: bool) -> Arc<Self> {
        let r = Arc::new(CliReporter {
            cancel: CancelToken::new(),
            phase: Mutex::new(String::new()),
            quiet,
        });
        if !quiet {
            let token = r.cancel.clone();
            // Ctrl-C and, on Unix, SIGTERM (cron/launchd stopping the job): cancel the run, which
            // kills the tool and removes the temporary files on the way out.
            tokio::spawn(async move {
                #[cfg(unix)]
                {
                    use tokio::signal::unix::{SignalKind, signal};
                    if let Ok(mut term) = signal(SignalKind::terminate()) {
                        tokio::select! {
                            _ = tokio::signal::ctrl_c() => {}
                            _ = term.recv() => {}
                        }
                        token.cancel();
                        return;
                    }
                }
                if tokio::signal::ctrl_c().await.is_ok() {
                    token.cancel();
                }
            });
        }
        r
    }
}

impl JobReporter for CliReporter {
    fn progress(&self, p: &JobProgress) {
        let mut cur = self.phase.lock().unwrap_or_else(|e| e.into_inner());
        if *cur != p.phase {
            *cur = p.phase.clone();
            if !self.quiet {
                eprintln!("[{}]", p.phase);
            }
        }
    }

    fn log(&self, line: &str) {
        if !self.quiet {
            eprintln!("{line}");
        }
    }

    fn cancel_token(&self) -> &CancelToken {
        &self.cancel
    }
}

fn print_tools(
    out: &mut dyn Write,
    overrides: &std::collections::BTreeMap<dbstudio_backup::NativeTool, String>,
) -> u8 {
    for t in detect_tools(overrides) {
        let _ = writeln!(
            out,
            "{:<14} {}  {}",
            t.tool.binary().unwrap_or("-"),
            t.path.as_deref().unwrap_or("-"),
            t.version.clone().or(t.problem).unwrap_or_default()
        );
    }
    EXIT_OK
}

async fn verify_only(file: &Path, pass: Option<&Secret>) -> u8 {
    let rep = CliReporter::new(false);
    match verify(file, pass, rep.as_ref()).await {
        Ok(()) => {
            println!("ok: {} is intact", file.display());
            EXIT_OK
        }
        Err(e) => {
            eprintln!("dbstudio-cli: {}", e.message());
            EXIT_ERROR
        }
    }
}

fn fail(e: &Error) -> u8 {
    eprintln!("dbstudio-cli: {}", e.message());
    if matches!(e, Error::Busy(_)) {
        EXIT_LOCKED
    } else {
        EXIT_ERROR
    }
}

fn describe(run: &BackupRun) -> String {
    format!(
        "{} {:?} {}",
        run.job_name,
        run.status,
        run.file_path.as_deref().unwrap_or("")
    )
}

fn ask(prompt: &str, deps: &CliDeps) -> bool {
    if let Some(answer) = deps.assume_yes {
        return answer;
    }
    if !std::io::stdin().is_terminal() {
        return false;
    }
    eprint!("{prompt} [y/N] ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).is_ok()
        && matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Run a parsed command line against `deps`, writing normal output to `out`.
pub async fn execute_async(cli: &Cli, deps: &CliDeps, out: &mut dyn Write) -> u8 {
    let env = &deps.env;
    match &cli.command {
        Command::Tools => {
            let overrides = tool_overrides(env).unwrap_or_default();
            print_tools(out, &overrides)
        }
        Command::Backup { job: None, .. } => match env.service.store().backup_jobs_list() {
            Ok(rows) => {
                for row in rows {
                    let _ = writeln!(
                        out,
                        "{}\t{}\t{}\t{}\t{}",
                        row.id,
                        row.name,
                        if row.enabled { "enabled" } else { "disabled" },
                        row.cron.as_deref().unwrap_or("manual"),
                        row.last_run_at.as_deref().unwrap_or("never")
                    );
                }
                EXIT_OK
            }
            Err(e) => fail(&e),
        },
        Command::Backup { job: Some(id), .. } => {
            let row = match env.service.store().backup_job_get(*id) {
                Ok(Some(r)) => r,
                Ok(None) => return fail(&Error::NotFound(format!("backup job {id}"))),
                Err(e) => return fail(&e),
            };
            let job = match row_to_job(&row) {
                Ok(j) => j,
                Err(e) => return fail(&e),
            };
            let rep = CliReporter::new(false);
            match run_job(env, &job, RunTrigger::Cli, rep).await {
                Ok(run) if run.status == RunStatus::Succeeded => {
                    let _ = writeln!(out, "{}", describe(&run));
                    EXIT_OK
                }
                Ok(run) => {
                    let msg = run.error.map_or(format!("{:?}", run.status), |e| e.message);
                    eprintln!("dbstudio-cli: backup {} failed: {msg}", job.name);
                    EXIT_ERROR
                }
                Err(e) => fail(&e),
            }
        }
        Command::Restore {
            file,
            connection,
            database,
            create_database,
            clean,
            continue_on_error,
            yes,
            confirm_text,
            passphrase_env,
        } => {
            let spec = RestoreSpec {
                file: file.to_string_lossy().into_owned(),
                target_connection_id: *connection,
                target_database: database.clone(),
                create_database: *create_database,
                clean: *clean,
                objects: vec![],
                continue_on_error: *continue_on_error,
            };
            let profile = match env.service.get_profile(*connection) {
                Ok(p) => p,
                Err(e) => return fail(&e),
            };
            if restore_is_guarded(&profile.config, &spec) && !restore_names_its_target(&spec) {
                eprintln!("dbstudio-cli: {}", explicit_target_required().message());
                return EXIT_USAGE;
            }
            if let Some(required) = restore_confirmation_text(&profile.config, &spec) {
                if !*yes || confirm_text.as_deref() != Some(required.as_str()) {
                    eprintln!(
                        "dbstudio-cli: restoring into {} needs --yes --confirm-text {required}",
                        profile.config.name
                    );
                    return EXIT_USAGE;
                }
            } else if !*yes
                && !ask(
                    &format!("Restore {} into {}?", file.display(), profile.config.name),
                    deps,
                )
            {
                eprintln!("dbstudio-cli: not confirmed (use --yes)");
                return EXIT_USAGE;
            }
            let pass = match needs_passphrase(file, passphrase_env, deps).await {
                Ok(p) => p,
                Err(code) => return code,
            };
            let rep = CliReporter::new(false);
            let meta = RunMeta {
                job_id: None,
                job_name: file_label(file),
                connection_id: Some(*connection),
                kind: RunKind::Restore,
                trigger: RunTrigger::Cli,
                engine: "restore".into(),
            };
            let spec2 = spec.clone();
            let env2 = env.clone();
            let run = record_run(env, meta, rep, move |r| async move {
                let o = execute_restore(&env2, &spec2, pass, r).await?;
                for e in &o.errors {
                    eprintln!("statement {}: {}", e.line, e.message);
                }
                Ok(RunOutput::from(&o))
            })
            .await;
            match run {
                Ok(run) if run.status == RunStatus::Succeeded => {
                    let _ = writeln!(
                        out,
                        "restored {} objects, {} rows",
                        run.objects.unwrap_or(0),
                        run.rows.unwrap_or(0)
                    );
                    EXIT_OK
                }
                Ok(run) => {
                    let msg = run.error.map_or(format!("{:?}", run.status), |e| e.message);
                    eprintln!("dbstudio-cli: restore failed: {msg}");
                    EXIT_ERROR
                }
                Err(e) => fail(&e),
            }
        }
        Command::Verify {
            file,
            passphrase_env,
        } => {
            let pass = match needs_passphrase(file, passphrase_env, deps).await {
                Ok(p) => p,
                Err(code) => return code,
            };
            let rep = CliReporter::new(false);
            let meta = RunMeta {
                job_id: None,
                job_name: file_label(file),
                connection_id: None,
                kind: RunKind::Verify,
                trigger: RunTrigger::Cli,
                engine: "verify".into(),
            };
            let path = file.clone();
            let run = record_run(env, meta, rep, move |r| async move {
                verify(&path, pass.as_ref(), r.as_ref()).await?;
                let size = std::fs::metadata(&path).map(|m| m.len()).ok();
                Ok(RunOutput {
                    file_path: Some(path.to_string_lossy().into_owned()),
                    size_bytes: size,
                    ..Default::default()
                })
            })
            .await;
            match run {
                Ok(run) if run.status == RunStatus::Succeeded => {
                    let _ = writeln!(out, "ok: {} is intact", file.display());
                    EXIT_OK
                }
                Ok(run) => {
                    let msg = run.error.map_or(format!("{:?}", run.status), |e| e.message);
                    eprintln!("dbstudio-cli: {msg}");
                    EXIT_ERROR
                }
                Err(e) => fail(&e),
            }
        }
    }
}

fn file_label(file: &Path) -> String {
    file.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The passphrase for `file` when it is encrypted; an error code when it is missing.
async fn needs_passphrase(file: &Path, var: &str, deps: &CliDeps) -> Result<Option<Secret>, u8> {
    let info = inspect(file, None).await.map_err(|e| fail(&e))?;
    if !info.encrypted {
        return Ok(None);
    }
    match (deps.passphrase)(var) {
        Some(p) => Ok(Some(p)),
        None => {
            eprintln!(
                "dbstudio-cli: {} is encrypted: put the passphrase in the environment variable {var}",
                file.display()
            );
            Err(EXIT_USAGE)
        }
    }
}

#[cfg(test)]
mod tests;
