//! `dbstudio-cli` entry point.

use std::process::ExitCode;

fn main() -> ExitCode {
    ExitCode::from(dbstudio_cli::run(std::env::args_os()))
}
