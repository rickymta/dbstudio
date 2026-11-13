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
    ConfirmationGrant, ConfirmationRequest, ConnectionConfig, DbKind, DestructiveStatement,
    Environment, Error, ParamValue, Result, ScriptAnalysis, ScriptDialect, TextRange,
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
    match dbstudio_sql::guard::check_with_text(&policy, analysis, &bound.text) {
        Verdict::Allow => Ok(()),
        Verdict::Deny(Error::ReadOnlyViolation { message, position }) => {
            Err(Error::ReadOnlyViolation {
                message,
                position: position.map(|p| bound.map.to_original(p)),
            })
        }
        Verdict::Deny(other) => Err(other),
        Verdict::Confirm(statements) => {
            if let Some(grant) = input.confirm {
                match tokens.consume(grant, input.target_id, bound.text.as_bytes()) {
                    Ok(()) => return Ok(()),
                    Err(why) => tracing::debug!(?why, "confirmation refused"),
                }
            }
            let (token, expires_in_ms) =
                tokens.issue(input.target_id, bound.text.as_bytes(), input.confirm_text);
            let statements: Vec<DestructiveStatement> = statements
                .into_iter()
                .map(|s| DestructiveStatement {
                    range: map_range(&bound.map, s.range),
                    ..s
                })
                .collect();
            Err(Error::ConfirmationRequired(Box::new(ConfirmationRequest {
                token,
                confirm_text: input.confirm_text.to_string(),
                statements,
                expires_in_ms,
            })))
        }
    }
}

/// The five-step flow for a script: bind, analyze, policy, check, verdict.
pub fn run_guard(tokens: &ConfirmTokens, input: &GuardInput<'_>) -> Result<Guarded> {
    let bound = dbstudio_sql::bind(input.dialect, input.text, input.params)?;
    let analysis = dbstudio_sql::analyze(input.dialect, &bound.text);
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
    fn run_guard_with_the_scaffold_analyzer() {
        // The stub analyzer classifies everything as Unknown, which read-only refuses.
        let (t, _) = tokens();
        let err = run_guard(&t, &input("select 1", true, false, None)).unwrap_err();
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
}
