//! Data model shared between the store and the IPC layer.

use dbstudio_core::{ConnectionConfig, Secret};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A saved connection: the driver-level config plus presentation and storage metadata.
/// The password itself is never part of the profile; see [`SecretChange`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionProfile {
    /// Driver-level connection settings (flattened into the same JSON object).
    #[serde(flatten)]
    pub config: ConnectionConfig,
    /// Group the profile belongs to.
    #[serde(default)]
    pub group_id: Option<Uuid>,
    /// Display color (CSS hex string such as `#e5484d`).
    #[serde(default)]
    pub color: Option<String>,
    /// Whether the password is stored in the OS keychain.
    #[serde(default)]
    pub save_password: bool,
    /// Whether a secret is currently stored in the keychain (read-only for the client).
    #[serde(default)]
    pub has_password: bool,
    /// Position among siblings.
    #[serde(default)]
    pub sort_order: i64,
    /// Creation time, ISO-8601.
    #[serde(default)]
    pub created_at: String,
    /// Last update time, ISO-8601.
    #[serde(default)]
    pub updated_at: String,
}

/// A folder of connections (groups may nest).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionGroup {
    /// Stable identifier.
    pub id: Uuid,
    /// Display name.
    pub name: String,
    /// Parent group, `None` for top level.
    #[serde(default)]
    pub parent_id: Option<Uuid>,
    /// Position among siblings.
    #[serde(default)]
    pub sort_order: i64,
}

/// Everything shown in the connections sidebar.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionsSnapshot {
    /// All groups.
    pub groups: Vec<ConnectionGroup>,
    /// All profiles.
    pub profiles: Vec<ConnectionProfile>,
}

/// What to do with the stored password when saving a profile. `Debug` never prints the value.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "action", rename_all = "camelCase")]
pub enum SecretChange {
    /// Leave the stored secret untouched.
    Keep,
    /// Replace the stored secret.
    Set {
        /// New secret value.
        value: Secret,
    },
    /// Delete the stored secret.
    Clear,
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_core::DbKind;
    use serde_json::json;

    fn profile() -> ConnectionProfile {
        ConnectionProfile {
            config: ConnectionConfig::new(DbKind::Postgres, "dev", "localhost"),
            group_id: None,
            color: Some("#00aa00".into()),
            save_password: true,
            has_password: false,
            sort_order: 2,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    #[test]
    fn profile_flattens_config_and_uses_camel_case() {
        let p = profile();
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["name"], "dev");
        assert_eq!(json["host"], "localhost");
        assert_eq!(json["savePassword"], true);
        assert_eq!(json["hasPassword"], false);
        assert_eq!(json["sortOrder"], 2);
        assert_eq!(json["tls"]["mode"], "prefer");
        assert!(json.get("config").is_none());
        let back: ConnectionProfile = serde_json::from_value(json).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn group_and_snapshot_serde() {
        let g = ConnectionGroup {
            id: Uuid::new_v4(),
            name: "Work".into(),
            parent_id: None,
            sort_order: 0,
        };
        let snap = ConnectionsSnapshot {
            groups: vec![g],
            profiles: vec![profile()],
        };
        let json = serde_json::to_value(&snap).unwrap();
        assert!(json["groups"][0]["parentId"].is_null());
        let back: ConnectionsSnapshot = serde_json::from_value(json).unwrap();
        assert_eq!(back, snap);
    }

    #[test]
    fn secret_change_parses_and_hides_value() {
        let keep: SecretChange = serde_json::from_value(json!({"action": "keep"})).unwrap();
        assert_eq!(keep, SecretChange::Keep);
        let clear: SecretChange = serde_json::from_value(json!({"action": "clear"})).unwrap();
        assert_eq!(clear, SecretChange::Clear);
        let set: SecretChange =
            serde_json::from_value(json!({"action": "set", "value": "hunter2"})).unwrap();
        assert!(!format!("{set:?}").contains("hunter2"));
        assert!(matches!(set, SecretChange::Set { value } if value.expose() == "hunter2"));
    }
}
