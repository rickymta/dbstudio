//! Background behaviour of the app: close-to-tray decision, autostart, single instance and OS
//! notifications for finished backup runs.
//!
//! `lib.rs` wires four things to this module: the single-instance plugin
//! ([`on_second_instance`]), the window event hook ([`on_window_event`]), [`setup`] (tray,
//! scheduler, hidden start) and the autostart launch argument [`BACKGROUND_ARG`].

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use dbstudio_backup::{BackupJob, BackupRun, RunStatus};
use dbstudio_core::{Error, Result};
use dbstudio_store::CloseBehavior;
use tauri::{AppHandle, Manager, Window, WindowEvent};
use tauri_plugin_autostart::ManagerExt;

use crate::host::Host;
use crate::notify::{Lang, RunFacts};
use crate::state::AppState;

/// Label of the main window.
pub const MAIN_WINDOW: &str = "main";
/// Argument the autostart entry passes: start hidden in the tray.
pub const BACKGROUND_ARG: &str = "--background";

static TRAY_READY: AtomicBool = AtomicBool::new(false);

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
pub fn setup(app: &AppHandle, state: &AppState) {
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
    // Scratch directories and half-written files of a run that died with the app.
    let env = crate::commands::backup::run_env(state);
    tauri::async_runtime::spawn_blocking(move || {
        let removed = dbstudio_backup::runner::cleanup_orphans(&env);
        if removed > 0 {
            tracing::info!(removed, "removed leftovers of interrupted backup runs");
        }
    });
    crate::scheduler::start(state.clone());
}

/// Seconds within which a second "Quit" confirms stopping the running jobs.
const QUIT_CONFIRM_SECS: i64 = 30;
static QUIT_ASKED_AT: AtomicI64 = AtomicI64::new(0);

/// What a "Quit" request does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuitDecision {
    /// Nothing is running: exit.
    Exit,
    /// Jobs are running: tell the user and wait for a second request.
    Ask,
    /// Confirmed: cancel the jobs, then exit.
    CancelAndExit,
}

/// Pure decision: `active` running jobs, `now` and the time of the previous request (0 = none).
pub fn quit_decision(active: usize, now: i64, asked_at: i64) -> QuitDecision {
    if active == 0 {
        QuitDecision::Exit
    } else if asked_at > 0 && now - asked_at <= QUIT_CONFIRM_SECS {
        QuitDecision::CancelAndExit
    } else {
        QuitDecision::Ask
    }
}

fn active_jobs(state: &AppState) -> Vec<dbstudio_core::JobId> {
    use dbstudio_core::JobStatus;
    state
        .jobs
        .list()
        .into_iter()
        .filter(|j| matches!(j.status, JobStatus::Queued | JobStatus::Running))
        .map(|j| j.id)
        .collect()
}

