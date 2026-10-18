//! `biome.toml`: what to publish (spec §7; software: SOFTWARE_PRODUCTS_SPEC.md).
//!
//! A game:
//!
//! ```toml
//! [game]
//! app_id = "mecha-aether"
//!
//! [release]
//! version = "0.3.0"          # required
//! channel = "beta"           # internal | beta | stable (RELEASE), default stable
//! notes = "release-notes.md" # optional: a file whose text becomes the notes
//!
//! [rules]                    # optional
//! wasm = "target/wasm32-unknown-unknown/release/mecha_rules.wasm"
//!
//! [client.windows-x64]       # one table per platform
//! path = "dist/windows"      # a folder (sent file by file) or a .zip (sent whole)
//! entrypoint = "MechaAether.exe"
//! # delivery = "zip"         # optional: send the folder zipped, as before per-file delivery
//!
//! [client.web]               # optional: a build that plays in the browser (biome.cafe/play/<app_id>)
//! path = "dist/web"          # always a folder, sent file by file
//! entrypoint = "index.html"  # the page the browser opens; the default for web
//! ```
//!
//! A software product (an add-on, a plugin, a tool):
//!
//! ```toml
//! [product]
//! app_id = "node-wrangler"
//!
//! [release]
//! version = "1.2.0"
//!
//! [[download]]               # 1 to 10 of them
//! path = "dist/node_wrangler-1.2.0.zip"   # any file, uploaded as is; a folder is zipped
//! platform = "any"           # default any; or windows-x64, macos-arm64, macos-x64, linux-x64
//! label = "Add-on"           # optional, shown next to the file
//! # file_name = "..."        # optional: publish under another name
//! ```
//!
//! Paths are relative to the folder the manifest is in. Parsing checks
//! everything that can be checked without touching the files; whether the
//! files exist and what is in them is [`crate::prepare`]'s job.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::Error;

/// Desktop platforms, in the server's spelling: what a download may target.
pub const PLATFORMS: [&str; 4] = ["windows-x64", "macos-arm64", "macos-x64", "linux-x64"];

/// The platform of a web build: played in the browser at
/// `biome.cafe/play/<app_id>` (spec §11), always sent file by file.
pub const WEB: &str = "web";

/// Platforms a client build may target: the desktops and the web.
pub const CLIENT_PLATFORMS: [&str; 5] = ["windows-x64", "macos-arm64", "macos-x64", "linux-x64", WEB];

/// Release notes limit (characters), as the `game_releases.notes` column.
pub const MAX_NOTES_CHARS: usize = 4000;

/// A software release has at most this many downloads.
pub const MAX_DOWNLOADS: usize = 10;

/// Download label limit (characters), as `game_release_files.label`.
pub const MAX_LABEL_CHARS: usize = 80;

/// What a manifest publishes: `[game]` or `[product]` (software).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Game,
    Software,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Game => "game",
            Kind::Software => "software",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// The release track (docs/RELEASE_TRACKS_SPEC.md): INTERNAL goes live for
/// the game's managers and invited testers once BioShield has checked it;
/// BETA (testers) and RELEASE (everyone, once listed) wait for a site admin.
pub enum Channel {
    Stable,
    Beta,
    Internal,
}

impl Channel {
    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Stable => "stable",
            Channel::Beta => "beta",
            Channel::Internal => "internal",
        }
    }
}

/// A parsed, validated manifest. Paths are already joined to the manifest's
/// folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// The folder the manifest lives in.
    pub dir: PathBuf,
    /// `[game]` or `[product]`.
    pub kind: Kind,
    pub app_id: String,
    pub version: String,
    pub channel: Channel,
    /// File whose text becomes the release notes.
    pub notes: Option<PathBuf>,
    /// Games: the compiled rules module, for games the match host runs.
    pub rules: Option<PathBuf>,
    /// Games: one per platform, sorted by platform name.
    pub clients: Vec<ClientSpec>,
    /// Software: the files buyers download, in manifest order.
    pub downloads: Vec<DownloadSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientSpec {
    pub platform: String,
    /// A folder (sent file by file, or zipped), or a `.zip` to upload as it is.
    pub path: PathBuf,
    /// Path of the executable inside the build, `/`-separated.
    pub entrypoint: String,
    /// How a folder is sent: file by file (the default), or zipped.
    pub delivery: Delivery,
}

