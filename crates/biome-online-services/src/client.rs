//! [`BiomeClient`] — the game-facing surface.
//!
//! Threading model (per cafe spec):
//!
//! - Connecting spawns a **dedicated OS thread** that owns a
//!   `tokio::runtime::Runtime` and the IPC connection.
//! - Game code calls sync methods. Requests get a [`RequestHandle`] back —
//!   game polls `.poll()` at its own cadence (typically once per frame).
//! - Events arrive over a crossbeam channel. Game drains via `next_event()`.
//! - Game code NEVER sees `.await`, NEVER blocks on the runtime, NEVER
//!   allocates per-frame for the steady state.
//!
//! Two ways to get a client (2026-10):
//!
//! - [`BiomeClient::connect`] — picks the path for you: if Biome launched
//!   the game (`BIOME_SESSION_NONCE` set) it handshakes with the nonce,
//!   otherwise it attaches and the user is asked once per app. Blocks for
//!   up to [`ConnectOptions::consent_timeout`] on the first run.
//! - [`BiomeClient::connect_background`] — same, but returns a
//!   [`PendingClient`] you poll each frame, so a menu can keep animating
//!   while the consent prompt is up.
//!
//! [`BiomeClient::new`] keeps its old meaning (nonce required) for games
//! that were written against it.

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::{bounded, unbounded, Receiver, Sender, TryRecvError};
use tokio::runtime::Runtime;
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::error::BosError;
use crate::events::BiomeEvent;
use crate::handshake;
use crate::protocol::{
    AttachHello, BosMsgV1, BosWire, ClientHello, RequestBody, ResponseBody, ServerHello,
    CURRENT_SDK_VERSION,
};
use crate::transport::{self, platform};
use crate::types::{AttachVia, BosConfig, ConnectInfo, GameTicket, MatchId, Session, Transport};

const HANDSHAKE_TIMEOUT_MS: u64 = 2_000;
const CONSENT_TIMEOUT_SECS: u64 = 120;
const REQUEST_TIMEOUT_MS: u64 = 10_000;

// ─────────────────────────────────────────────────────────────────────────────
// Public API
// ─────────────────────────────────────────────────────────────────────────────

/// Knobs for [`BiomeClient::connect_with`] / [`BiomeClient::connect_background`].
#[derive(Clone, Debug)]
pub struct ConnectOptions {
    /// How long to wait for the host's first reply after the hello.
    pub handshake_timeout: Duration,
    /// How long to wait for the user to answer the consent prompt once the
    /// host says `AwaitingConsent`. Human timescale.
    pub consent_timeout: Duration,
    /// Use this launch nonce instead of reading `BIOME_SESSION_NONCE`.
    /// `Some("")` forces the attach path even when the env var is set.
    pub launch_nonce: Option<String>,
    /// Game build version sent with an attach (recorded next to the consent).
    pub client_version: String,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        Self {
            handshake_timeout: Duration::from_millis(HANDSHAKE_TIMEOUT_MS),
            consent_timeout: Duration::from_secs(CONSENT_TIMEOUT_SECS),
            launch_nonce: None,
            client_version: String::new(),
        }
    }
}

/// Where a background connect is at. Poll via [`PendingClient::status`] to
/// drive UI ("waiting for you to allow Mecha Aether in Biome…").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectStatus {
    Connecting,
    AwaitingConsent,
    Done,
}

const STATUS_CONNECTING: u8 = 0;
const STATUS_AWAITING_CONSENT: u8 = 1;
const STATUS_DONE: u8 = 2;

/// The game-facing client. Owns a dedicated tokio thread internally.
///
/// Drop this to shut down cleanly. The IPC connection closes, the tokio
/// runtime shuts down, the thread joins.
pub struct BiomeClient {
    inner: Arc<ClientInner>,
}

