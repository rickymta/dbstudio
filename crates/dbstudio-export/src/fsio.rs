//! Output files: written to a hidden temporary file next to the destination and renamed when
//! complete, so a cancelled or failed export never leaves a partial file behind.

use std::fs::File;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use dbstudio_core::{Error, Result};
use uuid::Uuid;
use zip::CompressionMethod;
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

/// Hidden temporary path next to `dest`: `<dir>/.<name>.dbstudio-tmp-<random>`.
pub(crate) fn tmp_path_for(dest: &Path) -> PathBuf {
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "export".into());
    dest.with_file_name(format!(".{name}.dbstudio-tmp-{}", Uuid::new_v4().simple()))
}

/// Check that `dest` can be created: its directory exists and the file does not exist unless
/// `overwrite`.
pub(crate) fn check_destination(dest: &Path, overwrite: bool) -> Result<()> {
    if dest.file_name().is_none() || dest.is_dir() {
        return Err(Error::Invalid(format!(
            "invalid destination: {}",
            dest.display()
        )));
    }
    let parent = match dest.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    if !parent.is_dir() {
        return Err(Error::Invalid(format!(
            "destination folder does not exist: {}",
            parent.display()
        )));
    }
    if dest.exists() && !overwrite {
        return Err(Error::Io(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("file already exists: {}", dest.display()),
        )));
    }
    Ok(())
}

/// Writer that counts the bytes passing through it.
#[derive(Debug)]
pub(crate) struct Counting<W> {
    inner: W,
    count: Arc<AtomicU64>,
}

impl<W: Write> Write for Counting<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.count.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl<W: Seek> Seek for Counting<W> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}

type FileOut = Counting<BufWriter<File>>;

enum Inner {
    Plain(FileOut),
    Zip(Box<ZipWriter<FileOut>>),
    /// The file is produced by somebody else (XLSX); only the rename is ours.
    External,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Inner::Plain(_) => "Plain",
            Inner::Zip(_) => "Zip",
            Inner::External => "External",
        })
    }
}

/// A file being written (plain or zip archive); removed on drop unless committed.
#[derive(Debug)]
pub(crate) struct Output {
    dest: PathBuf,
    tmp: PathBuf,
    inner: Option<Inner>,
    count: Arc<AtomicU64>,
    entry_open: bool,
}

impl Output {
    /// Create the temporary file of `dest`; `zip` makes it an archive that takes several entries.
    pub fn create(dest: &Path, overwrite: bool, zip: bool) -> Result<Output> {
        check_destination(dest, overwrite)?;
        let tmp = tmp_path_for(dest);
        let file = File::create(&tmp)?;
        let count = Arc::new(AtomicU64::new(0));
        let out = Counting {
            inner: BufWriter::with_capacity(256 * 1024, file),
            count: Arc::clone(&count),
        };
        let inner = if zip {
            Inner::Zip(Box::new(ZipWriter::new(out)))
        } else {
            Inner::Plain(out)
        };
        Ok(Output {
            dest: dest.to_path_buf(),
            tmp,
            inner: Some(inner),
            count,
            entry_open: false,
        })
    }

    /// An output whose content is written to [`Output::tmp_path`] by another library.
    pub fn external(dest: &Path, overwrite: bool) -> Result<Output> {
        check_destination(dest, overwrite)?;
        Ok(Output {
            dest: dest.to_path_buf(),
            tmp: tmp_path_for(dest),
            inner: Some(Inner::External),
            count: Arc::new(AtomicU64::new(0)),
            entry_open: false,
        })
    }

    /// Temporary file path (for [`Output::external`]).
    pub fn tmp_path(&self) -> &Path {
        &self.tmp
    }

    /// Directory of the destination (temporary files of libraries go there).
    pub fn dir(&self) -> PathBuf {
        match self.dest.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        }
    }

    /// Bytes written to disk so far.
    pub fn bytes(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    /// Start a new archive entry (zip) — a plain file accepts exactly one "entry".
    pub fn start_entry(&mut self, name: &str) -> Result<()> {
        match self.inner.as_mut() {
            Some(Inner::Zip(z)) => {
                let opts = SimpleFileOptions::default()
                    .compression_method(CompressionMethod::Deflated)
                    .large_file(true);
                z.start_file(name, opts)
                    .map_err(|e| Error::Other(format!("zip: {e}")))?;
                self.entry_open = true;
                Ok(())
            }
            Some(Inner::Plain(_)) if !self.entry_open => {
                self.entry_open = true;
                Ok(())
            }
            _ => Err(Error::Other("output cannot take another entry".into())),
        }
    }

    /// Finish the file: flush, fsync, rename to the destination. Returns the final size.
    pub fn commit(mut self) -> Result<(PathBuf, u64)> {
        match self.inner.take() {
            Some(Inner::Plain(mut w)) => {
                w.flush()?;
                let file = w
                    .inner
                    .into_inner()
                    .map_err(|e| Error::Io(e.into_error()))?;
                file.sync_all()?;
            }
            Some(Inner::Zip(z)) => {
                let mut w = z.finish().map_err(|e| Error::Other(format!("zip: {e}")))?;
                w.flush()?;
                let file = w
                    .inner
                    .into_inner()
                    .map_err(|e| Error::Io(e.into_error()))?;
                file.sync_all()?;
            }
            Some(Inner::External) => {
                File::open(&self.tmp)?.sync_all()?;
            }
            None => return Err(Error::Other("output already finished".into())),
        }
        let size = std::fs::metadata(&self.tmp)?.len();
        std::fs::rename(&self.tmp, &self.dest)?;
        Ok((self.dest.clone(), size))
    }
}

