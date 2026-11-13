//! Removal of secrets from text before it is stored in the query history.

use dbstudio_core::ScriptDialect;

/// Replace the literal after `PASSWORD` / `IDENTIFIED BY` / `AUTH` / `CONFIG SET requirepass`
/// with `'***'`. Scaffold: identity.
pub fn redact(d: ScriptDialect, text: &str) -> String {
    let _ = d;
    text.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_contract_is_identity() {
        let t = "CREATE USER u IDENTIFIED BY 'secret'";
        assert_eq!(redact(ScriptDialect::Oracle, t), t);
    }
}
