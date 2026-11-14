//! Safety guard of the command layer (design 1.5): confirmation tokens and the five-step flow
//! `bind -> analyze -> policy -> check -> verdict`.
//!
//! [`dbstudio_sql::guard`] only decides; this module owns the state (single-use tokens with a
//! digest of what was shown to the user) and maps the verdict to errors. The same token store
//! serves `restore_start`, `import_start` (overwrite) and `keys_delete_pattern` (digest of the
//! JSON spec through [`ConfirmTokens::issue`] / [`ConfirmTokens::consume`]). Package B owns the
//! flow, the other packages only call it.

#![allow(dead_code)] // scaffold: partly consumed by later packages

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use dbstudio_core::{
    ConfirmationGrant, ConfirmationRequest, ConnectionConfig, DbKind, DestructiveReason,
    DestructiveStatement, Environment, Error, ParamValue, Result, ScriptAnalysis, ScriptDialect,
    StatementClass, TextRange,
};
use dbstudio_sql::{BoundScript, OffsetMap, Policy, Verdict};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// How long a confirmation token stays valid.
pub const TOKEN_TTL: Duration = Duration::from_secs(120);

/// Source of time, replaceable in tests.
pub trait Clock: Send + Sync + std::fmt::Debug {
    /// Current instant.
    fn now(&self) -> Instant;
}

/// The real clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

#[derive(Debug)]
struct TokenEntry {
    digest: Vec<u8>,
    target: String,
    confirm_text: String,
    expires: Instant,
}

/// Why a presented grant was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    /// No such token (never issued, already used, or purged).
    Unknown,
    /// The token expired.
    Expired,
    /// The token was issued for another target or other text.
    DigestMismatch,
    /// `typed` is not exactly the required confirmation text.
    WrongText,
}

/// `sha256(target || 0x00 || payload)`.
fn digest(target: &str, payload: &[u8]) -> Vec<u8> {
    let mut h = Sha256::new();
    h.update(target.as_bytes());
    h.update([0u8]);
    h.update(payload);
    h.finalize().as_slice().to_vec()
}

/// Store of pending confirmations. A token is single use: every presentation removes it,
/// successful or not.
#[derive(Debug)]
pub struct ConfirmTokens {
    ttl: Duration,
    clock: Arc<dyn Clock>,
    tokens: Mutex<HashMap<String, TokenEntry>>,
}

impl Default for ConfirmTokens {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfirmTokens {
    /// Tokens valid for [`TOKEN_TTL`].
    pub fn new() -> Self {
        Self::with_clock(TOKEN_TTL, Arc::new(SystemClock))
    }

