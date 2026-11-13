//! IPC commands exposed to the frontend. Keep them thin: logic belongs in the core, driver and
//! store crates.

pub mod app;
pub mod browse;
pub mod connections;
pub mod sessions;
