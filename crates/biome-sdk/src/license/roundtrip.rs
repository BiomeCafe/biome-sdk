//! `require_with` end to end over the real OS transport: a `BosServer` with
//! a scripted backend (Biome's side: it hands out licenses signed with a
//! test key, or refuses) and the SDK on the other side, with a cache folder
//! per test.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use biome_online_services::error::BosError;
use biome_online_services::protocol::license_error;
use biome_online_services::server::{BosBackend, BosServer, ConnContext, ServerConfig};
use biome_online_services::types::{ConnectInfo, LicenseGrant, MatchId, Session};
use biome_online_services::{device, Transport};

use super::tests::{Signer, TEST_KEYS, USER};
use super::*;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

fn unique_endpoint() -> String {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    #[cfg(windows)]
    {
        format!(r"\\.\pipe\biome-sdk-license-test-{}-{n}", std::process::id())
    }
    #[cfg(unix)]
    {
        std::env::temp_dir().join(format!("biome-sdk-license-{}-{n}.sock", std::process::id())).to_string_lossy().into_owned()
    }
}

fn temp_dir() -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("biome-sdk-license-cache-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// The one signer the tests' Biome uses, trusted by this test build.
fn signer() -> Arc<Signer> {
    static SIGNER: std::sync::OnceLock<Arc<Signer>> = std::sync::OnceLock::new();
    SIGNER
        .get_or_init(|| {
            let s = Arc::new(Signer::new("roundtrip-key"));
            TEST_KEYS.lock().unwrap().push(s.key());
            s
        })
        .clone()
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

/// The version the tests' builds say they are.
const VERSION: &str = "1.0.0";

/// A license as the server signs it, for this machine, issued `age` ago.
fn license_token(app: &str, age: Duration) -> String {
    license_token_for(app, VERSION, age)
}

fn license_token_for(app: &str, ver: &str, age: Duration) -> String {
    let iat = now_secs() - age.as_secs();
    signer().license(serde_json::json!({
        "iss": ISSUER, "sub": USER, "aud": app, "username": "mandy", "name": "Mandy",
        "track": "beta", "ver": ver, "dev": device::device_hash(USER).unwrap(), "jti": "j", "basis": "tester",
        "iat": iat, "exp": iat + 7 * 86400,
    }))
}

#[derive(Clone)]
enum Answer {
    Grant,
    Refuse(&'static str),
}

struct FakeBiome {
    signed_in: bool,
    answer: Answer,
    asked: Arc<Mutex<Vec<String>>>,
}

impl BosBackend for FakeBiome {
    async fn match_connect_info(&self, _m: MatchId) -> Result<ConnectInfo, BosError> {
        Err(BosError::Internal("n/a".into()))
    }
    async fn session(&self) -> Option<Session> {
        self.signed_in.then(|| Session { user_id: uuid::Uuid::parse_str(USER).unwrap(), username: "mandy".into(), entitlements: vec![] })
    }
    async fn license(&self, ctx: ConnContext, version: Option<String>) -> Result<LicenseGrant, String> {
        // As Biome's host: it asks the server for the version the build says it is.
        let version = version.unwrap_or_else(|| "installed".into());
        self.asked.lock().unwrap().push(format!("{}@{version}", ctx.app_id));
        // As Biome's host: nobody signed in, no license (a dev-mode server
        // lets the attach in signed out).
        if !self.signed_in {
            return Err(license_error::NOT_SIGNED_IN.to_string());
        }
        match &self.answer {
            Answer::Grant => Ok(LicenseGrant {
                token: license_token_for(&ctx.app_id, &version, Duration::from_secs(5)),
                expires_at: SystemTime::now() + Duration::from_secs(7 * 86400),
                app_id: ctx.app_id,
            }),
            Answer::Refuse(code) => Err(code.to_string()),
        }
    }
}

struct ServerGuard {
    stop: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        self.stop.take();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Biome running at `endpoint` (dev mode: attaches are let in, as Biome's
/// own installs are).
fn biome(endpoint: &str, backend: FakeBiome) -> ServerGuard {
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
    let endpoint = endpoint.to_string();
    let thread = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let server = BosServer::start(ServerConfig { endpoint: Some(endpoint), dev_mode: true, verbose: false }, Arc::new(backend))
                .await
                .expect("server start");
            let _ = ready_tx.send(());
            let _ = tokio::task::spawn_blocking(move || stop_rx.recv()).await;
            server.shutdown().await;
        });
    });
    ready_rx.recv_timeout(Duration::from_secs(5)).expect("server ready");
    ServerGuard { stop: Some(stop_tx), thread: Some(thread) }
}

fn opts(endpoint: &str, cache: &Path) -> RequireOptions {
    RequireOptions {
        relaunch: false,
        cache_dir: Some(cache.to_path_buf()),
        transport: Transport::Custom { path: endpoint.into() },
        consent_timeout: Duration::from_secs(5),
        version: Some(VERSION.into()),
        ..RequireOptions::default()
    }
}

fn fake(signed_in: bool, answer: Answer) -> (FakeBiome, Arc<Mutex<Vec<String>>>) {
    let asked = Arc::new(Mutex::new(Vec::new()));
    (FakeBiome { signed_in, answer, asked: asked.clone() }, asked)
}

#[test]
fn biome_hands_out_a_license_that_is_checked_and_cached() {
    let ep = unique_endpoint();
    let cache = temp_dir();
    let (backend, asked) = fake(true, Answer::Grant);
    let _biome = biome(&ep, backend);
    let l = require_with("trillavio", &opts(&ep, &cache)).unwrap();
    assert_eq!(l.source, LicenseSource::Biome);
    assert_eq!((l.app_id.as_str(), l.user_id.as_str(), l.display_name()), ("trillavio", USER, "Mandy"));
    assert_eq!(l.track, Track::Beta);
    assert_eq!(*asked.lock().unwrap(), ["trillavio@1.0.0"], "Biome asked for the build's own version");
    let on_disk = std::fs::read_to_string(cache.join(CACHE_FILE)).unwrap();
    assert_eq!(on_disk, l.token);
    let _ = std::fs::remove_dir_all(&cache);
}

#[test]
fn without_biome_the_cache_is_used_until_it_expires() {
    let ep = unique_endpoint(); // nothing listens here
    let cache = temp_dir();
    // Nothing cached: Biome must be running.
    assert_eq!(require_with("trillavio", &opts(&ep, &cache)), Err(LicenseError::BiomeNotRunning));
    // A good cached license.
    write_cache(&cache.join(CACHE_FILE), &license_token("trillavio", Duration::from_secs(3 * 86400))).unwrap();
    let l = require_with("trillavio", &opts(&ep, &cache)).unwrap();
    assert_eq!(l.source, LicenseSource::Cache);
    // Eight days old: expired.
    write_cache(&cache.join(CACHE_FILE), &license_token("trillavio", Duration::from_secs(8 * 86400))).unwrap();
    assert_eq!(require_with("trillavio", &opts(&ep, &cache)), Err(LicenseError::Expired));
    // Another app's license in this app's folder doesn't count.
    write_cache(&cache.join(CACHE_FILE), &license_token("other-app", Duration::from_secs(60))).unwrap();
    assert_eq!(require_with("trillavio", &opts(&ep, &cache)), Err(LicenseError::BiomeNotRunning));
    let _ = std::fs::remove_dir_all(&cache);
}

#[test]
fn a_refusal_from_biome_wins_over_the_cache_and_deletes_it() {
    let ep = unique_endpoint();
    let cache = temp_dir();
    write_cache(&cache.join(CACHE_FILE), &license_token("trillavio", Duration::from_secs(60))).unwrap();
    let (backend, _) = fake(true, Answer::Refuse(license_error::NOT_ENTITLED));
    let _biome = biome(&ep, backend);
    assert_eq!(require_with("trillavio", &opts(&ep, &cache)), Err(LicenseError::NotEntitled));
    assert!(!cache.join(CACHE_FILE).exists(), "revoked: the cached license is gone");
    let _ = std::fs::remove_dir_all(&cache);
}

#[test]
fn a_newer_edition_than_the_one_owned_is_an_upgrade_and_drops_the_cache() {
    let ep = unique_endpoint();
    let cache = temp_dir();
    write_cache(&cache.join(CACHE_FILE), &license_token("trillavio", Duration::from_secs(60))).unwrap();
    let (backend, _) = fake(true, Answer::Refuse("needs-upgrade\nTrillavio 2026"));
    let _biome = biome(&ep, backend);
    assert_eq!(
        require_with("trillavio", &opts(&ep, &cache)),
        Err(LicenseError::NeedsUpgrade(vec!["Trillavio 2026".to_string()]))
    );
    assert!(!cache.join(CACHE_FILE).exists(), "an older version's license doesn't run this one");
    let _ = std::fs::remove_dir_all(&cache);
}

#[test]
fn when_biome_cant_reach_its_server_the_cache_stands_in() {
    let ep = unique_endpoint();
    let cache = temp_dir();
    let (backend, _) = fake(true, Answer::Refuse(license_error::UNAVAILABLE));
    let _biome = biome(&ep, backend);
    assert!(matches!(require_with("trillavio", &opts(&ep, &cache)), Err(LicenseError::Unavailable(_))));
    write_cache(&cache.join(CACHE_FILE), &license_token("trillavio", Duration::from_secs(60))).unwrap();
    assert_eq!(require_with("trillavio", &opts(&ep, &cache)).unwrap().source, LicenseSource::Cache);
    let _ = std::fs::remove_dir_all(&cache);
}

#[test]
fn signed_out_of_biome_is_not_signed_in_unless_a_license_is_cached() {
    let ep = unique_endpoint();
    let cache = temp_dir();
    let (backend, asked) = fake(false, Answer::Grant);
    let _biome = biome(&ep, backend);
    assert_eq!(require_with("trillavio", &opts(&ep, &cache)), Err(LicenseError::NotSignedIn));
    write_cache(&cache.join(CACHE_FILE), &license_token("trillavio", Duration::from_secs(60))).unwrap();
    assert_eq!(require_with("trillavio", &opts(&ep, &cache)).unwrap().source, LicenseSource::Cache);
    let _ = asked;
    let _ = std::fs::remove_dir_all(&cache);
}

#[test]
fn a_license_this_build_cant_check_is_invalid_and_not_cached() {
    let ep = unique_endpoint();
    let cache = temp_dir();
    // A Biome whose server signs with a key this build doesn't have.
    struct Rogue;
    impl BosBackend for Rogue {
        async fn match_connect_info(&self, _m: MatchId) -> Result<ConnectInfo, BosError> {
            Err(BosError::Internal("n/a".into()))
        }
        async fn session(&self) -> Option<Session> {
            Some(Session { user_id: uuid::Uuid::parse_str(USER).unwrap(), username: "mandy".into(), entitlements: vec![] })
        }
        async fn license(&self, ctx: ConnContext, _version: Option<String>) -> Result<LicenseGrant, String> {
            let s = Signer::new("unknown-key");
            Ok(LicenseGrant { token: s.license(super::tests::claims(&ctx.app_id, "x")), expires_at: SystemTime::now(), app_id: ctx.app_id })
        }
    }
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
    let ep2 = ep.clone();
    let t = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let server = BosServer::start(ServerConfig { endpoint: Some(ep2), dev_mode: true, verbose: false }, Arc::new(Rogue)).await.unwrap();
            let _ = ready_tx.send(());
            let _ = tokio::task::spawn_blocking(move || stop_rx.recv()).await;
            server.shutdown().await;
        });
    });
    ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let e = require_with("trillavio", &opts(&ep, &cache)).unwrap_err();
    assert!(matches!(&e, LicenseError::Invalid(why) if why.contains("unknown-key")), "{e}");
    assert!(!cache.join(CACHE_FILE).exists());
    drop(stop_tx);
    let _ = t.join();
    let _ = std::fs::remove_dir_all(&cache);
}

