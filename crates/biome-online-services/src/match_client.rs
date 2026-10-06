//! [`MatchClient`] — play a hosted match from sync game code (feature
//! `match-client`).
//!
//! The game never writes networking code. It hands over a game ticket (from
//! [`crate::BiomeClient::game_ticket`]) and says "create a match" or "join
//! this code"; the client gets a seat from the match host over HTTP, opens
//! the WebSocket, and from then on the game polls events and sends moves.
//!
//! ```ignore
//! let ticket = bos.game_ticket().wait()?.ticket;
//! let m = MatchClient::start(
//!     MatchEndpoint::new("https://biome-match.example.workers.dev", "mecha-aether", MechaAether::VERSION),
//!     ticket,
//!     Entry::Create { config: codec::encode(&cfg)?, clock: None },
//! );
//! // every frame:
//! for ev in m.drain() {
//!     match ev {
//!         MatchEvent::Seated(info) => show_join_code(info.join_code),
//!         MatchEvent::Welcome(w) => state = w.view,          // full picture (also after a reconnect)
//!         MatchEvent::Update(u) => { animate(u.events); state = u.view; }
//!         MatchEvent::Rejected { code, .. } => toast(code),
//!         MatchEvent::Over { outcome } => show_result(outcome),
//!         MatchEvent::Disconnected { reconnecting: true } => show_spinner(),
//!         _ => {}
//!     }
//! }
//! m.send_move(codec::encode(&my_move)?);
//! ```
//!
//! Threading is the same as [`crate::BiomeClient`]: a dedicated OS thread
//! owns a tokio runtime and the socket; the game side is channels.
//!
//! ## What it handles for you
//!
//! - **Reconnection.** The seat token returned with the seat is kept; when
//!   the socket drops the client reconnects with backoff and says `hello`
//!   again. The host answers with a full [`Welcome`], so game code treats a
//!   reconnect like any other state refresh.
//! - **Moves in flight.** Each move gets an increasing `client_seq`. Moves
//!   not yet confirmed when the socket dropped are resent after the
//!   reconnect; the host's `last_client_seq` says which were already applied,
//!   and it ignores duplicates anyway. The numbers you see start at 1 for
//!   every `MatchClient`; on the wire they continue from what the host
//!   remembers for the seat, so a client started with [`Entry::Resume`] or
//!   [`Entry::Rejoin`] (a restarted game) is not mistaken for one retrying
//!   old moves.
//! - **Giving up when it should.** A wrong seat token, a rules version the
//!   host doesn't accept, being replaced by a newer connection, or an
//!   abandoned match end the client instead of looping.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use crossbeam_channel::{unbounded, Receiver, Sender};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

pub use biome_rules::Outcome;

/// Where the match host lives and which game this is.
#[derive(Clone, Debug)]
pub struct MatchEndpoint {
    /// `https://…` (production) or `http://127.0.0.1:8787` (`biome-match-dev`).
    pub base_url: String,
    /// The registered app id; also the ticket's audience.
    pub app_id: String,
    /// The rules version this build of the game runs (`Rules::VERSION`).
    /// The host refuses a client whose version differs from the match's.
    pub rules_version: String,
}

impl MatchEndpoint {
    pub fn new(base_url: impl Into<String>, app_id: impl Into<String>, rules_version: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            app_id: app_id.into(),
            rules_version: rules_version.into(),
        }
    }

    fn http(&self, path: &str) -> String {
        format!("{}/v1/{}{}", self.base_url, self.app_id, path)
    }

    fn ws_base(&self) -> String {
        if let Some(rest) = self.base_url.strip_prefix("https://") {
            format!("wss://{rest}")
        } else if let Some(rest) = self.base_url.strip_prefix("http://") {
            format!("ws://{rest}")
        } else {
            self.base_url.clone()
        }
    }

    fn ws(&self, match_id: &str) -> String {
        format!("{}/v1/{}/matches/{match_id}/ws", self.ws_base(), self.app_id)
    }

    /// The queue socket for `mode` at this endpoint's rules version.
    fn queue_ws(&self, mode: &str) -> String {
        format!(
            "{}/v1/{}/queue?mode={}&rules_version={}",
            self.ws_base(),
            self.app_id,
            encode_query(mode),
            encode_query(&self.rules_version)
        )
    }

    /// The public listing of open matches at this endpoint's rules version.
    fn open_url(&self) -> String {
        format!("{}/v1/{}/open?rules_version={}", self.base_url, self.app_id, encode_query(&self.rules_version))
    }
}

