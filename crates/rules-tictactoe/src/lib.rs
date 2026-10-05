//! Tic-tac-toe — the smallest complete `biome-rules` game.
//!
//! Two seats, perfect information, no randomness. Seat 0 plays first.
//! Copy this file to start a new game: everything a rules crate needs is
//! here, and nothing else.

use biome_rules::{LegalMoves, Outcome, Reject, Rng, Rules, Scoped, Seat, SeatInfo, Turn};
use serde::{Deserialize, Serialize};

pub struct TicTacToe;

/// No options.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {}

/// `board[i]`: 0 = empty, 1 = seat 0, 2 = seat 1. Cells are row-major:
///
/// ```text
/// 0 1 2
/// 3 4 5
/// 6 7 8
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub board: [u8; 9],
    pub next: u8,
    /// Set when a seat gives up or times out.
    pub forfeited: Option<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Move {
    Place { cell: u8 },
    Resign,
}

/// Everything is public, so the view is the state plus "which seat are you".
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct View {
    pub board: [u8; 9],
    pub next: Option<u8>,
    pub you: Option<u8>,
    pub winner: Option<u8>,
    pub draw: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    Placed { seat: u8, cell: u8 },
    Forfeited { seat: u8 },
}

const LINES: [[usize; 3]; 8] = [
    [0, 1, 2],
    [3, 4, 5],
    [6, 7, 8],
    [0, 3, 6],
    [1, 4, 7],
    [2, 5, 8],
    [0, 4, 8],
    [2, 4, 6],
];

fn line_winner(board: &[u8; 9]) -> Option<u8> {
    LINES
        .iter()
        .find(|l| board[l[0]] != 0 && board[l[0]] == board[l[1]] && board[l[1]] == board[l[2]])
        .map(|l| board[l[0]] - 1)
}

fn full(board: &[u8; 9]) -> bool {
    board.iter().all(|c| *c != 0)
}

impl Rules for TicTacToe {
    const NAME: &'static str = "tictactoe";
    const VERSION: &'static str = "1.0.0";
    const MIN_SEATS: u8 = 2;
    const MAX_SEATS: u8 = 2;

    type Config = Config;
    type State = State;
    type Move = Move;
    type View = View;
    type Event = Event;

    fn init(_config: &Config, _seats: &[SeatInfo], _rng: &mut Rng) -> Result<State, Reject> {
        Ok(State { board: [0; 9], next: 0, forfeited: None })
    }

    fn apply(state: &mut State, seat: Seat, mv: Move, _rng: &mut Rng) -> Result<Vec<Scoped<Event>>, Reject> {
        match mv {
            Move::Place { cell } => {
                let Some(slot) = state.board.get_mut(cell as usize) else {
                    return Err(Reject::illegal(format!("cell {cell} is off the board")));
                };
                if *slot != 0 {
                    return Err(Reject::illegal(format!("cell {cell} is taken")));
                }
                *slot = seat.0 + 1;
                state.next = 1 - seat.0;
                Ok(vec![Scoped::all(Event::Placed { seat: seat.0, cell })])
            }
            Move::Resign => {
                state.forfeited = Some(seat.0);
                Ok(vec![Scoped::all(Event::Forfeited { seat: seat.0 })])
            }
        }
    }

    fn to_move(state: &State) -> Turn {
        if Self::outcome(state).is_some() {
            Turn::Over
        } else {
            Turn::Seat(Seat(state.next))
        }
    }

    fn view(state: &State, seat: Seat) -> View {
        let outcome = Self::outcome(state);
        View {
            board: state.board,
            next: if outcome.is_some() { None } else { Some(state.next) },
            you: if seat.is_spectator() { None } else { Some(seat.0) },
            winner: outcome.as_ref().and_then(|o| o.winners.first().map(|s| s.0)),
            draw: outcome.map(|o| o.draw).unwrap_or(false),
        }
    }

    fn outcome(state: &State) -> Option<Outcome> {
        if let Some(loser) = state.forfeited {
            return Some(Outcome::win(Seat(1 - loser), "forfeit"));
        }
        if let Some(winner) = line_winner(&state.board) {
            return Some(Outcome::win(Seat(winner), "three-in-a-row"));
        }
        if full(&state.board) {
            return Some(Outcome::draw("board-full"));
        }
        None
    }

    fn on_timeout(state: &mut State, seat: Seat, _rng: &mut Rng) -> Vec<Scoped<Event>> {
        state.forfeited = Some(seat.0);
        vec![Scoped::all(Event::Forfeited { seat: seat.0 })]
    }
}

