//! Input validation shared by saving and importing profiles.

use dbstudio_core::{Error, Result};

use crate::model::{ConnectionGroup, ConnectionProfile};

/// Longest accepted name, host, database, user or group name (in characters).
pub const MAX_FIELD_CHARS: usize = 255;

/// Longest accepted custom environment label (characters).
pub const MAX_ENVIRONMENT_LABEL_CHARS: usize = 32;

/// `options` keys an imported profile may keep. Everything else (notably `init_command` or
/// arbitrary server settings) is dropped on import because a shared file must not be able to run
/// statements or change server parameters on every connect.
pub const IMPORT_OPTION_ALLOWLIST: &[&str] = &["application_name", "pool_max"];

fn invalid(msg: impl std::fmt::Display) -> Error {
    Error::Invalid(msg.to_string())
}

/// Check length and reject control characters.
pub(crate) fn check_text(label: &str, value: &str) -> Result<()> {
    if value.chars().count() > MAX_FIELD_CHARS {
        return Err(invalid(format!(
            "{label} is longer than {MAX_FIELD_CHARS} characters"
        )));
    }
    if value.chars().any(char::is_control) {
        return Err(invalid(format!("{label} contains control characters")));
    }
    Ok(())
}

/// `#rgb` or `#rrggbb`.
pub fn is_valid_color(color: &str) -> bool {
    let Some(hex) = color.strip_prefix('#') else {
        return false;
    };
    matches!(hex.len(), 3 | 6) && hex.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Trim the custom environment label; an empty one means "no label".
pub(crate) fn normalize_profile(profile: &mut ConnectionProfile) {
    profile.config.environment_label = profile
        .config
        .environment_label
        .take()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty());
}

/// Reject profiles that cannot be connected to or carry unsafe presentation values.
pub(crate) fn validate_profile(profile: &ConnectionProfile) -> Result<()> {
    let c = &profile.config;
    if c.name.trim().is_empty() {
        return Err(invalid("connection name must not be empty"));
    }
    if c.host.trim().is_empty() {
        return Err(invalid("host must not be empty"));
    }
    if c.port == 0 {
        return Err(invalid("port must be greater than zero"));
    }
    check_text("connection name", &c.name)?;
    check_text("host", &c.host)?;
    if let Some(v) = &c.database {
        check_text("database", v)?;
    }
    if let Some(v) = &c.username {
        check_text("username", v)?;
    }
    if let Some(label) = &c.environment_label {
        if label.trim().chars().count() > MAX_ENVIRONMENT_LABEL_CHARS {
            return Err(invalid(format!(
                "environment label is longer than {MAX_ENVIRONMENT_LABEL_CHARS} characters"
            )));
        }
        if label.chars().any(char::is_control) {
            return Err(invalid("environment label contains control characters"));
        }
    }
    if let Some(color) = &profile.color
        && !is_valid_color(color)
    {
        return Err(invalid("color must be a hex value like #rgb or #rrggbb"));
    }
    Ok(())
}

pub(crate) fn validate_group(group: &ConnectionGroup) -> Result<()> {
    if group.name.trim().is_empty() {
        return Err(invalid("group name must not be empty"));
    }
    check_text("group name", &group.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors() {
        for ok in ["#fff", "#E5484D", "#00aa00"] {
            assert!(is_valid_color(ok), "{ok}");
        }
        for bad in [
            "", "#", "#ffff", "fff", "#ggg", "red", "#fff;}", "url(x)", "#12345", " #fff",
        ] {
            assert!(!is_valid_color(bad), "{bad}");
        }
    }

    #[test]
    fn text_limits() {
        assert!(check_text("x", &"a".repeat(255)).is_ok());
        assert!(check_text("x", &"a".repeat(256)).is_err());
        assert!(check_text("x", "multi\nline").is_err());
        assert!(check_text("x", "tab\there").is_err());
        assert!(check_text("x", "nul\0").is_err());
        assert!(check_text("x", "ünïcode 日本").is_ok());
    }
}
