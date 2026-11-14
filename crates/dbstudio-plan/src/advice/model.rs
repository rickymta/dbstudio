//! Data model of the Query Advisor (see `docs/design/plan-advisor.md`, section 2.1).

use std::collections::BTreeMap;

use dbstudio_core::{ObjectPath, TextRange};
use serde::{Deserialize, Serialize};

use crate::{PlanRule, Severity};

/// One optimisation opportunity with its evidence and concrete actions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanAdvice {
    /// Stable per plan: `"{rule}:{fingerprint-hash8}"`; React key.
    pub id: String,
    /// Rule that produced the advice.
    pub rule: AdviceRule,
    /// How much it matters.
    pub severity: Severity,
    /// How sure the engine is.
    pub confidence: Confidence,
    /// Nodes concerned, most relevant first (empty for statement-only advice).
    #[serde(default)]
    pub node_ids: Vec<u32>,
    /// Table the advice is about (index / statistics advice).
    #[serde(default)]
    pub table: Option<TableRef>,
    /// Values for `advice.rules.<rule>.title` / `.why` (`table`, `columns`, `index`...).
    #[serde(default)]
    pub params: BTreeMap<String, String>,
    /// Figures from the plan shown under "why".
    #[serde(default)]
    pub evidence: Vec<Evidence>,
    /// What to do.
    #[serde(default)]
    pub actions: Vec<AdviceAction>,
    /// Reasons for caution.
    #[serde(default)]
    pub caveats: Vec<Caveat>,
    /// Estimated weight of the problem.
    pub impact: Impact,
    /// Warnings this advice answers.
    #[serde(default)]
    pub from_warnings: Vec<PlanRule>,
    /// Character range in `RawPlan.statement` for statement rules.
    #[serde(default)]
    pub statement_range: Option<TextRange>,
    /// Plan independent identity for compare: rule + table + columns/expression, lower-cased.
    #[serde(default)]
    pub fingerprint: String,
}

/// Advice rules (design sections 4.0 - 4.8). Unknown names read as [`AdviceRule::Unknown`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AdviceRule {
    // 4.0 common
    IndexForFilter,
    IndexResidualFilter,
    IndexForSort,
    NestedLoopInnerScan,
    StaleStatistics,
    SortSpill,
    HashSpill,
    FullScanReview,
    /// An index that already covers the suggestion exists but the plan does not use it.
    ExistingIndexUnused,
    // 4.1 PostgreSQL
    PgBitmapLossy,
    PgHeapFetches,
    PgFunctionOnColumn,
    ImplicitConversion,
    PgParallelNotLaunched,
    PgJitOverhead,
    PgColdCache,
    PgNotInSubplan,
    // 4.2 MySQL / MariaDB
    MysqlPossibleKeysUnused,
    MysqlTempTableGroup,
    MysqlJoinBuffer,
    MysqlIndexMerge,
    // 4.3 SQL Server
    MssqlMissingIndex,
    MssqlKeyLookup,
    MssqlNoJoinPredicate,
    MssqlNoStatistics,
    MssqlExcessiveGrant,
    // 4.4 Oracle
    OracleCartesian,
    OracleIndexSkipScan,
    // 4.5 MongoDB
    MongoCollscan,
    MongoExaminedRatio,
    MongoBlockingSort,
    MongoLookupNoIndex,
    MongoUnanchoredRegex,
    // 4.6 ClickHouse
    ChPrimaryKeyUnused,
    ChPartitionNotPruned,
    ChSkipIndexUseless,
    ChFinal,
    ChJoinRightLarge,
    // 4.7 CockroachDB / TiDB
    CrdbIndexRecommendation,
    CrdbMissingStats,
    TidbPseudoStats,
    // 4.8 static analysis of the statement
    NonSargablePredicate,
    LeadingWildcardLike,
    OrAcrossColumns,
    SelectStar,
    LargeOffset,
    NotInSubquery,
    TypeMismatch,
    /// A rule newer than this build.
    #[serde(other)]
    Unknown,
}

/// How sure the engine is about an advice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Confidence {
    /// Plan figures (and metadata when needed) confirm it.
    High,
    /// Likely, with a stated limit.
    Medium,
    /// A hypothesis worth checking.
    Low,
}

impl Confidence {
    /// One step down, never below `Low`.
    pub fn lowered(self) -> Self {
        match self {
            Confidence::High => Confidence::Medium,
            _ => Confidence::Low,
        }
    }

    pub(crate) fn weight(self) -> f64 {
        match self {
            Confidence::High => 1.0,
            Confidence::Medium => 0.7,
            Confidence::Low => 0.4,
        }
    }
}

/// A table named by the plan, optionally resolved against the catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TableRef {
    /// Database (MySQL schema, Mongo database, ClickHouse database).
    #[serde(default)]
    pub database: Option<String>,
    /// Schema.
    #[serde(default)]
    pub schema: Option<String>,
    /// Table or collection name.
    pub name: String,
    /// Alias in the statement.
    #[serde(default)]
    pub alias: Option<String>,
    /// Resolved by `plan_advise` (enables "Open table").
    #[serde(default)]
    pub path: Option<ObjectPath>,
}

