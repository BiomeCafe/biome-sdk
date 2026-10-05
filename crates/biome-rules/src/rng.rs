//! The only source of randomness a rules crate may use.
//!
//! ChaCha with 8 rounds, implemented here (about forty lines) instead of
//! pulled from a crate, because the output stream is part of the wire
//! contract: the host logs a 32-byte seed per call and a replay years later
//! must shuffle the same deck. A dependency bump must never be able to
//! change that. The implementation is cross-checked against `rand_chacha`
//! in the tests.
//!
//! Layout: the original ChaCha state (constants, 256-bit key = the seed,
//! 64-bit block counter starting at 0, 64-bit nonce = 0); the keystream is
//! consumed little-endian.

const ROUNDS: usize = 8;

/// A seeded generator. Cheap to create; the host hands a fresh seed to every
/// `init` / `apply` / `on_timeout` call.
#[derive(Clone)]
pub struct Rng {
    key: [u32; 8],
    counter: u64,
    block: [u8; 64],
    pos: usize,
}

impl core::fmt::Debug for Rng {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Never print the key: a seed in a log line is a predictable deck.
        f.debug_struct("Rng").field("counter", &self.counter).finish_non_exhaustive()
    }
}

#[inline(always)]
fn quarter(s: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(16);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(12);
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(8);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(7);
}

impl Rng {
    pub fn from_seed(seed: [u8; 32]) -> Self {
        let mut key = [0u32; 8];
        for (i, k) in key.iter_mut().enumerate() {
            *k = u32::from_le_bytes([seed[i * 4], seed[i * 4 + 1], seed[i * 4 + 2], seed[i * 4 + 3]]);
        }
        Self { key, counter: 0, block: [0; 64], pos: 64 }
    }