#[test]
fn a_biome_that_doesnt_issue_licenses_is_unavailable_and_the_cache_still_works() {
    // The default backend (a host from before licenses answers the same
    // way; a 0.1.23 host hangs up instead, which is also no answer).
    struct Old;
    impl BosBackend for Old {
        async fn match_connect_info(&self, _m: MatchId) -> Result<ConnectInfo, BosError> {
            Err(BosError::Internal("n/a".into()))
        }
        async fn session(&self) -> Option<Session> {
            Some(Session { user_id: uuid::Uuid::parse_str(USER).unwrap(), username: "mandy".into(), entitlements: vec![] })
        }
    }
    let ep = unique_endpoint();
    let cache = temp_dir();
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
    let ep2 = ep.clone();
    let t = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let server = BosServer::start(ServerConfig { endpoint: Some(ep2), dev_mode: true, verbose: false }, Arc::new(Old)).await.unwrap();
            let _ = ready_tx.send(());
            let _ = tokio::task::spawn_blocking(move || stop_rx.recv()).await;
            server.shutdown().await;
        });
    });
    ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(matches!(require_with("trillavio", &opts(&ep, &cache)), Err(LicenseError::Unavailable(_))));
    write_cache(&cache.join(CACHE_FILE), &license_token("trillavio", Duration::from_secs(60))).unwrap();
    assert_eq!(require_with("trillavio", &opts(&ep, &cache)).unwrap().source, LicenseSource::Cache);
    drop(stop_tx);
    let _ = t.join();
    let _ = std::fs::remove_dir_all(&cache);
}

