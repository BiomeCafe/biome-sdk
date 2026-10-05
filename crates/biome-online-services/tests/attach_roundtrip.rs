//! End-to-end over the real OS transport (named pipe / unix socket): a
//! `BosServer` with a scripted backend on one side, the public `BiomeClient`
//! API on the other. Each test uses its own endpoint name so they can run in
//! parallel.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use biome_online_services::error::BosError;
use biome_online_services::protocol::reject;
use biome_online_services::server::{AttachDecision, AttachRequest, BosBackend, BosServer, ConnContext, ServerConfig};
use biome_online_services::types::{ConnectInfo, GameTicket, MatchId, Session};
use biome_online_services::{detect, AttachVia, BiomeClient, BosConfig, ConnectOptions, Transport};
use uuid::Uuid;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

fn unique_endpoint() -> String {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    #[cfg(windows)]
    {
        format!(r"\\.\pipe\biome-os-test-{}-{n}", std::process::id())
    }
    #[cfg(unix)]
    {
        std::env::temp_dir()
            .join(format!("biome-os-test-{}-{n}.sock", std::process::id()))
            .to_string_lossy()
            .into_owned()
    }
}

struct Scripted {
    session: Option<Session>,
    decision: AttachDecision,
    /// How long the "user" takes to answer the prompt.
    think: Duration,
    prompts: Arc<AtomicUsize>,
}

impl BosBackend for Scripted {
    async fn match_connect_info(&self, _m: MatchId) -> Result<ConnectInfo, BosError> {
        Err(BosError::Internal("n/a".into()))
    }
    async fn session(&self) -> Option<Session> {
        self.session.clone()
    }
    async fn authorize_attach(&self, req: AttachRequest) -> AttachDecision {
        self.prompts.fetch_add(1, Ordering::Relaxed);
        assert_eq!(req.app_id, "mecha-aether");
        assert_eq!(req.app_name, "Mecha Aether");
        assert_eq!(req.client_version, "9.9.9");
        tokio::time::sleep(self.think).await;
        self.decision.clone()
    }
    async fn game_ticket(&self, ctx: ConnContext) -> Result<GameTicket, BosError> {
        Ok(GameTicket {
            ticket: format!("hdr.{}.sig", ctx.app_id),
            expires_at: SystemTime::now() + Duration::from_secs(300),
            app_id: ctx.app_id,
        })
    }
}

/// Runs a server on its own runtime thread until the returned guard drops.
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

fn run_server(endpoint: &str, backend: Scripted, dev_mode: bool) -> ServerGuard {
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let endpoint = endpoint.to_string();
    let thread = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let server = BosServer::start(
                ServerConfig { endpoint: Some(endpoint), dev_mode, verbose: false },
                Arc::new(backend),
            )
            .await;
            let server = match server {
                Ok(s) => {
                    let _ = ready_tx.send(Ok(()));
                    s
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e.to_string()));
                    return;
                }
            };
            // Stay alive until the guard drops (sender closed → recv errs).
            let _ = tokio::task::spawn_blocking(move || stop_rx.recv()).await;
            server.shutdown().await;
        });
    });
    ready_rx.recv_timeout(Duration::from_secs(5)).expect("server thread").expect("server start");
    ServerGuard { stop: Some(stop_tx), thread: Some(thread) }
}

fn user() -> Session {
    Session { user_id: Uuid::from_u128(7), username: "mandy".into(), entitlements: vec![] }
}

fn config(endpoint: &str) -> BosConfig {
    BosConfig::new("mecha-aether", "Mecha Aether").with_transport(Transport::Custom { path: endpoint.into() })
}

fn opts() -> ConnectOptions {
    ConnectOptions {
        launch_nonce: Some(String::new()), // force the attach path even if the env var is set
        client_version: "9.9.9".into(),
        consent_timeout: Duration::from_secs(5),
        ..ConnectOptions::default()
    }
}

