//! Connection settings, TLS (custom certificate verifier), session builder and the pool of
//! sessions per keyspace.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use dbstudio_core::{ConnectionConfig, Error, Result, Secret, TlsMode};
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, RootCertStore, SignatureScheme};
use scylla::client::session::{Session, TlsContext};
use scylla::client::session_builder::SessionBuilder;
use scylla::cluster::metadata::Peer;
use scylla::errors::TranslationError;
use scylla::policies::address_translator::{AddressTranslator, UntranslatedPeer};
use scylla::policies::host_filter::HostFilter;
use scylla::statement::Consistency;

use crate::error;

/// Maximum keyspace sessions kept besides the base one.
const MAX_KEYSPACE_SESSIONS: usize = 8;
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Settings parsed once from the connection profile.
#[derive(Debug, Clone)]
pub(crate) struct Settings {
    pub host: String,
    pub port: u16,
    /// Keyspace of the profile (default context of the console).
    pub default_keyspace: Option<String>,
    pub consistency: Consistency,
    pub local_dc: Option<String>,
    pub show_system: bool,
    pub lwt_edits: bool,
    pub read_only: bool,
    /// Only one node is reachable (SSH tunnel): never talk to the others.
    pub single_node: bool,
}

/// Parse a consistency level name (`LOCAL_QUORUM`, `local_one`...).
pub(crate) fn parse_consistency(s: &str) -> Option<Consistency> {
    Some(
        match s.trim().to_ascii_uppercase().replace(' ', "_").as_str() {
            "ANY" => Consistency::Any,
            "ONE" => Consistency::One,
            "TWO" => Consistency::Two,
            "THREE" => Consistency::Three,
            "QUORUM" => Consistency::Quorum,
            "ALL" => Consistency::All,
            "LOCAL_QUORUM" => Consistency::LocalQuorum,
            "EACH_QUORUM" => Consistency::EachQuorum,
            "LOCAL_ONE" => Consistency::LocalOne,
            "SERIAL" => Consistency::Serial,
            "LOCAL_SERIAL" => Consistency::LocalSerial,
            _ => return None,
        },
    )
}

pub(crate) fn consistency_name(c: Consistency) -> &'static str {
    match c {
        Consistency::Any => "ANY",
        Consistency::One => "ONE",
        Consistency::Two => "TWO",
        Consistency::Three => "THREE",
        Consistency::Quorum => "QUORUM",
        Consistency::All => "ALL",
        Consistency::LocalQuorum => "LOCAL_QUORUM",
        Consistency::EachQuorum => "EACH_QUORUM",
        Consistency::LocalOne => "LOCAL_ONE",
        Consistency::Serial => "SERIAL",
        Consistency::LocalSerial => "LOCAL_SERIAL",
    }
}

fn flag(config: &ConnectionConfig, key: &str, default: bool) -> bool {
    match config
        .options
        .get(key)
        .map(|v| v.trim().to_ascii_lowercase())
    {
        Some(v) if matches!(v.as_str(), "true" | "1" | "yes" | "on") => true,
        Some(v) if matches!(v.as_str(), "false" | "0" | "no" | "off") => false,
        _ => default,
    }
}

impl Settings {
    pub(crate) fn from_config(config: &ConnectionConfig) -> Result<Settings> {
        let consistency = match config.options.get("cql.consistency") {
            Some(v) if !v.trim().is_empty() => parse_consistency(v).ok_or_else(|| {
                Error::Invalid(format!(
                    "unknown consistency level `{v}` (option cql.consistency)"
                ))
            })?,
            _ => Consistency::LocalOne,
        };
        Ok(Settings {
            host: config.host.clone(),
            port: config.port,
            default_keyspace: config.database.clone().filter(|d| !d.trim().is_empty()),
            consistency,
            local_dc: config
                .options
                .get("cql.localDc")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            show_system: flag(config, "cql.showSystem", false),
            lwt_edits: flag(config, "cql.lwtEdits", true),
            read_only: config.read_only,
            single_node: config.ssh.is_some() || flag(config, "cql.singleNode", false),
        })
    }
}

// ---------------------------------------------------------------------------------------------
// TLS
// ---------------------------------------------------------------------------------------------

