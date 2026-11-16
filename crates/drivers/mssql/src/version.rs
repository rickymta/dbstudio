//! SQL Server product version and the T-SQL features each release gained.

use std::cmp::Ordering;

/// `ProductVersion` of the server (`major.minor.build[.revision]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ServerVersion {
    pub major: u32,
    pub minor: u32,
    pub build: u32,
}

impl ServerVersion {
    /// Assumed when the version is unknown: the newest feature set.
    pub const LATEST: ServerVersion = ServerVersion {
        major: 99,
        minor: 0,
        build: 0,
    };

    pub const fn new(major: u32, minor: u32, build: u32) -> Self {
        ServerVersion {
            major,
            minor,
            build,
        }
    }

    /// Parse `"10.50.6000.34"` (missing parts count as 0). `None` on garbage.
    pub fn parse(s: &str) -> Option<ServerVersion> {
        let mut it = s.trim().split('.');
        let major = it.next()?.trim().parse().ok()?;
        let minor = it.next().map_or(Some(0), |p| p.trim().parse().ok())?;
        let build = it.next().map_or(Some(0), |p| p.trim().parse().ok())?;
        Some(ServerVersion {
            major,
            minor,
            build,
        })
    }

    /// Parse, assuming the newest server when that fails.
    pub fn parse_or_latest(s: &str) -> ServerVersion {
        Self::parse(s).unwrap_or(Self::LATEST)
    }

    /// `OFFSET .. FETCH` paging (2012).
    pub fn has_offset_fetch(&self) -> bool {
        self.major >= 11
    }
    /// `sys.sequences` / `CREATE SEQUENCE` (2012).
    pub fn has_sequences(&self) -> bool {
        self.major >= 11
    }
    /// `sys.tables.is_memory_optimized` (2014).
    pub fn has_memory_optimized(&self) -> bool {
        self.major >= 12
    }
    /// `sys.tables.temporal_type` / `is_external` (2016).
    pub fn has_temporal(&self) -> bool {
        self.major >= 13
    }
    /// `STRING_AGG` (2017).
    pub fn has_string_agg(&self) -> bool {
        self.major >= 14
    }
    /// `CREATE OR ALTER` (2016 SP1, 13.0.4001).
    pub fn has_create_or_alter(&self) -> bool {
        *self >= ServerVersion::new(13, 0, 4001)
    }
    /// `DROP .. IF EXISTS` (2016).
    pub fn has_drop_if_exists(&self) -> bool {
        self.major >= 13
    }
}

impl Default for ServerVersion {
    fn default() -> Self {
        Self::LATEST
    }
}

impl PartialOrd for ServerVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ServerVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.build).cmp(&(other.major, other.minor, other.build))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_versions() {
        assert_eq!(
            ServerVersion::parse("10.50.6000.34"),
            Some(ServerVersion::new(10, 50, 6000))
        );
        assert_eq!(
            ServerVersion::parse("10.50.6000"),
            Some(ServerVersion::new(10, 50, 6000))
        );
        assert_eq!(
            ServerVersion::parse("16"),
            Some(ServerVersion::new(16, 0, 0))
        );
        assert_eq!(
            ServerVersion::parse(" 15.0.2000.5 "),
            Some(ServerVersion::new(15, 0, 2000))
        );
        assert_eq!(ServerVersion::parse("abc"), None);
        assert_eq!(ServerVersion::parse(""), None);
        assert_eq!(ServerVersion::parse("10.x"), None);
        assert_eq!(ServerVersion::parse_or_latest("zzz"), ServerVersion::LATEST);
    }

    #[test]
    fn predicates() {
        let v2008 = ServerVersion::new(10, 50, 6000);
        assert!(!v2008.has_offset_fetch() && !v2008.has_sequences());
        assert!(!v2008.has_string_agg() && !v2008.has_create_or_alter());
        assert!(!v2008.has_drop_if_exists() && !v2008.has_memory_optimized());
        let v2012 = ServerVersion::new(11, 0, 2100);
        assert!(v2012.has_offset_fetch() && v2012.has_sequences() && !v2012.has_memory_optimized());
        assert!(ServerVersion::new(12, 0, 0).has_memory_optimized());
        assert!(!ServerVersion::new(12, 0, 0).has_temporal());
        assert!(!ServerVersion::new(13, 0, 1601).has_create_or_alter());
        assert!(ServerVersion::new(13, 0, 1601).has_drop_if_exists());
        assert!(ServerVersion::new(13, 0, 4001).has_create_or_alter());
        assert!(!ServerVersion::new(13, 0, 4001).has_string_agg());
        assert!(ServerVersion::new(14, 0, 1).has_string_agg());
        assert!(ServerVersion::LATEST.has_create_or_alter());
        assert!(v2008 < v2012);
    }
}
