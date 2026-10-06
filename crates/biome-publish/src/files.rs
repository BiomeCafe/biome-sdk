//! Per-file clients (docs/GAME_PUBLISHING_SPEC.md §10): a client folder
//! sent as its file list, each distinct file an object the game stores once,
//! gzip-compressed.
//!
//! - [`file_list`] hashes a folder's files and checks them against the
//!   server's rules ([`path_problem`], at most [`MAX_FILES`] files and 4 GiB
//!   per platform, no two names Windows or macOS would merge, no name that
//!   is both a file and a folder, the entrypoint among them).
//! - [`build_id`] is the server's id for a file list: the sha256 of one line
//!   `<sha256> <size> <x|-> <path>\n` per file, sorted by path (bytes).
//! - [`gzip_file`] writes an object's stored form: a gzip with no name and
//!   no timestamp (header `1f 8b 08 00 00000000 00 ff`), deflate level 6, so
//!   the same file always compresses to the same bytes. The deflate stream
//!   comes from the `zip` crate (already a dependency): the file is written
//!   as a one-entry zip and its raw stream is wrapped in a gzip header and
//!   trailer. [`gunzip`] reads one back the same way round.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, DateTime, ZipArchive, ZipWriter};

use crate::error::Error;
use crate::pack::{self, FolderEntry, MAX_CLIENT_BYTES};

/// Files in one platform's per-file client.
pub const MAX_FILES: usize = 20_000;

/// Longest path, in characters.
pub const MAX_PATH_CHARS: usize = 260;

/// One file of a per-file client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentFile {
    /// Relative, `/`-separated.
    pub path: String,
    pub source: PathBuf,
    pub size: u64,
    pub sha256: String,
    pub executable: bool,
}

/// A per-file client's files, sorted by path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileList {
    pub files: Vec<ContentFile>,
    pub total: u64,
    pub build_id: String,
}

impl FileList {
    /// Distinct contents: sha256 -> (size, the first file that has it).
    pub fn objects(&self) -> BTreeMap<String, (u64, &ContentFile)> {
        let mut out = BTreeMap::new();
        for f in &self.files {
            out.entry(f.sha256.clone()).or_insert((f.size, f));
        }
        out
    }
}

/// Why `path` can't be a file's path in a per-file client, or `None`. The
/// same rules as the server (`game_content_path_ok`): relative, `/`
/// separated, no empty, `.` or `..` segments, no control characters, `\`,
/// `: * ? " < > |`, no segment ending in a dot or a space, no Windows device
/// name (CON, PRN, AUX, NUL, COM1-9, LPT1-9, with or without an extension),
/// at most 260 characters.
pub fn path_problem(path: &str) -> Option<String> {
    if path.is_empty() {
        return Some("an empty path".into());
    }
    if path.chars().count() > MAX_PATH_CHARS {
        return Some(format!("{path}: longer than {MAX_PATH_CHARS} characters"));
    }
    if path.chars().any(char::is_control) {
        return Some(format!("{path:?}: control characters"));
    }
    if let Some(c) = path.chars().find(|c| matches!(c, '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')) {
        return Some(format!("{path}: {c:?} can't be in a file name on Windows"));
    }
    if path.starts_with('/') {
        return Some(format!("{path}: must be relative"));
    }
    for seg in path.split('/') {
        if seg.is_empty() {
            return Some(format!("{path}: an empty folder name"));
        }
        if seg == "." || seg == ".." {
            return Some(format!("{path}: no . or .. in a path"));
        }
        if seg.ends_with('.') || seg.ends_with(' ') {
            return Some(format!("{path}: {seg:?} ends in a dot or a space, which Windows drops"));
        }
        if is_device_name(seg) {
            return Some(format!("{path}: {seg} is a reserved name on Windows"));
        }
    }
    None
}

