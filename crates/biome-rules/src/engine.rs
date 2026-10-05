//! The typed, native face of a rules crate, plus the checks every entry
//! point shares. The byte-level [`crate::abi`] is built on the same
//! `do_*` functions, so the client-side [`Engine`] and the host's
//! WebAssembly module cannot disagree about turn order, game-over handling
//! or what happens to state on a rejected move.

use crate::rng::Rng;
use crate::types::{Outcome, Reject, Scoped, Seat, SeatInfo, Turn};
use crate::{codec, Rules};

/// Seats must be `0..n` in order with `MIN_SEATS <= n <= MAX_SEATS`.
pub(crate) fn check_seats<R: Rules>(seats: &[SeatInfo]) -> Result<(), Reject> {
    let n = seats.len();
    if n < R::MIN_SEATS as usize || n > R::MAX_SEATS as usize {
        return Err(Reject::new(
            Reject::BAD_SEATS,
            format!("{} seat(s); {} needs {}..={}", n, R::NAME, R::MIN_SEATS, R::MAX_SEATS),
        ));
    }
    for (i, s) in seats.iter().enumerate() {
        if s.seat.index() != i {
            return Err(Reject::new(Reject::BAD_SEATS, format!("seat {} at position {i}", s.seat.0)));
        }
    }
    Ok(())
}

pub(crate) fn do_init<R: Rules>(
    config: &R::Config,
    seats: &[SeatInfo],
    seed: &[u8; 32],
) -> Result<R::State, Reject> {
    check_seats::<R>(seats)?;
    let mut rng = Rng::from_seed(*seed);
    R::init(config, seats, &mut rng)
}

/// Turn + game-over checks, then the game's `apply` on a copy. The input
/// state is never touched; a reject leaves the caller with what it had.
pub(crate) fn do_apply<R: Rules>(
    state: &R::State,
    seat: Seat,
    mv: R::Move,
    seed: &[u8; 32],
) -> Result<(R::State, Vec<Scoped<R::Event>>), Reject> {
    if R::outcome(state).is_some() {
        return Err(Reject::new(Reject::GAME_OVER, "the match is over"));
    }
    if !R::to_move(state).includes(seat) {
        return Err(Reject::new(Reject::NOT_YOUR_TURN, format!("seat {} may not move now", seat.0)));
    }
    let mut next = state.clone();
    let mut rng = Rng::from_seed(*seed);
    let events = R::apply(&mut next, seat, mv, &mut rng)?;
    Ok((next, events))
}

pub(crate) fn do_timeout<R: Rules>(
    state: &R::State,
    seat: Seat,
    seed: &[u8; 32],
) -> Result<(R::State, Vec<Scoped<R::Event>>), Reject> {
    if R::outcome(state).is_some() {
        return Err(Reject::new(Reject::GAME_OVER, "the match is over"));
    }
    if !R::to_move(state).includes(seat) {
        return Err(Reject::new(Reject::NOT_YOUR_TURN, format!("seat {} is not on the clock", seat.0)));
    }
    let mut next = state.clone();
    let mut rng = Rng::from_seed(*seed);
    let events = R::on_timeout(&mut next, seat, &mut rng);
    Ok((next, events))
}

/// A match held in memory with the game's own types. What the client uses
/// for previews, bots and offline play; what tests use to script matches.
pub struct Engine<R: Rules> {
    state: R::State,
}

impl<R: Rules> Clone for Engine<R> {
    fn clone(&self) -> Self {
        Self { state: self.state.clone() }
    }
}

impl<R: Rules> Engine<R> {
    pub fn new(config: &R::Config, seats: &[SeatInfo], seed: [u8; 32]) -> Result<Self, Reject> {
        Ok(Self { state: do_init::<R>(config, seats, &seed)? })
    }

    pub fn from_state(state: R::State) -> Self {
        Self { state }
    }

    pub fn from_state_bytes(bytes: &[u8]) -> Result<Self, Reject> {
        codec::decode::<R::State>(bytes)
            .map(|state| Self { state })
            .map_err(|e| Reject::new(Reject::BAD_STATE, e))
    }

    /// Apply a move. On `Err` the engine is unchanged.
    pub fn apply(&mut self, seat: Seat, mv: R::Move, seed: [u8; 32]) -> Result<Vec<Scoped<R::Event>>, Reject> {
        let (next, events) = do_apply::<R>(&self.state, seat, mv, &seed)?;
        self.state = next;
        Ok(events)
    }

    /// `seat` ran out of time. On `Err` the engine is unchanged.
    pub fn timeout(&mut self, seat: Seat, seed: [u8; 32]) -> Result<Vec<Scoped<R::Event>>, Reject> {
        let (next, events) = do_timeout::<R>(&self.state, seat, &seed)?;
        self.state = next;
        Ok(events)
    }

    pub fn view(&self, seat: Seat) -> R::View {
        R::view(&self.state, seat)
    }

    pub fn to_move(&self) -> Turn {
        R::to_move(&self.state)
    }

    pub fn outcome(&self) -> Option<Outcome> {
        R::outcome(&self.state)
    }

    pub fn state(&self) -> &R::State {
        &self.state
    }

    /// The canonical bytes of the state (what the host stores). An encode
    /// failure is reported, never papered over with an empty buffer.
    pub fn state_bytes(&self) -> Result<Vec<u8>, Reject> {
        codec::encode(&self.state).map_err(|e| Reject::new("internal", format!("state encode: {e}")))
    }
}

/// The events one seat is allowed to receive, in order.
pub fn events_for<E: Clone>(events: &[Scoped<E>], seat: Seat) -> Vec<E> {
    events.iter().filter(|e| e.to.includes(seat)).map(|e| e.event.clone()).collect()
}