impl Write for Output {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.inner.as_mut() {
            Some(Inner::Plain(w)) => w.write(buf),
            Some(Inner::Zip(z)) => z.write(buf),
            _ => Err(io::Error::other("output is not writable")),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self.inner.as_mut() {
            Some(Inner::Plain(w)) => w.flush(),
            Some(Inner::Zip(z)) => z.flush(),
            _ => Ok(()),
        }
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        if self.inner.is_some() {
            // Not committed: close the handle first (Windows), then remove the partial file.
            self.inner = None;
            let _ = std::fs::remove_file(&self.tmp);
        }
    }
}

/// `.<name>.dbstudio-tmp-<32 hex digits>`: exactly what [`temp_path`] generates.
fn is_temp_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix('.') else {
        return false;
    };
    match rest.rsplit_once(".dbstudio-tmp-") {
        Some((stem, id)) => {
            !stem.is_empty() && id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())
        }
        None => false,
    }
}

/// Remove temporary export files (`.name.dbstudio-tmp-<id>`) in `dir` that were last written
/// more than `older_than` ago: leftovers of an export that was killed. Returns how many files
/// were removed.
pub fn cleanup_orphans(dir: &Path, older_than: std::time::Duration) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !is_temp_name(&name) {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age >= older_than);
        if old && entry.path().is_file() && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Zip entry / sheet friendly file name: no path separators or reserved characters.
pub(crate) fn safe_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.');
    if trimmed.is_empty() {
        "object".into()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_renames_and_drop_removes() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("a.txt");
        let mut out = Output::create(&dest, false, false).unwrap();
        out.start_entry("a").unwrap();
        out.write_all(b"hello").unwrap();
        let tmp = out.tmp_path().to_path_buf();
        assert!(tmp.exists() && !dest.exists());
        let (path, size) = out.commit().unwrap();
        assert_eq!((path, size), (dest.clone(), 5));
        assert!(!tmp.exists());

        // Dropped without commit: nothing is left.
        let other = dir.path().join("b.txt");
        let out = Output::create(&other, false, false).unwrap();
        let tmp = out.tmp_path().to_path_buf();
        drop(out);
        assert!(!tmp.exists() && !other.exists());
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers.len(), 1);
    }

    #[test]
    fn existing_file_needs_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("a.txt");
        std::fs::write(&dest, "x").unwrap();
        assert!(matches!(
            Output::create(&dest, false, false),
            Err(Error::Io(e)) if e.kind() == io::ErrorKind::AlreadyExists
        ));
        assert!(Output::create(&dest, true, false).is_ok());
        let missing = dir.path().join("no/such/dir/a.txt");
        assert!(Output::create(&missing, true, false).is_err());
    }

    #[test]
    fn zip_takes_entries() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("a.zip");
        let mut out = Output::create(&dest, false, true).unwrap();
        out.start_entry("one.txt").unwrap();
        out.write_all(b"1").unwrap();
        out.start_entry("two.txt").unwrap();
        out.write_all(b"22").unwrap();
        out.commit().unwrap();
        let mut z = zip::ZipArchive::new(File::open(&dest).unwrap()).unwrap();
        assert_eq!(z.len(), 2);
        let mut s = String::new();
        io::Read::read_to_string(&mut z.by_name("two.txt").unwrap(), &mut s).unwrap();
        assert_eq!(s, "22");
    }

    #[test]
    fn orphans_are_removed_when_old_enough() {
        let dir = tempfile::tempdir().unwrap();
        let orphan = dir
            .path()
            .join(".data.csv.dbstudio-tmp-0123456789abcdef0123456789abcdef");
        std::fs::write(&orphan, "x").unwrap();
        let other = dir.path().join("data.csv");
        std::fs::write(&other, "x").unwrap();
        let hidden = dir.path().join(".gitignore");
        std::fs::write(&hidden, "x").unwrap();
        // Lookalikes are the user's files: only the exact generated pattern is removed.
        let lookalike = dir.path().join(".notes.dbstudio-tmp-keepme");
        std::fs::write(&lookalike, "x").unwrap();
        // Too young: kept (it may belong to an export that is running).
        assert_eq!(
            cleanup_orphans(dir.path(), std::time::Duration::from_secs(3600)),
            0
        );
        assert!(orphan.exists());
        assert_eq!(cleanup_orphans(dir.path(), std::time::Duration::ZERO), 1);
        assert!(!orphan.exists() && other.exists() && hidden.exists() && lookalike.exists());
        assert_eq!(
            cleanup_orphans(&dir.path().join("missing"), std::time::Duration::ZERO),
            0
        );
    }

    #[test]
    fn safe_names() {
        assert_eq!(safe_name("a/b:c"), "a_b_c");
        assert_eq!(safe_name("  "), "object");
        assert_eq!(safe_name("public.orders"), "public.orders");
    }
}