fn is_device_name(seg: &str) -> bool {
    let stem = seg.split('.').next().unwrap_or(seg).to_ascii_lowercase();
    matches!(stem.as_str(), "con" | "prn" | "aux" | "nul")
        || ((stem.starts_with("com") || stem.starts_with("lpt"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit()
            && stem.as_bytes()[3] != b'0')
}

/// Hash every file of a client folder (from [`pack::list_folder`]) and check
/// the list. `executable`: a Unix executable bit, or the entrypoint.
pub fn file_list(entries: &[FolderEntry], entrypoint: &str, table: &str) -> Result<FileList, Error> {
    let mut problems = Vec::new();
    if entries.len() > MAX_FILES {
        problems.push(format!("{table}: {} files; a client sent file by file has at most {MAX_FILES} (zip it instead: delivery = \"zip\")", entries.len()));
    }
    let mut lower: HashMap<String, &str> = HashMap::new();
    for e in entries {
        if let Some(why) = path_problem(&e.name) {
            problems.push(format!("{table}: {why}"));
        }
        if let Some(other) = lower.insert(e.name.to_lowercase(), &e.name) {
            problems.push(format!("{table}: {} and {other} would be one file on Windows and macOS", e.name));
        }
    }
    for e in entries {
        let low = e.name.to_lowercase();
        let parts: Vec<&str> = low.split('/').collect();
        for i in 1..parts.len() {
            let dir = parts[..i].join("/");
            if let Some(file) = lower.get(&dir) {
                problems.push(format!("{table}: {file} is both a file and a folder"));
            }
        }
    }
    if !entries.iter().any(|e| e.name == entrypoint) {
        problems.push(format!("{table}: the entrypoint {entrypoint:?} is not in the folder"));
    }
    let total: u64 = entries.iter().map(|e| e.size).sum();
    if total > MAX_CLIENT_BYTES {
        problems.push(format!("{table}: {total} bytes; a client is at most {MAX_CLIENT_BYTES} (4 GiB)"));
    }
    if total == 0 && !entries.is_empty() {
        problems.push(format!("{table}: every file is empty"));
    }
    if !problems.is_empty() {
        problems.dedup();
        return Err(Error::invalid(problems.join("\n")));
    }

    let hashes = hash_all(entries)?;
    let mut files: Vec<ContentFile> = entries
        .iter()
        .zip(hashes)
        .map(|(e, (size, sha256))| ContentFile {
            path: e.name.clone(),
            source: e.source.clone(),
            size,
            sha256,
            executable: e.executable || e.name == entrypoint,
        })
        .collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let total = files.iter().map(|f| f.size).sum();
    let build_id = build_id(&files);
    Ok(FileList { files, total, build_id })
}

/// sha256 of each file, on a few threads (a big build is mostly hashing).
fn hash_all(entries: &[FolderEntry]) -> Result<Vec<(u64, String)>, Error> {
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 8);
    let chunk = entries.len().div_ceil(threads).max(1);
    let mut out = Vec::with_capacity(entries.len());
    let results: Vec<Result<Vec<(u64, String)>, Error>> = std::thread::scope(|s| {
        let handles: Vec<_> = entries
            .chunks(chunk)
            .map(|part| {
                s.spawn(move || {
                    part.iter()
                        .map(|e| pack::sha256_file(&e.source).map_err(|err| Error::io(format!("cannot read {}: {err}", e.source.display()))))
                        .collect::<Result<Vec<_>, Error>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|_| Err(Error::io("a hashing thread stopped"))))
            .collect()
    });
    for r in results {
        out.extend(r?);
    }
    Ok(out)
}

/// The server's id for a file list: sha256 of `<sha256> <size> <x|-> <path>\n`
/// per file, sorted by path (byte order, as Rust sorts strings).
pub fn build_id(files: &[ContentFile]) -> String {
    let mut sorted: Vec<&ContentFile> = files.iter().collect();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));
    let mut h = Sha256::new();
    for f in sorted {
        h.update(format!("{} {} {} {}\n", f.sha256, f.size, if f.executable { "x" } else { "-" }, f.path).as_bytes());
    }
    pack::hex(&h.finalize())
}

const GZIP_HEADER: [u8; 10] = [0x1f, 0x8b, 0x08, 0x00, 0, 0, 0, 0, 0x00, 0xff];

/// What [`gzip_file`] wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gzipped {
    /// The gzip: what is uploaded.
    pub stored_size: u64,
    pub stored_sha256: String,
    /// The file as it was read while compressing.
    pub size: u64,
    pub sha256: String,
}

