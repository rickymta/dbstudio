//! The `.dbsbak` container: `"DBSBAK1\0" | u32 LE header_len | header JSON | payload`, where
//! the payload is `encrypt?(compress(tar(preamble.json, <entries>, manifest.json)))`.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use dbstudio_core::{Error, Result, Secret};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};

use crate::crypto::{DecryptReader, EncryptWriter, EncryptionHeader, Finish, hex};
use crate::{Compression, NativeTool};

/// File magic.
pub const MAGIC: &[u8; 8] = b"DBSBAK1\0";
/// Largest header accepted when reading.
const MAX_HEADER: usize = 16 * 1024 * 1024;
/// Name of the first tar entry.
pub const PREAMBLE: &str = "preamble.json";
/// Name of the last tar entry.
pub const MANIFEST: &str = "manifest.json";

/// Clear header of a backup file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Header {
    /// Always 1.
    pub format_version: u32,
    /// `none`, `gzip` or `zstd`.
    pub compression: String,
    /// Encryption parameters; `None` = plain payload.
    pub encryption: Option<EncryptionHeader>,
    /// Set for files made by a native tool.
    #[serde(default)]
    pub native: Option<NativeTool>,
    /// RFC 3339 creation time.
    pub created_at: String,
    /// The preamble, only when the payload is not encrypted.
    #[serde(default)]
    pub summary: Option<Json>,
}

/// One entry of the manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    /// Path inside the tar.
    pub path: String,
    /// Hex SHA-256 of the content.
    pub sha256: String,
    /// Content size.
    pub bytes: u64,
    /// Rows contained (data entries).
    #[serde(default)]
    pub rows: u64,
}

/// `manifest.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    /// Always 1.
    pub format_version: u32,
    /// The writer finished.
    pub complete: bool,
    /// Every entry except the manifest itself.
    pub entries: Vec<ManifestEntry>,
    /// Sum of the entry sizes.
    pub total_bytes: u64,
}

/// A file written to the spool directory, waiting to be packed.
#[derive(Debug, Clone)]
pub struct SpoolEntry {
    /// Name inside the tar.
    pub name: String,
    /// Where the content is now.
    pub path: PathBuf,
    /// Hex SHA-256.
    pub sha256: String,
    /// Size in bytes.
    pub bytes: u64,
    /// Rows contained.
    pub rows: u64,
}

/// Hashing writer for one spool file.
#[derive(Debug)]
pub struct SpoolWriter {
    name: String,
    path: PathBuf,
    out: BufWriter<File>,
    hasher: Sha256,
    bytes: u64,
    /// Rows written so far; the producer bumps it.
    pub rows: u64,
}

impl SpoolWriter {
    /// Create `dir/<n>` for the tar entry `name`.
    pub fn create(dir: &Path, index: usize, name: &str) -> Result<Self> {
        let path = dir.join(format!("{index:05}.spool"));
        Ok(SpoolWriter {
            name: name.to_string(),
            out: BufWriter::new(File::create(&path)?),
            path,
            hasher: Sha256::new(),
            bytes: 0,
            rows: 0,
        })
    }

    /// Bytes written so far.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Flush and describe the file.
    pub fn finish(mut self) -> Result<SpoolEntry> {
        self.out.flush()?;
        Ok(SpoolEntry {
            name: self.name,
            path: self.path,
            sha256: hex(&self.hasher.finalize()),
            bytes: self.bytes,
            rows: self.rows,
        })
    }
}

impl Write for SpoolWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let n = self.out.write(data)?;
        self.hasher.update(&data[..n]);
        self.bytes += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

/// Options of [`write_container`].
#[derive(Debug, Clone)]
pub struct ContainerOptions {
    /// Payload compression.
    pub compression: Compression,
    /// Passphrase when the payload is encrypted.
    pub passphrase: Option<Secret>,
    /// Set for native tool dumps.
    pub native: Option<NativeTool>,
    /// RFC 3339 creation time.
    pub created_at: String,
    /// KDF cost override (tests only).
    pub kdf_cost: Option<(u32, u32, u32)>,
}

struct FileSink(BufWriter<File>);

