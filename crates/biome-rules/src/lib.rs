//! # biome-rules — the rules SDK for Biome-hosted turn-based games
//!
//! A game author writes **one pure crate** against [`Rules`]. It compiles
//! twice:
//!
//! - natively, into the game client (legality checks, animation events,
//!   offline bots), through the typed [`Engine`];
//! - to `wasm32-unknown-unknown`, for the match host, through the byte-level
//!   [`abi`] that [`export!`] turns into WebAssembly exports.
//!
//! The match host (a Durable Object per match) owns the state. Clients send
//! moves; the host validates and applies them with this crate's code and
//! tells each seat only what [`Rules::view`] lets it see.
//!
//! ## The three rules a rules crate must follow
//!
//! 1. **Deterministic.** Same state, move and seed → same result on every
//!    target. No `HashMap` iteration, no floats in rules, no clock, no threads.
//! 2. **Randomness only from [`Rng`].** The host seeds it per call and logs
//!    the seed, so matches replay and nobody can predict a draw.
//! 3. **Hidden information only leaves through [`Rules::view`] and
//!    [`Scoped`] events.** The host never sends `State` to anyone.
//!
//! ## Minimal game
//!
//! ```ignore
//! pub struct MyGame;
//! impl biome_rules::Rules for MyGame { /* … */ }
//! biome_rules::export!(MyGame);            // wasm exports (no-op natively)
//! ```
//!
//! and in `Cargo.toml`: `crate-type = ["cdylib", "rlib"]`.
//!
//! See `docs/MATCH_HOST_SPEC.md` for the whole design, `crates/rules-tictactoe`
//! and `crates/rules-highcard` for two complete games.

pub mod abi;
pub mod codec;
pub mod engine;
pub mod rng;
pub mod sim;
pub mod types;
pub mod wire;

#[cfg(target_arch = "wasm32")]
pub mod export;

pub use engine::Engine;
pub use rng::Rng;
pub use types::{Audience, Meta, Outcome, Reject, Scoped, Seat, SeatInfo, Turn};

use serde::de::DeserializeOwned;
use serde::Serialize;

/// A game's rules. Implement on a unit struct; every function is an
/// associated function because the state is passed in — the implementation
/// keeps nothing between calls.
pub trait Rules {
    /// Short machine name, e.g. `"mecha-aether"`. Informational.
    const NAME: &'static str;
    /// Version of the *rules*. The client and the host must run exactly the
    /// same value; the host refuses a client that doesn't. Bump it whenever
    /// `apply` could produce a different result for the same input.
    const VERSION: &'static str;
    const MIN_SEATS: u8;
    const MAX_SEATS: u8;

    /// Match setup chosen before the first move (mode, decks, clocks the
    /// game cares about…).
    type Config: Serialize + DeserializeOwned;
    /// The whole truth. Never leaves the host.
    type State: Serialize + DeserializeOwned + Clone;
    /// What a player asks for.
    type Move: Serialize + DeserializeOwned;
    /// What one seat is allowed to see.
    type View: Serialize + DeserializeOwned;
    /// What just happened, for animation and logs.
    type Event: Serialize + DeserializeOwned;

    /// Build the initial state. `seats` are numbered `0..n` in order, with
    /// `MIN_SEATS <= n <= MAX_SEATS` already checked.
    fn init(config: &Self::Config, seats: &[SeatInfo], rng: &mut Rng) -> Result<Self::State, Reject>;

    /// Validate and apply one move. The caller has already checked that the
    /// game is not over and that `seat` is one of [`Rules::to_move`]. On
    /// `Err` any change made to `state` is discarded by the caller.
    fn apply(
        state: &mut Self::State,
        seat: Seat,
        mv: Self::Move,
        rng: &mut Rng,
    ) -> Result<Vec<Scoped<Self::Event>>, Reject>;

    /// Whose move it is. Must be [`Turn::Over`] exactly when
    /// [`Rules::outcome`] is `Some`.
    fn to_move(state: &Self::State) -> Turn;

