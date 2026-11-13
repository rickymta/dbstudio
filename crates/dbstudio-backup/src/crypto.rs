//! Chunked AES-256-GCM stream encryption with an Argon2id derived key.
//!
//! The plaintext is cut into chunks of `chunk_size` bytes. Chunk `i` is sealed with the nonce
//! `noncePrefix (7 B) || i as u32 BE || last (1 B)` and the additional data `AAD` (SHA-256 of
//! the clear header), so reordering, truncation, header tampering and a wrong passphrase all
//! surface as an authentication failure. The last chunk (possibly empty) carries `last = 1`.

use std::io::{self, Read, Write};

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use argon2::{Algorithm, Argon2, Params, Version};
use dbstudio_core::{Error, Result, Secret};
use serde::{Deserialize, Serialize};

/// Size of the GCM authentication tag.
const TAG_LEN: usize = 16;
/// Algorithm identifier stored in the header.
pub const ALG: &str = "aes-256-gcm-chunked";
/// KDF identifier stored in the header.
pub const KDF: &str = "argon2id";
/// Default plaintext chunk size (1 MiB).
pub const CHUNK_SIZE: u32 = 1024 * 1024;

/// `encryption` object of the file header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EncryptionHeader {
    /// Always [`ALG`].
    pub alg: String,
    /// Always [`KDF`].
    pub kdf: String,
    /// Hex encoded Argon2 salt.
    pub salt: String,
    /// Argon2 memory cost in KiB.
    pub m: u32,
    /// Argon2 time cost.
    pub t: u32,
    /// Argon2 parallelism.
    pub p: u32,
    /// Hex encoded 7 byte nonce prefix.
    pub nonce_prefix: String,
    /// Plaintext bytes per chunk.
    pub chunk_size: u32,
}

/// KDF cost used for new files; tests lower it through [`EncryptionHeader::with_cost`].
const DEFAULT_M: u32 = 65_536;
const DEFAULT_T: u32 = 3;
const DEFAULT_P: u32 = 1;

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn unhex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    let mut filled = 0;
    while filled < N {
        let id = uuid::Uuid::new_v4();
        let b = id.as_bytes();
        let n = (N - filled).min(16);
        out[filled..filled + n].copy_from_slice(&b[..n]);
        filled += n;
    }
    out
}

impl EncryptionHeader {
    /// Fresh salt and nonce prefix with the default KDF cost.
    pub fn new() -> Self {
        Self::with_cost(DEFAULT_M, DEFAULT_T, DEFAULT_P)
    }

    /// Fresh salt and nonce prefix with an explicit KDF cost (tests use a tiny one).
    pub fn with_cost(m: u32, t: u32, p: u32) -> Self {
        EncryptionHeader {
            alg: ALG.into(),
            kdf: KDF.into(),
            salt: hex(&random_bytes::<16>()),
            m,
            t,
            p,
            nonce_prefix: hex(&random_bytes::<7>()),
            chunk_size: CHUNK_SIZE,
        }
    }

    /// Derive the cipher for `passphrase`.
    fn cipher(&self, passphrase: &Secret) -> Result<(Aes256Gcm, [u8; 7])> {
        if self.alg != ALG || self.kdf != KDF {
            return Err(Error::Unsupported(format!(
                "backup encryption {}/{}",
                self.alg, self.kdf
            )));
        }
        let salt = unhex(&self.salt).ok_or_else(|| Error::Integrity("bad salt".into()))?;
        let prefix: [u8; 7] = unhex(&self.nonce_prefix)
            .and_then(|v| v.try_into().ok())
            .ok_or_else(|| Error::Integrity("bad nonce prefix".into()))?;
        if self.chunk_size == 0 || self.chunk_size > 64 * 1024 * 1024 {
            return Err(Error::Integrity("bad chunk size".into()));
        }
        let params = Params::new(self.m, self.t, self.p, Some(32))
            .map_err(|e| Error::Integrity(format!("bad KDF parameters: {e}")))?;
        let mut key = [0u8; 32];
        Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
            .hash_password_into(passphrase.expose().as_bytes(), &salt, &mut key)
            .map_err(|e| Error::Other(format!("key derivation failed: {e}")))?;
        let cipher = Aes256Gcm::new_from_slice(&key)
            .map_err(|_| Error::Other("invalid key length".into()))?;
        Ok((cipher, prefix))
    }
}

