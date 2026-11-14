//! Business layer combining the SQLite [`Store`] and the [`SecretStore`].

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use dbstudio_core::{Error, Result, Secret};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::db::{Store, now_iso};
use crate::model::{ConnectionGroup, ConnectionProfile, ConnectionsSnapshot, SecretChange};
use crate::secret::SecretStore;
use crate::validate::{IMPORT_OPTION_ALLOWLIST, check_text, is_valid_color};

/// Largest import file accepted (bytes).
pub const MAX_IMPORT_BYTES: u64 = 5 * 1024 * 1024;
/// Most profiles one import file may contain.
pub const MAX_IMPORT_PROFILES: usize = 5_000;
/// Most groups one import file may contain.
pub const MAX_IMPORT_GROUPS: usize = 2_000;

/// What an import changed besides adding rows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImportReport {
    /// Profiles added.
    pub profiles: usize,
    /// Groups added.
    pub groups: usize,
    /// `options` entries removed because their key is not in the import allow-list.
    pub dropped_options: usize,
    /// Profiles whose invalid color was reset.
    pub reset_colors: usize,
}

/// Value of the `format` field of an export file.
pub const EXPORT_FORMAT: &str = "dbstudio-connections";
/// Current export file version.
pub const EXPORT_VERSION: u32 = 1;

/// JSON document produced by export and consumed by import. Never contains secrets.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExportFile {
    format: String,
    version: u32,
    groups: Vec<ConnectionGroup>,
    profiles: Vec<ConnectionProfile>,
}

/// Connection management: profiles in SQLite, passwords in the keychain.
pub struct ConnectionService {
    store: Store,
    secrets: Arc<dyn SecretStore>,
}

impl std::fmt::Debug for ConnectionService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionService").finish_non_exhaustive()
    }
}

impl ConnectionService {
    /// Combine a store and a secret store.
    pub fn new(store: Store, secrets: Arc<dyn SecretStore>) -> Self {
        ConnectionService { store, secrets }
    }

    /// The underlying SQLite store.
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Application settings.
    pub fn settings(&self) -> Result<crate::settings::AppSettings> {
        self.store.get_settings()
    }

    /// Store application settings (normalized); returns what was stored.
    pub fn set_settings(
        &self,
        settings: crate::settings::AppSettings,
    ) -> Result<crate::settings::AppSettings> {
        self.store.set_settings(settings)
    }

    /// All groups and profiles.
    pub fn snapshot(&self) -> Result<ConnectionsSnapshot> {
        self.store.snapshot()
    }

    /// One profile by id.
    pub fn get_profile(&self, id: Uuid) -> Result<ConnectionProfile> {
        self.store.get_profile(id)
    }

    /// Save a profile and apply `secret`. The profile is persisted first; when the keychain
    /// then fails the profile stays saved and the returned error says only the password failed.
    pub fn save(
        &self,
        mut profile: ConnectionProfile,
        secret: SecretChange,
    ) -> Result<ConnectionProfile> {
        let id = profile.config.id;
        let had_password = self.store.find_profile(id)?.is_some_and(|p| p.has_password);
        // has_password is owned by the service, never trusted from the client.
        profile.has_password = had_password;
        let mut saved = self.store.save_profile(&profile)?;

        let wanted = match (&secret, profile.save_password) {
            (SecretChange::Set { value }, true) => Some(Some(value)),
            (SecretChange::Keep, true) => None,
            // Not saving passwords, or an explicit clear: drop whatever is stored.
            _ => Some(None),
        };
        let new_state = match wanted {
            None => had_password,
            Some(Some(value)) => {
                self.secrets.set(id, value).map_err(partial("stored"))?;
                true
            }
            Some(None) => {
                if had_password {
                    self.secrets.delete(id).map_err(partial("removed"))?;
                }
                false
            }
        };
        if new_state != had_password {
            self.store.set_has_password(id, new_state)?;
        }
        saved.has_password = new_state;
        Ok(saved)
    }

    /// Delete a profile and its stored secret. The row is removed first; when the keychain then
    /// fails the profile stays deleted (so a locked keychain never blocks cleanup) and the
    /// returned error says the password may remain in the keychain.
    pub fn delete(&self, id: Uuid) -> Result<()> {
        let profile = self.store.get_profile(id)?;
        self.store.delete_profile(id)?;
        if profile.has_password {
            self.secrets
                .delete(id)
                .map_err(|e| Error::Storage(format!("connection deleted, but its password could not be removed from the keychain: {}", detail(&e))))?;
        }
        if profile.config.ssh.is_some() {
            self.secrets
                .delete_account(&crate::secret::ssh_secret_account(id))
                .map_err(|e| Error::Storage(format!("connection deleted, but its SSH secret could not be removed from the keychain: {}", detail(&e))))?;
        }
        Ok(())
    }

