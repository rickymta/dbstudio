//! Background behaviour of the app: close-to-tray decision, autostart, single instance and OS
//! notifications for finished backup runs.
//!
//! `lib.rs` wires four things to this module: the single-instance plugin
//! ([`on_second_instance`]), the window event hook ([`on_window_event`]), [`setup`] (tray,
//! scheduler, hidden start) and the autostart launch argument [`BACKGROUND_ARG`].

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use dbstudio_backup::{BackupJob, BackupRun, RunStatus};
use dbstudio_core::{Error, Result};
use dbstudio_store::CloseBehavior;
use tauri::{AppHandle, Manager, Window, WindowEvent};
use tauri_plugin_autostart::ManagerExt;

use crate::host::Host;
use crate::state::AppState;

/// Label of the main window.
pub const MAIN_WINDOW: &str = "main";
/// Argument the autostart entry passes: start hidden in the tray.
pub const BACKGROUND_ARG: &str = "--background";

static LOCKS_DIR: OnceLock<PathBuf> = OnceLock::new();
static TRAY_READY: AtomicBool = AtomicBool::new(false);

/// Directory of the per-job lock files, shared with `dbstudio-cli` (`<data dir>/locks`).
pub fn locks_dir() -> PathBuf {
    LOCKS_DIR
        .get()
        .cloned()
        .unwrap_or_else(|| std::env::temp_dir().join("dbstudio-locks"))
}

/// Whether a tray icon exists (closing the window must not hide the app without one).
pub fn tray_ready() -> bool {
    TRAY_READY.load(Ordering::SeqCst)
}

pub(crate) fn set_tray_ready(ready: bool) {
    TRAY_READY.store(ready, Ordering::SeqCst);
}

/// Pure decision: should closing the main window keep the app running in the tray?
pub fn should_keep_running(
    behavior: CloseBehavior,
    has_scheduled_jobs: bool,
    tray_available: bool,
) -> bool {
    if !tray_available {
        return false;
    }
    match behavior {
        CloseBehavior::Always => true,
        CloseBehavior::Never => false,
        CloseBehavior::Auto => has_scheduled_jobs,
    }
}

/// True when some enabled job has a schedule.
pub fn has_scheduled_jobs(state: &AppState) -> bool {
    state
        .connections
        .store()
        .backup_jobs_list()
        .map(|rows| rows.iter().any(|r| r.enabled && r.cron.is_some()))
        .unwrap_or(false)
}

/// Whether the main window should be hidden instead of closed right now.
pub fn keep_running_on_close(state: &AppState) -> bool {
    let behavior = state
        .connections
        .settings()
        .map(|s| s.close_to_tray)
        .unwrap_or_default();
    should_keep_running(behavior, has_scheduled_jobs(state), tray_ready())
}

/// Window event hook: the close button hides the window when the app should keep running.
pub fn on_window_event(window: &Window, event: &WindowEvent) {
    if window.label() != MAIN_WINDOW {
        return;
    }
    if let WindowEvent::CloseRequested { api, .. } = event {
        let keep = window
            .app_handle()
            .try_state::<AppState>()
            .is_some_and(|s| keep_running_on_close(&s));
        if keep {
            api.prevent_close();
            if let Err(e) = window.hide() {
                tracing::warn!(error = %e, "cannot hide the main window");
            }
        }
    }
}

