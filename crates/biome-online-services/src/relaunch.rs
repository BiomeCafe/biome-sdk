//! Start through Biome, the way Steam's `RestartAppIfNecessary` does
//! (GAME_SDK_SPEC.md §1.1).
//!
//! A game Biome installed, started any other way (a desktop shortcut, a
//! double click in its folder), hands itself to Biome and exits; Biome
//! starts the installed build again with a launch nonce, so it signs in
//! without asking. Call [`restart_through_biome_if_needed`] first thing in
//! `main`:
//!
//! ```ignore
//! if biome_online_services::restart_through_biome_if_needed("mecha-aether") {
//!     return; // Biome is starting the game
//! }
//! ```
//!
//! It does nothing (returns `false`) when:
//! - this is a debug build;
//! - `BIOME_NO_RELAUNCH=1` is set or `--no-relaunch` was passed;
//! - Biome started this process (`BIOME_SESSION_NONCE` or `BIOME_APP_ID`
//!   set, so a launch without a nonce never loops);
//! - Biome isn't installed for this user;
//! - the installed Biome doesn't take `biome://run/` links (before 0.1.18:
//!   it would ignore the link, and with this process gone nothing would
//!   start). Biome 0.1.18 and later say they do in a file next to their
//!   program ([`CAPABILITIES_FILE`]), which an update or a downgrade
//!   replaces along with the program;
//! - this program isn't inside the version folder Biome's games folder
//!   records for this app id (`installed.json`: this app id, a valid
//!   version, an entrypoint that is there), exactly what Biome needs to
//!   start it. A developer running their own build is never sent to the
//!   installed copy, and a broken install just runs as is.
//!
//! Windows only for now: the games folder is found next to the `biome:`
//! launcher the registry names; other platforms have no installed layout to
//! find yet and always get `false`.

use std::path::{Component, Path, PathBuf};

use crate::detect;
use crate::handshake::{ENV_APP_ID, ENV_SESSION_NONCE};

/// Set to `1` to never be handed to Biome.
pub const ENV_NO_RELAUNCH: &str = "BIOME_NO_RELAUNCH";
/// The command-line form of [`ENV_NO_RELAUNCH`].
pub const NO_RELAUNCH_ARG: &str = "--no-relaunch";

/// The link that asks Biome to start an installed game: `biome://run/<app_id>`.
/// (`biome://play/<app_id>` is already the page that plays a game's web
/// build, so a different word.)
pub fn run_url(app_id: &str) -> String {
    format!("biome://run/{}", detect::url_escape(app_id))
}

/// Why a game was not handed to Biome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Skip {
    DebugBuild,
    OptedOut,
    LaunchedByBiome,
    BiomeNotInstalled,
    /// The installed Biome is older than run links (no
    /// [`RUN_LINKS`] in its [`CAPABILITIES_FILE`]).
    BiomeTooOld,
    /// This program isn't Biome's installed copy of the game, or the install
    /// is one Biome couldn't start.
    NotInstalledByBiome,
    /// No installed layout to look for on this platform.
    Unsupported,
}

/// What [`decide`] says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Relaunch {
    /// Hand `run_url(app_id)` to this launcher and exit.
    Through { launcher: PathBuf },
    Skip(Skip),
}

/// What the decision is made from. [`RelaunchFacts::gather`] reads them
/// from this process; tests build them by hand.
#[derive(Clone, Debug, Default)]
pub struct RelaunchFacts {
    pub supported: bool,
    pub debug_build: bool,
    pub opted_out: bool,
    pub launched_by_biome: bool,
    /// Biome's launcher (canonical path), if Biome is installed.
    pub launcher: Option<PathBuf>,
    /// That Biome takes `biome://run/` links ([`takes_run_links`]).
    pub run_links: bool,
    /// This program (canonical path).
    pub exe: Option<PathBuf>,
}

impl RelaunchFacts {
    pub fn gather() -> Self {
        let set = |name: &str| std::env::var_os(name).is_some_and(|v| !v.is_empty());
        let opted_out = std::env::var(ENV_NO_RELAUNCH).is_ok_and(|v| !v.is_empty() && v != "0")
            || std::env::args().skip(1).any(|a| a == NO_RELAUNCH_ARG);
        let launcher = detect::installed_launcher().and_then(|l| canonical(&l));
        Self {
            supported: cfg!(windows),
            debug_build: cfg!(debug_assertions),
            opted_out,
            launched_by_biome: set(ENV_SESSION_NONCE) || set(ENV_APP_ID),
            run_links: launcher.as_deref().is_some_and(takes_run_links),
            launcher,
            exe: std::env::current_exe().ok().and_then(|e| canonical(&e)),
        }
    }
}