/// Write `src`'s stored form to `dest`: a deterministic gzip (see the module
/// docs), also hashing the file as it is read (so a file that changed since
/// it was listed is caught). Written to `<dest>.part` and renamed; a scratch
/// zip `<dest>.zip.tmp` is removed afterwards.
pub fn gzip_file(src: &Path, dest: &Path) -> Result<Gzipped, Error> {
    let werr = |p: &Path, e: &dyn std::fmt::Display| Error::io(format!("cannot write {}: {e}", p.display()));
    let scratch = dest.with_extension("zip.tmp");
    let part = dest.with_extension("part");
    let size = fs::metadata(src).map_err(|e| Error::io(format!("cannot read {}: {e}", src.display())))?.len();
    let mut raw = Sha256::new();
    let mut raw_size = 0u64;

    // 1. A one-entry zip: the deflate stream we want, plus zip framing.
    {
        let file = File::create(&scratch).map_err(|e| werr(&scratch, &e))?;
        let mut zip = ZipWriter::new(BufWriter::with_capacity(1 << 20, file));
        let options = SimpleFileOptions::DEFAULT
            .compression_method(CompressionMethod::Deflated)
            .compression_level(Some(6))
            .last_modified_time(DateTime::DEFAULT)
            .large_file(size >= u64::from(u32::MAX));
        zip.start_file("x", options).map_err(|e| werr(&scratch, &e))?;
        let mut input = File::open(src).map_err(|e| Error::io(format!("cannot read {}: {e}", src.display())))?;
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = input.read(&mut buf).map_err(|e| Error::io(format!("cannot read {}: {e}", src.display())))?;
            if n == 0 {
                break;
            }
            raw.update(&buf[..n]);
            raw_size += n as u64;
            zip.write_all(&buf[..n]).map_err(|e| Error::io(format!("cannot compress {}: {e}", src.display())))?;
        }
        let w = zip.finish().map_err(|e| werr(&scratch, &e))?;
        let mut f = w.into_inner().map_err(|e| werr(&scratch, e.error()))?;
        f.flush().map_err(|e| werr(&scratch, &e))?;
    }

    // 2. Its raw stream between a gzip header and trailer.
    let result = (|| -> Result<(u64, String), Error> {
        let file = File::open(&scratch).map_err(|e| werr(&scratch, &e))?;
        let mut archive = ZipArchive::new(BufReader::new(file)).map_err(|e| werr(&scratch, &e))?;
        let mut entry = archive.by_index_raw(0).map_err(|e| werr(&scratch, &e))?;
        let crc = entry.crc32();
        let isize = (entry.size() & 0xffff_ffff) as u32;
        let out = File::create(&part).map_err(|e| werr(&part, &e))?;
        let mut out = HashingWriter { inner: BufWriter::with_capacity(1 << 20, out), hasher: Sha256::new(), written: 0 };
        out.write_all(&GZIP_HEADER).map_err(|e| werr(&part, &e))?;
        io::copy(&mut entry, &mut out).map_err(|e| werr(&part, &e))?;
        out.write_all(&crc.to_le_bytes()).map_err(|e| werr(&part, &e))?;
        out.write_all(&isize.to_le_bytes()).map_err(|e| werr(&part, &e))?;
        out.inner.flush().map_err(|e| werr(&part, &e))?;
        Ok((out.written, pack::hex(&out.hasher.finalize())))
    })();
    let _ = fs::remove_file(&scratch);
    let (stored_size, stored_sha256) = result?;
    fs::rename(&part, dest).map_err(|e| Error::io(format!("cannot rename {} to {}: {e}", part.display(), dest.display())))?;
    Ok(Gzipped { stored_size, stored_sha256, size: raw_size, sha256: pack::hex(&raw.finalize()) })
}

