//! High-card — a tiny card game that exercises everything tic-tac-toe
//! doesn't: a deck shuffled by the host's seed, hidden hands, draws only the
//! drawing seat may see, and a score-based finish.
//!
//! Two seats. Each holds `hand_size` cards. A round: the leader plays a card
//! face up, then the other seat does; the higher rank scores a point (equal
//! ranks score nothing). Both then draw one card if the deck has any, and
//! the lead passes. After `rounds` rounds (or when hands run out) the higher
//! score wins.
//!
//! Cards are `0..52`; rank = `card % 13` (0 = two … 12 = ace).

use biome_rules::{LegalMoves, Outcome, Reject, Rng, Rules, Scoped, Seat, SeatInfo, Turn};
use serde::{Deserialize, Serialize};

pub struct HighCard;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub hand_size: u8,
    pub rounds: u8,
}

impl Default for Config {
    fn default() -> Self {
        Self { hand_size: 3, rounds: 5 }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    /// Remaining deck, top of the deck at the end. Hidden from everyone.
    pub deck: Vec<u8>,
    /// Hidden from the other seat.
    pub hands: [Vec<u8>; 2],
    /// Cards played this round, face up.
    pub table: [Option<u8>; 2],
    pub scores: [u8; 2],
    /// Who plays first this round.
    pub leader: u8,
    pub rounds_left: u8,
    pub forfeited: Option<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Move {
    Play { card: u8 },
    Resign,
}

/// What one seat sees. A spectator gets `hand: []` and both hand sizes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct View {
    pub you: Option<u8>,
    pub hand: Vec<u8>,
    pub hand_sizes: [u8; 2],
    pub table: [Option<u8>; 2],
    pub scores: [u8; 2],
    pub deck_count: u8,
    pub rounds_left: u8,
    pub to_play: Option<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    /// Public: a card hit the table.
    Played { seat: u8, card: u8 },
    /// Public: the round was scored. `winner: None` = equal ranks.
    RoundScored { winner: Option<u8>, scores: [u8; 2] },
    /// Private to the drawing seat: which card.
    Drew { card: u8 },
    /// Everyone else: that seat drew *a* card.
    OpponentDrew { seat: u8 },
    Forfeited { seat: u8 },
}

pub fn rank(card: u8) -> u8 {
    card % 13
}

fn other(seat: u8) -> u8 {
    1 - seat
}

/// The seat that must play now, if the round is still open.
fn to_play(state: &State) -> u8 {
    if state.table[state.leader as usize].is_none() {
        state.leader
    } else {
        other(state.leader)
    }
}

/// The match is over when someone forfeited, the rounds are used up, or a
/// seat has nothing left to play. The empty-hand check only counts
/// **between rounds** (table clear): the leader's hand may legitimately be
/// empty while its last card sits on the table waiting for the reply, and
/// that round must still be played out and scored.
fn finished(state: &State) -> bool {
    if state.forfeited.is_some() || state.rounds_left == 0 {
        return true;
    }
    let between_rounds = state.table.iter().all(|c| c.is_none());
    between_rounds && state.hands.iter().any(|h| h.is_empty())
}

impl Rules for HighCard {
    const NAME: &'static str = "highcard";
    const VERSION: &'static str = "1.0.0";
    const MIN_SEATS: u8 = 2;
    const MAX_SEATS: u8 = 2;

    type Config = Config;
    type State = State;
    type Move = Move;
    type View = View;
    type Event = Event;

    fn init(config: &Config, _seats: &[SeatInfo], rng: &mut Rng) -> Result<State, Reject> {
        if config.hand_size == 0 || config.hand_size > 10 {
            return Err(Reject::new(Reject::BAD_CONFIG, "hand_size must be 1..=10"));
        }
        if config.rounds == 0 || config.rounds > 26 {
            return Err(Reject::new(Reject::BAD_CONFIG, "rounds must be 1..=26"));
        }
        let mut deck: Vec<u8> = (0..52).collect();
        rng.shuffle(&mut deck);
        let mut hands = [Vec::new(), Vec::new()];
        for _ in 0..config.hand_size {
            for hand in hands.iter_mut() {
                // 52 cards, at most 20 dealt: pop cannot fail.
                if let Some(card) = deck.pop() {
                    hand.push(card);
                }
            }
        }
        Ok(State {
            deck,
            hands,
            table: [None, None],
            scores: [0, 0],
            leader: 0,
            rounds_left: config.rounds,
            forfeited: None,
        })
    }