/// The path as the disk spells it (links and `..` resolved, the case on
/// disk), without Windows' `\\?\` prefix on a plain drive path, so the
/// launcher is started by the path it is registered under.
pub fn canonical(path: &Path) -> Option<PathBuf> {
    let full = std::fs::canonicalize(path).ok()?;
    #[cfg(windows)]
    {
        let text = full.to_str()?;
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            if rest.as_bytes().get(1) == Some(&b':') {
                return Some(PathBuf::from(rest));
            }
        }
    }
    Some(full)
}

/// Biome's games folder for `app_id`, next to its launcher:
/// `<root>/games/<app_id>` (GAME_PUBLISHING_SPEC.md §6).
pub fn install_dir(launcher: &Path, app_id: &str) -> Option<PathBuf> {
    Some(launcher.parent()?.join("games").join(app_id))
}

/// The file Biome (0.1.18 and later) writes next to its own program at
/// startup, one capability per line. In an installed Biome that is
/// `<root>/current/`, which Velopack replaces whole on every update or
/// downgrade, so an older Biome never inherits a newer one's claims.
pub const CAPABILITIES_FILE: &str = "biome-capabilities";
/// The line in [`CAPABILITIES_FILE`] that says `biome://run/<app_id>` works.
pub const RUN_LINKS: &str = "run-links";
/// The line in [`CAPABILITIES_FILE`] that says Biome answers license
/// requests (`RequestBody::License`, Biome 0.1.28 and later).
pub const LICENSES: &str = "licenses";

/// Does the Biome behind `launcher` take run links? Looks for
/// [`RUN_LINKS`] in [`CAPABILITIES_FILE`] in `current/` next to the
/// launcher stub (an installed Biome), or next to the launcher itself.
/// Reads two small files, no IPC: Biome may not be running.
pub fn takes_run_links(launcher: &Path) -> bool {
    has_capability(launcher, RUN_LINKS)
}

/// Does the Biome behind `launcher` say it can do `capability` (a line of
/// its [`CAPABILITIES_FILE`])? See [`takes_run_links`].
pub fn has_capability(launcher: &Path, capability: &str) -> bool {
    let Some(dir) = launcher.parent() else {
        return false;
    };
    [dir.join("current").join(CAPABILITIES_FILE), dir.join(CAPABILITIES_FILE)].iter().any(|file| {
        std::fs::read_to_string(file).is_ok_and(|text| text.lines().any(|line| line.trim() == capability))
    })
}

/// Does the installed Biome (the `biome:` launcher's) answer license
/// requests? `None` when no Biome is installed.
pub fn installed_biome_issues_licenses() -> Option<bool> {
    let launcher = detect::installed_launcher().and_then(|l| canonical(&l))?;
    Some(has_capability(&launcher, LICENSES))
}

/// What `installed.json` says, as far as starting the game goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallRecord {
    pub app_id: String,
    pub version: String,
    pub entrypoint: String,
}

/// Read `<dir>/installed.json`. `None` when it is missing or unreadable.
pub fn read_install_record(dir: &Path) -> Option<InstallRecord> {
    let text = std::fs::read_to_string(dir.join("installed.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let field = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    Some(InstallRecord { app_id: field("app_id")?, version: field("version")?, entrypoint: field("entrypoint")? })
}

/// Same rule as the library's install versions (`game_library::valid_version`).
fn valid_version(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 40
        && v.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'.' || c == b'-')
        && !v.starts_with('.')
        && !v.contains("..")
}

/// The entrypoint inside `version_dir`, by the library's rule
/// (`game_library::entry_path`): relative, `/`-separated, no `..`.
fn entry_path(version_dir: &Path, entrypoint: &str) -> Option<PathBuf> {
    if entrypoint.is_empty() || entrypoint.contains('\\') || entrypoint.starts_with('/') {
        return None;
    }
    let rel = Path::new(entrypoint);
    if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return None;
    }
    Some(version_dir.join(rel))
}

