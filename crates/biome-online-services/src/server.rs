//! IPC server. Embedded by biome-app (desktop) and by the standalone
//! `biome-os-host` binary.
//!
//! Reference impl provides:
//!
//! - Accept loop: bind the IPC endpoint, accept new connections.
//! - Per-connection task: handshake, request/response dispatch, push fanout.
//! - Pluggable [`BosBackend`] trait: caller (biome-app or biome-os-host)
//!   implements this to resolve `match_connect_info`, fetch sessions, decide
//!   attach requests, mint tickets. Keeps this crate free of biome-app's
//!   specific tokens / HTTP clients.
//!
//! Two ways in (see `protocol.rs`):
//!
//! 1. **Launch nonce** — Biome started the game and put a one-time nonce in
//!    its environment. Issued nonces are tracked in-memory per server
//!    instance. If biome-app restarts, in-flight game processes lose their
//!    valid nonce — they get `HandshakeRejected` and have to be re-launched
//!    (or fall back to attach).
//! 2. **Attach** (2026-10) — the game started on its own and sends an
//!    `AttachHello`. The server checks there is a session, asks the backend
//!    (which may let the game in silently, or ask the user and remember the
//!    answer per app), tells the client `AwaitingConsent` only when the
//!    backend is about to ask (or is slow), then answers `Ok` or `Rejected`.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{mpsc, Mutex};

use crate::error::BosError;
use crate::events::BiomeEvent;
use crate::protocol::{
    reject, AttachHello, BosMsgV1, BosWire, RequestBody, ResponseBody, ServerHello,
    CURRENT_SDK_VERSION,
};
use crate::peer::PeerProcess;
use crate::transport::{self, platform};
use crate::types::{AttachVia, GameTicket, Session};

// ─────────────────────────────────────────────────────────────────────────────
// Backend trait + the context it gets
// ─────────────────────────────────────────────────────────────────────────────

/// What a game sent in its [`AttachHello`], handed to
/// [`BosBackend::authorize_attach`]. Nothing here is trusted: look `app_id`
/// up in the app registry and show the user the registered name.
#[derive(Clone, Debug)]
pub struct AttachRequest {
    pub app_id: String,
    pub app_name: String,
    /// What the game says its process id is. Untrusted, like the rest.
    pub game_pid: u32,
    pub client_version: String,
    /// The process id the operating system reports for the other end of
    /// this connection ([`crate::peer`]). Unlike `game_pid` the client can't
    /// choose it. `None` where the platform can't tell.
    pub peer_pid: Option<u32>,
    /// That process, pinned when the connection was accepted: ask it which
    /// program is attaching ([`PeerProcess::exe`]), never a pid looked up
    /// again later (it could belong to another process by then).
    pub peer: Option<PeerProcess>,
    /// Call [`PromptNotice::about_to_prompt`] right before showing the
    /// player anything (or waiting on a prompt for another game): the server
    /// then tells the client `AwaitingConsent`, so it waits on a human
    /// timescale. A silent decision never calls it, and the client never
    /// sees "waiting for consent".
    pub prompting: PromptNotice,
}

/// How a backend tells the server it is about to ask the player
/// ([`AttachRequest::prompting`]).
#[derive(Clone, Debug, Default)]
pub struct PromptNotice(Arc<tokio::sync::Notify>);

impl PromptNotice {
    pub fn about_to_prompt(&self) {
        self.0.notify_one();
    }
}

/// A decision that takes longer than this without a prompt (a slow registry
/// lookup) gets `AwaitingConsent` anyway, so it lands inside the client's
/// first-reply deadline (`ConnectOptions::handshake_timeout`, 2 s by
/// default) and the client waits for the answer instead of timing out.
pub const AWAITING_CONSENT_AFTER: std::time::Duration = std::time::Duration::from_secs(1);

/// The backend's answer to an attach request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttachDecision {
    Allowed,
    /// A stable code from [`reject`] (preferred) or a human-readable reason.
    Rejected(String),
}

/// How long a launch nonce stays valid after it is issued. The game reads it
/// from its environment at startup, so this only has to cover a slow start.
pub const LAUNCH_NONCE_TTL: std::time::Duration = std::time::Duration::from_secs(120);

/// A launch nonce this server issued: for which app, and when. A nonce is
/// only good for the app it was issued for, once, within
/// [`LAUNCH_NONCE_TTL`].
#[derive(Clone, Debug)]
pub struct IssuedNonce {
    pub app_id: String,
    pub issued_at: std::time::Instant,
}

impl IssuedNonce {
    pub fn new(app_id: &str) -> Self {
        Self { app_id: app_id.to_string(), issued_at: std::time::Instant::now() }
    }

    fn expired(&self, now: std::time::Instant) -> bool {
        now.saturating_duration_since(self.issued_at) > LAUNCH_NONCE_TTL
    }
}

/// Issued, not yet used nonces.
pub type NonceBook = Arc<Mutex<HashMap<String, IssuedNonce>>>;