    fn apply(state: &mut State, seat: Seat, mv: Move, _rng: &mut Rng) -> Result<Vec<Scoped<Event>>, Reject> {
        let me = seat.0;
        let card = match mv {
            Move::Resign => {
                state.forfeited = Some(me);
                return Ok(vec![Scoped::all(Event::Forfeited { seat: me })]);
            }
            Move::Play { card } => card,
        };

        let hand = &mut state.hands[me as usize];
        let Some(pos) = hand.iter().position(|c| *c == card) else {
            return Err(Reject::illegal(format!("card {card} is not in your hand")));
        };
        hand.remove(pos);
        state.table[me as usize] = Some(card);
        let mut events = vec![Scoped::all(Event::Played { seat: me, card })];

        // Round complete?
        if let [Some(a), Some(b)] = state.table {
            let winner = match rank(a).cmp(&rank(b)) {
                core::cmp::Ordering::Greater => Some(0u8),
                core::cmp::Ordering::Less => Some(1u8),
                core::cmp::Ordering::Equal => None,
            };
            if let Some(w) = winner {
                state.scores[w as usize] += 1;
            }
            events.push(Scoped::all(Event::RoundScored { winner, scores: state.scores }));

            state.table = [None, None];
            state.rounds_left -= 1;
            // Leader draws first. The card is the drawer's secret.
            for s in [state.leader, other(state.leader)] {
                if let Some(drawn) = state.deck.pop() {
                    state.hands[s as usize].push(drawn);
                    events.push(Scoped::to(Seat(s), Event::Drew { card: drawn }));
                    events.push(Scoped::to(Seat(other(s)), Event::OpponentDrew { seat: s }));
                }
            }
            state.leader = other(state.leader);
        }
        Ok(events)
    }

    fn to_move(state: &State) -> Turn {
        if finished(state) {
            Turn::Over
        } else {
            Turn::Seat(Seat(to_play(state)))
        }
    }

    fn view(state: &State, seat: Seat) -> View {
        let you = if seat.is_spectator() || seat.0 > 1 { None } else { Some(seat.0) };
        View {
            you,
            hand: you.map(|s| state.hands[s as usize].clone()).unwrap_or_default(),
            hand_sizes: [state.hands[0].len() as u8, state.hands[1].len() as u8],
            table: state.table,
            scores: state.scores,
            deck_count: state.deck.len() as u8,
            rounds_left: state.rounds_left,
            to_play: if finished(state) { None } else { Some(to_play(state)) },
        }
    }

    fn outcome(state: &State) -> Option<Outcome> {
        if let Some(loser) = state.forfeited {
            return Some(Outcome::win(Seat(other(loser)), "forfeit"));
        }
        if !finished(state) {
            return None;
        }
        Some(match state.scores[0].cmp(&state.scores[1]) {
            core::cmp::Ordering::Greater => Outcome::win(Seat(0), "score"),
            core::cmp::Ordering::Less => Outcome::win(Seat(1), "score"),
            core::cmp::Ordering::Equal => Outcome::draw("score"),
        })
    }

    fn on_timeout(state: &mut State, seat: Seat, _rng: &mut Rng) -> Vec<Scoped<Event>> {
        state.forfeited = Some(seat.0);
        vec![Scoped::all(Event::Forfeited { seat: seat.0 })]
    }
}

impl LegalMoves for HighCard {
    fn legal_moves(view: &View, seat: Seat) -> Vec<Move> {
        if view.to_play != Some(seat.0) || view.you != Some(seat.0) {
            return Vec::new();
        }
        view.hand.iter().map(|c| Move::Play { card: *c }).collect()
    }
}

biome_rules::export!(HighCard);

#[cfg(test)]
mod tests {
    use super::*;
    use biome_rules::engine::events_for;
    use biome_rules::{codec, sim, Engine};

    fn game(seed: u8) -> Engine<HighCard> {
        Engine::new(&Config::default(), &sim::seats(2), [seed; 32]).unwrap()
    }