struct HashingWriter<W> {
    inner: W,
    hasher: Sha256,
    written: u64,
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Unpack a gzip made by [`gzip_file`] (or any single-member gzip) from
/// `reader` (seekable, `len` bytes) into `out`. The deflate stream is read
/// with the `zip` crate, which also checks the trailer's CRC-32.
pub fn gunzip<R: Read + Seek>(reader: &mut R, len: u64, out: &mut dyn Write) -> io::Result<u64> {
    let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, format!("not a gzip: {m}"));
    let mut head = [0u8; 10];
    reader.read_exact(&mut head)?;
    if head[0] != 0x1f || head[1] != 0x8b || head[2] != 8 {
        return Err(bad("wrong magic"));
    }
    let flags = head[3];
    let mut start = 10u64;
    if flags & 0x04 != 0 {
        let mut x = [0u8; 2];
        reader.read_exact(&mut x)?;
        let n = u16::from_le_bytes(x) as u64;
        reader.seek(SeekFrom::Current(n as i64))?;
        start += 2 + n;
    }
    for bit in [0x08u8, 0x10] {
        if flags & bit != 0 {
            let mut b = [0u8; 1];
            loop {
                reader.read_exact(&mut b)?;
                start += 1;
                if b[0] == 0 {
                    break;
                }
            }
        }
    }
    if flags & 0x02 != 0 {
        reader.seek(SeekFrom::Current(2))?;
        start += 2;
    }
    if len < start + 8 {
        return Err(bad("too short"));
    }
    reader.seek(SeekFrom::Start(len - 8))?;
    let mut trailer = [0u8; 8];
    reader.read_exact(&mut trailer)?;
    let crc = u32::from_le_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]);
    let isize = u32::from_le_bytes([trailer[4], trailer[5], trailer[6], trailer[7]]);
    let compressed = len - start - 8;
    reader.seek(SeekFrom::Start(start))?;

    // A zip local header in front of the deflate stream (sizes are given to
    // the reader directly; the CRC is checked when the stream ends).
    let mut local = Vec::with_capacity(30);
    local.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
    local.extend_from_slice(&20u16.to_le_bytes()); // version needed
    local.extend_from_slice(&0u16.to_le_bytes()); // flags
    local.extend_from_slice(&8u16.to_le_bytes()); // deflate
    local.extend_from_slice(&0u16.to_le_bytes()); // time
    local.extend_from_slice(&0x21u16.to_le_bytes()); // date: 1980-01-01
    local.extend_from_slice(&crc.to_le_bytes());
    local.extend_from_slice(&(compressed.min(u64::from(u32::MAX)) as u32).to_le_bytes());
    local.extend_from_slice(&isize.to_le_bytes());
    local.extend_from_slice(&0u16.to_le_bytes()); // name length
    local.extend_from_slice(&0u16.to_le_bytes()); // extra length
    let mut chained = io::Cursor::new(local).chain(reader.take(compressed));
    let mut entry = zip::read::read_zipfile_from_stream_with_compressed_size(&mut chained, compressed)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?
        .ok_or_else(|| bad("no stream"))?;
    let n = io::copy(&mut entry, out)?;
    if (n & 0xffff_ffff) as u32 != isize {
        return Err(bad("the size in its trailer doesn't match"));
    }
    Ok(n)
}

