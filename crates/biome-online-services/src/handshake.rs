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

/// Where this process's environment nonce is at: free, claimed by a
/// connect that hasn't sent it yet, or sent.
const NONCE_FREE: u8 = 0;
const NONCE_CLAIMED: u8 = 1;
const NONCE_SENT: u8 = 2;
static NONCE_STATE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(NONCE_FREE);

/// The environment's launch nonce, held by one connect until its hello has
/// gone out ([`NonceClaim::sent`]). Dropped without that (the pipe couldn't
/// be opened, the hello couldn't be written), the nonce is free again for
/// the next connect: the host never saw it, so it is still good.
pub struct NonceClaim {
    nonce: String,
    state: &'static std::sync::atomic::AtomicU8,
    sent: bool,
}

impl NonceClaim {
    pub fn nonce(&self) -> &str {
        &self.nonce
    }

    /// The hello carrying the nonce was written: it is used, whatever the
    /// host answers.
    pub fn sent(mut self) {
        self.state.store(NONCE_SENT, std::sync::atomic::Ordering::SeqCst);
        self.sent = true;
    }
}

impl Drop for NonceClaim {
    fn drop(&mut self) {
        if !self.sent {
            let _ = self.state.compare_exchange(
                NONCE_CLAIMED,
                NONCE_FREE,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            );
        }
    }
}

/// Claim the launch nonce from the environment for one connect. `None` when
/// there is none, when it was already sent, or while another connect holds
/// it. A nonce is good once, so showing it again could only be refused: a
/// later connect (a reconnect, signing in again) attaches instead, which
/// Biome lets through without a prompt for a game it installed
/// (GAME_SDK_SPEC.md §4.2).
pub fn claim_launch_nonce() -> Option<NonceClaim> {
    claim_in(&NONCE_STATE, read_launch_nonce())
}

/// The launch nonce from the environment, marked used at once. Kept for
/// callers that send it themselves; `BiomeClient` uses
/// [`claim_launch_nonce`], which marks it used only once the hello is out.
pub fn take_launch_nonce() -> Option<String> {
    let claim = claim_launch_nonce()?;
    let nonce = claim.nonce.clone();
    claim.sent();
    Some(nonce)
}

fn claim_in(state: &'static std::sync::atomic::AtomicU8, nonce: Option<String>) -> Option<NonceClaim> {
    let nonce = nonce?;
    state
        .compare_exchange(NONCE_FREE, NONCE_CLAIMED, std::sync::atomic::Ordering::SeqCst, std::sync::atomic::Ordering::SeqCst)
        .ok()
        .map(|_| NonceClaim { nonce, state, sent: false })
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
    fn the_environment_nonce_is_used_once_its_hello_is_sent() {
        static STATE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(NONCE_FREE);
        // Nothing in the environment: nothing, and nothing used up.
        assert!(claim_in(&STATE, None).is_none());
        // A connect that never got its hello out (no pipe) gives it back.
        let first = claim_in(&STATE, Some("n".into())).expect("free");
        assert_eq!(first.nonce(), "n");
        // While it is held, another connect attaches instead.
        assert!(claim_in(&STATE, Some("n".into())).is_none());
        drop(first);
        // So the next connect still has it...
        let second = claim_in(&STATE, Some("n".into())).expect("given back");
        second.sent();
        // ...and once sent, it is gone for good.
        assert!(claim_in(&STATE, Some("n".into())).is_none(), "the next connect attaches");
    }

    #[test]
    fn nonces_are_unique() {
        let set: HashSet<String> = (0..1000).map(|_| generate_launch_nonce()).collect();
        assert_eq!(set.len(), 1000);
    }
}