    #[test]
    fn deal_is_a_partition_of_the_deck_and_depends_on_the_seed() {
        let a = game(1);
        let b = game(2);
        let s = a.state();
        let mut all: Vec<u8> = s.deck.iter().chain(s.hands[0].iter()).chain(s.hands[1].iter()).copied().collect();
        all.sort_unstable();
        assert_eq!(all, (0..52).collect::<Vec<u8>>());
        assert_eq!(s.hands[0].len(), 3);
        assert_eq!(s.deck.len(), 46);
        assert_ne!(a.state().hands, b.state().hands, "different seeds, different deals");
        assert_eq!(game(1).state_bytes().unwrap(), a.state_bytes().unwrap(), "same seed, same deal");
    }

    #[test]
    fn a_view_never_contains_the_other_hand_or_the_deck() {
        let e = game(7);
        let s = e.state();
        for seat in [0u8, 1] {
            let v = e.view(Seat(seat));
            assert_eq!(v.hand, s.hands[seat as usize]);
            let bytes = codec::encode(&v).unwrap();
            // The opponent's cards must not be recoverable from the bytes:
            // re-decode and check structurally (a byte scan would trip over
            // small numbers like scores).
            let back: View = codec::decode(&bytes).unwrap();
            for hidden in &s.hands[1 - seat as usize] {
                assert!(!back.hand.contains(hidden));
            }
            assert_eq!(back.hand_sizes, [3, 3]);
            assert_eq!(back.deck_count, 46);
        }
        let spectator = e.view(Seat::SPECTATOR);
        assert!(spectator.hand.is_empty());
        assert_eq!(spectator.you, None);
    }

    #[test]
    fn a_round_scores_and_draws_are_private() {
        let mut e = game(3);
        let c0 = e.state().hands[0][0];
        let c1 = e.state().hands[1][0];
        let ev = e.apply(Seat(0), Move::Play { card: c0 }, [0; 32]).unwrap();
        assert_eq!(ev, vec![Scoped::all(Event::Played { seat: 0, card: c0 })]);
        assert_eq!(e.to_move(), Turn::Seat(Seat(1)));

        let ev = e.apply(Seat(1), Move::Play { card: c1 }, [0; 32]).unwrap();
        let expected_winner = match rank(c0).cmp(&rank(c1)) {
            core::cmp::Ordering::Greater => Some(0),
            core::cmp::Ordering::Less => Some(1),
            core::cmp::Ordering::Equal => None,
        };
        assert!(ev.contains(&Scoped::all(Event::RoundScored { winner: expected_winner, scores: e.state().scores })));

        // Seat 0 sees its own draw and only "opponent drew" for seat 1.
        let for0 = events_for(&ev, Seat(0));
        let for1 = events_for(&ev, Seat(1));
        let spect = events_for(&ev, Seat::SPECTATOR);
        let drew0 = for0.iter().filter(|e| matches!(e, Event::Drew { .. })).count();
        let drew1 = for1.iter().filter(|e| matches!(e, Event::Drew { .. })).count();
        assert_eq!((drew0, drew1), (1, 1));
        assert!(for0.contains(&Event::OpponentDrew { seat: 1 }));
        assert!(for1.contains(&Event::OpponentDrew { seat: 0 }));
        assert!(spect.iter().all(|e| !matches!(e, Event::Drew { .. } | Event::OpponentDrew { .. })));
        // The card seat 0 drew is in seat 0's hand and was not told to seat 1.
        let Some(Event::Drew { card }) = for0.iter().find(|e| matches!(e, Event::Drew { .. })) else { unreachable!() };
        assert!(e.state().hands[0].contains(card));
        assert!(!for1.contains(&Event::Drew { card: *card }));

        assert_eq!(e.state().leader, 1, "lead passes");
        assert_eq!(e.state().rounds_left, 4);
        assert_eq!(e.state().hands[0].len(), 3);
    }

    #[test]
    fn playing_a_card_you_do_not_hold_is_illegal_and_changes_nothing() {
        let mut e = game(4);
        let theirs = e.state().hands[1][0];
        let before = e.state_bytes().unwrap();
        assert_eq!(e.apply(Seat(0), Move::Play { card: theirs }, [0; 32]).unwrap_err().code, Reject::ILLEGAL);
        assert_eq!(e.apply(Seat(1), Move::Play { card: theirs }, [0; 32]).unwrap_err().code, Reject::NOT_YOUR_TURN);
        assert_eq!(e.state_bytes().unwrap(), before);
    }