/// How the server certificate is checked.
#[derive(Debug)]
enum Verification {
    /// Accept any certificate (`Require`).
    None,
    /// Check the chain; the name only when `check_name`.
    Chain { check_name: bool },
}

/// Verifier that checks the certificate against a configured name instead of the one the driver
/// passes (the driver always passes the node's IP address).
#[derive(Debug)]
struct NameVerifier {
    inner: Arc<WebPkiServerVerifier>,
    name: ServerName<'static>,
    check_name: bool,
}

impl ServerCertVerifier for NameVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        match self.inner.verify_server_cert(
            end_entity,
            intermediates,
            &self.name,
            ocsp_response,
            now,
        ) {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::NotValidForName
                | rustls::CertificateError::NotValidForNameContext { .. },
            )) if !self.check_name => Ok(ServerCertVerified::assertion()),
            other => other,
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// Accepts every certificate (TLS mode `Require`: encrypted, not authenticated).
#[derive(Debug)]
struct AcceptAny {
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for AcceptAny {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn tls_err(msg: impl Into<String>) -> Error {
    Error::Tls(msg.into())
}

fn load_certs(path: &str) -> Result<Vec<CertificateDer<'static>>> {
    CertificateDer::pem_file_iter(path)
        .map_err(|e| tls_err(format!("cannot read certificate file {path}: {e}")))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| tls_err(format!("invalid certificate file {path}: {e}")))
}

fn roots(config: &ConnectionConfig) -> Result<RootCertStore> {
    let mut store = RootCertStore::empty();
    if let Some(path) = config.tls.ca_cert_path.as_deref().filter(|p| !p.is_empty()) {
        for cert in load_certs(path)? {
            store
                .add(cert)
                .map_err(|e| tls_err(format!("invalid CA certificate in {path}: {e}")))?;
        }
        if store.is_empty() {
            return Err(tls_err(format!("no certificate found in {path}")));
        }
    } else {
        let native = rustls_native_certs::load_native_certs();
        for cert in native.certs {
            let _ = store.add(cert);
        }
    }
    Ok(store)
}

/// The rustls client configuration of a profile (`None` = plain text).
pub(crate) fn tls_config(config: &ConnectionConfig) -> Result<Option<Arc<rustls::ClientConfig>>> {
    let verification = match config.tls.mode {
        TlsMode::Disable | TlsMode::Prefer => return Ok(None),
        TlsMode::Require => Verification::None,
        TlsMode::VerifyCa => Verification::Chain { check_name: false },
        TlsMode::VerifyFull => Verification::Chain { check_name: true },
    };
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| tls_err(e.to_string()))?;
    let verifier: Arc<dyn ServerCertVerifier> = match verification {
        Verification::None => Arc::new(AcceptAny {
            provider: provider.clone(),
        }),
        Verification::Chain { check_name } => {
            let inner = WebPkiServerVerifier::builder_with_provider(
                Arc::new(roots(config)?),
                provider.clone(),
            )
            .build()
            .map_err(|e| tls_err(e.to_string()))?;
            let host = config
                .tls
                .server_name
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| config.host.clone());
            let name = ServerName::try_from(host.clone())
                .map_err(|e| tls_err(format!("invalid server name `{host}`: {e}")))?
                .to_owned();
            Arc::new(NameVerifier {
                inner,
                name,
                check_name,
            })
        }
    };
    let builder = builder
        .dangerous()
        .with_custom_certificate_verifier(verifier);
    let cert_path = config
        .tls
        .client_cert_path
        .as_deref()
        .filter(|p| !p.is_empty());
    let key_path = config
        .tls
        .client_key_path
        .as_deref()
        .filter(|p| !p.is_empty());
    let cfg = match (cert_path, key_path) {
        (Some(c), Some(k)) => {
            let certs = load_certs(c)?;
            let key = PrivateKeyDer::from_pem_file(k)
                .map_err(|e| tls_err(format!("cannot read private key {k}: {e}")))?;
            builder
                .with_client_auth_cert(certs, key)
                .map_err(|e| tls_err(format!("client certificate rejected: {e}")))?
        }
        (None, None) => builder.with_no_client_auth(),
        _ => {
            return Err(Error::Invalid(
                "mutual TLS needs both a client certificate and a client key".into(),
            ));
        }
    };
    Ok(Some(Arc::new(cfg)))
}

