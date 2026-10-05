//! `check`: turn a manifest into the exact files and metadata to upload,
//! without touching the network.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use biome_rules::Meta;

use crate::api::{ClientMeta, CreateRelease, RulesMeta};
use crate::error::Error;
use crate::manifest::{is_semver, Channel, ClientSpec, Manifest, MAX_NOTES_CHARS};
use crate::pack::{self, MAX_CLIENT_BYTES};

/// Largest rules module the server (and the match host) accepts.
pub const MAX_RULES_BYTES: u64 = biome_rules_host::MAX_MODULE_BYTES as u64;

/// Everything `upload` sends, ready to go.
#[derive(Debug, Clone)]
pub struct Prepared {
    pub app_id: String,
    pub version: String,
    pub channel: Channel,
    pub notes: String,
    pub rules: Option<PreparedRules>,
    pub clients: Vec<PreparedClient>,
}

#[derive(Debug, Clone)]
pub struct PreparedRules {
    pub path: PathBuf,
    pub size: u64,
    pub sha256: String,
    /// What the module says about itself; `rules_version` is sent.
    pub meta: Meta,
}

#[derive(Debug, Clone)]
pub struct PreparedClient {
    pub platform: String,
    /// The folder or zip named in the manifest.
    pub source: PathBuf,
    /// The archive to upload: the zip the CLI built, or the given zip.
    pub archive: PathBuf,
    /// `<app_id>-<version>-<platform>.zip`.
    pub file_name: String,
    pub size: u64,
    pub sha256: String,
    pub entrypoint: String,
    /// Number of files zipped; `None` when a ready-made zip was given.
    pub zipped_files: Option<usize>,
}

impl Prepared {
    /// The `POST /releases` body.
    pub fn request(&self) -> CreateRelease {
        CreateRelease {
            app_id: self.app_id.clone(),
            version: self.version.clone(),
            channel: self.channel.as_str().to_string(),
            notes: self.notes.clone(),
            rules: self.rules.as_ref().map(|r| RulesMeta {
                rules_version: r.meta.rules_version.clone(),
                size: r.size,
                sha256: r.sha256.clone(),
            }),
            clients: self
                .clients
                .iter()
                .map(|c| ClientMeta {
                    platform: c.platform.clone(),
                    file_name: c.file_name.clone(),
                    size: c.size,
                    sha256: c.sha256.clone(),
                    entrypoint: c.entrypoint.clone(),
                })
                .collect(),
        }
    }
}

/// Where client folders are zipped unless `--out-dir` says otherwise.
pub fn default_out_dir(m: &Manifest) -> PathBuf {
    std::env::temp_dir().join("biome-publish").join(&m.app_id).join(&m.version)
}

/// Read the notes, validate the rules module, build and check every client
/// archive. Cheap checks first, so a bad module fails before gigabytes are
/// zipped. Progress lines go to `log`.
pub fn prepare(m: &Manifest, out_dir: &Path, log: &mut dyn Write) -> Result<Prepared, Error> {
    let notes = match &m.notes {
        Some(path) => read_notes(path)?,
        None => String::new(),
    };
    let rules = match &m.rules {
        Some(path) => Some(check_rules(path)?),
        None => None,
    };
    let mut clients = Vec::with_capacity(m.clients.len());
    for spec in &m.clients {
        clients.push(build_client(spec, &m.app_id, &m.version, out_dir, log)?);
    }
    Ok(Prepared { app_id: m.app_id.clone(), version: m.version.clone(), channel: m.channel, notes, rules, clients })
}

/// The notes file's text, trimmed, at most [`MAX_NOTES_CHARS`] characters.
pub fn read_notes(path: &Path) -> Result<String, Error> {
    let bytes = fs::read(path).map_err(|e| missing_or_io("[release] notes", path, e))?;
    let text = String::from_utf8(bytes).map_err(|_| Error::invalid(format!("[release] notes: {} is not UTF-8 text", path.display())))?;
    let text = text.trim_start_matches('\u{feff}').trim().to_string();
    let chars = text.chars().count();
    if chars > MAX_NOTES_CHARS {
        return Err(Error::invalid(format!("[release] notes: {} is {chars} characters; the limit is {MAX_NOTES_CHARS}", path.display())));
    }
    Ok(text)
}

