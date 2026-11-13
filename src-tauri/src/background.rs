//! Background behaviour of the app: close-to-tray decision, autostart, single instance and OS
//! notifications for finished backup runs.
//!
//! Owner: package H (backup). Scaffold: placeholder. The notification and autostart plugins are
//! registered by `lib.rs` but never enabled; the single-instance plugin is only a dependency.

#![allow(dead_code)] // placeholder

/// Whether closing the main window should keep the app running. Placeholder: never.
pub fn keep_running_on_close() -> bool {
    false
}