    /// Duplicate a profile, copying its stored secret when it has one.
    pub fn duplicate(&self, id: Uuid) -> Result<ConnectionProfile> {
        let source = self.store.get_profile(id)?;
        let mut copy = self.store.duplicate_profile(id)?;
        if source.has_password {
            let copied = self.secrets.get(id).and_then(|secret| match secret {
                Some(s) => self.secrets.set(copy.config.id, &s).map(|()| true),
                None => Ok(false),
            });
            match copied {
                Ok(true) => {
                    self.store.set_has_password(copy.config.id, true)?;
                    copy.has_password = true;
                }
                Ok(false) => {}
                Err(e) => {
                    return Err(Error::Storage(format!(
                        "connection duplicated, but its password could not be copied: {}",
                        detail(&e)
                    )));
                }
            }
        }
        Ok(copy)
    }

    /// Insert or update a group.
    pub fn save_group(&self, group: ConnectionGroup) -> Result<ConnectionGroup> {
        self.store.save_group(&group)
    }

    /// Delete a group; children move to its parent.
    pub fn delete_group(&self, id: Uuid) -> Result<()> {
        self.store.delete_group(id)
    }

    /// Secret to use when testing a connection: `Set` uses the supplied value, `Keep` reads
    /// the stored one, `Clear` yields none.
    pub fn resolve_secret(
        &self,
        profile: &ConnectionProfile,
        change: &SecretChange,
    ) -> Result<Option<Secret>> {
        match change {
            SecretChange::Set { value } => Ok(Some(value.clone())),
            SecretChange::Clear => Ok(None),
            SecretChange::Keep => {
                let stored = self.store.find_profile(profile.config.id)?;
                if stored.is_some_and(|p| p.has_password) {
                    self.secrets.get(profile.config.id)
                } else {
                    Ok(None)
                }
            }
        }
    }

    /// Profile plus the secret to open a session with: the stored password when the profile
    /// has one, otherwise `password`. A keychain failure is tolerated when `password` is given.
    pub fn session_credentials(
        &self,
        id: Uuid,
        password: Option<Secret>,
    ) -> Result<(ConnectionProfile, Option<Secret>)> {
        let profile = self.store.get_profile(id)?;
        let secret = if profile.save_password && profile.has_password {
            match self.secrets.get(id) {
                Ok(Some(s)) => Some(s),
                Ok(None) => password,
                Err(e) if password.is_some() => {
                    tracing::warn!(connection = %id, error = %e, "keychain unavailable, using supplied password");
                    password
                }
                Err(e) => return Err(e),
            }
        } else {
            password
        };
        Ok((profile, secret))
    }

    /// Serialize profiles (all when `ids` is `None`) and the groups they need to JSON. Returns
    /// the document and the number of profiles. Secrets are never included.
    pub fn export_json(&self, ids: Option<&[Uuid]>) -> Result<(String, u32)> {
        let snap = self.store.snapshot()?;
        let profiles: Vec<ConnectionProfile> = match ids {
            None => snap.profiles,
            Some(ids) => {
                let wanted: HashSet<Uuid> = ids.iter().copied().collect();
                if let Some(missing) = wanted
                    .iter()
                    .find(|id| !snap.profiles.iter().any(|p| p.config.id == **id))
                {
                    return Err(Error::NotFound(format!("connection {missing}")));
                }
                snap.profiles
                    .into_iter()
                    .filter(|p| wanted.contains(&p.config.id))
                    .collect()
            }
        };
        let groups = match ids {
            None => snap.groups,
            Some(_) => {
                let by_id: HashMap<Uuid, &ConnectionGroup> =
                    snap.groups.iter().map(|g| (g.id, g)).collect();
                let mut needed = HashSet::new();
                for p in &profiles {
                    let mut cursor = p.group_id;
                    while let Some(g) = cursor {
                        if !needed.insert(g) {
                            break;
                        }
                        cursor = by_id.get(&g).and_then(|g| g.parent_id);
                    }
                }
                snap.groups
                    .into_iter()
                    .filter(|g| needed.contains(&g.id))
                    .collect()
            }
        };
        let count = profiles.len() as u32;
        let file = ExportFile {
            format: EXPORT_FORMAT.into(),
            version: EXPORT_VERSION,
            groups,
            profiles: profiles
                .into_iter()
                .map(|mut p| {
                    p.has_password = false;
                    p
                })
                .collect(),
        };
        let json = serde_json::to_string_pretty(&file)
            .map_err(|e| Error::Storage(format!("cannot serialize export: {e}")))?;
        Ok((json, count))
    }