struct ClientInner {
    /// Channel of inbound events from server → game thread.
    events_rx: Receiver<BiomeEvent>,
    /// Send channel for outbound requests game → IPC task.
    outbound_tx: mpsc::UnboundedSender<OutboundCommand>,
    /// Monotonic counter for request IDs.
    request_seq: AtomicU64,
    /// Session resolved at handshake (None in dev-mode with no user signed in).
    initial_session: Option<Session>,
    /// How this connection got in.
    via: AttachVia,
    app_id: String,
    /// Keep the runtime alive for the lifetime of the client.
    _runtime: Runtime,
    /// Thread handle for cleanup on drop.
    _io_thread: Option<std::thread::JoinHandle<()>>,
}

/// Async-poll handle returned by request methods. Game polls each frame.
///
/// Dropping the handle cancels the request (best-effort). The server may
/// still process it but the response will be discarded.
pub struct RequestHandle<T> {
    rx: crossbeam_channel::Receiver<Result<T, BosError>>,
}

impl<T> RequestHandle<T> {
    /// Non-blocking poll. Returns `None` if response hasn't arrived yet.
    pub fn poll(&self) -> Option<Result<T, BosError>> {
        match self.rx.try_recv() {
            Ok(v) => Some(v),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                Some(Err(BosError::Internal("response channel closed".into())))
            }
        }
    }

    /// Blocking wait — only use from one-shot tools (CLI, tests).
    /// Game code should always use [`poll`](Self::poll).
    pub fn wait(&self) -> Result<T, BosError> {
        self.rx
            .recv()
            .map_err(|_| BosError::Internal("response channel closed".into()))?
    }
}

/// A connect in progress (see [`BiomeClient::connect_background`]).
pub struct PendingClient {
    rx: Receiver<Result<BiomeClient, BosError>>,
    status: Arc<AtomicU8>,
    taken: bool,
}

impl PendingClient {
    /// Non-blocking. `Some` exactly once, when the connect finished either
    /// way; `None` while still connecting or waiting for consent.
    pub fn poll(&mut self) -> Option<Result<BiomeClient, BosError>> {
        if self.taken {
            return None;
        }
        match self.rx.try_recv() {
            Ok(r) => {
                self.taken = true;
                Some(r)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.taken = true;
                Some(Err(BosError::Internal("connect thread died".into())))
            }
        }
    }

    pub fn status(&self) -> ConnectStatus {
        match self.status.load(Ordering::Relaxed) {
            STATUS_AWAITING_CONSENT => ConnectStatus::AwaitingConsent,
            STATUS_DONE => ConnectStatus::Done,
            _ => ConnectStatus::Connecting,
        }
    }