// ---------------------------------------------------------------------------------------------
// Single node mode (SSH tunnel)
// ---------------------------------------------------------------------------------------------

/// Sends every node to the tunnel endpoint.
struct TunnelTranslator(SocketAddr);

#[async_trait]
impl AddressTranslator for TunnelTranslator {
    async fn translate_address(
        &self,
        _peer: &UntranslatedPeer,
    ) -> std::result::Result<SocketAddr, TranslationError> {
        Ok(self.0)
    }
}

/// Keeps only the first node seen.
#[derive(Default)]
struct FirstNodeOnly(Mutex<Option<uuid::Uuid>>);

impl HostFilter for FirstNodeOnly {
    fn accept(&self, peer: &Peer) -> bool {
        let mut first = self.0.lock().unwrap_or_else(|e| e.into_inner());
        match *first {
            None => {
                *first = Some(peer.host_id);
                true
            }
            Some(id) => id == peer.host_id,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------------------------

/// Resolve `host:port` to the first socket address.
async fn resolve(host: &str, port: u16) -> Result<SocketAddr> {
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| Error::Connection(format!("cannot resolve {host}: {e}")))?
        .collect();
    // Prefer IPv4: tunnels and published container ports usually listen there only.
    addrs
        .iter()
        .find(|a| a.is_ipv4())
        .or(addrs.first())
        .copied()
        .ok_or_else(|| Error::Connection(format!("cannot resolve {host}")))
}

/// Builds sessions and keeps one per keyspace.
pub(crate) struct Pool {
    pub settings: Settings,
    builder: SessionBuilder,
    base: Arc<Session>,
    by_keyspace: Mutex<Vec<(String, Arc<Session>)>>,
    create: tokio::sync::Mutex<()>,
    /// Message to show when the connection was opened in a restricted mode.
    pub notice: Option<String>,
}

impl Pool {
    pub(crate) async fn connect(
        config: &ConnectionConfig,
        secret: Option<&Secret>,
    ) -> Result<Pool> {
        let settings = Settings::from_config(config)?;
        let timeout = config
            .connect_timeout_ms
            .filter(|ms| *ms > 0)
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_CONNECT_TIMEOUT);
        let mut builder = SessionBuilder::new()
            .connection_timeout(timeout)
            // Schema comes from our own `system_schema` queries.
            .fetch_schema_metadata(false)
            .keepalive_interval(Duration::from_secs(30))
            .keepalive_timeout(Duration::from_secs(60))
            // Traces are written asynchronously by the server.
            .tracing_info_fetch_attempts(
                std::num::NonZeroU32::new(20).unwrap_or(std::num::NonZeroU32::MIN),
            )
            .tracing_info_fetch_interval(Duration::from_millis(100))
            .tracing_info_fetch_consistency(Consistency::One);
        let mut notice = None;
        if settings.single_node {
            let addr = resolve(&settings.host, settings.port).await?;
            builder = builder
                .known_node_addr(addr)
                .address_translator(Arc::new(TunnelTranslator(addr)))
                .host_filter(Arc::new(FirstNodeOnly::default()))
                .disallow_shard_aware_port(true);
            notice = Some(
                "Connected through a single node (SSH tunnel): only one node is used, so \
                 requests are not token-aware."
                    .to_string(),
            );
        } else {
            let host = if settings.host.contains(':') && !settings.host.starts_with('[') {
                format!("[{}]", settings.host)
            } else {
                settings.host.clone()
            };
            builder = builder.known_node(format!("{host}:{}", settings.port));
        }
        if let Some(user) = config.username.as_deref().filter(|u| !u.is_empty()) {
            builder = builder.user(user, secret.map(|s| s.expose()).unwrap_or(""));
        }
        if let Some(tls) = tls_config(config)? {
            builder = builder.tls_context(Some(TlsContext::Rustls023(tls)));
        }
        if let Some(dc) = &settings.local_dc {
            builder = builder.prefer_datacenter(dc.clone());
        }
        let base = tokio::time::timeout(timeout + Duration::from_secs(5), builder.build())
            .await
            .map_err(|_| Error::Timeout)?
            .map_err(|e| error::from_new_session(&e))?;
        Ok(Pool {
            settings,
            builder,
            base: Arc::new(base),
            by_keyspace: Mutex::new(Vec::new()),
            create: tokio::sync::Mutex::new(()),
            notice,
        })
    }

    /// The session without keyspace.
    pub(crate) fn base(&self) -> Arc<Session> {
        self.base.clone()
    }

    /// The session whose keyspace is `keyspace` (created on first use, at most
    /// [`MAX_KEYSPACE_SESSIONS`] are kept). `None` gives the base session.
    pub(crate) async fn session(&self, keyspace: Option<&str>) -> Result<Arc<Session>> {
        let Some(ks) = keyspace.filter(|k| !k.is_empty()) else {
            return Ok(self.base());
        };
        if let Some(s) = self.lookup(ks) {
            return Ok(s);
        }
        let _guard = self.create.lock().await;
        if let Some(s) = self.lookup(ks) {
            return Ok(s);
        }
        let session = self
            .builder
            .clone()
            .use_keyspace(ks, true)
            .build()
            .await
            .map_err(|e| match e {
                scylla::errors::NewSessionError::UseKeyspaceError(_) => {
                    Error::NotFound(format!("keyspace {ks}"))
                }
                other => error::from_new_session(&other),
            })?;
        let session = Arc::new(session);
        let mut list = self.by_keyspace.lock().unwrap_or_else(|e| e.into_inner());
        list.insert(0, (ks.to_string(), session.clone()));
        list.truncate(MAX_KEYSPACE_SESSIONS);
        Ok(session)
    }

    fn lookup(&self, ks: &str) -> Option<Arc<Session>> {
        let mut list = self.by_keyspace.lock().unwrap_or_else(|e| e.into_inner());
        let pos = list.iter().position(|(k, _)| k == ks)?;
        let entry = list.remove(pos);
        let s = entry.1.clone();
        list.insert(0, entry);
        Some(s)
    }

    /// Drop every keyspace session.
    pub(crate) fn clear(&self) {
        self.by_keyspace
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_core::DbKind;

    #[test]
    fn consistency_names() {
        assert_eq!(
            parse_consistency("local_quorum"),
            Some(Consistency::LocalQuorum)
        );
        assert_eq!(parse_consistency("LOCAL ONE"), Some(Consistency::LocalOne));
        assert_eq!(parse_consistency("nope"), None);
        assert_eq!(consistency_name(Consistency::EachQuorum), "EACH_QUORUM");
    }

    #[test]
    fn settings_from_options() {
        let mut cfg = ConnectionConfig::new(DbKind::Cassandra, "t", "localhost");
        cfg.options.insert("cql.localDc".into(), "dc1".into());
        cfg.options
            .insert("cql.consistency".into(), "QUORUM".into());
        cfg.options.insert("cql.showSystem".into(), "true".into());
        cfg.options.insert("cql.lwtEdits".into(), "false".into());
        let s = Settings::from_config(&cfg).unwrap();
        assert_eq!(s.local_dc.as_deref(), Some("dc1"));
        assert_eq!(s.consistency, Consistency::Quorum);
        assert!(s.show_system && !s.lwt_edits && !s.single_node);
        cfg.options.insert("cql.consistency".into(), "bogus".into());
        assert_eq!(Settings::from_config(&cfg).unwrap_err().kind(), "invalid");
        let d = Settings::from_config(&ConnectionConfig::new(DbKind::ScyllaDb, "t", "h")).unwrap();
        assert_eq!(d.consistency, Consistency::LocalOne);
        assert!(!d.show_system && d.lwt_edits);
    }

    #[test]
    fn tls_modes() {
        let mut cfg = ConnectionConfig::new(DbKind::Cassandra, "t", "localhost");
        cfg.tls.mode = TlsMode::Disable;
        assert!(tls_config(&cfg).unwrap().is_none());
        cfg.tls.mode = TlsMode::Require;
        assert!(tls_config(&cfg).unwrap().is_some());
        cfg.tls.mode = TlsMode::VerifyFull;
        cfg.tls.ca_cert_path = Some("/nonexistent/ca.pem".into());
        assert_eq!(tls_config(&cfg).unwrap_err().kind(), "tls");
        cfg.tls.ca_cert_path = None;
        cfg.tls.client_cert_path = Some("/x.pem".into());
        assert_eq!(tls_config(&cfg).unwrap_err().kind(), "invalid");
    }
}
