//! HTTP(S) client construction: TLS modes, client certificates and the host-name override that
//! makes `verifyFull` work behind an SSH tunnel (design R6).
//!
//! Behind a tunnel the application connects to `127.0.0.1:<local port>` and puts the original
//! host in `tls.server_name`. The URL then uses the original host (so SNI and certificate
//! verification see it) while [`MapResolver`] maps that name to the real connect address.

use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use clickhouse::{Client, Compression};
use dbstudio_core::{ConnectionConfig, Error, Result, Secret, TlsConfig, TlsMode};
use hyper_util::client::legacy::Client as HyperClient;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::connect::dns::Name;
use hyper_util::rt::TokioExecutor;
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{CertificateError, DigitallySignedStruct, RootCertStore, SignatureScheme};
use tower_service::Service;

/// Default connect timeout when the configuration sets none.
pub(crate) const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Install the process-wide rustls crypto provider (`ring`) if none is installed yet.
pub(crate) fn ensure_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Resolver that maps one host name to a fixed address and resolves everything else normally.
#[derive(Clone, Debug)]
pub(crate) struct MapResolver {
    from: Option<String>,
    to: String,
}

impl Service<Name> for MapResolver {
    type Response = std::vec::IntoIter<SocketAddr>;
    type Error = std::io::Error;
    type Future = Pin<Box<dyn Future<Output = std::io::Result<Self::Response>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, name: Name) -> Self::Future {
        let target = match &self.from {
            Some(from) if from.eq_ignore_ascii_case(name.as_str()) => self.to.clone(),
            _ => name.as_str().to_string(),
        };
        Box::pin(async move {
            if let Ok(ip) = target.parse::<IpAddr>() {
                return Ok(vec![SocketAddr::new(ip, 0)].into_iter());
            }
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((target.as_str(), 0))
                .await?
                .collect();
            Ok(addrs.into_iter())
        })
    }
}

/// Certificate verifier that accepts any certificate (`Prefer` / `Require`) but still checks
/// handshake signatures with the provider's algorithms.
#[derive(Debug)]
struct AcceptAny(WebPkiSupportedAlgorithms);

impl ServerCertVerifier for AcceptAny {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
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
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_schemes()
    }
}

/// `VerifyCa`: the chain must be valid but the host name is not checked.
#[derive(Debug)]
struct ChainOnly(Arc<WebPkiServerVerifier>);

impl ServerCertVerifier for ChainOnly {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        match self
            .0
            .verify_server_cert(end_entity, intermediates, server_name, ocsp, now)
        {
            Err(rustls::Error::InvalidCertificate(
                CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
            )) => Ok(ServerCertVerified::assertion()),
            other => other,
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_verify_schemes()
    }
}

fn tls_err(what: &str, path: &str, e: impl std::fmt::Display) -> Error {
    Error::Tls(format!("cannot read {what} `{path}`: {e}"))
}

fn root_store(tls: &TlsConfig) -> Result<RootCertStore> {
    let mut roots = RootCertStore::empty();
    match &tls.ca_cert_path {
        Some(path) => {
            let mut found = 0;
            for cert in CertificateDer::pem_file_iter(path)
                .map_err(|e| tls_err("CA certificate", path, e))?
            {
                let cert = cert.map_err(|e| tls_err("CA certificate", path, e))?;
                roots
                    .add(cert)
                    .map_err(|e| Error::Tls(format!("bad CA certificate in `{path}`: {e}")))?;
                found += 1;
            }
            if found == 0 {
                return Err(Error::Tls(format!("no certificate found in `{path}`")));
            }
        }
        None => {
            // The operating system's certificate store.
            let loaded = rustls_native_certs::load_native_certs();
            roots.add_parsable_certificates(loaded.certs);
        }
    }
    Ok(roots)
}