/// [`gunzip`] for bytes in memory.
pub fn gunzip_bytes(gz: &[u8]) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    gunzip(&mut io::Cursor::new(gz), gz.len() as u64, &mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, bytes: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn paths_follow_the_server_rules() {
        for ok in ["Game.exe", "data/a.pak", "Game.app/Contents/MacOS/Game", ".hidden", "a b/c d.txt", "com0.dll", "conf/console.ini", "ñandú/日本.png"] {
            assert_eq!(path_problem(ok), None, "{ok}");
        }
        for bad in [
            "", "../evil.exe", "bin/../../x", "/abs/x", "bin\\x.exe", "C:/x.exe", "a//b", "dir/", "./x", "x/.", "trail.", "trail /x",
            "con.txt", "bin/NUL", "LPT1", "COM9.dll", "a:b", "what?", "tab\there",
        ] {
            assert!(path_problem(bad).is_some(), "{bad:?} should be refused");
        }
        assert!(path_problem(&"x".repeat(261)).is_some());
        assert_eq!(path_problem(&"é".repeat(260)), None, "characters, not bytes");
    }

    #[test]
    fn a_folder_becomes_a_sorted_hashed_list_with_its_build_id() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("dist");
        write(&src.join("Game.exe"), b"MZ program");
        write(&src.join("data/a.pak"), &[1u8; 5000]);
        write(&src.join("data/copy.pak"), &[1u8; 5000]);
        write(&src.join("README.txt"), b"hi");
        let entries = pack::list_folder(&src).unwrap();
        let list = file_list(&entries, "Game.exe", "[client.windows-x64]").unwrap();
        let paths: Vec<_> = list.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["Game.exe", "README.txt", "data/a.pak", "data/copy.pak"]);
        assert_eq!(list.total, 10 + 5000 + 5000 + 2);
        assert!(list.files[0].executable, "the entrypoint is executable");
        assert!(!list.files[1].executable);
        assert_eq!(list.files[2].sha256, pack::sha256_hex(&[1u8; 5000]));
        // Two files, one content: one object.
        assert_eq!(list.objects().len(), 3);
        // The build id, spelled out.
        let lines: String = list
            .files
            .iter()
            .map(|f| format!("{} {} {} {}\n", f.sha256, f.size, if f.executable { "x" } else { "-" }, f.path))
            .collect();
        assert_eq!(list.build_id, pack::sha256_hex(lines.as_bytes()));
    }

    #[test]
    fn the_build_id_matches_the_database_formula() {
        // The same files as supabase/tests/security/14_per_file_delivery.sql
        // ("each client is one row"), whose expectation is computed there
        // with the same formula in SQL.
        let f = |path: &str, sha: &str, size: u64, x: bool| ContentFile { path: path.into(), source: PathBuf::new(), size, sha256: sha.into(), executable: x };
        let a = "a".repeat(64);
        let id = build_id(&[f("data/b.pak", &a, 7, false), f("Game.exe", &a, 10, true)]);
        let want = pack::sha256_hex(format!("{a} 10 x Game.exe\n{a} 7 - data/b.pak\n").as_bytes());
        assert_eq!(id, want, "sorted by path, bytewise (upper case first)");
    }

    #[test]
    fn bad_folders_are_refused_with_every_reason() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("dist");
        write(&src.join("Game.exe"), b"MZ");
        // Names a Linux or macOS build folder can hold (Windows itself
        // won't create them), so listed by hand.
        let entry = |name: &str| FolderEntry { name: name.into(), source: src.join("Game.exe"), size: 2, executable: false };
        let entries = vec![entry("Game.exe"), entry("con.txt"), entry("trail."), entry("what?")];
        match file_list(&entries, "Other.exe", "[client.linux-x64]") {
            Err(Error::Invalid(msg)) => {
                assert!(msg.contains("con.txt is a reserved name"), "{msg}");
                assert!(msg.contains("ends in a dot"), "{msg}");
                assert!(msg.contains("'?' can't be in a file name"), "{msg}");
                assert!(msg.contains("entrypoint \"Other.exe\""), "{msg}");
            }
            other => panic!("{other:?}"),
        }
        let entries = vec![
            FolderEntry { name: "Data/A.pak".into(), source: src.join("Game.exe"), size: 2, executable: false },
            FolderEntry { name: "data/a.pak".into(), source: src.join("Game.exe"), size: 2, executable: false },
            FolderEntry { name: "data".into(), source: src.join("Game.exe"), size: 2, executable: false },
        ];
        match file_list(&entries, "data", "[client.linux-x64]") {
            Err(Error::Invalid(msg)) => {
                assert!(msg.contains("would be one file"), "{msg}");
                assert!(msg.contains("both a file and a folder"), "{msg}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn gzip_is_deterministic_and_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.bin");
        let bytes: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).chain(b"tail".iter().copied()).collect();
        fs::write(&src, &bytes).unwrap();
        let a = gzip_file(&src, &dir.path().join("a.gz")).unwrap();
        let b = gzip_file(&src, &dir.path().join("b.gz")).unwrap();
        assert_eq!(a, b, "same file, same bytes");
        assert_eq!((a.size, a.sha256.as_str()), (bytes.len() as u64, pack::sha256_hex(&bytes).as_str()), "the file as read");
        let gz = fs::read(dir.path().join("a.gz")).unwrap();
        assert_eq!(gz.len() as u64, a.stored_size);
        assert_eq!(pack::sha256_hex(&gz), a.stored_sha256);
        assert_eq!(&gz[..10], &GZIP_HEADER, "no name, no timestamp");
        assert!(gz.len() < bytes.len() / 10, "it compresses");
        assert_eq!(gunzip_bytes(&gz).unwrap(), bytes);
        assert!(!dir.path().join("a.zip.tmp").exists() && !dir.path().join("a.part").exists());

        // Empty files and incompressible ones too.
        let empty = dir.path().join("empty");
        fs::write(&empty, b"").unwrap();
        gzip_file(&empty, &dir.path().join("e.gz")).unwrap();
        assert_eq!(gunzip_bytes(&fs::read(dir.path().join("e.gz")).unwrap()).unwrap(), b"");
        let noise: Vec<u8> = (0..70_000u64).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        let n = dir.path().join("noise");
        fs::write(&n, &noise).unwrap();
        let stored = gzip_file(&n, &dir.path().join("n.gz")).unwrap().stored_size;
        assert!(stored <= noise.len() as u64 + noise.len() as u64 / 1024 + 1024, "within the server's bound");
        assert_eq!(gunzip_bytes(&fs::read(dir.path().join("n.gz")).unwrap()).unwrap(), noise);
    }

    #[test]
    fn a_damaged_gzip_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.bin");
        fs::write(&src, b"hello hello hello hello").unwrap();
        gzip_file(&src, &dir.path().join("a.gz")).unwrap();
        let gz = fs::read(dir.path().join("a.gz")).unwrap();
        let mut crc = gz.clone();
        let at = crc.len() - 8;
        crc[at] ^= 1;
        assert!(gunzip_bytes(&crc).is_err(), "a wrong CRC");
        let mut len = gz.clone();
        let at = len.len() - 4;
        len[at] ^= 1;
        assert!(gunzip_bytes(&len).is_err(), "a wrong size");
        assert!(gunzip_bytes(b"not a gzip at all").is_err());
        assert!(gunzip_bytes(&gz[..12]).is_err());
    }
}
