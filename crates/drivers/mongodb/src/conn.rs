//! Connection options, TLS policy, error mapping and the raw command helper.

use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use bson::{Document, doc};
use dbstudio_core::{ConnectionConfig, Error, Result, Secret, TlsMode};
use mongodb::Client;
use mongodb::ClientSession;
use mongodb::error::{ErrorKind, WriteFailure};
use mongodb::options::{AuthMechanism, ClientOptions, Credential, Tls, TlsOptions};

const DEFAULT_CONNECT_MS: u64 = 10_000;
/// Time to try TLS before falling back to plain in `prefer` mode.
const PREFER_TLS_MS: u64 = 4_000;

/// Map a driver error onto [`Error`]. Messages never contain credentials.
pub fn map_error(err: mongodb::error::Error) -> Error {
    let text = err.to_string();
    match err.kind.as_ref() {
        ErrorKind::Authentication { message, .. } => Error::Authentication(message.clone()),
        ErrorKind::Command(c) => match c.code {
            13 => Error::Permission(c.message.clone()),
            18 | 11 => Error::Authentication(c.message.clone()),
            50 | 202 => Error::Timeout,
            11601 | 237 => Error::Cancelled,
            _ => Error::Query {
                message: format!("{} (code {})", c.message, c.code),
                position: None,
            },
        },
        ErrorKind::Write(WriteFailure::WriteError(w)) => Error::Query {
            message: format!("{} (code {})", w.message, w.code),
            position: None,
        },
        ErrorKind::BulkWrite(_) => Error::Query {
            message: text,
            position: None,
        },
        ErrorKind::Io(_)
        | ErrorKind::ServerSelection { .. }
        | ErrorKind::DnsResolve { .. }
        | ErrorKind::ConnectionPoolCleared { .. } => {
            if text.to_ascii_lowercase().contains("tls") || text.contains("certificate") {
                Error::Tls(text)
            } else {
                Error::Connection(text)
            }
        }
        _ => Error::Other(text),
    }
}

fn bad(msg: impl Into<String>) -> Error {
    Error::Other(msg.into())
}

fn opt<'a>(config: &'a ConnectionConfig, key: &str) -> Option<&'a str> {
    config
        .options
        .get(key)
        .map(String::as_str)
        .filter(|s| !s.is_empty())
}

fn truthy(config: &ConnectionConfig, key: &str) -> bool {
    matches!(opt(config, key), Some("true" | "1" | "yes"))
}

/// Temporary PEM (cert + key) file removed on drop.
pub struct TempPem(pub PathBuf);

impl Drop for TempPem {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Client certificate file for the driver: the single combined PEM, or a temporary 0600 file
/// holding certificate and key when the profile has two files.
fn client_pem(config: &ConnectionConfig) -> Result<(Option<PathBuf>, Option<TempPem>)> {
    let (Some(cert), key) = (&config.tls.client_cert_path, &config.tls.client_key_path) else {
        return Ok((None, None));
    };
    match key {
        None => Ok((Some(PathBuf::from(cert)), None)),
        Some(key) if key == cert => Ok((Some(PathBuf::from(cert)), None)),
        Some(key) => {
            let mut data = std::fs::read(cert)?;
            data.push(b'\n');
            data.extend(std::fs::read(key)?);
            let path = std::env::temp_dir().join(format!("dbstudio-mongo-{}.pem", uuid::Uuid::new_v4()));
            #[cfg(unix)]
            {
                use std::io::Write;
                use std::os::unix::fs::OpenOptionsExt;
                let mut f = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&path)?;
                f.write_all(&data)?;
            }
            #[cfg(not(unix))]
            std::fs::write(&path, &data)?;
            Ok((Some(path.clone()), Some(TempPem(path))))
        }
    }
}

/// How a client should be built for a TLS attempt.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TlsAttempt {
    Plain,
    Tls { insecure: bool },
}

fn tls_attempts(mode: TlsMode) -> Vec<TlsAttempt> {
    match mode {
        TlsMode::Disable => vec![TlsAttempt::Plain],
        TlsMode::Prefer => vec![TlsAttempt::Tls { insecure: true }, TlsAttempt::Plain],
        TlsMode::Require => vec![TlsAttempt::Tls { insecure: true }],
        TlsMode::VerifyCa | TlsMode::VerifyFull => vec![TlsAttempt::Tls { insecure: false }],
    }
}

