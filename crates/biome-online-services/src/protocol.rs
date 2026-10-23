//! IPC wire protocol — shared between [`crate::client`] and [`crate::server`].
//!
//! Versioning strategy (per cafe spec):
//!
//! - [`BosWire`] is the on-wire root. Add variants for new major versions;
//!   never reorder or remove. Old SDKs continue to send `V1`; old hosts
//!   continue to handle `V1`.
//! - Inside a version, add new request/response variants conservatively.
//!   Don't reorder existing ones. Everything added in 2026-10 (attach without
//!   a launch nonce, game tickets) is appended at the END of its enum so a
//!   host built before it still decodes every older frame.
//! - Length-delimited framing on the socket (see [`crate::transport`]).
//!   Body is bincode2.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::events::BiomeEvent;
use crate::types::{ConnectInfo, GameTicket, LicenseGrant, MatchId, Session};

/// Current SDK wire version. Server announces what minimum it requires;
/// client refuses to operate if `MIN_SUPPORTED_BY_SERVER > CURRENT`.
///
/// Still 1: the 2026-10 additions are appended variants, not a new wire
/// version. A v1 host that predates them answers an [`BosMsgV1::AttachHello`]
/// with a decode error / closed socket, which the client reports as
/// [`crate::BosError::HandshakeRejected`].
pub const CURRENT_SDK_VERSION: u32 = 1;

/// Wire root. Wrap every message in here; add variants for breaking changes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum BosWire {
    V1(BosMsgV1),
}

/// Version-1 protocol messages.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum BosMsgV1 {
    // ── Handshake (first frame on a new connection) ─────────────────────
    /// Sent by client immediately after socket connect.
    ClientHello(ClientHello),
    /// Sent by server in response. Includes session if auth succeeded.
    ServerHello(ServerHello),

    // ── Request / response (client-initiated) ────────────────────────────
    /// Client requests something; server replies with [`BosMsgV1::Response`]
    /// carrying the same `request_id`.
    Request {
        request_id: u64,
        body: RequestBody,
    },
    Response {
        request_id: u64,
        body: ResponseBody,
    },

    // ── Server push (server-initiated) ───────────────────────────────────
    /// Server pushes a platform event to the client (drains into game
    /// `BiomeEvent` stream).
    Push(BiomeEvent),

    // ── Keepalive ────────────────────────────────────────────────────────
    Ping,
    Pong,

    // ── Added 2026-10: attach without a launch nonce ─────────────────────
    /// Alternative first frame. A game that was started on its own (no
    /// `BIOME_SESSION_NONCE`) asks to attach to the signed-in user. The
    /// server answers with [`ServerHello::AwaitingConsent`] while it asks the
    /// user, then [`ServerHello::Ok`] or [`ServerHello::Rejected`].
    AttachHello(AttachHello),
}

// ─────────────────────────────────────────────────────────────────────────────
// Handshake
// ─────────────────────────────────────────────────────────────────────────────

/// First client → server frame. Identifies the game and presents the
/// per-launch nonce that biome-app injected as `BIOME_SESSION_NONCE` env var.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClientHello {
    pub sdk_version: u32,
    pub app_id: String,
    /// Per-launch nonce from `BIOME_SESSION_NONCE` env var. Server checks
    /// this against the nonce it issued when launching the game.
    ///
    /// In `--dev-mode`, the server accepts any value.
    pub session_nonce: String,
    pub game_pid: u32,
}

/// First client → server frame for the consent path (no launch nonce).
///
/// Any local process running as the same OS user can open the IPC endpoint,
/// so nothing in this frame is trusted on its own: the host looks `app_id`
/// up in its registry of known games, shows the user the *registered* name,
/// and remembers the answer per app. `app_name` is only a fallback label for
/// logs and for hosts running in dev mode.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AttachHello {
    pub sdk_version: u32,
    /// Registered app id (`^[a-z0-9][a-z0-9-]{2,63}$`), e.g. `mecha-aether`.
    pub app_id: String,
    /// Human-readable name the game calls itself. Not trusted for consent.
    pub app_name: String,
    pub game_pid: u32,
    /// Free-form build version of the game, recorded with the consent.
    pub client_version: String,
}

/// Server → client response to [`ClientHello`] / [`AttachHello`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ServerHello {
    /// Handshake accepted.
    Ok {
        /// Server's wire protocol version. Client refuses if `min_supported_sdk` > its own.
        server_version: u32,
        min_supported_sdk: u32,
        /// Current session (if logged in). May be `None` if BOS host is
        /// running in dev/test mode without an authenticated user.
        session: Option<Session>,
    },
    /// Handshake rejected. For the attach path `reason` is one of the stable
    /// codes in [`reject`]; for the nonce path it is a human-readable string.
    Rejected { reason: String },

    // ── Added 2026-10 ────────────────────────────────────────────────────
    /// The attach request was received and the user is being asked. The
    /// client should keep waiting (minutes, not seconds); `Ok` or `Rejected`
    /// follows on the same connection.
    AwaitingConsent { app_id: String },
}