/// Same rule as `game_apps.app_id`; anything else can't be a folder Biome made.
fn valid_app_id(id: &str) -> bool {
    let b = id.as_bytes();
    (3..=64).contains(&b.len())
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// The decision, from `facts`, `read_install` for a games folder's
/// `installed.json` ([`read_install_record`]) and `is_file` for its
/// entrypoint.
pub fn decide(
    app_id: &str,
    facts: &RelaunchFacts,
    read_install: impl Fn(&Path) -> Option<InstallRecord>,
    is_file: impl Fn(&Path) -> bool,
) -> Relaunch {
    if !facts.supported {
        return Relaunch::Skip(Skip::Unsupported);
    }
    if facts.debug_build {
        return Relaunch::Skip(Skip::DebugBuild);
    }
    if facts.opted_out {
        return Relaunch::Skip(Skip::OptedOut);
    }
    if facts.launched_by_biome {
        return Relaunch::Skip(Skip::LaunchedByBiome);
    }
    let Some(launcher) = facts.launcher.clone() else {
        return Relaunch::Skip(Skip::BiomeNotInstalled);
    };
    if !facts.run_links {
        return Relaunch::Skip(Skip::BiomeTooOld);
    }
    // What Biome's launch needs (game_library::installed_in + launch): the
    // record names this app, a valid version, an entrypoint that is there;
    // and this program is inside that version's folder.
    let installed_copy = valid_app_id(app_id)
        && install_dir(&launcher, app_id).is_some_and(|dir| {
            read_install(&dir).is_some_and(|record| {
                let version_dir = dir.join(&record.version);
                record.app_id == app_id
                    && valid_version(&record.version)
                    && entry_path(&version_dir, &record.entrypoint).is_some_and(|entry| is_file(&entry))
                    && facts.exe.as_ref().is_some_and(|exe| exe.starts_with(&version_dir) && exe != &version_dir)
            })
        });
    if installed_copy {
        Relaunch::Through { launcher }
    } else {
        Relaunch::Skip(Skip::NotInstalledByBiome)
    }
}

/// What [`restart_through_biome`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RestartOutcome {
    /// Biome is starting the installed copy: exit now.
    Relaunched,
    /// Nothing to do (see [`Skip`]): run as is.
    Skipped(Skip),
    /// This is Biome's installed copy, started outside Biome, and handing
    /// it to Biome failed.
    Failed(String),
}

/// [`restart_through_biome_if_needed`] with the reason: `Failed` tells a
/// program that must run through Biome (a licensed one) from one that
/// simply isn't Biome's install.
pub fn restart_through_biome(app_id: &str) -> RestartOutcome {
    match decide(app_id, &RelaunchFacts::gather(), read_install_record, Path::is_file) {
        Relaunch::Through { launcher } => match detect::open_link(&launcher, &run_url(app_id)) {
            Ok(()) => {
                tracing::info!(app_id, "started outside Biome; Biome is starting the installed game");
                RestartOutcome::Relaunched
            }
            Err(e) => {
                tracing::warn!(app_id, "could not hand the game to Biome, running as is: {e}");
                RestartOutcome::Failed(e.to_string())
            }
        },
        Relaunch::Skip(why) => {
            tracing::debug!(app_id, ?why, "not restarting through Biome");
            RestartOutcome::Skipped(why)
        }
    }
}

