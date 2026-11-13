//! Read-only and production safety decisions over an analysed script.
//!
//! Confirmation tokens (issuing, expiry, digest, single use) live in the application layer
//! (`src-tauri`); this module only decides.

use dbstudio_core::{
    DestructiveStatement, Error, ScriptAnalysis, StatementClass, StatementInfo, TextRange,
};

/// Maximum characters of statement text carried in a [`DestructiveStatement`].
const MAX_STATEMENT_TEXT: usize = 2000;

/// What the connection allows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    /// Connection is read-only (CONN-10).
    pub read_only: bool,
    /// Connection is a production environment (CONN-11).
    pub production: bool,
    /// Text the user must type to confirm destructive statements (database name, `db0`...).
    pub confirm_text: String,
}

/// Outcome of [`check`].
#[derive(Debug)]
pub enum Verdict {
    /// Run it.
    Allow,
    /// Refuse; the error is `ReadOnlyViolation` whose `position` is in the analysed text (the
    /// caller maps it back through the `OffsetMap`).
    Deny(Error),
    /// Production: these statements need an explicit confirmation first.
    Confirm(Vec<DestructiveStatement>),
}

/// Whether read-only mode lets `class` through.
fn allowed_when_read_only(class: StatementClass) -> bool {
    matches!(
        class,
        StatementClass::Read
            | StatementClass::Explain { writes: false, .. }
            | StatementClass::Transaction { .. }
            | StatementClass::Session
            | StatementClass::Empty
    )
}

fn denial(s: &StatementInfo) -> Error {
    let what = if s.verb.is_empty() {
        "statement".to_string()
    } else {
        s.verb.clone()
    };
    Error::ReadOnlyViolation {
        message: format!("{what} is not allowed on a read-only connection"),
        position: Some(s.range.start),
    }
}

fn slice(text: &str, range: TextRange) -> String {
    text.chars()
        .skip(range.start as usize)
        .take(range.end.saturating_sub(range.start) as usize)
        .take(MAX_STATEMENT_TEXT)
        .collect()
}

/// Decide without statement text: the `text` of listed [`DestructiveStatement`]s is empty.
pub fn check(policy: &Policy, analysis: &ScriptAnalysis) -> Verdict {
    decide(policy, analysis, None)
}

/// Like [`check`] but fills `DestructiveStatement::text` from `text`, the text that was analysed
/// (truncated to 2000 characters).
pub fn check_with_text(policy: &Policy, analysis: &ScriptAnalysis, text: &str) -> Verdict {
    decide(policy, analysis, Some(text))
}

fn decide(policy: &Policy, analysis: &ScriptAnalysis, text: Option<&str>) -> Verdict {
    if policy.read_only {
        if let Some(s) = analysis
            .statements
            .iter()
            .find(|s| !allowed_when_read_only(s.class))
        {
            return Verdict::Deny(denial(s));
        }
    }
    if policy.production {
        let list: Vec<DestructiveStatement> = analysis
            .statements
            .iter()
            .filter_map(|s| {
                s.destructive.map(|reason| DestructiveStatement {
                    index: s.index,
                    range: s.range,
                    text: text.map(|t| slice(t, s.range)).unwrap_or_default(),
                    reason,
                })
            })
            .collect();
        if !list.is_empty() {
            return Verdict::Confirm(list);
        }
    }
    Verdict::Allow
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_core::{DestructiveReason, ScriptDialect, TxControl};

    fn stmt(
        index: u32,
        class: StatementClass,
        destructive: Option<DestructiveReason>,
    ) -> StatementInfo {
        StatementInfo {
            index,
            range: TextRange {
                start: index * 10,
                end: index * 10 + 9,
            },
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

    fn policy(read_only: bool, production: bool) -> Policy {
        Policy {
            read_only,
            production,
            confirm_text: "app".into(),
        }
    }

    #[test]
    fn read_only_allows_reads_explain_tx_session_empty() {
        let a = analysis(vec![
            stmt(0, StatementClass::Read, None),
            stmt(
                1,
                StatementClass::Explain {
                    analyze: true,
                    writes: false,
                },
                None,
            ),
            stmt(
                2,
                StatementClass::Transaction {
                    op: TxControl::Begin,
                },
                None,
            ),
            stmt(3, StatementClass::Session, None),
            stmt(4, StatementClass::Empty, None),
        ]);
        assert!(matches!(check(&policy(true, false), &a), Verdict::Allow));
    }

    #[test]
    fn read_only_denies_everything_else_with_position() {
        let others = [
            StatementClass::Write,
            StatementClass::Ddl,
            StatementClass::Procedural,
            StatementClass::Admin,
            StatementClass::Unknown,
            StatementClass::Explain {
                analyze: true,
                writes: true,
            },
        ];
        for class in others {
            let a = analysis(vec![
                stmt(0, StatementClass::Read, None),
                stmt(1, class, None),
            ]);
            match check(&policy(true, false), &a) {
                Verdict::Deny(Error::ReadOnlyViolation { position, .. }) => {
                    assert_eq!(position, Some(10), "{class:?}");
                }
                other => panic!("{class:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn writes_are_allowed_when_not_read_only() {
        let a = analysis(vec![stmt(0, StatementClass::Write, None)]);
        assert!(matches!(check(&policy(false, false), &a), Verdict::Allow));
        assert!(matches!(check(&policy(false, true), &a), Verdict::Allow));
    }

    #[test]
    fn production_confirms_destructive_statements_only() {
        let a = analysis(vec![
            stmt(0, StatementClass::Write, None),
            stmt(1, StatementClass::Ddl, Some(DestructiveReason::Drop)),
            stmt(
                2,
                StatementClass::Write,
                Some(DestructiveReason::DeleteWithoutWhere),
            ),
        ]);
        match check(&policy(false, true), &a) {
            Verdict::Confirm(list) => {
                assert_eq!(list.len(), 2);
                assert_eq!(list[0].index, 1);
                assert_eq!(list[0].reason, DestructiveReason::Drop);
                assert_eq!(list[0].range, TextRange { start: 10, end: 19 });
                assert_eq!(list[1].index, 2);
                assert_eq!(list[1].text, "");
            }
            other => panic!("{other:?}"),
        }
        // Not production: destructive statements run without confirmation.
        assert!(matches!(check(&policy(false, false), &a), Verdict::Allow));
    }

    #[test]
    fn read_only_wins_over_production_confirmation() {
        let a = analysis(vec![stmt(
            0,
            StatementClass::Ddl,
            Some(DestructiveReason::Drop),
        )]);
        assert!(matches!(
            check(&policy(true, true), &a),
            Verdict::Deny(Error::ReadOnlyViolation { .. })
        ));
    }

    #[test]
    fn check_with_text_fills_and_truncates_statement_text() {
        let long = "x".repeat(5000);
        let text = format!("select 1; {long}");
        let mut s = stmt(1, StatementClass::Ddl, Some(DestructiveReason::Truncate));
        s.range = TextRange {
            start: 10,
            end: u32::try_from(text.chars().count()).unwrap(),
        };
        let a = analysis(vec![s]);
        match check_with_text(&policy(false, true), &a, &text) {
            Verdict::Confirm(list) => assert_eq!(list[0].text.chars().count(), 2000),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn empty_analysis_is_allowed() {
        let a = analysis(vec![]);
        assert!(matches!(check(&policy(true, true), &a), Verdict::Allow));
    }
}
