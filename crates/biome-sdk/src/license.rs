//! Licenses: may this Biome account run this app on this device?
//! (docs/GAME_SDK_SPEC.md §10, docs/SOFTWARE_PRODUCTS_SPEC.md §17.)
//!
//! First thing in `main`:
//!
//! ```ignore
//! let license = match biome_sdk::license::require("trillavio", env!("CARGO_PKG_VERSION")) {
//!     Ok(l) => l,
//!     Err(e) => { show_error(&e.to_string()); return; }
//! };
//! ```
//!
//! What [`require`] does:
//!
//! 1. A release build that is Biome's installed copy, started some other way
//!    (a shortcut, a double click), hands itself to Biome and **exits the
//!    process** ([`biome_online_services::relaunch`]); Biome starts it again
//!    and it comes back here. When that hand-over fails:
//!    [`LicenseError::NotStartedByBiome`].
//! 2. With Biome running, it attaches (silently for Biome's own install or
//!    a launch by Biome; any other copy gets Biome's consent prompt once)
//!    and asks for a license. Biome asks the server with the signed-in
//!    user's session; the server signs one when that account may have the
//!    product's release (an entitlement, a tester on its track, its team).
//! 3. It checks the license: Ed25519 signature by a key compiled into this
//!    build ([`keys`]), issuer, this app id, this version (the one the app
//!    passes: a license for another version, cached or fresh, doesn't run
//!    it, so an edition can't be stretched to a newer one), this device and
//!    user (the `dev` hash, [`biome_online_services::device`]), not expired.
//!    A good one is written to the app's config folder ([`cache_path`]).
//! 4. Biome not running, or it can't reach the server: the cached license is
//!    used until it expires (7 days after it was issued). A refusal from the
//!    server (not entitled any more) deletes the cache: revocation reaches a
//!    device the next time it runs with Biome, or within 7 days offline.
//!
//! What this does and doesn't stop: it ties running the app to an invited
//! Biome account and a device, and makes sharing a build or a license file
//! useless on its own. It is a check inside a native program: someone who
//! patches the program can remove it. Offline expiry trusts the computer's
//! clock (a clock set more than 5 minutes before the license was issued is
//! refused).

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use biome_online_services::protocol::license_error;
use biome_online_services::relaunch::{self, RestartOutcome};
use biome_online_services::{detect, device, transport, BiomeClient, BosConfig, BosError, ConnectOptions, Transport};

pub mod keys;
pub use keys::{LicenseKey, PRODUCTION_KEYS};

#[cfg(test)]
mod roundtrip;

/// The `iss` of every license.
pub const ISSUER: &str = "https://biome.cafe";
/// The header's `typ`.
pub const TYP: &str = "biome-license+jwt";
/// The cached license's file name inside the cache folder.
pub const CACHE_FILE: &str = "biome-license.jwt";
/// How far the clock may be behind a license's issue time.
pub const CLOCK_SKEW: Duration = Duration::from_secs(300);

/// The release track a license was issued for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Track {
    Internal,
    Beta,
    Release,
}

impl Track {
    /// INTERNAL, BETA or RELEASE, as Biome says it.
    pub fn label(self) -> &'static str {
        match self {
            Track::Internal => "INTERNAL",
            Track::Beta => "BETA",
            Track::Release => "RELEASE",
        }
    }

    fn from_claim(s: &str) -> Option<Self> {
        match s {
            "internal" => Some(Track::Internal),
            "beta" => Some(Track::Beta),
            "release" => Some(Track::Release),
            _ => None,
        }
    }
}

/// Why the account may run this release: what the license rests on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Basis {
    /// The app's team (its guild's owner and admins).
    Manager,
    /// A tester: access, not ownership. It ends with their tester place.
    Tester,
    /// An owner: a purchase or a grant whose edition covers this version.
    Owner,
    /// A free product.
    Free,
}

impl Basis {
    fn from_claim(s: &str) -> Option<Self> {
        match s {
            "manager" => Some(Basis::Manager),
            "tester" => Some(Basis::Tester),
            "owner" => Some(Basis::Owner),
            "free" => Some(Basis::Free),
            _ => None,
        }
    }
}