/// Percent-encode a query value (everything but RFC 3986 unreserved
/// characters), so a version like `1.0.0+build` survives the trip.
fn encode_query(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Optional chess-style clock, enforced by the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockConfig {
    pub initial_ms: u64,
    pub increment_ms: u64,
}

/// How to get a seat.
#[derive(Clone, Debug)]
pub enum Entry {
    /// Start a new match. `config` is the game's own (postcard) config
    /// bytes. The creator takes seat 0 and gets a join code to share.
    Create { config: Vec<u8>, clock: Option<ClockConfig> },
    /// Join a match by its code.
    Join { code: String },
    /// You hold a seat in this match but lost the seat token (the game was
    /// restarted). Needs a fresh ticket; the old token stops working.
    Rejoin { match_id: String },
    /// You still have the seat token (saved from [`SeatedInfo`]). No ticket
    /// is used.
    Resume { match_id: String, seat_token: String },
    /// Make a match that is listed for anyone to join (it also gets a code).
    Open { config: Vec<u8>, clock: Option<ClockConfig> },
    /// Take the open seat of a listed match.
    JoinOpen { match_id: String },
    /// Wait in the queue for `mode` until paired. "ranked" changes ratings; anything else is casual.
    Queue { mode: String, config: Vec<u8>, clock: Option<ClockConfig> },
}

/// A player's rating in one mode of one game (Glicko-2). Everyone starts at
/// 1500 with a deviation of 350; the deviation shrinks as matches are played.
#[derive(Clone, Debug, PartialEq)]
pub struct Rating {
    pub rating: f64,
    pub deviation: f64,
    pub matches: u32,
}

/// A listed match waiting for a player (see [`list_open`], [`Entry::JoinOpen`]).
#[derive(Clone, Debug, PartialEq)]
pub struct OpenMatch {
    pub match_id: String,
    /// The username of the player who opened it.
    pub host: String,
    /// When it was opened, milliseconds since the Unix epoch.
    pub created_at_ms: u64,
    pub rules_version: String,
    /// The game's own (postcard) config bytes the match was opened with.
    pub config: Vec<u8>,
}

/// The seat this client holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeatedInfo {
    pub match_id: String,
    /// Share this with the other player(s). Present for create / join and
    /// open matches; `None` after queue pairing.
    pub join_code: Option<String>,
    pub seat: u8,
    /// Secret for this seat. Save it to [`Entry::Resume`] after a restart.
    pub seat_token: String,
    pub rules_version: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeatStatus {
    pub seat: u8,
    pub username: String,
    pub connected: bool,
}

/// The full picture for this seat. Sent on connect, on every reconnect, and
/// when the match starts.
#[derive(Clone, Debug, PartialEq)]
pub struct Welcome {
    /// `waiting`, `active`, `over` or `abandoned`.
    pub status: String,
    pub seat: u8,
    /// Number of moves applied so far.
    pub seq: u64,
    /// This seat's view (postcard `Rules::View`); `None` until the match starts.
    pub view: Option<Vec<u8>>,
    pub to_move: Vec<u8>,
    /// Remaining milliseconds per seat, if the match has a clock.
    pub clock: Option<BTreeMap<u8, u64>>,
    pub seats: Vec<SeatStatus>,
    pub outcome: Option<Outcome>,
}

/// One applied move (or timeout), as this seat is allowed to see it.
#[derive(Clone, Debug, PartialEq)]
pub struct Update {
    pub seq: u64,
    /// The seat that moved (or whose clock ran out).
    pub by: u8,
    /// `move` or `timeout`.
    pub kind: String,
    /// Events addressed to this seat (postcard `Rules::Event`), in order.
    pub events: Vec<Vec<u8>>,
    pub view: Option<Vec<u8>>,
    pub to_move: Vec<u8>,
    pub clock: Option<BTreeMap<u8, u64>>,
}

impl Welcome {
    /// Decode the view with the game's own type.
    pub fn view_as<R: biome_rules::Rules>(&self) -> Option<Result<R::View, String>> {
        self.view.as_deref().map(biome_rules::codec::decode::<R::View>)
    }
}

impl Update {
    pub fn view_as<R: biome_rules::Rules>(&self) -> Option<Result<R::View, String>> {
        self.view.as_deref().map(biome_rules::codec::decode::<R::View>)
    }

    /// Decode the events with the game's own type.
    pub fn events_as<R: biome_rules::Rules>(&self) -> Result<Vec<R::Event>, String> {
        self.events.iter().map(|e| biome_rules::codec::decode::<R::Event>(e)).collect()
    }
}

/// What the game polls.
#[derive(Clone, Debug, PartialEq)]
pub enum MatchEvent {
    /// The host gave us a seat; the socket is connecting.
    Seated(SeatedInfo),
    Welcome(Welcome),
    Update(Update),
    /// The host refused one of our moves. `code` is stable (`not-your-turn`,
    /// `illegal-move`, `out-of-time`, …).
    Rejected { client_seq: Option<u64>, code: String, message: String },
    /// Someone joined, connected or dropped.
    Seats { status: String, seats: Vec<SeatStatus> },
    /// The match ended. Sent exactly once per client, whether the end
    /// arrives live or in the [`Welcome`] after a reconnect (the game may
    /// have ended while this client was offline).
    Over { outcome: Outcome },
    /// The socket dropped. `reconnecting: false` means the client stopped
    /// (see the [`MatchEvent::Error`] or [`MatchEvent::Failed`] before it).
    Disconnected { reconnecting: bool },
    /// An error frame from the host (`rules-version-mismatch`, `replaced`,
    /// `bad-seat-token`, `abandoned`, `not-enough-seats`, …).
    Error { code: String, detail: String },
    /// Could not get a seat at all: `unauthorized`, `unknown-code`,
    /// `match-started`, `match-full`, `no-such-match`, `bad-config`,
    /// `network`, …; from the queue also `queue-timeout`, `rate-limited`,
    /// `replaced` (the same player queued again elsewhere), `match-failed`.
    Failed { code: String, message: String },
    /// In the queue. Sent on entry and when the number of players waiting changes.
    Queued { mode: String, waiting: u32, rating: Option<Rating> },
    // Pairing ends in the existing `Seated(SeatedInfo)` (join_code: None), then Welcome as usual.
}

enum Cmd {
    Move { client_seq: u64, mv: Vec<u8> },
    Start,
    Sync,
    Close,
}

/// A match in progress, from this seat's side. Drop it to leave.
pub struct MatchClient {
    events_rx: Receiver<MatchEvent>,
    cmd_tx: mpsc::UnboundedSender<Cmd>,
    next_seq: Arc<AtomicU64>,
}

const BACKOFF_MS: [u64; 6] = [300, 600, 1200, 2500, 5000, 10_000];
/// Host error codes after which reconnecting cannot help.
const FATAL: [&str; 6] = ["bad-seat-token", "rules-version-mismatch", "replaced", "abandoned", "no-such-match", "match-fault"];

impl MatchClient {
    /// Get a seat and connect. Returns at once; watch the events.
    ///
    /// `ticket` is a game ticket for `endpoint.app_id` (or `dev:<id>:<name>`
    /// against `biome-match-dev`). It is used once, for the HTTP call (or
    /// the queue's `hello`); the socket and reconnects use the seat token.
    /// [`Entry::Resume`] ignores it.
    ///
    /// With [`Entry::Queue`] the client reports [`MatchEvent::Queued`] until
    /// it is paired, then [`MatchEvent::Seated`] and carries on as after a
    /// Resume. Dropping the client while queued leaves the queue.
    pub fn start(endpoint: MatchEndpoint, ticket: impl Into<String>, entry: Entry) -> Self {
        let (events_tx, events_rx) = unbounded::<MatchEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Cmd>();
        let ticket = ticket.into();
        let failed_tx = events_tx.clone();

        let spawned = std::thread::Builder::new().name("biome-match-io".into()).spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = events_tx.send(MatchEvent::Failed { code: "internal".into(), message: format!("tokio runtime: {e}") });
                    return;
                }
            };
            runtime.block_on(run(endpoint, ticket, entry, events_tx, cmd_rx));
        });
        if let Err(e) = spawned {
            let _ = failed_tx.send(MatchEvent::Failed { code: "internal".into(), message: format!("spawn io thread: {e}") });
        }
        Self { events_rx, cmd_tx, next_seq: Arc::new(AtomicU64::new(1)) }
    }

    /// One event if there is one. Call every frame.
    pub fn next_event(&self) -> Option<MatchEvent> {
        self.events_rx.try_recv().ok()
    }

    /// Every event that has arrived.
    pub fn drain(&self) -> Vec<MatchEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = self.events_rx.try_recv() {
            out.push(ev);
        }
        out
    }

    /// Blocking wait for the next event — tools and tests only.
    pub fn wait_event(&self, timeout: Duration) -> Option<MatchEvent> {
        self.events_rx.recv_timeout(timeout).ok()
    }

    /// Send a move (postcard `Rules::Move`). Returns its `client_seq`, which
    /// a [`MatchEvent::Rejected`] for it will carry. Safe to call while
    /// disconnected: it is sent after the reconnect.
    pub fn send_move(&self, mv: Vec<u8>) -> u64 {
        let client_seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let _ = self.cmd_tx.send(Cmd::Move { client_seq, mv });
        client_seq
    }

    /// Encode and send a move with the game's own type.
    pub fn send_typed<R: biome_rules::Rules>(&self, mv: &R::Move) -> Result<u64, String> {
        Ok(self.send_move(biome_rules::codec::encode(mv)?))
    }

    /// Seat 0 only: start a waiting match once the minimum seats are filled.
    pub fn request_start(&self) {
        let _ = self.cmd_tx.send(Cmd::Start);
    }

    /// Ask the host for a fresh [`Welcome`].
    pub fn sync(&self) {
        let _ = self.cmd_tx.send(Cmd::Sync);
    }
}

