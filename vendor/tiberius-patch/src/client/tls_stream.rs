use crate::Config;
use futures_util::io::{AsyncRead, AsyncWrite};

/// ALPN protocol name advertised for TDS 8.0 ("strict") encryption.
#[cfg(any(
    feature = "rustls",
    feature = "native-tls",
    feature = "vendored-openssl"
))]
// Used by the native-tls and rustls backends to advertise TDS 8.0 strict; the
// opentls (vendored-openssl) backend cannot set ALPN, so this is unused there.
#[allow(dead_code)]
pub(crate) const TDS_ALPN_PROTOCOL_NAME: &str = "tds/8.0";

// Backend-agnostic CA loader shared by all three TLS backends.
#[cfg(any(
    feature = "rustls",
    feature = "native-tls",
    feature = "vendored-openssl"
))]
mod certs;

#[cfg(feature = "native-tls")]
mod native_tls_stream;

#[cfg(feature = "rustls")]
mod rustls_tls_stream;

#[cfg(feature = "vendored-openssl")]
mod opentls_tls_stream;

// DBStudio patch: with both `rustls` and `native-tls` the `TlsStream` is an enum (below) that
// dispatches to the backend chosen by `Config::tls_backend`.
#[cfg(all(feature = "native-tls", not(feature = "rustls")))]
pub(crate) use native_tls_stream::TlsStream;

#[cfg(all(feature = "rustls", not(feature = "native-tls")))]
pub(crate) use rustls_tls_stream::TlsStream;

#[cfg(feature = "vendored-openssl")]
pub(crate) use opentls_tls_stream::TlsStream;

#[cfg(all(feature = "rustls", not(feature = "native-tls")))]
pub(crate) async fn create_tls_stream<S: AsyncRead + AsyncWrite + Unpin + Send>(
    config: &Config,
    stream: S,
) -> crate::Result<TlsStream<S>> {
    TlsStream::new(config, stream).await
}

#[cfg(all(feature = "native-tls", not(feature = "rustls")))]
pub(crate) async fn create_tls_stream<S: AsyncRead + AsyncWrite + Unpin + Send>(
    config: &Config,
    stream: S,
) -> crate::Result<TlsStream<S>> {
    native_tls_stream::create_tls_stream(config, stream).await
}

#[cfg(feature = "vendored-openssl")]
pub(crate) async fn create_tls_stream<S: AsyncRead + AsyncWrite + Unpin + Send>(
    config: &Config,
    stream: S,
) -> crate::Result<TlsStream<S>> {
    opentls_tls_stream::create_tls_stream(config, stream).await
}

/// DBStudio patch: TLS stream of the backend selected at runtime.
#[cfg(all(feature = "rustls", feature = "native-tls"))]
pub(crate) enum TlsStream<S: AsyncRead + AsyncWrite + Unpin + Send> {
    Rustls(rustls_tls_stream::TlsStream<S>),
    Native(native_tls_stream::TlsStream<S>),
}

#[cfg(all(feature = "rustls", feature = "native-tls"))]
impl<S: AsyncRead + AsyncWrite + Unpin + Send> TlsStream<S> {
    pub(crate) fn get_mut(&mut self) -> &mut S {
        match self {
            Self::Rustls(s) => s.get_mut(),
            Self::Native(s) => s.get_mut(),
        }
    }
}

#[cfg(all(feature = "rustls", feature = "native-tls"))]
impl<S: AsyncRead + AsyncWrite + Unpin + Send> AsyncRead for TlsStream<S> {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Self::Rustls(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            Self::Native(s) => std::pin::Pin::new(s).poll_read(cx, buf),
        }
    }
}

#[cfg(all(feature = "rustls", feature = "native-tls"))]
impl<S: AsyncRead + AsyncWrite + Unpin + Send> AsyncWrite for TlsStream<S> {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Self::Rustls(s) => std::pin::Pin::new(s).poll_write(cx, buf),
            Self::Native(s) => std::pin::Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Rustls(s) => std::pin::Pin::new(s).poll_flush(cx),
            Self::Native(s) => std::pin::Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_close(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Rustls(s) => std::pin::Pin::new(s).poll_close(cx),
            Self::Native(s) => std::pin::Pin::new(s).poll_close(cx),
        }
    }
}

#[cfg(all(feature = "rustls", feature = "native-tls"))]
pub(crate) async fn create_tls_stream<S: AsyncRead + AsyncWrite + Unpin + Send>(
    config: &Config,
    stream: S,
) -> crate::Result<TlsStream<S>> {
    match config.tls_backend {
        crate::client::TlsBackend::Rustls => Ok(TlsStream::Rustls(
            rustls_tls_stream::TlsStream::new(config, stream).await?,
        )),
        crate::client::TlsBackend::Native => Ok(TlsStream::Native(
            native_tls_stream::create_tls_stream(config, stream).await?,
        )),
    }
}