/// If this is Biome's installed copy of `app_id` and Biome didn't start it,
/// ask Biome to start it (`biome://run/<app_id>`) and return `true`: the
/// game should exit at once. `false` in every other case (see the module
/// docs), and when handing the link over fails, so the game just runs.
pub fn restart_through_biome_if_needed(app_id: &str) -> bool {
    restart_through_biome(app_id) == RestartOutcome::Relaunched
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(r"C:\Users\me\AppData\Local\Biome")
        } else {
            PathBuf::from("/home/me/.local/share/biome")
        }
    }

    fn version_dir() -> PathBuf {
        root().join("games").join("omg").join("0.2.0")
    }

    /// A release build of the installed copy, started by hand, with a Biome
    /// that takes run links.
    fn installed_copy() -> RelaunchFacts {
        RelaunchFacts {
            supported: true,
            debug_build: false,
            opted_out: false,
            launched_by_biome: false,
            launcher: Some(root().join("Biome.exe")),
            run_links: true,
            exe: Some(version_dir().join("omg_client.exe")),
        }
    }

    fn record(app_id: &str, version: &str, entrypoint: &str) -> InstallRecord {
        InstallRecord { app_id: app_id.into(), version: version.into(), entrypoint: entrypoint.into() }
    }

    /// The install as Biome left it: omg 0.2.0, entrypoint omg_client.exe.
    fn intact(dir: &Path) -> Option<InstallRecord> {
        (dir == root().join("games").join("omg")).then(|| record("omg", "0.2.0", "omg_client.exe"))
    }

    fn entry_exists(path: &Path) -> bool {
        path == version_dir().join("omg_client.exe")
    }

    fn decide_with(app_id: &str, facts: &RelaunchFacts, read: impl Fn(&Path) -> Option<InstallRecord>) -> Relaunch {
        decide(app_id, facts, read, entry_exists)
    }

    #[test]
    fn the_installed_copy_started_by_hand_goes_through_biome() {
        assert_eq!(decide_with("omg", &installed_copy(), intact), Relaunch::Through { launcher: root().join("Biome.exe") });
        assert_eq!(run_url("omg"), "biome://run/omg");
        assert_eq!(run_url("a&b c"), "biome://run/a%26b%20c");
    }

    #[test]
    fn every_way_out_is_honoured() {
        let skip = |f: RelaunchFacts| match decide_with("omg", &f, intact) {
            Relaunch::Skip(why) => Some(why),
            Relaunch::Through { .. } => None,
        };
        assert_eq!(skip(RelaunchFacts { supported: false, ..installed_copy() }), Some(Skip::Unsupported));
        assert_eq!(skip(RelaunchFacts { debug_build: true, ..installed_copy() }), Some(Skip::DebugBuild));
        assert_eq!(skip(RelaunchFacts { opted_out: true, ..installed_copy() }), Some(Skip::OptedOut));
        // Biome started it (with or without a nonce): never loop.
        assert_eq!(skip(RelaunchFacts { launched_by_biome: true, ..installed_copy() }), Some(Skip::LaunchedByBiome));
        assert_eq!(skip(RelaunchFacts { launcher: None, ..installed_copy() }), Some(Skip::BiomeNotInstalled));
    }

    /// Biome 0.1.17 and older don't know `biome://run/`: handing them the
    /// link would leave nothing running. The game runs as is instead.
    #[test]
    fn an_older_biome_without_run_links_is_not_handed_the_game() {
        assert_eq!(
            decide_with("omg", &RelaunchFacts { run_links: false, ..installed_copy() }, intact),
            Relaunch::Skip(Skip::BiomeTooOld)
        );
    }

    #[test]
    fn the_capability_marker_is_read_from_beside_the_launcher() {
        let dir = std::env::temp_dir().join(format!("biome-relaunch-caps-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("current")).unwrap();
        let launcher = dir.join("Biome.exe");
        // No marker: an older Biome.
        assert!(!takes_run_links(&launcher));
        // A marker without the line.
        std::fs::write(dir.join("current").join(CAPABILITIES_FILE), "something-else\n").unwrap();
        assert!(!takes_run_links(&launcher));
        // An installed Biome: <root>/current/biome-capabilities.
        std::fs::write(dir.join("current").join(CAPABILITIES_FILE), "something-else\r\nrun-links\r\n").unwrap();
        assert!(takes_run_links(&launcher));
        // The launcher is the program itself (no stub).
        std::fs::remove_file(dir.join("current").join(CAPABILITIES_FILE)).unwrap();
        assert!(!takes_run_links(&launcher));
        std::fs::write(dir.join(CAPABILITIES_FILE), "run-links").unwrap();
        assert!(takes_run_links(&launcher));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_developers_own_build_is_never_sent_to_the_installed_copy() {
        let dev = RelaunchFacts {
            exe: Some(PathBuf::from(if cfg!(windows) { r"D:\BiomeCafe\target\release\omg_client.exe" } else { "/src/target/release/omg_client" })),
            ..installed_copy()
        };
        assert_eq!(decide_with("omg", &dev, intact), Relaunch::Skip(Skip::NotInstalledByBiome));
        // Another game's folder, or a folder Biome no longer records as installed.
        assert_eq!(decide_with("mecha-aether", &installed_copy(), |_| Some(record("omg", "0.2.0", "omg_client.exe"))), Relaunch::Skip(Skip::NotInstalledByBiome));
        assert_eq!(decide_with("omg", &installed_copy(), |_| None), Relaunch::Skip(Skip::NotInstalledByBiome));
        // A path that only shares a prefix with the folder's name.
        let lookalike = RelaunchFacts {
            exe: Some(root().join("games").join("omg-evil").join("0.2.0").join("omg_client.exe")),
            ..installed_copy()
        };
        assert_eq!(decide_with("omg", &lookalike, intact), Relaunch::Skip(Skip::NotInstalledByBiome));
        // An app id that would climb out of the games folder.
        assert_eq!(decide_with("../omg", &installed_copy(), intact), Relaunch::Skip(Skip::NotInstalledByBiome));
        assert_eq!(decide_with("omg", &RelaunchFacts { exe: None, ..installed_copy() }, intact), Relaunch::Skip(Skip::NotInstalledByBiome));
    }

    /// Only an install Biome's launch would start is relaunched: otherwise
    /// the game would exit and Biome would refuse to start it.
    #[test]
    fn a_broken_install_is_not_relaunched() {
        let with = |r: InstallRecord| decide_with("omg", &installed_copy(), move |_| Some(r.clone()));
        let skipped = Relaunch::Skip(Skip::NotInstalledByBiome);
        // installed.json names another app.
        assert_eq!(with(record("mecha-aether", "0.2.0", "omg_client.exe")), skipped);
        // A version Biome wouldn't accept.
        assert_eq!(with(record("omg", "../0.2.0", "omg_client.exe")), skipped);
        assert_eq!(with(record("omg", "", "omg_client.exe")), skipped);
        // An entrypoint that isn't there, or escapes the version folder.
        assert_eq!(with(record("omg", "0.2.0", "missing.exe")), skipped);
        assert_eq!(with(record("omg", "0.2.0", "../omg_client.exe")), skipped);
        assert_eq!(with(record("omg", "0.2.0", "")), skipped);
        // This program is in an older version's folder, not the installed one.
        let old = RelaunchFacts { exe: Some(root().join("games").join("omg").join("0.1.0").join("omg_client.exe")), ..installed_copy() };
        assert_eq!(decide_with("omg", &old, intact), skipped);
    }

    #[test]
    fn the_install_record_is_read_from_installed_json() {
        let dir = std::env::temp_dir().join(format!("biome-relaunch-record-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(read_install_record(&dir), None);
        std::fs::write(dir.join("installed.json"), "not json").unwrap();
        assert_eq!(read_install_record(&dir), None);
        std::fs::write(
            dir.join("installed.json"),
            r#"{"app_id":"omg","version":"0.2.0","entrypoint":"bin/omg.exe","sha256":"","installed_at":1}"#,
        )
        .unwrap();
        assert_eq!(read_install_record(&dir), Some(record("omg", "0.2.0", "bin/omg.exe")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn canonical_paths_keep_a_plain_drive_spelling() {
        let here = canonical(&std::env::current_exe().unwrap()).unwrap();
        assert!(!here.to_string_lossy().starts_with(r"\\?\"), "{}", here.display());
        assert!(here.is_file());
    }

    #[test]
    fn this_test_binary_is_not_relaunched() {
        // A debug test build, outside any games folder: always false, and
        // nothing is started.
        assert!(!restart_through_biome_if_needed("omg"));
        let expected = if cfg!(windows) { Skip::DebugBuild } else { Skip::Unsupported };
        assert_eq!(restart_through_biome("omg"), RestartOutcome::Skipped(expected));
    }

    #[test]
    fn capabilities_are_read_line_by_line() {
        let dir = std::env::temp_dir().join(format!("biome-relaunch-licenses-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("current")).unwrap();
        let launcher = dir.join("Biome.exe");
        // Biome 0.1.18 to 0.1.23: run links, no licenses.
        std::fs::write(dir.join("current").join(CAPABILITIES_FILE), "run-links\n").unwrap();
        assert!(has_capability(&launcher, RUN_LINKS) && !has_capability(&launcher, LICENSES));
        // 0.1.28 and later.
        std::fs::write(dir.join("current").join(CAPABILITIES_FILE), "run-links\nlicenses\n").unwrap();
        assert!(has_capability(&launcher, RUN_LINKS) && has_capability(&launcher, LICENSES));
        assert!(!has_capability(&launcher, "licens"), "whole lines only");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