impl LegalMoves for TicTacToe {
    fn legal_moves(view: &View, seat: Seat) -> Vec<Move> {
        if view.next != Some(seat.0) {
            return Vec::new();
        }
        view.board
            .iter()
            .enumerate()
            .filter(|(_, c)| **c == 0)
            .map(|(i, _)| Move::Place { cell: i as u8 })
            .collect()
    }
}

biome_rules::export!(TicTacToe);

#[cfg(test)]
mod tests {
    use super::*;
    use biome_rules::{sim, Engine};

    const SEED: [u8; 32] = [0; 32];

    fn new_game() -> Engine<TicTacToe> {
        Engine::new(&Config {}, &sim::seats(2), SEED).unwrap()
    }

    fn place(e: &mut Engine<TicTacToe>, seat: u8, cell: u8) -> Result<Vec<Scoped<Event>>, Reject> {
        e.apply(Seat(seat), Move::Place { cell }, SEED)
    }

    #[test]
    fn seat_zero_wins_the_top_row() {
        let mut e = new_game();
        for (seat, cell) in [(0, 0), (1, 3), (0, 1), (1, 4)] {
            place(&mut e, seat, cell).unwrap();
            assert!(e.outcome().is_none());
        }
        let events = place(&mut e, 0, 2).unwrap();
        assert_eq!(events, vec![Scoped::all(Event::Placed { seat: 0, cell: 2 })]);
        assert_eq!(e.outcome(), Some(Outcome::win(Seat(0), "three-in-a-row")));
        assert_eq!(e.to_move(), Turn::Over);
        assert_eq!(e.view(Seat(1)).winner, Some(0));
    }

    #[test]
    fn a_full_board_without_a_line_is_a_draw() {
        let mut e = new_game();
        // X O X / X O O / O X X
        for (seat, cell) in [(0, 0), (1, 1), (0, 2), (1, 4), (0, 3), (1, 5), (0, 7), (1, 6), (0, 8)] {
            place(&mut e, seat, cell).unwrap();
        }
        assert_eq!(e.outcome(), Some(Outcome::draw("board-full")));
        assert!(e.view(Seat::SPECTATOR).draw);
    }

    #[test]
    fn out_of_turn_taken_cell_and_off_board_are_rejected_and_change_nothing() {
        let mut e = new_game();
        let before = e.state_bytes().unwrap();
        assert_eq!(place(&mut e, 1, 0).unwrap_err().code, Reject::NOT_YOUR_TURN);
        assert_eq!(place(&mut e, 0, 9).unwrap_err().code, Reject::ILLEGAL);
        assert_eq!(e.state_bytes().unwrap(), before);
        place(&mut e, 0, 4).unwrap();
        let before = e.state_bytes().unwrap();
        assert_eq!(place(&mut e, 1, 4).unwrap_err().code, Reject::ILLEGAL);
        assert_eq!(e.state_bytes().unwrap(), before);
        assert_eq!(e.apply(Seat::SPECTATOR, Move::Resign, SEED).unwrap_err().code, Reject::NOT_YOUR_TURN);
    }

    #[test]
    fn no_moves_after_the_end() {
        let mut e = new_game();
        e.apply(Seat(0), Move::Resign, SEED).unwrap();
        assert_eq!(e.outcome(), Some(Outcome::win(Seat(1), "forfeit")));
        assert_eq!(place(&mut e, 1, 0).unwrap_err().code, Reject::GAME_OVER);
        assert_eq!(e.timeout(Seat(1), SEED).unwrap_err().code, Reject::GAME_OVER);
    }

    #[test]
    fn timeout_forfeits_the_seat_on_the_clock_only() {
        let mut e = new_game();
        assert_eq!(e.timeout(Seat(1), SEED).unwrap_err().code, Reject::NOT_YOUR_TURN);
        e.timeout(Seat(0), SEED).unwrap();
        assert_eq!(e.outcome(), Some(Outcome::win(Seat(1), "forfeit")));
    }

    #[test]
    fn wrong_seat_counts_are_refused() {
        for n in [0u8, 1, 3] {
            let err = Engine::<TicTacToe>::new(&Config {}, &sim::seats(n), SEED).err().unwrap();
            assert_eq!(err.code, Reject::BAD_SEATS);
        }
    }

    #[test]
    fn random_games_always_finish_legally() {
        for i in 0..200u8 {
            let r = sim::play_random::<TicTacToe>(&Config {}, 2, [i; 32], 20).unwrap();
            assert!(r.outcome.is_some(), "game {i} did not finish");
            assert!(r.log.len() >= 5 && r.log.len() <= 9);
            assert_eq!(r.states.len(), r.log.len() + 1);
        }
    }
}
