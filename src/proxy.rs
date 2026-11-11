//! A stand-in Bullet, built from months of Binance history.
//!
//! Bullet has no public history, so for long backtests we imitate it: take a
//! Binance trade tape, and make the Bullet book and trades the profile from
//! `calibrate.rs` says Bullet would have shown:
//!
//! - **Book.** Bullet's mid follows the tape `lag_ms` later, `basis_bps_mean`
//!   away, with the typical spread around it, rounded to Bullet's tick.
//!   Re-sent at least once a second, as Bullet does. The gap isn't fixed:
//!   it wanders as much as we measured (`basis_bps_robust_std`) and drifts back as
//!   fast as we measured (`basis_halflife_secs`). Without that, Bullet would
//!   follow Binance perfectly and the backtest would flatter us.
//! - **Trades.** We keep a random sample of the tape's trades, just enough to
//!   match Bullet's trade rate. Keeping real trades (not made-up ones) keeps
//!   their timing and side: aggressive flow bunches up when the price is
//!   about to move, and that's what picks market makers off. Sizes are drawn
//!   from what we measured on Bullet. Each trade takes Bullet's best price,
//!   but only if that's within what the trader will pay over Binance: a
//!   distance drawn from how far past the *tape* real Bullet trades printed
//!   (`trade_gap_bps_quantiles`). Real traders don't buy on Bullet while it's
//!   dearer than Binance, so when the book has wandered, trades only print
//!   on the side that's back toward Binance. Trades that don't happen are
//!   made up for by keeping a few more of the rest, so the rate still
//!   matches. (Profiles from before that measurement print past Bullet's
//!   mid instead, which overstates fills on wide quotes.)
//!
//! This is a model, so backtests on it are labelled PROXY. It is good for
//! asking "does the pricing logic survive three months of markets?", not
//! "how much would we have made on Bullet?".

use std::collections::VecDeque;

use anyhow::Result;
use rust_decimal::Decimal;
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};

use crate::backtest::Market;
use crate::calibrate::Profile;
use crate::marketdata::{Quote, Trade};

const BPS: f64 = 10_000.0;
const SECOND: u64 = 1_000_000;

pub struct ProxyVenue<I> {
    tape: I,
    tick: Decimal,
    /// Chance of keeping each tape trade as a Bullet trade.
    keep: f64,
    lag_us: u64,
    /// Mean gap to the tape, as a fraction.
    basis: f64,
    /// The wandering part of the gap (a fraction), its size and half-life.
    wander: f64,
    wander_std: f64,
    wander_halflife_us: f64,
    last_tape_us: Option<u64>,
    /// Half the spread, as a fraction of the mid.
    half_spread: Decimal,
    top_size: Decimal,
    sizes: Vec<Decimal>,
    offsets_bps: Vec<f64>,
    gaps_bps: Vec<f64>,
    /// Trades we tried to print, and those that could (not inside the spread).
    tried: u64,
    printed: u64,
    rng: SplitMix64,
    out: VecDeque<Market>,
    book: Option<Quote>,
    next_heartbeat: u64,
}

impl<I: Iterator<Item = Result<Trade>>> ProxyVenue<I> {
    pub fn new(tape: I, profile: &Profile, tick: Decimal, seed: u64) -> Self {
        let dec = |x: f64| Decimal::from_f64(x).unwrap_or_default();
        Self {
            tape,
            tick,
            keep: (profile.trades_per_hour / profile.tape_trades_per_hour.max(1.0)).clamp(0.0, 1.0),
            lag_us: profile.lag_ms * 1000,
            basis: profile.basis_bps_mean / BPS,
            wander: 0.0,
            // The typical wander, not the std a few odd moments inflate
            // (old profiles only have the std).
            wander_std: if profile.basis_bps_robust_std > 0.0 {
                profile.basis_bps_robust_std
            } else {
                profile.basis_bps_std
            } / BPS,
            wander_halflife_us: profile.basis_halflife_secs * 1e6,
            last_tape_us: None,
            half_spread: dec(profile.spread_bps_p50 / 2.0 / BPS),
            top_size: profile.top_size_p50,
            sizes: profile.trade_size_quantiles.clone(),
            offsets_bps: profile.trade_offset_bps_quantiles.clone(),
            gaps_bps: profile.trade_gap_bps_quantiles.clone(),
            tried: 0,
            printed: 0,
            rng: SplitMix64(seed),
            out: VecDeque::new(),
            book: None,
            next_heartbeat: 0,
        }
    }