/// How a client folder reaches Biome (spec §10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Delivery {
    /// Each distinct file is an object the game stores once; updates fetch
    /// only what changed. The default for a folder.
    #[default]
    Files,
    /// One zip, uploaded and downloaded whole (a `.zip` path always is).
    Zip,
}

/// One `[[download]]` of a software release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadSpec {
    /// 1-based position in the manifest, for messages (`[[download]] #2`).
    pub index: usize,
    /// A file to upload as it is, or a folder to zip.
    pub path: PathBuf,
    /// `any` (the default) or one of [`PLATFORMS`].
    pub platform: String,
    /// Shown next to the file ("Add-on", "Docs"), trimmed.
    pub label: Option<String>,
    /// Publish under this name instead of the file's own (a folder: `<folder>.zip`).
    pub file_name: Option<String>,
}

impl DownloadSpec {
    /// `[[download]] #<n>`, for messages.
    pub fn table(&self) -> String {
        format!("[[download]] #{}", self.index)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    game: Option<RawGame>,
    product: Option<RawGame>,
    release: RawRelease,
    rules: Option<RawRules>,
    #[serde(default)]
    client: BTreeMap<String, RawClient>,
    #[serde(default)]
    download: Vec<RawDownload>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGame {
    app_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRelease {
    version: String,
    channel: Option<String>,
    notes: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRules {
    wasm: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawClient {
    path: String,
    /// Required, except for `[client.web]` (default `index.html`).
    entrypoint: Option<String>,
    delivery: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDownload {
    path: String,
    platform: Option<String>,
    label: Option<String>,
    file_name: Option<String>,
}

impl Manifest {
    /// Read and validate the manifest at `path`.
    pub fn load(path: &Path) -> Result<Manifest, Error> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::invalid(format!("no manifest at {}", path.display())))
            }
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                return Err(Error::invalid(format!("{} is not UTF-8 text", path.display())))
            }
            Err(e) => return Err(Error::io(format!("cannot read {}: {e}", path.display()))),
        };
        let dir = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        };
        Manifest::parse(&text, &dir)
    }