/// Load the module the way the match host would and return its meta.
pub fn check_rules(path: &Path) -> Result<PreparedRules, Error> {
    let meta = fs::metadata(path).map_err(|e| missing_or_io("[rules] wasm", path, e))?;
    if !meta.is_file() {
        return Err(Error::invalid(format!("[rules] wasm: {} is not a file", path.display())));
    }
    if meta.len() > MAX_RULES_BYTES {
        return Err(Error::invalid(format!(
            "[rules] wasm: {} is {} bytes; the limit is {MAX_RULES_BYTES} (8 MiB)",
            path.display(),
            meta.len()
        )));
    }
    let bytes = fs::read(path).map_err(|e| missing_or_io("[rules] wasm", path, e))?;
    let module = check_rules_bytes(&bytes).map_err(|e| Error::invalid(format!("[rules] {}: {e}", path.display())))?;
    Ok(PreparedRules { path: path.to_path_buf(), size: bytes.len() as u64, sha256: pack::sha256_hex(&bytes), meta: module })
}

/// [`biome_rules_host::validate`] plus the publishing rules on top: the
/// module's `rules_version` is semver (it becomes a file name in the match
/// host's bundle).
pub fn check_rules_bytes(bytes: &[u8]) -> Result<Meta, String> {
    if bytes.len() as u64 > MAX_RULES_BYTES {
        return Err(format!("module is {} bytes; the limit is {MAX_RULES_BYTES} (8 MiB)", bytes.len()));
    }
    let meta = biome_rules_host::validate(bytes).map_err(|e| e.to_string())?;
    if !is_semver(&meta.rules_version) {
        return Err(format!("the module's rules_version {:?} is not semver MAJOR.MINOR.PATCH[-pre]", meta.rules_version));
    }
    Ok(meta)
}

/// Longest archive name the server takes (`game_release_files.file_name`).
pub const MAX_FILE_NAME: usize = 120;

/// Zip a client folder (or take a ready zip), check the entrypoint is in it
/// and hash it.
pub fn build_client(spec: &ClientSpec, app_id: &str, version: &str, out_dir: &Path, log: &mut dyn Write) -> Result<PreparedClient, Error> {
    let table = format!("[client.{}]", spec.platform);
    let meta = fs::metadata(&spec.path).map_err(|e| missing_or_io(&format!("{table} path"), &spec.path, e))?;
    let file_name = format!("{app_id}-{version}-{}.zip", spec.platform);
    // game-publish accepts names of at most 120 characters.
    if file_name.len() > MAX_FILE_NAME {
        return Err(Error::invalid(format!(
            "{table}: the archive name {file_name} is longer than {MAX_FILE_NAME} characters; shorten the version"
        )));
    }

    let (archive, zipped_files) = if meta.is_dir() {
        let entries = pack::list_folder(&spec.path)?;
        if entries.is_empty() {
            return Err(Error::invalid(format!("{table} path: {} is an empty folder", spec.path.display())));
        }
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        require_entrypoint(&table, &spec.entrypoint, &spec.path, &names)?;

        fs::create_dir_all(out_dir).map_err(|e| Error::io(format!("cannot create {}: {e}", out_dir.display())))?;
        refuse_out_dir_inside(&table, &spec.path, out_dir)?;
        let dest = out_dir.join(&file_name);
        let _ = writeln!(log, "zipping client {} ({} files) ...", spec.platform, entries.len());
        pack::zip_folder(&entries, &spec.entrypoint, &dest)?;
        (dest, Some(entries.len()))
    } else if meta.is_file() {
        let is_zip = spec.path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("zip"));
        if !is_zip {
            return Err(Error::invalid(format!("{table} path: {} must be a folder or a .zip", spec.path.display())));
        }
        let names = pack::zip_file_names(&spec.path)?;
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        require_entrypoint(&table, &spec.entrypoint, &spec.path, &names)?;
        (spec.path.clone(), None)
    } else {
        return Err(Error::invalid(format!("{table} path: {} is neither a folder nor a file", spec.path.display())));
    };

    let (size, sha256) = pack::sha256_file(&archive).map_err(|e| Error::io(format!("cannot read {}: {e}", archive.display())))?;
    if size > MAX_CLIENT_BYTES {
        return Err(Error::invalid(format!("{table}: {} is {size} bytes; the limit is {MAX_CLIENT_BYTES} (4 GiB)", archive.display())));
    }
    Ok(PreparedClient {
        platform: spec.platform.clone(),
        source: spec.path.clone(),
        archive,
        file_name,
        size,
        sha256,
        entrypoint: spec.entrypoint.clone(),
        zipped_files,
    })
}