    /// Project the state for one seat, or for [`Seat::SPECTATOR`].
    fn view(state: &Self::State, seat: Seat) -> Self::View;

    /// `Some` once the match is decided.
    fn outcome(state: &Self::State) -> Option<Outcome>;

    /// `seat` ran out of clock (or disconnected past the grace period). The
    /// usual answer is "that seat forfeits"; a game may instead auto-pass.
    fn on_timeout(state: &mut Self::State, seat: Seat, rng: &mut Rng) -> Vec<Scoped<Self::Event>>;
}

/// Optional: enumerate legal moves from a *view*. Lets the client grey out
/// illegal plays and gives [`sim`] a random-legal bot for free.
pub trait LegalMoves: Rules {
    fn legal_moves(view: &Self::View, seat: Seat) -> Vec<Self::Move>;
}

/// Generate the WebAssembly exports (`br_abi`, `br_alloc`, `br_free`,
/// `br_meta`, `br_init`, `br_apply`, `br_timeout`, `br_to_move`, `br_view`,
/// `br_outcome`) for a [`Rules`] implementation. Expands to nothing on
/// native targets, so the same crate links into the game client unchanged.
///
/// Put it once at the crate root: `biome_rules::export!(MyGame);`
#[macro_export]
macro_rules! export {
    ($rules:ty) => {
        #[cfg(target_arch = "wasm32")]
        const _: () = {
            use $crate::abi;
            use $crate::export as ex;

            #[no_mangle]
            pub extern "C" fn br_abi() -> u32 {
                abi::ABI_VERSION
            }

            #[no_mangle]
            pub extern "C" fn br_alloc(len: u32) -> u32 {
                ex::alloc(len)
            }

            /// # Safety
            /// `ptr`/`len` must come from `br_alloc` or from a returned buffer.
            #[no_mangle]
            pub unsafe extern "C" fn br_free(ptr: u32, len: u32) {
                ex::free(ptr, len)
            }

            #[no_mangle]
            pub extern "C" fn br_meta() -> u64 {
                ex::give(abi::meta::<$rules>())
            }

            /// # Safety
            /// Pointers must describe buffers obtained from `br_alloc`; `seed` points at 32 bytes.
            #[no_mangle]
            pub unsafe extern "C" fn br_init(cfg: u32, cfg_len: u32, seats: u32, seats_len: u32, seed: u32) -> u64 {
                ex::give(abi::init::<$rules>(ex::slice(cfg, cfg_len), ex::slice(seats, seats_len), &ex::seed(seed)))
            }

            /// # Safety
            /// See `br_init`.
            #[no_mangle]
            pub unsafe extern "C" fn br_apply(state: u32, state_len: u32, seat: u32, mv: u32, mv_len: u32, seed: u32) -> u64 {
                ex::give(abi::apply::<$rules>(ex::slice(state, state_len), ex::seat(seat), ex::slice(mv, mv_len), &ex::seed(seed)))
            }

            /// # Safety
            /// See `br_init`.
            #[no_mangle]
            pub unsafe extern "C" fn br_timeout(state: u32, state_len: u32, seat: u32, seed: u32) -> u64 {
                ex::give(abi::timeout::<$rules>(ex::slice(state, state_len), ex::seat(seat), &ex::seed(seed)))
            }

            /// # Safety
            /// See `br_init`.
            #[no_mangle]
            pub unsafe extern "C" fn br_to_move(state: u32, state_len: u32) -> u64 {
                ex::give(abi::to_move::<$rules>(ex::slice(state, state_len)))
            }

            /// # Safety
            /// See `br_init`.
            #[no_mangle]
            pub unsafe extern "C" fn br_view(state: u32, state_len: u32, seat: u32) -> u64 {
                ex::give(abi::view::<$rules>(ex::slice(state, state_len), ex::seat(seat)))
            }

            /// # Safety
            /// See `br_init`.
            #[no_mangle]
            pub unsafe extern "C" fn br_outcome(state: u32, state_len: u32) -> u64 {
                ex::give(abi::outcome::<$rules>(ex::slice(state, state_len)))
            }
        };
    };
}
