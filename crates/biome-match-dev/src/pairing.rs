//! Who plays whom: the same rules as the production host
//! (`cloudflare/biome-match/src/pairing.ts`).
//!
//! Players are only paired within one pool (same game, mode, rules version,
//! config bytes and clock). `ranked` pairs by rating windows that widen with
//! waiting time (±50, +25 per 10 s, at most ±400; the difference must be
//! inside both players' windows; the longest-waiting player is served first
//! and gets the closest rating). Every other mode is first in, first out.

/// How a ranked rating window grows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WindowRule {
    /// Half-width at entry.
    pub start: f64,
    /// Added every `every_ms` waited.
    pub step: f64,
    pub every_ms: i64,
    /// The half-width never grows beyond this.
    pub max: f64,
}

/// The production rule.
pub const RANKED_WINDOW: WindowRule = WindowRule { start: 50.0, step: 25.0, every_ms: 10_000, max: 400.0 };

impl Default for WindowRule {
    fn default() -> Self {
        RANKED_WINDOW
    }
}

pub const DEFAULT_RATING: f64 = 1500.0;

/// "ranked" changes ratings and pairs by rating; anything else is casual.
pub fn is_ranked(mode: &str) -> bool {
    mode == "ranked"
}

/// The half-window of a player who has waited `waited_ms`.
pub fn rating_window(waited_ms: i64, rule: &WindowRule) -> f64 {
    let steps = if rule.every_ms > 0 { waited_ms.max(0) / rule.every_ms } else { 0 };
    (rule.start + rule.step * steps as f64).min(rule.max)
}

/// One waiting player, as pairing sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    pub user_id: String,
    pub pool: String,
    pub joined_at: i64,
    /// Arrival order; breaks ties of `joined_at`.
    pub order: u64,
    /// Ranked only; `None` counts as 1500.
    pub rating: Option<f64>,
}

fn pools(waiting: &[Candidate]) -> Vec<Vec<usize>> {
    let mut keys: Vec<&str> = Vec::new();
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for (i, c) in waiting.iter().enumerate() {
        match keys.iter().position(|k| *k == c.pool) {
            Some(g) => groups[g].push(i),
            None => {
                keys.push(&c.pool);
                groups.push(vec![i]);
            }
        }
    }
    for g in groups.iter_mut() {
        g.sort_by_key(|&i| (waiting[i].joined_at, waiting[i].order));
    }
    groups
}