/// Where a [`License`] came from this time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LicenseSource {
    /// Freshly issued through the running Biome app.
    Biome,
    /// The cached one (Biome not running, or it couldn't reach the server).
    Cache,
}

/// A checked license.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct License {
    pub app_id: String,
    /// The Biome user id.
    pub user_id: String,
    pub username: String,
    /// The user's display name (their username when they have none).
    pub display_name: String,
    pub track: Track,
    /// The release the license was issued for (the version Biome installed).
    pub version: String,
    /// Why this account may run it.
    pub basis: Basis,
    /// The edition owned ("Trillavio 2026"), for an owner whose purchase or
    /// grant names one.
    pub edition: Option<String>,
    pub issued_at: SystemTime,
    pub expires_at: SystemTime,
    /// The device hash it is bound to (`dev`).
    pub device: String,
    /// The signing key's id.
    pub key_id: String,
    /// The signed token itself.
    pub token: String,
    pub source: LicenseSource,
}

impl License {
    /// For the About box.
    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    pub fn track(&self) -> Track {
        self.track
    }

    pub fn is_expired(&self) -> bool {
        SystemTime::now() >= self.expires_at
    }

    /// "licensed to Mandy · INTERNAL build", for an About box.
    pub fn about_line(&self) -> String {
        match self.track {
            Track::Release => format!("licensed to {}", self.display_name),
            t => format!("licensed to {} · {} build", self.display_name, t.label()),
        }
    }
}

/// Why there is no license. `Display` is a sentence an app can show.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum LicenseError {
    /// This is Biome's installed copy, started outside Biome, and it could
    /// not be handed to Biome.
    #[error("start this app from the Biome app")]
    NotStartedByBiome,
    /// Biome isn't running and there is no valid cached license.
    #[error("open the Biome app, sign in, and start this app again")]
    BiomeNotRunning,
    /// Biome is running but nobody is signed in (and no valid cache).
    #[error("sign in to the Biome app, then start this app again")]
    NotSignedIn,
    /// The signed-in Biome account may not have this app (or this version).
    #[error("this Biome account doesn't have this app")]
    NotEntitled,
    /// The cached license expired and Biome isn't there to renew it.
    #[error("this app's license expired: open the Biome app and start it again")]
    Expired,
    /// A license that doesn't check out (signature, app, device, clock), or
    /// no key to check it with.
    #[error("this app's license is not valid: {0}")]
    Invalid(String),
    /// The account owns an edition of the app, not the one this version
    /// belongs to (the editions it owns, maybe none named).
    #[error("{}", needs_upgrade_line(.0))]
    NeedsUpgrade(Vec<String>),
    /// The running Biome is older than 0.1.28 and doesn't issue licenses.
    #[error("update the Biome app (0.1.28 or later), then start this app again")]
    BiomeOutdated,
    /// Biome is running but couldn't get a license right now (its server,
    /// the network), and there is no valid cache.
    #[error("couldn't get a license from Biome right now: {0}")]
    Unavailable(String),
}

/// How [`require_with`] works; [`Default`] is what [`require`] uses.
#[derive(Clone, Debug)]
pub struct RequireOptions {
    /// Hand Biome's installed copy to Biome when it was started outside it
    /// (release builds only; see [`relaunch`]). Default `true`.
    pub relaunch: bool,
    /// Where the license is cached. Default: [`default_cache_dir`].
    pub cache_dir: Option<PathBuf>,
    /// How to reach Biome. Default: the running desktop app.
    pub transport: Transport,
    /// What the app calls itself in Biome's logs (the consent prompt shows
    /// the registered name). Default: the app id.
    pub app_name: Option<String>,
    /// Sent with an attach and logged by Biome.
    pub client_version: String,
    /// The build's own version (`env!("CARGO_PKG_VERSION")`, as [`require`]
    /// passes it): Biome asks for a license for this version, and a license
    /// for any other is refused. `None` skips that check (tests only).
    pub version: Option<String>,
    /// How long the player has to answer Biome's consent prompt (only a copy
    /// Biome didn't install gets one). Default 120 s.
    pub consent_timeout: Duration,
}

