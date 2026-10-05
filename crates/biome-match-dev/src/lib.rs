//! A local match host for development and tests.
//!
//! Speaks the protocol of the production host (`cloudflare/biome-match`, see
//! `docs/MATCH_HOST_SPEC.md` §4): the same routes, the same JSON frames, the
//! same seat tokens, clocks, idempotent `client_seq`, and views / events
//! scoped per seat. The rules run as the same compiled WebAssembly, through
//! `biome-rules-host`, so a game behaves here exactly as it will in
//! production.
//!
//! What it deliberately does not do:
//!
//! - **Verify tickets.** `Authorization: Bearer dev:<user id>:<username>` is
//!   taken at its word. Never expose this server to a network you don't own.
//! - **Persist.** Matches live in memory and vanish with the process.
//! - **Record results.** There is no database behind it.
//!
//! Layout: [`Host`] holds every match behind one mutex (plenty for a dev
//! tool) and is pure logic: each call returns the frames to deliver.
//! [`router`] is the thin axum layer that moves those frames over HTTP and
//! WebSockets; [`serve`] binds and runs it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use biome_rules::wire::ApplyOk;
use biome_rules::{Outcome, Seat, SeatInfo, Turn};
use biome_rules_host::RulesModule;
use serde_json::{json, Value};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

mod http;
pub use http::{router, serve};

pub const WAITING_TTL_MS: i64 = 30 * 60 * 1000;
pub const MAX_MOVE_BYTES: usize = 64 * 1024;
pub const MAX_CONFIG_BYTES: usize = 64 * 1024;
const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
const CODE_LENGTH: usize = 6;

const CLOSE_REPLACED: u16 = 4000;
const CLOSE_PROTOCOL: u16 = 4400;
const CLOSE_BAD_TOKEN: u16 = 4401;
const CLOSE_VERSION: u16 = 4409;
const CLOSE_ENDED: u16 = 4410;

/// Something to deliver to one socket.
#[derive(Clone, Debug, PartialEq)]
pub enum Out {
    Frame(String),
    Close(u16, String),
}

/// A caller identity. In production this comes from a verified ticket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct User {
    pub user_id: String,
    pub username: String,
}

/// An HTTP-ish answer: status code and JSON body.
pub type Reply = (u16, Value);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    Waiting,
    Active,
    Over,
    Abandoned,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Waiting => "waiting",
            Status::Active => "active",
            Status::Over => "over",
            Status::Abandoned => "abandoned",
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Clock {
    initial_ms: i64,
    increment_ms: i64,
}

struct SeatRow {
    seat: u8,
    user_id: String,
    username: String,
    token: String,
    remaining_ms: Option<i64>,
    last_client_seq: u64,
}

struct Conn {
    id: u64,
    seat: Option<u8>,
    tx: UnboundedSender<Out>,
    open: bool,
}

/// One applied move, as the production host logs it.
#[derive(Clone, Debug)]
pub struct LogEntry {
    pub seat: u8,
    pub kind: &'static str,
    pub mv: Vec<u8>,
    pub seed: [u8; 32],
}

struct Match {
    match_id: String,
    app_id: String,
    rules_version: String,
    rules: Arc<RulesModule>,
    config: Vec<u8>,
    status: Status,
    created_at: i64,
    clock: Option<Clock>,
    seats: Vec<SeatRow>,
    state: Option<Vec<u8>>,
    seq: u64,
    to_move: Vec<u8>,
    since: HashMap<u8, i64>,
    outcome: Option<Outcome>,
    conns: Vec<Conn>,
    /// A clock expiry the rules could not apply: how often, and when to retry.
    clock_failures: u32,
    clock_retry_at: i64,
    log: Vec<LogEntry>,
}

struct Inner {
    matches: HashMap<String, Match>,
    /// (app, code) → match key
    codes: HashMap<(String, String), String>,
    next_conn: u64,
}

/// The dev host: registered rules modules and every live match.
pub struct Host {
    modules: HashMap<String, BTreeMap<String, Arc<RulesModule>>>,
    /// Apps where one user may hold several seats (see [`Host::allow_self_play`]).
    self_play: HashSet<String>,
    inner: Mutex<Inner>,
    /// Milliseconds since the epoch; replaceable so tests can drive clocks.
    now: Box<dyn Fn() -> i64 + Send + Sync>,
}