fn require_entrypoint(table: &str, entrypoint: &str, path: &Path, names: &[&str]) -> Result<(), Error> {
    if names.contains(&entrypoint) {
        return Ok(());
    }
    let mut msg = format!("{table} entrypoint {entrypoint:?} is not in {} ({} files)", path.display(), names.len());
    if let Some(close) = names.iter().find(|n| n.eq_ignore_ascii_case(entrypoint)) {
        msg.push_str(&format!("; did you mean {close:?}? (names are case-sensitive once installed)"));
    }
    Err(Error::invalid(msg))
}

/// A zip written inside the folder being zipped would end up in the next
/// run's archive.
fn refuse_out_dir_inside(table: &str, src: &Path, out_dir: &Path) -> Result<(), Error> {
    let (Ok(src), Ok(out)) = (fs::canonicalize(src), fs::canonicalize(out_dir)) else {
        return Ok(());
    };
    if out.starts_with(&src) {
        return Err(Error::invalid(format!(
            "{table}: the output folder {} is inside the client folder {}; pick another --out-dir",
            out_dir.display(),
            src.display()
        )));
    }
    Ok(())
}

fn missing_or_io(what: &str, path: &Path, e: io::Error) -> Error {
    if e.kind() == io::ErrorKind::NotFound {
        Error::invalid(format!("{what}: nothing at {}", path.display()))
    } else {
        Error::io(format!("{what}: cannot read {}: {e}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
    }

    fn demo_rules() -> PathBuf {
        workspace_root().join("cloudflare/biome-match/rules/tictactoe/1.0.0.wasm")
    }

    #[test]
    fn accepts_the_checked_in_demo_module() {
        let r = check_rules(&demo_rules()).unwrap();
        assert_eq!(r.meta.rules_version, "1.0.0");
        assert_eq!(r.meta.abi, 1);
        assert!(r.meta.min_seats >= 1 && r.meta.min_seats <= r.meta.max_seats);
        let bytes = fs::read(demo_rules()).unwrap();
        assert_eq!(r.size, bytes.len() as u64);
        assert_eq!(r.sha256, pack::sha256_hex(&bytes));
    }

    #[test]
    fn rejects_junk_and_oversized_modules() {
        assert!(check_rules_bytes(b"definitely not wasm").is_err());
        assert!(check_rules_bytes(&[]).is_err());
        // A valid wasm header with nothing in it: no exports, so not a rules module.
        assert!(check_rules_bytes(b"\0asm\x01\0\0\0").is_err());

        let dir = tempfile::tempdir().unwrap();
        let junk = dir.path().join("junk.wasm");
        fs::write(&junk, b"junk").unwrap();
        match check_rules(&junk) {
            Err(Error::Invalid(msg)) => assert!(msg.contains("invalid rules module"), "{msg}"),
            other => panic!("{other:?}"),
        }
        let big = dir.path().join("big.wasm");
        fs::write(&big, vec![0u8; MAX_RULES_BYTES as usize + 1]).unwrap();
        match check_rules(&big) {
            Err(Error::Invalid(msg)) => assert!(msg.contains("8 MiB"), "{msg}"),
            other => panic!("{other:?}"),
        }
        match check_rules(&dir.path().join("missing.wasm")) {
            Err(Error::Invalid(msg)) => assert!(msg.contains("nothing at"), "{msg}"),
            other => panic!("{other:?}"),
        }
    }

    fn spec(platform: &str, path: PathBuf, entrypoint: &str) -> ClientSpec {
        ClientSpec { platform: platform.into(), path, entrypoint: entrypoint.into() }
    }

    #[test]
    fn folder_clients_are_zipped_twice_to_the_same_hash() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("dist");
        fs::create_dir_all(src.join("bin")).unwrap();
        fs::write(src.join("bin/game"), b"\x7fELF fake").unwrap();
        fs::write(src.join("assets.pak"), vec![3u8; 50_000]).unwrap();
        let s = spec("linux-x64", src.clone(), "bin/game");
        let mut log = Vec::new();
        let a = build_client(&s, "tic-tac-toe", "1.0.0", &dir.path().join("out1"), &mut log).unwrap();
        let b = build_client(&s, "tic-tac-toe", "1.0.0", &dir.path().join("out2"), &mut log).unwrap();
        assert_eq!(a.file_name, "tic-tac-toe-1.0.0-linux-x64.zip");
        assert_eq!((a.size, &a.sha256), (b.size, &b.sha256));
        assert_eq!(fs::read(&a.archive).unwrap(), fs::read(&b.archive).unwrap());
        assert_eq!(a.zipped_files, Some(2));

        // A ready-made zip is used as it is: same bytes, same hash.
        let given = spec("linux-x64", a.archive.clone(), "bin/game");
        let c = build_client(&given, "tic-tac-toe", "1.0.0", &dir.path().join("unused"), &mut log).unwrap();
        assert_eq!(c.archive, a.archive);
        assert_eq!(c.sha256, a.sha256);
        assert_eq!(c.zipped_files, None);
        assert!(!dir.path().join("unused").exists());
    }

    #[test]
    fn entrypoint_must_be_in_the_archive() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("dist");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("Game.exe"), b"MZ").unwrap();
        let mut log = Vec::new();
        let out = dir.path().join("out");

        match build_client(&spec("windows-x64", src.clone(), "Other.exe"), "abc", "1.0.0", &out, &mut log) {
            Err(Error::Invalid(msg)) => assert!(msg.contains("\"Other.exe\" is not in"), "{msg}"),
            other => panic!("{other:?}"),
        }
        match build_client(&spec("windows-x64", src.clone(), "game.exe"), "abc", "1.0.0", &out, &mut log) {
            Err(Error::Invalid(msg)) => assert!(msg.contains("did you mean \"Game.exe\""), "{msg}"),
            other => panic!("{other:?}"),
        }
        // Same check for a given zip.
        let ok = build_client(&spec("windows-x64", src.clone(), "Game.exe"), "abc", "1.0.0", &out, &mut log).unwrap();
        match build_client(&spec("windows-x64", ok.archive.clone(), "bin/Game.exe"), "abc", "1.0.0", &out, &mut log) {
            Err(Error::Invalid(msg)) => assert!(msg.contains("\"bin/Game.exe\" is not in"), "{msg}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn bad_client_paths_are_validation_errors() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = Vec::new();
        let out = dir.path().join("out");
        let missing = build_client(&spec("linux-x64", dir.path().join("nope"), "g"), "abc", "1.0.0", &out, &mut log);
        assert!(matches!(missing, Err(Error::Invalid(ref m)) if m.contains("nothing at")), "{missing:?}");

        let empty = dir.path().join("empty");
        fs::create_dir_all(&empty).unwrap();
        let r = build_client(&spec("linux-x64", empty, "g"), "abc", "1.0.0", &out, &mut log);
        assert!(matches!(r, Err(Error::Invalid(ref m)) if m.contains("empty folder")), "{r:?}");

        let tarball = dir.path().join("game.tar.gz");
        fs::write(&tarball, b"x").unwrap();
        let r = build_client(&spec("linux-x64", tarball, "g"), "abc", "1.0.0", &out, &mut log);
        assert!(matches!(r, Err(Error::Invalid(ref m)) if m.contains("folder or a .zip")), "{r:?}");

        let src = dir.path().join("dist");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("g"), b"x").unwrap();
        let r = build_client(&spec("linux-x64", src.clone(), "g"), "abc", "1.0.0", &src.join("out"), &mut log);
        assert!(matches!(r, Err(Error::Invalid(ref m)) if m.contains("inside the client folder")), "{r:?}");
    }

    #[test]
    fn notes_are_read_trimmed_and_limited() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("notes.md");
        fs::write(&p, "\u{feff}\n  New cards.\n\n").unwrap();
        assert_eq!(read_notes(&p).unwrap(), "New cards.");
        fs::write(&p, "é".repeat(MAX_NOTES_CHARS)).unwrap();
        assert_eq!(read_notes(&p).unwrap().chars().count(), MAX_NOTES_CHARS);
        fs::write(&p, "x".repeat(MAX_NOTES_CHARS + 1)).unwrap();
        assert!(matches!(read_notes(&p), Err(Error::Invalid(_))));
        fs::write(&p, [0xffu8, 0xfe, 0x00]).unwrap();
        assert!(matches!(read_notes(&p), Err(Error::Invalid(_))));
    }
}
