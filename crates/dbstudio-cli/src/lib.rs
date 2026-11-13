//! Command line interface of DBStudio (backup, restore, verify, tools).
//!
//! Exit codes: [`EXIT_OK`] success, [`EXIT_ERROR`] failure, [`EXIT_USAGE`] bad arguments,
//! [`EXIT_LOCKED`] the job is already running (file lock held by the app or another CLI).
//!
//! Scaffold status: every subcommand prints `not implemented` to stderr and exits with
//! [`EXIT_ERROR`]. Package H implements them.

use std::ffi::OsString;
use std::path::PathBuf;

use clap::{ArgGroup, Parser, Subcommand};
use uuid::Uuid;

/// Everything went fine.
pub const EXIT_OK: u8 = 0;
/// The operation failed.
pub const EXIT_ERROR: u8 = 1;
/// Wrong arguments.
pub const EXIT_USAGE: u8 = 2;
/// The job is already running (lock held).
pub const EXIT_LOCKED: u8 = 3;

/// `dbstudio-cli`.
#[derive(Debug, Parser)]
#[command(name = "dbstudio-cli", version, about = "DBStudio command line")]
pub struct Cli {
    /// Application data directory (default: `<OS data dir>/dev.dbstudio.desktop`).
    #[arg(long, global = true, value_name = "DIR")]
    pub data_dir: Option<PathBuf>,

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
        /// Do not ask for confirmation.
        #[arg(long)]
        yes: bool,
        /// Database name typed to confirm a restore into a production connection.
        #[arg(long, value_name = "TEXT")]
        confirm_text: Option<String>,
    },
    /// Check the integrity of a backup file.
    Verify {
        /// Backup file.
        #[arg(long, value_name = "FILE")]
        file: PathBuf,
    },
    /// Show the native backup tools that were found.
    Tools,
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

/// Run a parsed command line. Scaffold: `not implemented`.
pub fn execute(cli: &Cli) -> u8 {
    let name = match &cli.command {
        Command::Backup { .. } => "backup",
        Command::Restore { .. } => "restore",
        Command::Verify { .. } => "verify",
        Command::Tools => "tools",
    };
    eprintln!("dbstudio-cli {name}: not implemented");
    EXIT_ERROR
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    const JOB: &str = "00000000-0000-0000-0000-000000000001";

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        let mut v = vec!["dbstudio-cli"];
        v.extend_from_slice(args);
        Cli::try_parse_from(v)
    }

    #[test]
    fn command_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_backup_job_and_list() {
        match parse(&["backup", "--job", JOB]).unwrap().command {
            Command::Backup { job, list } => {
                assert_eq!(job, Some(Uuid::from_u128(1)));
                assert!(!list);
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            parse(&["backup", "--list"]).unwrap().command,
            Command::Backup {
                job: None,
                list: true
            }
        ));
        assert!(parse(&["backup"]).is_err());
        assert!(parse(&["backup", "--list", "--job", JOB]).is_err());
        assert!(parse(&["backup", "--job", "not-a-uuid"]).is_err());
    }

    #[test]
    fn parses_restore_verify_tools_and_global_data_dir() {
        let cli = parse(&[
            "--data-dir",
            "/d",
            "restore",
            "--file",
            "a.dbsbak",
            "--connection",
            JOB,
            "--database",
            "app",
            "--yes",
            "--confirm-text",
            "app",
        ])
        .unwrap();
        assert_eq!(cli.data_dir, Some(PathBuf::from("/d")));
        match cli.command {
            Command::Restore {
                file,
                connection,
                database,
                yes,
                confirm_text,
            } => {
                assert_eq!(file, PathBuf::from("a.dbsbak"));
                assert_eq!(connection, Uuid::from_u128(1));
                assert_eq!(database.as_deref(), Some("app"));
                assert!(yes);
                assert_eq!(confirm_text.as_deref(), Some("app"));
            }
            other => panic!("{other:?}"),
        }
        // `--data-dir` is global: accepted after the subcommand too.
        let cli = parse(&["tools", "--data-dir", "/x"]).unwrap();
        assert_eq!(cli.data_dir, Some(PathBuf::from("/x")));
        assert!(matches!(
            parse(&["verify", "--file", "f"]).unwrap().command,
            Command::Verify { .. }
        ));
        assert!(parse(&["restore", "--file", "f"]).is_err());
        assert!(parse(&[]).is_err());
    }

    #[test]
    fn exit_codes() {
        assert_eq!(run(["dbstudio-cli", "tools"]), EXIT_ERROR);
        assert_eq!(run(["dbstudio-cli", "backup"]), EXIT_USAGE);
        assert_eq!(run(["dbstudio-cli", "--help"]), EXIT_OK);
        assert_eq!(run(["dbstudio-cli", "verify", "--file", "x"]), EXIT_ERROR);
        assert_eq!((EXIT_OK, EXIT_ERROR, EXIT_USAGE, EXIT_LOCKED), (0, 1, 2, 3));
    }
}
