//! Rules ABI v1 — the byte-level interface the match host calls.
//!
//! These functions are plain Rust over slices so they can be unit-tested
//! natively; [`crate::export!`] wraps them in `extern "C"` exports for
//! wasm32. Layouts are documented in [`crate::wire`].
//!
//! Nothing here panics on hostile input: undecodable state, moves, config or
//! seats come back as a reject (`bad-state`, `bad-move`, `bad-config`,
//! `bad-seats`). A panic inside the game's own code becomes a WebAssembly
//! trap, which the host treats as a rejected call and keeps the old state.

use crate::engine::{do_apply, do_init, do_timeout};
use crate::types::{Audience, Meta, Reject, Scoped, Seat, SeatInfo};
use crate::{codec, wire, Rules};

/// Value of `br_abi()`. Bump only for an incompatible change to the exports
/// or to [`crate::wire`].
pub const ABI_VERSION: u32 = 1;

fn decode_state<R: Rules>(bytes: &[u8]) -> Result<R::State, Reject> {
    codec::decode::<R::State>(bytes).map_err(|e| Reject::new(Reject::BAD_STATE, e))
}

fn encode_events<R: Rules>(events: &[Scoped<R::Event>]) -> Result<Vec<(Audience, Vec<u8>)>, Reject> {
    events
        .iter()
        .map(|e| codec::encode(&e.event).map(|b| (e.to.clone(), b)))
        .collect::<Result<_, _>>()
        .map_err(|e| Reject::new("internal", format!("event encode: {e}")))
}

fn finish(result: Result<Vec<u8>, Reject>) -> Vec<u8> {
    match result {
        Ok(payload) => wire::ok(&payload),
        Err(r) => wire::reject(&r),
    }
}

/// JSON [`Meta`]. Cannot fail, so it carries no result tag.
pub fn meta<R: Rules>() -> Vec<u8> {
    serde_json::to_vec(&Meta {
        abi: ABI_VERSION,
        name: R::NAME.to_string(),
        rules_version: R::VERSION.to_string(),
        min_seats: R::MIN_SEATS,
        max_seats: R::MAX_SEATS,
    })
    .unwrap_or_default()
}

/// `config`: postcard `R::Config`. `seats`: JSON array of [`SeatInfo`].
/// Ok payload: state bytes.
pub fn init<R: Rules>(config: &[u8], seats_json: &[u8], seed: &[u8; 32]) -> Vec<u8> {
    finish((|| {
        let config = codec::decode::<R::Config>(config).map_err(|e| Reject::new(Reject::BAD_CONFIG, e))?;
        let seats: Vec<SeatInfo> = serde_json::from_slice(seats_json)
            .map_err(|e| Reject::new(Reject::BAD_SEATS, e.to_string()))?;
        let state = do_init::<R>(&config, &seats, seed)?;
        codec::encode(&state).map_err(|e| Reject::new("internal", format!("state encode: {e}")))
    })())
}

/// Ok payload: [`wire::ApplyOk`] (new state + scoped events).
pub fn apply<R: Rules>(state: &[u8], seat: u8, mv: &[u8], seed: &[u8; 32]) -> Vec<u8> {
    finish((|| {
        let state = decode_state::<R>(state)?;
        let mv = codec::decode::<R::Move>(mv).map_err(|e| Reject::new(Reject::BAD_MOVE, e))?;
        let (next, events) = do_apply::<R>(&state, Seat(seat), mv, seed)?;
        let state_bytes = codec::encode(&next).map_err(|e| Reject::new("internal", format!("state encode: {e}")))?;
        Ok(wire::encode_apply_ok(&state_bytes, &encode_events::<R>(&events)?))
    })())
}

/// Ok payload: [`wire::ApplyOk`].
pub fn timeout<R: Rules>(state: &[u8], seat: u8, seed: &[u8; 32]) -> Vec<u8> {
    finish((|| {
        let state = decode_state::<R>(state)?;
        let (next, events) = do_timeout::<R>(&state, Seat(seat), seed)?;
        let state_bytes = codec::encode(&next).map_err(|e| Reject::new("internal", format!("state encode: {e}")))?;
        Ok(wire::encode_apply_ok(&state_bytes, &encode_events::<R>(&events)?))
    })())
}

/// Ok payload: `[n][seat]*n` (n = 0 when over).
pub fn to_move<R: Rules>(state: &[u8]) -> Vec<u8> {
    finish(decode_state::<R>(state).map(|s| wire::encode_turn(&R::to_move(&s))))
}

/// Ok payload: postcard `R::View` for `seat` (255 = spectator).
pub fn view<R: Rules>(state: &[u8], seat: u8) -> Vec<u8> {
    finish((|| {
        let s = decode_state::<R>(state)?;
        codec::encode(&R::view(&s, Seat(seat))).map_err(|e| Reject::new("internal", format!("view encode: {e}")))
    })())
}

/// Ok payload: `[0]` or `[1]` + JSON [`crate::Outcome`].
pub fn outcome<R: Rules>(state: &[u8]) -> Vec<u8> {
    finish(decode_state::<R>(state).map(|s| wire::encode_outcome(&R::outcome(&s))))
}