fn system_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// 32 bytes from the OS generator (two v4 UUIDs' worth, with the version
/// bits overwritten by a third so no position is constant).
fn fresh_seed() -> [u8; 32] {
    let a = *uuid::Uuid::new_v4().as_bytes();
    let b = *uuid::Uuid::new_v4().as_bytes();
    let c = *uuid::Uuid::new_v4().as_bytes();
    let mut out = [0u8; 32];
    out[..16].copy_from_slice(&a);
    out[16..].copy_from_slice(&b);
    // Bytes 6 and 8 of a v4 UUID carry fixed version / variant bits.
    out[6] ^= c[0];
    out[8] ^= c[1];
    out[22] ^= c[2];
    out[24] ^= c[3];
    out
}

fn random_token() -> String {
    format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple())
}

fn random_code() -> String {
    let mut out = String::with_capacity(CODE_LENGTH);
    while out.len() < CODE_LENGTH {
        for b in uuid::Uuid::new_v4().as_bytes() {
            let limit = 256 - (256 % CODE_ALPHABET.len());
            if (*b as usize) < limit && out.len() < CODE_LENGTH {
                out.push(CODE_ALPHABET[*b as usize % CODE_ALPHABET.len()] as char);
            }
        }
    }
    out
}

/// Uppercase and strip the separators people add when reading a code aloud.
pub fn normalize_code(raw: &str) -> Option<String> {
    let code: String = raw.chars().filter(|c| !c.is_whitespace() && *c != '-').map(|c| c.to_ascii_uppercase()).collect();
    (code.len() == CODE_LENGTH && code.bytes().all(|b| CODE_ALPHABET.contains(&b))).then_some(code)
}

fn key(app: &str, match_id: &str) -> String {
    format!("{app}:{}", match_id.to_ascii_lowercase())
}

fn err(status: u16, code: &str) -> Reply {
    (status, json!({ "error": code }))
}

fn err_msg(status: u16, code: &str, message: impl Into<String>) -> Reply {
    (status, json!({ "error": code, "message": message.into() }))
}

impl Host {
    pub fn new() -> Self {
        Self {
            modules: HashMap::new(),
            self_play: HashSet::new(),
            inner: Mutex::new(Inner { matches: HashMap::new(), codes: HashMap::new(), next_conn: 1 }),
            now: Box::new(system_now),
        }
    }

    /// Replace the clock (tests).
    pub fn with_clock(mut self, now: impl Fn() -> i64 + Send + Sync + 'static) -> Self {
        self.now = Box::new(now);
        self
    }

    /// Register a validated rules module under an app id. The version comes
    /// from the module's own meta.
    pub fn add_rules(mut self, app_id: &str, module: RulesModule) -> Self {
        let version = module.meta().rules_version.clone();
        self.modules.entry(app_id.to_string()).or_default().insert(version, Arc::new(module));
        self
    }

    /// Let one user take several seats of a waiting match of `app_id`, as
    /// the production host does for the SDK demo games. Without it, a seated
    /// user who joins again gets a rejoin of the seat it already holds.
    pub fn allow_self_play(mut self, app_id: &str) -> Self {
        self.self_play.insert(app_id.to_string());
        self
    }

    /// `{app: [versions]}`, as `GET /v1/rules` reports it.
    pub fn describe(&self) -> Value {
        let map: BTreeMap<&String, Vec<&String>> = self.modules.iter().map(|(a, v)| (a, v.keys().collect())).collect();
        json!({ "rules": map })
    }

