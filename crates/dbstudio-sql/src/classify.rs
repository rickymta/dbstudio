//! Statement analysis: split, classify and detect destructive statements.

use dbstudio_core::{ScriptAnalysis, ScriptDialect, StatementClass, StatementInfo, TextRange};

/// Analyse `text`. The real implementation uses `sqlparser` first and a keyword fallback.
///
/// Scaffold: the whole text is ONE statement classed [`StatementClass::Unknown`] (verb `""`,
/// not destructive); empty or whitespace-only text has no statements.
pub fn analyze(d: ScriptDialect, text: &str) -> ScriptAnalysis {
    let statements = if text.trim().is_empty() {
        Vec::new()
    } else {
        vec![StatementInfo {
            index: 0,
            range: TextRange {
                start: 0,
                end: crate::char_len(text),
            },
            class: StatementClass::Unknown,
            verb: String::new(),
            destructive: None,
            implicit_commit: false,
            targets: Vec::new(),
        }]
    };
    ScriptAnalysis {
        dialect: d,
        statements,
        params: Vec::new(),
        problems: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_contract_one_unknown_statement() {
        let a = analyze(ScriptDialect::Postgres, "drop table t; select 1");
        assert_eq!(a.dialect, ScriptDialect::Postgres);
        assert_eq!(a.statements.len(), 1);
        let s = &a.statements[0];
        assert_eq!(s.class, StatementClass::Unknown);
        assert_eq!(s.verb, "");
        assert_eq!(s.destructive, None);
        assert_eq!(s.range, TextRange { start: 0, end: 22 });
        assert!(a.params.is_empty() && a.problems.is_empty());
    }

    #[test]
    fn stub_contract_blank_text_has_no_statements() {
        assert!(analyze(ScriptDialect::MsSql, "").statements.is_empty());
        assert!(analyze(ScriptDialect::MsSql, " \n\t").statements.is_empty());
    }
}