impl Write for FileSink {
    fn write(&mut self, d: &[u8]) -> io::Result<usize> {
        self.0.write(d)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl Finish for FileSink {
    fn finish_all(mut self: Box<Self>) -> io::Result<()> {
        self.0.flush()?;
        self.0.get_ref().sync_all()
    }
}

struct ZstdSink(zstd::stream::write::Encoder<'static, Box<dyn Finish>>);

impl Write for ZstdSink {
    fn write(&mut self, d: &[u8]) -> io::Result<usize> {
        self.0.write(d)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl Finish for ZstdSink {
    fn finish_all(self: Box<Self>) -> io::Result<()> {
        self.0.finish()?.finish_all()
    }
}

struct GzSink(flate2::write::GzEncoder<Box<dyn Finish>>);

impl Write for GzSink {
    fn write(&mut self, d: &[u8]) -> io::Result<usize> {
        self.0.write(d)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl Finish for GzSink {
    fn finish_all(self: Box<Self>) -> io::Result<()> {
        self.0.finish()?.finish_all()
    }
}

fn compression_name(c: Compression) -> &'static str {
    match c {
        Compression::None => "none",
        Compression::Gzip { .. } => "gzip",
        Compression::Zstd { .. } => "zstd",
    }
}

fn tar_append(
    b: &mut tar::Builder<Box<dyn Finish>>,
    name: &str,
    len: u64,
    data: &mut dyn Read,
) -> io::Result<()> {
    let mut h = tar::Header::new_gnu();
    h.set_size(len);
    h.set_mode(0o600);
    h.set_mtime(0);
    b.append_data(&mut h, name, data)
}

fn finalize(entries: &[SpoolEntry], preamble: &SpoolEntry) -> Manifest {
    let mut all = vec![ManifestEntry {
        path: preamble.name.clone(),
        sha256: preamble.sha256.clone(),
        bytes: preamble.bytes,
        rows: 0,
    }];
    all.extend(entries.iter().map(|e| ManifestEntry {
        path: e.name.clone(),
        sha256: e.sha256.clone(),
        bytes: e.bytes,
        rows: e.rows,
    }));
    let total_bytes = all.iter().map(|e| e.bytes).sum();
    Manifest {
        format_version: 1,
        complete: true,
        entries: all,
        total_bytes,
    }
}

/// Create the temporary output next to `dest`: a name no other writer uses, mode 0600.
fn create_part(dest: &Path) -> io::Result<(PathBuf, File)> {
    let dir = dest
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let part = dir.join(format!(".{name}.{}.part", uuid::Uuid::new_v4().simple()));
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts.open(&part)?;
    Ok((part, file))
}

/// Move `part` to `dest` without ever replacing an existing file; when `dest` is taken a
/// numeric suffix is added (`name-1.dbsbak`). Returns the path that was used.
fn publish_noclobber(part: &Path, dest: &Path) -> io::Result<PathBuf> {
    let stem = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let base = stem.strip_suffix(".dbsbak").unwrap_or(&stem).to_string();
    let dir = dest
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    for n in 0..10_000u32 {
        let candidate = if n == 0 {
            dest.to_path_buf()
        } else {
            dir.join(format!("{base}-{n}.dbsbak"))
        };
        // A hard link fails when the target exists, unlike a rename.
        match fs::hard_link(part, &candidate) {
            Ok(()) => {
                let _ = fs::remove_file(part);
                return Ok(candidate);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no free file name",
    ))
}

/// Pack `preamble` + `entries` + a generated manifest into `dest`. The data is written to a
/// private temporary file (0600) in the same directory and then published without replacing
/// anything (a taken name gets a numeric suffix), so a failed or cancelled pack leaves no file
/// and two writers never share one. Returns the final path and its size. `tick` is called with
/// the bytes of each entry as it is packed.
pub fn write_container(
    dest: &Path,
    opts: &ContainerOptions,
    preamble: &Json,
    entries: &[SpoolEntry],
    spool_dir: &Path,
    tick: &dyn Fn(u64) -> bool,
) -> Result<(PathBuf, u64)> {
    let preamble_bytes =
        serde_json::to_vec_pretty(preamble).map_err(|e| Error::Other(e.to_string()))?;
    let mut pw = SpoolWriter::create(spool_dir, 99_998, PREAMBLE)?;
    pw.write_all(&preamble_bytes)?;
    let preamble_entry = pw.finish()?;
    let manifest = finalize(entries, &preamble_entry);

    let encryption = opts.passphrase.as_ref().map(|_| match opts.kdf_cost {
        Some((m, t, p)) => EncryptionHeader::with_cost(m, t, p),
        None => EncryptionHeader::new(),
    });
    let header = Header {
        format_version: 1,
        compression: compression_name(opts.compression).into(),
        encryption: encryption.clone(),
        native: opts.native,
        created_at: opts.created_at.clone(),
        summary: if encryption.is_none() {
            Some(preamble.clone())
        } else {
            None
        },
    };
    let header_bytes = serde_json::to_vec(&header).map_err(|e| Error::Other(e.to_string()))?;
    let aad: [u8; 32] = Sha256::digest(&header_bytes).into();

    let (part, part_file) = create_part(dest)?;
    let result = (|| -> Result<()> {
        let mut file = BufWriter::new(part_file);
        file.write_all(MAGIC)?;
        file.write_all(&(header_bytes.len() as u32).to_le_bytes())?;
        file.write_all(&header_bytes)?;
        let mut sink: Box<dyn Finish> = Box::new(FileSink(file));
        if let (Some(enc), Some(pass)) = (&encryption, &opts.passphrase) {
            sink = Box::new(EncryptWriter::new(sink, enc, pass, aad)?);
        }
        sink = match opts.compression {
            Compression::None => sink,
            Compression::Gzip { level } => Box::new(GzSink(flate2::write::GzEncoder::new(
                sink,
                flate2::Compression::new(level.min(9)),
            ))),
            Compression::Zstd { level } => Box::new(ZstdSink(
                zstd::stream::write::Encoder::new(sink, level)
                    .map_err(|e| Error::Other(format!("zstd: {e}")))?,
            )),
        };
        let mut tar = tar::Builder::new(sink);
        tar_append(
            &mut tar,
            PREAMBLE,
            preamble_entry.bytes,
            &mut &preamble_bytes[..],
        )?;
        for e in entries {
            if !tick(e.bytes) {
                return Err(Error::Cancelled);
            }
            let mut f = File::open(&e.path)?;
            tar_append(&mut tar, &e.name, e.bytes, &mut f)?;
        }
        let manifest_bytes =
            serde_json::to_vec_pretty(&manifest).map_err(|e| Error::Other(e.to_string()))?;
        tar_append(
            &mut tar,
            MANIFEST,
            manifest_bytes.len() as u64,
            &mut &manifest_bytes[..],
        )?;
        tar.into_inner()?.finish_all()?;
        Ok(())
    })();
    match result {
        Ok(()) => match publish_noclobber(&part, dest) {
            Ok(path) => {
                let size = fs::metadata(&path)?.len();
                Ok((path, size))
            }
            Err(e) => {
                let _ = fs::remove_file(&part);
                Err(e.into())
            }
        },
        Err(e) => {
            let _ = fs::remove_file(&part);
            Err(e)
        }
    }
}

/// Read and parse the clear header. Returns it with the SHA-256 of its raw bytes (the AAD).
pub fn read_header(r: &mut dyn Read) -> Result<(Header, [u8; 32])> {
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)
        .map_err(|_| Error::Integrity("not a DBStudio backup file".into()))?;
    if &magic != MAGIC {
        return Err(Error::Integrity("not a DBStudio backup file".into()));
    }
    let mut len = [0u8; 4];
    r.read_exact(&mut len)
        .map_err(|_| Error::Integrity("truncated header".into()))?;
    let len = u32::from_le_bytes(len) as usize;
    if len == 0 || len > MAX_HEADER {
        return Err(Error::Integrity("bad header length".into()));
    }
    let mut raw = vec![0u8; len];
    r.read_exact(&mut raw)
        .map_err(|_| Error::Integrity("truncated header".into()))?;
    let header: Header = serde_json::from_slice(&raw)
        .map_err(|e| Error::Integrity(format!("unreadable header: {e}")))?;
    if header.format_version != 1 {
        return Err(Error::Unsupported(format!(
            "backup format version {}",
            header.format_version
        )));
    }
    Ok((header, Sha256::digest(&raw).into()))
}

/// Header of the file at `path`.
pub fn header_of(path: &Path) -> Result<Header> {
    let mut f = File::open(path)?;
    Ok(read_header(&mut f)?.0)
}

/// Map an error of the decrypt/decompress/tar chain to the right [`Error`].
fn chain_err(e: io::Error) -> Error {
    match e.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied | io::ErrorKind::StorageFull => {
            Error::Io(e)
        }
        _ => Error::Integrity(e.to_string()),
    }
}

/// Payload reader of `path`: header and the decrypted, decompressed tar stream.
pub fn open_payload(
    path: &Path,
    passphrase: Option<&Secret>,
) -> Result<(Header, Box<dyn Read + Send>)> {
    let mut file = io::BufReader::new(File::open(path)?);
    let (header, aad) = read_header(&mut file)?;
    let mut stream: Box<dyn Read + Send> = Box::new(file);
    if let Some(enc) = &header.encryption {
        let pass = passphrase.ok_or_else(|| Error::Integrity("passphrase required".into()))?;
        stream = Box::new(DecryptReader::new(stream, enc, pass, aad)?);
    }
    stream = match header.compression.as_str() {
        "none" => stream,
        "gzip" => Box::new(flate2::read::GzDecoder::new(stream)),
        "zstd" => Box::new(
            zstd::stream::read::Decoder::new(stream)
                .map_err(|e| Error::Integrity(format!("zstd: {e}")))?,
        ),
        other => return Err(Error::Unsupported(format!("compression {other}"))),
    };
    Ok((header, stream))
}

/// Preamble of the file (decrypts / decompresses only up to the first entry).
pub fn read_preamble(path: &Path, passphrase: Option<&Secret>) -> Result<Json> {
    let (_, stream) = open_payload(path, passphrase)?;
    let mut archive = tar::Archive::new(stream);
    let mut entries = archive.entries().map_err(chain_err)?;
    let mut first = entries
        .next()
        .ok_or_else(|| Error::Integrity("empty backup".into()))?
        .map_err(chain_err)?;
    let name = first
        .path()
        .map_err(chain_err)?
        .to_string_lossy()
        .into_owned();
    if name != PREAMBLE {
        return Err(Error::Integrity("preamble missing".into()));
    }
    let mut text = Vec::new();
    first.read_to_end(&mut text).map_err(chain_err)?;
    serde_json::from_slice(&text).map_err(|e| Error::Integrity(format!("bad preamble: {e}")))
}

/// What [`scan`] found.
#[derive(Debug)]
pub struct Scanned {
    /// Header.
    pub header: Header,
    /// Parsed preamble.
    pub preamble: Json,
    /// Manifest.
    pub manifest: Manifest,
    /// Extracted files by entry name (empty when not extracting).
    pub files: BTreeMap<String, PathBuf>,
}

fn safe_name(name: &str) -> Result<()> {
    let p = Path::new(name);
    let ok = !name.is_empty()
        && p.components()
            .all(|c| matches!(c, std::path::Component::Normal(_)));
    if ok {
        Ok(())
    } else {
        Err(Error::Integrity(format!("unsafe entry name {name:?}")))
    }
}

/// Read the whole file sequentially, hash every entry and compare with the manifest. With
/// `extract_to` the entries are also written there. `tick(bytes)` is called per entry and
/// returns false to cancel.
pub fn scan(
    path: &Path,
    passphrase: Option<&Secret>,
    extract_to: Option<&Path>,
    tick: &dyn Fn(u64) -> bool,
) -> Result<Scanned> {
    let (header, stream) = open_payload(path, passphrase)?;
    let mut archive = tar::Archive::new(stream);
    let mut seen: Vec<ManifestEntry> = Vec::new();
    let mut preamble: Option<Json> = None;
    let mut manifest: Option<Manifest> = None;
    let mut files = BTreeMap::new();
    for entry in archive.entries().map_err(chain_err)? {
        let mut entry = entry.map_err(chain_err)?;
        let name = entry
            .path()
            .map_err(chain_err)?
            .to_string_lossy()
            .into_owned();
        if manifest.is_some() {
            return Err(Error::Integrity("data after the manifest".into()));
        }
        safe_name(&name)?;
        if seen.is_empty() && name != PREAMBLE {
            return Err(Error::Integrity("preamble missing".into()));
        }
        if !tick(entry.size()) {
            return Err(Error::Cancelled);
        }
        if name == MANIFEST {
            let mut text = Vec::new();
            entry.read_to_end(&mut text).map_err(chain_err)?;
            manifest = Some(
                serde_json::from_slice(&text)
                    .map_err(|e| Error::Integrity(format!("bad manifest: {e}")))?,
            );
            continue;
        }
        let mut hasher = Sha256::new();
        let mut bytes = 0u64;
        let mut buf = vec![0u8; 64 * 1024];
        let mut out = match extract_to {
            Some(dir) if name != PREAMBLE => {
                let target = dir.join(&name);
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                files.insert(name.clone(), target.clone());
                Some(BufWriter::new(File::create(target)?))
            }
            _ => None,
        };
        let mut pre = Vec::new();
        loop {
            let n = entry.read(&mut buf).map_err(chain_err)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            bytes += n as u64;
            if let Some(o) = out.as_mut() {
                o.write_all(&buf[..n])?;
            }
            if name == PREAMBLE {
                pre.extend_from_slice(&buf[..n]);
            }
        }
        if let Some(mut o) = out {
            o.flush()?;
        }
        if name == PREAMBLE {
            preamble = Some(
                serde_json::from_slice(&pre)
                    .map_err(|e| Error::Integrity(format!("bad preamble: {e}")))?,
            );
        }
        seen.push(ManifestEntry {
            path: name,
            sha256: hex(&hasher.finalize()),
            bytes,
            rows: 0,
        });
    }
    let manifest =
        manifest.ok_or_else(|| Error::Integrity("manifest missing (incomplete file)".into()))?;
    if !manifest.complete {
        return Err(Error::Integrity("backup is marked incomplete".into()));
    }
    if manifest.entries.len() != seen.len() {
        return Err(Error::Integrity(
            "entry list differs from the manifest".into(),
        ));
    }
    for (m, s) in manifest.entries.iter().zip(&seen) {
        if m.path != s.path || m.bytes != s.bytes || m.sha256 != s.sha256 {
            return Err(Error::Integrity(format!("checksum mismatch in {}", m.path)));
        }
    }
    Ok(Scanned {
        header,
        preamble: preamble.unwrap_or_else(|| json!({})),
        manifest,
        files,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(dir: &Path) -> Vec<SpoolEntry> {
        let mut out = Vec::new();
        for (i, (name, text, rows)) in [
            ("schema/pre.sql", "CREATE TABLE t (a int);\n", 0),
            ("data/t.sql", "INSERT INTO t VALUES (1);\n", 1),
        ]
        .iter()
        .enumerate()
        {
            let mut w = SpoolWriter::create(dir, i, name).unwrap();
            w.write_all(text.as_bytes()).unwrap();
            w.rows = *rows;
            out.push(w.finish().unwrap());
        }
        out
    }

    fn opts(c: Compression, pass: Option<&str>) -> ContainerOptions {
        ContainerOptions {
            compression: c,
            passphrase: pass.map(Secret::new),
            native: None,
            created_at: "2026-01-01T00:00:00Z".into(),
            kdf_cost: Some((8, 1, 1)),
        }
    }

    fn build(c: Compression, pass: Option<&str>) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let spool = dir.path().join("spool");
        fs::create_dir(&spool).unwrap();
        let e = entries(&spool);
        let dest = dir.path().join("x.dbsbak");
        write_container(
            &dest,
            &opts(c, pass),
            &json!({"database": "app"}),
            &e,
            &spool,
            &|_| true,
        )
        .unwrap();
        (dir, dest)
    }

    #[test]
    fn roundtrip_all_modes() {
        for c in [
            Compression::None,
            Compression::Gzip { level: 6 },
            Compression::Zstd { level: 3 },
        ] {
            for pass in [None, Some("pw")] {
                let (dir, dest) = build(c, pass);
                let secret = pass.map(Secret::new);
                let out = dir.path().join("out");
                fs::create_dir(&out).unwrap();
                let s = scan(&dest, secret.as_ref(), Some(&out), &|_| true).unwrap();
                assert_eq!(s.preamble["database"], "app");
                assert_eq!(s.manifest.entries.len(), 3);
                assert_eq!(
                    fs::read_to_string(&s.files["data/t.sql"]).unwrap(),
                    "INSERT INTO t VALUES (1);\n"
                );
                assert_eq!(s.manifest.entries[2].rows, 1);
                let pre = read_preamble(&dest, secret.as_ref()).unwrap();
                assert_eq!(pre["database"], "app");
                let h = header_of(&dest).unwrap();
                assert_eq!(h.encryption.is_some(), pass.is_some());
                assert_eq!(h.summary.is_some(), pass.is_none());
            }
        }
    }

    #[test]
    fn wrong_or_missing_passphrase_is_integrity() {
        let (_d, dest) = build(Compression::Zstd { level: 3 }, Some("pw"));
        for p in [Some(Secret::new("nope")), None] {
            let e = scan(&dest, p.as_ref(), None, &|_| true).unwrap_err();
            assert_eq!(e.kind(), "integrity", "{e}");
        }
        assert_eq!(
            read_preamble(&dest, Some(&Secret::new("nope")))
                .unwrap_err()
                .kind(),
            "integrity"
        );
    }

    #[test]
    fn modified_files_are_integrity_errors() {
        for (c, pass) in [
            (Compression::None, None),
            (Compression::Gzip { level: 1 }, None),
            (Compression::Zstd { level: 1 }, Some("pw")),
        ] {
            let (_d, dest) = build(c, pass);
            let secret = pass.map(Secret::new);
            let good = fs::read(&dest).unwrap();
            // Flip one byte in the middle of the payload, then truncate.
            let mut bad = good.clone();
            let i = match c {
                Compression::None => good.windows(6).position(|w| w == b"CREATE").unwrap(),
                _ => good.len() - 40,
            };
            bad[i] ^= 0x55;
            fs::write(&dest, &bad).unwrap();
            let e = scan(&dest, secret.as_ref(), None, &|_| true).unwrap_err();
            assert_eq!(e.kind(), "integrity", "{c:?}: {e}");
            fs::write(&dest, &good[..good.len() - 1700.min(good.len() / 2)]).unwrap();
            let e = scan(&dest, secret.as_ref(), None, &|_| true).unwrap_err();
            assert_eq!(e.kind(), "integrity", "{c:?} truncated: {e}");
            // Tampering with the clear header of an encrypted file breaks the AAD.
            if pass.is_some() {
                let mut bad = good.clone();
                let pos = bad.windows(7).position(|w| w == b"created").unwrap();
                bad[pos + 12] ^= 1;
                fs::write(&dest, &bad).unwrap();
                let e = scan(&dest, secret.as_ref(), None, &|_| true).unwrap_err();
                assert_eq!(e.kind(), "integrity", "header tamper: {e}");
            }
        }
    }

    #[test]
    fn manifest_catches_a_silent_swap_in_a_plain_file() {
        let (_d, dest) = build(Compression::None, None);
        let mut bytes = fs::read(&dest).unwrap();
        // Change a data byte of the plain tar content ("INSERT" -> "INSERX").
        let pos = bytes.windows(6).position(|w| w == b"INSERT").unwrap();
        bytes[pos + 5] = b'X';
        fs::write(&dest, &bytes).unwrap();
        let e = scan(&dest, None, None, &|_| true).unwrap_err();
        assert_eq!(e.kind(), "integrity");
        assert!(e.to_string().contains("checksum mismatch"), "{e}");
    }

    #[test]
    fn rejects_foreign_files_and_cancel_leaves_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.dbsbak");
        fs::write(&f, b"hello world, not a backup").unwrap();
        assert_eq!(
            scan(&f, None, None, &|_| true).unwrap_err().kind(),
            "integrity"
        );

        let spool = dir.path().join("spool");
        fs::create_dir(&spool).unwrap();
        let e = entries(&spool);
        let dest = dir.path().join("c.dbsbak");
        let err = write_container(
            &dest,
            &opts(Compression::None, None),
            &json!({}),
            &e,
            &spool,
            &|_| false,
        )
        .unwrap_err();
        assert_eq!(err.kind(), "cancelled");
        assert!(!dest.exists());
        let left: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|f| f.file_name().to_string_lossy().ends_with(".part"))
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn a_taken_name_is_never_overwritten_and_files_are_private() {
        let dir = tempfile::tempdir().unwrap();
        let spool = dir.path().join("spool");
        fs::create_dir(&spool).unwrap();
        let e = entries(&spool);
        let dest = dir.path().join("x.dbsbak");
        let o = opts(Compression::None, None);
        let pre = json!({});
        let (a, _) = write_container(&dest, &o, &pre, &e, &spool, &|_| true).unwrap();
        let first = fs::read(&a).unwrap();
        let (b, _) = write_container(&dest, &o, &pre, &e, &spool, &|_| true).unwrap();
        assert_eq!(a, dest);
        assert_ne!(a, b);
        assert_eq!(b.file_name().unwrap(), "x-1.dbsbak");
        assert_eq!(fs::read(&a).unwrap(), first, "the first file is untouched");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for f in [&a, &b] {
                assert_eq!(fs::metadata(f).unwrap().permissions().mode() & 0o777, 0o600);
            }
        }
    }

    #[test]
    fn unsafe_entry_names_are_refused() {
        assert!(safe_name("data/a.sql").is_ok());
        assert!(safe_name("../a").is_err());
        assert!(safe_name("/etc/passwd").is_err());
        assert!(safe_name("").is_err());
    }
}
