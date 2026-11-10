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

// ---------- screen ----------

#[derive(clap::Args, Debug)]
pub struct ScreenArgs {
    /// Folder with one subfolder of recordings per market.
    #[arg(long, default_value = "bullet-data")]
    pub data: PathBuf,
    /// Only use files from this UTC day (YYYY-MM-DD).
    #[arg(long)]
    pub day: Option<String>,
    /// Write the table here too (Markdown).
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Write a backtest config per market here, from `--template`, with
    /// orders of about `--order-usd` (rounded up to the market's lot size).
    #[arg(long)]
    pub write_configs: Option<PathBuf>,
    #[arg(long, default_value = "backtest.toml")]
    pub template: PathBuf,
    #[arg(long, default_value = "5")]
    pub order_usd: Decimal,
}

/// One market's numbers, from Bullet recordings alone.
#[derive(Debug, Clone, PartialEq)]
pub struct MarketStats {
    pub market: String,
    pub hours: f64,
    pub mid: Decimal,
    pub tick: Decimal,
    pub spread_bps_p50: f64,
    pub tick_bps: f64,
    pub top_usd_p50: f64,
    pub book_updates_per_sec: f64,
    pub trades_per_hour: f64,
    pub usd_per_hour: f64,
    pub buy_share: f64,
}