    fn on_tape(&mut self, trade: Trade) {
        let t = trade.recv_us + self.lag_us;
        // Heartbeats for the quiet seconds before this trade.
        while let Some(book) = self.book
            && self.next_heartbeat <= t
        {
            self.out.push_back(Market::BulletBook(Quote {
                recv_us: self.next_heartbeat,
                exch_us: self.next_heartbeat,
                ..book
            }));
            self.next_heartbeat += SECOND;
        }

        self.drift_gap(trade.recv_us);
        let gap = Decimal::from_f64(self.basis + self.wander).unwrap_or_default();
        let mid = trade.price * (Decimal::ONE + gap);
        let bid = floor_to(mid * (Decimal::ONE - self.half_spread), self.tick);
        let ask = ceil_to(mid * (Decimal::ONE + self.half_spread), self.tick).max(bid + self.tick);
        if self.book.is_none_or(|b| (b.bid, b.ask) != (bid, ask)) {
            let q = Quote {
                recv_us: t,
                exch_us: t,
                bid,
                bid_qty: self.top_size,
                ask,
                ask_qty: self.top_size,
            };
            self.book = Some(q);
            self.out.push_back(Market::BulletBook(q));
            self.next_heartbeat = t + SECOND;
        }

        // Skipped trades are made up for by keeping a few more of the rest.
        let topped_up = self.keep * self.tried.max(1) as f64 / self.printed.max(1) as f64;
        if self.rng.unit() < topped_up.min(1.0) {
            let size = sample_decimal(&self.sizes, self.rng.unit())
                .round_dp(2)
                .max(Decimal::new(1, 2));
            let price = if self.gaps_bps.is_empty() {
                // Old profile: past Bullet's mid, at the touch or beyond it.
                let offset =
                    Decimal::from_f64(sample_f64(&self.offsets_bps, self.rng.unit()) / BPS)
                        .unwrap_or_default();
                if trade.buyer_is_maker {
                    floor_to(mid * (Decimal::ONE - offset), self.tick).min(bid)
                } else {
                    ceil_to(mid * (Decimal::ONE + offset), self.tick).max(ask)
                }
            } else if trade.buyer_is_maker {
                bid
            } else {
                ask
            };
            self.tried += 1;
            let possible = if self.gaps_bps.is_empty() {
                true
            } else {
                // The trader takes Bullet's best price only if it's within
                // what they'll pay over the tape (plus the mean basis, not the
                // wander): a distance drawn from what we measured.
                let anchor = trade.price * Decimal::from_f64(1.0 + self.basis).unwrap_or_default();
                let most = Decimal::from_f64(sample_f64(&self.gaps_bps, self.rng.unit()) / BPS)
                    .unwrap_or_default();
                if trade.buyer_is_maker {
                    price >= anchor * (Decimal::ONE - most)
                } else {
                    price <= anchor * (Decimal::ONE + most)
                }
            };
            if !possible {
                return;
            }
            self.printed += 1;
            self.out.push_back(Market::BulletTrade(Trade {
                recv_us: t,
                exch_us: t,
                price,
                qty: size,
                buyer_is_maker: trade.buyer_is_maker,
            }));
        }
    }
}

impl<I> ProxyVenue<I> {
    /// Move the wandering gap forward to `now`: it fades toward zero with the
    /// measured half-life and picks up fresh noise, keeping its measured size
    /// (an Ornstein-Uhlenbeck process, stepped exactly).
    fn drift_gap(&mut self, now: u64) {
        let Some(last) = self.last_tape_us.replace(now) else {
            return;
        };
        if self.wander_halflife_us <= 0.0 || self.wander_std <= 0.0 {
            return;
        }
        let keep = 0.5f64.powf(now.saturating_sub(last) as f64 / self.wander_halflife_us);
        self.wander =
            self.wander * keep + self.wander_std * (1.0 - keep * keep).sqrt() * self.rng.normal();
    }
}