    pub fn has_app(&self, app: &str) -> bool {
        self.modules.contains_key(app)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A panic while holding the lock poisons it; the data is still
        // usable for a dev tool, so keep going.
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    // ── HTTP-level operations ───────────────────────────────────────────────

    /// `POST /v1/:app/matches`
    pub fn create(&self, app: &str, user: &User, body: &Value) -> Reply {
        let Some(versions) = self.modules.get(app) else {
            return err_msg(404, "unknown-app", format!("no rules loaded for {app}"));
        };
        let version = match body.get("rules_version").and_then(Value::as_str).filter(|v| !v.is_empty()) {
            Some(v) => v.to_string(),
            // BTreeMap order is lexical; fine for the handful of versions a
            // dev server holds.
            None => match versions.keys().next_back() {
                Some(v) => v.clone(),
                None => return err(404, "unknown-rules"),
            },
        };
        let Some(rules) = versions.get(&version).cloned() else {
            return (404, json!({ "error": "unknown-rules", "message": format!("{app} {version} is not loaded"), "available": versions.keys().collect::<Vec<_>>() }));
        };

        let config = match body.get("config").and_then(Value::as_str) {
            None => Vec::new(),
            Some(s) => match B64.decode(s) {
                Ok(bytes) if bytes.len() <= MAX_CONFIG_BYTES => bytes,
                _ => return err_msg(400, "bad-config", "config is not base64 or too large"),
            },
        };
        let clock = match body.get("clock") {
            None | Some(Value::Null) => None,
            Some(c) => {
                let initial = c.get("initial_ms").and_then(Value::as_i64);
                let increment = c.get("increment_ms").and_then(Value::as_i64);
                match (initial, increment) {
                    (Some(i), Some(inc)) if (1000..=7 * 24 * 3_600_000).contains(&i) && (0..=24 * 3_600_000).contains(&inc) => {
                        Some(Clock { initial_ms: i, increment_ms: inc })
                    }
                    _ => return err(400, "bad-clock"),
                }
            }
        };

        // Dry run: a bad config fails now, not when the second player joins.
        let meta = rules.meta().clone();
        let probes: Vec<SeatInfo> = (0..meta.min_seats)
            .map(|i| SeatInfo { seat: Seat(i), user_id: format!("probe-{i}"), username: format!("probe{i}") })
            .collect();
        match rules.init(&config, &probes, &[0u8; 32]) {
            Ok(Ok(_)) => {}
            Ok(Err(r)) => return err_msg(400, &r.code, r.message),
            Err(e) => return err_msg(500, "rules-error", e.to_string()),
        }

        let now = (self.now)();
        let match_id = uuid::Uuid::new_v4().to_string();
        let token = random_token();
        let mut m = Match {
            match_id: match_id.clone(),
            app_id: app.to_string(),
            rules_version: version.clone(),
            rules,
            config,
            status: Status::Waiting,
            created_at: now,
            clock,
            seats: vec![SeatRow {
                seat: 0,
                user_id: user.user_id.clone(),
                username: user.username.clone(),
                token: token.clone(),
                remaining_ms: clock.map(|c| c.initial_ms),
                last_client_seq: 0,
            }],
            state: None,
            seq: 0,
            to_move: Vec::new(),
            since: HashMap::new(),
            outcome: None,
            conns: Vec::new(),
            clock_failures: 0,
            clock_retry_at: 0,
            log: Vec::new(),
        };
        if m.seats.len() >= meta.max_seats as usize {
            if let Err(reply) = m.start(now) {
                return reply;
            }
        }

        let mut inner = self.lock();
        let code = loop {
            let c = random_code();
            if !inner.codes.contains_key(&(app.to_string(), c.clone())) {
                break c;
            }
        };
        let status = m.status;
        inner.codes.insert((app.to_string(), code.clone()), key(app, &match_id));
        inner.matches.insert(key(app, &match_id), m);
        (200, json!({
            "match_id": match_id, "app_id": app, "rules_version": version,
            "seat": 0, "seat_token": token, "status": status.as_str(), "join_code": code,
        }))
    }

    /// `POST /v1/:app/matches/join`
    pub fn join_by_code(&self, app: &str, user: &User, raw_code: &str) -> Reply {
        let Some(code) = normalize_code(raw_code) else { return err(400, "bad-code") };
        let now = (self.now)();
        let mut inner = self.lock();
        let Some(k) = inner.codes.get(&(app.to_string(), code.clone())).cloned() else {
            return err(404, "unknown-code");
        };
        let Some(m) = inner.matches.get_mut(&k) else { return err(404, "unknown-code") };
        let (status, mut body) = m.join(user, false, self.self_play.contains(app), now);
        if status == 200 {
            body["join_code"] = json!(code);
        }
        (status, body)
    }

    /// `POST /v1/:app/matches/:id/rejoin`
    pub fn rejoin(&self, app: &str, match_id: &str, user: &User) -> Reply {
        let now = (self.now)();
        let mut inner = self.lock();
        match inner.matches.get_mut(&key(app, match_id)) {
            Some(m) => m.join(user, true, false, now),
            None => err(404, "no-such-match"),
        }
    }

    /// `GET /v1/:app/matches/:id`
    pub fn summary(&self, app: &str, match_id: &str, user: &User) -> Reply {
        let inner = self.lock();
        let Some(m) = inner.matches.get(&key(app, match_id)) else { return err(404, "no-such-match") };
        if !m.seats.iter().any(|s| s.user_id == user.user_id) {
            return err(403, "not-seated");
        }
        (200, json!({
            "match_id": m.match_id, "app_id": m.app_id, "rules_version": m.rules_version,
            "status": m.status.as_str(), "seq": m.seq,
            "to_move": if m.status == Status::Active { m.to_move.clone() } else { Vec::new() },
            "seats": m.public_seats(), "outcome": m.outcome,
        }))
    }

    // ── sockets ─────────────────────────────────────────────────────────────

    /// A socket connected to a match. Returns its id and the stream of
    /// things to send to it, or `None` if there is no such match.
    pub fn connect(&self, app: &str, match_id: &str) -> Option<(u64, UnboundedReceiver<Out>)> {
        let mut inner = self.lock();
        let id = inner.next_conn;
        let m = inner.matches.get_mut(&key(app, match_id))?;
        let (tx, rx) = unbounded_channel();
        m.conns.push(Conn { id, seat: None, tx, open: true });
        inner.next_conn += 1;
        Some((id, rx))
    }

    /// A text frame arrived on a socket.
    pub fn on_frame(&self, app: &str, match_id: &str, conn_id: u64, text: &str) {
        let now = (self.now)();
        let mut inner = self.lock();
        if let Some(m) = inner.matches.get_mut(&key(app, match_id)) {
            m.on_frame(conn_id, text, now);
        }
    }

    /// A socket went away.
    pub fn on_close(&self, app: &str, match_id: &str, conn_id: u64) {
        let mut inner = self.lock();
        if let Some(m) = inner.matches.get_mut(&key(app, match_id)) {
            let had_seat = m.conns.iter().find(|c| c.id == conn_id).and_then(|c| c.seat).is_some();
            m.conns.retain(|c| c.id != conn_id);
            if had_seat {
                m.broadcast_seats();
            }
        }
    }

    /// Apply clock expiries and abandon stale lobbies. The HTTP layer calls
    /// this a few times a second; tests call it directly.
    pub fn tick(&self) {
        let now = (self.now)();
        let mut inner = self.lock();
        for m in inner.matches.values_mut() {
            m.tick(now);
        }
    }

    /// Drop every socket of a match as if the network failed (tests of
    /// client reconnection). The match itself is untouched.
    pub fn kick_all(&self, app: &str, match_id: &str) {
        let mut inner = self.lock();
        if let Some(m) = inner.matches.get_mut(&key(app, match_id)) {
            for c in m.conns.iter_mut() {
                if c.open {
                    let _ = c.tx.send(Out::Close(1012, "service restart".to_string()));
                    c.open = false;
                }
            }
            m.conns.clear();
        }
    }

    /// The move log of a match (tests, debugging).
    pub fn log(&self, app: &str, match_id: &str) -> Option<Vec<LogEntry>> {
        self.lock().matches.get(&key(app, match_id)).map(|m| m.log.clone())
    }
}

impl Default for Host {
    fn default() -> Self {
        Self::new()
    }
}

impl Match {
    fn public_seats(&self) -> Value {
        Value::Array(
            self.seats
                .iter()
                .map(|s| json!({ "seat": s.seat, "username": s.username, "connected": self.conns.iter().any(|c| c.open && c.seat == Some(s.seat)) }))
                .collect(),
        )
    }

