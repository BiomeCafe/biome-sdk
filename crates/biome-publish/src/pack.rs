//! Hashing and client archives.
//!
//! A client folder becomes a zip that depends only on the folder's file
//! names, contents and (on Unix) executable bits: entries sorted by name,
//! `/` separators, every timestamp 1980-01-01 00:00:00, Unix permissions
//! 0644 or 0755, deflate level 6. The same folder zipped twice is the same
//! bytes and the same sha256, which is what lets `upload` resume a release
//! whose file metadata must match the first attempt exactly.

use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use base64::Engine as _;
use sha2::{Digest, Sha256};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, DateTime, System, ZipArchive, ZipWriter};

use crate::error::Error;

/// Largest client archive the server accepts.
pub const MAX_CLIENT_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Lowercase hex.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// sha256 of `bytes`, lowercase hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// sha256 of a file, streamed from disk: `(size, lowercase hex)`.
pub fn sha256_file(path: &Path) -> io::Result<(u64, String)> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut size = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    Ok((size, hex(&hasher.finalize())))
}

/// The `x-amz-checksum-sha256` form of a digest: base64 of the raw 32
/// bytes. `None` if `sha256_hex` is not 64 hex digits.
pub fn checksum_header(sha256_hex: &str) -> Option<String> {
    if sha256_hex.len() != 64 {
        return None;
    }
    let raw: Option<Vec<u8>> = (0..32).map(|i| u8::from_str_radix(sha256_hex.get(i * 2..i * 2 + 2)?, 16).ok()).collect();
    Some(base64::engine::general_purpose::STANDARD.encode(raw?))
}

/// One file of a client folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderEntry {
    /// Name inside the archive: relative, `/`-separated.
    pub name: String,
    pub source: PathBuf,
    pub size: u64,
    /// Unix only: the file has an executable bit. Always false elsewhere.
    pub executable: bool,
}

/// Every file under `root` (following links, refusing loops), sorted by
/// archive name. Directories themselves are not entries.
pub fn list_folder(root: &Path) -> Result<Vec<FolderEntry>, Error> {
    let mut out = Vec::new();
    let mut ancestors = Vec::new();
    walk(root, "", &mut ancestors, &mut out)?;
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

fn walk(dir: &Path, prefix: &str, ancestors: &mut Vec<PathBuf>, out: &mut Vec<FolderEntry>) -> Result<(), Error> {
    let canonical = fs::canonicalize(dir).map_err(|e| read_err(dir, e))?;
    if ancestors.contains(&canonical) {
        return Err(Error::invalid(format!("{} links back to one of its parent folders", dir.display())));
    }
    ancestors.push(canonical);
    for entry in fs::read_dir(dir).map_err(|e| read_err(dir, e))? {
        let entry = entry.map_err(|e| read_err(dir, e))?;
        let path = entry.path();
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            return Err(Error::invalid(format!("{}: file name is not valid UTF-8", path.display())));
        };
        if name.contains('\\') {
            return Err(Error::invalid(format!("{}: file name contains a backslash", path.display())));
        }
        let rel = if prefix.is_empty() { name.to_string() } else { format!("{prefix}/{name}") };
        let meta = match fs::metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(Error::invalid(format!("{} is a broken link", path.display())))
            }
            Err(e) => return Err(read_err(&path, e)),
        };
        if meta.is_dir() {
            walk(&path, &rel, ancestors, out)?;
        } else if meta.is_file() {
            out.push(FolderEntry { name: rel, source: path, size: meta.len(), executable: has_exec_bit(&meta) });
        } else {
            return Err(Error::invalid(format!("{} is not a regular file", path.display())));
        }
    }
    ancestors.pop();
    Ok(())
}