/// rustls configuration of `tls` (modes: see the module docs of the crate).
pub(crate) fn tls_config(tls: &TlsConfig) -> Result<rustls::ClientConfig> {
    let provider: Arc<CryptoProvider> = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::Tls(e.to_string()))?;
    let builder = match tls.mode {
        TlsMode::Disable | TlsMode::Prefer | TlsMode::Require => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAny(
                provider.signature_verification_algorithms,
            ))),
        TlsMode::VerifyCa | TlsMode::VerifyFull => {
            let roots = Arc::new(root_store(tls)?);
            let inner = WebPkiServerVerifier::builder_with_provider(roots.clone(), provider)
                .build()
                .map_err(|e| Error::Tls(e.to_string()))?;
            if tls.mode == TlsMode::VerifyCa {
                builder
                    .dangerous()
                    .with_custom_certificate_verifier(Arc::new(ChainOnly(inner)))
            } else {
                builder.dangerous().with_custom_certificate_verifier(inner)
            }
        }
    };
    match (&tls.client_cert_path, &tls.client_key_path) {
        (Some(cert), Some(key)) => {
            let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert)
                .map_err(|e| tls_err("client certificate", cert, e))?
                .collect::<std::result::Result<_, _>>()
                .map_err(|e| tls_err("client certificate", cert, e))?;
            let key =
                PrivateKeyDer::from_pem_file(key).map_err(|e| tls_err("client key", key, e))?;
            builder
                .with_client_auth_cert(certs, key)
                .map_err(|e| Error::Tls(format!("bad client certificate: {e}")))
        }
        (None, None) => Ok(builder.with_no_client_auth()),
        _ => Err(Error::Tls(
            "client certificate and client key must be configured together".into(),
        )),
    }
}

/// How a configuration reaches the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    pub url: String,
    pub secure: bool,
    /// Host the URL uses when it differs from `config.host` (SSH + `server_name`).
    pub resolve_from: Option<String>,
}

