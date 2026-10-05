//! Types shared by rules crates, the native engine and the match host.
//!
//! [`SeatInfo`], [`Reject`], [`Outcome`] and [`Meta`] cross the WebAssembly
//! boundary as JSON (the TypeScript host reads them directly); keep their
//! field names stable.

use serde::{Deserialize, Serialize};

/// A player's position in a match: `0..n`. [`Seat::SPECTATOR`] asks
/// [`crate::Rules::view`] for the public projection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Seat(pub u8);

impl Seat {
    /// Not a player. Only meaningful as the `seat` argument of `view`.
    pub const SPECTATOR: Seat = Seat(u8::MAX);

    pub const fn index(self) -> usize {
        self.0 as usize
    }

    pub const fn is_spectator(self) -> bool {
        self.0 == u8::MAX
    }
}

/// Who sits where. `user_id` is the Biome user id from the verified ticket.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatInfo {
    pub seat: Seat,
    pub user_id: String,
    pub username: String,
}

/// Whose move it is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Turn {
    /// One seat.
    Seat(Seat),
    /// Several seats may move now (simultaneous choice). The host accepts
    /// one move from each in any order. An empty list means the same as
    /// [`Turn::Over`] and is indistinguishable from it on the wire.
    Seats(Vec<Seat>),
    /// Nobody: the match is decided.
    Over,
}

impl Turn {
    /// The seats that may move, empty when over.
    pub fn seats(&self) -> Vec<Seat> {
        match self {
            Turn::Seat(s) => vec![*s],
            Turn::Seats(v) => v.clone(),
            Turn::Over => Vec::new(),
        }
    }

    pub fn includes(&self, seat: Seat) -> bool {
        match self {
            Turn::Seat(s) => *s == seat,
            Turn::Seats(v) => v.contains(&seat),
            Turn::Over => false,
        }
    }
}

/// How a match ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outcome {
    /// Empty when `draw`.
    pub winners: Vec<Seat>,
    pub draw: bool,
    /// Short machine-ish reason: `"three-in-a-row"`, `"forfeit"`, `"score"`.
    pub reason: String,
}

impl Outcome {
    pub fn win(seat: Seat, reason: &str) -> Self {
        Self { winners: vec![seat], draw: false, reason: reason.to_string() }
    }

    pub fn draw(reason: &str) -> Self {
        Self { winners: Vec::new(), draw: true, reason: reason.to_string() }
    }
}

/// Why a move (or a setup) was refused.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reject {
    /// Stable code for programs. See the constants on this type.
    pub code: String,
    /// For humans and logs.
    pub message: String,
}

impl Reject {
    pub const NOT_YOUR_TURN: &'static str = "not-your-turn";
    pub const GAME_OVER: &'static str = "game-over";
    pub const ILLEGAL: &'static str = "illegal-move";
    pub const BAD_MOVE: &'static str = "bad-move";
    pub const BAD_STATE: &'static str = "bad-state";
    pub const BAD_CONFIG: &'static str = "bad-config";
    pub const BAD_SEATS: &'static str = "bad-seats";

    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self { code: code.to_string(), message: message.into() }
    }

    /// The common case: the move breaks the game's rules.
    pub fn illegal(message: impl Into<String>) -> Self {
        Self::new(Self::ILLEGAL, message)
    }
}

impl core::fmt::Display for Reject {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for Reject {}

/// Who may see an event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Audience {
    All,
    Seat(Seat),
    Seats(Vec<Seat>),
}

impl Audience {
    /// Spectators only ever receive [`Audience::All`] events.
    pub fn includes(&self, seat: Seat) -> bool {
        match self {
            Audience::All => true,
            Audience::Seat(s) => *s == seat && !seat.is_spectator(),
            Audience::Seats(v) => !seat.is_spectator() && v.contains(&seat),
        }
    }
}

/// An event plus who may see it. "You drew the ace" goes to one seat;
/// "opponent drew a card" goes to the other.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scoped<E> {
    pub to: Audience,
    pub event: E,
}

impl<E> Scoped<E> {
    pub fn all(event: E) -> Self {
        Self { to: Audience::All, event }
    }

    pub fn to(seat: Seat, event: E) -> Self {
        Self { to: Audience::Seat(seat), event }
    }

    pub fn to_seats(seats: Vec<Seat>, event: E) -> Self {
        Self { to: Audience::Seats(seats), event }
    }
}

/// What a compiled rules module says about itself (`br_meta`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    pub abi: u32,
    pub name: String,
    pub rules_version: String,
    pub min_seats: u8,
    pub max_seats: u8,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seat_json_is_a_plain_number() {
        let info = SeatInfo { seat: Seat(1), user_id: "u".into(), username: "n".into() };
        assert_eq!(serde_json::to_string(&info).unwrap(), r#"{"seat":1,"user_id":"u","username":"n"}"#);
    }

    #[test]
    fn audience_never_includes_spectators_for_scoped_events() {
        assert!(Audience::All.includes(Seat::SPECTATOR));
        assert!(!Audience::Seat(Seat::SPECTATOR).includes(Seat::SPECTATOR));
        assert!(Audience::Seats(vec![Seat(0), Seat(1)]).includes(Seat(1)));
        assert!(!Audience::Seats(vec![Seat(0)]).includes(Seat(1)));
    }

    #[test]
    fn turn_helpers() {
        assert_eq!(Turn::Seat(Seat(1)).seats(), vec![Seat(1)]);
        assert!(Turn::Seats(vec![Seat(0), Seat(1)]).includes(Seat(0)));
        assert!(!Turn::Over.includes(Seat(0)));
    }
}
