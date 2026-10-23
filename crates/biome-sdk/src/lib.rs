//! # The Biome SDK
//!
//! For apps and games that Biome installs and starts:
//!
//! - everything in [`biome_online_services`] (re-exported here): sign-in
//!   through the Biome desktop app ([`BiomeClient`]), game tickets, finding
//!   and waking Biome ([`detect`]), starting through Biome
//!   ([`restart_through_biome_if_needed`]), and with features the ticket
//!   verifier and the match client;
//! - [`license`]: a signed license that ties running the app to an invited
//!   Biome account and this device, for paid and private software.
//!
//! ```ignore
//! fn main() {
//!     let license = match biome_sdk::license::require("trillavio", env!("CARGO_PKG_VERSION")) {
//!         Ok(l) => l,
//!         Err(e) => return show_and_quit(&e.to_string()),
//!     };
//!     about_box(&license.about_line()); // "licensed to Mandy · INTERNAL build"
//! }
//! ```
//!
//! Contract: docs/GAME_SDK_SPEC.md §10 (licenses) and the rest of that file.

pub use biome_online_services;
pub use biome_online_services::*;

pub mod license;
