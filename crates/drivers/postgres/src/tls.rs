//! TLS configuration (rustls with the `ring` provider; no OpenSSL).

use std::sync::Arc;

use dbstudio_core::{Error, Result, TlsConfig, TlsMode};
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme,
};
use tokio_postgres::config::SslMode;
use tokio_postgres_rustls::MakeRustlsConnect;

/// Result of translating a [`TlsConfig`]: the libpq-style mode plus the connector (absent for
/// [`TlsMode::Disable`]).
#[derive(Clone)]
pub struct TlsSetup {
    pub ssl_mode: SslMode,
    pub connector: Option<MakeRustlsConnect>,
}

impl std::fmt::Debug for TlsSetup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsSetup")
            .field("ssl_mode", &self.ssl_mode)
            .field("tls", &self.connector.is_some())
            .finish()
    }
}

fn tls_err(context: &str, e: impl std::fmt::Display) -> Error {
    Error::Tls(format!("{context}: {e}"))
}

/// Accepts any server certificate (used for `Prefer` and `Require`, like libpq).
#[derive(Debug)]
struct AcceptAny(WebPkiSupportedAlgorithms);

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

/// Verifies the certificate chain but not that the certificate matches the host name
/// (`verify-ca`).
#[derive(Debug)]
struct ChainOnly(Arc<WebPkiServerVerifier>);

impl ServerCertVerifier for ChainOnly {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        match self
            .0
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
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

fn load_roots(ca_cert_path: Option<&str>) -> Result<RootCertStore> {
    let mut roots = RootCertStore::empty();
    match ca_cert_path {
        Some(path) => {
            let iter = CertificateDer::pem_file_iter(path)
                .map_err(|e| tls_err(&format!("cannot read CA certificate file {path}"), e))?;
            for cert in iter {
                let cert =
                    cert.map_err(|e| tls_err(&format!("invalid CA certificate in {path}"), e))?;
                roots
                    .add(cert)
                    .map_err(|e| tls_err(&format!("unusable CA certificate in {path}"), e))?;
            }
            if roots.is_empty() {
                return Err(Error::Tls(format!("no certificate found in {path}")));
            }
        }
        None => {
            let native = rustls_native_certs::load_native_certs();
            roots.add_parsable_certificates(native.certs);
            if roots.is_empty() {
                return Err(Error::Tls(
                    "no system root certificates available; provide a CA certificate file"
                        .to_string(),
                ));
            }
        }
    }
    Ok(roots)
}

fn load_client_auth(
    cert_path: &str,
    key_path: &str,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    let certs = CertificateDer::pem_file_iter(cert_path)
        .map_err(|e| {
            tls_err(
                &format!("cannot read client certificate file {cert_path}"),
                e,
            )
        })?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| tls_err(&format!("invalid client certificate in {cert_path}"), e))?;
    if certs.is_empty() {
        return Err(Error::Tls(format!("no certificate found in {cert_path}")));
    }
    let key = PrivateKeyDer::from_pem_file(key_path)
        .map_err(|e| tls_err(&format!("cannot read client key file {key_path}"), e))?;
    Ok((certs, key))
}

/// Translate the connection's TLS settings.
pub fn build(cfg: &TlsConfig) -> Result<TlsSetup> {
    if cfg.mode == TlsMode::Disable {
        return Ok(TlsSetup {
            ssl_mode: SslMode::Disable,
            connector: None,
        });
    }
    // Make sure a process-wide provider exists for libraries that rely on it; ignore the error
    // when another driver already installed one.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let provider: Arc<CryptoProvider> = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| tls_err("unsupported TLS protocol versions", e))?;

    let builder = match cfg.mode {
        TlsMode::Prefer | TlsMode::Require => {
            builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(AcceptAny(
                    provider.signature_verification_algorithms,
                )))
        }
        TlsMode::VerifyCa | TlsMode::VerifyFull => {
            let roots = load_roots(cfg.ca_cert_path.as_deref())?;
            let verifier = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider)
                .build()
                .map_err(|e| tls_err("cannot build certificate verifier", e))?;
            if cfg.mode == TlsMode::VerifyFull {
                builder.with_webpki_verifier(verifier)
            } else {
                builder
                    .dangerous()
                    .with_custom_certificate_verifier(Arc::new(ChainOnly(verifier)))
            }
        }
        TlsMode::Disable => unreachable!("handled above"),
    };

    let config = match (&cfg.client_cert_path, &cfg.client_key_path) {
        (Some(cert), Some(key)) => {
            let (certs, key) = load_client_auth(cert, key)?;
            builder
                .with_client_auth_cert(certs, key)
                .map_err(|e| tls_err("invalid client certificate or key", e))?
        }
        (None, None) => builder.with_no_client_auth(),
        _ => {
            return Err(Error::Tls(
                "client certificate and client key must be provided together".to_string(),
            ));
        }
    };

    Ok(TlsSetup {
        ssl_mode: if cfg.mode == TlsMode::Prefer {
            SslMode::Prefer
        } else {
            SslMode::Require
        },
        connector: Some(MakeRustlsConnect::new(config)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disable_has_no_connector() {
        let s = build(&TlsConfig {
            mode: TlsMode::Disable,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(s.ssl_mode, SslMode::Disable);
        assert!(s.connector.is_none());
    }

    #[test]
    fn modes_map_to_ssl_modes() {
        let mk = |mode| {
            build(&TlsConfig {
                mode,
                ca_cert_path: Some(format!(
                    "{}/../../../docker/certs/ca.crt",
                    env!("CARGO_MANIFEST_DIR")
                )),
                ..Default::default()
            })
        };
        if !std::path::Path::new(&format!(
            "{}/../../../docker/certs/ca.crt",
            env!("CARGO_MANIFEST_DIR")
        ))
        .exists()
        {
            return;
        }
        assert_eq!(mk(TlsMode::Prefer).unwrap().ssl_mode, SslMode::Prefer);
        assert_eq!(mk(TlsMode::Require).unwrap().ssl_mode, SslMode::Require);
        assert_eq!(mk(TlsMode::VerifyCa).unwrap().ssl_mode, SslMode::Require);
        assert_eq!(mk(TlsMode::VerifyFull).unwrap().ssl_mode, SslMode::Require);
    }

    #[test]
    fn bad_files_are_tls_errors() {
        let err = build(&TlsConfig {
            mode: TlsMode::VerifyFull,
            ca_cert_path: Some("/nonexistent/ca.pem".into()),
            ..Default::default()
        })
        .unwrap_err();
        assert!(matches!(err, Error::Tls(_)), "{err:?}");
        let err = build(&TlsConfig {
            mode: TlsMode::Require,
            client_cert_path: Some("/nonexistent/c.pem".into()),
            ..Default::default()
        })
        .unwrap_err();
        assert!(matches!(err, Error::Tls(_)), "{err:?}");
    }
}
