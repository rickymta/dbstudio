//! PostgreSQL-protocol flavours: detection from `version()` and per-server capabilities.

use dbstudio_core::{Capabilities, DbKind};

/// Server family behind the PostgreSQL wire protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Flavor {
    /// Stock PostgreSQL (also Aurora, RDS, ...).
    Postgres,
    /// CockroachDB, with its major/minor version.
    Cockroach { major: u32, minor: u32 },
    /// YugabyteDB YSQL.
    Yugabyte,
}

impl Flavor {
    /// Detect the flavour from the text of `SELECT version()`.
    pub fn detect(version: &str) -> Flavor {
        if version.contains("CockroachDB") {
            let (major, minor) = cockroach_version(version).unwrap_or((0, 0));
            Flavor::Cockroach { major, minor }
        } else if version.contains("-YB-") {
            Flavor::Yugabyte
        } else {
            Flavor::Postgres
        }
    }

    /// `DbKind` that corresponds to the flavour.
    pub fn kind(self) -> DbKind {
        match self {
            Flavor::Postgres => DbKind::Postgres,
            Flavor::Cockroach { .. } => DbKind::CockroachDb,
            Flavor::Yugabyte => DbKind::YugabyteDb,
        }
    }

    pub fn is_cockroach(self) -> bool {
        matches!(self, Flavor::Cockroach { .. })
    }

    pub fn is_yugabyte(self) -> bool {
        matches!(self, Flavor::Yugabyte)
    }

    /// Product name for `ServerInfo`.
    pub fn product(self) -> &'static str {
        match self {
            Flavor::Postgres => "PostgreSQL",
            Flavor::Cockroach { .. } => "CockroachDB",
            Flavor::Yugabyte => "YugabyteDB",
        }
    }

    /// Value of `ServerInfo.extra["flavor"]`.
    pub fn id(self) -> &'static str {
        match self {
            Flavor::Postgres => "postgres",
            Flavor::Cockroach { .. } => "cockroachdb",
            Flavor::Yugabyte => "yugabytedb",
        }
    }

    /// Factor applied to catalog/search timeouts (YugabyteDB catalog queries are slower).
    pub fn timeout_factor(self) -> u32 {
        if self.is_yugabyte() { 2 } else { 1 }
    }

    /// Capabilities of this server, derived from the table of its `DbKind`.
    pub fn capabilities(self) -> Capabilities {
        let mut caps = Capabilities::planned_for(self.kind());
        if let Flavor::Cockroach { major, minor } = self {
            // Triggers arrived in CockroachDB 24.3.
            caps.triggers = (major, minor) >= (24, 3);
        }
        caps
    }
}

/// `v25.4.20` in `CockroachDB CCL v25.4.20 (...)`.
fn cockroach_version(version: &str) -> Option<(u32, u32)> {
    let rest = version.split(" v").nth(1)?;
    let mut parts = rest
        .split(|c: char| !c.is_ascii_digit())
        .filter(|p| !p.is_empty());
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_flavours() {
        let crdb = "CockroachDB CCL v25.4.20 (aarch64-unknown-linux-gnu, built 2026/11/03, go1.23)";
        assert_eq!(
            Flavor::detect(crdb),
            Flavor::Cockroach {
                major: 25,
                minor: 4
            }
        );
        let yb =
            "PostgreSQL 15.12-YB-2025.2.8.1-b0 on aarch64-unknown-linux-gnu, compiled by clang";
        assert_eq!(Flavor::detect(yb), Flavor::Yugabyte);
        assert_eq!(
            Flavor::detect("PostgreSQL 16.4 (Debian 16.4-1) on x86_64"),
            Flavor::Postgres
        );
    }

    #[test]
    fn cockroach_triggers_follow_version() {
        let old = Flavor::Cockroach {
            major: 24,
            minor: 2,
        };
        let new = Flavor::Cockroach {
            major: 24,
            minor: 3,
        };
        assert!(!old.capabilities().triggers);
        assert!(new.capabilities().triggers);
        assert!(Flavor::Yugabyte.capabilities().triggers);
        assert_eq!(Flavor::Yugabyte.timeout_factor(), 2);
    }
}
