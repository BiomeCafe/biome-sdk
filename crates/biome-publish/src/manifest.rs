//! `biome.toml`: what to publish (spec §7).
//!
//! ```toml
//! [game]
//! app_id = "mecha-aether"
//!
//! [release]
//! version = "0.3.0"          # required
//! channel = "beta"           # stable | beta, default stable
//! notes = "release-notes.md" # optional: a file whose text becomes the notes
//!
//! [rules]                    # optional
//! wasm = "target/wasm32-unknown-unknown/release/mecha_rules.wasm"
//!
//! [client.windows-x64]       # one table per platform
//! path = "dist/windows"      # a folder (zipped by the CLI) or a .zip
//! entrypoint = "MechaAether.exe"
//! ```
//!
//! Paths are relative to the folder the manifest is in. Parsing checks
//! everything that can be checked without touching the files; whether the
//! files exist and what is in them is [`crate::prepare`]'s job.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::Error;

/// Platforms a client build may target, in the server's spelling.
pub const PLATFORMS: [&str; 4] = ["windows-x64", "macos-arm64", "macos-x64", "linux-x64"];

/// Release notes limit (characters), as the `game_releases.notes` column.
pub const MAX_NOTES_CHARS: usize = 4000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Stable,
    Beta,
}

impl Channel {
    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Stable => "stable",
            Channel::Beta => "beta",
        }
    }
}

/// A parsed, validated manifest. Paths are already joined to the manifest's
/// folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// The folder the manifest lives in.
    pub dir: PathBuf,
    pub app_id: String,
    pub version: String,
    pub channel: Channel,
    /// File whose text becomes the release notes.
    pub notes: Option<PathBuf>,
    /// The compiled rules module, for games the match host runs.
    pub rules: Option<PathBuf>,
    /// One per platform, sorted by platform name.
    pub clients: Vec<ClientSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientSpec {
    pub platform: String,
    /// A folder to zip, or a `.zip` to upload as it is.
    pub path: PathBuf,
    /// Path of the executable inside the archive, `/`-separated.
    pub entrypoint: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    game: RawGame,
    release: RawRelease,
    rules: Option<RawRules>,
    #[serde(default)]
    client: BTreeMap<String, RawClient>,
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
    entrypoint: String,
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

        if !is_app_id(&raw.game.app_id) {
            problems.push(format!("[game] app_id {:?} must match ^[a-z0-9][a-z0-9-]{{2,63}}$", raw.game.app_id));
        }
        if !is_semver(&raw.release.version) {
            problems.push(format!("[release] version {:?} must be semver MAJOR.MINOR.PATCH[-pre]", raw.release.version));
        }
        let channel = match raw.release.channel.as_deref() {
            None | Some("stable") => Channel::Stable,
            Some("beta") => Channel::Beta,
            Some(other) => {
                problems.push(format!("[release] channel {other:?} must be \"stable\" or \"beta\""));
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
            Some(r) if r.wasm.is_empty() => {
                problems.push("[rules] wasm is empty".to_string());
                None
            }
            Some(r) => Some(dir.join(r.wasm)),
        };

        if raw.client.is_empty() {
            problems.push("no [client.<platform>] table: a release needs at least one client build".to_string());
        }
        let mut clients = Vec::new();
        for (platform, c) in raw.client {
            let table = format!("[client.{platform}]");
            if !PLATFORMS.contains(&platform.as_str()) {
                problems.push(format!("{table} unknown platform; expected one of {}", PLATFORMS.join(", ")));
            }
            if c.path.is_empty() {
                problems.push(format!("{table} path is empty"));
            }
            if let Some(why) = entrypoint_problem(&c.entrypoint) {
                problems.push(format!("{table} entrypoint {:?} {why}", c.entrypoint));
            }
            clients.push(ClientSpec { platform, path: dir.join(&c.path), entrypoint: c.entrypoint });
        }

        if !problems.is_empty() {
            return Err(Error::invalid(problems.join("\n")));
        }
        Ok(Manifest {
            dir: dir.to_path_buf(),
            app_id: raw.game.app_id,
            version: raw.release.version,
            channel,
            notes,
            rules,
            clients,
        })
    }
}

/// `^[a-z0-9][a-z0-9-]{2,63}$`, the `game_apps.app_id` check.
pub fn is_app_id(s: &str) -> bool {
    let b = s.as_bytes();
    let ok = |c: &u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    (3..=64).contains(&b.len()) && ok(&b[0]) && b[1..].iter().all(|c| ok(c) || *c == b'-')
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