    fn send(&self, conn_id: u64, frame: Value) {
        if let Some(c) = self.conns.iter().find(|c| c.id == conn_id && c.open) {
            let _ = c.tx.send(Out::Frame(frame.to_string()));
        }
    }

    /// Close a connection a newer one took the seat from; it loses the seat
    /// at once so nothing it still sends is applied for that seat.
    fn retire(&mut self, conn_id: u64, reason: &str) {
        if let Some(c) = self.conns.iter_mut().find(|c| c.id == conn_id) {
            c.seat = None;
        }
        self.close(conn_id, CLOSE_REPLACED, reason);
    }

    fn close(&mut self, conn_id: u64, code: u16, reason: &str) {
        if let Some(c) = self.conns.iter_mut().find(|c| c.id == conn_id) {
            if c.open {
                let _ = c.tx.send(Out::Close(code, reason.to_string()));
                c.open = false;
            }
        }
    }

    fn broadcast_seats(&self) {
        let frame = json!({ "t": "seats", "status": self.status.as_str(), "seats": self.public_seats() });
        for c in self.conns.iter().filter(|c| c.open && c.seat.is_some()) {
            let _ = c.tx.send(Out::Frame(frame.to_string()));
        }
    }

    fn clock_now(&self, now: i64) -> Value {
        if self.clock.is_none() {
            return Value::Null;
        }
        let mut map = serde_json::Map::new();
        for s in &self.seats {
            let mut remaining = s.remaining_ms.unwrap_or(0);
            if self.status == Status::Active && self.to_move.contains(&s.seat) {
                if let Some(since) = self.since.get(&s.seat) {
                    remaining -= now - since;
                }
            }
            map.insert(s.seat.to_string(), json!(remaining.max(0)));
        }
        Value::Object(map)
    }