    /// Validate manifest text; relative paths are joined to `dir`. Every
    /// problem found is reported, one per line.
    pub fn parse(text: &str, dir: &Path) -> Result<Manifest, Error> {
        let raw: Raw = toml::from_str(text).map_err(|e| Error::invalid(format!("biome.toml: {}", e.to_string().trim_end())))?;
        let mut problems = Vec::new();

        let (kind, table, app_id) = match (raw.game, raw.product) {
            (Some(g), None) => (Kind::Game, "[game]", g.app_id),
            (None, Some(p)) => (Kind::Software, "[product]", p.app_id),
            (Some(g), Some(_)) => {
                problems.push("[game] and [product] are both here: a manifest publishes a game or a software product".to_string());
                (Kind::Game, "[game]", g.app_id)
            }
            (None, None) => {
                return Err(Error::invalid(
                    "biome.toml needs a [game] table (a game) or a [product] table (software: add-ons, plugins, tools) with its app_id",
                ))
            }
        };
        if !is_app_id(&app_id) {
            problems.push(format!("{table} app_id {app_id:?} must match ^[a-z0-9][a-z0-9-]{{2,63}}$"));
        }
        if !is_semver(&raw.release.version) {
            problems.push(format!("[release] version {:?} must be semver MAJOR.MINOR.PATCH[-pre]", raw.release.version));
        }
        let channel = match raw.release.channel.as_deref() {
            None | Some("stable") => Channel::Stable,
            Some("beta") => Channel::Beta,
            Some("internal") => Channel::Internal,
            Some(other) => {
                problems.push(format!("[release] channel {other:?} must be \"internal\", \"beta\" or \"stable\""));
                Channel::Stable
            }
        };
        let notes = match raw.release.notes.as_deref() {
            None => None,
            Some("") => {
                problems.push("[release] notes is empty; leave it out or name a file".to_string());
                None
            }
            Some(p) => Some(dir.join(p)),
        };
        let rules = match raw.rules {
            None => None,
            Some(_) if kind == Kind::Software => {
                problems.push("[rules] is for games; a [product] release has [[download]] files only".to_string());
                None
            }
            Some(r) if r.wasm.is_empty() => {
                problems.push("[rules] wasm is empty".to_string());
                None
            }
            Some(r) => Some(dir.join(r.wasm)),
        };

        match kind {
            Kind::Game => {
                if raw.client.is_empty() {
                    problems.push("no [client.<platform>] table: a release needs at least one client build".to_string());
                }
                if !raw.download.is_empty() {
                    problems.push(
                        "[[download]] is for software products: use [product] instead of [game], or ship [client.<platform>] builds"
                            .to_string(),
                    );
                }
            }
            Kind::Software => {
                if !raw.client.is_empty() {
                    problems.push("[client.<platform>] tables are for games; a [product] release has [[download]] files".to_string());
                }
                if raw.download.is_empty() {
                    problems.push("no [[download]]: a software release needs at least one file".to_string());
                }
                if raw.download.len() > MAX_DOWNLOADS {
                    problems.push(format!("{} [[download]] entries; a release has at most {MAX_DOWNLOADS}", raw.download.len()));
                }
            }
        }

        let mut downloads = Vec::new();
        for (i, d) in raw.download.into_iter().enumerate() {
            let table = format!("[[download]] #{}", i + 1);
            if d.path.is_empty() {
                problems.push(format!("{table} path is empty"));
            }
            let platform = d.platform.unwrap_or_else(|| "any".to_string());
            if platform != "any" && !PLATFORMS.contains(&platform.as_str()) {
                problems.push(format!("{table} platform {platform:?}: expected any, {}", PLATFORMS.join(", ")));
            }
            let label = match d.label.as_deref().map(str::trim) {
                None => None,
                Some("") => {
                    problems.push(format!("{table} label is empty; leave it out or say what the file is"));
                    None
                }
                Some(l) if l.chars().count() > MAX_LABEL_CHARS || l.chars().any(char::is_control) => {
                    problems.push(format!("{table} label is at most {MAX_LABEL_CHARS} characters on one line"));
                    None
                }
                Some(l) => Some(l.to_string()),
            };
            if let Some(name) = &d.file_name {
                if !is_download_name(name) {
                    problems.push(format!("{table} file_name {name:?} {DOWNLOAD_NAME_RULE}"));
                }
            }
            downloads.push(DownloadSpec { index: i + 1, path: dir.join(&d.path), platform, label, file_name: d.file_name });
        }

        let mut clients = Vec::new();
        for (platform, c) in raw.client {
            let table = format!("[client.{platform}]");
            if !CLIENT_PLATFORMS.contains(&platform.as_str()) {
                problems.push(format!("{table} unknown platform; expected one of {}", CLIENT_PLATFORMS.join(", ")));
            }
            let web = platform == WEB;
            if web && is_app_id(&app_id) && !is_dns_label(&app_id) {
                problems.push(format!(
                    "{table} {app_id:?} can't be played in a browser: a web build plays at <app_id>.biome.news, so its app id must be 1-63 letters, digits and '-', not ending in '-'"
                ));
            }
            if c.path.is_empty() {
                problems.push(format!("{table} path is empty"));
            }
            let entrypoint = match c.entrypoint {
                Some(e) => e,
                None if web => "index.html".to_string(),
                None => {
                    problems.push(format!("{table} entrypoint is missing: the executable inside the build"));
                    String::new()
                }
            };
            if !entrypoint.is_empty() {
                if let Some(why) = entrypoint_problem(&entrypoint) {
                    problems.push(format!("{table} entrypoint {entrypoint:?} {why}"));
                }
            }
            if web && !(entrypoint.ends_with(".html") || entrypoint.ends_with(".htm")) {
                problems.push(format!("{table} entrypoint {entrypoint:?}: a web build opens an .html page (index.html)"));
            }
            let is_zip = Path::new(&c.path).extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("zip"));
            if web && (is_zip || c.delivery.as_deref() == Some("zip")) {
                problems.push(format!("{table} a web build is a folder sent file by file (the browser fetches each file); not a zip"));
            }
            let delivery = match c.delivery.as_deref() {
                None | Some("files") if !is_zip => Delivery::Files,
                None | Some("zip") => Delivery::Zip,
                Some("files") => {
                    problems.push(format!("{table} delivery \"files\" needs a folder; a .zip is sent whole (point path at the folder)"));
                    Delivery::Zip
                }
                Some(other) => {
                    problems.push(format!("{table} delivery {other:?} must be \"files\" (the default for a folder) or \"zip\""));
                    Delivery::Files
                }
            };
            clients.push(ClientSpec { platform, path: dir.join(&c.path), entrypoint, delivery });
        }