    #[test]
    fn bad_config_is_refused() {
        for cfg in [Config { hand_size: 0, rounds: 5 }, Config { hand_size: 11, rounds: 5 }, Config { hand_size: 3, rounds: 0 }] {
            assert_eq!(Engine::<HighCard>::new(&cfg, &sim::seats(2), [0; 32]).err().unwrap().code, Reject::BAD_CONFIG);
        }
    }

    #[test]
    fn full_random_matches_finish_with_consistent_scores() {
        for i in 0..100u8 {
            let r = sim::play_random::<HighCard>(&Config::default(), 2, [i; 32], 100).unwrap();
            let outcome = r.outcome.expect("finished");
            assert_eq!(r.log.len(), 10, "5 rounds, 2 plays each");
            let last: State = codec::decode(r.states.last().unwrap()).unwrap();
            assert!(last.scores[0] + last.scores[1] <= 5);
            match last.scores[0].cmp(&last.scores[1]) {
                core::cmp::Ordering::Greater => assert_eq!(outcome, Outcome::win(Seat(0), "score")),
                core::cmp::Ordering::Less => assert_eq!(outcome, Outcome::win(Seat(1), "score")),
                core::cmp::Ordering::Equal => assert!(outcome.draw),
            }
        }
    }

    /// Regression: `finished` used to fire as soon as any hand was empty,
    /// including mid-round, so a one-card hand ended the match after a single
    /// play with a card stranded on the table.
    #[test]
    fn a_round_in_progress_is_always_played_out_and_scored() {
        // hand_size 1: every round empties the leader's hand before the reply.
        for seed in 0..40u8 {
            let cfg = Config { hand_size: 1, rounds: 4 };
            let r = sim::play_random::<HighCard>(&cfg, 2, [seed; 32], 200).unwrap();
            let last: State = codec::decode(r.states.last().unwrap()).unwrap();
            assert_eq!(r.log.len(), 8, "4 rounds, 2 plays each (seed {seed})");
            assert_eq!(last.table, [None, None], "no card may be left on the table");
            assert_eq!(last.rounds_left, 0);
            assert!(r.outcome.is_some());
        }

        // Deck exhaustion: 3-card hands, 26 rounds. 46 cards in the deck feed
        // 23 rounds of draws; hands then run down and the match ends between
        // rounds with every started round scored.
        for seed in 0..10u8 {
            let cfg = Config { hand_size: 3, rounds: 26 };
            let r = sim::play_random::<HighCard>(&cfg, 2, [seed; 32], 200).unwrap();
            let last: State = codec::decode(r.states.last().unwrap()).unwrap();
            assert_eq!(r.log.len() % 2, 0, "plays come in pairs (seed {seed})");
            assert_eq!(last.table, [None, None]);
            assert_eq!(r.log.len(), 52, "all 52 cards get played: 26 rounds");
            assert_eq!(last.rounds_left, 0);
            assert!(last.hands.iter().all(|h| h.is_empty()));
        }
    }

    #[test]
    fn the_reply_is_still_owed_when_the_leaders_hand_is_empty() {
        let mut e = Engine::<HighCard>::new(&Config { hand_size: 1, rounds: 2 }, &sim::seats(2), [5; 32]).unwrap();
        let c0 = e.state().hands[0][0];
        e.apply(Seat(0), Move::Play { card: c0 }, [0; 32]).unwrap();
        assert!(e.state().hands[0].is_empty());
        assert_eq!(e.outcome(), None, "mid-round is not the end");
        assert_eq!(e.to_move(), Turn::Seat(Seat(1)));
        assert_eq!(e.view(Seat(1)).to_play, Some(1));
        let c1 = e.state().hands[1][0];
        e.apply(Seat(1), Move::Play { card: c1 }, [0; 32]).unwrap();
        assert_eq!(e.state().rounds_left, 1);
        assert_eq!(e.state().table, [None, None]);
    }

    #[test]
    fn same_master_seed_replays_identically() {
        let a = sim::play_random::<HighCard>(&Config::default(), 2, [9; 32], 100).unwrap();
        let b = sim::play_random::<HighCard>(&Config::default(), 2, [9; 32], 100).unwrap();
        assert_eq!(a.states, b.states);
        assert_eq!(a.log, b.log);
    }
}