impl Default for EncryptionHeader {
    fn default() -> Self {
        Self::new()
    }
}

fn nonce(prefix: &[u8; 7], counter: u32, last: bool) -> Nonce<aes_gcm::aead::consts::U12> {
    let mut n = [0u8; 12];
    n[..7].copy_from_slice(prefix);
    n[7..11].copy_from_slice(&counter.to_be_bytes());
    n[11] = u8::from(last);
    Nonce::from(n)
}

fn integrity_io(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

/// A [`Write`] that can be closed, flushing every layer below it.
pub(crate) trait Finish: Write + Send {
    /// Flush and close this layer and everything below.
    fn finish_all(self: Box<Self>) -> io::Result<()>;
}

/// Encrypting layer over a [`Finish`] writer.
pub(crate) struct EncryptWriter {
    inner: Box<dyn Finish>,
    cipher: Aes256Gcm,
    prefix: [u8; 7],
    aad: Vec<u8>,
    chunk: usize,
    counter: u32,
    buf: Vec<u8>,
}

impl EncryptWriter {
    pub(crate) fn new(
        inner: Box<dyn Finish>,
        header: &EncryptionHeader,
        passphrase: &Secret,
        aad: [u8; 32],
    ) -> Result<Self> {
        let (cipher, prefix) = header.cipher(passphrase)?;
        Ok(EncryptWriter {
            inner,
            cipher,
            prefix,
            aad: aad.to_vec(),
            chunk: header.chunk_size as usize,
            counter: 0,
            buf: Vec::new(),
        })
    }

    fn seal(&mut self, len: usize, last: bool) -> io::Result<()> {
        let plain: Vec<u8> = self.buf.drain(..len).collect();
        let ct = self
            .cipher
            .encrypt(
                &nonce(&self.prefix, self.counter, last),
                Payload {
                    msg: &plain,
                    aad: &self.aad,
                },
            )
            .map_err(|_| io::Error::other("encryption failed"))?;
        self.counter = self
            .counter
            .checked_add(1)
            .ok_or_else(|| io::Error::other("backup too large"))?;
        self.inner.write_all(&ct)
    }
}

impl Write for EncryptWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        // Keep strictly more than one chunk buffered before sealing a non-last chunk.
        while self.buf.len() > self.chunk {
            self.seal(self.chunk, false)?;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Finish for EncryptWriter {
    fn finish_all(mut self: Box<Self>) -> io::Result<()> {
        let n = self.buf.len();
        self.seal(n, true)?;
        let inner = self.inner;
        inner.finish_all()
    }
}

/// Decrypting layer over any reader.
pub(crate) struct DecryptReader<R: Read> {
    inner: R,
    cipher: Aes256Gcm,
    prefix: [u8; 7],
    aad: Vec<u8>,
    block: usize,
    counter: u32,
    /// Next encrypted block, read ahead to know whether the current one is the last.
    ahead: Option<Vec<u8>>,
    started: bool,
    plain: Vec<u8>,
    pos: usize,
    done: bool,
}

impl<R: Read> DecryptReader<R> {
    pub(crate) fn new(
        inner: R,
        header: &EncryptionHeader,
        passphrase: &Secret,
        aad: [u8; 32],
    ) -> Result<Self> {
        let (cipher, prefix) = header.cipher(passphrase)?;
        Ok(DecryptReader {
            inner,
            cipher,
            prefix,
            aad: aad.to_vec(),
            block: header.chunk_size as usize + TAG_LEN,
            counter: 0,
            ahead: None,
            started: false,
            plain: Vec::new(),
            pos: 0,
            done: false,
        })
    }

    fn read_block(&mut self) -> io::Result<Vec<u8>> {
        let mut buf = vec![0u8; self.block];
        let mut filled = 0;
        while filled < buf.len() {
            let n = self.inner.read(&mut buf[filled..])?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        buf.truncate(filled);
        Ok(buf)
    }

    fn next_chunk(&mut self) -> io::Result<()> {
        if !self.started {
            self.ahead = Some(self.read_block()?);
            self.started = true;
        }
        let current = self.ahead.take().unwrap_or_default();
        if current.len() < TAG_LEN {
            return Err(integrity_io("truncated encrypted payload"));
        }
        let next = self.read_block()?;
        let last = next.is_empty();
        if !last {
            self.ahead = Some(next);
        }
        let plain = self
            .cipher
            .decrypt(
                &nonce(&self.prefix, self.counter, last),
                Payload {
                    msg: &current,
                    aad: &self.aad,
                },
            )
            .map_err(|_| {
                integrity_io("authentication failed (wrong passphrase or modified file)")
            })?;
        self.counter = self.counter.wrapping_add(1);
        self.plain = plain;
        self.pos = 0;
        self.done = last;
        Ok(())
    }
}

impl<R: Read> Read for DecryptReader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        while self.pos >= self.plain.len() {
            if self.done {
                return Ok(0);
            }
            self.next_chunk()?;
        }
        let n = out.len().min(self.plain.len() - self.pos);
        out[..n].copy_from_slice(&self.plain[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct Mem(Arc<Mutex<Vec<u8>>>);
    impl Write for Mem {
        fn write(&mut self, d: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(d);
            Ok(d.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl Finish for Mem {
        fn finish_all(self: Box<Self>) -> io::Result<()> {
            Ok(())
        }
    }

    fn header() -> EncryptionHeader {
        let mut h = EncryptionHeader::with_cost(8, 1, 1);
        h.chunk_size = 16;
        h
    }

    fn seal(data: &[u8], pass: &str, h: &EncryptionHeader) -> Vec<u8> {
        let out = Arc::new(Mutex::new(Vec::new()));
        let mut w = Box::new(
            EncryptWriter::new(Box::new(Mem(out.clone())), h, &Secret::new(pass), [7; 32]).unwrap(),
        );
        w.write_all(data).unwrap();
        w.finish_all().unwrap();
        out.lock().unwrap().clone()
    }

    fn open(ct: &[u8], pass: &str, h: &EncryptionHeader) -> io::Result<Vec<u8>> {
        let mut r = DecryptReader::new(ct, h, &Secret::new(pass), [7; 32]).unwrap();
        let mut v = Vec::new();
        r.read_to_end(&mut v)?;
        Ok(v)
    }

    #[test]
    fn roundtrip_various_lengths() {
        let h = header();
        for len in [0usize, 1, 15, 16, 17, 32, 33, 100] {
            let data: Vec<u8> = (0..len as u8).collect();
            let ct = seal(&data, "pw", &h);
            assert_eq!(open(&ct, "pw", &h).unwrap(), data, "len {len}");
        }
    }

    #[test]
    fn wrong_passphrase_tamper_and_truncation_fail() {
        let h = header();
        let data = vec![5u8; 40];
        let ct = seal(&data, "pw", &h);
        assert!(open(&ct, "other", &h).is_err());
        let mut bad = ct.clone();
        bad[3] ^= 1;
        assert!(open(&bad, "pw", &h).is_err());
        // Dropping the last chunk must not look like a valid shorter stream.
        let cut = &ct[..ct.len() - (ct.len() % (16 + TAG_LEN)).max(TAG_LEN)];
        assert!(open(cut, "pw", &h).is_err());
        assert!(open(&ct[..ct.len() - 1], "pw", &h).is_err());
        // Different AAD (header tamper).
        let mut r = DecryptReader::new(&ct[..], &h, &Secret::new("pw"), [8; 32]).unwrap();
        assert!(r.read_to_end(&mut Vec::new()).is_err());
    }

    #[test]
    fn hex_roundtrip() {
        assert_eq!(unhex(&hex(&[0, 1, 255])).unwrap(), vec![0, 1, 255]);
        assert!(unhex("abc").is_none());
        assert!(unhex("zz").is_none());
    }
}
