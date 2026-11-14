//! `dbstudio-cli` entry point.

use std::process::ExitCode;

fn main() -> ExitCode {
    // See dbstudio_lib::install_crypto_provider: ring and aws-lc-rs are both linked.
    let _ = rustls::crypto::ring::default_provider().install_default();
    ExitCode::from(dbstudio_cli::run(std::env::args_os()))
}
