//! Measure how Bullet behaves, from our recordings, so the proxy venue
//! (`proxy.rs`) can imitate it over months of Binance history.
//!
//! We compare Bullet with one Binance trade tape (the "tape") over the hours
//! we recorded both, and write what we learn to a small TOML profile:
//!
//! - how wide Bullet's spread is and how big its best levels are;
//! - how often Bullet trades, how big the trades are, and how far from the
//!   mid they print;
//! - the steady price gap between Bullet and the tape (the basis);
//! - how long Bullet's mid takes to follow the tape (the lag).
//!
//! These are statistics, not money, so plain `f64` is fine here.

use std::path::PathBuf;

use anyhow::{Result, bail};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde::{Deserialize, Serialize};

use crate::marketdata::{CsvReader, Quote, Trade};

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Bullet top-of-book recordings.
    #[arg(long, num_args = 1.., required = true)]
    pub bullet_quotes: Vec<PathBuf>,
    /// Bullet trade recordings.
    #[arg(long, num_args = 1.., required = true)]
    pub bullet_trades: Vec<PathBuf>,
    /// The Binance trade tape to compare with (the one the proxy will be built from).
    #[arg(long, num_args = 1.., required = true)]
    pub tape: Vec<PathBuf>,
    /// A name for the tape, e.g. "spot SOLUSDC".
    #[arg(long)]
    pub tape_label: String,
    /// Where to write the profile.
    #[arg(long)]
    pub out: PathBuf,
}

/// What Bullet looked like while we recorded it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Profile {
    /// Which Binance tape this was measured against.
    pub tape: String,
    /// Hours of overlapping data behind these numbers.
    pub hours: f64,
    pub book_updates_per_sec: f64,
    pub spread_bps_p50: f64,
    pub spread_bps_p90: f64,
    /// Typical size at the best bid/ask.
    pub top_size_p50: Decimal,
    pub trades_per_hour: f64,
    /// How many tape trades there were per hour over the same time.
    pub tape_trades_per_hour: f64,
    /// Share of Bullet trades where the buyer crossed the spread.
    pub buy_share: f64,
    /// Bullet trade sizes at the 0th, 5th, ..., 100th percentile.
    pub trade_size_quantiles: Vec<Decimal>,
    /// How far past the mid Bullet trades print, toward the aggressor's
    /// side, in bps, at the 0th, 5th, ..., 100th percentile.
    pub trade_offset_bps_quantiles: Vec<f64>,
    /// (Bullet mid − tape price) / tape price, in bps.
    pub basis_bps_mean: f64,
    pub basis_bps_std: f64,
    /// How fast the gap returns to its mean: the time for its
    /// autocorrelation to fall to one half, in seconds.
    #[serde(default)]
    pub basis_halflife_secs: f64,
    /// How far behind the tape Bullet's mid moves, in ms.
    pub lag_ms: u64,
    /// The correlation of 1 s moves at that lag (how much Bullet follows the tape).
    pub lag_correlation: f64,
}

pub fn run(args: &Args) -> Result<()> {
    let quotes: Vec<Quote> =
        CsvReader::open(args.bullet_quotes.clone(), Quote::parse).collect::<Result<_>>()?;
    let trades: Vec<Trade> =
        CsvReader::open(args.bullet_trades.clone(), Trade::parse).collect::<Result<_>>()?;
    let (Some(first), Some(last)) = (quotes.first(), quotes.last()) else {
        bail!("no Bullet quotes");
    };
    let (start, end) = (first.recv_us, last.recv_us);
    // Only the part of the tape that overlaps the recording (plus a little
    // before, so we know the price at the start).
    let tape: Vec<Trade> = CsvReader::open(args.tape.clone(), Trade::parse)
        .filter(|t| {
            t.as_ref().map_or(true, |t| {
                t.recv_us + 60_000_000 >= start && t.recv_us <= end
            })
        })
        .collect::<Result<_>>()?;
    let profile = measure(&quotes, &trades, &tape, &args.tape_label)?;
    let text = toml::to_string_pretty(&profile)?;
    std::fs::write(&args.out, &text)?;
    println!("{text}");
    println!("written to {}", args.out.display());
    Ok(())
}