#[cfg(unix)]
fn has_exec_bit(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn has_exec_bit(_meta: &fs::Metadata) -> bool {
    false
}

fn read_err(path: &Path, e: io::Error) -> Error {
    Error::io(format!("cannot read {}: {e}", path.display()))
}

/// Write `entries` (from [`list_folder`]) to `dest` as a deterministic zip.
/// The entrypoint, and on Unix anything with an executable bit, is stored
/// as 0755; everything else 0644. Written to `<dest>.part` and renamed, so
/// an interrupted run never leaves a truncated archive under the real name.
pub fn zip_folder(entries: &[FolderEntry], entrypoint: &str, dest: &Path) -> Result<(), Error> {
    let part = dest.with_extension("zip.part");
    let write_err = |e: &dyn std::fmt::Display| Error::io(format!("cannot write {}: {e}", part.display()));
    let file = File::create(&part).map_err(|e| write_err(&e))?;
    let mut zip = ZipWriter::new(BufWriter::with_capacity(1 << 20, file));
    for entry in entries {
        let mode = if entry.executable || entry.name == entrypoint { 0o755 } else { 0o644 };
        let options = SimpleFileOptions::DEFAULT
            .compression_method(CompressionMethod::Deflated)
            .compression_level(Some(6))
            .last_modified_time(DateTime::DEFAULT)
            .system(System::Unix)
            .unix_permissions(mode)
            .large_file(entry.size >= u64::from(u32::MAX));
        zip.start_file(entry.name.as_str(), options).map_err(|e| write_err(&e))?;
        let mut src = File::open(&entry.source).map_err(|e| read_err(&entry.source, e))?;
        io::copy(&mut src, &mut zip).map_err(|e| Error::io(format!("cannot zip {}: {e}", entry.source.display())))?;
    }
    let writer = zip.finish().map_err(|e| write_err(&e))?;
    let mut file = writer.into_inner().map_err(|e| write_err(e.error()))?;
    file.flush().map_err(|e| write_err(&e))?;
    file.sync_all().map_err(|e| write_err(&e))?;
    drop(file);
    fs::rename(&part, dest).map_err(|e| Error::io(format!("cannot rename {} to {}: {e}", part.display(), dest.display())))
}

/// Names of the file entries of an existing zip. Refuses archives an
/// installer could not unpack safely: names that are absolute or climb out
/// with `..`, and `\` separators (Windows PowerShell 5.1's
/// `Compress-Archive` writes those).
pub fn zip_file_names(path: &Path) -> Result<Vec<String>, Error> {
    let file = File::open(path).map_err(|e| read_err(path, e))?;
    let mut archive = ZipArchive::new(BufReader::new(file))
        .map_err(|e| Error::invalid(format!("{} is not a readable zip: {e}", path.display())))?;
    let mut names = Vec::new();
    for i in 0..archive.len() {
        let entry = archive
            .by_index_raw(i)
            .map_err(|e| Error::invalid(format!("{} entry {i} is unreadable: {e}", path.display())))?;
        let name = entry.name().to_string();
        if name.contains('\\') {
            return Err(Error::invalid(format!(
                "{}: entry {name:?} uses \\ as a separator; rebuild the zip with / separators (or point path at the folder and let biome-publish zip it)",
                path.display()
            )));
        }
        if escapes(&name) || entry.enclosed_name().is_none() {
            return Err(Error::invalid(format!("{}: entry {name:?} would unpack outside the install folder", path.display())));
        }
        if !entry.is_dir() {
            names.push(name);
        }
    }
    Ok(names)
}

/// Absolute, drive-prefixed, NUL-carrying or `..`-climbing entry names.
/// (`zip`'s own `enclosed_name` tolerates a leading `/`; an installer that
/// joins names to its folder must not.)
fn escapes(name: &str) -> bool {
    let b = name.as_bytes();
    name.is_empty()
        || name.starts_with('/')
        || (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':')
        || name.contains('\0')
        || name.split('/').any(|seg| seg == "..")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, bytes: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn sample_folder(root: &Path) {
        write(&root.join("Game.exe"), b"MZ fake executable");
        write(&root.join("data/levels/01.json"), br#"{"level":1}"#);
        write(&root.join("data/levels/02.json"), br#"{"level":2}"#);
        write(&root.join("data/a-b.txt"), &[7u8; 10_000]);
        write(&root.join("README.txt"), b"hello");
    }

    #[test]
    fn hex_and_checksum_header() {
        // sha256("abc"), from FIPS 180-2.
        let h = sha256_hex(b"abc");
        assert_eq!(h, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(checksum_header(&h).unwrap(), "ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0=");
        assert_eq!(checksum_header("abc"), None);
        assert_eq!(checksum_header(&"zz".repeat(32)), None);
    }

    #[test]
    fn sha256_file_matches_in_memory_hash() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.bin");
        let bytes: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        fs::write(&p, &bytes).unwrap();
        assert_eq!(sha256_file(&p).unwrap(), (bytes.len() as u64, sha256_hex(&bytes)));
    }

    #[test]
    fn folder_listing_is_sorted_with_forward_slashes() {
        let dir = tempfile::tempdir().unwrap();
        sample_folder(dir.path());
        let names: Vec<_> = list_folder(dir.path()).unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["Game.exe", "README.txt", "data/a-b.txt", "data/levels/01.json", "data/levels/02.json"]);
    }

    #[test]
    fn zipping_is_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        sample_folder(&src);
        let entries = list_folder(&src).unwrap();
        let a = dir.path().join("a.zip");
        let b = dir.path().join("b.zip");
        zip_folder(&entries, "Game.exe", &a).unwrap();

        // Different modification times must not change the archive.
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(86_400);
        File::options().write(true).open(src.join("README.txt")).unwrap().set_modified(later).unwrap();
        let entries = list_folder(&src).unwrap();
        zip_folder(&entries, "Game.exe", &b).unwrap();

        let (a_bytes, b_bytes) = (fs::read(&a).unwrap(), fs::read(&b).unwrap());
        assert_eq!(a_bytes, b_bytes);
        assert_eq!(sha256_file(&a).unwrap(), sha256_file(&b).unwrap());
        assert!(!dir.path().join("a.zip.part").exists());

        // And it is a zip with exactly those entries, in order, readable back.
        let names = zip_file_names(&a).unwrap();
        assert_eq!(names, ["Game.exe", "README.txt", "data/a-b.txt", "data/levels/01.json", "data/levels/02.json"]);
        let mut archive = ZipArchive::new(File::open(&a).unwrap()).unwrap();
        let mut level = String::new();
        archive.by_name("data/levels/02.json").unwrap().read_to_string(&mut level).unwrap();
        assert_eq!(level, r#"{"level":2}"#);
        let exe = archive.by_name("Game.exe").unwrap();
        assert_eq!(exe.unix_mode().map(|m| m & 0o777), Some(0o755));
        assert_eq!(exe.last_modified(), Some(DateTime::DEFAULT));
        drop(exe);
        assert_eq!(archive.by_name("README.txt").unwrap().unix_mode().map(|m| m & 0o777), Some(0o644));
    }

    #[test]
    fn content_changes_change_the_hash() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        sample_folder(&src);
        let a = dir.path().join("a.zip");
        zip_folder(&list_folder(&src).unwrap(), "Game.exe", &a).unwrap();
        fs::write(src.join("README.txt"), b"hello!").unwrap();
        let b = dir.path().join("b.zip");
        zip_folder(&list_folder(&src).unwrap(), "Game.exe", &b).unwrap();
        assert_ne!(sha256_file(&a).unwrap().1, sha256_file(&b).unwrap().1);
    }

    #[test]
    fn unsafe_zip_entries_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        for (i, bad) in ["../evil.exe", "bin/../../evil", "bin\\game.exe", "/abs/game", "C:/game.exe"].iter().enumerate() {
            let p = dir.path().join(format!("bad{i}.zip"));
            let mut zip = ZipWriter::new(File::create(&p).unwrap());
            zip.start_file(*bad, SimpleFileOptions::DEFAULT).unwrap();
            zip.write_all(b"x").unwrap();
            zip.finish().unwrap();
            assert!(matches!(zip_file_names(&p), Err(Error::Invalid(_))), "{bad} should be refused");
        }
        let junk = dir.path().join("junk.zip");
        fs::write(&junk, b"not a zip").unwrap();
        assert!(matches!(zip_file_names(&junk), Err(Error::Invalid(_))));
    }
}
