//! Editor hints: keywords, functions and (Redis/Mongo) commands per dialect.

use dbstudio_core::ScriptDialect;
use serde::{Deserialize, Serialize};

/// Completion hints for the editor.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Vocabulary {
    /// SQL keywords.
    pub keywords: Vec<String>,
    /// Built-in functions.
    pub functions: Vec<String>,
    /// Commands (Redis commands, Mongo methods).
    pub commands: Vec<CommandHint>,
}

/// A command with its syntax, for completion and signature help.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandHint {
    /// Command name.
    pub name: String,
    /// Syntax line, e.g. `SET key value [EX seconds]`.
    pub syntax: String,
    /// One-line summary.
    pub summary: String,
    /// Flags as text (`write`, `dangerous`...).
    pub flags: Vec<String>,
}

/// Vocabulary of `d`. Scaffold: empty.
pub fn vocabulary(d: ScriptDialect) -> Vocabulary {
    let _ = d;
    Vocabulary::default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_core::testing::write_or_check;
    use serde_json::json;

    fn hint() -> CommandHint {
        CommandHint {
            name: "SET".into(),
            syntax: "SET key value [EX seconds]".into(),
            summary: "Set the string value of a key".into(),
            flags: vec!["write".into()],
        }
    }

    fn sample() -> Vocabulary {
        Vocabulary {
            keywords: vec!["SELECT".into()],
            functions: vec!["count".into()],
            commands: vec![hint()],
        }
    }

    #[test]
    fn stub_contract_vocabulary_is_empty() {
        assert_eq!(vocabulary(ScriptDialect::RedisCli), Vocabulary::default());
    }

    #[test]
    fn json_shapes() {
        let v = sample();
        let j = serde_json::to_value(&v).unwrap();
        assert_eq!(
            j,
            json!({
                "keywords": ["SELECT"],
                "functions": ["count"],
                "commands": [{
                    "name": "SET",
                    "syntax": "SET key value [EX seconds]",
                    "summary": "Set the string value of a key",
                    "flags": ["write"]
                }]
            })
        );
        assert_eq!(serde_json::from_value::<Vocabulary>(j).unwrap(), v);
    }

    #[test]
    fn fixtures() {
        write_or_check(
            "script",
            &json!({
                "Vocabulary": serde_json::to_value(sample()).unwrap(),
                "CommandHint": serde_json::to_value(hint()).unwrap(),
            }),
        );
    }
}
