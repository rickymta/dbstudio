//! Application logging (APP-07): console in debug builds plus a daily rotating file.
//!
//! Secrets are never logged: command handlers log identifiers only, and `Secret`/`SecretChange`
//! have redacting `Debug` implementations.

use std::io;
use std::path::Path;
use std::sync::OnceLock;

use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_appender::rolling::{Builder, Rotation};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

/// Number of daily log files kept.
pub const MAX_LOG_FILES: usize = 7;
const FILE_PREFIX: &str = "dbstudio";
const FILE_SUFFIX: &str = "log";

static LOG_GUARD: OnceLock<WorkerGuard> = OnceLock::new();

/// Default filter: `debug` in debug builds, `info` otherwise; `RUST_LOG` overrides it.
pub fn env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(if cfg!(debug_assertions) {
            "debug"
        } else {
            "info"
        })
    })
}

/// Non-blocking writer onto a daily rotating file in `dir`, keeping [`MAX_LOG_FILES`] files.
pub fn file_writer(dir: &Path) -> io::Result<(NonBlocking, WorkerGuard)> {
    std::fs::create_dir_all(dir)?;
    let appender = Builder::new()
        .rotation(Rotation::DAILY)
        .filename_prefix(FILE_PREFIX)
        .filename_suffix(FILE_SUFFIX)
        .max_log_files(MAX_LOG_FILES)
        .build(dir)
        .map_err(io::Error::other)?;
    Ok(tracing_appender::non_blocking(appender))
}

/// Install the global subscriber. `log_dir` of `None` (or an unusable directory) leaves only
/// the console output. Safe to call more than once; later calls are ignored.
pub fn init(log_dir: Option<&Path>) {
    let console = cfg!(debug_assertions).then(|| {
        tracing_subscriber::fmt::layer()
            .with_target(false)
            .with_filter(env_filter())
    });
    let mut file_error = None;
    let file = log_dir.and_then(|dir| match file_writer(dir) {
        Ok((writer, guard)) => {
            let _ = LOG_GUARD.set(guard);
            Some(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_target(false)
                    .with_writer(writer)
                    .with_filter(env_filter()),
            )
        }
        Err(e) => {
            file_error = Some(e);
            None
        }
    });
    let installed = tracing_subscriber::registry()
        .with(console)
        .with(file)
        .try_init()
        .is_ok();
    if let (true, Some(e)) = (installed, file_error) {
        tracing::warn!(error = %e, "file logging disabled");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_events_to_a_dated_log_file() {
        let dir = tempfile::tempdir().unwrap();
        let (writer, guard) = file_writer(&dir.path().join("logs")).unwrap();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(writer)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(host = "db.example", "session opened");
        });
        drop(guard); // flush
        let files: Vec<_> = std::fs::read_dir(dir.path().join("logs"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(files.len(), 1);
        let name = files[0].file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with("dbstudio.") && name.ends_with(".log"),
            "{name}"
        );
        let text = std::fs::read_to_string(&files[0]).unwrap();
        assert!(text.contains("session opened") && text.contains("db.example"));
    }
}
