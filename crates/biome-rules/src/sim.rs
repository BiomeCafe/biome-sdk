//! Play whole matches in memory: bots, property tests, and a move log that
//! can be replayed through any other implementation of the same rules (the
//! compiled WebAssembly, in `biome-rules-host`'s conformance tests).
//!
//! Everything is derived from one master seed, so a simulated match is
//! reproducible from `(config, seat count, master seed, bot)`.

use crate::engine::Engine;
use crate::rng::Rng;
use crate::types::{Outcome, Reject, Seat, SeatInfo, Turn};
use crate::{codec, LegalMoves, Rules};

/// One applied move, exactly as the host would log it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogEntry {
    pub seat: Seat,
    /// postcard `R::Move`
    pub mv: Vec<u8>,
    /// The seed that call received.
    pub seed: [u8; 32],
}

/// A finished (or move-capped) simulated match.
#[derive(Clone, Debug)]
pub struct SimResult {
    /// postcard `R::Config`
    pub config: Vec<u8>,
    pub seats: Vec<SeatInfo>,
    pub init_seed: [u8; 32],
    pub log: Vec<LogEntry>,
    /// State bytes after `init` (index 0) and after each move (index i + 1).
    pub states: Vec<Vec<u8>>,
    pub outcome: Option<Outcome>,
}

/// `n` placeholder seats: `user-0`, `user-1`, …
pub fn seats(n: u8) -> Vec<SeatInfo> {
    (0..n)
        .map(|i| SeatInfo { seat: Seat(i), user_id: format!("user-{i}"), username: format!("player{i}") })
        .collect()
}

/// Play until the game is over or `max_moves` were applied. `pick` chooses
/// a move from what the seat can see; it gets its own generator so bot
/// randomness never touches the rules' seeds.
pub fn play<R: Rules>(
    config: &R::Config,
    n_seats: u8,
    master_seed: [u8; 32],
    mut pick: impl FnMut(&R::View, Seat, &mut Rng) -> Option<R::Move>,
    max_moves: usize,
) -> Result<SimResult, Reject> {
    let mut master = Rng::from_seed(master_seed);
    let init_seed = master.fork_seed();
    let mut bot_rng = Rng::from_seed(master.fork_seed());
    let seat_infos = seats(n_seats);

    let mut engine = Engine::<R>::new(config, &seat_infos, init_seed)?;
    let mut states = vec![engine.state_bytes()?];
    let mut log = Vec::new();

    while log.len() < max_moves {
        let seat = match engine.to_move() {
            Turn::Over => break,
            Turn::Seat(s) => s,
            // Simultaneous choice: resolve in list order.
            Turn::Seats(v) => match v.first() {
                Some(s) => *s,
                None => break,
            },
        };
        let view = engine.view(seat);
        let Some(mv) = pick(&view, seat, &mut bot_rng) else {
            return Err(Reject::illegal(format!("bot for seat {} had no move", seat.0)));
        };
        let mv_bytes = codec::encode(&mv).map_err(|e| Reject::new(Reject::BAD_MOVE, e))?;
        let seed = master.fork_seed();
        engine.apply(seat, mv, seed)?;
        log.push(LogEntry { seat, mv: mv_bytes, seed });
        states.push(engine.state_bytes()?);
    }

    Ok(SimResult {
        config: codec::encode(config).map_err(|e| Reject::new(Reject::BAD_CONFIG, e))?,
        seats: seat_infos,
        init_seed,
        log,
        states,
        outcome: engine.outcome(),
    })
}

/// [`play`] with a bot that picks uniformly among the legal moves.
pub fn play_random<R: LegalMoves>(
    config: &R::Config,
    n_seats: u8,
    master_seed: [u8; 32],
    max_moves: usize,
) -> Result<SimResult, Reject> {
    play::<R>(
        config,
        n_seats,
        master_seed,
        |view, seat, rng| {
            let mut moves = R::legal_moves(view, seat);
            if moves.is_empty() {
                None
            } else {
                let i = rng.below(moves.len() as u32) as usize;
                Some(moves.swap_remove(i))
            }
        },
        max_moves,
    )
}