/// Per-connection facts the backend may need when serving a request: which
/// app this is and how it got in.
#[derive(Clone, Debug)]
pub struct ConnContext {
    pub app_id: String,
    pub via: AttachVia,
    pub game_pid: u32,
}

/// Backend trait — implement this to plug a server into a real biome
/// platform (or a fake one for testing).
///
/// Uses native async fn in traits (Rust 1.75+). Trait is not `dyn`-safe in
/// the standard sense; we use it as a generic bound on the server start fn.
pub trait BosBackend: Send + Sync + 'static {
    /// Resolve a match into a connect bundle. Server calls this when the
    /// client sends a `MatchConnectInfo` request.
    fn match_connect_info(
        &self,
        match_id: crate::types::MatchId,
    ) -> impl std::future::Future<Output = Result<crate::types::ConnectInfo, BosError>> + Send;

    /// Return the current session for the user this BOS host is signed in as.
    /// May return `None` if running unsigned (dev mode).
    fn session(&self) -> impl std::future::Future<Output = Option<Session>> + Send;

    /// Handle arbitrary requests not in the typed enum yet.
    fn handle_request(
        &self,
        body: RequestBody,
    ) -> impl std::future::Future<Output = ResponseBody> + Send {
        async move { ResponseBody::Error(format!("unsupported request: {body:?}")) }
    }

    /// Decide whether a game attaching WITHOUT a launch nonce may have the
    /// session. This is where the consent prompt lives (and the per-app
    /// memory of earlier answers). Called only when a session exists.
    ///
    /// Default: refuse. A host that never shows UI must not silently hand
    /// identity to any local process.
    fn authorize_attach(
        &self,
        req: AttachRequest,
    ) -> impl std::future::Future<Output = AttachDecision> + Send {
        async move {
            let _ = req;
            AttachDecision::Rejected(reject::ATTACH_UNSUPPORTED.to_string())
        }
    }

    /// Decide whether a game Biome launched with a nonce may have the
    /// session. The nonce already proves the host started this app; this is
    /// where the host applies its registry (a disabled game is refused even
    /// when launched). Not called in dev mode.
    ///
    /// Default: allow.
    fn authorize_launch(&self, app_id: String) -> impl std::future::Future<Output = AttachDecision> + Send {
        async move {
            let _ = app_id;
            AttachDecision::Allowed
        }
    }

    /// Mint a short-lived signed ticket for the connected app. The host is
    /// expected to call the Biome backend with the user's session; the
    /// signing key never lives on the desktop.
    fn game_ticket(
        &self,
        ctx: ConnContext,
    ) -> impl std::future::Future<Output = Result<GameTicket, BosError>> + Send {
        async move {
            let _ = ctx;
            Err(BosError::Request("game tickets are not supported by this host".into()))
        }
    }
}

/// Server config.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// IPC endpoint path; pass `None` for [`transport::default_endpoint()`].
    pub endpoint: Option<String>,
    /// Skip nonce verification on incoming handshakes AND auto-allow attach
    /// requests without asking the backend. Use for development only —
    /// equivalent to "anyone on this machine can talk to BOS."
    pub dev_mode: bool,
    /// Log every IPC frame in/out to tracing (useful for dev).
    pub verbose: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            endpoint: None,
            dev_mode: false,
            verbose: false,
        }
    }
}

/// Handle for a running BOS server. Drop to stop accepting new connections;
/// existing per-connection tasks continue until their clients disconnect.
pub struct BosServer {
    /// All sessions get a clone of this; sending a [`BiomeEvent`] here
    /// broadcasts to every connected client.
    push_tx: tokio::sync::broadcast::Sender<BiomeEvent>,
    /// Nonces issued by this server, with the app each is for. Looked up
    /// (and consumed) at handshake time.
    issued_nonces: NonceBook,
    /// Send to this to issue a shutdown signal to the accept loop.
    shutdown_tx: mpsc::Sender<()>,
}

impl BosServer {
    /// Start the server. Returns once the listener is bound and ready.
    /// Per-connection work happens in tokio tasks spawned by the accept loop.
    pub async fn start<B: BosBackend>(
        config: ServerConfig,
        backend: Arc<B>,
    ) -> Result<Self, BosError> {
        let endpoint = config
            .endpoint
            .unwrap_or_else(transport::default_endpoint);
        let dev_mode = config.dev_mode;
        let verbose = config.verbose;

        let (push_tx, _) = tokio::sync::broadcast::channel::<BiomeEvent>(64);
        let issued_nonces: NonceBook = Arc::new(Mutex::new(HashMap::new()));
        let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);

        tracing::info!(endpoint = %endpoint, dev_mode, "BOS server starting");

        let backend_clone = backend.clone();
        let push_tx_clone = push_tx.clone();
        let issued_clone = issued_nonces.clone();

