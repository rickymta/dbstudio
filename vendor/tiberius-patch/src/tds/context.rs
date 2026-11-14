use super::codec::*;
use futures_util::task::AtomicWaker;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

// DBStudio patch: cooperative TDS attention (query cancel) shared between a running token
// stream and any number of `CancelHandle`s.
#[derive(Debug, Default)]
pub(crate) struct AttentionShared {
    requested: AtomicBool,
    waker: AtomicWaker,
}

impl AttentionShared {
    pub(crate) fn request(&self) {
        self.requested.store(true, Ordering::Release);
        self.waker.wake();
    }

    /// Consume a pending request.
    pub(crate) fn take(&self) -> bool {
        self.requested.swap(false, Ordering::AcqRel)
    }

    pub(crate) fn reset(&self) {
        self.requested.store(false, Ordering::Release);
    }

    pub(crate) fn wait(&self) -> WaitAttention<'_> {
        WaitAttention(self)
    }
}

/// Resolves once a cancel was requested.
pub(crate) struct WaitAttention<'a>(&'a AttentionShared);

impl Future for WaitAttention<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<()> {
        if self.0.requested.load(Ordering::Acquire) {
            return Poll::Ready(());
        }
        self.0.waker.register(cx.waker());
        if self.0.requested.load(Ordering::Acquire) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

/// Handle that cancels the request running on the connection of the [`Client`](crate::Client)
/// it came from, from any task. The running result stream sends a TDS attention packet and
/// drains the server's acknowledgement; the connection stays usable. DBStudio patch.
#[derive(Debug, Clone)]
pub struct CancelHandle(pub(crate) Arc<AttentionShared>);

impl CancelHandle {
    /// Ask the running request to stop. Has no effect when nothing is running.
    pub fn cancel(&self) {
        self.0.request();
    }
}

/// Context, that might be required to make sure we understand and are understood by the server
#[derive(Debug)]
pub(crate) struct Context {
    version: FeatureLevel,
    packet_size: u32,
    packet_id: u8,
    transaction_desc: [u8; 8],
    last_meta: Option<Arc<TokenColMetaData<'static>>>,
    /// Metadata for COMPUTE (BY) result sets (`ALTMETADATA`), keyed by the
    /// COMPUTE clause id that the matching `ALTROW` rows refer back to.
    alt_metas: HashMap<u16, Arc<TokenAltMetaData<'static>>>,
    spn: Option<String>,
    /// Per-response deadline for reading command results, propagated from
    /// [`Config::command_timeout`](crate::Config::command_timeout) at connect
    /// time. `None` means unbounded. Read by the token stream to bound each
    /// server round-trip (see `TokenStream::try_unfold`).
    command_timeout: Option<Duration>,
    /// When `true`, NVARCHAR/NTEXT row values that contain malformed UTF-16
    /// (e.g. unpaired surrogates that SQL Server stored as unchecked UCS-2)
    /// are decoded losslessly by replacing each invalid sequence with the
    /// Unicode replacement character (U+FFFD) instead of aborting the row
    /// stream with a protocol error. Propagated from
    /// [`Config::lossy_utf16_decoding`](crate::Config::lossy_utf16_decoding) at
    /// connect time. Defaults to `false` (strict decoding).
    lossy_utf16: bool,
    attention: Arc<AttentionShared>,
    // DBStudio patch: byte cap for PLP values (`varchar(max)`, ...) and the pending truncation note.
    plp_limit: Option<usize>,
    plp_truncated: Option<u64>,
}

impl Context {
    pub fn new() -> Context {
        Context {
            // Overwritten with the server's negotiated TDS version from LOGINACK
            // on a successful login (`set_version`, called from
            // `TokenStream::get_login_ack` in stream/token.rs). tiberius targets
            // TDS 7.2+ (SQL Server 2005+), so this default's 7.2+ field widths
            // (4-byte ERROR/INFO LineNumber, 8-byte DONE row-count) are correct
            // for all supported servers even on the login-failure path, where no
            // LOGINACK ever arrives to update this value. A pre-7.2 server that
            // rejects login is out of supported scope.
            version: FeatureLevel::SqlServerN,
            packet_size: 4096,
            packet_id: 0,
            transaction_desc: [0; 8],
            last_meta: None,
            alt_metas: HashMap::new(),
            spn: None,
            command_timeout: None,
            lossy_utf16: false,
            attention: Arc::new(AttentionShared::default()),
            plp_limit: None,
            plp_truncated: None,
        }
    }