    /// Tokens valid for `ttl` measured with `clock`.
    pub fn with_clock(ttl: Duration, clock: Arc<dyn Clock>) -> Self {
        ConfirmTokens {
            ttl,
            clock,
            tokens: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, TokenEntry>> {
        self.tokens.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Number of pending tokens (after dropping expired ones).
    pub fn pending(&self) -> usize {
        let now = self.clock.now();
        let mut map = self.lock();
        map.retain(|_, e| e.expires > now);
        map.len()
    }

    /// Issue a token bound to `sha256(target || payload)` and to `confirm_text`; returns the
    /// 32 hex character token and its lifetime in milliseconds.
    pub fn issue(&self, target: &str, payload: &[u8], confirm_text: &str) -> (String, u64) {
        let now = self.clock.now();
        let token = Uuid::new_v4().simple().to_string();
        let mut map = self.lock();
        map.retain(|_, e| e.expires > now);
        map.insert(
            token.clone(),
            TokenEntry {
                digest: digest(target, payload),
                target: target.to_string(),
                confirm_text: confirm_text.to_string(),
                expires: now + self.ttl,
            },
        );
        (
            token,
            u64::try_from(self.ttl.as_millis()).unwrap_or(u64::MAX),
        )
    }

    /// Check `grant` against what is being run now and remove the token either way.
    pub fn consume(
        &self,
        grant: &ConfirmationGrant,
        target: &str,
        payload: &[u8],
    ) -> std::result::Result<(), Rejection> {
        let now = self.clock.now();
        let entry = {
            let mut map = self.lock();
            let entry = map.remove(&grant.token);
            // Purge the rest of the expired ones (the presented token was handled above).
            map.retain(|_, e| e.expires > now);
            entry
        };
        let entry = entry.ok_or(Rejection::Unknown)?;
        if entry.expires <= now {
            return Err(Rejection::Expired);
        }
        if entry.target != target || entry.digest != digest(target, payload) {
            return Err(Rejection::DigestMismatch);
        }
        // Exact, case-sensitive comparison: the user must type the database name as it is.
        if grant.typed != entry.confirm_text {
            return Err(Rejection::WrongText);
        }
        Ok(())
    }
}

/// Text the user must type to confirm destructive statements: the database the statement runs
/// in (`lease_database` when the lease switched context, else the connection's database), `db<N>`
/// for Redis, else the connection name.
pub fn confirm_text_for(config: &ConnectionConfig, lease_database: Option<&str>) -> String {
    let database = lease_database
        .filter(|d| !d.is_empty())
        .or(config.database.as_deref().filter(|d| !d.is_empty()));
    if config.kind == DbKind::Redis {
        let n = database
            .map(|d| d.strip_prefix("db").unwrap_or(d))
            .unwrap_or("0");
        return format!("db{n}");
    }
    database.map_or_else(|| config.name.clone(), str::to_string)
}

/// What the guard needs to know about the connection and the request.
#[derive(Debug, Clone, Copy)]
pub struct GuardInput<'a> {
    /// Language of the session.
    pub dialect: ScriptDialect,
    /// Text as typed by the user (before parameter binding).
    pub text: &'a str,
    /// Parameter values to substitute.
    pub params: &'a [ParamValue],
    /// Connection is read-only.
    pub read_only: bool,
    /// Connection is a production environment.
    pub production: bool,
    /// Text the user must type to confirm.
    pub confirm_text: &'a str,
    /// Grant answering an earlier `ConfirmationRequired`.
    pub confirm: Option<&'a ConfirmationGrant>,
    /// Identity of what runs (`session:<id>` / `lease:<id>`); part of the token digest.
    pub target_id: &'a str,
    /// Where the text runs (effective database / schema); part of the token digest, so a token
    /// issued in one context cannot be spent after the context changed.
    pub context: &'a str,
}

/// A script that passed the guard.
#[derive(Debug)]
pub struct Guarded {
    /// Text to send to the server and the map back to the original text.
    pub bound: BoundScript,
    /// Analysis of the bound text.
    pub analysis: ScriptAnalysis,
}

/// `read_only` / `production` of a connection.
pub fn policy_flags(config: &ConnectionConfig) -> (bool, bool) {
    (
        config.read_only,
        config.environment == Some(Environment::Production),
    )
}

fn map_range(map: &OffsetMap, r: TextRange) -> TextRange {
    TextRange {
        start: map.to_original(r.start),
        end: map.to_original(r.end),
    }
}

// ---------------------------------------------------------------------------------------------
// Defence in depth. `dbstudio_sql` decides from its parse; this layer works on plain tokens and
// is deliberately blunt: it never needs the parser to be right.

#[derive(Debug, Clone)]
struct Tok {
    /// Upper-cased word, or `;`.
    word: String,
    /// Character offsets in the scanned text.
    start: u32,
    end: u32,
}

/// Words and statement separators of `text`, without string literals, quoted identifiers and
/// comments (so a keyword inside a string never counts and a hidden one cannot be missed).
fn scan_tokens(dialect: ScriptDialect, text: &str) -> Vec<Tok> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mysql = matches!(dialect, ScriptDialect::MySql { .. });
    let clickhouse = matches!(dialect, ScriptDialect::ClickHouse);
    let cql = matches!(dialect, ScriptDialect::Cql);
    let hash_comment = mysql || clickhouse;
    let ansi_quotes = matches!(
        dialect,
        ScriptDialect::MySql {
            ansi_quotes: true,
            ..
        }
    );
    let backslash = match dialect {
        ScriptDialect::MySql {
            backslash_escapes, ..
        } => backslash_escapes,
        ScriptDialect::ClickHouse => true,
        _ => false,
    };
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        let c = chars[i];
        match c {
            '\'' | '"' | '`' => {
                i += 1;
                while i < n {
                    if chars[i] == '\\'
                        && backslash
                        && (c != '`' || clickhouse)
                        && !(c == '"' && ansi_quotes)
                    {
                        i += 2;
                        continue;
                    }
                    if chars[i] == c {
                        if i + 1 < n && chars[i + 1] == c {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                i += 1;
            }
            '-' if i + 1 < n && chars[i + 1] == '-' => {
                while i < n && chars[i] != '\n' {
                    i += 1;
                }
            }
            '#' if hash_comment => {
                while i < n && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if cql && i + 1 < n && chars[i + 1] == '/' => {
                while i < n && chars[i] != '\n' {
                    i += 1;
                }
            }
            // MySQL / MariaDB / TiDB executable comments (`/*! .. */`, `/*M! .. */`,
            // `/*T![feature] .. */`) are code: skip only the opener, scan the content.
            '/' if mysql
                && i + 2 < n
                && chars[i + 1] == '*'
                && (chars[i + 2] == '!'
                    || (matches!(chars[i + 2], 'M' | 'T') && i + 3 < n && chars[i + 3] == '!')) =>
            {
                i += if chars[i + 2] == '!' { 3 } else { 4 };
            }
            '*' if mysql && i + 1 < n && chars[i + 1] == '/' => i += 2,
            '/' if i + 1 < n && chars[i + 1] == '*' => {
                // ClickHouse (like PostgreSQL) nests block comments: `/* /* */ DROP */`.
                let nests = clickhouse || matches!(dialect, ScriptDialect::Postgres);
                let mut depth = 1usize;
                i += 2;
                while i + 1 < n {
                    if nests && chars[i] == '/' && chars[i + 1] == '*' {
                        depth += 1;
                        i += 2;
                    } else if chars[i] == '*' && chars[i + 1] == '/' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                i += 2;
            }
            '$' if matches!(
                dialect,
                ScriptDialect::Postgres | ScriptDialect::ClickHouse | ScriptDialect::Cql
            ) =>
            {
                // Dollar quote `$tag$ ... $tag$` (CQL only knows the empty tag `$$`).
                let mut j = i + 1;
                while j < n && (chars[j].is_alphanumeric() || chars[j] == '_') {
                    j += 1;
                }
                if j < n && chars[j] == '$' && (!cql || j == i + 1) {
                    let tag: String = chars[i..=j].iter().collect();
                    let mut k = j + 1;
                    let tag_chars: Vec<char> = tag.chars().collect();
                    while k + tag_chars.len() <= n && chars[k..k + tag_chars.len()] != tag_chars[..]
                    {
                        k += 1;
                    }
                    i = (k + tag_chars.len()).min(n);
                } else {
                    i += 1;
                }
            }
            ';' => {
                out.push(Tok {
                    word: ";".into(),
                    start: i as u32,
                    end: i as u32 + 1,
                });
                i += 1;
            }
            c if c.is_alphabetic() || c == '_' => {
                let start = i;
                while i < n && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '$') {
                    i += 1;
                }
                out.push(Tok {
                    word: chars[start..i].iter().collect::<String>().to_uppercase(),
                    start: start as u32,
                    end: i as u32,
                });
            }
            _ => i += 1,
        }
    }
    out
}

/// Tokens grouped by statement (split at `;`).
fn token_statements(tokens: &[Tok]) -> Vec<&[Tok]> {
    tokens
        .split(|t| t.word == ";")
        .filter(|s| !s.is_empty())
        .collect()
}

/// A read-only connection must stay read-only: refuse the statements that switch it off,
/// whatever class the analyzer gave them (`SET` and `BEGIN` are otherwise allowed).
fn read_only_defense(dialect: ScriptDialect, text: &str, map: &OffsetMap) -> Result<()> {
    let tokens = scan_tokens(dialect, text);
    for stmt in token_statements(&tokens) {
        let words: Vec<&str> = stmt.iter().map(|t| t.word.as_str()).collect();
        let has = |w: &str| words.contains(&w);
        let offender = if words.windows(2).any(|p| p == ["READ", "WRITE"]) {
            Some("READ WRITE")
        } else if has("SET_CONFIG") {
            Some("set_config")
        } else if has("SET")
            && words
                .iter()
                .any(|w| w.ends_with("_READ_ONLY") || *w == "TX_READ_ONLY")
        {
            Some("changing the read-only setting")
        } else if words.first() == Some(&"SET") && has("SQL_MODE") {
            Some("changing sql_mode")
        } else {
            None
        };
        if let Some(what) = offender {
            return Err(Error::ReadOnlyViolation {
                message: format!("{what} is not allowed on a read-only connection"),
                position: Some(map.to_original(stmt[0].start)),
            });
        }
    }
    Ok(())
}

/// Production fallback for statements the analyzer could not classify (`Unknown`,
/// `Procedural`): keywords that destroy data are enough to ask for a confirmation
/// (design risk R5: be conservative).
fn production_fallback(
    dialect: ScriptDialect,
    analysis: &ScriptAnalysis,
    text: &str,
) -> Vec<DestructiveStatement> {
    let tokens = scan_tokens(dialect, text);
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    for (index, stmt) in token_statements(&tokens).into_iter().enumerate() {
        let start = stmt[0].start;
        let analysed = analysis
            .statements
            .iter()
            .find(|s| s.range.start <= start && start < s.range.end.max(s.range.start + 1))
            .map(|s| s.class);
        if !matches!(
            analysed,
            None | Some(StatementClass::Unknown | StatementClass::Procedural)
        ) {
            continue;
        }
        let words: Vec<&str> = stmt.iter().map(|t| t.word.as_str()).collect();
        let reason = if words.contains(&"DROP") {
            Some(DestructiveReason::Drop)
        } else if words.contains(&"TRUNCATE") {
            Some(DestructiveReason::Truncate)
        } else if words.contains(&"ALTER")
            || words.contains(&"DELETE")
            || (words.contains(&"UPDATE") && !words.contains(&"WHERE"))
        {
            // The statement was not understood, so the kind of damage is a guess: say so.
            Some(DestructiveReason::KeywordScan)
        } else {
            None
        };
        if let Some(reason) = reason {
            let end = stmt[stmt.len() - 1].end;
            out.push(DestructiveStatement {
                index: u32::try_from(index).unwrap_or(u32::MAX),
                range: TextRange { start, end },
                text: chars[start as usize..end as usize]
                    .iter()
                    .take(2000)
                    .collect(),
                reason,
            });
        }
    }
    out
}

/// Why a MySQL script cannot be split reliably: its statement boundaries differ between the two
/// settings of `NO_BACKSLASH_ESCAPES` (guard and server could then disagree on what runs), or,
/// when the script itself changes `sql_mode`, between the combinations of `NO_BACKSLASH_ESCAPES`
/// and `ANSI_QUOTES` (the mode in force changes in the middle of the script).
fn split_ambiguity(dialect: ScriptDialect, text: &str) -> Option<String> {
    let ScriptDialect::MySql {
        mariadb,
        backslash_escapes,
        ansi_quotes,
    } = dialect
    else {
        return None;
    };
    let with = |backslash_escapes, ansi_quotes| ScriptDialect::MySql {
        mariadb,
        backslash_escapes,
        ansi_quotes,
    };
    let base = dbstudio_sql::split(dialect, text);
    if base != dbstudio_sql::split(with(!backslash_escapes, ansi_quotes), text) {
        return Some(
            "the statement boundaries depend on the NO_BACKSLASH_ESCAPES sql_mode (a quote escaped \
             with a backslash); write the literal with \'\' so the script means the same in both modes"
                .to_string(),
        );
    }
    // `PREPARE ... FROM '...'` / `EXECUTE` can change the mode from inside a literal, so they
    // count like an explicit `SET sql_mode`.
    let changes_mode = scan_tokens(dialect, text)
        .iter()
        .any(|t| matches!(t.word.as_str(), "SQL_MODE" | "PREPARE" | "EXECUTE"));
    if changes_mode {
        for (b, a) in [(true, false), (true, true), (false, false), (false, true)] {
            if (b, a) != (backslash_escapes, ansi_quotes)
                && base != dbstudio_sql::split(with(b, a), text)
            {
                return Some(
                    "the script changes sql_mode and its statement boundaries differ between the \
                     ANSI_QUOTES / NO_BACKSLASH_ESCAPES modes; split it into separate runs or avoid \
                     double quotes and backslashes in its literals"
                        .to_string(),
                );
            }
        }
    }
    None
}

/// What a confirmation token is bound to besides the target: where the text runs, the databases
/// its statements act on, what the user has to type, and the text itself.
fn token_payload(
    input: &GuardInput<'_>,
    bound_text: &str,
    confirm_text: &str,
    databases: &[Option<String>],
) -> Vec<u8> {
    let dbs: Vec<&str> = databases
        .iter()
        .map(|d| d.as_deref().unwrap_or("-"))
        .collect();
    format!(
        "{}\0{}\0{}\0{}",
        input.context,
        confirm_text,
        dbs.join("\u{1}"),
        bound_text
    )
    .into_bytes()
}

/// Steps 3-4 of the flow on an already analysed script: policy, verdict, token handling.
pub fn evaluate(
    tokens: &ConfirmTokens,
    input: &GuardInput<'_>,
    bound: &BoundScript,
    analysis: &ScriptAnalysis,
) -> Result<()> {
    let policy = Policy {
        read_only: input.read_only,
        production: input.production,
        confirm_text: input.confirm_text.to_string(),
    };
    if input.read_only {
        read_only_defense(input.dialect, &bound.text, &bound.map)?;
    }
    let mut verdict = dbstudio_sql::guard::check_with_text(&policy, analysis, &bound.text);
    if input.production && matches!(verdict, Verdict::Allow) {
        let extra = production_fallback(input.dialect, analysis, &bound.text);
        if !extra.is_empty() {
            verdict = Verdict::Confirm(extra);
        }
    }
    match verdict {
        Verdict::Allow => Ok(()),
        Verdict::Deny(Error::ReadOnlyViolation { message, position }) => {
            Err(Error::ReadOnlyViolation {
                message,
                position: position.map(|p| bound.map.to_original(p)),
            })
        }
        Verdict::Deny(other) => Err(other),
        Verdict::Confirm(statements) => {
            // The user types the database the destructive statements really act on (a `USE`
            // earlier in the script moves them), not just the one the tab started in.
            let databases = dbstudio_sql::statement_databases(input.dialect, &bound.text, analysis);
            let confirm_text =
                dbstudio_sql::confirm_text_for(input.confirm_text, &databases, &statements);
            let payload = token_payload(input, &bound.text, &confirm_text, &databases);
            if let Some(grant) = input.confirm {
                match tokens.consume(grant, input.target_id, &payload) {
                    Ok(()) => return Ok(()),
                    Err(why) => tracing::debug!(?why, "confirmation refused"),
                }
            }
            let (token, expires_in_ms) = tokens.issue(input.target_id, &payload, &confirm_text);
            let statements: Vec<DestructiveStatement> = statements
                .into_iter()
                .map(|s| DestructiveStatement {
                    range: map_range(&bound.map, s.range),
                    ..s
                })
                .collect();
            Err(Error::ConfirmationRequired(Box::new(ConfirmationRequest {
                token,
                confirm_text,
                statements,
                expires_in_ms,
            })))
        }
    }
}

/// Analysis step of the flow (an indirection so command tests can substitute a keyword
/// classifier while `dbstudio_sql::analyze` is still the scaffold).
fn analyze(dialect: ScriptDialect, text: &str) -> ScriptAnalysis {
    #[cfg(test)]
    if let Some(f) = test_support::OVERRIDE.with(std::cell::Cell::get) {
        return f(dialect, text);
    }
    dbstudio_sql::analyze(dialect, text)
}

/// The five-step flow for a script: bind, analyze, policy, check, verdict.
pub fn run_guard(tokens: &ConfirmTokens, input: &GuardInput<'_>) -> Result<Guarded> {
    let bound = dbstudio_sql::bind(input.dialect, input.text, input.params)?;
    if let Some(message) = split_ambiguity(input.dialect, &bound.text) {
        return Err(Error::Query {
            message,
            position: None,
        });
    }
    let analysis = analyze(input.dialect, &bound.text);
    evaluate(tokens, input, &bound, &analysis)?;
    Ok(Guarded { bound, analysis })
}

/// Read-only check without text (changes preview/apply, which build their own statements):
/// everything that writes is refused when the connection is read-only.
pub fn ensure_writable(config: &ConnectionConfig, what: &str) -> Result<()> {
    if config.read_only {
        return Err(Error::ReadOnlyViolation {
            message: format!("{what} is not allowed on a read-only connection"),
            position: None,
        });
    }
    Ok(())
}

/// Helpers shared by the command tests.
#[cfg(test)]
pub(crate) mod test_support {
    use std::cell::Cell;
    use std::sync::atomic::{AtomicU64, Ordering};

    use dbstudio_core::{DestructiveReason, StatementClass, StatementInfo, TxControl};

    use super::*;

    type AnalyzeFn = fn(ScriptDialect, &str) -> ScriptAnalysis;

    thread_local! {
        pub(super) static OVERRIDE: Cell<Option<AnalyzeFn>> = const { Cell::new(None) };
    }

    /// Restores the real analyzer when dropped.
    pub struct KeywordClassifier;

    impl Drop for KeywordClassifier {
        fn drop(&mut self) {
            OVERRIDE.with(|c| c.set(None));
        }
    }

    /// Classify statements by their first keyword on the current thread (use a current-thread
    /// runtime in the test). Only for tests.
    pub fn use_keyword_classifier() -> KeywordClassifier {
        OVERRIDE.with(|c| c.set(Some(keyword_analyze)));
        KeywordClassifier
    }

    /// Whether `dbstudio_sql::analyze` already classifies statements (package A finished).
    pub fn real_classifier_available() -> bool {
        dbstudio_sql::analyze(ScriptDialect::Postgres, "select 1")
            .statements
            .first()
            .is_some_and(|s| s.class == StatementClass::Read)
    }

    /// A minimal classifier: verb decides the class, `DROP`/`TRUNCATE` and `DELETE`/`UPDATE`
    /// without `WHERE` are destructive.
    pub fn keyword_analyze(dialect: ScriptDialect, text: &str) -> ScriptAnalysis {
        let chars: Vec<char> = text.chars().collect();
        let statements = dbstudio_sql::split(ScriptDialect::Postgres, text)
            .into_iter()
            .enumerate()
            .map(|(i, range)| {
                let sql: String = chars[range.start as usize..range.end as usize]
                    .iter()
                    .collect::<String>()
                    .to_ascii_uppercase();
                let verb = sql.split_whitespace().next().unwrap_or("").to_string();
                let has_where = sql.contains(" WHERE ");
                let (class, destructive) = match verb.as_str() {
                    "SELECT" | "SHOW" | "WITH" | "VALUES" => (StatementClass::Read, None),
                    "INSERT" => (StatementClass::Write, None),
                    "UPDATE" => (
                        StatementClass::Write,
                        (!has_where).then_some(DestructiveReason::UpdateWithoutWhere),
                    ),
                    "DELETE" => (
                        StatementClass::Write,
                        (!has_where).then_some(DestructiveReason::DeleteWithoutWhere),
                    ),
                    "DROP" => (StatementClass::Ddl, Some(DestructiveReason::Drop)),
                    "TRUNCATE" => (StatementClass::Ddl, Some(DestructiveReason::Truncate)),
                    "CREATE" | "ALTER" => (StatementClass::Ddl, None),
                    "BEGIN" => (
                        StatementClass::Transaction {
                            op: TxControl::Begin,
                        },
                        None,
                    ),
                    "COMMIT" => (
                        StatementClass::Transaction {
                            op: TxControl::Commit,
                        },
                        None,
                    ),
                    "ROLLBACK" => (
                        StatementClass::Transaction {
                            op: TxControl::Rollback,
                        },
                        None,
                    ),
                    "SET" | "USE" => (StatementClass::Session, None),
                    _ => (StatementClass::Unknown, None),
                };
                StatementInfo {
                    index: i as u32,
                    range,
                    class,
                    verb,
                    destructive,
                    implicit_commit: false,
                    targets: Vec::new(),
                }
            })
            .collect();
        ScriptAnalysis {
            dialect,
            statements,
            params: Vec::new(),
            problems: Vec::new(),
        }
    }

    /// Clock that only moves when told to.
    #[derive(Debug)]
    pub struct TestClock {
        base: Instant,
        offset_ms: AtomicU64,
    }

    impl TestClock {
        pub fn new() -> Arc<Self> {
            Arc::new(TestClock {
                base: Instant::now(),
                offset_ms: AtomicU64::new(0),
            })
        }
        pub fn advance(&self, d: Duration) {
            self.offset_ms
                .fetch_add(u64::try_from(d.as_millis()).unwrap(), Ordering::SeqCst);
        }
    }

    impl Clock for TestClock {
        fn now(&self) -> Instant {
            self.base + Duration::from_millis(self.offset_ms.load(Ordering::SeqCst))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use dbstudio_core::{DestructiveReason, StatementClass, StatementInfo, TxControl};
    use dbstudio_sql::params::Edit;

    use super::*;

    /// Clock that only moves when told to.
    #[derive(Debug, Default)]
    struct TestClock {
        base: Option<Instant>,
        offset_ms: AtomicU64,
    }

    impl TestClock {
        fn new() -> Arc<Self> {
            Arc::new(TestClock {
                base: Some(Instant::now()),
                offset_ms: AtomicU64::new(0),
            })
        }
        fn advance(&self, d: Duration) {
            self.offset_ms
                .fetch_add(u64::try_from(d.as_millis()).unwrap(), Ordering::SeqCst);
        }
    }

    impl Clock for TestClock {
        fn now(&self) -> Instant {
            self.base.unwrap() + Duration::from_millis(self.offset_ms.load(Ordering::SeqCst))
        }
    }

    fn grant(token: &str, typed: &str) -> ConfirmationGrant {
        ConfirmationGrant {
            token: token.into(),
            typed: typed.into(),
        }
    }

    fn tokens() -> (ConfirmTokens, Arc<TestClock>) {
        let clock = TestClock::new();
        (ConfirmTokens::with_clock(TOKEN_TTL, clock.clone()), clock)
    }

    #[test]
    fn token_is_32_hex_and_valid_once() {
        let (t, _) = tokens();
        let (token, ms) = t.issue("session:1", b"drop table x", "app");
        assert_eq!(token.len(), 32);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(ms, 120_000);
        assert_eq!(t.pending(), 1);
        assert_eq!(
            t.consume(&grant(&token, "app"), "session:1", b"drop table x"),
            Ok(())
        );
        // Reuse is refused.
        assert_eq!(
            t.consume(&grant(&token, "app"), "session:1", b"drop table x"),
            Err(Rejection::Unknown)
        );
        assert_eq!(t.pending(), 0);
    }

    #[test]
    fn wrong_typed_text_is_refused_case_sensitively_and_burns_the_token() {
        let (t, _) = tokens();
        for typed in ["App", "app ", "", "ap"] {
            let (token, _) = t.issue("s", b"x", "app");
            assert_eq!(
                t.consume(&grant(&token, typed), "s", b"x"),
                Err(Rejection::WrongText),
                "{typed:?}"
            );
            assert_eq!(
                t.consume(&grant(&token, "app"), "s", b"x"),
                Err(Rejection::Unknown)
            );
        }
    }

    #[test]
    fn expired_token_is_refused() {
        let (t, clock) = tokens();
        let (token, _) = t.issue("s", b"x", "app");
        clock.advance(Duration::from_secs(119));
        assert_eq!(t.pending(), 1);
        clock.advance(Duration::from_secs(1));
        assert_eq!(
            t.consume(&grant(&token, "app"), "s", b"x"),
            Err(Rejection::Expired)
        );
    }

    #[test]
    fn expired_tokens_are_purged() {
        let (t, clock) = tokens();
        t.issue("s", b"x", "app");
        t.issue("s", b"y", "app");
        clock.advance(TOKEN_TTL + Duration::from_secs(1));
        assert_eq!(t.pending(), 0);
    }

    #[test]
    fn digest_mismatch_on_other_text_or_other_target() {
        let (t, _) = tokens();
        let (token, _) = t.issue("session:1", b"drop table x", "app");
        assert_eq!(
            t.consume(&grant(&token, "app"), "session:1", b"drop table y"),
            Err(Rejection::DigestMismatch)
        );
        let (token, _) = t.issue("session:1", b"drop table x", "app");
        assert_eq!(
            t.consume(&grant(&token, "app"), "session:2", b"drop table x"),
            Err(Rejection::DigestMismatch)
        );
        // A made-up token is unknown.
        assert_eq!(
            t.consume(&grant("0123456789abcdef0123456789abcdef", "app"), "s", b"x"),
            Err(Rejection::Unknown)
        );
    }

    #[test]
    fn digest_separates_target_from_payload() {
        assert_ne!(digest("ab", b"c"), digest("a", b"bc"));
    }

    // ---- guard flow with hand-built analyses ------------------------------------------------

    fn stmt(
        index: u32,
        start: u32,
        end: u32,
        class: StatementClass,
        destructive: Option<DestructiveReason>,
    ) -> StatementInfo {
        StatementInfo {
            index,
            range: TextRange { start, end },
            class,
            verb: "X".into(),
            destructive,
            implicit_commit: false,
            targets: vec![],
        }
    }

    fn analysis(statements: Vec<StatementInfo>) -> ScriptAnalysis {
        ScriptAnalysis {
            dialect: ScriptDialect::Postgres,
            statements,
            params: vec![],
            problems: vec![],
        }
    }

    fn bound(text: &str) -> BoundScript {
        BoundScript {
            text: text.into(),
            map: OffsetMap::identity(),
        }
    }

    fn input<'a>(
        text: &'a str,
        read_only: bool,
        production: bool,
        confirm: Option<&'a ConfirmationGrant>,
    ) -> GuardInput<'a> {
        GuardInput {
            dialect: ScriptDialect::Postgres,
            text,
            params: &[],
            read_only,
            production,
            confirm_text: "app",
            confirm,
            target_id: "session:1",
            context: "ctx",
        }
    }

    #[test]
    fn read_only_denies_writes_with_position_in_the_original_text() {
        let (t, _) = tokens();
        // original "select :a; delete from t" -> bound "select 'xyz'; delete from t"
        let b = BoundScript {
            text: "select 'xyz'; delete from t".into(),
            map: OffsetMap {
                edits: vec![Edit {
                    bound_start: 7,
                    bound_end: 12,
                    orig_start: 7,
                    orig_end: 9,
                }],
            },
        };
        let a = analysis(vec![
            stmt(0, 0, 12, StatementClass::Read, None),
            stmt(1, 14, 27, StatementClass::Write, None),
        ]);
        let err = evaluate(
            &t,
            &input("select :a; delete from t", true, false, None),
            &b,
            &a,
        )
        .unwrap_err();
        match err {
            Error::ReadOnlyViolation { position, .. } => assert_eq!(position, Some(11)),
            other => panic!("{other:?}"),
        }
        // Reads pass.
        let reads = analysis(vec![stmt(0, 0, 8, StatementClass::Read, None)]);
        evaluate(
            &t,
            &input("select 1", true, false, None),
            &bound("select 1"),
            &reads,
        )
        .unwrap();
        assert_eq!(t.pending(), 0);
    }

    #[test]
    fn production_destructive_needs_confirmation_then_runs_once() {
        let (t, _) = tokens();
        let text = "delete from t";
        let a = analysis(vec![stmt(
            0,
            0,
            13,
            StatementClass::Write,
            Some(DestructiveReason::DeleteWithoutWhere),
        )]);
        let b = bound(text);
        let err = evaluate(&t, &input(text, false, true, None), &b, &a).unwrap_err();
        let req = match err {
            Error::ConfirmationRequired(req) => req,
            other => panic!("{other:?}"),
        };
        assert_eq!(req.confirm_text, "app");
        assert_eq!(req.expires_in_ms, 120_000);
        assert_eq!(req.statements.len(), 1);
        assert_eq!(req.statements[0].text, "delete from t");
        assert_eq!(
            req.statements[0].reason,
            DestructiveReason::DeleteWithoutWhere
        );
        assert_eq!(t.pending(), 1);

        // Wrong typed text: refused, a fresh confirmation is requested.
        let bad = grant(&req.token, "other");
        assert!(matches!(
            evaluate(&t, &input(text, false, true, Some(&bad)), &b, &a),
            Err(Error::ConfirmationRequired(_))
        ));
        // Re-issue and confirm properly.
        let req = match evaluate(&t, &input(text, false, true, None), &b, &a).unwrap_err() {
            Error::ConfirmationRequired(req) => req,
            other => panic!("{other:?}"),
        };
        let good = grant(&req.token, "app");
        evaluate(&t, &input(text, false, true, Some(&good)), &b, &a).unwrap();
        // The token cannot be replayed.
        assert!(matches!(
            evaluate(&t, &input(text, false, true, Some(&good)), &b, &a),
            Err(Error::ConfirmationRequired(_))
        ));
    }

    #[test]
    fn confirmation_does_not_transfer_to_different_text() {
        let (t, _) = tokens();
        let a = analysis(vec![stmt(
            0,
            0,
            12,
            StatementClass::Ddl,
            Some(DestructiveReason::Drop),
        )]);
        let req = match evaluate(
            &t,
            &input("drop table a", false, true, None),
            &bound("drop table a"),
            &a,
        )
        .unwrap_err()
        {
            Error::ConfirmationRequired(req) => req,
            other => panic!("{other:?}"),
        };
        let g = grant(&req.token, "app");
        assert!(matches!(
            evaluate(
                &t,
                &input("drop table b", false, true, Some(&g)),
                &bound("drop table b"),
                &a
            ),
            Err(Error::ConfirmationRequired(_))
        ));
    }

    #[test]
    fn confirmation_ranges_are_mapped_back_to_the_original_text() {
        let (t, _) = tokens();
        let b = BoundScript {
            text: "select 'xyz'; drop table t".into(),
            map: OffsetMap {
                edits: vec![Edit {
                    bound_start: 7,
                    bound_end: 12,
                    orig_start: 7,
                    orig_end: 9,
                }],
            },
        };
        let a = analysis(vec![
            stmt(0, 0, 12, StatementClass::Read, None),
            stmt(
                1,
                14,
                26,
                StatementClass::Ddl,
                Some(DestructiveReason::Drop),
            ),
        ]);
        match evaluate(
            &t,
            &input("select :a; drop table t", false, true, None),
            &b,
            &a,
        )
        .unwrap_err()
        {
            Error::ConfirmationRequired(req) => {
                assert_eq!(req.statements[0].range, TextRange { start: 11, end: 23 });
                assert_eq!(req.statements[0].text, "drop table t");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn non_production_and_harmless_statements_run_without_confirmation() {
        let (t, _) = tokens();
        let a = analysis(vec![stmt(
            0,
            0,
            12,
            StatementClass::Ddl,
            Some(DestructiveReason::Drop),
        )]);
        evaluate(
            &t,
            &input("drop table a", false, false, None),
            &bound("drop table a"),
            &a,
        )
        .unwrap();
        let tx = analysis(vec![stmt(
            0,
            0,
            5,
            StatementClass::Transaction {
                op: TxControl::Begin,
            },
            None,
        )]);
        evaluate(&t, &input("begin", true, true, None), &bound("begin"), &tx).unwrap();
        assert_eq!(t.pending(), 0);
    }

    #[test]
    fn run_guard_with_the_real_analyzer() {
        let (t, _) = tokens();
        run_guard(&t, &input("select 1", true, false, None)).unwrap();
        let err = run_guard(&t, &input("insert into t values (1)", true, false, None)).unwrap_err();
        assert_eq!(err.kind(), "readOnly");
        let ok = run_guard(&t, &input("select 1", false, false, None)).unwrap();
        assert_eq!(ok.bound.text, "select 1");
    }

    #[test]
    fn confirm_text_prefers_lease_database_then_connection() {
        let mut cfg = ConnectionConfig::new(DbKind::Postgres, "prod", "h");
        assert_eq!(confirm_text_for(&cfg, None), "prod");
        cfg.database = Some("app".into());
        assert_eq!(confirm_text_for(&cfg, None), "app");
        assert_eq!(confirm_text_for(&cfg, Some("other")), "other");
        let mut redis = ConnectionConfig::new(DbKind::Redis, "r", "h");
        assert_eq!(confirm_text_for(&redis, None), "db0");
        redis.database = Some("3".into());
        assert_eq!(confirm_text_for(&redis, None), "db3");
        assert_eq!(confirm_text_for(&redis, Some("db5")), "db5");
    }

    #[test]
    fn policy_flags_and_ensure_writable() {
        let mut cfg = ConnectionConfig::new(DbKind::Postgres, "c", "h");
        assert_eq!(policy_flags(&cfg), (false, false));
        cfg.environment = Some(Environment::Production);
        cfg.read_only = true;
        assert_eq!(policy_flags(&cfg), (true, true));
        assert_eq!(
            ensure_writable(&cfg, "changes").unwrap_err().kind(),
            "readOnly"
        );
        cfg.read_only = false;
        ensure_writable(&cfg, "changes").unwrap();
    }

    #[test]
    fn tokens_skip_strings_comments_and_quoted_identifiers() {
        let words =
            |d, t: &str| -> Vec<String> { scan_tokens(d, t).into_iter().map(|x| x.word).collect() };
        assert_eq!(
            words(
                ScriptDialect::Postgres,
                "select 'a;b' /* drop */ , \"c d\" -- x\n from t;"
            ),
            ["SELECT", "FROM", "T", ";"]
        );
        assert_eq!(
            words(ScriptDialect::Postgres, "do $f$ drop $f$ ; select 1"),
            ["DO", ";", "SELECT"]
        );
        let mysql = ScriptDialect::mysql(false, true);
        assert_eq!(words(mysql, "select 'it\\'s' # drop\n, 1"), ["SELECT"]);
        let t = scan_tokens(ScriptDialect::Postgres, "é; DROP");
        assert_eq!((t[2].start, t[2].end), (3, 7));
    }

    #[test]
    fn mysql_scripts_with_sql_mode_dependent_boundaries_are_refused() {
        let attack = "SELECT 'a\\'; DROP TABLE t; -- '";
        let mysql = ScriptDialect::mysql(false, true);
        let (t, _) = tokens();
        let mut i = input(attack, false, false, None);
        i.dialect = mysql;
        let err = run_guard(&t, &i).unwrap_err();
        assert!(err.to_string().contains("NO_BACKSLASH_ESCAPES"), "{err}");
        i.dialect = ScriptDialect::mysql(false, false);
        assert!(run_guard(&t, &i).is_err());
        // Unambiguous scripts and other dialects are untouched.
        i.dialect = mysql;
        i.text = "SELECT 'it''s'; SELECT 2";
        run_guard(&t, &i).unwrap();
        i.dialect = ScriptDialect::Postgres;
        i.text = attack;
        run_guard(&t, &i).unwrap();
    }

    #[test]
    fn confirmation_asks_for_the_database_the_statement_really_hits() {
        let mysql = ScriptDialect::mysql(false, true);
        let text = "USE other; DROP TABLE t";
        let (t, _) = tokens();
        let mut i = input(text, false, true, None);
        i.dialect = mysql;
        let c = match run_guard(&t, &i).unwrap_err() {
            Error::ConfirmationRequired(c) => *c,
            other => panic!("{other:?}"),
        };
        // Typing the tab's database ("app") is not enough: the DROP acts on `other`.
        assert_eq!(c.confirm_text, "other");
        let wrong = ConfirmationGrant {
            token: c.token.clone(),
            typed: "app".into(),
        };
        i.confirm = Some(&wrong);
        let c2 = match run_guard(&t, &i).unwrap_err() {
            Error::ConfirmationRequired(c) => *c,
            other => panic!("{other:?}"),
        };
        let right = ConfirmationGrant {
            token: c2.token,
            typed: "other".into(),
        };
        i.confirm = Some(&right);
        run_guard(&t, &i).unwrap();
        // Without a USE it is the tab's database.
        let mut j = input("DROP TABLE t", false, true, None);
        j.dialect = mysql;
        let c = match run_guard(&t, &j).unwrap_err() {
            Error::ConfirmationRequired(c) => *c,
            other => panic!("{other:?}"),
        };
        assert_eq!(c.confirm_text, "app");
    }

    #[test]
    fn production_fallback_ignores_strings_and_comments() {
        let unknown = |text: &str| {
            analysis(vec![stmt(
                0,
                0,
                text.chars().count() as u32,
                StatementClass::Unknown,
                None,
            )])
        };
        for ok in [
            "SELECT 'DROP TABLE t' /* DELETE FROM t */",
            "-- TRUNCATE t\nSELECT \"delete\" FROM t",
            "UPDATE t SET a = 1 WHERE id = 1",
        ] {
            let a = unknown(ok);
            assert!(
                production_fallback(ScriptDialect::Postgres, &a, ok).is_empty(),
                "{ok}"
            );
        }
        for bad in [
            "DROP TABLE t",
            "select 1; truncate t",
            "CALL p(); DELETE FROM t",
        ] {
            let a = unknown(bad);
            assert!(
                !production_fallback(ScriptDialect::Postgres, &a, bad).is_empty(),
                "{bad}"
            );
        }
    }

    #[test]
    fn production_fallback_reasons() {
        let reason = |text: &str, d: ScriptDialect| {
            let a = analysis(vec![stmt(
                0,
                0,
                text.chars().count() as u32,
                StatementClass::Unknown,
                None,
            )]);
            production_fallback(d, &a, text).first().map(|s| s.reason)
        };
        let pg = ScriptDialect::Postgres;
        assert_eq!(reason("x; DROP TABLE t", pg), Some(DestructiveReason::Drop));
        assert_eq!(reason("TRUNCATE t", pg), Some(DestructiveReason::Truncate));
        // A statement the analyzer did not understand: the damage is only guessed.
        assert_eq!(
            reason("CALL p(); DELETE FROM t", pg),
            Some(DestructiveReason::KeywordScan)
        );
        assert_eq!(
            reason("foo ALTER TABLE t", pg),
            Some(DestructiveReason::KeywordScan)
        );
        assert_eq!(
            reason("foo UPDATE t SET a = 1", pg),
            Some(DestructiveReason::KeywordScan)
        );
        assert_eq!(reason("foo UPDATE t SET a = 1 WHERE b = 2", pg), None);
        assert_eq!(reason("foo bar", pg), None);
    }

    #[test]
    fn new_dialects_tokens_skip_their_literals_and_comments() {
        let words =
            |d, t: &str| -> Vec<String> { scan_tokens(d, t).into_iter().map(|x| x.word).collect() };
        let cql = ScriptDialect::Cql;
        assert_eq!(
            words(cql, "select $$ drop ; $$ // drop\n from t; -- drop\n use k"),
            ["SELECT", "FROM", "T", ";", "USE", "K"]
        );
        assert_eq!(words(cql, "select 'a\\' from t"), ["SELECT", "FROM", "T"]);
        let ch = ScriptDialect::ClickHouse;
        assert_eq!(
            words(
                ch,
                "select 'it\\'s drop' # drop\n, `a\\`drop`, $t$ drop $t$ from t"
            ),
            ["SELECT", "FROM", "T"]
        );
        // MySQL / TiDB executable comments are code
        let my = ScriptDialect::mysql(false, true);
        assert_eq!(
            words(my, "select /*T![a_b] drop */ 1"),
            ["SELECT", "A_B", "DROP"]
        );
        assert_eq!(words(my, "select /*!50000 drop */ 1"), ["SELECT", "DROP"]);
        assert_eq!(words(my, "select /*M! drop */ 1"), ["SELECT", "DROP"]);
        assert_eq!(words(my, "select /* drop */ 1"), ["SELECT"]);
    }

    #[test]
    fn scripts_that_change_sql_mode_with_ambiguous_boundaries_are_refused() {
        let (t, _) = tokens();
        let mut i = input(
            "SET sql_mode='ANSI_QUOTES'; SELECT \"a\\\"; DROP TABLE t; -- \"",
            false,
            false,
            None,
        );
        i.dialect = ScriptDialect::mysql(false, true);
        let err = run_guard(&t, &i).unwrap_err();
        assert!(err.to_string().contains("sql_mode"), "{err}");
        i.dialect = ScriptDialect::MySql {
            mariadb: false,
            backslash_escapes: true,
            ansi_quotes: true,
        };
        assert!(run_guard(&t, &i).is_err());
        // The same mode switch with plain text is fine.
        i.text = "SET sql_mode='ANSI_QUOTES'; SELECT \"a\" FROM t";
        run_guard(&t, &i).unwrap();
    }

    #[test]
    fn clickhouse_nested_comments_do_not_hide_keywords() {
        let words =
            |d, t: &str| -> Vec<String> { scan_tokens(d, t).into_iter().map(|x| x.word).collect() };
        let ch = ScriptDialect::ClickHouse;
        assert_eq!(
            words(ch, "/* /* */ SELECT ' */ DROP TABLE t -- '"),
            ["DROP", "TABLE", "T"]
        );
        assert_eq!(words(ch, "/* /* */ DROP */ SELECT 1"), ["SELECT"]);
        let text = "/* /* */ SELECT ' */ DROP TABLE t -- '";
        let a = analysis(vec![stmt(
            0,
            0,
            text.chars().count() as u32,
            StatementClass::Unknown,
            None,
        )]);
        assert!(!production_fallback(ch, &a, text).is_empty());
    }

    #[test]
    fn ansi_quotes_scan_has_no_backslash_escape_in_identifiers() {
        let words =
            |d, t: &str| -> Vec<String> { scan_tokens(d, t).into_iter().map(|x| x.word).collect() };
        let text = "select \"a\\\" drop table t; --\"";
        let my = ScriptDialect::mysql(false, true);
        let ansi = ScriptDialect::MySql {
            mariadb: false,
            backslash_escapes: true,
            ansi_quotes: true,
        };
        assert_eq!(words(my, text), ["SELECT"]);
        assert_eq!(words(ansi, text), ["SELECT", "DROP", "TABLE", "T", ";"]);
    }

    #[test]
    fn prepare_from_a_literal_counts_as_a_mode_change() {
        let (t, _) = tokens();
        let mut i = input(
            "PREPARE s FROM 'SET sql_mode=''ANSI_QUOTES'''; EXECUTE s; SELECT \"a\\\"; DROP TABLE t; -- \"",
            false,
            false,
            None,
        );
        i.dialect = ScriptDialect::mysql(false, true);
        assert!(run_guard(&t, &i).is_err());
        i.text = "PREPARE s FROM 'SELECT 1'; EXECUTE s";
        run_guard(&t, &i).unwrap();
    }
}