/// Bring the main window back (tray click, second launch).
pub fn show_main_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(MAIN_WINDOW) {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

/// Called by the single-instance plugin when the user starts DBStudio a second time.
pub fn on_second_instance(app: &AppHandle, _args: Vec<String>, _cwd: String) {
    show_main_window(app);
}

/// Start-up wiring: lock directory, tray icon, hidden start at login, scheduler.
pub fn setup(app: &AppHandle, state: &AppState, data_dir: &Path) {
    let _ = LOCKS_DIR.set(data_dir.join("locks"));
    match crate::tray::install(app) {
        Ok(()) => set_tray_ready(true),
        Err(e) => {
            tracing::warn!(error = %e, "cannot create the tray icon; closing the window quits")
        }
    }
    let started_in_background = std::env::args().any(|a| a == BACKGROUND_ARG);
    if started_in_background && tray_ready() {
        if let Some(w) = app.get_webview_window(MAIN_WINDOW) {
            let _ = w.hide();
        }
    }
    crate::scheduler::start(state.clone());
}

// ---------------------------------------------------------------------------------------------
// Autostart
// ---------------------------------------------------------------------------------------------

/// Launch-at-login switch (a trait so command logic is testable without a Tauri runtime).
pub trait Autostart: Send + Sync {
    /// Whether the entry exists.
    fn is_enabled(&self) -> Result<bool>;
    /// Create or remove the entry.
    fn set_enabled(&self, enabled: bool) -> Result<()>;
}

/// [`Autostart`] backed by the Tauri plugin.
pub struct TauriAutostart(pub AppHandle);

impl Autostart for TauriAutostart {
    fn is_enabled(&self) -> Result<bool> {
        self.0
            .autolaunch()
            .is_enabled()
            .map_err(|e| Error::Other(format!("cannot read the autostart setting: {e}")))
    }

    fn set_enabled(&self, enabled: bool) -> Result<()> {
        let launcher = self.0.autolaunch();
        let result = if enabled {
            launcher.enable()
        } else {
            launcher.disable()
        };
        result.map_err(|e| Error::Other(format!("cannot change the autostart setting: {e}")))
    }
}

/// No autostart support (tests and the scaffold-compatible command helpers).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoAutostart;

impl Autostart for NoAutostart {
    fn is_enabled(&self) -> Result<bool> {
        Ok(false)
    }

    fn set_enabled(&self, _enabled: bool) -> Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Notifications
// ---------------------------------------------------------------------------------------------

/// Title and body of the OS notification for a finished run, honouring the job's policy.
/// `None` when nothing should be shown.
pub fn notification_for(job: &BackupJob, run: &BackupRun) -> Option<(String, String)> {
    match run.status {
        RunStatus::Succeeded if job.notify.on_success => Some((
            "DBStudio backup".into(),
            format!(
                "{}: finished{}",
                job.name,
                run.size_bytes
                    .map(|b| format!(" ({})", human_size(b)))
                    .unwrap_or_default()
            ),
        )),
        RunStatus::Failed if job.notify.on_failure => Some((
            "DBStudio backup failed".into(),
            format!(
                "{}: {}",
                job.name,
                run.error
                    .as_ref()
                    .map_or("unknown error", |e| e.message.as_str())
            ),
        )),
        _ => None,
    }
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// Show the notification for `run` (failures to show are only logged).
pub fn notify_run(host: &dyn Host, job: &BackupJob, run: &BackupRun) {
    if let Some((title, body)) = notification_for(job, run) {
        if let Err(e) = host.notify(&title, &body) {
            tracing::warn!(error = %e, "cannot show the backup notification");
        }
    }
}

#[cfg(test)]
mod tests {
    use dbstudio_backup::{
        BackupEngine, BackupSpec, Compression, Destination, MissedRunPolicy, NotifyPolicy,
        Retention, RunKind, RunTrigger,
    };
    use dbstudio_core::JobError;
    use dbstudio_export::ExportContent;
    use uuid::Uuid;

    use super::*;

    #[test]
    fn close_behavior_matrix() {
        use CloseBehavior::{Always, Auto, Never};
        for (b, jobs, tray, keep) in [
            (Auto, false, true, false),
            (Auto, true, true, true),
            (Always, false, true, true),
            (Never, true, true, false),
            // Without a tray icon the app can never hide.
            (Always, true, false, false),
            (Auto, true, false, false),
        ] {
            assert_eq!(
                should_keep_running(b, jobs, tray),
                keep,
                "{b:?} {jobs} {tray}"
            );
        }
    }

    fn job(notify: NotifyPolicy) -> BackupJob {
        BackupJob {
            id: Uuid::nil(),
            name: "nightly".into(),
            enabled: true,
            spec: BackupSpec {
                connection_id: Uuid::nil(),
                targets: vec![],
                engine: BackupEngine::Logical {
                    content: ExportContent::Both,
                    ddl: Default::default(),
                },
                compression: Compression::None,
                encrypted: false,
                destination: Destination::LocalDir {
                    path: "/b".into(),
                    file_template: "x".into(),
                },
            },
            schedule: None,
            retention: Retention::default(),
            missed_run: MissedRunPolicy::RunOnce,
            notify,
            last_run_at: None,
            next_run_at: None,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn run(status: RunStatus) -> BackupRun {
        BackupRun {
            id: Uuid::nil(),
            job_id: None,
            job_name: "nightly".into(),
            connection_id: None,
            kind: RunKind::Backup,
            trigger: RunTrigger::Schedule,
            engine: "logical".into(),
            status,
            started_at: String::new(),
            finished_at: None,
            duration_ms: None,
            file_path: None,
            size_bytes: Some(3 * 1024 * 1024),
            checksum: None,
            objects: None,
            rows: None,
            error: Some(JobError {
                kind: "io".into(),
                message: "disk full".into(),
            }),
            log_tail: None,
        }
    }

    #[test]
    fn notifications_follow_the_policy() {
        let all = job(NotifyPolicy::default());
        let (_, body) = notification_for(&all, &run(RunStatus::Succeeded)).unwrap();
        assert_eq!(body, "nightly: finished (3.0 MB)");
        let (title, body) = notification_for(&all, &run(RunStatus::Failed)).unwrap();
        assert!(title.contains("failed") && body == "nightly: disk full");
        assert!(notification_for(&all, &run(RunStatus::Cancelled)).is_none());
        let quiet_ok = job(NotifyPolicy {
            on_success: false,
            on_failure: true,
        });
        assert!(notification_for(&quiet_ok, &run(RunStatus::Succeeded)).is_none());
        assert!(notification_for(&quiet_ok, &run(RunStatus::Failed)).is_some());
        let quiet_fail = job(NotifyPolicy {
            on_success: true,
            on_failure: false,
        });
        assert!(notification_for(&quiet_fail, &run(RunStatus::Failed)).is_none());
    }

    #[test]
    fn human_sizes() {
        assert_eq!(human_size(10), "10 B");
        assert_eq!(human_size(1536), "1.5 KB");
    }
}
