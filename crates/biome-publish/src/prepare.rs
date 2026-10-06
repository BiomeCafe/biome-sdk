//! `check`: turn a manifest into the exact files and metadata to upload,
//! without touching the network.

use std::collections::HashSet;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use biome_rules::Meta;

use crate::api::{ClientMeta, ContentMeta, CreateRelease, DownloadMeta, FilesClientMeta, RulesMeta, ZipClientMeta};
use crate::error::Error;
use crate::files::{self, FileList};
use crate::manifest::{
    is_download_name, is_semver, suggest_download_name, Channel, ClientSpec, Delivery, DownloadSpec, Kind, Manifest,
    DOWNLOAD_NAME_RULE, MAX_NOTES_CHARS,
};
use crate::pack::{self, MAX_CLIENT_BYTES};

/// Largest rules module the server (and the match host) accepts.
pub const MAX_RULES_BYTES: u64 = biome_rules_host::MAX_MODULE_BYTES as u64;

/// Largest download the server accepts (the same cap as a client build).
pub const MAX_DOWNLOAD_BYTES: u64 = MAX_CLIENT_BYTES;

/// Everything `upload` sends, ready to go.
#[derive(Debug, Clone)]
pub struct Prepared {
    pub kind: Kind,
    pub app_id: String,
    pub version: String,
    pub channel: Channel,
    pub notes: String,
    pub rules: Option<PreparedRules>,
    pub clients: Vec<PreparedClient>,
    pub downloads: Vec<PreparedDownload>,
}

/// One file of a software release, ready to upload.
#[derive(Debug, Clone)]
pub struct PreparedDownload {
    pub platform: String,
    pub label: Option<String>,
    /// The file or folder named in the manifest.
    pub source: PathBuf,
    /// What is uploaded: the given file, or the zip the CLI built.
    pub file: PathBuf,
    /// The name it is published under.
    pub file_name: String,
    pub size: u64,
    pub sha256: String,
    /// Number of files zipped; `None` when a file was given.
    pub zipped_files: Option<usize>,
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
    pub entrypoint: String,
    /// How it is sent.
    pub build: ClientBuild,
}

/// A client sent whole (a zip) or file by file (spec §10).
#[derive(Debug, Clone)]
pub enum ClientBuild {
    Zip(ZipBuild),
    Files(FileList),
}

#[derive(Debug, Clone)]
pub struct ZipBuild {
    /// The archive to upload: the zip the CLI built, or the given zip.
    pub archive: PathBuf,
    /// `<app_id>-<version>-<platform>.zip`.
    pub file_name: String,
    pub size: u64,
    pub sha256: String,
    /// Number of files zipped; `None` when a ready-made zip was given.
    pub zipped_files: Option<usize>,
}

impl PreparedClient {
    /// The zip, for a client sent whole.
    pub fn zip(&self) -> Option<&ZipBuild> {
        match &self.build {
            ClientBuild::Zip(z) => Some(z),
            ClientBuild::Files(_) => None,
        }
    }

    /// The file list, for a client sent file by file.
    pub fn files(&self) -> Option<&FileList> {
        match &self.build {
            ClientBuild::Files(f) => Some(f),
            ClientBuild::Zip(_) => None,
        }
    }
}

impl Prepared {
    /// The distinct objects of the per-file clients: sha256 -> (size, the
    /// first file that holds it, by platform then path).
    pub fn objects(&self) -> std::collections::BTreeMap<String, (u64, &files::ContentFile)> {
        let mut out = std::collections::BTreeMap::new();
        for c in &self.clients {
            if let Some(list) = c.files() {
                for (sha, v) in list.objects() {
                    out.entry(sha).or_insert(v);
                }
            }
        }
        out
    }

