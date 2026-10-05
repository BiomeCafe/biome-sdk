//! Game-facing events. Drain via [`crate::BiomeClient::next_event`] (sync poll)
//! or via the bevy plugin's `EventReader<BiomeEvent>` (feature = "bevy").

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::types::{ConnectInfo, MatchId, Session};

/// One-of-N platform event. Always non-blocking to construct; safe to handle
/// at any cadence (game tick, periodic system, etc).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum BiomeEvent {
    // ── Session ──────────────────────────────────────────────────────────
    /// Auth completed (initial sign-in or post-disconnect resume).
    SessionEstablished(Session),
    /// Backend cleared the session. Game should return to menu / re-login.
    SessionLost { reason: String },

    // ── Platform health ──────────────────────────────────────────────────
    /// IPC to biome-app dropped. Match in progress should keep playing —
    /// Vultr game-server is independent. Result manifest will be buffered.
    PlatformDisconnected,
    /// IPC reconnected after a drop. Any buffered match results will ship.
    PlatformReconnected,
    /// Server announced a backend protocol bump that this SDK version doesn't
    /// understand. Surface a "please update" dialog; subsequent calls fail.
    ProtocolStale { min_supported_sdk_version: u32 },

    // ── Friends / presence ───────────────────────────────────────────────
    FriendOnline { friend_id: Uuid, username: String },
    FriendOffline { friend_id: Uuid },

    // ── Party ────────────────────────────────────────────────────────────
    PartyInvite {
        from_user_id: Uuid,
        from_username: String,
        party_id: Uuid,
    },
    PartyJoined { party_id: Uuid },
    PartyLeft { party_id: Uuid },

    // ── Matchmaking + match lifecycle ────────────────────────────────────
    /// Server provisioned, connect bundle ready. Game should pre-load assets
    /// then connect via the bundle.
    MatchReady { match_id: MatchId, bundle: ConnectInfo },
    /// Match results committed server-side. Game can show rank delta, drops.
    MatchFinalized {
        match_id: MatchId,
        rank_delta: i32,
        /// Item / cosmetic drops, opaque identifiers the game knows how to render.
        drops: Vec<String>,
    },
    /// Matchmaking ticket cancelled (by user, by timeout, by error).
    MatchTicketCancelled { reason: String },

    // ── Generic notification (mirrors biome-rt `notif.new`) ──────────────
    /// A non-game social notification (someone followed you, etc). Game
    /// usually ignores; biome-app desktop UI handles. Surfaced in case the
    /// game wants to render an in-game toast.
    Notification { kind: String, ref_id: Option<String> },
}