impl Default for RequireOptions {
    fn default() -> Self {
        Self {
            relaunch: true,
            cache_dir: None,
            transport: Transport::Production,
            app_name: None,
            client_version: String::new(),
            version: None,
            consent_timeout: Duration::from_secs(120),
        }
    }
}

/// The license for `app_id` at `version` (this build's own version:
/// `env!("CARGO_PKG_VERSION")`): through the running Biome app, else the
/// cached one. May exit the process (a release build started outside
/// Biome, handed to Biome). See the module docs.
pub fn require(app_id: &str, version: &str) -> Result<License, LicenseError> {
    require_with(
        app_id,
        &RequireOptions { version: Some(version.to_string()), client_version: version.to_string(), ..RequireOptions::default() },
    )
}

/// [`require`] with options.
pub fn require_with(app_id: &str, opts: &RequireOptions) -> Result<License, LicenseError> {
    if opts.relaunch {
        match relaunch::restart_through_biome(app_id) {
            RestartOutcome::Relaunched => std::process::exit(0),
            RestartOutcome::Failed(why) => {
                tracing::warn!(app_id, "could not start through Biome: {why}");
                return Err(LicenseError::NotStartedByBiome);
            }
            RestartOutcome::Skipped(_) => {}
        }
    }
    let cache = cache_file(app_id, opts);
    let endpoint = match &opts.transport {
        Transport::Custom { path } => path.clone(),
        _ => transport::default_endpoint(),
    };
    let version = opts.version.as_deref();
    if !detect::endpoint_accepts(&endpoint) {
        return from_cache(app_id, version, cache.as_deref()).map_err(|e| match e {
            CacheMiss::None => LicenseError::BiomeNotRunning,
            CacheMiss::Expired => LicenseError::Expired,
        });
    }

    let config = BosConfig::new(app_id, opts.app_name.clone().unwrap_or_else(|| app_id.to_string()))
        .with_transport(opts.transport.clone());
    let connect = ConnectOptions {
        consent_timeout: opts.consent_timeout,
        client_version: opts.client_version.clone(),
        ..ConnectOptions::default()
    };
    let client = match BiomeClient::connect_with(config, connect) {
        Ok(c) => c,
        Err(e) => return after_failure(app_id, version, cache.as_deref(), connect_failure(e, &opts.transport)),
    };
    let result = license_through(&client, app_id, version, &opts.transport);
    finish(app_id, version, cache.as_deref(), result)
}

/// The license for the app `client` is connected as, at `version` (this
/// build's own), through that connection (for an app that keeps its own
/// [`BiomeClient`]); verified and cached like [`require`]'s, falling back to
/// the cache the same way. Never relaunches.
pub fn require_through(client: &BiomeClient, version: &str, cache_dir: Option<&Path>) -> Result<License, LicenseError> {
    let app_id = client.app_id().to_string();
    let opts = RequireOptions { cache_dir: cache_dir.map(Path::to_path_buf), ..RequireOptions::default() };
    let cache = cache_file(&app_id, &opts);
    let result = license_through(client, &app_id, Some(version), &Transport::Production);
    finish(&app_id, Some(version), cache.as_deref(), result)
}

/// Check a license token for `app_id` at `version` (this build's; `None`
/// skips that) on this device, now, against the keys compiled into this
/// build. (What [`require`] does with every license.)
pub fn verify(token: &str, app_id: &str, version: Option<&str>) -> Result<License, LicenseError> {
    verify_with(token, app_id, &trusted_keys(), &device::device_hash, SystemTime::now()).and_then(|l| for_version(l, version))
}

/// The cached license for `app_id` at `version`, if it is still valid on this device.
pub fn cached(app_id: &str, version: &str) -> Option<License> {
    from_cache(app_id, Some(version), cache_file(app_id, &RequireOptions::default()).as_deref()).ok()
}