#[test]
fn attach_allowed_yields_session_and_ticket() {
    let ep = unique_endpoint();
    let prompts = Arc::new(AtomicUsize::new(0));
    let _server = run_server(
        &ep,
        Scripted { session: Some(user()), decision: AttachDecision::Allowed, think: Duration::from_millis(300), prompts: prompts.clone() },
        false,
    );
    assert!(detect::endpoint_accepts(&ep), "server should be detectable");
    assert_eq!(detect::detect_at(&ep), detect::PlatformState::Running);

    let client = BiomeClient::connect_with(config(&ep), opts()).expect("attach");
    let s = client.session().expect("session");
    assert_eq!(s.username, "mandy");
    assert_eq!(s.user_id, Uuid::from_u128(7));
    assert_eq!(client.attached_via(), AttachVia::Consent);
    assert_eq!(prompts.load(Ordering::Relaxed), 1, "backend asked exactly once");

    let t = client.game_ticket().wait().expect("ticket");
    assert_eq!(t.ticket, "hdr.mecha-aether.sig");
    assert_eq!(t.app_id, "mecha-aether");
    assert!(!t.is_expired());
}

#[test]
fn attach_denied_is_typed() {
    let ep = unique_endpoint();
    let _server = run_server(
        &ep,
        Scripted {
            session: Some(user()),
            decision: AttachDecision::Rejected(reject::CONSENT_DENIED.into()),
            think: Duration::ZERO,
            prompts: Arc::new(AtomicUsize::new(0)),
        },
        false,
    );
    match BiomeClient::connect_with(config(&ep), opts()) {
        Err(BosError::ConsentDenied) => {}
        other => panic!("expected ConsentDenied, got {other:?}", other = other.map(|_| "client")),
    }
}

#[test]
fn attach_without_session_is_not_signed_in() {
    let ep = unique_endpoint();
    let _server = run_server(
        &ep,
        Scripted { session: None, decision: AttachDecision::Allowed, think: Duration::ZERO, prompts: Arc::new(AtomicUsize::new(0)) },
        false,
    );
    match BiomeClient::connect_with(config(&ep), opts()) {
        Err(BosError::NotSignedIn) => {}
        other => panic!("expected NotSignedIn, got {:?}", other.map(|_| "client")),
    }
}

#[test]
fn consent_timeout_fires_when_nobody_answers() {
    let ep = unique_endpoint();
    let _server = run_server(
        &ep,
        Scripted { session: Some(user()), decision: AttachDecision::Allowed, think: Duration::from_secs(30), prompts: Arc::new(AtomicUsize::new(0)) },
        false,
    );
    let o = ConnectOptions { consent_timeout: Duration::from_millis(500), ..opts() };
    let started = std::time::Instant::now();
    match BiomeClient::connect_with(config(&ep), o) {
        Err(BosError::ConsentTimeout { .. }) => {}
        other => panic!("expected ConsentTimeout, got {:?}", other.map(|_| "client")),
    }
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn background_connect_reports_awaiting_consent() {
    let ep = unique_endpoint();
    let _server = run_server(
        &ep,
        Scripted { session: Some(user()), decision: AttachDecision::Allowed, think: Duration::from_millis(800), prompts: Arc::new(AtomicUsize::new(0)) },
        false,
    );
    let mut pending = BiomeClient::connect_background(config(&ep), opts());
    let mut saw_awaiting = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let client = loop {
        if pending.status() == biome_online_services::ConnectStatus::AwaitingConsent {
            saw_awaiting = true;
        }
        if let Some(r) = pending.poll() {
            break r.expect("connect");
        }
        assert!(std::time::Instant::now() < deadline, "timed out");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(saw_awaiting, "status should have passed through AwaitingConsent");
    assert_eq!(client.session().unwrap().username, "mandy");
}

#[test]
fn nonce_path_still_works_and_is_single_use() {
    let ep = unique_endpoint();
    // A nonce path needs a nonce the server issued; use dev_mode so any
    // nonce is accepted — what matters here is that ClientHello still
    // handshakes and reports LaunchNonce.
    let _server = run_server(
        &ep,
        Scripted { session: Some(user()), decision: AttachDecision::Rejected("never".into()), think: Duration::ZERO, prompts: Arc::new(AtomicUsize::new(0)) },
        true,
    );
    let o = ConnectOptions { launch_nonce: Some("issued-by-host".into()), ..opts() };
    let client = BiomeClient::connect_with(config(&ep), o).expect("nonce handshake");
    assert_eq!(client.attached_via(), AttachVia::LaunchNonce);
    assert_eq!(client.session().unwrap().username, "mandy");
}

#[test]
fn nothing_running_is_platform_unavailable() {
    let ep = unique_endpoint();
    assert!(!detect::endpoint_accepts(&ep));
    match BiomeClient::connect_with(config(&ep), opts()) {
        Err(BosError::PlatformUnavailable) => {}
        other => panic!("expected PlatformUnavailable, got {:?}", other.map(|_| "client")),
    }
}