pub fn screen(args: &ScreenArgs) -> Result<()> {
    let map = binance_map(&args.data.join("binance-map.toml"));
    let mut rows = Vec::new();
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&args.data)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    for dir in dirs {
        let market = dir
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let files = |kind: &str| -> Vec<PathBuf> {
            let prefix = format!(
                "bullet-{market}-{kind}-{}",
                args.day.as_deref().unwrap_or("")
            );
            std::fs::read_dir(&dir)
                .map(|it| {
                    it.filter_map(|e| e.ok().map(|e| e.path()))
                        .filter(|p| {
                            p.file_name()
                                .is_some_and(|n| n.to_string_lossy().starts_with(&prefix))
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        let Ok(tick) = read_tick(&dir, &market) else {
            continue;
        };
        let quotes: Vec<Quote> =
            CsvReader::open(files("quotes"), Quote::parse).collect::<Result<_>>()?;
        let trades: Vec<Trade> =
            CsvReader::open(files("trades"), Trade::parse).collect::<Result<_>>()?;
        if let Some(s) = market_stats(&market, tick, &quotes, &trades) {
            rows.push(s);
        }
    }
    rows.sort_by(|a, b| b.usd_per_hour.total_cmp(&a.usd_per_hour));

    let mut table = String::from(
        "| market | hours | price | spread bp (p50) | tick bp | top of book $ (p50) | book updates/s | trades/h | $ traded/h | buys | Binance |\n|---|---|---|---|---|---|---|---|---|---|---|\n",
    );
    for s in &rows {
        table += &format!(
            "| {} | {:.1} | {} | {:.2} | {:.2} | {:.0} | {:.1} | {:.0} | {:.0} | {:.0}% | {} |\n",
            s.market,
            s.hours,
            s.mid.round_dp(4),
            s.spread_bps_p50,
            s.tick_bps,
            s.top_usd_p50,
            s.book_updates_per_sec,
            s.trades_per_hour,
            s.usd_per_hour,
            s.buy_share * 100.0,
            map.get(&s.market).map_or("-", String::as_str),
        );
    }
    println!("{table}");
    if let Some(out) = &args.out {
        std::fs::write(out, &table)?;
    }
    if let Some(dir) = &args.write_configs {
        std::fs::create_dir_all(dir)?;
        let template = std::fs::read_to_string(&args.template)?;
        for s in &rows {
            let Some(binance) = map.get(&s.market) else {
                continue;
            };
            let meta = read_meta(&args.data.join(&s.market), &s.market)?;
            let size = order_size(args.order_usd, s.mid, meta.step, meta.min_size);
            let text = market_config(&template, &s.market, binance, size, size * Decimal::TWO);
            let path = dir.join(format!("{}.toml", s.market));
            std::fs::write(&path, text)?;
            // Make sure it loads (catches a bad template early).
            crate::config::Config::load(&path)?;
        }
        println!("configs written to {}", dir.display());
    }
    Ok(())
}

pub fn market_stats(
    market: &str,
    tick: Decimal,
    quotes: &[Quote],
    trades: &[Trade],
) -> Option<MarketStats> {
    let (first, last) = (quotes.first()?, quotes.last()?);
    let hours = (last.recv_us.saturating_sub(first.recv_us)) as f64 / 3.6e9;
    if hours <= 0.0 {
        return None;
    }
    let f = |d: Decimal| d.to_f64().unwrap_or(f64::NAN);
    let mid_of = |q: &Quote| (q.bid + q.ask) / Decimal::TWO;
    let mut mids: Vec<Decimal> = quotes.iter().map(mid_of).collect();
    mids.sort();
    let mid = mids[mids.len() / 2];
    let mut spreads: Vec<f64> = quotes
        .iter()
        .map(|q| f((q.ask - q.bid) / mid_of(q)) * 1e4)
        .collect();
    let mut tops: Vec<f64> = quotes
        .iter()
        .map(|q| f((q.bid_qty + q.ask_qty) / Decimal::TWO * mid_of(q)))
        .collect();
    tops.sort_by(f64::total_cmp);
    let usd: f64 = trades.iter().map(|t| f(t.price * t.qty)).sum();
    let buys = trades.iter().filter(|t| !t.buyer_is_maker).count();
    Some(MarketStats {
        market: market.to_string(),
        hours,
        mid,
        tick,
        spread_bps_p50: quantile_f64(&mut spreads, 0.5),
        tick_bps: f(tick / mid) * 1e4,
        top_usd_p50: pick(&tops, 0.5),
        book_updates_per_sec: quotes.len() as f64 / (hours * 3600.0),
        trades_per_hour: trades.len() as f64 / hours,
        usd_per_hour: usd / hours,
        buy_share: if trades.is_empty() {
            0.5
        } else {
            buys as f64 / trades.len() as f64
        },
    })
}

/// About `usd` worth, rounded up to the lot size, and at least the minimum.
pub fn order_size(usd: Decimal, price: Decimal, step: Decimal, min_size: Decimal) -> Decimal {
    if price.is_zero() || step.is_zero() {
        return min_size;
    }
    let size = ((usd / price) / step).ceil() * step;
    size.max(min_size).normalize()
}

/// The template config with this market's symbol, reference and sizes.
pub fn market_config(
    template: &str,
    market: &str,
    binance: &str,
    order_size: Decimal,
    max_position: Decimal,
) -> String {
    template
        .lines()
        .map(|line| {
            let key = line.split('=').next().unwrap_or("").trim();
            match key {
                "symbol" => format!("symbol = \"{market}\""),
                "binance_symbol" => format!("binance_symbol = \"{binance}\""),
                "order_size" => format!("order_size = \"{order_size}\""),
                "max_position" => format!("max_position = \"{max_position}\""),
                _ => line.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

#[derive(Deserialize)]
struct Meta {
    tick: Decimal,
    step: Decimal,
    min_size: Decimal,
}

fn read_meta(dir: &std::path::Path, market: &str) -> Result<Meta> {
    let path = dir.join(format!("bullet-{market}-meta.toml"));
    Ok(toml::from_str(&std::fs::read_to_string(&path)?)?)
}

fn read_tick(dir: &std::path::Path, market: &str) -> Result<Decimal> {
    Ok(read_meta(dir, market)?.tick)
}

/// Bullet market -> Binance symbol, from `binance-map.toml` (empty if missing).
fn binance_map(path: &std::path::Path) -> std::collections::HashMap<String, String> {
    #[derive(Deserialize)]
    struct Entry {
        binance: String,
    }
    #[derive(Deserialize)]
    struct File {
        markets: std::collections::HashMap<String, Entry>,
    }
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| toml::from_str::<File>(&t).ok())
        .map(|f| f.markets.into_iter().map(|(k, v)| (k, v.binance)).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod screen_tests {
    use super::*;

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    #[test]
    fn orders_are_about_five_dollars_in_whole_lots() {
        assert_eq!(
            order_size(d("5"), d("85.86"), d("0.01"), d("0.01")),
            d("0.06")
        );
        assert_eq!(order_size(d("5"), d("0.0036"), d("1"), d("1")), d("1389"));
        assert_eq!(
            order_size(d("5"), d("4120"), d("0.001"), d("0.01")),
            d("0.01"),
            "never below the minimum"
        );
    }

    #[test]
    fn a_market_config_swaps_only_its_own_settings() {
        let template = std::fs::read_to_string("backtest.toml").unwrap();
        let text = market_config(&template, "HYPE-USD", "HYPEUSDT", d("0.07"), d("0.14"));
        let dir = std::env::temp_dir().join(format!("hft-screen-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("HYPE-USD.toml");
        std::fs::write(&path, text).unwrap();
        let c = crate::config::Config::load(&path).unwrap();
        assert_eq!(
            (c.symbol.as_str(), c.binance_symbol.as_str()),
            ("HYPE-USD", "HYPEUSDT")
        );
        assert_eq!((c.order_size, c.max_position), (d("0.07"), d("0.14")));
        assert!(c.dry_run, "still a dry-run config");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stats_from_a_simple_market() {
        let q = |t: u64| Quote {
            recv_us: t,
            exch_us: t,
            bid: d("99.99"),
            bid_qty: d("1"),
            ask: d("100.01"),
            ask_qty: d("1"),
        };
        let quotes = vec![q(0), q(3_600_000_000)];
        let trades = vec![
            Trade {
                recv_us: 1,
                exch_us: 1,
                price: d("100"),
                qty: d("2"),
                buyer_is_maker: false,
            },
            Trade {
                recv_us: 2,
                exch_us: 2,
                price: d("100"),
                qty: d("1"),
                buyer_is_maker: true,
            },
        ];
        let s = market_stats("X-USD", d("0.01"), &quotes, &trades).unwrap();
        assert!((s.spread_bps_p50 - 2.0).abs() < 1e-9);
        assert!((s.tick_bps - 1.0).abs() < 1e-9);
        assert!((s.usd_per_hour - 300.0).abs() < 1e-9);
        assert_eq!(s.trades_per_hour, 2.0);
        assert!((s.buy_share - 0.5).abs() < 1e-9);
    }
}