impl Drop for MatchClient {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(Cmd::Close);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// IO task
// ─────────────────────────────────────────────────────────────────────────────

struct Seat {
    seat_token: String,
    ws_url: String,
}

async fn run(
    endpoint: MatchEndpoint,
    ticket: String,
    entry: Entry,
    events: Sender<MatchEvent>,
    mut cmds: mpsc::UnboundedReceiver<Cmd>,
) {
    // Moves the game makes before the first welcome (while queued, say) wait
    // here and are sent once the match socket is up.
    let mut pending: VecDeque<(u64, Vec<u8>)> = VecDeque::new();

    // 1. Get a seat.
    let seat = match entry {
        Entry::Resume { match_id, seat_token } => {
            let ws_url = endpoint.ws(&match_id);
            Seat { seat_token, ws_url }
        }
        Entry::Queue { mode, config, clock } => {
            match queue(&endpoint, &ticket, &mode, &config, clock, &events, &mut cmds, &mut pending).await {
                QueueEnd::Paired(info) => {
                    // From here on it is exactly a Resume with the token the
                    // queue handed over.
                    let seat = Seat { seat_token: info.seat_token.clone(), ws_url: endpoint.ws(&info.match_id) };
                    let _ = events.send(MatchEvent::Seated(info));
                    seat
                }
                QueueEnd::Failed { code, message } => {
                    let _ = events.send(MatchEvent::Failed { code, message });
                    let _ = events.send(MatchEvent::Disconnected { reconnecting: false });
                    return;
                }
                QueueEnd::Closed => {
                    let _ = events.send(MatchEvent::Disconnected { reconnecting: false });
                    return;
                }
            }
        }
        other => match take_seat(&endpoint, &ticket, &other).await {
            Ok((info, ws_url)) => {
                let seat = Seat { seat_token: info.seat_token.clone(), ws_url };
                let _ = events.send(MatchEvent::Seated(info));
                seat
            }
            Err((code, message)) => {
                let _ = events.send(MatchEvent::Failed { code, message });
                let _ = events.send(MatchEvent::Disconnected { reconnecting: false });
                return;
            }
        },
    };

    // 2. Connect, and keep reconnecting until told to stop.
    // What the host had already counted for this seat when this client first
    // connected. `client_seq` on the wire = ours + this.
    let mut seq_base: Option<u64> = None;
    let mut over_sent = false;
    let mut attempt = 0usize;
    loop {
        match session(&endpoint, &seat, &events, &mut cmds, &mut pending, &mut seq_base, &mut over_sent).await {
            SessionEnd::Closed | SessionEnd::Fatal => {
                let _ = events.send(MatchEvent::Disconnected { reconnecting: false });
                return;
            }
            SessionEnd::Dropped { was_connected } => {
                let _ = events.send(MatchEvent::Disconnected { reconnecting: true });
                if was_connected {
                    attempt = 0;
                }
            }
        }
        let wait = Duration::from_millis(BACKOFF_MS[attempt.min(BACKOFF_MS.len() - 1)]);
        attempt += 1;
        // Keep accepting commands while waiting, so a move made offline is
        // queued and a Close ends the client promptly.
        let sleep = tokio::time::sleep(wait);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                _ = &mut sleep => break,
                cmd = cmds.recv() => match cmd {
                    Some(Cmd::Move { client_seq, mv }) => pending.push_back((client_seq, mv)),
                    Some(Cmd::Close) | None => {
                        let _ = events.send(MatchEvent::Disconnected { reconnecting: false });
                        return;
                    }
                    Some(Cmd::Start) | Some(Cmd::Sync) => {}
                },
            }
        }
    }
}