pub fn measure(
    quotes: &[Quote],
    trades: &[Trade],
    tape: &[Trade],
    tape_label: &str,
) -> Result<Profile> {
    let (start, end) = match (quotes.first(), quotes.last()) {
        (Some(f), Some(l)) if l.recv_us > f.recv_us => (f.recv_us, l.recv_us),
        _ => bail!("need at least two Bullet quotes"),
    };
    let tape_in: Vec<&Trade> = tape
        .iter()
        .filter(|t| t.recv_us >= start && t.recv_us <= end)
        .collect();
    if tape_in.is_empty() {
        bail!("the tape doesn't overlap the Bullet recording: fetch the same days");
    }
    let hours = (end - start) as f64 / 3.6e9;
    let f = |d: Decimal| d.to_f64().unwrap_or(f64::NAN);
    let mid = |q: &Quote| (q.bid + q.ask) / Decimal::TWO;

    // The book.
    let mut spreads: Vec<f64> = quotes
        .iter()
        .map(|q| f((q.ask - q.bid) / mid(q)) * 1e4)
        .collect();
    let mut sizes: Vec<Decimal> = quotes
        .iter()
        .map(|q| (q.bid_qty + q.ask_qty) / Decimal::TWO)
        .collect();
    sizes.sort();

    // Trades, each against the Bullet mid just before it.
    let trades_in: Vec<&Trade> = trades
        .iter()
        .filter(|t| t.recv_us >= start && t.recv_us <= end)
        .collect();
    let mut trade_sizes: Vec<Decimal> = trades_in.iter().map(|t| t.qty).collect();
    trade_sizes.sort();
    let mut offsets = Vec::new();
    let mut qi = 0;
    for t in &trades_in {
        while qi + 1 < quotes.len() && quotes[qi + 1].recv_us <= t.recv_us {
            qi += 1;
        }
        let m = mid(&quotes[qi]);
        let toward = if t.buyer_is_maker { -1.0 } else { 1.0 };
        offsets.push(toward * f((t.price - m) / m) * 1e4);
    }
    let buys = trades_in.iter().filter(|t| !t.buyer_is_maker).count();

    // Basis and lag, on a 100 ms grid of (Bullet mid, last tape price).
    const STEP: u64 = 100_000;
    let mut grid_bullet = Vec::new();
    let mut grid_tape = Vec::new();
    let (mut qi, mut ti) = (0, 0);
    let mut t = start;
    while t <= end {
        while qi + 1 < quotes.len() && quotes[qi + 1].recv_us <= t {
            qi += 1;
        }
        while ti + 1 < tape.len() && tape[ti + 1].recv_us <= t {
            ti += 1;
        }
        if tape[ti].recv_us <= t {
            grid_bullet.push(f(mid(&quotes[qi])));
            grid_tape.push(f(tape[ti].price));
        }
        t += STEP;
    }
    let (lag_steps, lag_correlation) = best_lag(&grid_bullet, &grid_tape, 10, 50);
    // The gap, against the tape as Bullet saw it: `lag` earlier.
    let basis: Vec<f64> = (lag_steps..grid_bullet.len())
        .map(|i| (grid_bullet[i] - grid_tape[i - lag_steps]) / grid_tape[i - lag_steps] * 1e4)
        .collect();

    Ok(Profile {
        tape: tape_label.to_string(),
        hours,
        book_updates_per_sec: quotes.len() as f64 / (hours * 3600.0),
        spread_bps_p50: quantile_f64(&mut spreads, 0.5),
        spread_bps_p90: quantile_f64(&mut spreads, 0.9),
        top_size_p50: sizes[sizes.len() / 2],
        trades_per_hour: trades_in.len() as f64 / hours,
        tape_trades_per_hour: tape_in.len() as f64 / hours,
        buy_share: if trades_in.is_empty() {
            0.5
        } else {
            buys as f64 / trades_in.len() as f64
        },
        trade_size_quantiles: (0..=20)
            .map(|i| pick(&trade_sizes, i as f64 / 20.0))
            .collect(),
        trade_offset_bps_quantiles: {
            offsets.sort_by(f64::total_cmp);
            (0..=20).map(|i| pick(&offsets, i as f64 / 20.0)).collect()
        },
        basis_bps_mean: mean(&basis),
        basis_bps_std: std(&basis),
        basis_halflife_secs: halflife_steps(&basis, 6000) as f64 * STEP as f64 / 1e6,
        lag_ms: lag_steps as u64 * STEP / 1000,
        lag_correlation,
    })
}