        // Accept loop.
        #[cfg(unix)]
        {
            let listener = platform::bind_listener(&endpoint)?;
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = shutdown_rx.recv() => {
                            tracing::info!("BOS accept loop shutting down");
                            return;
                        }
                        accept = listener.accept() => {
                            match accept {
                                Ok((stream, _addr)) => {
                                    let peer = crate::peer::unix_peer_pid(&stream).map(PeerProcess::open);
                                    spawn_conn(
                                        stream,
                                        peer,
                                        backend_clone.clone(),
                                        push_tx_clone.subscribe(),
                                        issued_clone.clone(),
                                        dev_mode,
                                        verbose,
                                    );
                                }
                                Err(e) => {
                                    tracing::error!("accept error: {e}");
                                }
                            }
                        }
                    }
                }
            });
        }
        #[cfg(windows)]
        {
            // Bind the first instance NOW so a name clash (another Biome
            // already serving this user) is reported to the caller instead
            // of dying inside the spawned task.
            let first_pipe = platform::create_server_pipe(&endpoint, true)?;
            let endpoint_clone = endpoint.clone();
            tokio::spawn(async move {
                let mut server = Some(first_pipe);
                loop {
                    let current = match server.take() {
                        Some(s) => s,
                        None => match platform::create_server_pipe(&endpoint_clone, false) {
                            Ok(s) => s,
                            Err(e) => {
                                tracing::error!("create_server_pipe: {e}");
                                return;
                            }
                        },
                    };

                    tokio::select! {
                        _ = shutdown_rx.recv() => {
                            tracing::info!("BOS accept loop shutting down");
                            return;
                        }
                        result = current.connect() => {
                            if let Err(e) = result {
                                tracing::error!("named pipe connect: {e}");
                                continue;
                            }
                            // Pin the client process now, while it is
                            // certainly the one on the pipe (PeerProcess).
                            let peer = crate::peer::pipe_client_pid(&current).map(PeerProcess::open);
                            spawn_conn(
                                current,
                                peer,
                                backend_clone.clone(),
                                push_tx_clone.subscribe(),
                                issued_clone.clone(),
                                dev_mode,
                                verbose,
                            );
                        }
                    }
                }
            });
        }

        Ok(Self {
            push_tx,
            issued_nonces,
            shutdown_tx,
        })
    }

    /// Issue a launch nonce for `app_id`. The host calls this BEFORE
    /// spawning that game and sets `BIOME_SESSION_NONCE` in its environment.
    /// The nonce is good once, only for a `ClientHello` naming the same app,
    /// within [`LAUNCH_NONCE_TTL`].
    pub async fn issue_launch_nonce(&self, app_id: &str) -> String {
        let nonce = crate::handshake::generate_launch_nonce();
        let mut book = self.issued_nonces.lock().await;
        let now = std::time::Instant::now();
        book.retain(|_, n| !n.expired(now));
        book.insert(nonce.clone(), IssuedNonce::new(app_id));
        nonce
    }

    /// Push an event to every connected client.
    pub fn broadcast(&self, ev: BiomeEvent) {
        let _ = self.push_tx.send(ev);
    }

    /// Stop accepting new connections.
    pub async fn shutdown(&self) {
        let _ = self.shutdown_tx.send(()).await;
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Per-connection task
// ─────────────────────────────────────────────────────────────────────────────

fn spawn_conn<S, B>(
    stream: S,
    peer: Option<PeerProcess>,
    backend: Arc<B>,
    push_rx: tokio::sync::broadcast::Receiver<BiomeEvent>,
    issued: NonceBook,
    dev_mode: bool,
    verbose: bool,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    B: BosBackend,
{
    tokio::spawn(async move {
        if let Err(e) = handle_conn(stream, peer, backend, push_rx, issued, dev_mode, verbose).await {
            tracing::warn!("BOS connection ended: {e}");
        }
    });
}

async fn send_rejected<S>(framed: &mut transport::Framed<S>, reason: &str) -> Result<(), BosError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    transport::send_wire(
        framed,
        &BosWire::V1(BosMsgV1::ServerHello(ServerHello::Rejected {
            reason: reason.to_string(),
        })),
    )
    .await
}

async fn handle_conn<S, B>(
    stream: S,
    peer: Option<PeerProcess>,
    backend: Arc<B>,
    mut push_rx: tokio::sync::broadcast::Receiver<BiomeEvent>,
    issued: NonceBook,
    dev_mode: bool,
    verbose: bool,
) -> Result<(), BosError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    B: BosBackend,
{
    let mut framed = transport::frame(stream);

    // 1. First frame: ClientHello (nonce path) or AttachHello (consent path).
    let ctx = match transport::recv_wire(&mut framed).await? {
        Some(BosWire::V1(BosMsgV1::ClientHello(hello))) => {
            if verbose {
                tracing::info!(?hello, "received ClientHello");
            }
            // 2a. Verify the nonce (skipped in dev mode): issued by us, for
            // THIS app, not used, not expired. A nonce presented by another
            // app is refused and left for the game it was issued to.
            if !dev_mode {
                let mut book = issued.lock().await;
                let now = std::time::Instant::now();
                book.retain(|_, n| !n.expired(now));
                let ok = matches!(book.get(&hello.session_nonce), Some(n) if n.app_id == hello.app_id);
                if ok {
                    book.remove(&hello.session_nonce);
                }
                drop(book);
                if !ok {
                    let reason = "unknown, expired or already-used session nonce, or one issued for another app";
                    let _ = send_rejected(&mut framed, reason).await;
                    return Err(BosError::HandshakeRejected(reason.into()));
                }
                // 2b. The host's registry still has the last word (a game
                // that was turned off is refused even when launched).
                if let AttachDecision::Rejected(reason) = backend.authorize_launch(hello.app_id.clone()).await {
                    let _ = send_rejected(&mut framed, &reason).await;
                    return Err(BosError::HandshakeRejected(reason));
                }
            }
            ConnContext {
                app_id: hello.app_id,
                via: AttachVia::LaunchNonce,
                game_pid: hello.game_pid,
            }
        }
        Some(BosWire::V1(BosMsgV1::AttachHello(hello))) => {
            if verbose {
                tracing::info!(?hello, "received AttachHello");
            }
            attach(&mut framed, backend.as_ref(), hello, peer, dev_mode).await?
        }
        None => {
            // Opened and closed without a hello: that's `detect::detect()`
            // probing whether we're alive. Not an error, not worth a log line.
            return Ok(());
        }
        Some(_) => {
            let reason = "first frame was not ClientHello or AttachHello";
            let _ = send_rejected(&mut framed, reason).await;
            return Err(BosError::HandshakeRejected(reason.into()));
        }
    };

    // 3. Send ServerHello::Ok
    let session = backend.session().await;
    let hello_response = BosWire::V1(BosMsgV1::ServerHello(ServerHello::Ok {
        server_version: CURRENT_SDK_VERSION,
        min_supported_sdk: 1,
        session,
    }));
    transport::send_wire(&mut framed, &hello_response).await?;
    tracing::info!(app_id = %ctx.app_id, via = ?ctx.via, pid = ctx.game_pid, "BOS client attached");

    // 4. Main loop: multiplex inbound requests + broadcast pushes.
    loop {
        tokio::select! {
            wire = transport::recv_wire(&mut framed) => {
                match wire? {
                    Some(BosWire::V1(BosMsgV1::Request { request_id, body })) => {
                        if verbose {
                            tracing::info!(request_id, ?body, "REQ");
                        }
                        let response = dispatch_request(backend.as_ref(), &ctx, body).await;
                        if verbose {
                            tracing::info!(request_id, ?response, "RES");
                        }
                        transport::send_wire(
                            &mut framed,
                            &BosWire::V1(BosMsgV1::Response { request_id, body: response }),
                        ).await?;
                    }
                    Some(BosWire::V1(BosMsgV1::Pong)) => { /* ack */ }
                    Some(BosWire::V1(BosMsgV1::Ping)) => {
                        transport::send_wire(&mut framed, &BosWire::V1(BosMsgV1::Pong)).await?;
                    }
                    Some(other) => {
                        tracing::warn!(?other, "unexpected inbound frame mid-session");
                    }
                    None => {
                        tracing::info!(app_id = %ctx.app_id, "client closed connection");
                        return Ok(());
                    }
                }
            }

            push = push_rx.recv() => {
                match push {
                    Ok(ev) => {
                        if verbose {
                            tracing::info!(?ev, "PUSH");
                        }
                        transport::send_wire(&mut framed, &BosWire::V1(BosMsgV1::Push(ev))).await?;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("client lagged {n} push events");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
                }
            }
        }
    }
}

/// The consent path. Returns the connection context on success; on refusal
/// a `Rejected` frame has already been sent.
async fn attach<S, B>(
    framed: &mut transport::Framed<S>,
    backend: &B,
    hello: AttachHello,
    peer: Option<PeerProcess>,
    dev_mode: bool,
) -> Result<ConnContext, BosError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    B: BosBackend,
{
    let ctx = ConnContext {
        app_id: hello.app_id.clone(),
        via: AttachVia::Consent,
        game_pid: hello.game_pid,
    };

    if dev_mode {
        // Dev host: anyone on this machine may attach, no prompt, session
        // may be None exactly like the nonce path in dev mode.
        return Ok(ctx);
    }

    // Nobody signed in: say so before bothering the user with a prompt.
    if backend.session().await.is_none() {
        let _ = send_rejected(framed, reject::NOT_SIGNED_IN).await;
        return Err(BosError::HandshakeRejected(reject::NOT_SIGNED_IN.into()));
    }

    // Ask the backend. `AwaitingConsent` goes out only when it is about to
    // prompt (or the answer is slow, AWAITING_CONSENT_AFTER), so a game let
    // in silently never shows "waiting for consent".
    let awaiting = BosWire::V1(BosMsgV1::ServerHello(ServerHello::AwaitingConsent {
        app_id: hello.app_id.clone(),
    }));
    let prompting = PromptNotice::default();
    let decide = backend.authorize_attach(AttachRequest {
        app_id: hello.app_id,
        app_name: hello.app_name,
        game_pid: hello.game_pid,
        client_version: hello.client_version,
        peer_pid: peer.as_ref().map(PeerProcess::pid),
        peer,
        prompting: prompting.clone(),
    });
    tokio::pin!(decide);
    let slow = tokio::time::sleep(AWAITING_CONSENT_AFTER);
    tokio::pin!(slow);
    // A failed send doesn't end the wait: the prompt may already be up, and
    // the player's answer is still kept; the final reply then fails instead.
    let mut told = false;
    let decision = loop {
        tokio::select! {
            // An answer that is ready wins over a prompt notice sent with it.
            biased;
            decision = &mut decide => break decision,
            _ = prompting.0.notified(), if !told => {
                let _ = transport::send_wire(framed, &awaiting).await;
                told = true;
            }
            _ = &mut slow, if !told => {
                let _ = transport::send_wire(framed, &awaiting).await;
                told = true;
            }
        }
    };

    match decision {
        AttachDecision::Allowed => {
            // The user may have signed out while the prompt was up.
            if backend.session().await.is_none() {
                let _ = send_rejected(framed, reject::NOT_SIGNED_IN).await;
                return Err(BosError::HandshakeRejected(reject::NOT_SIGNED_IN.into()));
            }
            Ok(ctx)
        }
        AttachDecision::Rejected(reason) => {
            let _ = send_rejected(framed, &reason).await;
            Err(BosError::HandshakeRejected(reason))
        }
    }
}

async fn dispatch_request<B: BosBackend>(
    backend: &B,
    ctx: &ConnContext,
    body: RequestBody,
) -> ResponseBody {
    match body {
        RequestBody::MatchConnectInfo { match_id } => {
            match backend.match_connect_info(match_id).await {
                Ok(info) => ResponseBody::ConnectInfo(info),
                Err(e) => ResponseBody::Error(e.to_string()),
            }
        }
        RequestBody::GetSession => ResponseBody::Session(backend.session().await),
        RequestBody::GameTicket => match backend.game_ticket(ctx.clone()).await {
            Ok(t) => ResponseBody::GameTicket(t),
            Err(e) => ResponseBody::Error(e.to_string()),
        },
        other => backend.handle_request(other).await,
    }
}

#[cfg(test)]
mod tests {
    //! Handshake state machine, driven over an in-memory duplex stream.
    use super::*;
    use crate::protocol::ClientHello;
    use crate::types::{ConnectInfo, MatchId};
    use std::time::{Duration, SystemTime};
    use uuid::Uuid;

    struct FakeBackend {
        session: Option<Session>,
        decision: AttachDecision,
        launch: AttachDecision,
        /// Whether reaching `decision` takes a prompt (false: a silent
        /// decision, like Biome's own install).
        prompts: bool,
        /// How long the decision takes.
        think: Duration,
    }

    impl FakeBackend {
        fn signed_in(decision: AttachDecision) -> Self {
            Self {
                session: Some(Session {
                    user_id: Uuid::nil(),
                    username: "tester".into(),
                    entitlements: vec!["beta".into()],
                }),
                decision,
                launch: AttachDecision::Allowed,
                prompts: true,
                think: Duration::ZERO,
            }
        }
        fn signed_out() -> Self {
            Self { session: None, decision: AttachDecision::Allowed, launch: AttachDecision::Allowed, prompts: true, think: Duration::ZERO }
        }
    }

    impl BosBackend for FakeBackend {
        async fn match_connect_info(&self, _match_id: MatchId) -> Result<ConnectInfo, BosError> {
            Err(BosError::Internal("no such match".into()))
        }
        async fn session(&self) -> Option<Session> {
            self.session.clone()
        }
        async fn authorize_attach(&self, req: AttachRequest) -> AttachDecision {
            assert_eq!(req.app_id, "test-app");
            // The pid the connection came with, not the one the game claimed.
            assert_eq!((req.peer_pid, req.game_pid), (Some(4242), 1));
            assert_eq!(req.peer.as_ref().map(PeerProcess::pid), Some(4242));
            if self.prompts {
                req.prompting.about_to_prompt();
            }
            tokio::time::sleep(self.think).await;
            self.decision.clone()
        }
        async fn authorize_launch(&self, _app_id: String) -> AttachDecision {
            self.launch.clone()
        }
        async fn game_ticket(&self, ctx: ConnContext) -> Result<GameTicket, BosError> {
            Ok(GameTicket {
                ticket: format!("ticket-for-{}", ctx.app_id),
                expires_at: SystemTime::now() + Duration::from_secs(300),
                app_id: ctx.app_id,
            })
        }
    }

    fn hello(nonce: &str) -> BosWire {
        hello_as("test-app", nonce)
    }

    fn hello_as(app_id: &str, nonce: &str) -> BosWire {
        BosWire::V1(BosMsgV1::ClientHello(ClientHello {
            sdk_version: CURRENT_SDK_VERSION,
            app_id: app_id.into(),
            session_nonce: nonce.into(),
            game_pid: 1,
        }))
    }

    /// A nonce book with `(nonce, app_id)` entries issued just now.
    fn book(entries: &[(&str, &str)]) -> NonceBook {
        Arc::new(Mutex::new(entries.iter().map(|(n, a)| (n.to_string(), IssuedNonce::new(a))).collect()))
    }

    fn attach_hello() -> BosWire {
        BosWire::V1(BosMsgV1::AttachHello(AttachHello {
            sdk_version: CURRENT_SDK_VERSION,
            app_id: "test-app".into(),
            app_name: "Test App".into(),
            game_pid: 1,
            client_version: "0.0.1".into(),
        }))
    }

    type Client = transport::Framed<tokio::io::DuplexStream>;

    /// Connects a client to a fresh server task, sends `first`, and returns
    /// the server's first reply plus the client stream for further requests.
    async fn connect_with(
        backend: FakeBackend,
        issued: NonceBook,
        dev_mode: bool,
        first: BosWire,
    ) -> (Option<BosWire>, Client, tokio::task::JoinHandle<Result<(), BosError>>) {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (push_tx, push_rx) = tokio::sync::broadcast::channel(8);
        let server = tokio::spawn(async move {
            let _keep_push_open = push_tx;
            handle_conn(server_io, Some(PeerProcess::unpinned(4242)), Arc::new(backend), push_rx, issued, dev_mode, false).await
        });
        let mut client = transport::frame(client_io);
        transport::send_wire(&mut client, &first).await.unwrap();
        let reply = transport::recv_wire(&mut client).await.unwrap_or(None);
        (reply, client, server)
    }

    async fn connect(
        issued: NonceBook,
        dev_mode: bool,
        first: BosWire,
    ) -> (Option<BosWire>, Client, tokio::task::JoinHandle<Result<(), BosError>>) {
        connect_with(FakeBackend::signed_in(AttachDecision::Allowed), issued, dev_mode, first).await
    }

    fn is_ok_hello(w: &Option<BosWire>) -> bool {
        matches!(w, Some(BosWire::V1(BosMsgV1::ServerHello(ServerHello::Ok { .. }))))
    }

    fn is_rejected(w: &Option<BosWire>) -> bool {
        matches!(w, Some(BosWire::V1(BosMsgV1::ServerHello(ServerHello::Rejected { .. }))))
    }

    fn rejected_reason(w: &Option<BosWire>) -> Option<String> {
        match w {
            Some(BosWire::V1(BosMsgV1::ServerHello(ServerHello::Rejected { reason }))) => Some(reason.clone()),
            _ => None,
        }
    }

    fn is_awaiting(w: &Option<BosWire>) -> bool {
        matches!(w, Some(BosWire::V1(BosMsgV1::ServerHello(ServerHello::AwaitingConsent { .. }))))
    }

    // ── nonce path (unchanged behaviour) ────────────────────────────────

    #[tokio::test]
    async fn issued_nonce_is_accepted_exactly_once() {
        let issued = book(&[("n1", "test-app")]);

        let (reply, _c, _s) = connect(issued.clone(), false, hello("n1")).await;
        assert!(is_ok_hello(&reply), "first use should be accepted: {reply:?}");

        let (reply, _c, server) = connect(issued.clone(), false, hello("n1")).await;
        assert!(is_rejected(&reply), "replay must be rejected: {reply:?}");
        assert!(matches!(server.await.unwrap(), Err(BosError::HandshakeRejected(_))));
    }

    #[tokio::test]
    async fn unknown_nonce_is_rejected() {
        let issued = book(&[("real", "test-app")]);
        let (reply, _c, _s) = connect(issued.clone(), false, hello("guess")).await;
        assert!(is_rejected(&reply));
        assert!(issued.lock().await.contains_key("real"), "a failed guess must not burn the real nonce");
    }

    #[tokio::test]
    async fn a_nonce_only_works_for_the_app_it_was_issued_for() {
        let issued = book(&[("n1", "the-launched-game")]);
        // Another app presenting it is refused, and the nonce survives...
        let (reply, _c, _s) = connect(issued.clone(), false, hello_as("test-app", "n1")).await;
        assert!(is_rejected(&reply), "{reply:?}");
        assert!(issued.lock().await.contains_key("n1"));
        // ...for the game it was issued to.
        let (reply, _c, _s) = connect(issued.clone(), false, hello_as("the-launched-game", "n1")).await;
        assert!(is_ok_hello(&reply), "{reply:?}");
        assert!(issued.lock().await.is_empty());
    }

    #[tokio::test]
    async fn an_expired_nonce_is_refused_and_dropped() {
        let Some(long_ago) = std::time::Instant::now().checked_sub(LAUNCH_NONCE_TTL + Duration::from_secs(1)) else {
            return; // the machine has been up for less than the TTL
        };
        let issued: NonceBook = Arc::new(Mutex::new(HashMap::from([(
            "old".to_string(),
            IssuedNonce { app_id: "test-app".into(), issued_at: long_ago },
        )])));
        let (reply, _c, _s) = connect(issued.clone(), false, hello("old")).await;
        assert!(is_rejected(&reply), "{reply:?}");
        assert!(issued.lock().await.is_empty(), "expired nonces are dropped");
    }

    #[tokio::test]
    async fn a_launched_game_the_registry_refuses_is_rejected() {
        let issued = book(&[("n1", "test-app")]);
        let mut backend = FakeBackend::signed_in(AttachDecision::Allowed);
        backend.launch = AttachDecision::Rejected(reject::UNKNOWN_APP.into());
        let (reply, _c, server) = connect_with(backend, issued, false, hello("n1")).await;
        match reply {
            Some(BosWire::V1(BosMsgV1::ServerHello(ServerHello::Rejected { reason }))) => assert_eq!(reason, reject::UNKNOWN_APP),
            other => panic!("expected a rejection, got {other:?}"),
        }
        assert!(matches!(server.await.unwrap(), Err(BosError::HandshakeRejected(_))));
    }

    #[tokio::test]
    async fn dev_mode_accepts_any_nonce() {
        let issued = Arc::new(Mutex::new(HashMap::new()));
        let (reply, _c, _s) = connect(issued, true, hello("anything")).await;
        assert!(is_ok_hello(&reply));
    }

    #[tokio::test]
    async fn first_frame_must_be_a_hello() {
        let issued = Arc::new(Mutex::new(HashMap::new()));
        let (reply, _c, server) = connect(issued, true, BosWire::V1(BosMsgV1::Ping)).await;
        assert!(is_rejected(&reply));
        assert!(matches!(server.await.unwrap(), Err(BosError::HandshakeRejected(_))));
    }

    #[tokio::test]
    async fn probe_that_closes_without_hello_is_not_an_error() {
        let (client_io, server_io) = tokio::io::duplex(1024);
        let (push_tx, push_rx) = tokio::sync::broadcast::channel(8);
        let issued = Arc::new(Mutex::new(HashMap::new()));
        let server = tokio::spawn(async move {
            let _keep = push_tx;
            handle_conn(server_io, None, Arc::new(FakeBackend::signed_out()), push_rx, issued, false, false).await
        });
        drop(client_io); // what detect() does: open, then close
        assert!(matches!(server.await.unwrap(), Ok(())));
    }

    #[tokio::test]
    async fn requests_are_dispatched_after_handshake() {
        let issued = Arc::new(Mutex::new(HashMap::new()));
        let (reply, mut client, _s) = connect(issued, true, hello("x")).await;
        assert!(is_ok_hello(&reply));

        transport::send_wire(&mut client, &BosWire::V1(BosMsgV1::Request {
            request_id: 7,
            body: RequestBody::GetSession,
        })).await.unwrap();
        match transport::recv_wire(&mut client).await.unwrap() {
            Some(BosWire::V1(BosMsgV1::Response { request_id: 7, body: ResponseBody::Session(Some(s)) })) => {
                assert_eq!(s.username, "tester");
            }
            other => panic!("unexpected response: {other:?}"),
        }

        transport::send_wire(&mut client, &BosWire::V1(BosMsgV1::Request {
            request_id: 8,
            body: RequestBody::MatchConnectInfo { match_id: MatchId(Uuid::nil()) },
        })).await.unwrap();
        assert!(matches!(
            transport::recv_wire(&mut client).await.unwrap(),
            Some(BosWire::V1(BosMsgV1::Response { request_id: 8, body: ResponseBody::Error(_) }))
        ));

        transport::send_wire(&mut client, &BosWire::V1(BosMsgV1::Ping)).await.unwrap();
        assert!(matches!(
            transport::recv_wire(&mut client).await.unwrap(),
            Some(BosWire::V1(BosMsgV1::Pong))
        ));
    }

    #[tokio::test]
    async fn server_issued_nonces_are_unique_and_tracked() {
        let issued = Arc::new(Mutex::new(HashMap::new()));
        let a = crate::handshake::generate_launch_nonce();
        let b = crate::handshake::generate_launch_nonce();
        assert_ne!(a, b);
        issued.lock().await.insert(a.clone(), IssuedNonce::new("test-app"));
        let (reply, _c, _s) = connect(issued, false, hello(&a)).await;
        assert!(is_ok_hello(&reply));
    }

    // ── attach path (2026-10) ───────────────────────────────────────────

    #[tokio::test]
    async fn attach_allowed_sends_awaiting_then_ok_with_session() {
        let issued = Arc::new(Mutex::new(HashMap::new()));
        let (first, mut client, _s) =
            connect_with(FakeBackend::signed_in(AttachDecision::Allowed), issued, false, attach_hello()).await;
        assert!(is_awaiting(&first), "{first:?}");
        let second = transport::recv_wire(&mut client).await.unwrap();
        match second {
            Some(BosWire::V1(BosMsgV1::ServerHello(ServerHello::Ok { session: Some(s), .. }))) => {
                assert_eq!(s.username, "tester");
            }
            other => panic!("expected Ok with session, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn attach_denied_is_rejected_with_consent_denied() {
        let issued = Arc::new(Mutex::new(HashMap::new()));
        let (first, mut client, server) = connect_with(
            FakeBackend::signed_in(AttachDecision::Rejected(reject::CONSENT_DENIED.into())),
            issued,
            false,
            attach_hello(),
        )
        .await;
        assert!(is_awaiting(&first));
        let second = transport::recv_wire(&mut client).await.unwrap();
        assert_eq!(rejected_reason(&second).as_deref(), Some(reject::CONSENT_DENIED));
        assert!(matches!(server.await.unwrap(), Err(BosError::HandshakeRejected(r)) if r == reject::CONSENT_DENIED));
    }

    #[tokio::test]
    async fn attach_without_session_is_rejected_before_any_prompt() {
        let issued = Arc::new(Mutex::new(HashMap::new()));
        let (first, _c, _s) = connect_with(FakeBackend::signed_out(), issued, false, attach_hello()).await;
        assert_eq!(rejected_reason(&first).as_deref(), Some(reject::NOT_SIGNED_IN));
    }

    #[tokio::test]
    async fn attach_in_dev_mode_skips_consent() {
        let issued = Arc::new(Mutex::new(HashMap::new()));
        let (first, _c, _s) = connect_with(
            FakeBackend::signed_in(AttachDecision::Rejected("would-deny".into())),
            issued,
            true,
            attach_hello(),
        )
        .await;
        assert!(is_ok_hello(&first), "dev mode must not consult the backend: {first:?}");
    }

    #[tokio::test]
    async fn default_backend_refuses_attach() {
        struct Bare;
        impl BosBackend for Bare {
            async fn match_connect_info(&self, _m: MatchId) -> Result<ConnectInfo, BosError> {
                Err(BosError::Internal("n/a".into()))
            }
            async fn session(&self) -> Option<Session> {
                Some(Session { user_id: Uuid::nil(), username: "u".into(), entitlements: vec![] })
            }
        }
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (push_tx, push_rx) = tokio::sync::broadcast::channel(8);
        let issued = Arc::new(Mutex::new(HashMap::new()));
        let _server = tokio::spawn(async move {
            let _keep = push_tx;
            handle_conn(server_io, None, Arc::new(Bare), push_rx, issued, false, false).await
        });
        let mut client = transport::frame(client_io);
        transport::send_wire(&mut client, &attach_hello()).await.unwrap();
        // It refuses without asking anyone, so no "waiting for consent".
        let first = transport::recv_wire(&mut client).await.unwrap();
        assert_eq!(rejected_reason(&first).as_deref(), Some(reject::ATTACH_UNSUPPORTED));
    }

    #[tokio::test]
    async fn a_silent_allow_never_says_awaiting_consent() {
        // Biome's own install: let in without a prompt, so the first and
        // only answer is Ok.
        let mut backend = FakeBackend::signed_in(AttachDecision::Allowed);
        backend.prompts = false;
        let (first, _c, _s) = connect_with(backend, Arc::new(Mutex::new(HashMap::new())), false, attach_hello()).await;
        assert!(is_ok_hello(&first), "{first:?}");
        // A silent refusal (a remembered deny) likewise.
        let mut backend = FakeBackend::signed_in(AttachDecision::Rejected(reject::CONSENT_DENIED.into()));
        backend.prompts = false;
        let (first, _c, _s) = connect_with(backend, Arc::new(Mutex::new(HashMap::new())), false, attach_hello()).await;
        assert_eq!(rejected_reason(&first).as_deref(), Some(reject::CONSENT_DENIED));
    }

    #[tokio::test]
    async fn a_slow_silent_decision_says_awaiting_consent_in_time() {
        // A registry lookup that takes longer than the client's first-reply
        // deadline would allow: AwaitingConsent goes out before it.
        let mut backend = FakeBackend::signed_in(AttachDecision::Allowed);
        backend.prompts = false;
        backend.think = AWAITING_CONSENT_AFTER + Duration::from_millis(300);
        let started = std::time::Instant::now();
        let (first, mut client, _s) = connect_with(backend, Arc::new(Mutex::new(HashMap::new())), false, attach_hello()).await;
        assert!(is_awaiting(&first), "{first:?}");
        assert!(started.elapsed() < AWAITING_CONSENT_AFTER + Duration::from_millis(250));
        assert!(is_ok_hello(&transport::recv_wire(&mut client).await.unwrap()));
    }

    #[tokio::test]
    async fn game_ticket_request_reaches_backend_with_app_id() {
        let issued = Arc::new(Mutex::new(HashMap::new()));
        let (first, mut client, _s) =
            connect_with(FakeBackend::signed_in(AttachDecision::Allowed), issued, false, attach_hello()).await;
        assert!(is_awaiting(&first));
        assert!(is_ok_hello(&transport::recv_wire(&mut client).await.unwrap()));

        transport::send_wire(&mut client, &BosWire::V1(BosMsgV1::Request {
            request_id: 3,
            body: RequestBody::GameTicket,
        })).await.unwrap();
        match transport::recv_wire(&mut client).await.unwrap() {
            Some(BosWire::V1(BosMsgV1::Response { request_id: 3, body: ResponseBody::GameTicket(t) })) => {
                assert_eq!(t.ticket, "ticket-for-test-app");
                assert_eq!(t.app_id, "test-app");
                assert!(!t.is_expired());
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }
}
