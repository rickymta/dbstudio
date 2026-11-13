//! SSH tunnel configuration (CONN-06).

use serde::{Deserialize, Serialize};

fn ssh_port() -> u16 {
    22
}

/// SSH tunnel settings of a connection. Secrets (password / key passphrase) are not stored here:
/// they live in the keychain under `<connId>#ssh`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SshConfig {
    /// Bastion host.
    pub host: String,
    /// Bastion port (default 22).
    #[serde(default = "ssh_port")]
    pub port: u16,
    /// Login user.
    pub username: String,
    /// Authentication method.
    pub auth: SshAuth,
    /// Keep-alive interval in seconds.
    #[serde(default)]
    pub keepalive_secs: Option<u32>,
    /// Connect timeout in milliseconds.
    #[serde(default)]
    pub connect_timeout_ms: Option<u64>,
}

/// SSH authentication method.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "camelCase")]
pub enum SshAuth {
    /// Password; secret in the keychain (`<connId>#ssh`).
    Password,
    /// Private key file; passphrase (if any) in the keychain (`<connId>#ssh`).
    #[serde(rename_all = "camelCase")]
    PrivateKey {
        /// Path of the key file.
        key_path: String,
        /// The key is encrypted with a passphrase.
        has_passphrase: bool,
    },
    /// `SSH_AUTH_SOCK` / Windows OpenSSH pipe.
    Agent,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn auth_shapes() {
        assert_eq!(
            serde_json::to_value(SshAuth::Password).unwrap(),
            json!({"method": "password"})
        );
        assert_eq!(
            serde_json::to_value(SshAuth::Agent).unwrap(),
            json!({"method": "agent"})
        );
        let k = SshAuth::PrivateKey {
            key_path: "/home/me/.ssh/id".into(),
            has_passphrase: true,
        };
        let json = serde_json::to_value(&k).unwrap();
        assert_eq!(
            json,
            json!({"method": "privateKey", "keyPath": "/home/me/.ssh/id", "hasPassphrase": true})
        );
        assert_eq!(serde_json::from_value::<SshAuth>(json).unwrap(), k);
    }

    #[test]
    fn port_defaults_to_22() {
        let c: SshConfig = serde_json::from_value(json!({
            "host": "b", "username": "u", "auth": {"method": "agent"}
        }))
        .unwrap();
        assert_eq!(c.port, 22);
        assert!(c.keepalive_secs.is_none() && c.connect_timeout_ms.is_none());
        let json = serde_json::to_value(&c).unwrap();
        assert_eq!(json["port"], 22);
        assert!(json["keepaliveSecs"].is_null());
        assert_eq!(serde_json::from_value::<SshConfig>(json).unwrap(), c);
    }
}