fn clock_json(clock: Option<ClockConfig>) -> Value {
    clock.map(|c| json!({ "initial_ms": c.initial_ms, "increment_ms": c.increment_ms })).unwrap_or(Value::Null)
}

/// The HTTP request that buys a seat for `entry`; `None` for the entries
/// that don't make one (Resume has its token, Queue uses a socket).
fn seat_request(endpoint: &MatchEndpoint, entry: &Entry) -> Option<(String, Value)> {
    match entry {
        Entry::Create { config, clock } => Some((
            endpoint.http("/matches"),
            json!({ "config": B64.encode(config), "rules_version": endpoint.rules_version, "clock": clock_json(*clock) }),
        )),
        Entry::Open { config, clock } => Some((
            endpoint.http("/matches"),
            json!({ "config": B64.encode(config), "rules_version": endpoint.rules_version, "clock": clock_json(*clock), "listed": true }),
        )),
        Entry::Join { code } => Some((endpoint.http("/matches/join"), json!({ "join_code": code }))),
        Entry::JoinOpen { match_id } => Some((endpoint.http(&format!("/matches/{}/join", encode_query(match_id))), json!({}))),
        Entry::Rejoin { match_id } => Some((endpoint.http(&format!("/matches/{}/rejoin", encode_query(match_id))), json!({}))),
        Entry::Resume { .. } | Entry::Queue { .. } => None,
    }
}

async fn take_seat(endpoint: &MatchEndpoint, ticket: &str, entry: &Entry) -> Result<(SeatedInfo, String), (String, String)> {
    let Some((url, body)) = seat_request(endpoint, entry) else {
        return Err(("internal".into(), "this entry does not take a seat over HTTP".into()));
    };

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| ("internal".to_string(), e.to_string()))?;
    let res = client
        .post(&url)
        .bearer_auth(ticket)
        .json(&body)
        .send()
        .await
        .map_err(|e| ("network".to_string(), e.to_string()))?;
    let status = res.status();
    let text = res.text().await.map_err(|e| ("network".to_string(), e.to_string()))?;
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if !status.is_success() {
        let code = v.get("error").and_then(Value::as_str).unwrap_or("http-error").to_string();
        let message = v.get("message").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| format!("http {status}"));
        return Err((code, message));
    }

    let field = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    let (Some(match_id), Some(seat_token)) = (field("match_id"), field("seat_token")) else {
        return Err(("bad-response".into(), text));
    };
    let rules_version = field("rules_version").unwrap_or_default();
    if rules_version != endpoint.rules_version {
        // Joining a match that runs other rules than this build: say so now
        // instead of failing on hello.
        return Err(("rules-version-mismatch".into(), format!("match runs {rules_version}, this client runs {}", endpoint.rules_version)));
    }
    let ws_url = field("ws_url").unwrap_or_else(|| endpoint.ws(&match_id));
    Ok((
        SeatedInfo {
            match_id,
            join_code: field("join_code"),
            seat: v.get("seat").and_then(Value::as_u64).unwrap_or(0) as u8,
            seat_token,
            rules_version,
        },
        ws_url,
    ))
}