    fn view_b64(&self, seat: u8) -> Value {
        match &self.state {
            Some(state) => match self.rules.view(state, Seat(seat)) {
                Ok(Ok(bytes)) => json!(B64.encode(bytes)),
                _ => Value::Null,
            },
            None => Value::Null,
        }
    }

    fn send_welcome(&self, conn_id: u64, seat: u8, now: i64) {
        self.send(conn_id, json!({
            "t": "welcome",
            "match_id": self.match_id, "app_id": self.app_id, "rules_version": self.rules_version,
            "status": self.status.as_str(), "seat": seat, "seq": self.seq,
            "view": self.view_b64(seat),
            "to_move": if self.status == Status::Active { self.to_move.clone() } else { Vec::new() },
            "clock": self.clock_now(now),
            "seats": self.public_seats(),
            "outcome": self.outcome,
            "last_client_seq": self.seats.iter().find(|s| s.seat == seat).map(|s| s.last_client_seq).unwrap_or(0),
        }));
    }

    /// Run the rules' init and go active.
    fn start(&mut self, now: i64) -> Result<(), Reply> {
        let seats: Vec<SeatInfo> = self
            .seats
            .iter()
            .map(|s| SeatInfo { seat: Seat(s.seat), user_id: s.user_id.clone(), username: s.username.clone() })
            .collect();
        let state = match self.rules.init(&self.config, &seats, &fresh_seed()) {
            Ok(Ok(s)) => s,
            Ok(Err(r)) => return Err(err_msg(400, &r.code, r.message)),
            Err(e) => return Err(err_msg(500, "rules-error", e.to_string())),
        };
        let to_move = match self.rules.to_move(&state) {
            Ok(Ok(t)) => turn_seats(&t),
            _ => return Err(err(500, "rules-error")),
        };
        self.status = Status::Active;
        self.since = to_move.iter().map(|s| (*s, now)).collect();
        self.to_move = to_move;
        self.state = Some(state);
        let seated: Vec<(u64, u8)> = self.conns.iter().filter(|c| c.open).filter_map(|c| c.seat.map(|s| (c.id, s))).collect();
        for (id, seat) in seated {
            self.send_welcome(id, seat, now);
        }
        Ok(())
    }