    /// Write an export file; returns the number of exported profiles.
    ///
    /// Only `.json` paths are accepted. The data is written to a temporary file next to the
    /// target and renamed into place, so a failure never leaves a half-written export.
    pub fn export_to_file(&self, path: impl AsRef<Path>, ids: Option<&[Uuid]>) -> Result<u32> {
        let path = path.as_ref();
        let is_json = path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("json"));
        if !is_json {
            return Err(Error::Invalid(
                "export file must have a .json extension".into(),
            ));
        }
        let file_name = path
            .file_name()
            .ok_or_else(|| Error::Invalid("export path has no file name".into()))?;
        let (json, count) = self.export_json(ids)?;
        let mut tmp_name = std::ffi::OsString::from(".");
        tmp_name.push(file_name);
        tmp_name.push(format!(".{}.tmp", Uuid::new_v4().simple()));
        let tmp = path.with_file_name(tmp_name);
        let written = std::fs::write(&tmp, json).and_then(|()| std::fs::rename(&tmp, path));
        if let Err(e) = written {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        Ok(count)
    }

    /// Import an export document. Everything gets a fresh id, nothing existing is overwritten.
    pub fn import_json(&self, json: &str) -> Result<ConnectionsSnapshot> {
        self.import_json_report(json).map(|(snap, _)| snap)
    }

    /// Like [`ConnectionService::import_json`], also reporting what was sanitized. Only
    /// allow-listed `options` keys survive, invalid colors are reset and limits are enforced.
    pub fn import_json_report(&self, json: &str) -> Result<(ConnectionsSnapshot, ImportReport)> {
        let bad = |m: String| Error::Invalid(format!("invalid connections file: {m}"));
        if json.len() as u64 > MAX_IMPORT_BYTES {
            return Err(bad(format!(
                "file is larger than {} MB",
                MAX_IMPORT_BYTES >> 20
            )));
        }
        let raw: serde_json::Value =
            serde_json::from_str(json).map_err(|e| bad(format!("not valid JSON ({e})")))?;
        if raw.get("format").and_then(|f| f.as_str()) != Some(EXPORT_FORMAT) {
            return Err(bad(format!("missing format \"{EXPORT_FORMAT}\"")));
        }
        match raw.get("version").and_then(|v| v.as_u64()) {
            Some(v) if v == u64::from(EXPORT_VERSION) => {}
            Some(v) => return Err(bad(format!("unsupported version {v}"))),
            None => return Err(bad("missing version".into())),
        }
        let file: ExportFile = serde_json::from_value(raw).map_err(|e| bad(e.to_string()))?;
        if file.profiles.len() > MAX_IMPORT_PROFILES || file.groups.len() > MAX_IMPORT_GROUPS {
            return Err(bad(format!(
                "too many entries (limit {MAX_IMPORT_PROFILES} profiles, {MAX_IMPORT_GROUPS} groups)"
            )));
        }
        for g in &file.groups {
            check_text("group name", &g.name).map_err(|e| bad(e.to_string()))?;
        }

        let group_map: HashMap<Uuid, Uuid> =
            file.groups.iter().map(|g| (g.id, Uuid::new_v4())).collect();
        if group_map.len() != file.groups.len() {
            return Err(bad("duplicate group ids".into()));
        }
        // Detect parent loops in the file before touching the database.
        let parents: HashMap<Uuid, Uuid> = file
            .groups
            .iter()
            .filter_map(|g| g.parent_id.map(|p| (g.id, p)))
            .collect();
        for g in &file.groups {
            let mut seen = HashSet::from([g.id]);
            let mut cursor = parents.get(&g.id);
            while let Some(p) = cursor {
                if !seen.insert(*p) {
                    return Err(bad("groups form a loop".into()));
                }
                cursor = parents.get(p);
            }
        }
        let groups: Vec<ConnectionGroup> = file
            .groups
            .iter()
            .map(|g| ConnectionGroup {
                id: group_map[&g.id],
                name: g.name.clone(),
                parent_id: g.parent_id.and_then(|p| group_map.get(&p).copied()),
                sort_order: g.sort_order,
            })
            .collect();

        let base = self
            .store
            .snapshot()?
            .profiles
            .iter()
            .map(|p| p.sort_order + 1)
            .max()
            .unwrap_or(0);
        let now = now_iso();
        let mut report = ImportReport {
            groups: groups.len(),
            ..ImportReport::default()
        };
        let profiles: Vec<ConnectionProfile> = file
            .profiles
            .into_iter()
            .enumerate()
            .map(|(i, mut p)| {
                let before = p.config.options.len();
                p.config.options.retain(|k, v| {
                    IMPORT_OPTION_ALLOWLIST.contains(&k.as_str()) && check_text("option", v).is_ok()
                });
                report.dropped_options += before - p.config.options.len();
                if p.color.as_deref().is_some_and(|c| !is_valid_color(c)) {
                    p.color = None;
                    report.reset_colors += 1;
                }
                p.config.id = Uuid::new_v4();
                p.group_id = p.group_id.and_then(|g| group_map.get(&g).copied());
                p.has_password = false;
                p.sort_order = base + i as i64;
                p.created_at = now.clone();
                p.updated_at = now.clone();
                p
            })
            .collect();
        report.profiles = profiles.len();
        self.store.insert_all(&groups, &profiles)?;
        Ok((self.store.snapshot()?, report))
    }