/// Base `mongodb://` / `mongodb+srv://` URI (no credentials, no options).
fn base_uri(config: &ConnectionConfig) -> String {
    let srv = truthy(config, "srv");
    let host = &config.host;
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.clone()
    };
    if srv {
        format!("mongodb+srv://{host}/")
    } else {
        format!("mongodb://{host}:{}/", config.port)
    }
}

async fn build_options(
    config: &ConnectionConfig,
    secret: Option<&Secret>,
    attempt: TlsAttempt,
    pem: Option<&PathBuf>,
    short: bool,
) -> Result<ClientOptions> {
    let mut o = ClientOptions::parse(base_uri(config))
        .await
        .map_err(map_error)?;
    o.app_name = Some(opt(config, "appName").unwrap_or("DBStudio").to_string());
    let timeout = Duration::from_millis(config.connect_timeout_ms.unwrap_or(DEFAULT_CONNECT_MS));
    let timeout = if short {
        timeout.min(Duration::from_millis(PREFER_TLS_MS))
    } else {
        timeout
    };
    o.connect_timeout = Some(timeout);
    o.server_selection_timeout = Some(timeout);
    if truthy(config, "directConnection") {
        o.direct_connection = Some(true);
    } else if opt(config, "directConnection") == Some("false") {
        o.direct_connection = Some(false);
    }
    if let Some(rs) = opt(config, "replicaSet") {
        o.repl_set_name = Some(rs.to_string());
    }
    o.tls = match attempt {
        TlsAttempt::Plain => None,
        TlsAttempt::Tls { insecure } => {
            let mut t = TlsOptions::default();
            if insecure {
                t.allow_invalid_certificates = Some(true);
            }
            if let Some(ca) = &config.tls.ca_cert_path {
                t.ca_file_path = Some(PathBuf::from(ca));
            }
            t.cert_key_file_path = pem.cloned();
            Some(Tls::Enabled(t))
        }
    };
    let mechanism = match opt(config, "authMechanism") {
        Some(m) => Some(AuthMechanism::from_str(m).map_err(map_error)?),
        None if config.username.is_none() && pem.is_some() => Some(AuthMechanism::MongoDbX509),
        None => None,
    };
    if config.username.is_some() || mechanism.is_some() {
        let source = opt(config, "authSource")
            .map(str::to_string)
            .or_else(|| {
                if mechanism == Some(AuthMechanism::MongoDbX509) {
                    Some("$external".to_string())
                } else {
                    config.database.clone().filter(|d| !d.is_empty())
                }
            })
            .or_else(|| Some("admin".to_string()));
        o.credential = Some(
            Credential::builder()
                .username(config.username.clone())
                .password(secret.map(|s| s.expose().to_string()))
                .source(source)
                .mechanism(mechanism)
                .build(),
        );
    }
    Ok(o)
}

/// An established client plus the facts probed at connect time.
pub struct Conn {
    pub client: Client,
    pub config: ConnectionConfig,
    pub version: String,
    pub major_minor: (u32, u32),
    /// Replica set member or mongos: multi-document transactions are available.
    pub transactions: bool,
    pub set_name: Option<String>,
    /// Keep the temporary client PEM alive while the client exists.
    _pem: Option<TempPem>,
    secret: Option<Secret>,
}

impl std::fmt::Debug for Conn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Conn").field("version", &self.version).finish()
    }
}

impl Conn {
    pub async fn connect(config: &ConnectionConfig, secret: Option<&Secret>) -> Result<Conn> {
        let (pem_path, temp) = client_pem(config)?;
        let attempts = tls_attempts(config.tls.mode);
        let mut last_err = None;
        for (i, attempt) in attempts.iter().enumerate() {
            let short = attempts.len() > 1 && i + 1 < attempts.len();
            let options = build_options(config, secret, *attempt, pem_path.as_ref(), short).await?;
            let client = Client::with_options(options).map_err(map_error)?;
            match probe(&client).await {
                Ok(info) => {
                    return Ok(Conn {
                        client,
                        config: config.clone(),
                        version: info.version,
                        major_minor: info.major_minor,
                        transactions: info.transactions,
                        set_name: info.set_name,
                        _pem: temp,
                        secret: secret.cloned(),
                    });
                }
                Err(e) => {
                    // An authentication failure is final; other failures may mean the server
                    // does not speak TLS (prefer mode).
                    if matches!(e, Error::Authentication(_)) {
                        return Err(e);
                    }
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| bad("no connection attempt was made")))
    }

    /// A second client for a lease: own physical connections, same TLS/auth settings.
    pub async fn dedicated_client(&self) -> Result<Client> {
        let (pem_path, temp) = client_pem(&self.config)?;
        let attempts = tls_attempts(self.config.tls.mode);
        let mut last = None;
        for (i, attempt) in attempts.iter().enumerate() {
            let short = attempts.len() > 1 && i + 1 < attempts.len();
            let mut o = build_options(&self.config, self.secret.as_ref(), *attempt, pem_path.as_ref(), short)
                .await?;
            o.max_pool_size = Some(2);
            let client = Client::with_options(o).map_err(map_error)?;
            match client.database("admin").run_command(doc! {"ping": 1}).await {
                Ok(_) => {
                    drop(temp);
                    return Ok(client);
                }
                Err(e) => last = Some(map_error(e)),
            }
        }
        Err(last.unwrap_or_else(|| bad("no connection attempt was made")))
    }

    /// Default database for commands: the profile database or `admin`.
    pub fn default_db(&self) -> String {
        self.config
            .database
            .clone()
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| "admin".into())
    }