    fn join(&mut self, user: &User, rejoin_only: bool, allow_self_play: bool, now: i64) -> Reply {
        let token = random_token();
        let max = self.rules.meta().max_seats as usize;
        // Same rule as the production host: a seated user joining again is a
        // rejoin, unless the app allows self-play and a seat is still free.
        let self_play = allow_self_play && !rejoin_only && self.status == Status::Waiting && self.seats.len() < max;
        let mine = if self_play { None } else { self.seats.iter().position(|s| s.user_id == user.user_id) };
        if let Some(i) = mine {
            // A rejoin: new token, old sockets for that seat are dropped.
            self.seats[i].token = token.clone();
            self.seats[i].username = user.username.clone();
            let seat = self.seats[i].seat;
            let old: Vec<u64> = self.conns.iter().filter(|c| c.seat == Some(seat)).map(|c| c.id).collect();
            for id in old {
                self.retire(id, "rejoined elsewhere");
            }
            return (200, self.seat_reply(seat, &token));
        }
        if rejoin_only {
            return err(403, "not-seated");
        }
        if self.status != Status::Waiting {
            return err(409, "match-started");
        }
        if self.seats.len() >= max {
            return err(409, "match-full");
        }
        let seat = self.seats.len() as u8;
        self.seats.push(SeatRow {
            seat,
            user_id: user.user_id.clone(),
            username: user.username.clone(),
            token: token.clone(),
            remaining_ms: self.clock.map(|c| c.initial_ms),
            last_client_seq: 0,
        });
        self.broadcast_seats();
        if self.seats.len() >= max {
            if let Err(reply) = self.start(now) {
                return reply;
            }
        }
        (200, self.seat_reply(seat, &token))
    }

    fn seat_reply(&self, seat: u8, token: &str) -> Value {
        json!({
            "match_id": self.match_id, "app_id": self.app_id, "rules_version": self.rules_version,
            "seat": seat, "seat_token": token, "status": self.status.as_str(),
        })
    }

    fn on_frame(&mut self, conn_id: u64, text: &str, now: i64) {
        // As the production host: a socket already closed (replaced, ended)
        // may still deliver frames it sent before the close. They don't count.
        if !self.conns.iter().any(|c| c.id == conn_id && c.open) {
            return;
        }
        let Ok(msg) = serde_json::from_str::<Value>(text) else {
            self.send(conn_id, json!({ "t": "error", "code": "bad-json" }));
            return;
        };
        let Some(t) = msg.get("t").and_then(Value::as_str) else {
            self.send(conn_id, json!({ "t": "error", "code": "bad-frame" }));
            return;
        };
        let seat = self.conns.iter().find(|c| c.id == conn_id).and_then(|c| c.seat);
        let Some(seat) = seat else {
            if t == "hello" {
                self.on_hello(conn_id, &msg, now);
            } else {
                self.close(conn_id, CLOSE_PROTOCOL, "hello first");
            }
            return;
        };
        match t {
            "move" => self.on_move(conn_id, seat, &msg, now),
            "start" => self.on_start(conn_id, seat, now),
            "sync" | "hello" => self.send_welcome(conn_id, seat, now),
            "ping" => self.send(conn_id, json!({ "t": "pong" })),
            other => self.send(conn_id, json!({ "t": "error", "code": "unknown-frame", "frame": other })),
        }
    }

    fn on_hello(&mut self, conn_id: u64, msg: &Value, now: i64) {
        let token = msg.get("seat_token").and_then(Value::as_str).unwrap_or("");
        let seat = self.seats.iter().find(|s| !token.is_empty() && s.token == token).map(|s| s.seat);
        let Some(seat) = seat else {
            self.send(conn_id, json!({ "t": "error", "code": "bad-seat-token" }));
            self.close(conn_id, CLOSE_BAD_TOKEN, "bad seat token");
            return;
        };
        if msg.get("rules_version").and_then(Value::as_str) != Some(self.rules_version.as_str()) {
            self.send(conn_id, json!({ "t": "error", "code": "rules-version-mismatch", "server": self.rules_version }));
            self.close(conn_id, CLOSE_VERSION, "rules version mismatch");
            return;
        }
        let older: Vec<u64> = self.conns.iter().filter(|c| c.id != conn_id && c.seat == Some(seat) && c.open).map(|c| c.id).collect();
        for id in older {
            self.send(id, json!({ "t": "error", "code": "replaced" }));
            self.retire(id, "replaced by a newer connection");
        }
        if let Some(c) = self.conns.iter_mut().find(|c| c.id == conn_id) {
            c.seat = Some(seat);
        }
        self.send_welcome(conn_id, seat, now);
        self.broadcast_seats();
    }

