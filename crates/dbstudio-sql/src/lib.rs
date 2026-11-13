//! Language support for the script editor, consoles and the safety guard.
//!
//! The crate is pure Rust (no I/O). Every position is a character offset (Unicode scalar values)
//! counted from 0, like [`dbstudio_core::TextRange`].
//!
//! Scaffold status: only [`guard::check`] and [`dialect::Dialect`] are real. The other modules
//! implement the minimal documented stub contract and are replaced by package A.

pub mod classify;
pub mod ddl;
pub mod dialect;
pub mod guard;
pub mod lexer;
pub mod mongo_shell;
pub mod params;
pub mod redact;
pub mod redis_cli;
pub mod split;
pub mod vocabulary;

pub use classify::analyze;
pub use dialect::Dialect;
pub use guard::{Policy, Verdict, check};
pub use params::{BoundScript, OffsetMap, bind, find_params};
pub use split::{StreamSplitter, split};
pub use vocabulary::{CommandHint, Vocabulary, vocabulary};