/// Which delay (in grid steps) best lines up Bullet's `horizon`-step moves
/// with the tape's earlier moves, and the correlation there.
fn best_lag(bullet: &[f64], tape: &[f64], horizon: usize, max_lag: usize) -> (usize, f64) {
    let mut best = (0, f64::NEG_INFINITY);
    for lag in 0..=max_lag {
        let mut xs = Vec::new();
        let mut ys = Vec::new();
        let mut i = lag;
        while i + horizon < bullet.len() {
            xs.push((bullet[i + horizon] / bullet[i]).ln());
            ys.push((tape[i + horizon - lag] / tape[i - lag]).ln());
            i += horizon;
        }
        let c = correlation(&xs, &ys);
        // On a tie, keep the shorter lag.
        if c > best.1 + 1e-9 {
            best = (lag, c);
        }
    }
    best
}

/// The first lag (in grid steps, up to `max`) at which the series'
/// autocorrelation falls below one half.
fn halflife_steps(v: &[f64], max: usize) -> usize {
    let m = mean(v);
    let d: Vec<f64> = v.iter().map(|x| x - m).collect();
    let var: f64 = d.iter().map(|x| x * x).sum();
    if var == 0.0 {
        return 0;
    }
    for k in 1..max.min(d.len()) {
        let c: f64 = d.iter().zip(&d[k..]).map(|(a, b)| a * b).sum();
        if c / var < 0.5 {
            return k;
        }
    }
    max
}

fn mean(v: &[f64]) -> f64 {
    if v.is_empty() {
        f64::NAN
    } else {
        v.iter().sum::<f64>() / v.len() as f64
    }
}

fn std(v: &[f64]) -> f64 {
    let m = mean(v);
    (v.iter().map(|x| (x - m).powi(2)).sum::<f64>() / v.len().max(1) as f64).sqrt()
}

fn correlation(x: &[f64], y: &[f64]) -> f64 {
    let (mx, my) = (mean(x), mean(y));
    let cov: f64 = x.iter().zip(y).map(|(a, b)| (a - mx) * (b - my)).sum();
    let vx: f64 = x.iter().map(|a| (a - mx).powi(2)).sum();
    let vy: f64 = y.iter().map(|b| (b - my).powi(2)).sum();
    if vx == 0.0 || vy == 0.0 {
        0.0
    } else {
        cov / (vx * vy).sqrt()
    }
}

fn quantile_f64(v: &mut [f64], q: f64) -> f64 {
    v.sort_by(f64::total_cmp);
    pick(v, q)
}