    fn on_start(&mut self, conn_id: u64, seat: u8, now: i64) {
        if self.status != Status::Waiting {
            self.send(conn_id, json!({ "t": "error", "code": "not-waiting" }));
            return;
        }
        if seat != 0 {
            self.send(conn_id, json!({ "t": "error", "code": "not-the-host" }));
            return;
        }
        let min = self.rules.meta().min_seats;
        if self.seats.len() < min as usize {
            self.send(conn_id, json!({ "t": "error", "code": "not-enough-seats", "need": min }));
            return;
        }
        if let Err((_, body)) = self.start(now) {
            self.send(conn_id, json!({ "t": "error", "code": "start-failed", "detail": body.to_string() }));
        }
    }

    fn on_move(&mut self, conn_id: u64, seat: u8, msg: &Value, now: i64) {
        let client_seq = msg.get("client_seq").and_then(Value::as_u64);
        let reject = |m: &Match, code: &str, message: &str| {
            m.send(conn_id, json!({ "t": "rejected", "client_seq": client_seq, "code": code, "message": message }));
        };
        let Some(client_seq) = client_seq.filter(|n| *n >= 1) else {
            return reject(self, "bad-move", "client_seq must be a positive integer");
        };
        match self.status {
            Status::Active => {}
            Status::Waiting => return reject(self, "not-started", "match is waiting"),
            _ => return reject(self, "game-over", "match is over"),
        }
        let Some(row) = self.seats.iter().position(|s| s.seat == seat) else {
            return reject(self, "not-seated", "");
        };
        if client_seq <= self.seats[row].last_client_seq {
            self.send_welcome(conn_id, seat, now);
            return;
        }
        let mv = match msg.get("move").and_then(Value::as_str).map(|s| B64.decode(s)) {
            Some(Ok(bytes)) if bytes.len() <= MAX_MOVE_BYTES => bytes,
            Some(Ok(_)) => return reject(self, "bad-move", "move too large"),
            _ => return reject(self, "bad-move", "move must be base64"),
        };
        let Some(state) = self.state.clone() else { return reject(self, "rules-error", "no state") };

        if self.clock.is_some() && self.to_move.contains(&seat) {
            let since = self.since.get(&seat).copied().unwrap_or(now);
            if self.seats[row].remaining_ms.unwrap_or(0) - (now - since) <= 0 {
                reject(self, "out-of-time", "your clock ran out");
                let _ = self.apply_timeout(seat, now);
                return;
            }
        }

        let seed = fresh_seed();
        match self.rules.apply(&state, Seat(seat), &mv, &seed) {
            Ok(Ok(applied)) => {
                let previous = self.seats[row].last_client_seq;
                self.seats[row].last_client_seq = client_seq;
                if !self.commit(applied, LogEntry { seat, kind: "move", mv, seed }, now) {
                    // Nothing changed; every move gets exactly one answer.
                    self.seats[row].last_client_seq = previous;
                    reject(self, "rules-error", "the rules produced a state the host could not use");
                }
            }
            Ok(Err(r)) => reject(self, &r.code, &r.message),
            Err(e) => reject(self, "rules-error", &e.to_string()),
        }
    }

    /// `false` when the rules could not apply the expiry.
    fn apply_timeout(&mut self, seat: u8, now: i64) -> bool {
        let Some(state) = self.state.clone() else { return false };
        let seed = fresh_seed();
        let Ok(Ok(applied)) = self.rules.timeout(&state, Seat(seat), &seed) else { return false };
        let row = self.seats.iter().position(|s| s.seat == seat);
        let previous = row.and_then(|i| self.seats[i].remaining_ms);
        if let Some(i) = row {
            self.seats[i].remaining_ms = Some(0);
        }
        let committed = self.commit(applied, LogEntry { seat, kind: "timeout", mv: Vec::new(), seed }, now);
        if let (false, Some(i)) = (committed, row) {
            self.seats[i].remaining_ms = previous;
        }
        committed
    }