enum QueueEnd {
    Paired(SeatedInfo),
    Failed { code: String, message: String },
    /// The game dropped the client while it was queued.
    Closed,
}

/// How often a queued client says `ping`, so idle proxies keep the socket.
/// The host answers without waking the queue object.
const QUEUE_PING: Duration = Duration::from_secs(30);

fn queue_failed(code: &str, message: impl Into<String>) -> QueueEnd {
    QueueEnd::Failed { code: code.to_string(), message: message.into() }
}

/// Wait in the queue until paired. Moves the game sends meanwhile are kept
/// in `pending` for the match socket.
#[allow(clippy::too_many_arguments)]
async fn queue(
    endpoint: &MatchEndpoint,
    ticket: &str,
    mode: &str,
    config: &[u8],
    clock: Option<ClockConfig>,
    events: &Sender<MatchEvent>,
    cmds: &mut mpsc::UnboundedReceiver<Cmd>,
    pending: &mut VecDeque<(u64, Vec<u8>)>,
) -> QueueEnd {
    use tokio_tungstenite::tungstenite::Error as WsError;

    let url = endpoint.queue_ws(mode);
    let ws = match tokio::time::timeout(Duration::from_secs(15), tokio_tungstenite::connect_async(url.as_str())).await {
        Ok(Ok((ws, _))) => ws,
        // The host refused the upgrade (unknown app or rules version, bad
        // mode): it says why in a JSON body.
        Ok(Err(WsError::Http(res))) => {
            let status = res.status();
            let v: Value = res.body().as_deref().and_then(|b| serde_json::from_slice(b).ok()).unwrap_or(Value::Null);
            let code = v.get("error").and_then(Value::as_str).unwrap_or("http-error").to_string();
            let message = v.get("message").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| format!("http {status}"));
            return QueueEnd::Failed { code, message };
        }
        Ok(Err(e)) => return queue_failed("network", e.to_string()),
        Err(_) => return queue_failed("network", "timed out connecting to the queue"),
    };
    let (mut sink, mut stream) = ws.split();
    let hello = json!({ "t": "hello", "ticket": ticket, "config": B64.encode(config), "clock": clock_json(clock) });
    if sink.send(Message::Text(hello.to_string())).await.is_err() {
        return queue_failed("network", "the queue connection closed");
    }

    let mut ping = tokio::time::interval_at(tokio::time::Instant::now() + QUEUE_PING, QUEUE_PING);
    loop {
        tokio::select! {
            frame = stream.next() => {
                let text = match frame {
                    Some(Ok(Message::Text(t))) => t,
                    Some(Ok(Message::Ping(p))) => {
                        let _ = sink.send(Message::Pong(p)).await;
                        continue;
                    }
                    Some(Ok(Message::Pong(_))) | Some(Ok(Message::Frame(_))) => continue,
                    // The ticket is spent: a dropped queue socket cannot be
                    // resumed. The game asks for a new ticket and queues again.
                    _ => return queue_failed("network", "the queue connection closed"),
                };
                let Ok(v) = serde_json::from_str::<Value>(text.as_str()) else { continue };
                match v.get("t").and_then(Value::as_str).unwrap_or("") {
                    "queued" => {
                        let waiting = v.get("waiting").and_then(Value::as_u64).map(|n| n.min(u64::from(u32::MAX)) as u32).unwrap_or(0);
                        let _ = events.send(MatchEvent::Queued { mode: mode.to_string(), waiting, rating: parse_rating(v.get("rating")) });
                    }
                    "paired" => {
                        let field = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
                        let (Some(match_id), Some(seat_token)) = (field("match_id"), field("seat_token")) else {
                            return queue_failed("bad-response", text.to_string());
                        };
                        let rules_version = field("rules_version").unwrap_or_else(|| endpoint.rules_version.clone());
                        let _ = sink.send(Message::Close(None)).await;
                        if rules_version != endpoint.rules_version {
                            return queue_failed("rules-version-mismatch", format!("match runs {rules_version}, this client runs {}", endpoint.rules_version));
                        }
                        let seat = v.get("seat").and_then(Value::as_u64).unwrap_or(0).min(u64::from(u8::MAX)) as u8;
                        return QueueEnd::Paired(SeatedInfo { match_id, join_code: None, seat, seat_token, rules_version });
                    }
                    "error" => {
                        let code = str_of(&v, "code");
                        let message = v.get("message").or_else(|| v.get("detail")).and_then(Value::as_str).map(str::to_string).unwrap_or_default();
                        return QueueEnd::Failed { code, message };
                    }
                    _ => {}
                }
            }
            cmd = cmds.recv() => match cmd {
                Some(Cmd::Move { client_seq, mv }) => pending.push_back((client_seq, mv)),
                Some(Cmd::Start) | Some(Cmd::Sync) => {}
                Some(Cmd::Close) | None => {
                    // Closing the socket is what takes us out of the queue.
                    let _ = sink.send(Message::Close(None)).await;
                    return QueueEnd::Closed;
                }
            },
            _ = ping.tick() => {
                if sink.send(Message::Text(json!({ "t": "ping" }).to_string())).await.is_err() {
                    return queue_failed("network", "the queue connection closed");
                }
            }
        }
    }
}

