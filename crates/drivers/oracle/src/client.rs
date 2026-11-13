//! Oracle Client (Instant Client) discovery.
//!
//! ODPI-C is compiled into the `oracle` crate but loads `libclntsh` at run time. The probe below
//! runs `InitParams::init` (optionally with a configured library directory) and caches the
//! outcome: a success is kept for the life of the process (the client cannot be unloaded), a
//! failure is kept until [`refresh`] or [`configure`] asks for a new attempt, so a user who
//! installs the client or points the settings at it does not have to restart the app.

use std::path::PathBuf;
use std::sync::Mutex;

use dbstudio_core::UnavailableReason;

/// `DPI-1050`: the client library is older than ODPI-C supports.
const DPI_CLIENT_TOO_OLD: [i32; 2] = [1050, 1072];

/// Environment variable that supplies the client directory when no setting is configured.
pub const LIB_DIR_ENV: &str = "DBSTUDIO_ORACLE_CLIENT_LIB_DIR";

/// Outcome of loading the client library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// Loaded; the string is the client version (`23.5.0.24.7`).
    Loaded(String),
    /// Not usable.
    Failed {
        /// ODPI-C error number (`1047` = library not found), if any.
        dpi_code: Option<i32>,
        /// Error text from ODPI-C (it lists the locations that were searched).
        message: String,
    },
}

impl Probe {
    /// Why the driver is unavailable, or `None` when the client loaded.
    pub fn unavailable_reason(&self) -> Option<UnavailableReason> {
        match self {
            Probe::Loaded(_) => None,
            Probe::Failed {
                dpi_code: Some(code),
                ..
            } if DPI_CLIENT_TOO_OLD.contains(code) => Some(UnavailableReason::NativeClientTooOld),
            Probe::Failed { .. } => Some(UnavailableReason::NativeClientMissing),
        }
    }
}

#[derive(Debug, Default)]
struct State {
    lib_dir: Option<String>,
    probe: Option<Probe>,
}

static STATE: Mutex<State> = Mutex::new(State {
    lib_dir: None,
    probe: None,
});

fn lock() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

fn normalize(dir: Option<&str>) -> Option<String> {
    dir.map(str::trim)
        .filter(|d| !d.is_empty())
        .map(str::to_string)
}

/// Set the directory that holds the Instant Client libraries (the `oracle_client` setting).
/// `None` or an empty string means "search the default places". A change forgets a failed probe;
/// once the client is loaded the directory can no longer change (a restart is needed).
pub fn configure(lib_dir: Option<&str>) {
    let dir = normalize(lib_dir);
    let mut st = lock();
    if st.lib_dir != dir {
        st.lib_dir = dir;
        if !matches!(st.probe, Some(Probe::Loaded(_))) {
            st.probe = None;
        }
    }
}

/// Directory configured with [`configure`] (or from [`LIB_DIR_ENV`]).
pub fn configured_dir() -> Option<String> {
    lock()
        .lib_dir
        .clone()
        .or_else(|| normalize(std::env::var(LIB_DIR_ENV).ok().as_deref()))
}

/// Forget a failed probe so that the next [`probe`] tries again.
pub fn refresh() {
    let mut st = lock();
    if !matches!(st.probe, Some(Probe::Loaded(_))) {
        st.probe = None;
    }
}

/// Load the client (once) and report the outcome.
pub fn probe() -> Probe {
    if let Some(p) = lock().probe.clone() {
        return p;
    }
    // Run outside the lock: loading a shared library can take a moment.
    let dir = configured_dir();
    let outcome = load(dir.as_deref());
    let mut st = lock();
    st.probe.get_or_insert(outcome).clone()
}

fn load(dir: Option<&str>) -> Probe {
    let failed = |e: oracle::Error| Probe::Failed {
        dpi_code: e.dpi_code(),
        message: e.to_string(),
    };
    let mut params = oracle::InitParams::new();
    if let Some(dir) = dir
        && let Err(e) = params.oracle_client_lib_dir(dir)
    {
        return failed(e);
    }
    if let Err(e) = params.init() {
        return failed(e);
    }
    match oracle::Version::client() {
        Ok(v) => Probe::Loaded(v.to_string()),
        Err(e) => failed(e),
    }
}

/// Download page of Instant Client for the running OS and CPU.
pub fn download_url() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => {
            "https://www.oracle.com/database/technologies/instant-client/macos-arm64-downloads.html"
        }
        ("macos", _) => {
            "https://www.oracle.com/database/technologies/instant-client/macos-intel-x86-downloads.html"
        }
        ("windows", _) => {
            "https://www.oracle.com/database/technologies/instant-client/winx64-64-downloads.html"
        }
        ("linux", "aarch64") => {
            "https://www.oracle.com/database/technologies/instant-client/linux-arm-aarch64-downloads.html"
        }
        ("linux", _) => {
            "https://www.oracle.com/database/technologies/instant-client/linux-x86-64-downloads.html"
        }
        _ => "https://www.oracle.com/database/technologies/instant-client/downloads.html",
    }
}

/// Name of the main client library on this OS.
fn library_name() -> &'static str {
    match std::env::consts::OS {
        "macos" => "libclntsh.dylib",
        "windows" => "oci.dll",
        _ => "libclntsh.so",
    }
}