    /// The `POST /releases` body: `clients` (+ `rules`) for a game,
    /// `downloads` for software. A game whose clients are zips sends exactly
    /// the body it did before software products and per-file delivery
    /// existed; a per-file client sends its `contents`.
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
                .map(|c| match &c.build {
                    ClientBuild::Zip(z) => ClientMeta::Zip(ZipClientMeta {
                        platform: c.platform.clone(),
                        file_name: z.file_name.clone(),
                        size: z.size,
                        sha256: z.sha256.clone(),
                        entrypoint: c.entrypoint.clone(),
                    }),
                    ClientBuild::Files(list) => ClientMeta::Files(FilesClientMeta {
                        platform: c.platform.clone(),
                        entrypoint: c.entrypoint.clone(),
                        contents: list
                            .files
                            .iter()
                            .map(|f| ContentMeta { path: f.path.clone(), size: f.size, sha256: f.sha256.clone(), executable: f.executable })
                            .collect(),
                    }),
                })
                .collect(),
            downloads: self
                .downloads
                .iter()
                .map(|d| DownloadMeta {
                    platform: d.platform.clone(),
                    file_name: d.file_name.clone(),
                    size: d.size,
                    sha256: d.sha256.clone(),
                    label: d.label.clone(),
                })
                .collect(),
            objects: Vec::new(),
            uploaded: Vec::new(),
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
    let mut downloads = Vec::with_capacity(m.downloads.len());
    let mut names = HashSet::new();
    for spec in &m.downloads {
        let d = build_download(spec, out_dir, log)?;
        // Storage keys are case-sensitive; the Windows folder it lands in is not.
        if !names.insert(d.file_name.to_ascii_lowercase()) {
            return Err(Error::invalid(format!(
                "{}: another download is already published as {}; set file_name = \"...\" on one of them",
                spec.table(),
                d.file_name
            )));
        }
        downloads.push(d);
    }
    Ok(Prepared {
        kind: m.kind,
        app_id: m.app_id.clone(),
        version: m.version.clone(),
        channel: m.channel,
        notes,
        rules,
        clients,
        downloads,
    })
}

/// A download: a file is uploaded as it is (a `.zip` must unpack safely); a
/// folder is zipped deterministically, its contents at the zip's root, to
/// `<folder>.zip` unless `file_name` says otherwise.
pub fn build_download(spec: &DownloadSpec, out_dir: &Path, log: &mut dyn Write) -> Result<PreparedDownload, Error> {
    let table = spec.table();
    let meta = fs::metadata(&spec.path).map_err(|e| missing_or_io(&format!("{table} path"), &spec.path, e))?;
    let own_name = spec.path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();

    let (file, file_name, zipped_files) = if meta.is_dir() {
        let file_name = match &spec.file_name {
            Some(n) if !n.to_ascii_lowercase().ends_with(".zip") => {
                return Err(Error::invalid(format!("{table}: a folder is published as a zip, so file_name must end in .zip (got {n:?})")));
            }
            Some(n) => n.clone(),
            None => publishable_name(&table, &format!("{own_name}.zip"))?,
        };
        let entries = pack::list_folder(&spec.path)?;
        if entries.is_empty() {
            return Err(Error::invalid(format!("{table} path: {} is an empty folder", spec.path.display())));
        }
        fs::create_dir_all(out_dir).map_err(|e| Error::io(format!("cannot create {}: {e}", out_dir.display())))?;
        refuse_out_dir_inside(&table, &spec.path, out_dir)?;
        let dest = out_dir.join(&file_name);
        let _ = writeln!(log, "zipping download {file_name} ({} files) ...", entries.len());
        // No entrypoint: executable bits come from the files themselves.
        pack::zip_folder(&entries, "", &dest)?;
        (dest, file_name, Some(entries.len()))
    } else if meta.is_file() {
        let file_name = match &spec.file_name {
            Some(n) => n.clone(),
            None => publishable_name(&table, &own_name)?,
        };
        let is_zip = spec.path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("zip"));
        if is_zip {
            // Same rule as client archives: nothing that unpacks outside its folder.
            pack::zip_file_names(&spec.path)?;
        }
        (spec.path.clone(), file_name, None)
    } else {
        return Err(Error::invalid(format!("{table} path: {} is neither a file nor a folder", spec.path.display())));
    };

    let (size, sha256) = pack::sha256_file(&file).map_err(|e| Error::io(format!("cannot read {}: {e}", file.display())))?;
    if size == 0 {
        return Err(Error::invalid(format!("{table}: {} is empty", file.display())));
    }
    if size > MAX_DOWNLOAD_BYTES {
        return Err(Error::invalid(format!("{table}: {} is {size} bytes; the limit is {MAX_DOWNLOAD_BYTES} (4 GiB)", file.display())));
    }
    Ok(PreparedDownload {
        platform: spec.platform.clone(),
        label: spec.label.clone(),
        source: spec.path.clone(),
        file,
        file_name,
        size,
        sha256,
        zipped_files,
    })
}