fn parse_rating(v: Option<&Value>) -> Option<Rating> {
    let o = v?.as_object()?;
    Some(Rating {
        rating: o.get("rating")?.as_f64()?,
        deviation: o.get("deviation")?.as_f64()?,
        matches: o.get("matches").and_then(Value::as_u64).unwrap_or(0).min(u64::from(u32::MAX)) as u32,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Open matches
// ─────────────────────────────────────────────────────────────────────────────

/// The open matches of the endpoint's game and rules version, fetched in the background.
///
/// No ticket is needed: the listing is public. At most 50, newest first.
/// Take one with [`Entry::JoinOpen`].
pub fn list_open(endpoint: &MatchEndpoint) -> OpenListing {
    let (tx, rx) = crossbeam_channel::bounded::<Result<Vec<OpenMatch>, String>>(1);
    let url = endpoint.open_url();
    // If the thread cannot be spawned the closure (and the sender) is
    // dropped, and `poll` reports the closed channel as an error.
    let _ = std::thread::Builder::new().name("biome-match-open".into()).spawn(move || {
        let result = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt.block_on(fetch_open(&url)),
            Err(e) => Err(format!("internal: tokio runtime: {e}")),
        };
        let _ = tx.send(result);
    });
    OpenListing { rx, taken: AtomicBool::new(false) }
}

/// A listing in progress (see [`list_open`]).
pub struct OpenListing {
    rx: Receiver<Result<Vec<OpenMatch>, String>>,
    taken: AtomicBool,
}

impl OpenListing {
    /// Non-blocking. `Some` exactly once, when the fetch finished either
    /// way; `None` while it is running and after the result was taken.
    pub fn poll(&self) -> Option<Result<Vec<OpenMatch>, String>> {
        if self.taken.load(Ordering::Acquire) {
            return None;
        }
        let result = match self.rx.try_recv() {
            Ok(r) => r,
            Err(crossbeam_channel::TryRecvError::Empty) => return None,
            Err(crossbeam_channel::TryRecvError::Disconnected) => Err("internal: the listing thread ended".to_string()),
        };
        self.taken.store(true, Ordering::Release);
        Some(result)
    }
}

async fn fetch_open(url: &str) -> Result<Vec<OpenMatch>, String> {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(15)).build().map_err(|e| format!("internal: {e}"))?;
    let res = client.get(url).send().await.map_err(|e| format!("network: {e}"))?;
    let status = res.status();
    let text = res.text().await.map_err(|e| format!("network: {e}"))?;
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if !status.is_success() {
        let code = v.get("error").and_then(Value::as_str).unwrap_or("http-error");
        return Err(format!("{code}: http {status}"));
    }
    let Some(items) = v.as_array() else { return Err(format!("bad-response: {text}")) };
    Ok(items.iter().filter_map(parse_open_match).collect())
}

fn parse_open_match(v: &Value) -> Option<OpenMatch> {
    Some(OpenMatch {
        match_id: v.get("match_id")?.as_str()?.to_string(),
        host: v.get("host").and_then(Value::as_str).unwrap_or_default().to_string(),
        created_at_ms: v.get("created_at_ms").and_then(Value::as_u64).unwrap_or(0),
        rules_version: v.get("rules_version").and_then(Value::as_str).unwrap_or_default().to_string(),
        config: v.get("config").and_then(Value::as_str).and_then(|s| B64.decode(s).ok()).unwrap_or_default(),
    })
}

enum SessionEnd {
    /// The game dropped the client.
    Closed,
    /// The host said something reconnecting can't fix.
    Fatal,
    /// The socket went away (or never came up).
    Dropped { was_connected: bool },
}

async fn session(
    endpoint: &MatchEndpoint,
    seat: &Seat,
    events: &Sender<MatchEvent>,
    cmds: &mut mpsc::UnboundedReceiver<Cmd>,
    pending: &mut VecDeque<(u64, Vec<u8>)>,
    seq_base: &mut Option<u64>,
    over_sent: &mut bool,
) -> SessionEnd {
    let (ws, _) = match tokio::time::timeout(Duration::from_secs(15), tokio_tungstenite::connect_async(seat.ws_url.as_str())).await {
        Ok(Ok(pair)) => pair,
        _ => return SessionEnd::Dropped { was_connected: false },
    };
    let (mut sink, mut stream) = ws.split();
    let hello = json!({ "t": "hello", "seat_token": seat.seat_token, "rules_version": endpoint.rules_version });
    if sink.send(Message::Text(hello.to_string())).await.is_err() {
        return SessionEnd::Dropped { was_connected: false };
    }

    let mut my_seat: Option<u8> = None;
    let mut welcomed = false;
    let mut over = false;

    loop {
        tokio::select! {
            frame = stream.next() => {
                let text = match frame {
                    Some(Ok(Message::Text(t))) => t,
                    Some(Ok(Message::Ping(p))) => {
                        let _ = sink.send(Message::Pong(p)).await;
                        continue;
                    }
                    Some(Ok(Message::Pong(_))) | Some(Ok(Message::Frame(_))) => continue,
                    // Binary, close, error, end of stream.
                    _ => return if over { SessionEnd::Closed } else { SessionEnd::Dropped { was_connected: welcomed } },
                };
                let Ok(v) = serde_json::from_str::<Value>(text.as_str()) else { continue };
                match v.get("t").and_then(Value::as_str).unwrap_or("") {
                    "welcome" => {
                        let w = parse_welcome(&v);
                        my_seat = Some(w.seat);
                        over = w.status == "over" || w.status == "abandoned";
                        // Drop what the host already applied, resend the rest
                        // (only on the first welcome of this connection).
                        let applied = v.get("last_client_seq").and_then(Value::as_u64).unwrap_or(0);
                        // The first welcome this client ever gets fixes the
                        // base: whatever the host counted before us belongs
                        // to an earlier client for the same seat (the game
                        // was restarted and resumed or rejoined). Without
                        // this our moves 1, 2, … would look like retries of
                        // moves the host already has, and be dropped.
                        let base = *seq_base.get_or_insert(applied);
                        pending.retain(|(seq, _)| seq.saturating_add(base) > applied);
                        let first = !welcomed;
                        welcomed = true;
                        let ended = w.outcome.clone();
                        let _ = events.send(MatchEvent::Welcome(w));
                        // The match may have ended while we were away.
                        if let (Some(outcome), false) = (ended, *over_sent) {
                            *over_sent = true;
                            let _ = events.send(MatchEvent::Over { outcome });
                        }
                        if first {
                            for (client_seq, mv) in pending.iter() {
                                let frame = json!({ "t": "move", "client_seq": client_seq.saturating_add(base), "move": B64.encode(mv) });
                                if sink.send(Message::Text(frame.to_string())).await.is_err() {
                                    return SessionEnd::Dropped { was_connected: true };
                                }
                            }
                        }
                    }
                    "update" => {
                        let u = parse_update(&v);
                        // Our own applied move confirms the oldest one in flight.
                        if Some(u.by) == my_seat && u.kind == "move" {
                            pending.pop_front();
                        }
                        let _ = events.send(MatchEvent::Update(u));
                    }
                    "rejected" => {
                        // Back to the number `send_move` returned.
                        let base = seq_base.unwrap_or(0);
                        let client_seq = v.get("client_seq").and_then(Value::as_u64).map(|wire| wire.saturating_sub(base));
                        if let Some(seq) = client_seq {
                            pending.retain(|(s, _)| *s != seq);
                        }
                        let _ = events.send(MatchEvent::Rejected {
                            client_seq,
                            code: str_of(&v, "code"),
                            message: str_of(&v, "message"),
                        });
                    }
                    "seats" => {
                        let _ = events.send(MatchEvent::Seats { status: str_of(&v, "status"), seats: parse_seats(v.get("seats")) });
                    }
                    "over" => {
                        over = true;
                        pending.clear();
                        if let Some(outcome) = v.get("outcome").and_then(|o| serde_json::from_value::<Outcome>(o.clone()).ok()) {
                            if !*over_sent {
                                *over_sent = true;
                                let _ = events.send(MatchEvent::Over { outcome });
                            }
                        }
                    }
                    "error" => {
                        let code = str_of(&v, "code");
                        let fatal = FATAL.contains(&code.as_str());
                        let detail = v.get("server").or_else(|| v.get("detail")).or_else(|| v.get("need")).map(|d| d.to_string().trim_matches('"').to_string()).unwrap_or_default();
                        let _ = events.send(MatchEvent::Error { code, detail });
                        if fatal {
                            return SessionEnd::Fatal;
                        }
                    }
                    _ => {}
                }
            }
            cmd = cmds.recv() => {
                let frame = match cmd {
                    Some(Cmd::Move { client_seq, mv }) => {
                        let encoded = B64.encode(&mv);
                        pending.push_back((client_seq, mv));
                        // Before the first welcome nothing is sent: the
                        // welcome handler flushes the queue in order (and
                        // only it knows the base for the wire numbers).
                        let (true, Some(base)) = (welcomed, *seq_base) else { continue };
                        json!({ "t": "move", "client_seq": client_seq.saturating_add(base), "move": encoded })
                    }
                    Some(Cmd::Start) => json!({ "t": "start" }),
                    Some(Cmd::Sync) => json!({ "t": "sync" }),
                    Some(Cmd::Close) | None => {
                        let _ = sink.send(Message::Close(None)).await;
                        return SessionEnd::Closed;
                    }
                };
                if sink.send(Message::Text(frame.to_string())).await.is_err() {
                    return SessionEnd::Dropped { was_connected: welcomed };
                }
            }
        }
    }
}

fn str_of(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or_default().to_string()
}

fn bytes_of(v: Option<&Value>) -> Option<Vec<u8>> {
    v.and_then(Value::as_str).and_then(|s| B64.decode(s).ok())
}

fn seats_u8(v: Option<&Value>) -> Vec<u8> {
    v.and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_u64).map(|n| n as u8).collect())
        .unwrap_or_default()
}