/// Places where the library is looked for, best effort (ODPI-C documents these per platform).
pub fn search_paths() -> Vec<String> {
    let lib = library_name();
    let mut out: Vec<PathBuf> = Vec::new();
    if let Some(dir) = configured_dir() {
        out.push(PathBuf::from(dir).join(lib));
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        out.push(dir.join(lib));
    }
    if let Ok(home) = std::env::var("ORACLE_HOME") {
        out.push(PathBuf::from(&home).join("lib").join(lib));
        if std::env::consts::OS == "windows" {
            out.push(PathBuf::from(&home).join("bin").join(lib));
        }
    }
    match std::env::consts::OS {
        "macos" => {
            if let Ok(home) = std::env::var("HOME") {
                out.push(
                    PathBuf::from(&home)
                        .join("Downloads/instantclient_*")
                        .join(lib),
                );
                out.push(PathBuf::from(&home).join("lib").join(lib));
            }
            out.push(PathBuf::from("/usr/local/lib").join(lib));
        }
        "linux" => {
            if let Ok(paths) = std::env::var("LD_LIBRARY_PATH") {
                out.extend(
                    paths
                        .split(':')
                        .filter(|p| !p.is_empty())
                        .map(|p| PathBuf::from(p).join(lib)),
                );
            }
            out.push(PathBuf::from("/etc/ld.so.cache (ldconfig)").join(lib));
        }
        "windows" => {
            if let Ok(path) = std::env::var("PATH") {
                out.extend(
                    path.split(';')
                        .filter(|p| !p.is_empty())
                        .map(|p| PathBuf::from(p).join(lib)),
                );
            }
        }
        _ => {}
    }
    out.into_iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect()
}

/// Everything the UI needs to explain the state of the Oracle client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientDiagnostics {
    /// The client loaded.
    pub available: bool,
    /// Why it did not (when `available` is false).
    pub reason: Option<UnavailableReason>,
    /// Client version when loaded.
    pub version: Option<String>,
    /// Directory from the settings / environment, if any.
    pub configured_dir: Option<String>,
    /// Error text reported by ODPI-C (lists what it tried), when not available.
    pub error: Option<String>,
    /// Locations looked at, best effort.
    pub tried_paths: Vec<String>,
    /// Short instructions on how to install the client.
    pub install_hint: String,
    /// Download page for this OS and CPU.
    pub download_url: &'static str,
}

/// Re-probe (forgetting an earlier failure) and describe the result.
pub fn diagnostics() -> ClientDiagnostics {
    refresh();
    let probe = probe();
    let configured_dir = configured_dir();
    let tried_paths = search_paths();
    let (available, reason, version, error) = match &probe {
        Probe::Loaded(v) => (true, None, Some(v.clone()), None),
        Probe::Failed { message, .. } => (
            false,
            probe.unavailable_reason(),
            None,
            Some(message.clone()),
        ),
    };
    let install_hint = install_hint(reason, configured_dir.as_deref());
    ClientDiagnostics {
        available,
        reason,
        version,
        configured_dir,
        error,
        tried_paths,
        install_hint,
        download_url: download_url(),
    }
}

fn install_hint(reason: Option<UnavailableReason>, configured: Option<&str>) -> String {
    let os = match std::env::consts::OS {
        "macos" => "macOS",
        "windows" => "Windows",
        _ => "Linux",
    };
    match reason {
        None => "Oracle Instant Client is loaded.".to_string(),
        Some(UnavailableReason::NativeClientTooOld) => format!(
            "The installed Oracle Instant Client is too old. Download the Basic or Basic Light \
             package (version 19 or newer) for {os} from the Oracle website, unzip it and select \
             its folder in Settings > Oracle client."
        ),
        Some(UnavailableReason::NativeClientMissing) => {
            let mut s = format!(
                "Oracle Instant Client was not found. Download the Basic or Basic Light package \
                 (version 19 or newer, 64-bit, matching this app) for {os} from the Oracle website, \
                 unzip it and select its folder in Settings > Oracle client."
            );
            if let Some(dir) = configured {
                s.push_str(&format!(
                    " The configured folder \"{dir}\" does not contain a usable client library."
                ));
            }
            if std::env::consts::OS == "linux" {
                s.push_str(
                    " On Linux you can also install the package and run ldconfig, or set LD_LIBRARY_PATH.",
                );
            }
            s
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn too_old_codes_map_to_too_old() {
        let p = Probe::Failed {
            dpi_code: Some(1050),
            message: "old".into(),
        };
        assert_eq!(
            p.unavailable_reason(),
            Some(UnavailableReason::NativeClientTooOld)
        );
        let p = Probe::Failed {
            dpi_code: Some(1047),
            message: "missing".into(),
        };
        assert_eq!(
            p.unavailable_reason(),
            Some(UnavailableReason::NativeClientMissing)
        );
        assert_eq!(Probe::Loaded("23.5".into()).unavailable_reason(), None);
    }

    #[test]
    fn download_url_points_at_oracle() {
        assert!(download_url().starts_with("https://www.oracle.com/database/technologies/"));
    }

    #[test]
    fn hints_name_the_settings_page() {
        let hint = install_hint(Some(UnavailableReason::NativeClientMissing), Some("/nope"));
        assert!(hint.contains("Instant Client") && hint.contains("/nope"));
        assert!(
            install_hint(Some(UnavailableReason::NativeClientTooOld), None).contains("too old")
        );
    }

    #[test]
    fn diagnostics_never_panic_and_are_consistent() {
        let d = diagnostics();
        assert_eq!(d.available, d.reason.is_none());
        assert_eq!(d.available, d.version.is_some());
        assert_eq!(d.available, d.error.is_none());
        assert!(!d.install_hint.is_empty());
        if !d.available {
            assert!(!d.tried_paths.is_empty() || std::env::consts::OS == "freebsd");
        }
    }
}