impl<I: Iterator<Item = Result<Trade>>> Iterator for ProxyVenue<I> {
    type Item = Result<Market>;

    fn next(&mut self) -> Option<Result<Market>> {
        loop {
            if let Some(m) = self.out.pop_front() {
                return Some(Ok(m));
            }
            match self.tape.next()? {
                Ok(trade) => self.on_tape(trade),
                Err(e) => return Some(Err(e)),
            }
        }
    }
}

fn floor_to(x: Decimal, tick: Decimal) -> Decimal {
    (x / tick).floor() * tick
}

fn ceil_to(x: Decimal, tick: Decimal) -> Decimal {
    (x / tick).ceil() * tick
}

/// Draw from a distribution given by evenly spaced quantiles, interpolating.
fn sample_f64(q: &[f64], u: f64) -> f64 {
    if q.is_empty() {
        return 0.0;
    }
    let pos = u * (q.len() - 1) as f64;
    let (i, frac) = (pos.floor() as usize, pos.fract());
    let j = (i + 1).min(q.len() - 1);
    q[i] + (q[j] - q[i]) * frac
}

fn sample_decimal(q: &[Decimal], u: f64) -> Decimal {
    let as_f: Vec<f64> = q.iter().map(|d| d.to_f64().unwrap_or(0.0)).collect();
    Decimal::from_f64(sample_f64(&as_f, u)).unwrap_or_default()
}