        if !problems.is_empty() {
            return Err(Error::invalid(problems.join("\n")));
        }
        Ok(Manifest {
            dir: dir.to_path_buf(),
            kind,
            app_id,
            version: raw.release.version,
            channel,
            notes,
            rules,
            clients,
            downloads,
        })
    }
}

/// What a published download's name may be.
pub const DOWNLOAD_NAME_RULE: &str =
    "must be 1-120 letters, digits, '.', '_' or '-', starting with a letter or digit";

/// `^[A-Za-z0-9][A-Za-z0-9._-]{0,119}$`: a download's name, which is also
/// its storage key and what the buyer's browser saves it as.
pub fn is_download_name(s: &str) -> bool {
    let b = s.as_bytes();
    (1..=120).contains(&b.len())
        && b[0].is_ascii_alphanumeric()
        && b.iter().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

/// A publishable name close to `name`: other characters become `_`, runs of
/// them collapse, leading dots and dashes go. `None` if nothing is left.
pub fn suggest_download_name(name: &str) -> Option<String> {
    let mut out = String::new();
    for c in name.chars() {
        let keep = c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-');
        if keep {
            out.push(c);
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    let out = out.trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
    let out: String = out.chars().take(120).collect();
    is_download_name(&out).then_some(out)
}

/// `^[a-z0-9][a-z0-9-]{2,63}$`, the `game_apps.app_id` check.
pub fn is_app_id(s: &str) -> bool {
    let b = s.as_bytes();
    let ok = |c: &u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    (3..=64).contains(&b.len()) && ok(&b[0]) && b[1..].iter().all(|c| ok(c) || *c == b'-')
}

/// An app id that can be a host label (1-63 of `[a-z0-9-]`, not starting or
/// ending with `-`): what a game with a web build needs, since it plays at
/// `<app_id>.biome.news` (spec §11).
pub fn is_dns_label(s: &str) -> bool {
    (1..=63).contains(&s.len())
        && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !s.starts_with('-')
        && !s.ends_with('-')
}

/// Semantic version `MAJOR.MINOR.PATCH` with an optional `-pre` part (dot
/// separated `[0-9A-Za-z-]` identifiers). No leading zeros in numbers, no
/// `+build` metadata.
pub fn is_semver(v: &str) -> bool {
    let (core, pre) = match v.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (v, None),
    };
    let numeric = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) && (p == "0" || !p.starts_with('0'));
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() != 3 || !parts.iter().all(|p| numeric(p)) {
        return false;
    }
    match pre {
        None => true,
        Some(pre) => pre.split('.').all(|id| {
            !id.is_empty()
                && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                && (!id.bytes().all(|b| b.is_ascii_digit()) || numeric(id))
        }),
    }
}

/// Why `entrypoint` is not a safe path inside an archive, or `None` when it
/// is: relative, `/`-separated, no empty, `.` or `..` segments.
pub fn entrypoint_problem(e: &str) -> Option<&'static str> {
    if e.is_empty() {
        return Some("is empty");
    }
    if e.contains('\\') {
        return Some("must use / as the separator");
    }
    if e.starts_with('/') || has_drive_prefix(e) {
        return Some("must be relative to the archive root");
    }
    if e.chars().any(|c| c.is_control()) {
        return Some("contains control characters");
    }
    for seg in e.split('/') {
        match seg {
            "" => return Some("has an empty path segment"),
            "." | ".." => return Some("must not contain . or .. segments"),
            _ => {}
        }
    }
    None
}