fn bracket(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

/// URL and TLS decision. `HTTPS` is used for `Require`/`VerifyCa`/`VerifyFull`, for `Prefer` on
/// the conventional TLS ports, or when the option `secure` is `true` (`false` forces HTTP).
pub(crate) fn target(cfg: &ConnectionConfig) -> Result<Target> {
    let secure = match crate::conn::option(cfg, "secure").map(|s| s.trim().to_ascii_lowercase()) {
        Some(v) if v == "true" || v == "1" => true,
        Some(v) if v == "false" || v == "0" => false,
        Some(v) => return Err(Error::Invalid(format!("invalid option secure `{v}`"))),
        None => match cfg.tls.mode {
            TlsMode::Disable => false,
            TlsMode::Prefer => matches!(cfg.port, 8443 | 443),
            _ => true,
        },
    };
    let (host, resolve_from) = match (&cfg.tls.server_name, secure) {
        (Some(sn), true) if !sn.is_empty() && !sn.eq_ignore_ascii_case(&cfg.host) => {
            (sn.clone(), Some(sn.clone()))
        }
        _ => (cfg.host.clone(), None),
    };
    Ok(Target {
        url: format!(
            "{}://{}:{}",
            if secure { "https" } else { "http" },
            bracket(&host),
            cfg.port
        ),
        secure,
        resolve_from,
    })
}

/// Build the base [`Client`] (URL, credentials, transport, compression).
pub(crate) fn build_client(
    cfg: &ConnectionConfig,
    secret: Option<&Secret>,
    target: &Target,
) -> Result<Client> {
    ensure_crypto_provider();
    let mut http = HttpConnector::new_with_resolver(MapResolver {
        from: target.resolve_from.clone(),
        to: cfg.host.clone(),
    });
    http.enforce_http(false);
    http.set_nodelay(true);
    http.set_keepalive(Some(Duration::from_secs(60)));
    http.set_connect_timeout(Some(
        cfg.connect_timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_CONNECT_TIMEOUT),
    ));
    let builder = if target.secure {
        hyper_rustls::HttpsConnectorBuilder::new().with_tls_config(tls_config(&cfg.tls)?)
    } else {
        // Never reached for plain HTTP below, but keeps one connector type.
        hyper_rustls::HttpsConnectorBuilder::new().with_tls_config(tls_config(&TlsConfig {
            mode: TlsMode::Require,
            ..TlsConfig::default()
        })?)
    };
    let connector = builder.https_or_http().enable_http1().wrap_connector(http);
    // ClickHouse closes idle keep-alive connections after 3 s (10 s on recent versions).
    let hyper = HyperClient::builder(TokioExecutor::new())
        .pool_idle_timeout(Duration::from_secs(2))
        .build(connector);
    let compression = match crate::conn::option(cfg, "compression")
        .map(|s| s.trim().to_ascii_lowercase())
        .as_deref()
    {
        None | Some("lz4") => Compression::Lz4,
        Some("none") | Some("off") | Some("false") => Compression::None,
        Some(other) => {
            return Err(Error::Invalid(format!(
                "invalid option compression `{other}` (expected lz4 or none)"
            )));
        }
    };
    let mut client = Client::with_http_client(hyper)
        .with_url(&target.url)
        .with_user(
            cfg.username
                .clone()
                .filter(|u| !u.is_empty())
                .unwrap_or_else(|| "default".to_string()),
        )
        .with_compression(compression);
    if let Some(secret) = secret {
        client = client.with_password(secret.expose());
    }
    Ok(client)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_core::DbKind;

    fn cfg() -> ConnectionConfig {
        ConnectionConfig::new(DbKind::ClickHouse, "t", "db.example.com")
    }

    #[test]
    fn url_follows_tls_mode() {
        let mut c = cfg();
        c.tls.mode = TlsMode::Disable;
        assert_eq!(target(&c).unwrap().url, "http://db.example.com:8123");
        c.tls.mode = TlsMode::Prefer;
        assert!(!target(&c).unwrap().secure);
        c.port = 8443;
        assert_eq!(target(&c).unwrap().url, "https://db.example.com:8443");
        c.tls.mode = TlsMode::VerifyFull;
        c.port = 18443;
        assert!(target(&c).unwrap().secure);
        c.options.insert("secure".into(), "false".into());
        assert!(!target(&c).unwrap().secure);
        c.options.insert("secure".into(), "maybe".into());
        assert!(matches!(target(&c), Err(Error::Invalid(_))));
    }

    #[test]
    fn tunnel_keeps_original_host_in_url() {
        let mut c = cfg();
        c.host = "127.0.0.1".into();
        c.port = 40001;
        c.tls.mode = TlsMode::VerifyFull;
        c.tls.server_name = Some("clickhouse26".into());
        let t = target(&c).unwrap();
        assert_eq!(t.url, "https://clickhouse26:40001");
        assert_eq!(t.resolve_from.as_deref(), Some("clickhouse26"));
        c.tls.mode = TlsMode::Disable;
        assert_eq!(target(&c).unwrap().url, "http://127.0.0.1:40001");
        c.host = "::1".into();
        c.tls.server_name = None;
        assert_eq!(target(&c).unwrap().url, "http://[::1]:40001");
    }

    #[test]
    fn tls_config_validates_files() {
        ensure_crypto_provider();
        let tls = TlsConfig {
            mode: TlsMode::VerifyFull,
            ca_cert_path: Some("/definitely/not/here.pem".into()),
            ..Default::default()
        };
        assert!(matches!(tls_config(&tls), Err(Error::Tls(_))));
        let tls = TlsConfig {
            mode: TlsMode::Require,
            client_cert_path: Some("/x".into()),
            ..Default::default()
        };
        assert!(matches!(tls_config(&tls), Err(Error::Tls(_))));
        assert!(tls_config(&TlsConfig::default()).is_ok());
    }

    #[test]
    fn builds_a_client() {
        let c = cfg();
        let t = target(&c).unwrap();
        assert!(build_client(&c, Some(&Secret::new("pw")), &t).is_ok());
    }
}