    // DBStudio patch: PLP value limit accessors.
    pub(crate) fn plp_limit(&self) -> Option<usize> {
        self.plp_limit
    }

    pub(crate) fn set_plp_limit(&mut self, limit: Option<usize>) {
        self.plp_limit = limit;
    }

    pub(crate) fn note_plp_truncated(&mut self, total: u64) {
        self.plp_truncated = Some(total);
    }

    pub(crate) fn take_plp_truncated(&mut self) -> Option<u64> {
        self.plp_truncated.take()
    }

    pub(crate) fn attention(&self) -> Arc<AttentionShared> {
        self.attention.clone()
    }

    pub fn next_packet_id(&mut self) -> u8 {
        let id = self.packet_id;
        self.packet_id = self.packet_id.wrapping_add(1);
        id
    }

    pub fn set_last_meta(&mut self, meta: Arc<TokenColMetaData<'static>>) {
        self.last_meta.replace(meta);
    }

    pub fn last_meta(&self) -> Option<Arc<TokenColMetaData<'static>>> {
        self.last_meta.clone()
    }

    /// Stores the metadata for a COMPUTE (BY) result set, keyed by its id, so
    /// that a following `ALTROW` token can be decoded.
    pub fn set_alt_meta(&mut self, meta: Arc<TokenAltMetaData<'static>>) {
        self.alt_metas.insert(meta.id, meta);
    }