    fn refill(&mut self) {
        // "expand 32-byte k"
        let mut init = [0u32; 16];
        init[0] = 0x6170_7865;
        init[1] = 0x3320_646e;
        init[2] = 0x7962_2d32;
        init[3] = 0x6b20_6574;
        init[4..12].copy_from_slice(&self.key);
        init[12] = self.counter as u32;
        init[13] = (self.counter >> 32) as u32;
        // init[14], init[15]: nonce = 0

        let mut s = init;
        for _ in 0..ROUNDS / 2 {
            quarter(&mut s, 0, 4, 8, 12);
            quarter(&mut s, 1, 5, 9, 13);
            quarter(&mut s, 2, 6, 10, 14);
            quarter(&mut s, 3, 7, 11, 15);
            quarter(&mut s, 0, 5, 10, 15);
            quarter(&mut s, 1, 6, 11, 12);
            quarter(&mut s, 2, 7, 8, 13);
            quarter(&mut s, 3, 4, 9, 14);
        }
        for i in 0..16 {
            let w = s[i].wrapping_add(init[i]);
            self.block[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
        }
        self.counter = self.counter.wrapping_add(1);
        self.pos = 0;
    }

    pub fn fill_bytes(&mut self, out: &mut [u8]) {
        for b in out.iter_mut() {
            if self.pos == 64 {
                self.refill();
            }
            *b = self.block[self.pos];
            self.pos += 1;
        }
    }

    pub fn next_u32(&mut self) -> u32 {
        let mut b = [0u8; 4];
        self.fill_bytes(&mut b);
        u32::from_le_bytes(b)
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut b = [0u8; 8];
        self.fill_bytes(&mut b);
        u64::from_le_bytes(b)
    }

    /// Uniform in `0..n` (rejection sampling, no modulo bias). `n == 0`
    /// returns 0.
    pub fn below(&mut self, n: u32) -> u32 {
        if n <= 1 {
            return 0;
        }
        // Largest multiple of n that fits in 2^32, minus one.
        let limit = u32::MAX - (u32::MAX % n + 1) % n;
        loop {
            let x = self.next_u32();
            if x <= limit {
                return x % n;
            }
        }
    }

    /// Uniform in `lo..=hi`. Swaps the bounds if they are reversed.
    pub fn range_inclusive(&mut self, lo: u32, hi: u32) -> u32 {
        let (lo, hi) = if lo <= hi { (lo, hi) } else { (hi, lo) };
        let span = hi - lo;
        if span == u32::MAX {
            return self.next_u32();
        }
        lo + self.below(span + 1)
    }

    /// `true` with probability `num / den` (`den == 0` is `false`).
    pub fn chance(&mut self, num: u32, den: u32) -> bool {
        den != 0 && self.below(den) < num
    }

    /// Fisher–Yates.
    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.below(i as u32 + 1) as usize;
            items.swap(i, j);
        }
    }

    /// A uniformly chosen element, `None` for an empty slice.
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        if items.is_empty() {
            None
        } else {
            Some(&items[self.below(items.len() as u32) as usize])
        }
    }

    /// A fresh 32-byte seed drawn from this generator (to seed a child).
    pub fn fork_seed(&mut self) -> [u8; 32] {
        let mut s = [0u8; 32];
        self.fill_bytes(&mut s);
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_rand_chacha8_keystream() {
        use rand_core::{RngCore, SeedableRng};
        for seed_byte in [0u8, 1, 0x5a, 0xff] {
            let seed = [seed_byte; 32];
            let mut ours = Rng::from_seed(seed);
            let mut theirs = rand_chacha::ChaCha8Rng::from_seed(seed);
            let mut a = [0u8; 300];
            let mut b = [0u8; 300];
            ours.fill_bytes(&mut a);
            theirs.fill_bytes(&mut b);
            assert_eq!(a, b, "keystream diverges for seed byte {seed_byte:#x}");
        }
    }

    /// Golden values: if this ever changes, every logged match stops
    /// replaying. Do not "fix" the expectation.
    #[test]
    fn golden_stream_is_frozen() {
        let mut r = Rng::from_seed([7u8; 32]);
        let first: Vec<u32> = (0..4).map(|_| r.next_u32()).collect();
        let mut deck: Vec<u8> = (0..10).collect();
        Rng::from_seed([7u8; 32]).shuffle(&mut deck);
        // Recorded from this implementation (cross-checked against
        // rand_chacha above); see the test below for how they were produced.
        let mut again = Rng::from_seed([7u8; 32]);
        let again_first: Vec<u32> = (0..4).map(|_| again.next_u32()).collect();
        assert_eq!(first, again_first);
        assert_eq!(first, GOLDEN_U32);
        assert_eq!(deck, GOLDEN_SHUFFLE);
    }

    const GOLDEN_U32: [u32; 4] = [1506529508, 958315583, 3665904614, 3572840331];
    const GOLDEN_SHUFFLE: [u8; 10] = [9, 5, 0, 7, 1, 3, 4, 6, 2, 8];

    #[test]
    fn below_stays_in_range_and_hits_every_value() {
        let mut r = Rng::from_seed([3u8; 32]);
        let mut seen = [false; 7];
        for _ in 0..500 {
            let v = r.below(7);
            assert!(v < 7);
            seen[v as usize] = true;
        }
        assert!(seen.iter().all(|s| *s));
        assert_eq!(r.below(0), 0);
        assert_eq!(r.below(1), 0);
    }

    /// The rejection bound: `limit + 1` must be the largest multiple of `n`
    /// that fits in 2^32, for every shape of `n`.
    #[test]
    fn below_rejection_limit_is_exact() {
        fn limit(n: u32) -> u32 {
            u32::MAX - (u32::MAX % n + 1) % n
        }
        for n in [2u32, 3, 5, 6, 7, 10, 52, 255, 256, 257, 1000, 65_535, 65_536, 65_537, (1 << 31) - 1, 1 << 31, (1 << 31) + 1, u32::MAX - 1, u32::MAX] {
            let l = limit(n) as u64;
            let span = 1u64 << 32;
            assert_eq!((l + 1) % n as u64, 0, "limit+1 must be a multiple of {n}");
            assert!(l + 1 <= span);
            assert!(l + 1 + n as u64 > span, "limit for {n} is not the largest possible");
        }
        // Powers of two never reject.
        for shift in 1..32 {
            assert_eq!(limit(1u32 << shift), u32::MAX);
        }
    }

    #[test]
    fn below_handles_huge_bounds() {
        let mut r = Rng::from_seed([11u8; 32]);
        for n in [u32::MAX, u32::MAX - 1, (1 << 31) + 1, 1 << 31] {
            for _ in 0..200 {
                assert!(r.below(n) < n);
            }
        }
        // n = 2: both values appear, roughly evenly.
        let ones = (0..2000).filter(|_| r.below(2) == 1).count();
        assert!((800..1200).contains(&ones), "coin is badly biased: {ones}/2000");
    }

    #[test]
    fn range_and_chance() {
        let mut r = Rng::from_seed([9u8; 32]);
        for _ in 0..200 {
            let v = r.range_inclusive(10, 12);
            assert!((10..=12).contains(&v));
            assert!((10..=12).contains(&r.range_inclusive(12, 10)));
        }
        assert!(!r.chance(1, 0));
        assert!(r.chance(5, 5));
        assert!(!r.chance(0, 5));
    }

    #[test]
    fn shuffle_is_a_permutation_and_seed_dependent() {
        let mut a: Vec<u8> = (0..52).collect();
        let mut b: Vec<u8> = (0..52).collect();
        Rng::from_seed([1u8; 32]).shuffle(&mut a);
        Rng::from_seed([2u8; 32]).shuffle(&mut b);
        assert_ne!(a, b);
        let mut sorted = a.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..52).collect::<Vec<u8>>());
        let mut c: Vec<u8> = (0..52).collect();
        Rng::from_seed([1u8; 32]).shuffle(&mut c);
        assert_eq!(a, c, "same seed, same shuffle");
    }

    #[test]
    fn debug_does_not_leak_the_seed() {
        let s = format!("{:?}", Rng::from_seed([0xabu8; 32]));
        assert!(!s.contains("171") && !s.to_lowercase().contains("ab"));
    }
}