/// A license issued for another version doesn't run this one: an owner of
/// one edition can't stretch its license (fresh or cached) to a newer build.
fn for_version(l: License, version: Option<&str>) -> Result<License, LicenseError> {
    match version {
        Some(v) if l.version != v => Err(LicenseError::Invalid(format!("issued for version {}, and this is {v}", l.version))),
        _ => Ok(l),
    }
}

/// Delete the cached license (signing out of an app, say).
pub fn forget(app_id: &str) {
    if let Some(path) = cache_file(app_id, &RequireOptions::default()) {
        let _ = std::fs::remove_file(path);
    }
}

/// The app's config folder: `%APPDATA%\<app_id>` on Windows,
/// `~/Library/Application Support/<app_id>` on macOS,
/// `$XDG_CONFIG_HOME/<app_id>` (or `~/.config/<app_id>`) elsewhere.
pub fn default_cache_dir(app_id: &str) -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library").join("Application Support"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    }?;
    Some(base.join(app_id))
}

/// Where [`require`] caches the license for `app_id`.
pub fn cache_path(app_id: &str) -> Option<PathBuf> {
    cache_file(app_id, &RequireOptions::default())
}

// ── internals ───────────────────────────────────────────────────────────────

/// What the attempt through Biome came to.
enum Through {
    Got(License),
    /// The server said no: the cache goes too.
    Refused(LicenseError),
    /// Nobody signed in: the cache may stand in.
    SignedOut,
    /// No answer this time: the cache may stand in.
    NoAnswer(LicenseError),
}

fn license_through(client: &BiomeClient, app_id: &str, version: Option<&str>, transport: &Transport) -> Through {
    match client.license(version).wait() {
        Ok(grant) => match verify(&grant.token, app_id, version) {
            Ok(mut l) => {
                l.source = LicenseSource::Biome;
                Through::Got(l)
            }
            // Biome handed over something this build can't check (most
            // likely a key this build doesn't have): no cache would help.
            Err(e) => Through::Refused(e),
        },
        Err(BosError::Request(code)) if license_error::parse_needs_upgrade(&code).is_some() => {
            Through::Refused(LicenseError::NeedsUpgrade(license_error::parse_needs_upgrade(&code).unwrap_or_default()))
        }
        Err(BosError::Request(code)) => match code.as_str() {
            license_error::NOT_ENTITLED => Through::Refused(LicenseError::NotEntitled),
            license_error::NOT_SIGNED_IN => Through::SignedOut,
            license_error::NO_DEVICE => Through::Refused(LicenseError::Invalid("Biome couldn't identify this device".into())),
            license_error::UNAVAILABLE => Through::NoAnswer(LicenseError::Unavailable("Biome couldn't reach its server".into())),
            other => Through::NoAnswer(LicenseError::Unavailable(other.to_string())),
        },
        // An older Biome can't decode the request and hangs up.
        Err(e) => Through::NoAnswer(outdated_or(transport, LicenseError::Unavailable(e.to_string()))),
    }
}

fn connect_failure(e: BosError, transport: &Transport) -> Through {
    match e {
        BosError::NotSignedIn => Through::SignedOut,
        // Not registered, or turned off: the kill switch.
        BosError::UnknownApp => Through::Refused(LicenseError::NotEntitled),
        BosError::ConsentDenied => Through::NoAnswer(LicenseError::Unavailable(
            "you didn't allow this app to use your Biome account (Biome remembers that answer)".into(),
        )),
        BosError::PlatformUnavailable => Through::NoAnswer(LicenseError::BiomeNotRunning),
        other => Through::NoAnswer(outdated_or(transport, LicenseError::Unavailable(other.to_string()))),
    }
}

/// [`LicenseError::BiomeOutdated`] when the installed Biome says it doesn't
/// issue licenses, else `fallback`.
fn outdated_or(transport: &Transport, fallback: LicenseError) -> LicenseError {
    match transport {
        Transport::Production if relaunch::installed_biome_issues_licenses() == Some(false) => LicenseError::BiomeOutdated,
        _ => fallback,
    }
}

