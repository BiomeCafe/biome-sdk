//! Byte layouts that cross the WebAssembly boundary. Mirrored by
//! `cloudflare/biome-match/src/wire.ts`; change both or neither.
//!
//! Every ABI call returns a **result buffer**:
//!
//! ```text
//! [tag: u8] [payload …]        tag 0 = ok, 1 = reject
//! reject payload               JSON  {"code": "...", "message": "..."}
//! ```
//!
//! Ok payloads by call:
//!
//! ```text
//! init                         state bytes
//! apply / timeout              [state_len: u32][state][n_events: u32] event*
//!   event                      [audience][len: u32][event bytes]
//!   audience                   [kind: u8]  0 = all
//!                              [kind: u8 = 1][n: u8][seat: u8]*n
//! to_move                      [n: u8][seat: u8]*n        (n = 0 → over)
//! view                         view bytes
//! outcome                      [0]  or  [1] JSON Outcome
//! meta                         JSON Meta (no tag: br_meta cannot fail)
//! ```
//!
//! All integers little-endian.

use crate::types::{Audience, Outcome, Reject, Seat, Turn};

pub const TAG_OK: u8 = 0;
pub const TAG_REJECT: u8 = 1;

/// Smallest possible encoded event: `[audience kind u8][len u32]`.
pub const MIN_EVENT_BYTES: usize = 5;
/// Upper bound on the up-front reservation when decoding events.
pub const MAX_EVENT_PREALLOC: usize = 256;

/// The ok payload of `apply` / `timeout`, decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyOk {
    pub state: Vec<u8>,
    /// `(who may see it, opaque event bytes)`
    pub events: Vec<(Audience, Vec<u8>)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WireError(pub String);

impl core::fmt::Display for WireError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "wire: {}", self.0)
    }
}

impl std::error::Error for WireError {}

fn err<T>(msg: &str) -> Result<T, WireError> {
    Err(WireError(msg.to_string()))
}

// ── result framing ──────────────────────────────────────────────────────────

pub fn ok(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 1);
    out.push(TAG_OK);
    out.extend_from_slice(payload);
    out
}