    pub fn read_only(&self) -> bool {
        self.config.read_only
    }

    pub fn show_system(&self) -> bool {
        truthy(&self.config, "showSystem")
    }
}

struct Probe {
    version: String,
    major_minor: (u32, u32),
    transactions: bool,
    set_name: Option<String>,
}

async fn probe(client: &Client) -> Result<Probe> {
    let admin = client.database("admin");
    let hello = admin
        .run_command(doc! {"hello": 1})
        .await
        .or_else(|_| Ok::<_, ()>(Document::new()))
        .unwrap_or_default();
    let hello = if hello.is_empty() {
        admin
            .run_command(doc! {"isMaster": 1})
            .await
            .map_err(map_error)?
    } else {
        hello
    };
    let set_name = hello.get_str("setName").ok().map(str::to_string);
    let mongos = hello.get_str("msg").ok() == Some("isdbgrid");
    let version = match admin.run_command(doc! {"buildInfo": 1}).await {
        Ok(b) => b.get_str("version").unwrap_or("").to_string(),
        Err(_) => String::new(),
    };
    let mut parts = version.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    let mm = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    Ok(Probe {
        version,
        major_minor: mm,
        transactions: (set_name.is_some() && mm.0 >= 4) || (mongos && mm.0 >= 4),
        set_name,
    })
}

/// Runs raw commands on a pooled client or inside a lease session.
pub struct Cmd<'a> {
    pub client: &'a Client,
    pub session: Option<&'a mut ClientSession>,
}

impl<'a> Cmd<'a> {
    pub fn pooled(client: &'a Client) -> Self {
        Cmd {
            client,
            session: None,
        }
    }

    /// Run `command` on database `db`.
    pub async fn run(&mut self, db: &str, command: Document) -> Result<Document> {
        let database = self.client.database(db);
        let reply = match self.session.as_deref_mut() {
            Some(s) => database.run_command(command).session(s).await,
            None => database.run_command(command).await,
        }
        .map_err(map_error)?;
        check_write_errors(&reply)?;
        Ok(reply)
    }
}

/// Turn `writeErrors` / `writeConcernError` of a command reply into an error.
pub fn check_write_errors(reply: &Document) -> Result<()> {
    if let Ok(errs) = reply.get_array("writeErrors") {
        if let Some(bson::Bson::Document(e)) = errs.first() {
            let code = e.get_i32("code").unwrap_or(0);
            let msg = e.get_str("errmsg").unwrap_or("write error");
            return Err(Error::Query {
                message: format!("{msg} (code {code})"),
                position: None,
            });
        }
    }
    if let Ok(e) = reply.get_document("writeConcernError") {
        return Err(Error::Query {
            message: e.get_str("errmsg").unwrap_or("write concern error").to_string(),
            position: None,
        });
    }
    Ok(())
}

/// i64 from any BSON number field.
pub fn num(d: &Document, key: &str) -> i64 {
    match d.get(key) {
        Some(bson::Bson::Int32(i)) => i64::from(*i),
        Some(bson::Bson::Int64(i)) => *i,
        Some(bson::Bson::Double(f)) => *f as i64,
        _ => 0,
    }
}

impl Cmd<'_> {
    /// Like [`Cmd::run`] but leaves `writeErrors` to the caller.
    pub async fn run_unchecked(&mut self, db: &str, command: Document) -> Result<Document> {
        let database = self.client.database(db);
        match self.session.as_deref_mut() {
            Some(s) => database.run_command(command).session(s).await,
            None => database.run_command(command).await,
        }
        .map_err(map_error)
    }
}