    /// Import from a file on disk.
    ///
    /// Only regular files up to [`MAX_IMPORT_BYTES`] are read (no devices, directories or
    /// oversized files; the limit is enforced while reading too, not just from metadata).
    pub fn import_from_file(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<(ConnectionsSnapshot, ImportReport)> {
        let file = std::fs::File::open(path.as_ref())?;
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Err(Error::Invalid("import path is not a regular file".into()));
        }
        if meta.len() > MAX_IMPORT_BYTES {
            return Err(Error::Invalid(format!(
                "import file is larger than {} MB",
                MAX_IMPORT_BYTES >> 20
            )));
        }
        let mut bytes = Vec::new();
        file.take(MAX_IMPORT_BYTES + 1).read_to_end(&mut bytes)?;
        let text = String::from_utf8(bytes)
            .map_err(|_| Error::Invalid("import file is not valid UTF-8 text".into()))?;
        self.import_json_report(&text)
    }
}

fn detail(e: &Error) -> String {
    match e {
        Error::Storage(m) => m.clone(),
        other => other.to_string(),
    }
}

fn partial(action: &'static str) -> impl Fn(Error) -> Error {
    move |e| {
        Error::Storage(format!(
            "connection saved, but its password could not be {action}: {}",
            detail(&e)
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secret::MemorySecretStore;
    use dbstudio_core::{ConnectionConfig, DbKind};

    fn service() -> (ConnectionService, Arc<MemorySecretStore>) {
        let secrets = Arc::new(MemorySecretStore::new());
        let svc = ConnectionService::new(Store::open_in_memory().unwrap(), secrets.clone());
        (svc, secrets)
    }

    fn profile(name: &str, save_password: bool) -> ConnectionProfile {
        ConnectionProfile {
            config: ConnectionConfig::new(DbKind::MySql, name, "db.local"),
            group_id: None,
            color: None,
            save_password,
            has_password: false,
            sort_order: 0,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn set(v: &str) -> SecretChange {
        SecretChange::Set {
            value: Secret::new(v),
        }
    }

    fn stored(s: &MemorySecretStore, id: Uuid) -> Option<String> {
        s.get(id).unwrap().map(|x| x.expose().to_string())
    }

    #[test]
    fn set_stores_secret_only_when_save_password() {
        let (svc, secrets) = service();
        let p = svc.save(profile("a", true), set("pw")).unwrap();
        assert!(p.has_password);
        assert_eq!(stored(&secrets, p.config.id).as_deref(), Some("pw"));
        assert!(svc.get_profile(p.config.id).unwrap().has_password);

        let q = svc.save(profile("b", false), set("pw")).unwrap();
        assert!(!q.has_password);
        assert!(stored(&secrets, q.config.id).is_none());
    }

    #[test]
    fn client_supplied_has_password_is_ignored() {
        let (svc, secrets) = service();
        let mut p = profile("a", true);
        p.has_password = true;
        let saved = svc.save(p, SecretChange::Keep).unwrap();
        assert!(!saved.has_password);
        assert!(secrets.is_empty());
    }

    #[test]
    fn keep_preserves_and_clear_removes() {
        let (svc, secrets) = service();
        let p = svc.save(profile("a", true), set("pw")).unwrap();
        let mut edit = p.clone();
        edit.config.name = "renamed".into();
        let kept = svc.save(edit.clone(), SecretChange::Keep).unwrap();
        assert!(kept.has_password);
        assert_eq!(stored(&secrets, p.config.id).as_deref(), Some("pw"));

        let cleared = svc.save(edit, SecretChange::Clear).unwrap();
        assert!(!cleared.has_password);
        assert!(secrets.is_empty());
        assert!(!svc.get_profile(p.config.id).unwrap().has_password);
    }

    #[test]
    fn disabling_save_password_removes_stored_secret() {
        let (svc, secrets) = service();
        let p = svc.save(profile("a", true), set("pw")).unwrap();
        let mut edit = p.clone();
        edit.save_password = false;
        let saved = svc.save(edit, SecretChange::Keep).unwrap();
        assert!(!saved.has_password);
        assert!(secrets.is_empty());

        // Re-enabling with Keep does not resurrect anything.
        let mut again = saved;
        again.save_password = true;
        assert!(!svc.save(again, SecretChange::Keep).unwrap().has_password);
    }

    #[test]
    fn set_replaces_existing_secret() {
        let (svc, secrets) = service();
        let p = svc.save(profile("a", true), set("old")).unwrap();
        svc.save(p.clone(), set("new")).unwrap();
        assert_eq!(stored(&secrets, p.config.id).as_deref(), Some("new"));
    }

    #[test]
    fn delete_also_removes_ssh_secret() {
        let (svc, secrets) = service();
        let mut p = profile("a", false);
        p.config.ssh = Some(dbstudio_core::SshConfig {
            host: "bastion".into(),
            port: 22,
            username: "u".into(),
            auth: dbstudio_core::SshAuth::Password,
            keepalive_secs: None,
            connect_timeout_ms: None,
        });
        let p = svc.save(p, SecretChange::Keep).unwrap();
        secrets
            .set_account(
                &crate::secret::ssh_secret_account(p.config.id),
                &dbstudio_core::Secret::new("tunnel"),
            )
            .unwrap();
        assert_eq!(secrets.len(), 1);
        svc.delete(p.config.id).unwrap();
        assert!(secrets.is_empty());
    }

    #[test]
    fn delete_removes_secret_and_profile() {
        let (svc, secrets) = service();
        let p = svc.save(profile("a", true), set("pw")).unwrap();
        svc.delete(p.config.id).unwrap();
        assert!(secrets.is_empty());
        assert!(svc.snapshot().unwrap().profiles.is_empty());
        assert!(matches!(svc.delete(p.config.id), Err(Error::NotFound(_))));
    }

    #[test]
    fn duplicate_copies_secret() {
        let (svc, secrets) = service();
        let p = svc.save(profile("a", true), set("pw")).unwrap();
        let copy = svc.duplicate(p.config.id).unwrap();
        assert!(copy.has_password);
        assert_eq!(copy.config.name, "a (copy)");
        assert_eq!(stored(&secrets, copy.config.id).as_deref(), Some("pw"));
        assert_eq!(stored(&secrets, p.config.id).as_deref(), Some("pw"));
        assert!(svc.get_profile(copy.config.id).unwrap().has_password);

        let plain = svc.save(profile("b", false), SecretChange::Keep).unwrap();
        let plain_copy = svc.duplicate(plain.config.id).unwrap();
        assert!(!plain_copy.has_password);
        assert_eq!(secrets.len(), 2);
    }

    #[test]
    fn resolve_secret_branches() {
        let (svc, _secrets) = service();
        let p = svc.save(profile("a", true), set("stored")).unwrap();
        let resolve = |c: &SecretChange| {
            svc.resolve_secret(&p, c)
                .unwrap()
                .map(|s| s.expose().to_string())
        };
        assert_eq!(resolve(&set("typed")).as_deref(), Some("typed"));
        assert_eq!(resolve(&SecretChange::Keep).as_deref(), Some("stored"));
        assert_eq!(resolve(&SecretChange::Clear), None);

        // An unsaved profile has nothing to keep.
        let unsaved = profile("new", true);
        assert!(
            svc.resolve_secret(&unsaved, &SecretChange::Keep)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn session_credentials_prefers_stored_then_supplied() {
        let (svc, secrets) = service();
        let saved = svc.save(profile("a", true), set("stored")).unwrap();
        let (_, s) = svc
            .session_credentials(saved.config.id, Some(Secret::new("typed")))
            .unwrap();
        assert_eq!(s.unwrap().expose(), "stored");

        let ask = svc.save(profile("b", false), SecretChange::Keep).unwrap();
        let (_, s) = svc.session_credentials(ask.config.id, None).unwrap();
        assert!(s.is_none());
        let (_, s) = svc
            .session_credentials(ask.config.id, Some(Secret::new("typed")))
            .unwrap();
        assert_eq!(s.unwrap().expose(), "typed");

        // Keychain down: supplied password still works, otherwise a storage error.
        secrets.set_unavailable(true);
        let (_, s) = svc
            .session_credentials(saved.config.id, Some(Secret::new("typed")))
            .unwrap();
        assert_eq!(s.unwrap().expose(), "typed");
        assert!(matches!(
            svc.session_credentials(saved.config.id, None),
            Err(Error::Storage(_))
        ));
        assert!(matches!(
            svc.session_credentials(Uuid::new_v4(), None),
            Err(Error::NotFound(_))
        ));
    }

    #[test]
    fn keychain_failure_keeps_profile_and_reports_password_error() {
        let (svc, secrets) = service();
        secrets.set_unavailable(true);
        let p = profile("a", true);
        let err = svc.save(p.clone(), set("pw")).unwrap_err();
        assert!(matches!(&err, Error::Storage(m) if m.contains("connection saved")));
        let loaded = svc.get_profile(p.config.id).unwrap();
        assert!(!loaded.has_password);
        // Saving without touching the secret works with a dead keychain.
        svc.save(loaded, SecretChange::Keep).unwrap();
        // Saving with save_password off needs no keychain access either.
        svc.save(profile("b", false), SecretChange::Clear).unwrap();
    }

    #[test]
    fn export_never_contains_secrets() {
        let (svc, _) = service();
        let g = svc
            .save_group(ConnectionGroup {
                id: Uuid::new_v4(),
                name: "G".into(),
                parent_id: None,
                sort_order: 0,
            })
            .unwrap();
        let mut p = profile("a", true);
        p.group_id = Some(g.id);
        svc.save(p, set("super-secret-pw")).unwrap();
        svc.save(profile("b", false), SecretChange::Keep).unwrap();

        let (json, count) = svc.export_json(None).unwrap();
        assert_eq!(count, 2);
        assert!(!json.contains("super-secret-pw"));
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["format"], "dbstudio-connections");
        assert_eq!(v["version"], 1);
        assert_eq!(v["groups"].as_array().unwrap().len(), 1);
        for p in v["profiles"].as_array().unwrap() {
            assert_eq!(p["hasPassword"], false);
            assert!(p.get("password").is_none());
        }
    }

    #[test]
    fn export_subset_includes_needed_groups() {
        let (svc, _) = service();
        let mk = |name: &str, parent| {
            svc.save_group(ConnectionGroup {
                id: Uuid::new_v4(),
                name: name.into(),
                parent_id: parent,
                sort_order: 0,
            })
            .unwrap()
        };
        let root = mk("root", None);
        let child = mk("child", Some(root.id));
        let _other = mk("other", None);
        let mut p = profile("a", false);
        p.group_id = Some(child.id);
        let p = svc.save(p, SecretChange::Keep).unwrap();
        let _q = svc.save(profile("b", false), SecretChange::Keep).unwrap();

        let (json, count) = svc.export_json(Some(&[p.config.id])).unwrap();
        assert_eq!(count, 1);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["groups"].as_array().unwrap().len(), 2);
        assert!(matches!(
            svc.export_json(Some(&[Uuid::new_v4()])),
            Err(Error::NotFound(_))
        ));
    }

    #[test]
    fn import_assigns_new_ids_and_remaps_groups() {
        let (a, _) = service();
        let root = a
            .save_group(ConnectionGroup {
                id: Uuid::new_v4(),
                name: "root".into(),
                parent_id: None,
                sort_order: 0,
            })
            .unwrap();
        let child = a
            .save_group(ConnectionGroup {
                id: Uuid::new_v4(),
                name: "child".into(),
                parent_id: Some(root.id),
                sort_order: 1,
            })
            .unwrap();
        let mut p = profile("in-child", true);
        p.group_id = Some(child.id);
        let p = a.save(p, set("pw")).unwrap();
        let (json, _) = a.export_json(None).unwrap();

        // Import into the same database: existing data stays, copies get new ids.
        let snap = a.import_json(&json).unwrap();
        assert_eq!(snap.groups.len(), 4);
        assert_eq!(snap.profiles.len(), 2);
        let imported = snap
            .profiles
            .iter()
            .find(|x| x.config.id != p.config.id)
            .unwrap();
        assert!(!imported.has_password);
        assert_eq!(imported.config.name, "in-child");
        let new_child = snap
            .groups
            .iter()
            .find(|g| Some(g.id) == imported.group_id)
            .unwrap();
        assert_ne!(new_child.id, child.id);
        assert_eq!(new_child.name, "child");
        let new_root = snap
            .groups
            .iter()
            .find(|g| Some(g.id) == new_child.parent_id)
            .unwrap();
        assert_ne!(new_root.id, root.id);
        assert_eq!(new_root.parent_id, None);
        // The original is untouched.
        assert_eq!(
            snap.profiles
                .iter()
                .filter(|x| x.config.id == p.config.id)
                .count(),
            1
        );

        // Importing again keeps adding rather than overwriting.
        assert_eq!(a.import_json(&json).unwrap().profiles.len(), 3);
    }

    #[test]
    fn import_rejects_bad_files() {
        let (svc, _) = service();
        let err = |json: &str| match svc.import_json(json) {
            Err(Error::Invalid(m)) => m,
            other => panic!("expected invalid-input error, got {other:?}"),
        };
        assert!(err("not json").contains("not valid JSON"));
        assert!(err("{}").contains("format"));
        assert!(err(r#"{"format":"other","version":1}"#).contains("format"));
        assert!(err(r#"{"format":"dbstudio-connections"}"#).contains("version"));
        assert!(
            err(r#"{"format":"dbstudio-connections","version":2,"groups":[],"profiles":[]}"#)
                .contains("unsupported version 2")
        );
        assert!(
            err(
                r#"{"format":"dbstudio-connections","version":1,"groups":[],"profiles":[{"x":1}]}"#
            )
            .contains("invalid connections file")
        );
        assert!(svc.snapshot().unwrap().profiles.is_empty());
    }

    #[test]
    fn import_rejects_group_loops_and_invalid_profiles_atomically() {
        let (svc, _) = service();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let looped = serde_json::json!({
            "format": "dbstudio-connections", "version": 1, "profiles": [],
            "groups": [
                {"id": a, "name": "a", "parentId": b},
                {"id": b, "name": "b", "parentId": a}
            ]
        });
        assert!(svc.import_json(&looped.to_string()).is_err());

        let mut bad = profile("", false);
        bad.config.name = String::new();
        let doc = serde_json::json!({
            "format": "dbstudio-connections", "version": 1,
            "groups": [{"id": a, "name": "a"}],
            "profiles": [bad]
        });
        assert!(svc.import_json(&doc.to_string()).is_err());
        let snap = svc.snapshot().unwrap();
        assert!(snap.groups.is_empty() && snap.profiles.is_empty());
    }

    #[test]
    fn export_import_via_files() {
        let (svc, _) = service();
        svc.save(profile("a", false), SecretChange::Keep).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.json");
        assert_eq!(svc.export_to_file(&path, None).unwrap(), 1);
        let (snap, report) = svc.import_from_file(&path).unwrap();
        assert_eq!(snap.profiles.len(), 2);
        assert_eq!(report.profiles, 1);
        assert!(matches!(
            svc.import_from_file(dir.path().join("missing.json")),
            Err(Error::Io(_))
        ));
    }

    fn doc(profiles: serde_json::Value) -> String {
        serde_json::json!({
            "format": "dbstudio-connections", "version": 1,
            "groups": [], "profiles": profiles
        })
        .to_string()
    }

    #[test]
    fn import_drops_unsafe_options_and_bad_colors() {
        let (svc, _) = service();
        let mut p = profile("shared", false);
        p.config
            .options
            .insert("init_command".into(), "DROP DATABASE prod".into());
        p.config.options.insert("search_path".into(), "evil".into());
        p.config
            .options
            .insert("application_name".into(), "dbs".into());
        p.config.options.insert("pool_max".into(), "3".into());
        p.color = Some("red;}</style>".into());
        let (snap, report) = svc
            .import_json_report(&doc(serde_json::json!([p])))
            .unwrap();
        assert_eq!(report.dropped_options, 2);
        assert_eq!(report.reset_colors, 1);
        assert_eq!(report.profiles, 1);
        let got = &snap.profiles[0];
        assert_eq!(got.color, None);
        let keys: Vec<_> = got.config.options.keys().map(String::as_str).collect();
        assert_eq!(keys, ["application_name", "pool_max"]);
        // Valid colors survive.
        let mut ok = profile("ok", false);
        ok.color = Some("#abc".into());
        let (snap, report) = svc
            .import_json_report(&doc(serde_json::json!([ok])))
            .unwrap();
        assert_eq!(report.reset_colors, 0);
        assert!(
            snap.profiles
                .iter()
                .any(|p| p.color.as_deref() == Some("#abc"))
        );
    }

    #[test]
    fn import_rejects_bad_text_and_oversized_input() {
        let (svc, _) = service();
        let mut p = profile("bad\nname", false);
        assert!(
            svc.import_json(&doc(serde_json::json!([p.clone()])))
                .is_err()
        );
        p.config.name = "x".repeat(300);
        assert!(svc.import_json(&doc(serde_json::json!([p]))).is_err());
        assert!(svc.snapshot().unwrap().profiles.is_empty());

        let many: Vec<_> = (0..=MAX_IMPORT_PROFILES)
            .map(|i| profile(&format!("p{i}"), false))
            .collect();
        let err = svc.import_json(&doc(serde_json::json!(many))).unwrap_err();
        assert!(err.to_string().contains("too many"), "{err}");

        let huge = " ".repeat(MAX_IMPORT_BYTES as usize + 1);
        assert!(
            svc.import_json(&huge)
                .unwrap_err()
                .to_string()
                .contains("larger")
        );
    }

    #[test]
    fn import_file_must_be_a_small_regular_file() {
        let (svc, _) = service();
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            svc.import_from_file(dir.path()),
            Err(Error::Invalid(_)) | Err(Error::Io(_))
        ));
        let big = dir.path().join("big.json");
        let f = std::fs::File::create(&big).unwrap();
        f.set_len(MAX_IMPORT_BYTES + 1).unwrap();
        let err = svc.import_from_file(&big).unwrap_err();
        assert!(err.to_string().contains("larger"), "{err}");
        let bin = dir.path().join("bin.json");
        std::fs::write(&bin, [0xff, 0xfe, 0x00]).unwrap();
        assert!(svc.import_from_file(&bin).is_err());
    }

    #[test]
    fn export_requires_json_extension_and_leaves_no_temp_files() {
        let (svc, _) = service();
        svc.save(profile("a", false), SecretChange::Keep).unwrap();
        let dir = tempfile::tempdir().unwrap();
        for bad in ["out.txt", "out", "out.json.exe", "out.jsonx"] {
            let err = svc.export_to_file(dir.path().join(bad), None).unwrap_err();
            assert!(matches!(err, Error::Invalid(_)), "{bad}");
        }
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        svc.export_to_file(dir.path().join("OUT.JSON"), None)
            .unwrap();
        // Overwrites atomically and leaves only the target behind.
        svc.export_to_file(dir.path().join("OUT.JSON"), None)
            .unwrap();
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["OUT.JSON"]);
        // A failing write (missing directory) reports an I/O error.
        assert!(matches!(
            svc.export_to_file(dir.path().join("nope/out.json"), None),
            Err(Error::Io(_))
        ));
    }

    #[test]
    fn save_validates_color_and_field_limits() {
        let (svc, _) = service();
        let mut p = profile("a", false);
        p.color = Some("javascript:alert(1)".into());
        assert!(svc.save(p.clone(), SecretChange::Keep).is_err());
        p.color = Some("#12".into());
        assert!(svc.save(p.clone(), SecretChange::Keep).is_err());
        p.color = Some("#a1B2c3".into());
        assert!(svc.save(p.clone(), SecretChange::Keep).is_ok());
        p.color = Some(String::new());
        assert_eq!(svc.save(p.clone(), SecretChange::Keep).unwrap().color, None);
        p.color = None;
        p.config.host = "h\u{7}".into();
        assert!(svc.save(p.clone(), SecretChange::Keep).is_err());
        p.config.host = "h".into();
        p.config.username = Some("u".repeat(256));
        assert!(svc.save(p.clone(), SecretChange::Keep).is_err());
        p.config.username = None;
        p.config.database = Some("a\nb".into());
        assert!(svc.save(p, SecretChange::Keep).is_err());
        let g = ConnectionGroup {
            id: Uuid::new_v4(),
            name: "g\tx".into(),
            parent_id: None,
            sort_order: 0,
        };
        assert!(svc.save_group(g).is_err());
    }
}