fn finish(app_id: &str, version: Option<&str>, cache: Option<&Path>, result: Through) -> Result<License, LicenseError> {
    match result {
        Through::Got(l) => {
            if let Some(path) = cache {
                if let Err(e) = write_cache(path, &l.token) {
                    tracing::warn!(app_id, "could not cache the license at {}: {e}", path.display());
                }
            }
            Ok(l)
        }
        other => after_failure(app_id, version, cache, other),
    }
}

fn after_failure(app_id: &str, version: Option<&str>, cache: Option<&Path>, result: Through) -> Result<License, LicenseError> {
    match result {
        Through::Got(l) => Ok(l),
        Through::Refused(e) => {
            if let Some(path) = cache {
                let _ = std::fs::remove_file(path);
            }
            Err(e)
        }
        Through::SignedOut => from_cache(app_id, version, cache).map_err(|_| LicenseError::NotSignedIn),
        Through::NoAnswer(e) => from_cache(app_id, version, cache).map_err(|miss| match miss {
            CacheMiss::Expired if e == LicenseError::BiomeNotRunning => LicenseError::Expired,
            _ => e,
        }),
    }
}

enum CacheMiss {
    None,
    Expired,
}

fn from_cache(app_id: &str, version: Option<&str>, cache: Option<&Path>) -> Result<License, CacheMiss> {
    let token = cache.and_then(|p| std::fs::read_to_string(p).ok()).ok_or(CacheMiss::None)?;
    match verify(token.trim(), app_id, version) {
        Ok(mut l) => {
            l.source = LicenseSource::Cache;
            Ok(l)
        }
        Err(LicenseError::Expired) => Err(CacheMiss::Expired),
        Err(e) => {
            tracing::warn!(app_id, "ignoring the cached license: {e}");
            Err(CacheMiss::None)
        }
    }
}

fn cache_file(app_id: &str, opts: &RequireOptions) -> Option<PathBuf> {
    let dir = opts.cache_dir.clone().or_else(|| default_cache_dir(app_id))?;
    Some(dir.join(CACHE_FILE))
}

fn write_cache(path: &Path, token: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("jwt.tmp");
    std::fs::write(&tmp, token)?;
    std::fs::rename(&tmp, path)
}

/// Every key this build trusts: the production keys, and a local key given
/// at build time.
pub(crate) fn trusted_keys() -> Vec<(String, String)> {
    let mut keys: Vec<(String, String)> = PRODUCTION_KEYS.iter().map(|k| (k.kid.to_string(), k.x.to_string())).collect();
    if let Some(dev) = keys::dev_key() {
        keys.push(dev);
    }
    #[cfg(test)]
    keys.extend(tests::TEST_KEYS.lock().unwrap().iter().cloned());
    keys
}

#[derive(serde::Deserialize)]
struct Header {
    alg: String,
    #[serde(default)]
    typ: Option<String>,
    kid: String,
}

#[derive(serde::Deserialize)]
struct Claims {
    iss: String,
    sub: String,
    aud: String,
    #[serde(default)]
    username: String,
    #[serde(default)]
    name: String,
    track: String,
    ver: String,
    dev: String,
    basis: String,
    #[serde(default)]
    edition: Option<String>,
    iat: u64,
    exp: u64,
}

