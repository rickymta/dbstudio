//! Texts of OS notifications in Vietnamese and English (scheduler, tray), chosen by
//! `AppSettings.language` (`System` follows the OS locale). Independent of the frontend i18n
//! files so notifications work while the window is closed.
//!
//! The table lives in `dbstudio_backup::notify` so the command line prints the same texts;
//! this module only reads the setting.

pub use dbstudio_backup::notify::{Lang, RunFacts, backup_failed, backup_succeeded, quit_busy};
use dbstudio_store::Store;

/// Language of notifications now: the stored setting (defaults when it cannot be read).
pub fn current_language(store: &Store) -> Lang {
    let language = store.get_settings().map(|s| s.language).unwrap_or_default();
    dbstudio_backup::notify::language_for(language)
}