/// `name` if it can be published as is, else a validation error that
/// suggests a name for `file_name`.
fn publishable_name(table: &str, name: &str) -> Result<String, Error> {
    if is_download_name(name) {
        return Ok(name.to_string());
    }
    let hint = match suggest_download_name(name) {
        Some(s) => format!("; rename it or set file_name = \"{s}\""),
        None => "; rename it or set file_name".to_string(),
    };
    Err(Error::invalid(format!("{table}: the name {name:?} can't be published: it {DOWNLOAD_NAME_RULE}{hint}")))
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

/// A client folder sent file by file (the default): every file hashed and
/// checked, nothing compressed yet (`upload` compresses only what Biome
/// doesn't have). A folder with `delivery = "zip"`, or a ready zip: zipped
/// (or taken as is), the entrypoint checked, hashed.
pub fn build_client(spec: &ClientSpec, app_id: &str, version: &str, out_dir: &Path, log: &mut dyn Write) -> Result<PreparedClient, Error> {
    let table = format!("[client.{}]", spec.platform);
    let meta = fs::metadata(&spec.path).map_err(|e| missing_or_io(&format!("{table} path"), &spec.path, e))?;
    if meta.is_dir() && spec.delivery == Delivery::Files {
        let entries = pack::list_folder(&spec.path)?;
        if entries.is_empty() {
            return Err(Error::invalid(format!("{table} path: {} is an empty folder", spec.path.display())));
        }
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        require_entrypoint(&table, &spec.entrypoint, &spec.path, &names)?;
        let _ = writeln!(log, "hashing client {} ({} files) ...", spec.platform, entries.len());
        let list = files::file_list(&entries, &spec.entrypoint, &table)?;
        return Ok(PreparedClient {
            platform: spec.platform.clone(),
            source: spec.path.clone(),
            entrypoint: spec.entrypoint.clone(),
            build: ClientBuild::Files(list),
        });
    }

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
        entrypoint: spec.entrypoint.clone(),
        build: ClientBuild::Zip(ZipBuild { archive, file_name, size, sha256, zipped_files }),
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
            "{table}: the output folder {} is inside the folder being zipped, {}; pick another --out-dir",
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

    /// A client sent as a zip (a folder with `delivery = "zip"`, or a .zip).
    fn spec(platform: &str, path: PathBuf, entrypoint: &str) -> ClientSpec {
        ClientSpec { platform: platform.into(), path, entrypoint: entrypoint.into(), delivery: Delivery::Zip }
    }

    /// A client folder sent file by file (the default).
    fn files_spec(platform: &str, path: PathBuf, entrypoint: &str) -> ClientSpec {
        ClientSpec { platform: platform.into(), path, entrypoint: entrypoint.into(), delivery: Delivery::Files }
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
        let (a, b) = (a.zip().unwrap(), b.zip().unwrap());
        assert_eq!(a.file_name, "tic-tac-toe-1.0.0-linux-x64.zip");
        assert_eq!((a.size, &a.sha256), (b.size, &b.sha256));
        assert_eq!(fs::read(&a.archive).unwrap(), fs::read(&b.archive).unwrap());
        assert_eq!(a.zipped_files, Some(2));

        // A ready-made zip is used as it is: same bytes, same hash.
        let given = spec("linux-x64", a.archive.clone(), "bin/game");
        let c = build_client(&given, "tic-tac-toe", "1.0.0", &dir.path().join("unused"), &mut log).unwrap();
        let c = c.zip().unwrap();
        assert_eq!(c.archive, a.archive);
        assert_eq!(c.sha256, a.sha256);
        assert_eq!(c.zipped_files, None);
        assert!(!dir.path().join("unused").exists());
    }

    #[test]
    fn a_folder_is_sent_file_by_file_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("dist");
        fs::create_dir_all(src.join("data")).unwrap();
        fs::write(src.join("Game.exe"), b"MZ fake").unwrap();
        fs::write(src.join("data/a.pak"), vec![3u8; 50_000]).unwrap();
        fs::write(src.join("data/b.pak"), vec![3u8; 50_000]).unwrap();
        let out = dir.path().join("out");
        let mut log = Vec::new();
        let c = build_client(&files_spec("windows-x64", src.clone(), "Game.exe"), "abc", "1.0.0", &out, &mut log).unwrap();
        let list = c.files().expect("a file list");
        assert_eq!(list.files.len(), 3);
        assert_eq!(list.total, 7 + 100_000);
        assert!(list.files[0].executable && list.files[0].path == "Game.exe");
        assert!(!out.exists(), "nothing is written before upload");
        assert!(String::from_utf8(log).unwrap().contains("hashing client windows-x64 (3 files)"));
        // The entrypoint check as for zips.
        let r = build_client(&files_spec("windows-x64", src.clone(), "game.exe"), "abc", "1.0.0", &out, &mut Vec::new());
        assert!(matches!(r, Err(Error::Invalid(ref m)) if m.contains("did you mean \"Game.exe\"")), "{r:?}");
        // A ready zip is still sent whole, whatever the spec says.
        let zipped = build_client(&spec("windows-x64", src.clone(), "Game.exe"), "abc", "1.0.0", &out, &mut Vec::new()).unwrap();
        let zip_path = zipped.zip().unwrap().archive.clone();
        let again = build_client(&spec("windows-x64", zip_path, "Game.exe"), "abc", "1.0.0", &out, &mut Vec::new()).unwrap();
        assert!(again.zip().is_some());
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
        match build_client(&spec("windows-x64", ok.zip().unwrap().archive.clone(), "bin/Game.exe"), "abc", "1.0.0", &out, &mut log) {
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
        assert!(matches!(r, Err(Error::Invalid(ref m)) if m.contains("inside the folder being zipped")), "{r:?}");
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

    fn download(path: PathBuf, file_name: Option<&str>) -> DownloadSpec {
        DownloadSpec { index: 1, path, platform: "any".into(), label: Some("Add-on".into()), file_name: file_name.map(String::from) }
    }

    #[test]
    fn a_download_file_is_uploaded_as_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let py = dir.path().join("node_wrangler.py");
        fs::write(&py, b"bl_info = {'name': 'Node Wrangler'}\n").unwrap();
        let mut log = Vec::new();
        let d = build_download(&download(py.clone(), None), &dir.path().join("out"), &mut log).unwrap();
        assert_eq!(d.file, py);
        assert_eq!(d.file_name, "node_wrangler.py");
        assert_eq!((d.size, d.sha256.clone()), pack::sha256_file(&py).unwrap());
        assert_eq!((d.platform.as_str(), d.label.as_deref(), d.zipped_files), ("any", Some("Add-on"), None));
        assert!(!dir.path().join("out").exists(), "nothing built for a file");

        let renamed = build_download(&download(py, Some("nw.py")), &dir.path().join("out"), &mut log).unwrap();
        assert_eq!(renamed.file_name, "nw.py");
    }

    #[test]
    fn a_download_folder_is_zipped_with_its_contents_at_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("node_wrangler");
        fs::create_dir_all(src.join("icons")).unwrap();
        fs::write(src.join("blender_manifest.toml"), b"id = \"node_wrangler\"\n").unwrap();
        fs::write(src.join("__init__.py"), b"def register(): pass\n").unwrap();
        fs::write(src.join("icons/frame.png"), [1u8, 2, 3]).unwrap();
        let mut log = Vec::new();
        let a = build_download(&download(src.clone(), None), &dir.path().join("out1"), &mut log).unwrap();
        let b = build_download(&download(src.clone(), None), &dir.path().join("out2"), &mut log).unwrap();
        assert_eq!(a.file_name, "node_wrangler.zip");
        assert_eq!(a.zipped_files, Some(3));
        assert_eq!((a.size, &a.sha256), (b.size, &b.sha256), "deterministic");
        assert_eq!(pack::zip_file_names(&a.file).unwrap(), ["__init__.py", "blender_manifest.toml", "icons/frame.png"]);

        let named = build_download(&download(src.clone(), Some("nw-1.2.0.zip")), &dir.path().join("out3"), &mut log).unwrap();
        assert_eq!(named.file_name, "nw-1.2.0.zip");
        let r = build_download(&download(src, Some("nw.tar")), &dir.path().join("out4"), &mut log);
        assert!(matches!(r, Err(Error::Invalid(ref m)) if m.contains("must end in .zip")), "{r:?}");
    }

    #[test]
    fn unpublishable_names_get_a_suggestion() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("My Addon (v2).zip");
        let mut zip = zip::ZipWriter::new(fs::File::create(&f).unwrap());
        zip.start_file("my_addon/__init__.py", zip::write::SimpleFileOptions::DEFAULT).unwrap();
        zip.write_all(b"x").unwrap();
        zip.finish().unwrap();
        let mut log = Vec::new();
        let r = build_download(&download(f.clone(), None), &dir.path().join("out"), &mut log);
        assert!(matches!(r, Err(Error::Invalid(ref m)) if m.contains("file_name = \"My_Addon_v2_.zip\"")), "{r:?}");
        let ok = build_download(&download(f, Some("my_addon.zip")), &dir.path().join("out"), &mut log).unwrap();
        assert_eq!(ok.file_name, "my_addon.zip");
    }

    #[test]
    fn bad_downloads_are_validation_errors() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = Vec::new();
        let out = dir.path().join("out");
        let r = build_download(&download(dir.path().join("nope.zip"), None), &out, &mut log);
        assert!(matches!(r, Err(Error::Invalid(ref m)) if m.contains("[[download]] #1 path: nothing at")), "{r:?}");
        let empty = dir.path().join("empty.bin");
        fs::write(&empty, b"").unwrap();
        let r = build_download(&download(empty, None), &out, &mut log);
        assert!(matches!(r, Err(Error::Invalid(ref m)) if m.contains("is empty")), "{r:?}");
        let evil = dir.path().join("evil.zip");
        let mut zip = zip::ZipWriter::new(fs::File::create(&evil).unwrap());
        zip.start_file("../outside.py", zip::write::SimpleFileOptions::DEFAULT).unwrap();
        zip.write_all(b"x").unwrap();
        zip.finish().unwrap();
        let r = build_download(&download(evil, None), &out, &mut log);
        assert!(matches!(r, Err(Error::Invalid(ref m)) if m.contains("outside")), "{r:?}");
    }

    fn manifest(text: &str, dir: &Path) -> Manifest {
        Manifest::parse(text, dir).unwrap()
    }

    #[test]
    fn two_downloads_cannot_share_a_name() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("a")).unwrap();
        fs::create_dir_all(dir.path().join("b")).unwrap();
        fs::write(dir.path().join("a/Addon.py"), b"1").unwrap();
        fs::write(dir.path().join("b/addon.py"), b"2").unwrap();
        let m = manifest(
            "[product]\napp_id = \"abc\"\n[release]\nversion = \"1.0.0\"\n[[download]]\npath = \"a/Addon.py\"\n[[download]]\npath = \"b/addon.py\"\n",
            dir.path(),
        );
        let r = prepare(&m, &dir.path().join("out"), &mut Vec::new());
        assert!(matches!(r, Err(Error::Invalid(ref msg)) if msg.contains("[[download]] #2") && msg.contains("already published")), "{r:?}");
    }

    /// The exact `POST /releases` body for a game sent as a zip: the shape
    /// the server has always taken (no `downloads`, `objects` or
    /// `contents` keys), field for field.
    #[test]
    fn a_zip_game_request_is_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("dist")).unwrap();
        fs::write(dir.path().join("dist/Game.exe"), b"MZ").unwrap();
        let m = manifest(
            "[game]\napp_id = \"abc\"\n[release]\nversion = \"1.0.0\"\n[client.windows-x64]\npath = \"dist\"\nentrypoint = \"Game.exe\"\ndelivery = \"zip\"\n",
            dir.path(),
        );
        let p = prepare(&m, &dir.path().join("out"), &mut Vec::new()).unwrap();
        assert_eq!(p.kind, Kind::Game);
        let c = p.clients[0].zip().unwrap();
        let expected = format!(
            r#"{{"app_id":"abc","version":"1.0.0","channel":"stable","notes":"","clients":[{{"platform":"windows-x64","file_name":"abc-1.0.0-windows-x64.zip","size":{},"sha256":"{}","entrypoint":"Game.exe"}}]}}"#,
            c.size, c.sha256
        );
        assert_eq!(serde_json::to_string(&p.request()).unwrap(), expected);
    }

    /// A folder (by default) sends its file list; two platforms that share a
    /// file share its object.
    #[test]
    fn a_per_file_game_request_lists_the_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("win/data")).unwrap();
        fs::create_dir_all(dir.path().join("linux/data")).unwrap();
        fs::write(dir.path().join("win/Game.exe"), b"MZ").unwrap();
        fs::write(dir.path().join("win/data/a.pak"), b"shared").unwrap();
        fs::write(dir.path().join("linux/game"), b"ELF").unwrap();
        fs::write(dir.path().join("linux/data/a.pak"), b"shared").unwrap();
        let m = manifest(
            "[game]\napp_id = \"abc\"\n[release]\nversion = \"1.0.0\"\n[client.windows-x64]\npath = \"win\"\nentrypoint = \"Game.exe\"\n[client.linux-x64]\npath = \"linux\"\nentrypoint = \"game\"\n",
            dir.path(),
        );
        let p = prepare(&m, &dir.path().join("out"), &mut Vec::new()).unwrap();
        let (mz, elf, shared) = (pack::sha256_hex(b"MZ"), pack::sha256_hex(b"ELF"), pack::sha256_hex(b"shared"));
        let expected = format!(
            r#"{{"app_id":"abc","version":"1.0.0","channel":"stable","notes":"","clients":[{{"platform":"linux-x64","entrypoint":"game","contents":[{{"path":"data/a.pak","size":6,"sha256":"{shared}","executable":false}},{{"path":"game","size":3,"sha256":"{elf}","executable":true}}]}},{{"platform":"windows-x64","entrypoint":"Game.exe","contents":[{{"path":"Game.exe","size":2,"sha256":"{mz}","executable":true}},{{"path":"data/a.pak","size":6,"sha256":"{shared}","executable":false}}]}}]}}"#
        );
        assert_eq!(serde_json::to_string(&p.request()).unwrap(), expected);
        // Three objects for four files; with descriptions, an `objects` key.
        assert_eq!(p.objects().len(), 3);
        let mut req = p.request();
        req.objects.push(crate::api::ObjectMeta { sha256: shared.clone(), stored_size: 26, stored_sha256: "f".repeat(64) });
        req.uploaded.push(shared.clone());
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.ends_with(&format!(r#""objects":[{{"sha256":"{shared}","stored_size":26,"stored_sha256":"{}"}}],"uploaded":["{shared}"]}}"#, "f".repeat(64))), "{json}");
    }

    #[test]
    fn a_software_request_has_downloads_only() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("addon.py"), b"pass\n").unwrap();
        let m = manifest(
            "[product]\napp_id = \"node-wrangler\"\n[release]\nversion = \"1.2.0\"\nchannel = \"beta\"\n[[download]]\npath = \"addon.py\"\nlabel = \"Add-on\"\n[[download]]\npath = \"addon.py\"\nplatform = \"linux-x64\"\nfile_name = \"addon-linux.py\"\n",
            dir.path(),
        );
        let p = prepare(&m, &dir.path().join("out"), &mut Vec::new()).unwrap();
        assert_eq!(p.kind, Kind::Software);
        let sha = pack::sha256_hex(b"pass\n");
        let expected = format!(
            r#"{{"app_id":"node-wrangler","version":"1.2.0","channel":"beta","notes":"","downloads":[{{"platform":"any","file_name":"addon.py","size":5,"sha256":"{sha}","label":"Add-on"}},{{"platform":"linux-x64","file_name":"addon-linux.py","size":5,"sha256":"{sha}"}}]}}"#
        );
        assert_eq!(serde_json::to_string(&p.request()).unwrap(), expected);
    }
}