/// [`verify`] with explicit keys (`(kid, x)`), device hash and clock.
pub(crate) fn verify_with(
    token: &str,
    app_id: &str,
    keys: &[(String, String)],
    device_of: &dyn Fn(&str) -> Option<String>,
    now: SystemTime,
) -> Result<License, LicenseError> {
    let invalid = |why: &str| LicenseError::Invalid(why.to_string());
    let mut parts = token.split('.');
    let (Some(h), Some(p), Some(s), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return Err(invalid("not a signed token"));
    };
    let header: Header = decode_part(h).ok_or_else(|| invalid("unreadable header"))?;
    if header.alg != "EdDSA" || header.typ.as_deref().is_some_and(|t| t != TYP) {
        return Err(invalid("not a Biome license"));
    }
    if keys.is_empty() {
        return Err(invalid("this build has no license keys (see biome_sdk::license::keys)"));
    }
    let Some((_, x)) = keys.iter().find(|(kid, _)| *kid == header.kid) else {
        return Err(LicenseError::Invalid(format!("signed with key {:?}, which this build doesn't know", header.kid)));
    };
    let public = URL_SAFE_NO_PAD.decode(x).map_err(|_| invalid("a bad key in this build"))?;
    let sig = URL_SAFE_NO_PAD.decode(s).map_err(|_| invalid("unreadable signature"))?;
    let message = format!("{h}.{p}");
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &public)
        .verify(message.as_bytes(), &sig)
        .map_err(|_| invalid("the signature doesn't match"))?;

    // Signed by Biome: now what it says.
    let claims: Claims = decode_part(p).ok_or_else(|| invalid("unreadable claims"))?;
    if claims.iss != ISSUER {
        return Err(invalid("not issued by Biome"));
    }
    if claims.aud != app_id {
        return Err(LicenseError::Invalid(format!("issued for {:?}, not {app_id:?}", claims.aud)));
    }
    let track = Track::from_claim(&claims.track).ok_or_else(|| invalid("unknown track"))?;
    let basis = Basis::from_claim(&claims.basis).ok_or_else(|| invalid("unknown basis"))?;
    match device_of(&claims.sub) {
        Some(d) if d == claims.dev => {}
        Some(_) => return Err(invalid("issued for another device or account")),
        None => return Err(invalid("this device can't be identified")),
    }
    let issued_at = UNIX_EPOCH + Duration::from_secs(claims.iat);
    let expires_at = UNIX_EPOCH + Duration::from_secs(claims.exp);
    if now + CLOCK_SKEW < issued_at {
        return Err(invalid("this computer's clock is behind the license's date"));
    }
    if now >= expires_at {
        return Err(LicenseError::Expired);
    }
    let display_name = if claims.name.trim().is_empty() { claims.username.clone() } else { claims.name.clone() };
    Ok(License {
        app_id: claims.aud,
        user_id: claims.sub,
        username: claims.username,
        display_name,
        track,
        version: claims.ver,
        basis,
        edition: claims.edition.filter(|e| !e.trim().is_empty()),
        issued_at,
        expires_at,
        device: claims.dev,
        key_id: header.kid,
        token: token.to_string(),
        source: LicenseSource::Cache,
    })
}

/// [`LicenseError::NeedsUpgrade`]'s sentence.
fn needs_upgrade_line(editions: &[String]) -> String {
    match editions {
        [] => "this version is a newer edition than this Biome account owns".to_string(),
        owned => format!("this version is a newer edition: this Biome account owns {}", owned.join(", ")),
    }
}