/// The `q` quantile of sorted values (nearest rank). Zero for no values.
fn pick<T: Copy + Default>(sorted: &[T], q: f64) -> T {
    if sorted.is_empty() {
        return T::default();
    }
    sorted[((sorted.len() - 1) as f64 * q).round() as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    const S: u64 = 1_000_000;

    /// A tape that wiggles every 100 ms, and a Bullet that copies it 300 ms
    /// later, 10 bps higher, with a 0.02 spread and a trade every 10 s.
    fn fixture() -> (Vec<Quote>, Vec<Trade>, Vec<Trade>) {
        // A random walk in 0.01 steps (a simple LCG, so the test is repeatable).
        let mut seed = 42u64;
        let mut level = 10_000i64;
        let walk: Vec<Decimal> = (0..36_000)
            .map(|_| {
                seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                level += if seed >> 63 == 1 { 1 } else { -1 };
                Decimal::new(level, 2)
            })
            .collect();
        let price = |i: u64| walk[i as usize];
        let mut tape = Vec::new();
        let mut quotes = Vec::new();
        let mut trades = Vec::new();
        for i in 0..36_000u64 {
            let t = 1_000 * S + i * 100_000;
            tape.push(Trade {
                recv_us: t,
                exch_us: t,
                price: price(i),
                qty: d("1"),
                buyer_is_maker: i % 2 == 0,
            });
            let m = price(i) * d("1.001");
            quotes.push(Quote {
                recv_us: t + 300_000,
                exch_us: t,
                bid: m - d("0.01"),
                bid_qty: d("5"),
                ask: m + d("0.01"),
                ask_qty: d("5"),
            });
            if i % 100 == 50 {
                trades.push(Trade {
                    recv_us: t + 350_000,
                    exch_us: t,
                    price: m + d("0.01"),
                    qty: d("0.5"),
                    buyer_is_maker: false,
                });
            }
        }
        (quotes, trades, tape)
    }

    #[test]
    fn measures_spread_rate_basis_and_lag() {
        let (quotes, trades, tape) = fixture();
        let p = measure(&quotes, &trades, &tape, "test").unwrap();
        assert!((p.spread_bps_p50 - 2.0).abs() < 0.01, "{p:?}");
        assert!((p.trades_per_hour - 360.0).abs() < 2.0, "{p:?}");
        assert!((p.tape_trades_per_hour - 36_000.0).abs() < 50.0, "{p:?}");
        assert!((p.basis_bps_mean - 10.0).abs() < 0.1, "{p:?}");
        assert!(p.basis_bps_std < 0.1, "lined up, the gap is steady: {p:?}");
        assert_eq!(p.lag_ms, 300, "{p:?}");
        assert_eq!(p.buy_share, 1.0);
        assert_eq!(p.top_size_p50, d("5"));
        assert_eq!(p.trade_size_quantiles[10], d("0.5"));
        assert!(
            (p.trade_offset_bps_quantiles[10] - 1.0).abs() < 0.01,
            "at the ask: 1 bp past the mid"
        );
    }

    #[test]
    fn measures_how_fast_a_gap_fades() {
        // A gap that halves every 50 steps: 0.5^(k/50) autocorrelation.
        let mut seed = 7u64;
        let mut x = 0.0;
        let v: Vec<f64> = (0..200_000)
            .map(|_| {
                seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                let shock = (seed >> 11) as f64 / (1u64 << 53) as f64 - 0.5;
                x = x * 0.5f64.powf(1.0 / 50.0) + shock;
                x
            })
            .collect();
        let k = halflife_steps(&v, 1000);
        assert!((45..=55).contains(&k), "{k}");
    }

    #[test]
    fn refuses_a_tape_from_other_days() {
        let (quotes, trades, mut tape) = fixture();
        for t in &mut tape {
            t.recv_us += 86_400 * S;
        }
        assert!(measure(&quotes, &trades, &tape, "test").is_err());
    }

    #[test]
    fn profiles_survive_toml() {
        let (quotes, trades, tape) = fixture();
        let p = measure(&quotes, &trades, &tape, "test").unwrap();
        let back: Profile = toml::from_str(&toml::to_string_pretty(&p).unwrap()).unwrap();
        assert_eq!(back.trade_size_quantiles, p.trade_size_quantiles);
        assert_eq!(back.lag_ms, p.lag_ms);
    }
}