#[test]
fn another_versions_license_doesnt_run_this_build() {
    // An owner of 1.0.0 runs a 2.0.0 build: the 1.0.0 license, cached or
    // handed over, doesn't run it.
    let ep = unique_endpoint(); // nothing listens here
    let cache = temp_dir();
    write_cache(&cache.join(CACHE_FILE), &license_token_for("trillavio", "1.0.0", Duration::from_secs(60))).unwrap();
    let newer = RequireOptions { version: Some("2.0.0".into()), ..opts(&ep, &cache) };
    assert_eq!(require_with("trillavio", &newer), Err(LicenseError::BiomeNotRunning), "the cached 1.0.0 license is ignored");
    // With Biome running, it asks for 2.0.0 (and the server decides).
    let (backend, asked) = fake(true, Answer::Grant);
    let _biome = biome(&ep, backend);
    let l = require_with("trillavio", &newer).unwrap();
    assert_eq!((l.version.as_str(), l.source), ("2.0.0", LicenseSource::Biome));
    assert_eq!(*asked.lock().unwrap(), ["trillavio@2.0.0"]);
    // And the 1.0.0 build doesn't take the 2.0.0 license now cached.
    drop(_biome);
    assert!(matches!(verify(&l.token, "trillavio", Some("1.0.0")), Err(LicenseError::Invalid(why)) if why.contains("2.0.0")));
    let _ = std::fs::remove_dir_all(&cache);
}