/// Tray "Quit": never kills a running backup silently.
pub fn request_quit(app: &AppHandle) {
    let Some(state) = app.try_state::<AppState>() else {
        app.exit(0);
        return;
    };
    let active = active_jobs(&state);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    match quit_decision(active.len(), now, QUIT_ASKED_AT.load(Ordering::SeqCst)) {
        QuitDecision::Exit => app.exit(0),
        QuitDecision::Ask => {
            QUIT_ASKED_AT.store(now, Ordering::SeqCst);
            show_main_window(app);
            let lang = crate::notify::current_language(state.connections.store());
            let (title, body) = crate::notify::quit_busy(lang, active.len(), QUIT_CONFIRM_SECS);
            if let Err(e) = state.host.notify(&title, &body) {
                tracing::warn!(error = %e, "cannot show the quit warning");
            }
        }
        QuitDecision::CancelAndExit => {
            for id in &active {
                let _ = state.jobs.cancel(*id);
            }
            let state = state.inner().clone();
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                // Give the jobs a moment to stop their tools and remove their temporary files.
                for _ in 0..50 {
                    if active_jobs(&state).is_empty() {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                app.exit(0);
            });
        }
    }
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
pub fn notification_for(lang: Lang, job: &BackupJob, run: &BackupRun) -> Option<(String, String)> {
    match run.status {
        RunStatus::Succeeded if job.notify.on_success => Some(crate::notify::backup_succeeded(
            lang,
            &job.name,
            RunFacts {
                size_bytes: run.size_bytes,
                duration_ms: run.duration_ms,
            },
        )),
        RunStatus::Failed if job.notify.on_failure => Some(crate::notify::backup_failed(
            lang,
            &job.name,
            // Error texts may quote a connection string: mask secrets before the OS shows it.
            run.error
                .as_ref()
                .map(|e| crate::redact::redact(&e.message))
                .as_deref(),
        )),
        _ => None,
    }
}

/// Show the notification for `run` (failures to show are only logged).
pub fn notify_run(host: &dyn Host, lang: Lang, job: &BackupJob, run: &BackupRun) {
    if let Some((title, body)) = notification_for(lang, job, run) {
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
        let (_, body) = notification_for(Lang::En, &all, &run(RunStatus::Succeeded)).unwrap();
        assert_eq!(body, "nightly: finished (3.0 MB)");
        let (title, body) = notification_for(Lang::En, &all, &run(RunStatus::Failed)).unwrap();
        assert!(title.contains("failed") && body == "nightly: disk full");
        let (title, body) = notification_for(Lang::Vi, &all, &run(RunStatus::Failed)).unwrap();
        assert!(title.contains("thất bại") && body == "nightly: disk full");
        assert!(notification_for(Lang::En, &all, &run(RunStatus::Cancelled)).is_none());
        let quiet_ok = job(NotifyPolicy {
            on_success: false,
            on_failure: true,
        });
        assert!(notification_for(Lang::En, &quiet_ok, &run(RunStatus::Succeeded)).is_none());
        assert!(notification_for(Lang::En, &quiet_ok, &run(RunStatus::Failed)).is_some());
        let quiet_fail = job(NotifyPolicy {
            on_success: true,
            on_failure: false,
        });
        assert!(notification_for(Lang::En, &quiet_fail, &run(RunStatus::Failed)).is_none());
    }

    #[test]
    fn quitting_while_jobs_run_needs_a_second_request() {
        assert_eq!(quit_decision(0, 100, 0), QuitDecision::Exit);
        assert_eq!(quit_decision(2, 100, 0), QuitDecision::Ask);
        assert_eq!(quit_decision(2, 110, 100), QuitDecision::CancelAndExit);
        // The warning expired.
        assert_eq!(
            quit_decision(2, 100 + QUIT_CONFIRM_SECS + 1, 100),
            QuitDecision::Ask
        );
        assert_eq!(quit_decision(0, 110, 100), QuitDecision::Exit);
    }

    #[test]
    fn the_fake_host_receives_vietnamese_text_when_the_setting_says_so() {
        use dbstudio_core::DriverRegistry;
        use dbstudio_store::{AppSettings, UiLanguage};

        let (state, _s, host) = crate::state::testing::test_state_with_host(DriverRegistry::new());
        let store = state.connections.store();
        let settings = AppSettings {
            language: UiLanguage::Vi,
            ..AppSettings::default()
        };
        store.set_settings(settings).unwrap();
        let lang = crate::notify::current_language(store);
        let j = job(NotifyPolicy::default());
        notify_run(host.as_ref(), lang, &j, &run(RunStatus::Succeeded));
        notify_run(host.as_ref(), lang, &j, &run(RunStatus::Failed));
        let seen = host.notifications.lock().unwrap().clone();
        assert_eq!(seen[0].0, "Sao lưu DBStudio");
        assert_eq!(seen[0].1, "nightly: đã hoàn tất (3.0 MB)");
        assert_eq!(seen[1].0, "Sao lưu DBStudio thất bại");
    }
}