/// One figure shown under "why".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Evidence {
    /// i18n `advice.evidence.<key>`: `rowsRemovedByFilter`...
    pub key: String,
    /// Node the figure comes from.
    #[serde(default)]
    pub node_id: Option<u32>,
    /// Numeric value.
    #[serde(default)]
    pub value: Option<f64>,
    /// Text value.
    #[serde(default)]
    pub text: Option<String>,
    /// Unit of `value`.
    #[serde(default)]
    pub unit: Option<EvidenceUnit>,
    /// `true` = measured (ANALYZE), `false` = estimate.
    #[serde(default)]
    pub actual: bool,
}

/// Unit of an [`Evidence`] value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EvidenceUnit {
    /// Rows or documents.
    Rows,
    /// Milliseconds.
    Ms,
    /// Bytes.
    Bytes,
    /// Buffer blocks.
    Blocks,
    /// Percent (0..=100).
    Pct,
    /// Plain ratio.
    Ratio,
    /// Plain count.
    Count,
    /// Kilobytes.
    Kb,
}

/// What an action does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ActionKind {
    /// `CREATE INDEX` and equivalents.
    CreateIndex,
    /// Refresh optimiser statistics.
    Statistics,
    /// Change a setting for the session.
    SessionSetting,
    /// Vacuum / rebuild.
    Maintenance,
    /// Rewrite the statement.
    Rewrite,
    /// A query hint.
    Hint,
    /// Something to look at or measure.
    Check,
    /// A server wide setting (described only).
    ServerSetting,
}

/// Where an action takes effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ActionScope {
    /// This statement only.
    Query,
    /// The current session.
    Session,
    /// One object (table, index).
    Object,
    /// A whole database.
    Database,
    /// The server.
    Server,
}

/// Locking cost of an action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LockRisk {
    /// No lock worth mentioning.
    None,
    /// Brief exclusive lock.
    ShortLock,
    /// Writers wait.
    BlocksWrites,
    /// Readers and writers wait.
    BlocksReadsWrites,
    /// Rewrites a lot of data in the background.
    HeavyMutation,
}

/// A concrete step: text to copy or insert into the editor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdviceAction {
    /// What it does.
    pub kind: ActionKind,
    /// i18n `advice.actions.<labelKey>`.
    pub label_key: String,
    /// Ready text in the connection's language (SQL, mongo shell).
    #[serde(default)]
    pub sql: Option<String>,
    /// Has placeholders (`:last_id`, `/* columns */`): never "ready to run".
    #[serde(default)]
    pub template: bool,
    /// Where it takes effect.
    pub scope: ActionScope,
    /// Lock risk, when relevant.
    #[serde(default)]
    pub lock: Option<LockRisk>,
    /// Values for the label.
    #[serde(default)]
    pub params: BTreeMap<String, String>,
}

/// A reason for caution, with i18n key `advice.caveats.<key>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Caveat {
    /// Key.
    pub key: String,
    /// The user must verify before acting (shown as "needs verification").
    #[serde(default)]
    pub verify: bool,
    /// Values for the text.
    #[serde(default)]
    pub params: BTreeMap<String, String>,
}

/// What the impact figure is based on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ImpactBasis {
    /// Share of the execution time.
    Time,
    /// Share of the estimated cost.
    Cost,
    /// Share of the rows.
    Rows,
    /// Unknown.
    None,
}

/// Weight of an advice.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Impact {
    /// What `share` is a share of.
    pub basis: ImpactBasis,
    /// Share of the whole statement attributed to the nodes (0..=1); `None` when unknown.
    #[serde(default)]
    pub share: Option<f64>,
    /// Ranking score, stored so UI and tests agree.
    pub score: f64,
}

/// Role of a condition piece, filled by the parsers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PredicateRole {
    /// Index condition (seek / access predicate).
    IndexCond,
    /// Residual filter.
    Filter,
    /// Join condition (hash / merge / equi).
    JoinCond,
    /// Extra join filter.
    JoinFilter,
    /// Bitmap recheck.
    Recheck,
    /// Sort key.
    SortKey,
    /// Group key.
    GroupKey,
    /// Anything else.
    Other,
}

/// One condition of a node with its role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodePredicate {
    /// Role.
    pub role: PredicateRole,
    /// Condition as the source printed it.
    pub text: String,
}

/// State of the catalog lookup behind an advice report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MetadataState {
    /// Tier 1: plan and statement only.
    #[default]
    NotRequested,
    /// Every needed table was read.
    Checked,
    /// Some tables could not be read.
    Partial,
    /// Nothing could be read.
    Unavailable,
    /// Turned off by the setting.
    Disabled,
}

/// Why a table could not be read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TableFailure {
    /// Table name as in the plan.
    pub table: String,
    /// `timeout` | `notFound` | `ambiguous` | `unsupported` | `error`.
    pub reason: String,
    /// Detail for the tooltip.
    #[serde(default)]
    pub message: String,
}

/// Outcome of the catalog lookup.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AdviceMetadata {
    /// Overall state.
    pub state: MetadataState,
    /// Tables read (with `path`).
    pub checked: Vec<TableRef>,
    /// Tables that failed.
    pub failed: Vec<TableFailure>,
    /// Tables skipped because of the limit.
    pub skipped: u32,
    /// Time spent.
    pub elapsed_ms: u64,
    /// Why the catalog was not consulted although it was wanted: `planWithoutConnection` (a plan
    /// from a file or the history), `dialectMismatch` (the plan belongs to another product than
    /// the connection).
    pub reason: Option<String>,
}

/// Result of `plan_advise`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdviceReport {
    /// Advice, ranked.
    pub advice: Vec<PlanAdvice>,
    /// What the catalog lookup did.
    pub metadata: AdviceMetadata,
}