fn decode_part<T: serde::de::DeserializeOwned>(part: &str) -> Option<T> {
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(part).ok()?).ok()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ring::signature::{Ed25519KeyPair, KeyPair};

    pub(crate) struct Signer {
        pub kid: String,
        pair: Ed25519KeyPair,
    }

    impl Signer {
        pub fn new(kid: &str) -> Self {
            let pkcs8 = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap();
            Signer { kid: kid.into(), pair: Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap() }
        }

        pub fn key(&self) -> (String, String) {
            (self.kid.clone(), URL_SAFE_NO_PAD.encode(self.pair.public_key().as_ref()))
        }

        pub fn sign(&self, header: serde_json::Value, claims: serde_json::Value) -> String {
            let h = URL_SAFE_NO_PAD.encode(header.to_string());
            let p = URL_SAFE_NO_PAD.encode(claims.to_string());
            let sig = self.pair.sign(format!("{h}.{p}").as_bytes());
            format!("{h}.{p}.{}", URL_SAFE_NO_PAD.encode(sig.as_ref()))
        }

        pub fn license(&self, claims: serde_json::Value) -> String {
            self.sign(serde_json::json!({"alg": "EdDSA", "typ": TYP, "kid": self.kid}), claims)
        }
    }

    /// Keys the crate's own tests trust on top of the compiled ones.
    pub(crate) static TEST_KEYS: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());

    pub(crate) const USER: &str = "11111111-1111-1111-1111-111111111111";
    pub(crate) const NOW: u64 = 1_800_000_000;

    pub(crate) fn claims(app: &str, dev: &str) -> serde_json::Value {
        serde_json::json!({
            "iss": ISSUER, "sub": USER, "aud": app, "username": "mandy", "name": "Mandy Moore",
            "track": "internal", "ver": "0.4.2", "dev": dev, "jti": "j", "basis": "tester", "iat": NOW, "exp": NOW + 7 * 86400,
        })
    }

    fn here(user: &str) -> Option<String> {
        Some(device::device_hash_for(user, "this-machine"))
    }

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn a_good_license_checks_out() {
        let s = Signer::new("k1");
        let token = s.license(claims("trillavio", &here(USER).unwrap()));
        let l = verify_with(&token, "trillavio", &[s.key()], &here, at(NOW + 60)).unwrap();
        assert_eq!(l.app_id, "trillavio");
        assert_eq!(l.user_id, USER);
        assert_eq!((l.username.as_str(), l.display_name()), ("mandy", "Mandy Moore"));
        assert_eq!(l.track(), Track::Internal);
        assert_eq!(l.version, "0.4.2");
        assert_eq!((l.basis, l.edition.as_deref()), (Basis::Tester, None));
        assert_eq!(l.key_id, "k1");
        assert_eq!(l.expires_at, at(NOW + 7 * 86400));
        assert_eq!(l.about_line(), "licensed to Mandy Moore · INTERNAL build");
        // One of several keys (rotation).
        let other = Signer::new("k0");
        assert!(verify_with(&token, "trillavio", &[other.key(), s.key()], &here, at(NOW)).is_ok());
    }

    #[test]
    fn everything_that_doesnt_check_out_is_refused() {
        let s = Signer::new("k1");
        let dev = here(USER).unwrap();
        let ok = |t: &str, app: &str, now: u64| verify_with(t, app, &[s.key()], &here, at(now));
        let good = s.license(claims("trillavio", &dev));

        // Another app's license.
        assert!(matches!(ok(&good, "other-app", NOW), Err(LicenseError::Invalid(_))));
        // Expired, and right at the edge.
        assert_eq!(ok(&good, "trillavio", NOW + 7 * 86400), Err(LicenseError::Expired));
        assert!(ok(&good, "trillavio", NOW + 7 * 86400 - 1).is_ok());
        // A clock set back before it was issued.
        assert!(matches!(ok(&good, "trillavio", NOW - 3600), Err(LicenseError::Invalid(_))));
        assert!(ok(&good, "trillavio", NOW - 60).is_ok(), "a little skew is fine");
        // Copied to another machine, or another user on this one.
        let elsewhere = |u: &str| Some(device::device_hash_for(u, "another-machine"));
        assert!(matches!(verify_with(&good, "trillavio", &[s.key()], &elsewhere, at(NOW)), Err(LicenseError::Invalid(_))));
        let mut c = claims("trillavio", &dev);
        c["sub"] = "22222222-2222-2222-2222-222222222222".into();
        assert!(matches!(ok(&s.license(c), "trillavio", NOW), Err(LicenseError::Invalid(_))));
        // A device that can't be identified.
        assert!(matches!(verify_with(&good, "trillavio", &[s.key()], &|_| None, at(NOW)), Err(LicenseError::Invalid(_))));
        // A tampered payload (a longer expiry) under the old signature.
        let mut parts: Vec<String> = good.split('.').map(str::to_string).collect();
        let mut longer = claims("trillavio", &dev);
        longer["exp"] = (NOW + 365 * 86400).into();
        parts[1] = URL_SAFE_NO_PAD.encode(longer.to_string());
        assert!(matches!(ok(&parts.join("."), "trillavio", NOW), Err(LicenseError::Invalid(_))));
        // Signed by a key this build doesn't know, under a known kid or not.
        let stranger = Signer::new("k1");
        assert!(matches!(ok(&stranger.license(claims("trillavio", &dev)), "trillavio", NOW), Err(LicenseError::Invalid(_))));
        let unknown = Signer::new("k9");
        let e = ok(&unknown.license(claims("trillavio", &dev)), "trillavio", NOW).unwrap_err();
        assert!(e.to_string().contains("k9"), "{e}");
        // No keys at all (a build before the production key exists).
        let e = verify_with(&good, "trillavio", &[], &here, at(NOW)).unwrap_err();
        assert!(e.to_string().contains("no license keys"), "{e}");
        // Not a license: another alg or typ, another issuer, a game ticket shape.
        let es = s.sign(serde_json::json!({"alg": "ES256", "typ": TYP, "kid": "k1"}), claims("trillavio", &dev));
        assert!(matches!(ok(&es, "trillavio", NOW), Err(LicenseError::Invalid(_))));
        let ticket = s.sign(serde_json::json!({"alg": "EdDSA", "typ": "biome-game-ticket+jwt", "kid": "k1"}), claims("trillavio", &dev));
        assert!(matches!(ok(&ticket, "trillavio", NOW), Err(LicenseError::Invalid(_))));
        let mut c = claims("trillavio", &dev);
        c["iss"] = "https://evil.example".into();
        assert!(matches!(ok(&s.license(c), "trillavio", NOW), Err(LicenseError::Invalid(_))));
        let mut c = claims("trillavio", &dev);
        c["track"] = "nightly".into();
        assert!(matches!(ok(&s.license(c), "trillavio", NOW), Err(LicenseError::Invalid(_))));
        // Every license says why; one that doesn't (or says something else) isn't one.
        for basis in [serde_json::Value::Null, "friend".into(), 3.into()] {
            let mut c = claims("trillavio", &dev);
            c["basis"] = basis.clone();
            assert!(matches!(ok(&s.license(c), "trillavio", NOW), Err(LicenseError::Invalid(_))), "{basis}");
        }
        for junk in ["", "a.b", "a.b.c.d", "!!.!!.!!"] {
            assert!(matches!(ok(junk, "trillavio", NOW), Err(LicenseError::Invalid(_))), "{junk}");
        }
    }

    #[test]
    fn the_display_name_falls_back_to_the_username_and_release_says_nothing() {
        let s = Signer::new("k1");
        let mut c = claims("trillavio", &here(USER).unwrap());
        c["name"] = " ".into();
        c["track"] = "release".into();
        let l = verify_with(&s.license(c), "trillavio", &[s.key()], &here, at(NOW)).unwrap();
        assert_eq!(l.display_name(), "mandy");
        assert_eq!(l.about_line(), "licensed to mandy");
        assert_eq!(Track::Beta.label(), "BETA");
    }

    #[test]
    fn an_owner_carries_their_edition_and_an_upgrade_names_it() {
        let s = Signer::new("k1");
        let mut c = claims("trillavio", &here(USER).unwrap());
        c["basis"] = "owner".into();
        c["edition"] = "Trillavio 2026".into();
        c["track"] = "release".into();
        let l = verify_with(&s.license(c), "trillavio", &[s.key()], &here, at(NOW)).unwrap();
        assert_eq!((l.basis, l.edition.as_deref()), (Basis::Owner, Some("Trillavio 2026")));
        assert_eq!(
            LicenseError::NeedsUpgrade(vec!["Trillavio 2026".into()]).to_string(),
            "this version is a newer edition: this Biome account owns Trillavio 2026"
        );
        assert_eq!(LicenseError::NeedsUpgrade(vec![]).to_string(), "this version is a newer edition than this Biome account owns");
    }

    #[test]
    fn cache_dirs_are_per_app() {
        let d = default_cache_dir("trillavio");
        if let Some(d) = d {
            assert!(d.ends_with("trillavio"), "{}", d.display());
        }
        let opts = RequireOptions { cache_dir: Some(PathBuf::from("x")), ..RequireOptions::default() };
        assert_eq!(cache_file("trillavio", &opts), Some(PathBuf::from("x").join(CACHE_FILE)));
    }
}