    /// Retrieves previously seen COMPUTE (BY) metadata by its id.
    pub fn alt_meta(&self, id: u16) -> Option<Arc<TokenAltMetaData<'static>>> {
        self.alt_metas.get(&id).cloned()
    }

    pub fn packet_size(&self) -> u32 {
        self.packet_size
    }

    pub fn set_packet_size(&mut self, new_size: u32) {
        self.packet_size = new_size;
    }

    /// The per-response command timeout, if any. See
    /// [`Config::command_timeout`](crate::Config::command_timeout).
    pub(crate) fn command_timeout(&self) -> Option<Duration> {
        self.command_timeout
    }

    /// Records the per-response command timeout negotiated from the [`Config`]
    /// at connect time.
    ///
    /// [`Config`]: crate::Config
    pub(crate) fn set_command_timeout(&mut self, timeout: Option<Duration>) {
        self.command_timeout = timeout;
    }

    /// Whether malformed UTF-16 in NVARCHAR/NTEXT row values should be decoded
    /// losslessly (replacing invalid sequences with U+FFFD) rather than
    /// erroring. See
    /// [`Config::lossy_utf16_decoding`](crate::Config::lossy_utf16_decoding).
    pub(crate) fn lossy_utf16(&self) -> bool {
        self.lossy_utf16
    }

    /// Records whether lossy UTF-16 decoding is enabled, negotiated from the
    /// [`Config`](crate::Config) at connect time.
    pub(crate) fn set_lossy_utf16(&mut self, lossy: bool) {
        self.lossy_utf16 = lossy;
    }

    pub fn transaction_descriptor(&self) -> [u8; 8] {
        self.transaction_desc
    }

    pub fn set_transaction_descriptor(&mut self, desc: [u8; 8]) {
        self.transaction_desc = desc;
    }

    /// Records the protocol version negotiated with the server (from the
    /// LOGINACK token). This drives version-dependent decode paths such as the
    /// pre-2005 4-byte DONE rowcount and the 2-byte ERROR/INFO LineNumber.
    pub(crate) fn set_version(&mut self, version: FeatureLevel) {
        self.version = version;
    }

    pub fn version(&self) -> FeatureLevel {
        self.version
    }

    pub fn set_spn(&mut self, host: impl AsRef<str>, port: u16) {
        self.spn = Some(format!("MSSQLSvc/{}:{}", host.as_ref(), port));
    }

    #[cfg(any(
        windows,
        all(unix, any(feature = "integrated-auth-gssapi", feature = "sspi-rs"))
    ))]
    pub fn spn(&self) -> &str {
        self.spn.as_deref().unwrap_or("")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_has_expected_defaults() {
        let ctx = Context::new();
        assert_eq!(ctx.packet_size(), 4096);
        assert_eq!(ctx.transaction_descriptor(), [0; 8]);
        assert!(ctx.last_meta().is_none());
        assert!(ctx.alt_meta(0).is_none());
    }

    #[test]
    fn next_packet_id_increments_and_wraps() {
        let mut ctx = Context::new();
        assert_eq!(ctx.next_packet_id(), 0);
        assert_eq!(ctx.next_packet_id(), 1);
        assert_eq!(ctx.next_packet_id(), 2);

        // Force a wraparound to make sure it doesn't panic on overflow.
        for _ in 0..252 {
            ctx.next_packet_id();
        }
        assert_eq!(ctx.next_packet_id(), 255);
        assert_eq!(ctx.next_packet_id(), 0);
    }

    #[test]
    fn set_and_get_packet_size() {
        let mut ctx = Context::new();
        ctx.set_packet_size(8192);
        assert_eq!(ctx.packet_size(), 8192);
    }

    #[test]
    fn command_timeout_defaults_to_none_and_roundtrips() {
        let mut ctx = Context::new();
        assert_eq!(ctx.command_timeout(), None);

        ctx.set_command_timeout(Some(Duration::from_secs(5)));
        assert_eq!(ctx.command_timeout(), Some(Duration::from_secs(5)));

        ctx.set_command_timeout(None);
        assert_eq!(ctx.command_timeout(), None);
    }

    #[test]
    fn lossy_utf16_defaults_to_false_and_roundtrips() {
        let mut ctx = Context::new();
        assert!(!ctx.lossy_utf16());

        ctx.set_lossy_utf16(true);
        assert!(ctx.lossy_utf16());

        ctx.set_lossy_utf16(false);
        assert!(!ctx.lossy_utf16());
    }

    #[test]
    fn set_and_get_transaction_descriptor() {
        let mut ctx = Context::new();
        let desc = [1, 2, 3, 4, 5, 6, 7, 8];
        ctx.set_transaction_descriptor(desc);
        assert_eq!(ctx.transaction_descriptor(), desc);
    }

    #[test]
    fn set_and_get_last_meta() {
        let mut ctx = Context::new();
        let meta = Arc::new(TokenColMetaData { columns: vec![] });
        ctx.set_last_meta(meta.clone());

        let got = ctx.last_meta().unwrap();
        assert_eq!(got.columns.len(), meta.columns.len());
    }

    #[test]
    fn set_and_get_alt_meta_by_id() {
        let mut ctx = Context::new();
        let meta = Arc::new(TokenAltMetaData {
            id: 7,
            by_columns: vec![1, 2],
            columns: vec![],
        });
        ctx.set_alt_meta(meta.clone());

        let got = ctx.alt_meta(7).unwrap();
        assert_eq!(got.id, 7);
        assert_eq!(got.by_columns, vec![1, 2]);

        // A different id should still be absent.
        assert!(ctx.alt_meta(8).is_none());
    }

    #[test]
    fn version_defaults_to_sql_server_n() {
        let ctx = Context::new();
        assert_eq!(ctx.version(), FeatureLevel::SqlServerN);
    }

    #[test]
    fn set_spn_formats_service_principal_name() {
        let mut ctx = Context::new();
        ctx.set_spn("dbhost", 1433);

        #[cfg(any(
            windows,
            all(unix, any(feature = "integrated-auth-gssapi", feature = "sspi-rs"))
        ))]
        assert_eq!(ctx.spn(), "MSSQLSvc/dbhost:1433");

        // On platforms without an spn() accessor, at least make sure setting
        // it doesn't panic.
        #[cfg(not(any(
            windows,
            all(unix, any(feature = "integrated-auth-gssapi", feature = "sspi-rs"))
        )))]
        let _ = ctx;
    }
}
