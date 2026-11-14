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

mod browse;
mod capabilities;
mod config;
pub mod debug;
mod driver;
mod edit;
mod error;
pub mod fallback;
mod job;
pub mod keyvalue;
mod kind;
mod lease;
pub mod lease_kit;
mod metadata;
mod model;
mod object;
mod plan;
mod query;
mod script;
mod ssh;
#[cfg(feature = "testing")]
pub mod testing;
mod value;

pub use browse::{
    ColumnFilter, FilterOp, ObjectSearchRequest, SortKey, TableCountRequest, TableDataRequest,
};
pub use capabilities::Capabilities;
pub use config::{ConnectionConfig, Environment, Secret, TlsConfig, TlsMode};
pub use driver::{
    Driver, DriverDescriptor, DriverRegistry, DriverStatus, ServerInfo, Session, UnavailableReason,
    execute_collect, planned_descriptors,
};
pub use edit::{
    ApplyResult, CellEdit, ChangeSet, DocInput, DocumentChange, DocumentChangeSet, EditNote,
    EditValue, Editability, FieldEdit, FieldOriginal, KeyChange, KeyChangeSet, NotEditableReason,
    PutMode, RowChange, RowChangeSet,
};
pub use error::{Error, Result};
pub use job::{
    CancelToken, JobError, JobEvent, JobId, JobInfo, JobKind, JobProgress, JobReporter, JobStatus,
};
pub use keyvalue::{
    BINARY_KEY_PREFIX, KeyData, KeyEntries, KeyReadRequest, KeyRecord, KeyScanPage, KeyScanRequest,
    KeyValuePage, StreamEntry, decode_key_name, encode_key_name,
};
pub use kind::{DbFamily, DbKind};
pub use lease::{
    IsolationLevel, Lease, LeaseCloseReport, LeaseContext, LeaseOptions, LeaseState, TxOptions,
    TxStatus,
};
pub use lease_kit::{ActorLease, Canceller, LeaseBackend};
pub use metadata::{ColumnDetail, ForeignKeyDetail, IndexDetail, TableDetails};
pub use model::{
    BulkInsertRequest, BulkInsertResult, DdlItem, DdlLanguage, DdlOptions, DdlRequest, DdlSection,
    ModelDetail, OnConflict, Relation, SchemaModel, SchemaModelRequest, TableModel,
    TableStreamRequest,
};
pub use object::{
    ChildrenPage, ChildrenPageRequest, ObjectKind, ObjectNode, ObjectPath, PathSegment, folder,
};
pub use plan::{ExplainOptions, ExplainRequest, PlanFormat, RawPlan};
pub use query::{
    CollectingSink, ColumnMeta, EndOutcome, ExecSummary, QueryEvent, QueryRequest, ResultSet,
    ResultSink, StatementEnd, StatementOutcome, StatementResult, StatementStart,
};
pub use script::{
    ConfirmationGrant, ConfirmationRequest, DestructiveReason, DestructiveStatement,
    ExecuteRequest, ExecuteResult, ParamInput, ParamRef, ParamStyle, ParamValue, ScriptAnalysis,
    ScriptDialect, ScriptProblem, StatementClass, StatementInfo, TextRange, TxControl,
};
pub use ssh::{SshAuth, SshConfig};
pub use value::{ExtValue, Value, ValueKind};
