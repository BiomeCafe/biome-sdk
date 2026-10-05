//! # Biome Online Services (BOS)
//!
//! Game-facing client for the Biome platform. Speaks to the locally-running
//! Biome desktop process via IPC; the desktop process owns the OAuth tokens,
//! the WebSocket connection to biome-rt, the Supabase REST surface — game
//! never sees raw credentials.
//!
//! Two ways in (see [`client`]):
//!
//! - **Launched by Biome** (Steamworks pattern): Biome starts the game with a
//!   one-time nonce in its environment and the handshake is silent.
//! - **Attach** (2026-10): the game starts on its own, finds Biome
//!   ([`detect`]), wakes it if needed, and attaches. The first time a given
//!   app attaches, Biome asks the user; after that it is silent.
//!
//! What a game gets: a [`Session`] (user id, username, entitlements) and, on
//! request, a short-lived signed [`GameTicket`] it can hand to a match host
//! (a Cloudflare Worker, say) which verifies it offline against Biome's
//! published public key ([`ticket`], feature `ticket-verify`). The game never
//! receives the user's Supabase tokens.
//!
//! ## Layout
//!
//! - [`client`] — [`BiomeClient`] — what games link against
//! - [`detect`] — running / installed / not installed, and [`detect::wake`]
//! - [`ticket`] — ticket claims + offline verifier (feature `ticket-verify`)
//! - [`server`] — IPC server — what biome-app (or `biome-os-host`) runs
//! - [`protocol`] — wire types ([`BosWire`]) — shared
//! - [`types`] — public types ([`ConnectInfo`], [`Session`], etc.)
//! - [`events`] — [`BiomeEvent`] — what the game listens to
//! - [`transport`] — length-delimited bincode framing
//!
//! ## Threading model
//!
//! [`BiomeClient`] owns a `tokio::runtime::Runtime` on a dedicated thread.
//! Async IPC reads land in a crossbeam channel; game polls events at its own
//! cadence. Game code never sees `.await`, never blocks on the runtime, never
//! allocates per-frame.
//!
//! ## Minimal boot sequence (any engine)
//!
//! ```ignore
//! use std::time::Duration;
//! use biome_online_services::{detect, BiomeClient, BosConfig};
//!
//! // 1. Make sure Biome is up (starts it when installed but closed).
//! detect::ensure_running("mecha-aether", Duration::from_secs(60))?;
//! // 2. Attach (or nonce handshake if Biome launched us). First run prompts.
//! let bos = BiomeClient::connect(BosConfig::new("mecha-aether", "Mecha Aether"))?;
//! let me = bos.session().expect("signed in");
//! // 3. When connecting to a match host:
//! let ticket = bos.game_ticket().wait()?.ticket;
//! ```
//!
//! ## Bevy integration
//!
//! Enable the `bevy` feature for a `BosPlugin` that pumps events into
//! `EventWriter<BiomeEvent>` each frame (pinned to bevy 0.16; on other
//! versions write the ten-line plugin yourself around [`BiomeClient`]).

pub mod client;
pub mod detect;
pub mod error;
pub mod events;
pub mod handshake;
pub mod netcode_token;
pub mod protocol;
pub mod server;
pub mod transport;
pub mod types;

#[cfg(feature = "ticket-verify")]
pub mod ticket;

#[cfg(feature = "match-client")]
pub mod match_client;

#[cfg(feature = "bevy")]
pub mod bevy;

// Convenience re-exports for the common case
pub use client::{BiomeClient, ConnectOptions, ConnectStatus, PendingClient, RequestHandle};
pub use detect::PlatformState;
pub use error::BosError;
pub use events::BiomeEvent;
pub use types::{
    AttachVia, BosConfig, ConnectInfo, GameMode, GameTicket, MapId, MatchId, Region, RoomId,
    Session, TeamId, Transport, VoiceRooms,
};
