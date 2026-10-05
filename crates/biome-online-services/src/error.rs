//! Error types surfaced by the BOS client and server.

use thiserror::Error;

use crate::protocol::reject;

#[derive(Debug, Error)]
pub enum BosError {
    #[error("biome-app process is not running or not reachable")]
    PlatformUnavailable,

    #[error("handshake rejected by server: {0}")]
    HandshakeRejected(String),

    #[error("game was not launched through biome-app — no session nonce found in env")]
    MissingLaunchNonce,

    #[error("IPC transport error: {0}")]
    Transport(String),

    #[error("serialization error: {0}")]
    Serde(String),

    #[error("request timed out after {ms}ms")]
    Timeout { ms: u64 },

    #[error("request failed: {0}")]
    Request(String),

    #[error("SDK protocol version {sdk} too old; server requires >= {min_required}")]
    SdkTooOld { sdk: u32, min_required: u32 },

    #[error("internal error: {0}")]
    Internal(String),

    // ── Added 2026-10: attach, detection, tickets ────────────────────────
    /// Biome is running but nobody is signed in. Tell the player to sign in
    /// to Biome, then retry the attach.
    #[error("biome is running but nobody is signed in")]
    NotSignedIn,

    /// The user refused to let this app use their Biome identity (now, or
    /// remembered from an earlier prompt). Don't retry in a loop; offer a
    /// way to play without Biome or tell them where to change the answer.
    #[error("the user did not allow this app to use their biome account")]
    ConsentDenied,

    /// `app_id` is not registered with Biome (or is disabled).
    #[error("app id is not registered with biome")]
    UnknownApp,

    /// The user did not answer the consent prompt within the client's
    /// consent timeout.
    #[error("no answer to the consent prompt within {secs}s")]
    ConsentTimeout { secs: u64 },

    /// No Biome install was found on this machine. Point the player at the
    /// download page; nothing to wake.
    #[error("biome is not installed on this machine")]
    PlatformNotInstalled,

    /// Biome is installed but could not be started, or did not come up
    /// within the wait.
    #[error("could not start biome: {0}")]
    WakeFailed(String),

    /// A game ticket failed verification (bad signature, expired, wrong
    /// audience, unknown key).
    #[error("ticket invalid: {0}")]
    TicketInvalid(String),
}

impl BosError {
    /// Map a [`crate::protocol::ServerHello::Rejected`] reason onto a typed
    /// error. Stable codes (see [`reject`]) become their own variants; anything
    /// else is passed through as [`BosError::HandshakeRejected`].
    pub fn from_reject_reason(reason: &str) -> Self {
        match reason {
            reject::NOT_SIGNED_IN => BosError::NotSignedIn,
            reject::CONSENT_DENIED => BosError::ConsentDenied,
            reject::UNKNOWN_APP => BosError::UnknownApp,
            other => BosError::HandshakeRejected(other.to_string()),
        }
    }
}

impl From<std::io::Error> for BosError {
    fn from(e: std::io::Error) -> Self {
        BosError::Transport(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reject_codes_map_to_typed_errors() {
        assert!(matches!(BosError::from_reject_reason("not-signed-in"), BosError::NotSignedIn));
        assert!(matches!(BosError::from_reject_reason("consent-denied"), BosError::ConsentDenied));
        assert!(matches!(BosError::from_reject_reason("unknown-app"), BosError::UnknownApp));
        assert!(matches!(BosError::from_reject_reason("unknown or already-used session nonce"), BosError::HandshakeRejected(_)));
    }
}