/// A tiny, well-known random number generator (SplitMix64). Seeded, so a
/// backtest gives the same answer every time it runs.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in [0, 1).
    fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Standard normal (Box-Muller).
    fn normal(&mut self) -> f64 {
        let u = self.unit().max(f64::MIN_POSITIVE);
        (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * self.unit()).cos()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    fn profile() -> Profile {
        Profile {
            tape: "test".into(),
            hours: 1.0,
            book_updates_per_sec: 1.0,
            spread_bps_p50: 4.0,
            spread_bps_p90: 6.0,
            top_size_p50: d("20"),
            trades_per_hour: 360.0,
            tape_trades_per_hour: 3600.0,
            buy_share: 0.5,
            trade_size_quantiles: vec![d("0.1"), d("1")],
            trade_offset_bps_quantiles: vec![2.0, 2.0],
            basis_bps_mean: 10.0,
            basis_bps_std: 0.0,
            basis_bps_robust_std: 0.0,
            basis_halflife_secs: 30.0,
            lag_ms: 300,
            lag_correlation: 0.9,
            trades_beyond_tape_share: vec![],
            trade_gap_bps_quantiles: vec![],
            basis_beyond_share: vec![],
        }
    }

    /// One hour of tape at 100.00, a trade a second, alternating sides.
    fn tape() -> Vec<Result<Trade>> {
        (0..3600u64)
            .map(|i| {
                let t = 1_000 * SECOND + i * SECOND;
                Ok(Trade {
                    recv_us: t,
                    exch_us: t,
                    price: d("100"),
                    qty: d("5"),
                    buyer_is_maker: i % 2 == 0,
                })
            })
            .collect()
    }

    fn run() -> Vec<Market> {
        ProxyVenue::new(tape().into_iter(), &profile(), d("0.001"), 7)
            .map(Result::unwrap)
            .collect()
    }

    #[test]
    fn the_book_is_the_tape_shifted_by_basis_and_lag() {
        let rows = run();
        let Market::BulletBook(q) = rows[0] else {
            panic!()
        };
        // Mid 100.10 (10 bps over), 2 bps either side.
        assert_eq!((q.bid, q.ask), (d("100.079"), d("100.121")));
        assert_eq!(
            q.recv_us,
            1_000 * SECOND + 300_000,
            "300 ms behind the tape"
        );
        let books = rows
            .iter()
            .filter(|m| matches!(m, Market::BulletBook(_)))
            .count();
        assert!(
            books >= 3600,
            "re-sent every second even when nothing moves: {books}"
        );
    }

    #[test]
    fn trades_match_the_measured_rate_and_print_at_or_past_the_touch() {
        let rows = run();
        let trades: Vec<Trade> = rows
            .iter()
            .filter_map(|m| {
                if let Market::BulletTrade(t) = m {
                    Some(*t)
                } else {
                    None
                }
            })
            .collect();
        assert!(
            (300..420).contains(&trades.len()),
            "about 360 an hour: {}",
            trades.len()
        );
        for t in &trades {
            assert!(t.qty >= d("0.1") && t.qty <= d("1"));
            if t.buyer_is_maker {
                assert!(t.price <= d("100.079"), "sells hit the bid or lower: {t:?}");
            } else {
                assert!(
                    t.price >= d("100.121"),
                    "buys lift the ask or higher: {t:?}"
                );
            }
        }
    }

    #[test]
    fn rows_come_out_in_time_order() {
        let rows = run();
        let times: Vec<u64> = rows
            .iter()
            .map(|m| match m {
                Market::BulletBook(q) => q.recv_us,
                Market::BulletTrade(t) => t.recv_us,
                Market::Reference(q) => q.recv_us,
            })
            .collect();
        assert!(times.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn the_gap_wanders_as_much_as_measured() {
        // The typical wander (2 bps), not the std a few jumps blew up to 10.
        let mut p = profile();
        p.basis_bps_std = 10.0;
        p.basis_bps_robust_std = 2.0;
        let rows: Vec<Market> = ProxyVenue::new(tape().into_iter(), &p, d("0.001"), 3)
            .map(Result::unwrap)
            .collect();
        let gaps: Vec<f64> = rows
            .iter()
            .filter_map(|m| {
                if let Market::BulletBook(q) = m {
                    Some(q)
                } else {
                    None
                }
            })
            .map(|q| (((q.bid + q.ask) / d("2")).to_f64().unwrap() / 100.0 - 1.0) * 1e4)
            .collect();
        let m = gaps.iter().sum::<f64>() / gaps.len() as f64;
        let sd = (gaps.iter().map(|g| (g - m).powi(2)).sum::<f64>() / gaps.len() as f64).sqrt();
        assert!((m - 10.0).abs() < 1.5, "centred on the mean gap: {m}");
        assert!((1.0..3.0).contains(&sd), "wanders about 2 bps: {sd}");
    }

    #[test]
    fn trades_follow_the_tape_not_the_wandering_book() {
        // Real Bullet trades print close to Binance even when Bullet's book has
        // wandered off: nobody buys on Bullet when it's dearer than Binance.
        let mut p = profile();
        p.basis_bps_robust_std = 20.0;
        // Traders take Bullet's best price only when it's within 3 bps of
        // the tape (100.10 with the basis): 100.07 to 100.13.
        p.trade_gap_bps_quantiles = vec![3.0, 3.0];
        let rows: Vec<Market> = ProxyVenue::new(tape().into_iter(), &p, d("0.001"), 5)
            .map(Result::unwrap)
            .collect();
        let mut book = None;
        let mut trades = 0;
        for m in &rows {
            match m {
                Market::BulletBook(q) => book = Some(*q),
                Market::BulletTrade(t) => {
                    trades += 1;
                    let q = book.unwrap();
                    if t.buyer_is_maker {
                        assert_eq!(t.price, q.bid, "sells hit the bid: {t:?}");
                        assert!(t.price >= d("100.069"), "not far below: {t:?}");
                    } else {
                        assert_eq!(t.price, q.ask, "buys lift the ask: {t:?}");
                        assert!(t.price <= d("100.131"), "not far above: {t:?}");
                    }
                }
                Market::Reference(_) => {}
            }
        }
        assert!(
            (300..420).contains(&trades),
            "still about 360 an hour: {trades}"
        );
    }

    #[test]
    fn the_same_seed_gives_the_same_market() {
        assert_eq!(format!("{:?}", run()), format!("{:?}", run()));
    }
}
