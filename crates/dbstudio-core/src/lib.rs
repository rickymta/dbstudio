//! Domain types and the driver abstraction for DBStudio.
//!
//! This crate is deliberately free of any UI or Tauri dependency so that it can be reused by the
//! desktop app, tests and any future headless tooling.
//!
//! # Model
//!
//! * A [`Driver`] represents one database product (PostgreSQL, Redis, ...). It is stateless: it
//!   describes itself through a [`DriverDescriptor`] and can open connections.
//! * [`Driver::connect`] takes a [`ConnectionConfig`] (never contains secrets) plus an optional
//!   [`Secret`] and returns a boxed [`Session`], one live connection to a server.
//! * A [`Session`] can ping, report [`ServerInfo`], execute statements, cancel a running query
//!   and lazily browse the server's object tree ([`ObjectPath`] to [`ObjectNode`]s). The tree is
//!   generic so relational schemas, MongoDB collections and Redis keys can all be represented.
//! * Query results are **streamed**: [`Session::execute`] takes a [`QueryRequest`] and pushes
//!   columns, row batches, messages and end-of-statement events into a [`ResultSink`] instead of
//!   returning everything in memory. A sink may return `Err` to stop the driver early. The sink
//!   can forward batches over a Tauri channel, write them to a file, or use [`CollectingSink`]
//!   (or [`execute_collect`]) to gather small results into [`StatementResult`]s.
//! * [`Capabilities`] is a set of boolean flags advertised by every driver. The UI enables or
//!   hides features (ER diagrams, EXPLAIN, transactions, ...) purely from these flags and never
//!   from the database kind.
//! * [`DriverRegistry`] maps [`DbKind`] to registered drivers and merges them with the
//!   [`DriverStatus::Planned`] descriptors of drivers that do not exist yet, so the UI can always
//!   list every supported database.
//!
//! Everything that crosses the IPC boundary derives `Serialize`/`Deserialize` using camelCase
//! field names. [`Value`] uses an explicit tagged representation so that 64-bit integers, decimals
//! and binary data never lose precision in JavaScript.

mod capabilities;
mod config;
mod driver;
mod error;
mod kind;
mod object;
mod query;
mod value;

pub use capabilities::Capabilities;
pub use config::{ConnectionConfig, Environment, Secret, TlsMode};
pub use driver::{
    Driver, DriverDescriptor, DriverRegistry, DriverStatus, ServerInfo, Session, execute_collect,
    planned_descriptors,
};
pub use error::{Error, Result};
pub use kind::{DbFamily, DbKind};
pub use object::{ObjectKind, ObjectNode, ObjectPath, PathSegment};
pub use query::{
    CollectingSink, ColumnMeta, EndOutcome, ExecSummary, QueryRequest, ResultSet, ResultSink,
    StatementEnd, StatementOutcome, StatementResult,
};
pub use value::{Value, ValueKind};
