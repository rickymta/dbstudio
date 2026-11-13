//! `PathGrants`: the only paths the webview may make the backend read or write.
//!
//! Every path that comes out of a native dialog (`file_pick_*`, `sql_file_*`, `plan_file_*`) is
//! recorded here (canonical form, purpose, expiry one hour later). Commands that take a path from
//! the webview (`export_start`, `import_*`, `backup_*`, `restore_start`) accept only granted
//! paths, so a hijacked webview cannot write arbitrary files.

#![allow(dead_code)] // scaffold: consumed by the export/backup packages

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use dbstudio_core::{Error, Result};

/// How long a grant stays valid.
pub const GRANT_TTL: Duration = Duration::from_secs(60 * 60);

/// What a granted path may be used for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// Read an existing file.
    Read,
    /// Create or overwrite a file.
    Write,
    /// A directory: files directly or indirectly inside it may be read and written.
    Dir,
}

#[derive(Debug)]
struct Grant {
    path: PathBuf,
    purpose: Purpose,
    expires: Instant,
}

/// Registry of granted paths.
#[derive(Debug)]
pub struct PathGrants {
    ttl: Duration,
    grants: Mutex<Vec<Grant>>,
}

impl Default for PathGrants {
    fn default() -> Self {
        Self::new()
    }
}

/// Canonical form of `path`; a file that does not exist yet is canonicalised through its parent.
fn canonical(path: &Path) -> Result<PathBuf> {
    if let Ok(p) = std::fs::canonicalize(path) {
        return Ok(p);
    }
    let name = path
        .file_name()
        .ok_or_else(|| Error::Other(format!("invalid path: {}", path.display())))?;
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let parent = std::fs::canonicalize(parent)
        .map_err(|e| Error::Other(format!("invalid path {}: {e}", path.display())))?;
    Ok(parent.join(name))
}

impl PathGrants {
    /// Grants valid for one hour.
    pub fn new() -> Self {
        Self::with_ttl(GRANT_TTL)
    }

    /// Grants valid for `ttl` (tests use a tiny value).
    pub fn with_ttl(ttl: Duration) -> Self {
        PathGrants {
            ttl,
            grants: Mutex::new(Vec::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Grant>> {
        self.grants.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Record `path` for `purpose`; returns the canonical path that was granted.
    pub fn grant(&self, path: &Path, purpose: Purpose) -> Result<PathBuf> {
        let path = canonical(path)?;
        let now = Instant::now();
        let mut grants = self.lock();
        grants.retain(|g| g.expires > now);
        grants.push(Grant {
            path: path.clone(),
            purpose,
            expires: now + self.ttl,
        });
        Ok(path)
    }

    /// The canonical path when `path` was granted for `purpose` (exactly, or through a granted
    /// directory), otherwise `Error::Permission`.
    pub fn check(&self, path: &Path, purpose: Purpose) -> Result<PathBuf> {
        let denied = || Error::Permission(format!("path not granted: {}", path.display()));
        let canon = canonical(path).map_err(|_| denied())?;
        let now = Instant::now();
        let mut grants = self.lock();
        grants.retain(|g| g.expires > now);
        let ok = grants.iter().any(|g| {
            (g.purpose == purpose && g.path == canon)
                || (g.purpose == Purpose::Dir
                    && purpose != Purpose::Dir
                    && canon.starts_with(&g.path))
        });
        if ok { Ok(canon) } else { Err(denied()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn granted_path_passes_for_its_purpose_only() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.csv");
        std::fs::write(&file, "x").unwrap();
        let grants = PathGrants::new();
        assert!(grants.check(&file, Purpose::Read).is_err());
        let canon = grants.grant(&file, Purpose::Read).unwrap();
        assert_eq!(grants.check(&file, Purpose::Read).unwrap(), canon);
        assert_eq!(
            grants.check(&file, Purpose::Write).unwrap_err().kind(),
            "permission"
        );
    }

    #[test]
    fn new_files_are_canonicalised_through_their_parent() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out").join("..").join("new.sql");
        std::fs::create_dir(dir.path().join("out")).unwrap();
        let grants = PathGrants::new();
        grants.grant(&target, Purpose::Write).unwrap();
        // The same file spelled differently is the same grant.
        assert!(
            grants
                .check(&dir.path().join("new.sql"), Purpose::Write)
                .is_ok()
        );
        // A missing parent cannot be canonicalised.
        assert!(
            grants
                .grant(&dir.path().join("nope/x"), Purpose::Write)
                .is_err()
        );
    }

    #[test]
    fn directory_grant_covers_files_inside_but_not_outside() {
        let dir = tempfile::tempdir().unwrap();
        let inner = dir.path().join("backups");
        std::fs::create_dir(&inner).unwrap();
        let other = dir.path().join("other");
        std::fs::create_dir(&other).unwrap();
        let grants = PathGrants::new();
        grants.grant(&inner, Purpose::Dir).unwrap();
        assert!(
            grants
                .check(&inner.join("x.dbsbak"), Purpose::Write)
                .is_ok()
        );
        assert!(grants.check(&inner, Purpose::Dir).is_ok());
        assert!(
            grants
                .check(&other.join("x.dbsbak"), Purpose::Write)
                .is_err()
        );
        // `backups-evil` shares a string prefix but is not inside the directory.
        std::fs::create_dir(dir.path().join("backups-evil")).unwrap();
        assert!(
            grants
                .check(&dir.path().join("backups-evil").join("x"), Purpose::Write)
                .is_err()
        );
    }

    #[test]
    fn grants_expire() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        let grants = PathGrants::with_ttl(Duration::from_millis(20));
        grants.grant(&file, Purpose::Write).unwrap();
        assert!(grants.check(&file, Purpose::Write).is_ok());
        std::thread::sleep(Duration::from_millis(60));
        assert!(grants.check(&file, Purpose::Write).is_err());
    }
}