fn parse_clock(v: Option<&Value>) -> Option<BTreeMap<u8, u64>> {
    let obj = v?.as_object()?;
    Some(obj.iter().filter_map(|(k, ms)| Some((k.parse::<u8>().ok()?, ms.as_u64()?))).collect())
}

fn parse_seats(v: Option<&Value>) -> Vec<SeatStatus> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|s| SeatStatus {
                    seat: s.get("seat").and_then(Value::as_u64).unwrap_or(0) as u8,
                    username: str_of(s, "username"),
                    connected: s.get("connected").and_then(Value::as_bool).unwrap_or(false),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_welcome(v: &Value) -> Welcome {
    Welcome {
        status: str_of(v, "status"),
        seat: v.get("seat").and_then(Value::as_u64).unwrap_or(0) as u8,
        seq: v.get("seq").and_then(Value::as_u64).unwrap_or(0),
        view: bytes_of(v.get("view")),
        to_move: seats_u8(v.get("to_move")),
        clock: parse_clock(v.get("clock")),
        seats: parse_seats(v.get("seats")),
        outcome: v.get("outcome").and_then(|o| serde_json::from_value::<Outcome>(o.clone()).ok()),
    }
}

fn parse_update(v: &Value) -> Update {
    Update {
        seq: v.get("seq").and_then(Value::as_u64).unwrap_or(0),
        by: v.get("by").and_then(Value::as_u64).unwrap_or(0) as u8,
        kind: str_of(v, "kind"),
        events: v
            .get("events")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|e| bytes_of(Some(e))).collect())
            .unwrap_or_default(),
        view: bytes_of(v.get("view")),
        to_move: seats_u8(v.get("to_move")),
        clock: parse_clock(v.get("clock")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_urls() {
        let e = MatchEndpoint::new("https://host.example/", "tictactoe", "1.0.0");
        assert_eq!(e.http("/matches"), "https://host.example/v1/tictactoe/matches");
        assert_eq!(e.ws("abc"), "wss://host.example/v1/tictactoe/matches/abc/ws");
        let d = MatchEndpoint::new("http://127.0.0.1:8787", "tictactoe", "1.0.0");
        assert_eq!(d.ws("abc"), "ws://127.0.0.1:8787/v1/tictactoe/matches/abc/ws");
        assert_eq!(e.queue_ws("ranked"), "wss://host.example/v1/tictactoe/queue?mode=ranked&rules_version=1.0.0");
        assert_eq!(e.open_url(), "https://host.example/v1/tictactoe/open?rules_version=1.0.0");
        // A `+` in a version must not turn into a space.
        let b = MatchEndpoint::new("http://h", "g", "1.0.0+build.7");
        assert_eq!(b.open_url(), "http://h/v1/g/open?rules_version=1.0.0%2Bbuild.7");
        assert_eq!(encode_query("a b/c?d&e=f"), "a%20b%2Fc%3Fd%26e%3Df");
    }

    #[test]
    fn parses_queue_and_listing_frames() {
        assert_eq!(
            parse_rating(Some(&json!({ "rating": 1612.5, "deviation": 80.25, "matches": 12 }))),
            Some(Rating { rating: 1612.5, deviation: 80.25, matches: 12 })
        );
        assert_eq!(parse_rating(Some(&Value::Null)), None);
        assert_eq!(parse_rating(Some(&json!({ "rating": "high" }))), None);
        let m = parse_open_match(&json!({ "match_id": "m1", "host": "mandy", "created_at_ms": 5, "rules_version": "1.0.0", "config": B64.encode([3u8, 5]) }));
        assert_eq!(m, Some(OpenMatch { match_id: "m1".into(), host: "mandy".into(), created_at_ms: 5, rules_version: "1.0.0".into(), config: vec![3, 5] }));
        assert_eq!(parse_open_match(&json!({ "host": "no id" })), None);
    }

    #[test]
    fn parses_host_frames() {
        let w = parse_welcome(&json!({
            "t": "welcome", "status": "active", "seat": 1, "seq": 3, "view": B64.encode([1u8, 2, 3]),
            "to_move": [0], "clock": { "0": 5000, "1": 60000 },
            "seats": [{ "seat": 0, "username": "a", "connected": true }, { "seat": 1, "username": "b", "connected": false }],
            "outcome": null, "last_client_seq": 2,
        }));
        assert_eq!(w.seat, 1);
        assert_eq!(w.view, Some(vec![1, 2, 3]));
        assert_eq!(w.clock.unwrap().get(&0), Some(&5000));
        assert_eq!(w.seats[1], SeatStatus { seat: 1, username: "b".into(), connected: false });

        let u = parse_update(&json!({ "t": "update", "seq": 4, "by": 0, "kind": "move", "events": [B64.encode([9u8])], "view": null, "to_move": [], "clock": null }));
        assert_eq!((u.seq, u.by, u.events, u.view, u.clock), (4, 0, vec![vec![9u8]], None, None));

        // Junk never panics; it parses to defaults.
        let junk = parse_welcome(&json!({ "seats": "nope", "clock": 5, "view": 12 }));
        assert_eq!(junk.status, "");
        assert!(junk.seats.is_empty() && junk.clock.is_none() && junk.view.is_none());
    }
}