    /// Store an applied move / timeout and tell everyone what they may know.
    /// `false`, with nothing changed or sent, when the rules cannot read the
    /// state they produced; the caller answers the mover.
    fn commit(&mut self, applied: ApplyOk, entry: LogEntry, now: i64) -> bool {
        let (to_move, outcome) = match (self.rules.to_move(&applied.state), self.rules.outcome(&applied.state)) {
            (Ok(Ok(t)), Ok(Ok(o))) => (turn_seats(&t), o),
            _ => return false,
        };

        // The mover pays for its time and earns the increment; a seat that
        // was on the clock and leaves the turn without moving pays too.
        if let Some(clock) = self.clock {
            for s in self.to_move.clone() {
                let Some(since) = self.since.get(&s).copied() else { continue };
                let Some(row) = self.seats.iter_mut().find(|r| r.seat == s) else { continue };
                let spent = (row.remaining_ms.unwrap_or(0) - (now - since)).max(0);
                if s == entry.seat {
                    if entry.kind == "move" {
                        row.remaining_ms = Some(spent + clock.increment_ms);
                    }
                } else if !to_move.contains(&s) {
                    row.remaining_ms = Some(spent);
                }
            }
        }
        self.clock_failures = 0;
        self.clock_retry_at = 0;
        let since: HashMap<u8, i64> = to_move
            .iter()
            .map(|s| {
                let keep = self.to_move.contains(s) && *s != entry.seat;
                (*s, if keep { self.since.get(s).copied().unwrap_or(now) } else { now })
            })
            .collect();

        self.seq += 1;
        self.state = Some(applied.state);
        self.to_move = to_move;
        self.since = since;
        let (by, kind) = (entry.seat, entry.kind);
        self.log.push(entry);
        if let Some(o) = &outcome {
            self.status = Status::Over;
            self.outcome = Some(o.clone());
        }

        let clock = self.clock_now(now);
        let targets: Vec<(u64, u8)> = self.conns.iter().filter(|c| c.open).filter_map(|c| c.seat.map(|s| (c.id, s))).collect();
        for (id, seat) in targets {
            let events: Vec<String> = applied
                .events
                .iter()
                .filter(|(aud, _)| aud.includes(Seat(seat)))
                .map(|(_, bytes)| B64.encode(bytes))
                .collect();
            self.send(id, json!({
                "t": "update", "seq": self.seq, "by": by, "kind": kind,
                "events": events, "view": self.view_b64(seat),
                "to_move": if outcome.is_some() { Vec::new() } else { self.to_move.clone() },
                "clock": clock,
            }));
            if let Some(o) = &outcome {
                self.send(id, json!({ "t": "over", "outcome": o }));
            }
        }
        true
    }

    fn tick(&mut self, now: i64) {
        if self.status == Status::Waiting && now >= self.created_at + WAITING_TTL_MS {
            self.status = Status::Abandoned;
            let ids: Vec<u64> = self.conns.iter().map(|c| c.id).collect();
            for id in ids {
                self.send(id, json!({ "t": "error", "code": "abandoned" }));
                self.close(id, CLOSE_ENDED, "nobody joined");
            }
        }
        if self.clock.is_none() || now < self.clock_retry_at {
            return;
        }
        for _ in 0..16 {
            if self.status != Status::Active {
                break;
            }
            let fallen = self.to_move.iter().copied().find(|s| {
                let row = self.seats.iter().find(|r| r.seat == *s);
                match (row, self.since.get(s)) {
                    (Some(r), Some(since)) => r.remaining_ms.unwrap_or(0) - (now - since) <= 0,
                    _ => false,
                }
            });
            let Some(seat) = fallen else { break };
            if self.apply_timeout(seat, now) {
                continue;
            }
            // As the production host: back off, then give the match up.
            self.clock_failures += 1;
            if self.clock_failures >= MAX_CLOCK_FAILURES {
                self.status = Status::Abandoned;
                let ids: Vec<u64> = self.conns.iter().map(|c| c.id).collect();
                for id in ids {
                    self.send(id, json!({ "t": "error", "code": "match-fault", "detail": "timeout-failed" }));
                    self.close(id, CLOSE_ENDED, "match fault");
                }
            } else {
                let backoff = CLOCK_RETRY_BASE_MS.saturating_mul(1 << (self.clock_failures - 1)).min(CLOCK_RETRY_MAX_MS);
                self.clock_retry_at = now + backoff;
            }
            break;
        }
    }
}

fn turn_seats(turn: &Turn) -> Vec<u8> {
    turn.seats().into_iter().map(|s| s.0).collect()
}

const CLOCK_RETRY_BASE_MS: i64 = 5_000;
const CLOCK_RETRY_MAX_MS: i64 = 5 * 60 * 1000;
const MAX_CLOCK_FAILURES: u32 = 8;

/// How often the HTTP layer calls [`Host::tick`].
pub const TICK: Duration = Duration::from_millis(200);