    /// Blocking wait — tools and tests only.
    pub fn wait(mut self) -> Result<BiomeClient, BosError> {
        if self.taken {
            return Err(BosError::Internal("already taken".into()));
        }
        self.taken = true;
        self.rx
            .recv()
            .map_err(|_| BosError::Internal("connect thread died".into()))?
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Internal command + state
// ─────────────────────────────────────────────────────────────────────────────

/// One outbound work item from game thread → IPC task.
enum OutboundCommand {
    Request {
        body: RequestBody,
        response_tx: crossbeam_channel::Sender<Result<ResponseBody, BosError>>,
        request_id: u64,
    },
    Shutdown,
}

/// What the handshake resolved to.
struct Handshake {
    session: Option<Session>,
    via: AttachVia,
}

// ─────────────────────────────────────────────────────────────────────────────
// Constructors
// ─────────────────────────────────────────────────────────────────────────────

impl BiomeClient {
    /// Open a BOS client **with a launch nonce**. Fails with
    /// [`BosError::MissingLaunchNonce`] when the game wasn't started by Biome.
    /// Kept for games written against the original API; new code should
    /// call [`BiomeClient::connect`].
    pub fn new(config: BosConfig) -> Result<Self, BosError> {
        if let Transport::Mock = config.transport {
            return Ok(Self::new_mock(config));
        }
        let nonce = handshake::read_launch_nonce().ok_or(BosError::MissingLaunchNonce)?;
        let opts = ConnectOptions {
            launch_nonce: Some(nonce),
            ..ConnectOptions::default()
        };
        Self::connect_inner(config, opts, Arc::new(AtomicU8::new(STATUS_CONNECTING)))
    }

    /// Connect however the situation allows: launch nonce if Biome started
    /// us, attach (with consent) otherwise. Blocks until the host answers —
    /// up to `handshake_timeout + consent_timeout` from [`ConnectOptions`].
    pub fn connect(config: BosConfig) -> Result<Self, BosError> {
        Self::connect_with(config, ConnectOptions::default())
    }

    /// [`connect`](Self::connect) with explicit options.
    pub fn connect_with(config: BosConfig, opts: ConnectOptions) -> Result<Self, BosError> {
        if let Transport::Mock = config.transport {
            return Ok(Self::new_mock(config));
        }
        Self::connect_inner(config, opts, Arc::new(AtomicU8::new(STATUS_CONNECTING)))
    }

    /// Non-blocking connect: returns immediately, the handshake (and the
    /// consent prompt, if any) runs on a helper thread. Poll the result each
    /// frame.
    pub fn connect_background(config: BosConfig, opts: ConnectOptions) -> PendingClient {
        let status = Arc::new(AtomicU8::new(STATUS_CONNECTING));
        let (tx, rx) = bounded::<Result<BiomeClient, BosError>>(1);
        let status_for_thread = status.clone();
        let spawned = std::thread::Builder::new()
            .name("biome-os-connect".into())
            .spawn(move || {
                let result = if let Transport::Mock = config.transport {
                    Ok(BiomeClient::new_mock(config))
                } else {
                    BiomeClient::connect_inner(config, opts, status_for_thread.clone())
                };
                status_for_thread.store(STATUS_DONE, Ordering::Relaxed);
                let _ = tx.send(result);
            });
        if let Err(e) = spawned {
            let (tx2, rx2) = bounded(1);
            let _ = tx2.send(Err(BosError::Internal(format!("spawn connect thread: {e}"))));
            status.store(STATUS_DONE, Ordering::Relaxed);
            return PendingClient { rx: rx2, status, taken: false };
        }
        PendingClient { rx, status, taken: false }
    }

    fn connect_inner(
        config: BosConfig,
        opts: ConnectOptions,
        status: Arc<AtomicU8>,
    ) -> Result<Self, BosError> {
        let endpoint = match &config.transport {
            Transport::Custom { path } => path.clone(),
            _ => transport::default_endpoint(),
        };

        // Nonce if we have one (explicit beats env; an explicit empty string
        // means "attach even though the env var is set").
        let nonce = match opts.launch_nonce.clone() {
            Some(n) => Some(n).filter(|n| !n.is_empty()),
            None => handshake::read_launch_nonce(),
        };
        let (hello, via) = match nonce {
            Some(session_nonce) => (
                BosWire::V1(BosMsgV1::ClientHello(ClientHello {
                    sdk_version: CURRENT_SDK_VERSION,
                    app_id: config.app_id.clone(),
                    session_nonce,
                    game_pid: std::process::id(),
                })),
                AttachVia::LaunchNonce,
            ),
            None => (
                BosWire::V1(BosMsgV1::AttachHello(AttachHello {
                    sdk_version: CURRENT_SDK_VERSION,
                    app_id: config.app_id.clone(),
                    app_name: config.app_name.clone(),
                    game_pid: std::process::id(),
                    client_version: opts.client_version.clone(),
                })),
                AttachVia::Consent,
            ),
        };

        // Build the runtime on a dedicated thread so the game's own tokio
        // (if any) is untouched.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| BosError::Internal(format!("tokio runtime build: {e}")))?;

        let (events_tx, events_rx) = unbounded::<BiomeEvent>();
        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel::<OutboundCommand>();
        let (handshake_tx, handshake_rx) = bounded::<Result<Handshake, BosError>>(1);

        let endpoint_cloned = endpoint.clone();
        let timeouts = (opts.handshake_timeout, opts.consent_timeout);
        let status_for_io = status.clone();

        let io_thread = std::thread::Builder::new()
            .name("biome-os-io".into())
            .spawn(move || {
                runtime.block_on(async move {
                    run_io_task(
                        endpoint_cloned,
                        hello,
                        via,
                        timeouts,
                        status_for_io,
                        events_tx,
                        outbound_rx,
                        handshake_tx,
                    )
                    .await;
                });
            })
            .map_err(|e| BosError::Internal(format!("spawn io thread: {e}")))?;

        // Wait for the handshake. The io task enforces the real deadlines;
        // this bound only guards against the task dying without reporting.
        let overall = opts.handshake_timeout + opts.consent_timeout + Duration::from_secs(5);
        let hs = handshake_rx
            .recv_timeout(overall)
            .map_err(|_| BosError::Timeout { ms: overall.as_millis() as u64 })??;

        // NB: the runtime we built above was moved into the io thread.
        // We can't keep our own handle to it; the io thread owns it and the
        // thread join is what shuts it down. Stash the JoinHandle for Drop.
        let inner = ClientInner {
            events_rx,
            outbound_tx,
            request_seq: AtomicU64::new(1),
            initial_session: hs.session,
            via: hs.via,
            app_id: config.app_id,
            _runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| BosError::Internal(format!("tokio runtime build: {e}")))?,
            _io_thread: Some(io_thread),
        };

        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    /// Mock client — synchronous, in-memory, no IPC. For tests.
    fn new_mock(config: BosConfig) -> Self {
        let (_events_tx, events_rx) = unbounded::<BiomeEvent>();
        let (outbound_tx, _outbound_rx) = mpsc::unbounded_channel::<OutboundCommand>();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("mock runtime build");
        Self {
            inner: Arc::new(ClientInner {
                events_rx,
                outbound_tx,
                request_seq: AtomicU64::new(1),
                initial_session: None,
                via: AttachVia::LaunchNonce,
                app_id: config.app_id,
                _runtime: runtime,
                _io_thread: None,
            }),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Sync game-facing API
// ─────────────────────────────────────────────────────────────────────────────

impl BiomeClient {
    /// Drain one inbound event if available. Returns `None` if the queue is
    /// empty. Call this every frame.
    pub fn next_event(&self) -> Option<BiomeEvent> {
        self.inner.events_rx.try_recv().ok()
    }

    /// Drain ALL inbound events at once. Convenience for batch processing.
    pub fn drain_events(&self) -> Vec<BiomeEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = self.inner.events_rx.try_recv() {
            out.push(ev);
        }
        out
    }

    /// Session resolved at handshake. May be `None` if the host was running
    /// in dev mode without an authenticated user.
    pub fn session(&self) -> Option<&Session> {
        self.inner.initial_session.as_ref()
    }

    /// How this connection got in: launched by Biome, or attached with the
    /// user's consent.
    pub fn attached_via(&self) -> AttachVia {
        self.inner.via
    }

    /// The app id this client was configured with.
    pub fn app_id(&self) -> &str {
        &self.inner.app_id
    }

    /// Request the connect bundle for a match. Non-blocking; game polls the
    /// returned handle each frame.
    pub fn match_connect_info(&self, match_id: MatchId) -> RequestHandle<ConnectInfo> {
        self.send_request(
            RequestBody::MatchConnectInfo { match_id },
            |body| match body {
                ResponseBody::ConnectInfo(info) => Ok(info),
                ResponseBody::Error(msg) => Err(BosError::Request(msg)),
                other => Err(BosError::Internal(format!(
                    "unexpected response to MatchConnectInfo: {other:?}"
                ))),
            },
        )
    }

    /// Get session snapshot from server (in case it changed since handshake).
    pub fn fetch_session(&self) -> RequestHandle<Option<Session>> {
        self.send_request(RequestBody::GetSession, |body| match body {
            ResponseBody::Session(s) => Ok(s),
            ResponseBody::Error(msg) => Err(BosError::Request(msg)),
            other => Err(BosError::Internal(format!(
                "unexpected response to GetSession: {other:?}"
            ))),
        })
    }

    /// Ask for a fresh short-lived signed ticket for this app. Hand the
    /// `ticket` string to your match host; it verifies offline against the
    /// published public key (see [`crate::ticket`]). Tickets last minutes;
    /// request a new one whenever you are about to connect somewhere.
    pub fn game_ticket(&self) -> RequestHandle<GameTicket> {
        self.send_request(RequestBody::GameTicket, |body| match body {
            ResponseBody::GameTicket(t) => Ok(t),
            ResponseBody::Error(msg) => Err(BosError::Request(msg)),
            other => Err(BosError::Internal(format!(
                "unexpected response to GameTicket: {other:?}"
            ))),
        })
    }

    // ─── Internal request plumbing ────────────────────────────────────────
    fn send_request<T, F>(&self, body: RequestBody, decode: F) -> RequestHandle<T>
    where
        T: Send + 'static,
        F: FnOnce(ResponseBody) -> Result<T, BosError> + Send + 'static,
    {
        let request_id = self.inner.request_seq.fetch_add(1, Ordering::Relaxed);
        let (raw_tx, raw_rx) = bounded::<Result<ResponseBody, BosError>>(1);

        if self
            .inner
            .outbound_tx
            .send(OutboundCommand::Request {
                body,
                response_tx: raw_tx,
                request_id,
            })
            .is_err()
        {
            // IPC task dead — return a handle that immediately resolves to an error.
            let (decoded_tx, decoded_rx) = bounded(1);
            let _ = decoded_tx.send(Err(BosError::PlatformUnavailable));
            return RequestHandle { rx: decoded_rx };
        }

        // Decode the response on a tiny thread to keep the IPC task free of
        // game-specific decode logic.
        let (decoded_tx, decoded_rx) = bounded::<Result<T, BosError>>(1);
        std::thread::spawn(move || {
            let result = match raw_rx.recv_timeout(Duration::from_millis(REQUEST_TIMEOUT_MS)) {
                Ok(Ok(body)) => decode(body),
                Ok(Err(e)) => Err(e),
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    Err(BosError::Timeout { ms: REQUEST_TIMEOUT_MS })
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    Err(BosError::Internal("response chan dropped".into()))
                }
            };
            let _ = decoded_tx.send(result);
        });
        RequestHandle { rx: decoded_rx }
    }
}

impl Drop for BiomeClient {
    fn drop(&mut self) {
        // Signal the IO task to shut down. Best-effort; if the task is
        // already dead the send fails and we move on.
        let _ = self.inner.outbound_tx.send(OutboundCommand::Shutdown);
        // The Arc may still be held by RequestHandle threads; the io thread
        // and its runtime will join when the last Arc drops.
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// IO task — runs entirely inside the dedicated tokio runtime thread
// ─────────────────────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn run_io_task(
    endpoint: String,
    hello: BosWire,
    via: AttachVia,
    (handshake_timeout, consent_timeout): (Duration, Duration),
    status: Arc<AtomicU8>,
    events_tx: Sender<BiomeEvent>,
    mut outbound_rx: mpsc::UnboundedReceiver<OutboundCommand>,
    handshake_tx: Sender<Result<Handshake, BosError>>,
) {
    let stream = match platform::connect(&endpoint).await {
        Ok(s) => s,
        Err(e) => {
            let _ = handshake_tx.send(Err(e));
            return;
        }
    };

    let mut framed = transport::frame(stream);

    // 1. Send the hello (ClientHello or AttachHello)
    if let Err(e) = transport::send_wire(&mut framed, &hello).await {
        let _ = handshake_tx.send(Err(e));
        return;
    }

    // 2. Wait for the host's answer. `AwaitingConsent` extends the wait to
    //    the consent timeout; the next frame must be Ok or Rejected.
    let mut deadline = handshake_timeout;
    let session = loop {
        match timeout(deadline, transport::recv_wire(&mut framed)).await {
            Ok(Ok(Some(BosWire::V1(BosMsgV1::ServerHello(ServerHello::Ok {
                session,
                min_supported_sdk,
                ..
            }))))) => {
                if CURRENT_SDK_VERSION < min_supported_sdk {
                    let _ = handshake_tx.send(Err(BosError::SdkTooOld {
                        sdk: CURRENT_SDK_VERSION,
                        min_required: min_supported_sdk,
                    }));
                    return;
                }
                break session;
            }
            Ok(Ok(Some(BosWire::V1(BosMsgV1::ServerHello(ServerHello::AwaitingConsent { .. }))))) => {
                status.store(STATUS_AWAITING_CONSENT, Ordering::Relaxed);
                deadline = consent_timeout;
                continue;
            }
            Ok(Ok(Some(BosWire::V1(BosMsgV1::ServerHello(ServerHello::Rejected { reason }))))) => {
                let _ = handshake_tx.send(Err(BosError::from_reject_reason(&reason)));
                return;
            }
            Ok(Ok(Some(_))) => {
                let _ = handshake_tx.send(Err(BosError::HandshakeRejected(
                    "first frame was not ServerHello".into(),
                )));
                return;
            }
            Ok(Ok(None)) => {
                let _ = handshake_tx.send(Err(BosError::HandshakeRejected(
                    "server closed before hello".into(),
                )));
                return;
            }
            Ok(Err(e)) => {
                let _ = handshake_tx.send(Err(e));
                return;
            }
            Err(_) => {
                let err = if status.load(Ordering::Relaxed) == STATUS_AWAITING_CONSENT {
                    BosError::ConsentTimeout { secs: consent_timeout.as_secs() }
                } else {
                    BosError::Timeout { ms: handshake_timeout.as_millis() as u64 }
                };
                let _ = handshake_tx.send(Err(err));
                return;
            }
        }
    };

    let _ = handshake_tx.send(Ok(Handshake { session, via }));

    // 3. Main loop: multiplex inbound frames and outbound commands.
    let mut pending: std::collections::HashMap<
        u64,
        crossbeam_channel::Sender<Result<ResponseBody, BosError>>,
    > = std::collections::HashMap::new();

    loop {
        tokio::select! {
            // Inbound from server
            wire = transport::recv_wire(&mut framed) => {
                match wire {
                    Ok(Some(BosWire::V1(BosMsgV1::Push(ev)))) => {
                        let _ = events_tx.send(ev);
                    }
                    Ok(Some(BosWire::V1(BosMsgV1::Response { request_id, body }))) => {
                        if let Some(tx) = pending.remove(&request_id) {
                            let _ = tx.send(Ok(body));
                        }
                    }
                    Ok(Some(BosWire::V1(BosMsgV1::Ping))) => {
                        let _ = transport::send_wire(&mut framed, &BosWire::V1(BosMsgV1::Pong)).await;
                    }
                    Ok(Some(_)) => {
                        // Unexpected frame; log + ignore.
                        tracing::warn!("unexpected inbound frame");
                    }
                    Ok(None) => {
                        tracing::warn!("BOS server closed connection");
                        // Surface as a disconnected event so the game can show
                        // a banner. Real reconnect logic is week-3 work.
                        let _ = events_tx.send(BiomeEvent::PlatformDisconnected);
                        return;
                    }
                    Err(e) => {
                        tracing::error!("BOS recv error: {e}");
                        let _ = events_tx.send(BiomeEvent::PlatformDisconnected);
                        return;
                    }
                }
            }

            // Outbound from game thread
            cmd = outbound_rx.recv() => {
                match cmd {
                    Some(OutboundCommand::Request { body, response_tx, request_id }) => {
                        pending.insert(request_id, response_tx.clone());
                        let frame = BosWire::V1(BosMsgV1::Request { request_id, body });
                        if let Err(e) = transport::send_wire(&mut framed, &frame).await {
                            // Send failed; resolve the pending entry with an error.
                            if let Some(tx) = pending.remove(&request_id) {
                                let _ = tx.send(Err(e));
                            }
                            return;
                        }
                    }
                    Some(OutboundCommand::Shutdown) | None => {
                        tracing::info!("BOS client shutting down");
                        return;
                    }
                }
            }
        }
    }
}