/// Stable rejection codes for the attach path. The client maps them to
/// typed [`crate::BosError`] variants; anything else becomes
/// [`crate::BosError::HandshakeRejected`] with the raw string.
pub mod reject {
    /// Biome is running but nobody is signed in. The game should tell the
    /// player to sign in to Biome, then retry.
    pub const NOT_SIGNED_IN: &str = "not-signed-in";
    /// The user said no (now or remembered from an earlier prompt).
    pub const CONSENT_DENIED: &str = "consent-denied";
    /// `app_id` is not a registered game, or it is disabled.
    pub const UNKNOWN_APP: &str = "unknown-app";
    /// This host does not support attaching without a launch nonce.
    pub const ATTACH_UNSUPPORTED: &str = "attach-unsupported";
    /// The host could not reach its backend to look the app up or mint
    /// anything (network). Retry later.
    pub const BACKEND_UNAVAILABLE: &str = "backend-unavailable";
}

// ─────────────────────────────────────────────────────────────────────────────
// Request bodies
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum RequestBody {
    /// "Tell me what's needed to enter this match."
    MatchConnectInfo { match_id: MatchId },

    /// "Queue me into matchmaking." (Stub for now; full impl in week 3+.)
    EnqueueMatchmaking { mode_id: u32 },

    /// "Cancel the matchmaking ticket I have."
    CancelMatchmaking { ticket_id: Uuid },

    /// "What's my current session?" (Used at boot to confirm we're authed.)
    GetSession,

    /// "Send a telemetry sample." Fire-and-forget; server may reply with
    /// `Ack` to confirm receipt.
    Telemetry { kind: String, payload: Vec<u8> },

    /// "Report an anti-cheat anomaly." Same fire-and-forget pattern.
    AnomalyReport {
        suspect_user_id: Uuid,
        kind: String,
        evidence: Vec<u8>,
    },

    // ── Added 2026-10 ────────────────────────────────────────────────────
    /// "Give me a short-lived signed ticket for this app." The host mints
    /// it through the Biome backend with the user's session; the game hands
    /// it to a match host, which verifies it offline against the published
    /// public key. See [`crate::ticket`].
    GameTicket,

    // ── Added 2026-10-17 (Biome 0.1.28) ──────────────────────────────────
    /// "Give me a signed license for this app." The host asks the Biome
    /// backend with the user's session, for `version` (the build the app
    /// says it is; else the version its library installed) and this
    /// device; the answer is [`ResponseBody::License`], or
    /// [`ResponseBody::Error`] with a code from [`license_error`]
    /// (`needs-upgrade` when the account's edition doesn't cover `version`).
    /// A host older than this can't decode the frame and drops the
    /// connection.
    License { version: Option<String> },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ResponseBody {
    /// Successful response carrying the [`ConnectInfo`] bundle.
    ConnectInfo(ConnectInfo),
    /// Matchmaking ticket created. Server will push `MatchReady` when it's ready.
    MatchmakingQueued { ticket_id: Uuid },
    /// Matchmaking cancelled.
    MatchmakingCancelled,
    /// Current session snapshot.
    Session(Option<Session>),
    /// Generic acknowledgment for fire-and-forget calls.
    Ack,
    /// The request failed — string is a human-readable reason.
    Error(String),

    // ── Added 2026-10 ────────────────────────────────────────────────────
    /// A freshly minted game ticket.
    GameTicket(GameTicket),

    // ── Added 2026-10-17 (Biome 0.1.28) ──────────────────────────────────
    /// A license for the connected app ([`RequestBody::License`]).
    License(LicenseGrant),
}

/// Stable codes in [`ResponseBody::Error`] for a [`RequestBody::License`].
/// Anything else is a human-readable reason.
pub mod license_error {
    /// Nobody is signed in to Biome.
    pub const NOT_SIGNED_IN: &str = "not-signed-in";
    /// This Biome account may not have this app (no entitlement, not a
    /// tester of the track, the version was withdrawn, the app is off).
    pub const NOT_ENTITLED: &str = "not-entitled";
    /// Biome couldn't reach its backend, or the backend can't sign licenses
    /// yet. Try again later; a cached license is still good.
    pub const UNAVAILABLE: &str = "unavailable";
    /// This device couldn't be identified (no machine id).
    pub const NO_DEVICE: &str = "no-device";
    /// The account owns an edition of the app, not the one this version
    /// belongs to: an upgrade ("Trillavio 2026" owned, 2027.0.0 installed).
    /// Sent as this code, then each edition owned on its own line
    /// ([`needs_upgrade`], [`parse_needs_upgrade`]).
    pub const NEEDS_UPGRADE: &str = "needs-upgrade";

    /// The [`NEEDS_UPGRADE`] code with the editions owned. Names never hold
    /// a line break (the database refuses one); any that did is dropped.
    pub fn needs_upgrade(editions: &[String]) -> String {
        let mut code = NEEDS_UPGRADE.to_string();
        for e in editions.iter().filter(|e| !e.is_empty() && !e.contains(['\n', '\r'])) {
            code.push('\n');
            code.push_str(e);
        }
        code
    }

    /// The editions in a [`NEEDS_UPGRADE`] code, or `None` for another code.
    pub fn parse_needs_upgrade(code: &str) -> Option<Vec<String>> {
        let mut lines = code.split('\n');
        if lines.next()? != NEEDS_UPGRADE {
            return None;
        }
        Some(lines.filter(|l| !l.is_empty()).map(str::to_string).collect())
    }
}

#[cfg(test)]
mod tests {
    //! Wire-compat: a frame encoded by this build must decode to the same
    //! frame, and the OLD variants must keep their discriminants (a host that
    //! predates 2026-10 decodes by position).
    use super::*;
    use bincode::config::standard;

    fn roundtrip(w: &BosWire) -> BosWire {
        let bytes = bincode::serde::encode_to_vec(w, standard()).unwrap();
        bincode::serde::decode_from_slice(&bytes, standard()).unwrap().0
    }

    #[test]
    fn attach_hello_roundtrips() {
        let w = BosWire::V1(BosMsgV1::AttachHello(AttachHello {
            sdk_version: 1,
            app_id: "mecha-aether".into(),
            app_name: "Mecha Aether".into(),
            game_pid: 42,
            client_version: "0.1.0".into(),
        }));
        assert!(matches!(roundtrip(&w), BosWire::V1(BosMsgV1::AttachHello(h)) if h.app_id == "mecha-aether"));
    }

    #[test]
    fn old_variants_keep_their_discriminants() {
        // Ping was the 6th variant (index 5) before AttachHello was appended;
        // bincode standard() writes enum variants as varint indices.
        let bytes = bincode::serde::encode_to_vec(&BosWire::V1(BosMsgV1::Ping), standard()).unwrap();
        assert_eq!(bytes, vec![0, 5], "BosWire::V1 = 0, BosMsgV1::Ping = 5");
        let bytes = bincode::serde::encode_to_vec(&BosWire::V1(BosMsgV1::Pong), standard()).unwrap();
        assert_eq!(bytes, vec![0, 6]);
        let bytes = bincode::serde::encode_to_vec(&BosWire::V1(BosMsgV1::Request { request_id: 1, body: RequestBody::GameTicket }), standard()).unwrap();
        // Request = 2, request_id = 1 (varint), GameTicket = 6th RequestBody variant
        assert_eq!(bytes, vec![0, 2, 1, 6]);
    }

    #[test]
    fn license_frames_are_appended() {
        // License is the 8th RequestBody variant (index 7) and the 8th
        // ResponseBody variant: every older frame keeps its bytes.
        let bytes = bincode::serde::encode_to_vec(&BosWire::V1(BosMsgV1::Request { request_id: 1, body: RequestBody::License { version: None } }), standard()).unwrap();
        assert_eq!(bytes, vec![0, 2, 1, 7, 0], "License = 7, then its version: None");
        let w = BosWire::V1(BosMsgV1::Request { request_id: 1, body: RequestBody::License { version: Some("2027.0.0".into()) } });
        match roundtrip(&w) {
            BosWire::V1(BosMsgV1::Request { body: RequestBody::License { version }, .. }) => assert_eq!(version.as_deref(), Some("2027.0.0")),
            other => panic!("{other:?}"),
        }
        let w = BosWire::V1(BosMsgV1::Response {
            request_id: 4,
            body: ResponseBody::License(LicenseGrant {
                token: "h.p.s".into(),
                expires_at: std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_800_000_000),
                app_id: "trillavio".into(),
            }),
        });
        let bytes = bincode::serde::encode_to_vec(&w, standard()).unwrap();
        assert_eq!(&bytes[..4], &[0, 3, 4, 7], "Response = 3, request_id 4, License = 7");
        match roundtrip(&w) {
            BosWire::V1(BosMsgV1::Response { request_id: 4, body: ResponseBody::License(l) }) => assert_eq!(l.token, "h.p.s"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn game_ticket_response_roundtrips() {
        let w = BosWire::V1(BosMsgV1::Response {
            request_id: 9,
            body: ResponseBody::GameTicket(GameTicket {
                ticket: "a.b.c".into(),
                expires_at: std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_800_000_000),
                app_id: "mecha-aether".into(),
            }),
        });
        match roundtrip(&w) {
            BosWire::V1(BosMsgV1::Response { request_id: 9, body: ResponseBody::GameTicket(t) }) => {
                assert_eq!(t.ticket, "a.b.c");
            }
            other => panic!("{other:?}"),
        }
    }
}
