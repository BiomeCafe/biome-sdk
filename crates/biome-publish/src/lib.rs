//! Library behind the `biome-publish` binary (see `src/main.rs` for usage
//! and `docs/GAME_PUBLISHING_SPEC.md` §3, §4, §7 for the contract).
//!
//! - [`manifest`]: `biome.toml` parsing and validation.
//! - [`pack`]: sha256 and deterministic client zips.
//! - [`files`]: per-file clients: file lists, build ids, deterministic gzip.
//! - [`prepare`]: `check`, turning a manifest into the files to upload.
//! - [`api`]: the `game-publish` function and presigned uploads.
//! - [`cli`]: the commands; [`run`] is the whole program.

pub mod api;
pub mod cli;
pub mod error;
pub mod files;
pub mod manifest;
pub mod pack;
pub mod prepare;

pub use cli::{run, Env};
pub use error::{Error, Refusal, EXIT_INVALID, EXIT_IO, EXIT_OK, EXIT_REFUSED};