fn has_drive_prefix(e: &str) -> bool {
    let b = e.as_bytes();
    b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"
[game]
app_id = "mecha-aether"

[release]
version = "0.3.0"
channel = "beta"
notes = "release-notes.md"

[rules]
wasm = "target/wasm32-unknown-unknown/release/mecha_rules.wasm"

[client.windows-x64]
path = "dist/windows"
entrypoint = "MechaAether.exe"

[client.linux-x64]
path = "dist/linux.zip"
entrypoint = "bin/mecha-aether"
"#;

    fn parse(text: &str) -> Result<Manifest, Error> {
        Manifest::parse(text, Path::new("game"))
    }

    fn problems(text: &str) -> String {
        match parse(text) {
            Err(Error::Invalid(msg)) => msg,
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    fn minimal(app_id: &str, version: &str, extra: &str, client: &str) -> String {
        format!("[game]\napp_id = \"{app_id}\"\n\n[release]\nversion = \"{version}\"\n{extra}\n{client}")
    }

    const ONE_CLIENT: &str = "[client.windows-x64]\npath = \"dist\"\nentrypoint = \"Game.exe\"\n";

    #[test]
    fn parses_the_spec_example() {
        let m = parse(FULL).unwrap();
        assert_eq!(m.app_id, "mecha-aether");
        assert_eq!(m.version, "0.3.0");
        assert_eq!(m.channel, Channel::Beta);
        assert_eq!(m.notes, Some(Path::new("game").join("release-notes.md")));
        assert_eq!(m.rules, Some(Path::new("game").join("target/wasm32-unknown-unknown/release/mecha_rules.wasm")));
        // Sorted by platform name.
        let platforms: Vec<_> = m.clients.iter().map(|c| c.platform.as_str()).collect();
        assert_eq!(platforms, ["linux-x64", "windows-x64"]);
        assert_eq!(m.clients[1].path, Path::new("game").join("dist/windows"));
        assert_eq!(m.clients[1].entrypoint, "MechaAether.exe");
        assert_eq!(m.clients[0].entrypoint, "bin/mecha-aether");
    }

    #[test]
    fn defaults_stable_channel_no_notes_no_rules() {
        let m = parse(&minimal("tic-tac-toe", "1.0.0", "", ONE_CLIENT)).unwrap();
        assert_eq!(m.channel, Channel::Stable);
        assert_eq!(m.notes, None);
        assert_eq!(m.rules, None);
        assert_eq!(m.clients.len(), 1);
    }

    #[test]
    fn all_four_platforms_are_accepted() {
        let mut clients = String::new();
        for p in PLATFORMS {
            clients.push_str(&format!("[client.{p}]\npath = \"dist/{p}\"\nentrypoint = \"game\"\n"));
        }
        let m = parse(&minimal("abc", "1.0.0", "", &clients)).unwrap();
        assert_eq!(m.clients.len(), 4);
    }

    #[test]
    fn a_web_build_is_a_folder_sent_file_by_file() {
        // entrypoint defaults to index.html for the web.
        let m = parse(&minimal("omg", "0.2.0", "", "[client.web]\npath = \"dist/web\"\n")).unwrap();
        assert_eq!(m.clients.len(), 1);
        let c = &m.clients[0];
        assert_eq!((c.platform.as_str(), c.entrypoint.as_str(), c.delivery), ("web", "index.html", Delivery::Files));
        // Next to desktop builds, sorted by platform name.
        let both = parse(&minimal("omg", "0.2.0", "", "[client.web]\npath = \"w\"\nentrypoint = \"play/index.html\"\n[client.windows-x64]\npath = \"d\"\nentrypoint = \"G.exe\"\n")).unwrap();
        let platforms: Vec<_> = both.clients.iter().map(|c| c.platform.as_str()).collect();
        assert_eq!(platforms, ["web", "windows-x64"]);
        assert_eq!(both.clients[0].entrypoint, "play/index.html");
    }

    #[test]
    fn a_web_build_is_never_a_zip_and_opens_a_page() {
        let msg = problems(&minimal("omg", "0.2.0", "", "[client.web]\npath = \"dist/web.zip\"\n"));
        assert!(msg.contains("[client.web] a web build is a folder sent file by file"), "{msg}");
        let msg = problems(&minimal("omg", "0.2.0", "", "[client.web]\npath = \"dist/web\"\ndelivery = \"zip\"\n"));
        assert!(msg.contains("not a zip"), "{msg}");
        let msg = problems(&minimal("omg", "0.2.0", "", "[client.web]\npath = \"dist/web\"\nentrypoint = \"omg.wasm\"\n"));
        assert!(msg.contains("an .html page"), "{msg}");
        // A desktop build still names its executable.
        let msg = problems(&minimal("omg", "0.2.0", "", "[client.linux-x64]\npath = \"d\"\n"));
        assert!(msg.contains("[client.linux-x64] entrypoint is missing"), "{msg}");
    }

    #[test]
    fn a_web_build_needs_an_app_id_that_is_a_host_label() {
        for app in ["omg-", &"a".repeat(64)] {
            let msg = problems(&minimal(app, "0.2.0", "", "[client.web]
path = \"dist/web\"
"));
            assert!(msg.contains("can't be played in a browser"), "{msg}");
        }
        // The same id may still ship desktop builds.
        assert!(parse(&minimal("omg-", "0.2.0", "", "[client.linux-x64]
path = \"d\"
entrypoint = \"g\"
")).is_ok());
        assert!(is_dns_label("omg") && is_dns_label("a1-b") && !is_dns_label("-a") && !is_dns_label("a-"));
    }

    #[test]
    fn a_download_cannot_target_the_web() {
        let msg = problems(&software("[[download]]\npath = \"a.zip\"\nplatform = \"web\"\n"));
        assert!(msg.contains("[[download]] #1 platform \"web\""), "{msg}");
    }

    #[test]
    fn app_id_rule() {
        for ok in ["abc", "mecha-aether", "0ad", "a-b", "a1-", &"a".repeat(64)] {
            assert!(is_app_id(ok), "{ok} should be valid");
        }
        for bad in ["ab", "-abc", "Mecha", "mecha_aether", "mecha aether", "", &"a".repeat(65), "méca"] {
            assert!(!is_app_id(bad), "{bad} should be invalid");
        }
        assert!(problems(&minimal("Mecha", "1.0.0", "", ONE_CLIENT)).contains("[game] app_id"));
    }

    #[test]
    fn version_rule() {
        for ok in ["0.0.0", "1.2.3", "10.20.30", "1.0.0-beta", "1.0.0-beta.1", "1.0.0-rc-1", "1.0.0-0", "1.0.0-x.7.z.92", "1.0.0-0a"] {
            assert!(is_semver(ok), "{ok} should be valid");
        }
        for bad in [
            "1.0", "1", "1.0.0.0", "01.0.0", "1.02.0", "1.0.0-", "1.0.0-beta..1", "1.0.0-01", "1.0.0+build", "1.0.0-beta+exp",
            "v1.0.0", "1.0.0-be ta", "", "a.b.c", "1..0",
        ] {
            assert!(!is_semver(bad), "{bad} should be invalid");
        }
        assert!(problems(&minimal("abc", "1.0", "", ONE_CLIENT)).contains("[release] version"));
    }

    #[test]
    fn channel_rule() {
        let m = parse(&minimal("abc", "1.0.0", "channel = \"stable\"", ONE_CLIENT)).unwrap();
        assert_eq!(m.channel, Channel::Stable);
        let m = parse(&minimal("abc", "1.0.0", "channel = \"internal\"", ONE_CLIENT)).unwrap();
        assert_eq!((m.channel, m.channel.as_str()), (Channel::Internal, "internal"));
        let msg = problems(&minimal("abc", "1.0.0", "channel = \"alpha\"", ONE_CLIENT));
        assert!(msg.contains("[release] channel \"alpha\""), "{msg}");
    }

    #[test]
    fn platform_rule() {
        let msg = problems(&minimal("abc", "1.0.0", "", "[client.windows]\npath = \"d\"\nentrypoint = \"g.exe\"\n"));
        assert!(msg.contains("[client.windows] unknown platform"), "{msg}");
    }

    #[test]
    fn needs_at_least_one_client() {
        let msg = problems(&minimal("abc", "1.0.0", "", ""));
        assert!(msg.contains("at least one client"), "{msg}");
    }

    #[test]
    fn entrypoint_rule() {
        for ok in ["Game.exe", "bin/game", "Game.app/Contents/MacOS/Game", "a b/c d.exe", "..hidden/x", "x..y"] {
            assert_eq!(entrypoint_problem(ok), None, "{ok} should be valid");
        }
        for bad in ["", "bin\\game.exe", "/usr/bin/game", "C:/game.exe", "../game", "bin/../game", "./game", "bin//game", "bin/", "a\u{0}b"] {
            assert!(entrypoint_problem(bad).is_some(), "{bad:?} should be invalid");
        }
        let msg = problems(&minimal("abc", "1.0.0", "", "[client.linux-x64]\npath = \"d\"\nentrypoint = \"../escape\"\n"));
        assert!(msg.contains("[client.linux-x64] entrypoint \"../escape\""), "{msg}");
    }

    #[test]
    fn empty_paths_are_refused() {
        let msg = problems(&minimal("abc", "1.0.0", "notes = \"\"\n\n[rules]\nwasm = \"\"\n", "[client.linux-x64]\npath = \"\"\nentrypoint = \"g\"\n"));
        assert!(msg.contains("[release] notes is empty"), "{msg}");
        assert!(msg.contains("[rules] wasm is empty"), "{msg}");
        assert!(msg.contains("[client.linux-x64] path is empty"), "{msg}");
    }

    #[test]
    fn reports_every_problem_at_once() {
        let msg = problems(&minimal("X", "1", "channel = \"nightly\"", "[client.amiga]\npath = \"d\"\nentrypoint = \"/abs\"\n"));
        assert_eq!(msg.lines().count(), 5, "{msg}");
    }

    #[test]
    fn unknown_keys_and_missing_tables_are_refused() {
        // A typo'd table name must not silently drop the clients.
        let msg = problems(&minimal("abc", "1.0.0", "", "[clients.windows-x64]\npath = \"d\"\nentrypoint = \"g\"\n"));
        assert!(msg.contains("clients"), "{msg}");
        let msg = problems(&minimal("abc", "1.0.0", "chanel = \"beta\"", ONE_CLIENT));
        assert!(msg.contains("chanel"), "{msg}");
        let msg = problems("[release]\nversion = \"1.0.0\"\n");
        assert!(msg.contains("game"), "{msg}");
        let msg = problems("[game]\napp_id = \"abc\"\n[release]\n");
        assert!(msg.contains("version"), "{msg}");
        let msg = problems("[game\n");
        assert!(msg.starts_with("biome.toml:"), "{msg}");
    }

    const ADDON: &str = r#"
[product]
app_id = "node-wrangler"

[release]
version = "1.2.0"
notes = "CHANGELOG.md"

[[download]]
path = "dist/node_wrangler-1.2.0.zip"
label = "  Add-on (Blender 4.2+)  "

[[download]]
path = "dist/helper"
platform = "windows-x64"
file_name = "helper-win.zip"
"#;

    #[test]
    fn parses_a_software_product() {
        let m = parse(ADDON).unwrap();
        assert_eq!(m.kind, Kind::Software);
        assert_eq!(m.app_id, "node-wrangler");
        assert_eq!(m.channel, Channel::Stable);
        assert!(m.clients.is_empty() && m.rules.is_none());
        assert_eq!(m.downloads.len(), 2);
        let d = &m.downloads[0];
        assert_eq!((d.index, d.platform.as_str()), (1, "any"), "platform defaults to any");
        assert_eq!(d.path, Path::new("game").join("dist/node_wrangler-1.2.0.zip"));
        assert_eq!(d.label.as_deref(), Some("Add-on (Blender 4.2+)"), "labels are trimmed");
        assert_eq!(d.file_name, None);
        assert_eq!(d.table(), "[[download]] #1");
        let d = &m.downloads[1];
        assert_eq!((d.platform.as_str(), d.file_name.as_deref(), d.label.as_deref()), ("windows-x64", Some("helper-win.zip"), None));
    }

    #[test]
    fn game_manifests_parse_as_before() {
        let m = parse(FULL).unwrap();
        assert_eq!(m.kind, Kind::Game);
        assert!(m.downloads.is_empty());
    }

    fn software(downloads: &str) -> String {
        format!("[product]\napp_id = \"node-wrangler\"\n\n[release]\nversion = \"1.0.0\"\n\n{downloads}")
    }

    const ONE_DOWNLOAD: &str = "[[download]]\npath = \"addon.zip\"\n";

    #[test]
    fn a_manifest_is_a_game_or_software() {
        let msg = problems(&format!("[game]\napp_id = \"abc\"\n{}", &software(ONE_DOWNLOAD)));
        assert!(msg.contains("[game] and [product] are both here"), "{msg}");
        let msg = problems("[release]\nversion = \"1.0.0\"\n");
        assert!(msg.contains("[game]") && msg.contains("[product]"), "{msg}");
        let msg = problems(&software(ONE_DOWNLOAD).replace("node-wrangler", "Node"));
        assert!(msg.contains("[product] app_id \"Node\""), "{msg}");
    }

    #[test]
    fn software_has_downloads_and_nothing_else() {
        let msg = problems(&software(""));
        assert!(msg.contains("no [[download]]"), "{msg}");
        let msg = problems(&software(&format!("{ONE_DOWNLOAD}\n[rules]\nwasm = \"r.wasm\"\n")));
        assert!(msg.contains("[rules] is for games"), "{msg}");
        let msg = problems(&software(&format!("{ONE_DOWNLOAD}\n{ONE_CLIENT}")));
        assert!(msg.contains("[client.<platform>] tables are for games"), "{msg}");
        let msg = problems(&minimal("abc", "1.0.0", "", &format!("{ONE_CLIENT}\n{ONE_DOWNLOAD}")));
        assert!(msg.contains("[[download]] is for software products"), "{msg}");
        let eleven: String = (0..11).map(|i| format!("[[download]]\npath = \"f{i}.zip\"\n")).collect();
        let msg = problems(&software(&eleven));
        assert!(msg.contains("at most 10"), "{msg}");
        let ten: String = (0..10).map(|i| format!("[[download]]\npath = \"f{i}.zip\"\n")).collect();
        assert_eq!(parse(&software(&ten)).unwrap().downloads.len(), 10);
    }

    #[test]
    fn download_fields_are_checked() {
        let msg = problems(&software("[[download]]\npath = \"\"\nplatform = \"amiga\"\nlabel = \" \"\nfile_name = \".env\"\n"));
        assert!(msg.contains("[[download]] #1 path is empty"), "{msg}");
        assert!(msg.contains("[[download]] #1 platform \"amiga\""), "{msg}");
        assert!(msg.contains("[[download]] #1 label is empty"), "{msg}");
        assert!(msg.contains("[[download]] #1 file_name \".env\""), "{msg}");
        let long = "x".repeat(81);
        let msg = problems(&software(&format!("[[download]]\npath = \"a.zip\"\nlabel = \"{long}\"\n")));
        assert!(msg.contains("label is at most 80"), "{msg}");
        // Unknown keys are typos, not silently ignored.
        let msg = problems(&software("[[download]]\npath = \"a.zip\"\nlable = \"Add-on\"\n"));
        assert!(msg.contains("lable"), "{msg}");
        for p in PLATFORMS {
            let m = parse(&software(&format!("[[download]]\npath = \"a.zip\"\nplatform = \"{p}\"\n"))).unwrap();
            assert_eq!(m.downloads[0].platform, p);
        }
    }

    #[test]
    fn download_names() {
        for ok in ["addon.zip", "a", "Node_Wrangler-1.2.0.tar.gz", "README", "9.py", &"a".repeat(120)] {
            assert!(is_download_name(ok), "{ok}");
        }
        for bad in ["", ".env", "..", "-x", "_x", "a b.zip", "a/b", "é.zip", &"a".repeat(121)] {
            assert!(!is_download_name(bad), "{bad}");
        }
        assert_eq!(suggest_download_name("My Addon (v2).zip").as_deref(), Some("My_Addon_v2_.zip"));
        assert_eq!(suggest_download_name(".hidden").as_deref(), Some("hidden"));
        assert_eq!(suggest_download_name("ñandú.py").as_deref(), Some("and_.py"));
        assert_eq!(suggest_download_name("...").as_deref(), None);
    }

    #[test]
    fn a_folder_goes_file_by_file_unless_it_says_zip() {
        let m = parse(FULL).unwrap();
        // linux-x64 is dist/linux.zip, windows-x64 the folder dist/windows.
        assert_eq!((m.clients[0].delivery, m.clients[1].delivery), (Delivery::Zip, Delivery::Files));
        let zipped = parse(&minimal("abc", "1.0.0", "", "[client.windows-x64]\npath = \"dist\"\nentrypoint = \"g\"\ndelivery = \"zip\"\n")).unwrap();
        assert_eq!(zipped.clients[0].delivery, Delivery::Zip);
        let files = parse(&minimal("abc", "1.0.0", "", "[client.windows-x64]\npath = \"dist\"\nentrypoint = \"g\"\ndelivery = \"files\"\n")).unwrap();
        assert_eq!(files.clients[0].delivery, Delivery::Files);
        let msg = problems(&minimal("abc", "1.0.0", "", "[client.windows-x64]\npath = \"g.zip\"\nentrypoint = \"g\"\ndelivery = \"files\"\n"));
        assert!(msg.contains("needs a folder"), "{msg}");
        let msg = problems(&minimal("abc", "1.0.0", "", "[client.windows-x64]\npath = \"dist\"\nentrypoint = \"g\"\ndelivery = \"rsync\"\n"));
        assert!(msg.contains("delivery \"rsync\""), "{msg}");
    }

    #[test]
    fn load_resolves_paths_next_to_the_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("biome.toml");
        std::fs::write(&path, FULL).unwrap();
        let m = Manifest::load(&path).unwrap();
        assert_eq!(m.dir, dir.path());
        assert_eq!(m.clients[1].path, dir.path().join("dist/windows"));

        match Manifest::load(&dir.path().join("missing.toml")) {
            Err(Error::Invalid(msg)) => assert!(msg.contains("no manifest"), "{msg}"),
            other => panic!("{other:?}"),
        }
    }
}
