//! Public types exposed to game code.
//!
//! Anything here is part of the BOS public API and is also a wire type
//! (de/serialized over IPC). Add fields conservatively; once shipped they
//! become a compatibility contract with both the game-side `BiomeClient`
//! and the desktop-side server (whether `biome-os-host` or `biome-app`).

use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::time::SystemTime;
use uuid::Uuid;

// ─────────────────────────────────────────────────────────────────────────────
// Config + transport
// ─────────────────────────────────────────────────────────────────────────────

/// Game-side configuration handed to [`crate::BiomeClient::connect`].
///
/// Build it with [`BosConfig::new`]; the struct literal form also works but
/// gains fields over time.
#[derive(Clone, Debug)]
pub struct BosConfig {
    /// Per-game identifier, used by the desktop side to scope entitlements,
    /// friends, etc. Equivalent of Steamworks' `app_id`. Must match the id
    /// the game is registered under (`game_apps.app_id`).
    pub app_id: String,
    /// How the client should reach the local biome-app process.
    pub transport: Transport,
    /// Human-readable name the game calls itself. Logged by the host and
    /// shown by dev hosts; the real consent prompt shows the *registered*
    /// name for `app_id`.
    pub app_name: String,
}

impl BosConfig {
    /// Production transport, given the registered app id and a display name.
    pub fn new(app_id: impl Into<String>, app_name: impl Into<String>) -> Self {
        Self {
            app_id: app_id.into(),
            app_name: app_name.into(),
            transport: Transport::Production,
        }
    }

    pub fn with_transport(mut self, transport: Transport) -> Self {
        self.transport = transport;
        self
    }
}

/// How a connection proved it may use the session (added 2026-10).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttachVia {
    /// Launched by Biome with a one-time `BIOME_SESSION_NONCE`.
    LaunchNonce,
    /// Started on its own; the user allowed the app (now or remembered).
    Consent,
}

// ─────────────────────────────────────────────────────────────────────────────
// Game tickets (added 2026-10)
// ─────────────────────────────────────────────────────────────────────────────

/// A short-lived, signed proof of "this user, for this app", minted by the
/// Biome backend and handed to the game by the desktop host. The game
/// presents it to a match host (a Cloudflare Worker, typically) which
/// verifies it offline against Biome's published public key.
///
/// Format: a compact JWS (JWT) signed with ES256. See [`crate::ticket`] for
/// the claims and a verifier. Treat the string as opaque in game code.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GameTicket {
    pub ticket: String,
    /// When the ticket stops being valid. Ask for a new one before this;
    /// they are cheap.
    pub expires_at: SystemTime,
    /// The app id the ticket was issued to (its `aud`).
    pub app_id: String,
}

impl GameTicket {
    pub fn is_expired(&self) -> bool {
        SystemTime::now() >= self.expires_at
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Licenses (added 2026-10-17, Biome 0.1.28)
// ─────────────────────────────────────────────────────────────────────────────

/// A signed license for the connected app, as the desktop host got it from
/// the Biome backend: "this user may run this app (this release) on this
/// device until `expires_at`". An Ed25519-signed compact JWS; the
/// `biome-sdk` crate's `license` module verifies it against the public keys
/// compiled into the app and caches it. See docs/GAME_SDK_SPEC.md §10.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LicenseGrant {
    pub token: String,
    pub expires_at: SystemTime,
    pub app_id: String,
}

/// Which IPC transport the client should use.
#[derive(Clone, Debug, Default)]
pub enum Transport {
    /// Connect to the running biome-app (or `biome-os-host`) via OS-default
    /// IPC: named pipe on Windows, unix socket on Linux/macOS.
    #[default]
    Production,

    /// Override the default socket / pipe path. Useful for local dev when
    /// you want multiple BOS hosts running at once.
    Custom { path: String },

