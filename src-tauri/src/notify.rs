//! Texts of OS notifications in Vietnamese and English (scheduler, tray), chosen by
//! `AppSettings.language` (`System` follows the OS locale). Independent of the frontend i18n
//! files so notifications work while the window is closed.
//!
//! Contract scaffold of the backlog contract v2 (OS locale detection uses the `sys-locale`
//! crate declared in the workspace): package B fills the table and the lookup in; this module
//! is empty on purpose so `lib.rs` does not change again.
