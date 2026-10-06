//! Glicko-2 (Glickman, "Example of the Glicko-2 system"), the same math as
//! `glicko2_rate` in `supabase/migrations/20261006090000_matchmaking.sql`.
//! The production host rates ranked matches in the database; the dev host
//! keeps ratings in memory with this.

/// Glicko scale ↔ Glicko-2 scale: 400 / ln 10.
const SCALE: f64 = 173.7178;
const EPSILON: f64 = 0.000_001;
const MAX_ITERATIONS: u32 = 100;
pub const TAU: f64 = 0.5;
pub const MAX_DEVIATION: f64 = 350.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Glicko {
    pub rating: f64,
    pub deviation: f64,
    pub volatility: f64,
}

impl Default for Glicko {
    /// A new player.
    fn default() -> Self {
        Self { rating: 1500.0, deviation: 350.0, volatility: 0.06 }
    }
}

/// One game of a rating period: the opponent's rating and deviation from
/// before the period, and the score (1 win, 0.5 draw, 0 loss).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Game {
    pub rating: f64,
    pub deviation: f64,
    pub score: f64,
}

fn g(phi: f64) -> f64 {
    1.0 / (1.0 + 3.0 * phi * phi / (std::f64::consts::PI * std::f64::consts::PI)).sqrt()
}

/// One rating period. Without games only the deviation widens.
pub fn rate(player: Glicko, games: &[Game], tau: f64) -> Glicko {
    let mu = (player.rating - 1500.0) / SCALE;
    let phi = player.deviation / SCALE;
    let sigma = player.volatility;
    if games.is_empty() {
        return Glicko { deviation: (SCALE * (phi * phi + sigma * sigma).sqrt()).min(MAX_DEVIATION), ..player };
    }

    // Steps 3 and 4.
    let mut v_inv = 0.0;
    let mut d_sum = 0.0;
    for game in games {
        let mu_j = (game.rating - 1500.0) / SCALE;
        let g_j = g(game.deviation / SCALE);
        let e = 1.0 / (1.0 + (-g_j * (mu - mu_j)).exp());
        v_inv += g_j * g_j * e * (1.0 - e);
        d_sum += g_j * (game.score - e);
    }
    let v = 1.0 / v_inv;
    let delta = v * d_sum;

    // Step 5: the new volatility (Illinois algorithm).
    let a = (sigma * sigma).ln();
    let f = |x: f64| {
        let ex = x.exp();
        ex * (delta * delta - phi * phi - v - ex) / (2.0 * (phi * phi + v + ex).powi(2)) - (x - a) / (tau * tau)
    };
    let mut big_a = a;
    let mut big_b = if delta * delta > phi * phi + v {
        (delta * delta - phi * phi - v).ln()
    } else {
        let mut k = 1.0;
        let mut it = 1;
        while f(a - k * tau) < 0.0 && it < MAX_ITERATIONS {
            k += 1.0;
            it += 1;
        }
        a - k * tau
    };
    let mut f_a = f(big_a);
    let mut f_b = f(big_b);
    let mut it = 0;
    while (big_b - big_a).abs() > EPSILON && it < MAX_ITERATIONS {
        let big_c = big_a + (big_a - big_b) * f_a / (f_b - f_a);
        let f_c = f(big_c);
        if f_c * f_b <= 0.0 {
            big_a = big_b;
            f_a = f_b;
        } else {
            f_a /= 2.0;
        }
        big_b = big_c;
        f_b = f_c;
        it += 1;
    }
    let volatility = (big_a / 2.0).exp();

    // Steps 6 to 8.
    let phi_star = (phi * phi + volatility * volatility).sqrt();
    let phi_new = 1.0 / (1.0 / (phi_star * phi_star) + 1.0 / v).sqrt();
    Glicko {
        rating: SCALE * (mu + phi_new * phi_new * d_sum) + 1500.0,
        deviation: (SCALE * phi_new).min(MAX_DEVIATION),
        volatility,
    }
}

/// A two-player match as one rating period for both: `score` is the first
/// player's (1, 0.5 or 0). Each is rated against the other's rating from
/// before the match.
pub fn rate_match(first: Glicko, second: Glicko, score: f64) -> (Glicko, Glicko) {
    (
        rate(first, &[Game { rating: second.rating, deviation: second.deviation, score }], TAU),
        rate(second, &[Game { rating: first.rating, deviation: first.deviation, score: 1.0 - score }], TAU),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_papers_example() {
        let r = rate(
            Glicko { rating: 1500.0, deviation: 200.0, volatility: 0.06 },
            &[
                Game { rating: 1400.0, deviation: 30.0, score: 1.0 },
                Game { rating: 1550.0, deviation: 100.0, score: 0.0 },
                Game { rating: 1700.0, deviation: 300.0, score: 0.0 },
            ],
            TAU,
        );
        assert!((r.rating - 1464.06).abs() < 0.01, "{r:?}");
        assert!((r.deviation - 151.52).abs() < 0.01, "{r:?}");
        assert!((r.volatility - 0.05999).abs() < 0.00001, "{r:?}");
    }

    #[test]
    fn two_new_players_move_symmetrically() {
        let (w, l) = rate_match(Glicko::default(), Glicko::default(), 1.0);
        // The same values the database computes (checked through PostgREST).
        assert!((w.rating - 1662.31).abs() < 0.01, "{w:?}");
        assert!((w.deviation - 290.32).abs() < 0.01, "{w:?}");
        assert!(((w.rating - 1500.0) - (1500.0 - l.rating)).abs() < 1e-6);
        let (a, b) = rate_match(Glicko { rating: 1700.0, ..Glicko::default() }, Glicko::default(), 0.5);
        assert!(a.rating < 1700.0 && b.rating > 1500.0, "a draw costs the favourite: {a:?} {b:?}");
    }

    #[test]
    fn a_period_without_games_only_widens_the_deviation() {
        let r = rate(Glicko { rating: 1600.0, deviation: 100.0, volatility: 0.06 }, &[], TAU);
        assert_eq!(r.rating, 1600.0);
        assert!(r.deviation > 100.0 && r.deviation < 101.0);
        assert_eq!(rate(Glicko { deviation: 349.9, ..Glicko::default() }, &[], TAU).deviation, MAX_DEVIATION);
    }
}