    /// In-process deterministic stub. Returns canned ConnectInfo for tests.
    /// No IPC, no thread, no runtime — all calls resolve synchronously.
    Mock,
}

// ─────────────────────────────────────────────────────────────────────────────
// Session / identity
// ─────────────────────────────────────────────────────────────────────────────

/// What BOS knows about the signed-in player. Surfaced via
/// [`crate::events::BiomeEvent::SessionEstablished`] or
/// [`crate::BiomeClient::session`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Session {
    pub user_id: Uuid,
    pub username: String,
    /// App-scoped entitlements. Game checks this to gate features.
    pub entitlements: Vec<String>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Match connect bundle
// ─────────────────────────────────────────────────────────────────────────────

/// Everything the game needs to enter a match. Returned by
/// [`crate::BiomeClient::match_connect_info`].
///
/// **Lightyear-agnostic**: `netcode_token` is opaque bytes. Use the
/// `lightyear` feature for the `auth_from_token` adapter, or roll your own.
///
/// **Time-bounded**: orchestrator-issued bundles are short-lived. If
/// `SystemTime::now() > expires_at` the client MUST re-request rather than
/// blindly connect — the underlying Vultr container may have died between
/// provision and connect.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConnectInfo {
    // ── Game server (Vultr) ──────────────────────────────────────────────
    /// Public IP:port of the cafe_shooter_server container.
    pub game_server: SocketAddr,
    /// Region the server is in. Surface to the player ("Playing on US East")
    /// and feed into telemetry / matchmaking quality metrics.
    pub server_region: Region,
    /// Opaque netcode token. Hand verbatim to the game's netcode layer
    /// (e.g. via the `lightyear` feature adapter).
    pub netcode_token: Vec<u8>,
    /// Wire protocol version the server expects. Game refuses to connect if
    /// its protocol_id != this value.
    pub protocol_version: u32,
    /// Server tick rate in Hz. Game uses this to size its prediction buffer.
    pub tick_hz: u16,

    // ── Voice (CF Calls SFU) ─────────────────────────────────────────────
    pub voice: VoiceRooms,

    // ── Match coordination DO (biome-rt) ─────────────────────────────────
    /// WebSocket URL of the match DO. Used for vote-kick, surrender vote,
    /// post-round chat archive, mid-match disconnect tracking.
    pub match_do_url: String,
    /// Short-lived JWT scoped to this match. Presented at WS upgrade.
    pub match_do_token: String,

    // ── Match metadata (pre-load + validate) ─────────────────────────────
    pub match_id: MatchId,
    pub map_id: MapId,
    pub mode: GameMode,
    /// Hard cap on match length. Past this, the server forfeits the match.
    pub max_duration_secs: u32,
    pub player_count: u8,
    /// Which team the player is on. Used to know which team voice room to
    /// join by default.
    pub my_team: TeamId,

    // ── Freshness ────────────────────────────────────────────────────────
    pub issued_at: SystemTime,
    pub expires_at: SystemTime,
}

/// CF Calls SFU room handles for the three voice channels per match.
///
/// **SFU-level enforcement**: each room is a separate CF Calls session, so
/// "team B never receives team A's frames" is enforced at the network level
/// — not by client-side muting (which is bypassable).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VoiceRooms {
    /// Team-only voice. Always-on by default.
    pub team: RoomId,
    /// All-chat (typically dead players or post-round).
    pub all_chat: RoomId,
    /// Party voice, present if the player queued with a party of 2-5.
    pub party: Option<RoomId>,
    /// CF Calls access token. Same value works for all three rooms.
    pub access_token: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// IDs / enums
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MatchId(pub Uuid);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MapId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TeamId(pub u8);

/// CF Calls room identifier. Opaque to the game.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RoomId(pub String);

/// Server placement region. Mirrors the regions where game-server VMs live.
/// Add variants when you bring up new regions; never reorder.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Region {
    /// US East (Virginia / NYC)
    UsEast,
    /// US West (Los Angeles / Seattle)
    UsWest,
    /// EU (Frankfurt / Amsterdam / London)
    Eu,
    /// APAC (Tokyo / Singapore / Sydney)
    Apac,
    /// Unknown / unrouted. Should not happen in production; treat as warning.
    Unknown,
}

/// Game mode identifier. Cafe-shooter-specific values land here once defined.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum GameMode {
    /// 8v8 standard competitive.
    Competitive8v8,
    /// 8v8 unranked / casual.
    Casual8v8,
    /// Custom / private match.
    Custom,
}