/// The pairs to make now, as indices into `waiting`.
pub fn pair(mode: &str, waiting: &[Candidate], now: i64, rule: &WindowRule) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for group in pools(waiting) {
        let mut taken = vec![false; group.len()];
        for ai in 0..group.len() {
            if taken[ai] {
                continue;
            }
            let a = &waiting[group[ai]];
            let mut best: Option<(usize, f64)> = None;
            for bi in 0..group.len() {
                if bi == ai || taken[bi] {
                    continue;
                }
                let b = &waiting[group[bi]];
                if b.user_id == a.user_id {
                    continue;
                }
                if !is_ranked(mode) {
                    best = Some((bi, 0.0));
                    break;
                }
                let diff = (a.rating.unwrap_or(DEFAULT_RATING) - b.rating.unwrap_or(DEFAULT_RATING)).abs();
                let wa = rating_window(now - a.joined_at, rule);
                let wb = rating_window(now - b.joined_at, rule);
                if diff > wa || diff > wb {
                    continue;
                }
                // Strictly closer only: on a tie the longer-waiting one stays.
                if best.is_none_or(|(_, d)| diff < d) {
                    best = Some((bi, diff));
                }
            }
            if let Some((bi, _)) = best {
                taken[ai] = true;
                taken[bi] = true;
                out.push((group[ai], group[bi]));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(user: &str, joined_at: i64, rating: Option<f64>, pool: &str, order: u64) -> Candidate {
        Candidate { user_id: user.into(), pool: pool.into(), joined_at, order, rating }
    }

    fn names(waiting: &[Candidate], pairs: &[(usize, usize)]) -> Vec<(String, String)> {
        pairs.iter().map(|(a, b)| (waiting[*a].user_id.clone(), waiting[*b].user_id.clone())).collect()
    }

    #[test]
    fn windows_widen_in_steps_and_stop() {
        let r = RANKED_WINDOW;
        assert_eq!(rating_window(0, &r), 50.0);
        assert_eq!(rating_window(9_999, &r), 50.0);
        assert_eq!(rating_window(10_000, &r), 75.0);
        assert_eq!(rating_window(140_000, &r), 400.0);
        assert_eq!(rating_window(-1, &r), 50.0);
    }

    #[test]
    fn casual_is_fifo_within_pools() {
        let w = vec![c("d", 40, None, "p", 4), c("b", 20, None, "p", 2), c("x", 5, None, "q", 9), c("a", 10, None, "p", 1), c("c", 30, None, "p", 3)];
        assert_eq!(
            names(&w, &pair("casual", &w, 0, &RANKED_WINDOW)),
            vec![("a".to_string(), "b".to_string()), ("c".to_string(), "d".to_string())]
        );
        // A player never meets itself.
        let same = vec![c("a", 1, None, "p", 1), c("a", 2, None, "p", 2)];
        assert!(pair("casual", &same, 0, &RANKED_WINDOW).is_empty());
    }

    #[test]
    fn ranked_needs_both_windows_and_widens() {
        let w = vec![c("a", 0, Some(1500.0), "p", 1), c("b", 0, Some(1600.0), "p", 2)];
        assert!(pair("ranked", &w, 10_000, &RANKED_WINDOW).is_empty());
        assert_eq!(pair("ranked", &w, 20_000, &RANKED_WINDOW), vec![(0, 1)]);
        let late = vec![c("a", 0, Some(1500.0), "p", 1), c("b", 60_000, Some(1600.0), "p", 2)];
        assert!(pair("ranked", &late, 60_000, &RANKED_WINDOW).is_empty());
        assert_eq!(pair("ranked", &late, 80_000, &RANKED_WINDOW), vec![(0, 1)]);
    }

    #[test]
    fn ranked_serves_the_longest_waiting_first_with_the_closest_rating() {
        let w = vec![
            c("far", 50_000, Some(1700.0), "p", 3),
            c("near", 100_000, Some(1510.0), "p", 4),
            c("other", 60_000, Some(1690.0), "p", 2),
            c("old", 0, Some(1500.0), "p", 1),
        ];
        assert_eq!(
            names(&w, &pair("ranked", &w, 300_000, &RANKED_WINDOW)),
            vec![("old".to_string(), "near".to_string()), ("far".to_string(), "other".to_string())]
        );
        let tie = vec![c("a", 0, Some(1500.0), "p", 1), c("later", 20, Some(1540.0), "p", 3), c("earlier", 10, Some(1460.0), "p", 2)];
        assert_eq!(names(&tie, &pair("ranked", &tie, 30, &RANKED_WINDOW)), vec![("a".to_string(), "earlier".to_string())]);
    }

    #[test]
    fn pools_never_mix() {
        let w = vec![c("a", 0, Some(1500.0), "p1", 1), c("b", 0, Some(1500.0), "p2", 2)];
        assert!(pair("ranked", &w, 0, &RANKED_WINDOW).is_empty());
        assert!(pair("casual", &w, 0, &RANKED_WINDOW).is_empty());
    }

    #[test]
    fn a_custom_rule_widens_faster() {
        let fast = WindowRule { start: 50.0, step: 25.0, every_ms: 100, max: 400.0 };
        let w = vec![c("a", 0, Some(1500.0), "p", 1), c("b", 0, Some(1700.0), "p", 2)];
        assert!(pair("ranked", &w, 500, &fast).is_empty());
        assert_eq!(pair("ranked", &w, 600, &fast), vec![(0, 1)]);
    }
}
