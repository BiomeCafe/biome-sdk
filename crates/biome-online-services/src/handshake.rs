//! Launch-nonce handshake authentication.
//!
//! When biome-app launches a game (`Steam pattern`), it sets:
//!
//! ```text
//! BIOME_APP_ID         = "cafe-shooter"
//! BIOME_SESSION_NONCE  = <random per-launch string>
//! BIOME_GAME_PID       = <child pid>  // optional
//! ```
//!
//! The BOS client reads `BIOME_SESSION_NONCE` on init and presents it in
//! [`crate::protocol::ClientHello`]. The server verifies the nonce was one
//! it issued; if not, the connection is rejected.
//!
//! `--dev-mode` on the server bypasses verification entirely so the game can
//! be launched independently for dev / integration testing.

use std::env;

pub const ENV_APP_ID: &str = "BIOME_APP_ID";
pub const ENV_SESSION_NONCE: &str = "BIOME_SESSION_NONCE";
pub const ENV_GAME_PID: &str = "BIOME_GAME_PID";

/// Try to read the launch nonce from the environment. Returns `None` if the
/// game wasn't launched through biome-app.
pub fn read_launch_nonce() -> Option<String> {
    env::var(ENV_SESSION_NONCE).ok().filter(|s| !s.is_empty())
}

/// Try to read the app id biome-app says it's launching. Compared by client
/// against its configured `app_id` to catch wrong-binary-launch bugs.
pub fn read_launch_app_id() -> Option<String> {
    env::var(ENV_APP_ID).ok().filter(|s| !s.is_empty())
}

/// Generate a launch nonce. Used by the server side when spawning a game.
///
/// 64 hex chars from two v4 UUIDs (244 random bits from the OS CSPRNG via
/// `getrandom`) — a malicious local process can't guess a valid nonce in the
/// window before the game's handshake. (The previous time + thread-id +
/// counter scheme was predictable.)
pub fn generate_launch_nonce() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn nonce_is_64_hex_chars() {
        let n = generate_launch_nonce();
        assert_eq!(n.len(), 64);
        assert!(n.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn nonces_are_unique() {
        let set: HashSet<String> = (0..1000).map(|_| generate_launch_nonce()).collect();
        assert_eq!(set.len(), 1000);
    }
}