pub fn reject(r: &Reject) -> Vec<u8> {
    let mut out = vec![TAG_REJECT];
    // A Reject is two strings; serialization cannot fail.
    out.extend_from_slice(serde_json::to_string(r).unwrap_or_else(|_| r#"{"code":"internal","message":""}"#.into()).as_bytes());
    out
}

/// Split a result buffer into its ok payload or the reject it carries.
pub fn split_result(buf: &[u8]) -> Result<Result<&[u8], Reject>, WireError> {
    match buf.first() {
        Some(&TAG_OK) => Ok(Ok(&buf[1..])),
        Some(&TAG_REJECT) => serde_json::from_slice::<Reject>(&buf[1..])
            .map(Err)
            .map_err(|e| WireError(format!("reject json: {e}"))),
        Some(t) => err(&format!("unknown result tag {t}")),
        None => err("empty result"),
    }
}

// ── audience ────────────────────────────────────────────────────────────────

fn put_audience(out: &mut Vec<u8>, a: &Audience) {
    match a {
        Audience::All => out.push(0),
        Audience::Seat(s) => out.extend_from_slice(&[1, 1, s.0]),
        Audience::Seats(v) => {
            out.push(1);
            out.push(v.len().min(255) as u8);
            out.extend(v.iter().take(255).map(|s| s.0));
        }
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn u8(&mut self) -> Result<u8, WireError> {
        let b = *self.buf.get(self.pos).ok_or_else(|| WireError("truncated".into()))?;
        self.pos += 1;
        Ok(b)
    }

    fn u32(&mut self) -> Result<u32, WireError> {
        let s = self.take(4)?;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], WireError> {
        let end = self.pos.checked_add(n).ok_or_else(|| WireError("length overflow".into()))?;
        let s = self.buf.get(self.pos..end).ok_or_else(|| WireError("truncated".into()))?;
        self.pos = end;
        Ok(s)
    }

    fn done(&self) -> bool {
        self.pos == self.buf.len()
    }
}

fn get_audience(r: &mut Reader<'_>) -> Result<Audience, WireError> {
    match r.u8()? {
        0 => Ok(Audience::All),
        1 => {
            let n = r.u8()? as usize;
            let seats: Vec<Seat> = r.take(n)?.iter().map(|b| Seat(*b)).collect();
            Ok(if seats.len() == 1 { Audience::Seat(seats[0]) } else { Audience::Seats(seats) })
        }
        k => err(&format!("unknown audience kind {k}")),
    }
}

// ── apply / timeout ─────────────────────────────────────────────────────────

pub fn encode_apply_ok(state: &[u8], events: &[(Audience, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(state.len() + 16);
    out.extend_from_slice(&(state.len() as u32).to_le_bytes());
    out.extend_from_slice(state);
    out.extend_from_slice(&(events.len() as u32).to_le_bytes());
    for (aud, bytes) in events {
        put_audience(&mut out, aud);
        out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(bytes);
    }
    out
}

pub fn decode_apply_ok(payload: &[u8]) -> Result<ApplyOk, WireError> {
    let mut r = Reader { buf: payload, pos: 0 };
    let state_len = r.u32()? as usize;
    let state = r.take(state_len)?.to_vec();
    let n = r.u32()? as usize;
    // An event is at least MIN_EVENT_BYTES (audience kind + u32 length), so
    // the bytes left bound how many there can be. Refuse an impossible count
    // BEFORE allocating: a 4 MB buffer claiming millions of events would
    // otherwise reserve hundreds of megabytes and then fail.
    let remaining = payload.len() - r.pos;
    if n > remaining / MIN_EVENT_BYTES {
        return err("event count exceeds buffer");
    }
    // And never trust the count for the reservation itself.
    let mut events = Vec::with_capacity(n.min(MAX_EVENT_PREALLOC));
    for _ in 0..n {
        let aud = get_audience(&mut r)?;
        let len = r.u32()? as usize;
        events.push((aud, r.take(len)?.to_vec()));
    }
    if !r.done() {
        return err("trailing bytes after events");
    }
    Ok(ApplyOk { state, events })
}

// ── to_move ─────────────────────────────────────────────────────────────────

pub fn encode_turn(turn: &Turn) -> Vec<u8> {
    let seats = turn.seats();
    let mut out = Vec::with_capacity(seats.len() + 1);
    out.push(seats.len().min(255) as u8);
    out.extend(seats.iter().take(255).map(|s| s.0));
    out
}

pub fn decode_turn(payload: &[u8]) -> Result<Turn, WireError> {
    let mut r = Reader { buf: payload, pos: 0 };
    let n = r.u8()? as usize;
    let seats: Vec<Seat> = r.take(n)?.iter().map(|b| Seat(*b)).collect();
    if !r.done() {
        return err("trailing bytes after turn");
    }
    Ok(match seats.len() {
        0 => Turn::Over,
        1 => Turn::Seat(seats[0]),
        _ => Turn::Seats(seats),
    })
}

// ── outcome ─────────────────────────────────────────────────────────────────

pub fn encode_outcome(outcome: &Option<Outcome>) -> Vec<u8> {
    match outcome {
        None => vec![0],
        Some(o) => {
            let mut out = vec![1];
            out.extend_from_slice(serde_json::to_string(o).unwrap_or_default().as_bytes());
            out
        }
    }
}

pub fn decode_outcome(payload: &[u8]) -> Result<Option<Outcome>, WireError> {
    match payload.first() {
        Some(0) if payload.len() == 1 => Ok(None),
        Some(1) => serde_json::from_slice(&payload[1..])
            .map(Some)
            .map_err(|e| WireError(format!("outcome json: {e}"))),
        _ => err("bad outcome payload"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_framing() {
        assert_eq!(split_result(&ok(b"abc")).unwrap(), Ok(&b"abc"[..]));
        let r = Reject::illegal("nope");
        assert_eq!(split_result(&reject(&r)).unwrap(), Err(r));
        assert!(split_result(&[]).is_err());
        assert!(split_result(&[9, 1, 2]).is_err());
        assert!(split_result(&[TAG_REJECT, b'x']).is_err());
    }

    #[test]
    fn apply_ok_roundtrip_with_every_audience() {
        let events = vec![
            (Audience::All, b"public".to_vec()),
            (Audience::Seat(Seat(1)), b"secret".to_vec()),
            (Audience::Seats(vec![Seat(0), Seat(2)]), Vec::new()),
        ];
        let bytes = encode_apply_ok(b"state!", &events);
        let back = decode_apply_ok(&bytes).unwrap();
        assert_eq!(back.state, b"state!");
        assert_eq!(back.events, events);
    }

    #[test]
    fn apply_ok_rejects_truncation_and_trailing_bytes() {
        let bytes = encode_apply_ok(b"s", &[(Audience::All, b"e".to_vec())]);
        for cut in 0..bytes.len() {
            assert!(decode_apply_ok(&bytes[..cut]).is_err(), "cut at {cut} should fail");
        }
        let mut extra = bytes.clone();
        extra.push(0);
        assert!(decode_apply_ok(&extra).is_err());
        // A huge declared event count must not allocate or loop.
        let mut bomb = Vec::new();
        bomb.extend_from_slice(&0u32.to_le_bytes());
        bomb.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode_apply_ok(&bomb).is_err());
    }

    #[test]
    fn event_count_is_bounded_by_the_bytes_that_follow() {
        // 4 MB of padding after a count that would "fit" by total length but
        // not by bytes-per-event: must be refused without a big reservation.
        let pad = 4 * 1024 * 1024;
        let mut buf = Vec::with_capacity(pad + 8);
        buf.extend_from_slice(&0u32.to_le_bytes()); // empty state
        let claimed = (pad / MIN_EVENT_BYTES + 1) as u32; // one more than can exist
        buf.extend_from_slice(&claimed.to_le_bytes());
        buf.resize(8 + pad, 0);
        assert_eq!(decode_apply_ok(&buf), Err(WireError("event count exceeds buffer".into())));

        // The largest count that can exist is accepted as a count (and then
        // decoded for real): pad/5 events of [audience=all][len=0].
        let n = 1000usize;
        let mut ok = Vec::new();
        ok.extend_from_slice(&0u32.to_le_bytes());
        ok.extend_from_slice(&(n as u32).to_le_bytes());
        for _ in 0..n {
            ok.push(0);
            ok.extend_from_slice(&0u32.to_le_bytes());
        }
        assert_eq!(decode_apply_ok(&ok).unwrap().events.len(), n);
    }

    #[test]
    fn an_empty_seat_list_is_over_on_the_wire() {
        // `Turn::Seats(vec![])` and `Turn::Over` are the same bytes; the
        // decoder always answers `Over`.
        assert_eq!(encode_turn(&Turn::Seats(Vec::new())), encode_turn(&Turn::Over));
        assert_eq!(decode_turn(&encode_turn(&Turn::Seats(Vec::new()))).unwrap(), Turn::Over);
    }

    #[test]
    fn turn_roundtrip() {
        for t in [Turn::Over, Turn::Seat(Seat(1)), Turn::Seats(vec![Seat(0), Seat(1)])] {
            assert_eq!(decode_turn(&encode_turn(&t)).unwrap(), t);
        }
        assert!(decode_turn(&[2, 0]).is_err());
        assert!(decode_turn(&[]).is_err());
    }

    #[test]
    fn outcome_roundtrip() {
        for o in [None, Some(Outcome::win(Seat(0), "x")), Some(Outcome::draw("full"))] {
            assert_eq!(decode_outcome(&encode_outcome(&o)).unwrap(), o);
        }
        assert!(decode_outcome(&[]).is_err());
        assert!(decode_outcome(&[0, 0]).is_err());
        assert!(decode_outcome(&[1, b'{']).is_err());
    }
}
