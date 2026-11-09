//! Backtest: replay recorded market data through the real `MarketMaker`.
//!
//! The strategy code is the live bot's, unchanged. Only two things are
//! simulated:
//!
//! - **The clock.** Every event carries the time we received it (`recv_us`),
//!   and the market maker is told that time, so staleness checks, requote
//!   rate limits, warm-up and order refreshes all behave as they would live.
//! - **The exchange.** [`SimExchange`] stands where the executor would. An
//!   order goes live `latency` after we send it, and a cancel lands
//!   `latency` after we ask (our order can still fill in between, as it can
//!   live). Orders that would cross the book when they arrive are rejected
//!   (they're post-only), and they expire after `quote_ttl_secs`.
//!
//! **Fills** are the guess, so they're explicit and conservative by default:
//! - a Bullet trade *through* our price fills us (a sell printed below our
//!   bid must have hit it first), up to the trade's size;
//! - with `--fill-at-touch`, a trade *at* our price also fills us. That's
//!   optimistic: it assumes we're first in the queue at our price;
//! - if Bullet's best ask drops to our bid (or the best bid rises to our
//!   ask), someone would have traded with us, so we fill in full.
//!
//! Fills are always at our order's price, paying `maker_fee_bps`.

use std::collections::VecDeque;
use std::iter::Peekable;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;

use crate::book::Book;
use crate::config::Config;
use crate::events::{Event, Fill, Intent, Order, Side, Stamp};
use crate::market_maker::MarketMaker;
use crate::marketdata::{CsvReader, CsvWriter, Quote, Trade};

const BPS: Decimal = Decimal::from_parts(10_000, 0, 0, false, 0);

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Bot settings (the strategy's numbers).
    #[arg(long, default_value = "bot.toml")]
    pub config: PathBuf,
    /// Binance best bid/ask files (`binance-*-quotes-*.csv`).
    #[arg(long, num_args = 1..)]
    pub binance_quotes: Vec<PathBuf>,
    /// Binance trade files, used as the price reference when there are no
    /// quote files (bid = ask = last trade). Coarser than quotes.
    #[arg(long, num_args = 1..)]
    pub binance_trades: Vec<PathBuf>,
    /// Bullet top-of-book files (`bullet-*-quotes-*.csv`, from `record`).
    #[arg(long, num_args = 1.., required = true)]
    pub bullet_quotes: Vec<PathBuf>,
    /// Bullet trade files (`bullet-*-trades-*.csv`, from `record`).
    #[arg(long, num_args = 1..)]
    pub bullet_trades: Vec<PathBuf>,
    /// Bullet's price increment. Defaults to the `meta.toml` that `record`
    /// writes next to the Bullet files.
    #[arg(long)]
    pub tick: Option<Decimal>,
    /// Time from sending an order (or cancel) to it taking effect, in ms.
    /// The scoreboard's order-to-live p50 from Tokyo is ~25 ms.
    #[arg(long, default_value_t = 25)]
    pub latency_ms: u64,
    /// Maker fee in bps of notional (negative for a rebate).
    #[arg(long, default_value = "0")]
    pub maker_fee_bps: Decimal,
    /// Also fill on trades exactly at our price (assumes we're first in line).
    #[arg(long)]
    pub fill_at_touch: bool,
    /// Write position and PnL once a second to this CSV.
    #[arg(long)]
    pub equity_out: Option<PathBuf>,
}

pub fn run(args: &Args) -> Result<()> {
    let config = Config::load(&args.config)?;
    let tick = match args.tick {
        Some(t) => t,
        None => tick_from_meta(&args.bullet_quotes, &config.symbol)?,
    };
    let reference: Box<dyn Iterator<Item = Result<Market>>> = if !args.binance_quotes.is_empty() {
        Box::new(
            CsvReader::open(args.binance_quotes.clone(), Quote::parse)
                .map(|q| q.map(Market::Reference)),
        )
    } else if !args.binance_trades.is_empty() {
        Box::new(
            CsvReader::open(args.binance_trades.clone(), Trade::parse).map(|t| {
                t.map(|t| {
                    Market::Reference(Quote {
                        recv_us: t.recv_us,
                        exch_us: t.exch_us,
                        bid: t.price,
                        bid_qty: t.qty,
                        ask: t.price,
                        ask_qty: t.qty,
                    })
                })
            }),
        )
    } else {
        bail!("give --binance-quotes or --binance-trades: the bot needs a reference price");
    };
    let bullet_book = CsvReader::open(args.bullet_quotes.clone(), Quote::parse)
        .map(|q| q.map(Market::BulletBook));
    let bullet_trades = CsvReader::open(args.bullet_trades.clone(), Trade::parse)
        .map(|t| t.map(Market::BulletTrade));
    if args.bullet_trades.is_empty() {
        tracing::warn!(
            "no --bullet-trades: fills only come from the book moving through our quotes"
        );
    }
    let sim = SimConfig {
        latency_us: args.latency_ms * 1000,
        fee_rate: args.maker_fee_bps / BPS,
        fill_at_touch: args.fill_at_touch,
        ttl_us: config.quote_ttl_secs * 1_000_000,
    };
    let mut equity = match &args.equity_out {
        Some(p) => {
            let _ = std::fs::remove_file(p);
            Some(CsvWriter::create(p, "ts_us,bullet_mid,position,pnl,fees")?)
        }
        None => None,
    };
    let report = replay(
        config,
        tick,
        sim,
        vec![
            Box::new(reference),
            Box::new(bullet_book),
            Box::new(bullet_trades),
        ],
        equity.as_mut(),
    )?;
    if let Some(e) = &mut equity {
        e.flush()?;
    }
    println!("{report}");
    Ok(())
}

fn tick_from_meta(bullet_files: &[PathBuf], symbol: &str) -> Result<Decimal> {
    #[derive(serde::Deserialize)]
    struct Meta {
        tick: Decimal,
    }
    let dir = bullet_files
        .first()
        .and_then(|p| p.parent())
        .unwrap_or(Path::new("."));
    let path = dir.join(format!("bullet-{symbol}-meta.toml"));
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("no --tick given and can't read {}", path.display()))?;
    Ok(toml::from_str::<Meta>(&text)?.tick)
}

/// One row of input, from any file.
#[derive(Debug, Clone, Copy)]
pub enum Market {
    Reference(Quote),
    BulletBook(Quote),
    BulletTrade(Trade),
}

impl Market {
    fn recv_us(&self) -> u64 {
        match self {
            Market::Reference(q) | Market::BulletBook(q) => q.recv_us,
            Market::BulletTrade(t) => t.recv_us,
        }
    }
}

type Source<'a> = Peekable<Box<dyn Iterator<Item = Result<Market>> + 'a>>;

/// Merge sources by receive time without loading them into memory.
struct Merged<'a> {
    sources: Vec<Source<'a>>,
}

impl Iterator for Merged<'_> {
    type Item = Result<Market>;

    fn next(&mut self) -> Option<Result<Market>> {
        let mut best: Option<(usize, u64)> = None;
        for (i, s) in self.sources.iter_mut().enumerate() {
            match s.peek() {
                Some(Err(_)) => return s.next(),
                Some(Ok(m)) if best.is_none_or(|(_, t)| m.recv_us() < t) => {
                    best = Some((i, m.recv_us()))
                }
                _ => {}
            }
        }
        self.sources[best?.0].next()
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SimConfig {
    pub latency_us: u64,
    /// Fee as a fraction of notional.
    pub fee_rate: Decimal,
    pub fill_at_touch: bool,
    pub ttl_us: u64,
}

#[derive(Debug)]
struct SimOrder {
    order: Order,
    sent_us: u64,
    live: bool,
    remaining: Decimal,
    cancel_at: Option<u64>,
}

/// A pretend Bullet: holds our orders and decides when they fill.
pub struct SimExchange {
    config: SimConfig,
    orders: Vec<SimOrder>,
    best_bid: Option<Decimal>,
    best_ask: Option<Decimal>,
    next_trade_id: u64,
    pub rejected: u64,
}

impl SimExchange {
    pub fn new(config: SimConfig) -> Self {
        Self {
            config,
            orders: Vec::new(),
            best_bid: None,
            best_ask: None,
            next_trade_id: 0,
            rejected: 0,
        }
    }

    /// Carry out the market maker's intents, sent at `now`.
    pub fn execute(&mut self, intents: &[Intent], now: u64) {
        let lands = now + self.config.latency_us;
        for intent in intents {
            match intent {
                Intent::Place(order) => self.orders.push(SimOrder {
                    order: *order,
                    sent_us: now,
                    live: false,
                    remaining: order.size,
                    cancel_at: None,
                }),
                Intent::Cancel { client_id } => {
                    for o in self
                        .orders
                        .iter_mut()
                        .filter(|o| o.order.client_id == *client_id)
                    {
                        o.cancel_at.get_or_insert(lands);
                    }
                }
                Intent::CancelAll => {
                    for o in &mut self.orders {
                        o.cancel_at.get_or_insert(lands);
                    }
                }
                Intent::SyncPosition | Intent::SyncFills => {}
            }
        }
    }

    /// Let time pass to `now`: orders arrive, cancels land, orders expire.
    pub fn advance(&mut self, now: u64) -> Vec<Event> {
        let mut events = Vec::new();
        let (latency, ttl) = (self.config.latency_us, self.config.ttl_us);
        let (best_bid, best_ask) = (self.best_bid, self.best_ask);
        let rejected = &mut self.rejected;
        self.orders.retain_mut(|o| {
            let arrives = o.sent_us + latency;
            if o.cancel_at.is_some_and(|c| c <= now) {
                return false; // cancelled; the market maker already forgot it
            }
            if !o.live && arrives <= now {
                // Post-only: an order that would take liquidity is rejected.
                let crosses = match o.order.side {
                    Side::Buy => best_ask.is_some_and(|a| a <= o.order.price),
                    Side::Sell => best_bid.is_some_and(|b| b >= o.order.price),
                };
                if crosses {
                    *rejected += 1;
                    events.push(Event::PlaceFailed {
                        client_ids: vec![o.order.client_id],
                    });
                    return false;
                }
                o.live = true;
            }
            if now >= o.sent_us + ttl {
                events.push(Event::OrderGone {
                    client_id: o.order.client_id,
                });
                return false;
            }
            true
        });
        events
    }

    /// Bullet's book moved. Anything it moved through has traded with us.
    pub fn on_book(&mut self, bid: Decimal, ask: Decimal) -> Vec<Event> {
        self.best_bid = Some(bid);
        self.best_ask = Some(ask);
        let mut fills = Vec::new();
        for o in self.orders.iter_mut().filter(|o| o.live) {
            let through = match o.order.side {
                Side::Buy => ask <= o.order.price,
                Side::Sell => bid >= o.order.price,
            };
            if through {
                let size = o.remaining;
                fills.push(self_fill(
                    &mut self.next_trade_id,
                    self.config.fee_rate,
                    o,
                    size,
                ));
            }
        }
        self.orders.retain(|o| o.remaining > Decimal::ZERO);
        fills
    }

    /// A public trade on Bullet. It fills our resting orders on the side it hit.
    pub fn on_trade(&mut self, trade: &Trade) -> Vec<Event> {
        // buyer_is_maker: a seller crossed the spread, so it hit the bids.
        let hit = if trade.buyer_is_maker {
            Side::Buy
        } else {
            Side::Sell
        };
        let mut left = trade.qty;
        let mut fills = Vec::new();
        for o in self
            .orders
            .iter_mut()
            .filter(|o| o.live && o.order.side == hit)
        {
            if left <= Decimal::ZERO {
                break;
            }
            let fills_us = match hit {
                Side::Buy => {
                    trade.price < o.order.price
                        || (self.config.fill_at_touch && trade.price == o.order.price)
                }
                Side::Sell => {
                    trade.price > o.order.price
                        || (self.config.fill_at_touch && trade.price == o.order.price)
                }
            };
            if fills_us {
                let size = o.remaining.min(left);
                left -= size;
                fills.push(self_fill(
                    &mut self.next_trade_id,
                    self.config.fee_rate,
                    o,
                    size,
                ));
            }
        }
        self.orders.retain(|o| o.remaining > Decimal::ZERO);
        fills
    }

    /// Both quotes resting (for the "time quoting" stat).
    fn quoting_both(&self) -> bool {
        let live = |s| self.orders.iter().any(|o| o.live && o.order.side == s);
        live(Side::Buy) && live(Side::Sell)
    }
}

fn self_fill(next_id: &mut u64, fee_rate: Decimal, o: &mut SimOrder, size: Decimal) -> Event {
    *next_id += 1;
    o.remaining -= size;
    Event::Fill(Fill {
        trade_id: *next_id,
        client_id: Some(o.order.client_id),
        side: o.order.side,
        price: o.order.price,
        size,
        fee: (o.order.price * size * fee_rate).round_dp(8),
        order_finished: o.remaining.is_zero(),
    })
}

/// What happened, for the summary.
#[derive(Debug, Default)]
pub struct Report {
    pub start_us: u64,
    pub end_us: u64,
    pub events: u64,
    pub fills: u64,
    pub bought: Decimal,
    pub sold: Decimal,
    pub notional: Decimal,
    pub fees: Decimal,
    pub final_position: Decimal,
    pub final_pnl: Option<Decimal>,
    pub max_abs_position: Decimal,
    pub max_drawdown: Decimal,
    pub seconds_quoting_both: u64,
    pub seconds: u64,
    pub rejected: u64,
    /// Mid move after each fill in our favour, in bps (+ = the fill was good).
    pub markout_1s: Vec<f64>,
    pub markout_10s: Vec<f64>,
    pub status: String,
}

impl std::fmt::Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let hours = (self.end_us.saturating_sub(self.start_us)) as f64 / 3.6e9;
        let mean = |v: &[f64]| {
            if v.is_empty() {
                f64::NAN
            } else {
                v.iter().sum::<f64>() / v.len() as f64
            }
        };
        writeln!(
            f,
            "backtest {} -> {} ({hours:.2} h, {} events)",
            iso(self.start_us),
            iso(self.end_us),
            self.events
        )?;
        writeln!(
            f,
            "  pnl            ${} (after ${} fees)",
            show(self.final_pnl.map(|p| p.round_dp(4))),
            self.fees.round_dp(4)
        )?;
        writeln!(f, "  max drawdown   ${}", self.max_drawdown.round_dp(4))?;
        writeln!(
            f,
            "  fills          {} (bought {}, sold {}, ${} traded)",
            self.fills,
            self.bought,
            self.sold,
            self.notional.round_dp(2)
        )?;
        writeln!(
            f,
            "  position       end {}, max |{}|",
            self.final_position, self.max_abs_position
        )?;
        writeln!(
            f,
            "  quoting        both sides {:.1}% of the time, {} orders rejected as crossing",
            100.0 * self.seconds_quoting_both as f64 / self.seconds.max(1) as f64,
            self.rejected
        )?;
        writeln!(
            f,
            "  markout        +1s {:.2} bps, +10s {:.2} bps (mean per fill; negative = adverse selection)",
            mean(&self.markout_1s),
            mean(&self.markout_10s)
        )?;
        write!(f, "  final status   {}", self.status)
    }
}

fn show(v: Option<Decimal>) -> String {
    v.map_or_else(|| "-".into(), |v| v.to_string())
}

fn iso(us: u64) -> String {
    let secs = us / 1_000_000;
    let day = crate::fetch::date_string((secs / 86_400) as i64);
    let s = secs % 86_400;
    format!("{day} {:02}:{:02}:{:02}Z", s / 3600, s / 60 % 60, s % 60)
}

/// Run the market maker over merged market data.
pub fn replay<'a>(
    config: Config,
    tick: Decimal,
    sim: SimConfig,
    sources: Vec<Box<dyn Iterator<Item = Result<Market>> + 'a>>,
    mut equity: Option<&mut CsvWriter>,
) -> Result<Report> {
    let mut input = Merged {
        sources: sources.into_iter().map(Iterator::peekable).collect(),
    }
    .peekable();
    let mut mm = MarketMaker::new(config, tick);
    let mut exchange = SimExchange::new(sim);
    let mut report = Report::default();
    let base = Instant::now();
    let Some(Ok(first)) = input.peek() else {
        bail!("no market data");
    };
    let t0 = first.recv_us();
    report.start_us = t0;
    let at = |us: u64| base + Duration::from_micros(us.saturating_sub(t0));
    let mut next_second = t0 + 1_000_000;
    let mut mid: Option<Decimal> = None;
    let mut peak = Decimal::ZERO;
    // Fills waiting for their markout: (due_us, side, price, horizon).
    let mut markouts: VecDeque<(u64, Side, Decimal, u8)> = VecDeque::new();

    exchange.execute(&mm.start(), t0);

    let step = |mm: &mut MarketMaker,
                exchange: &mut SimExchange,
                event: Event,
                now: u64,
                report: &mut Report,
                markouts: &mut VecDeque<(u64, Side, Decimal, u8)>| {
        feed(mm, exchange, event, now, at(now), report, markouts);
    };

    while let Some(row) = input.next() {
        let row = row?;
        let now = row.recv_us();
        report.events += 1;

        // The once-a-second timer, for every second that passed before this row.
        while next_second <= now {
            let t = next_second;
            for e in exchange.advance(t) {
                step(&mut mm, &mut exchange, e, t, &mut report, &mut markouts);
            }
            step(
                &mut mm,
                &mut exchange,
                Event::Tick,
                t,
                &mut report,
                &mut markouts,
            );
            let intents = mm.requote(at(t));
            exchange.execute(&intents, t);
            // Per-second bookkeeping.
            report.seconds += 1;
            report.seconds_quoting_both += u64::from(exchange.quoting_both());
            let pos = mm.position();
            report.max_abs_position = report.max_abs_position.max(pos.abs());
            if let Some(pnl) = mm.pnl() {
                peak = peak.max(pnl);
                report.max_drawdown = report.max_drawdown.max(peak - pnl);
                if let Some(w) = equity.as_deref_mut() {
                    w.row(&format!(
                        "{t},{},{pos},{},{}",
                        show(mid),
                        pnl.round_dp(6),
                        mm.fees()
                    ))?;
                }
            }
            while let Some(&(due, side, price, horizon)) = markouts.front() {
                if due > t {
                    break;
                }
                markouts.pop_front();
                if let Some(m) = mid {
                    let edge = match side {
                        Side::Buy => (m - price) / price,
                        Side::Sell => (price - m) / price,
                    };
                    let bps = (edge * BPS).to_f64().unwrap_or(0.0);
                    if horizon == 1 {
                        report.markout_1s.push(bps)
                    } else {
                        report.markout_10s.push(bps)
                    }
                }
            }
            next_second += 1_000_000;
        }

        for e in exchange.advance(now) {
            step(&mut mm, &mut exchange, e, now, &mut report, &mut markouts);
        }
        let event = match row {
            Market::Reference(q) => Event::Reference {
                bid: q.bid,
                ask: q.ask,
                stamp: stamp(q.recv_us, q.exch_us, at(now)),
            },
            Market::BulletBook(q) => {
                for fill in exchange.on_book(q.bid, q.ask) {
                    step(
                        &mut mm,
                        &mut exchange,
                        fill,
                        now,
                        &mut report,
                        &mut markouts,
                    );
                }
                mid = Some((q.bid + q.ask) / Decimal::TWO);
                let book = Book {
                    bids: [(q.bid, q.bid_qty)].into_iter().collect(),
                    asks: [(q.ask, q.ask_qty)].into_iter().collect(),
                };
                Event::Book(book, stamp(q.recv_us, q.exch_us, at(now)))
            }
            Market::BulletTrade(t) => {
                for fill in exchange.on_trade(&t) {
                    step(
                        &mut mm,
                        &mut exchange,
                        fill,
                        now,
                        &mut report,
                        &mut markouts,
                    );
                }
                continue; // the market maker doesn't watch public trades
            }
        };
        step(
            &mut mm,
            &mut exchange,
            event,
            now,
            &mut report,
            &mut markouts,
        );

        // Like the live loop: quote once we've caught up with everything
        // that arrived at the same moment.
        let caught_up = match input.peek() {
            Some(Ok(next)) => next.recv_us() > now,
            _ => true,
        };
        if caught_up {
            let intents = mm.requote(at(now));
            exchange.execute(&intents, now);
        }
        report.end_us = now;
    }

    let end = report.end_us;
    exchange.execute(&mm.stop(), end);
    for e in exchange.advance(end + sim.latency_us) {
        step(&mut mm, &mut exchange, e, end, &mut report, &mut markouts);
    }
    report.final_position = mm.position();
    report.final_pnl = mm.pnl();
    report.rejected = exchange.rejected;
    report.status = mm.status().to_string();
    Ok(report)
}

/// Feed one event to the market maker and carry out its reply.
fn feed(
    mm: &mut MarketMaker,
    exchange: &mut SimExchange,
    event: Event,
    now: u64,
    at: Instant,
    report: &mut Report,
    markouts: &mut VecDeque<(u64, Side, Decimal, u8)>,
) {
    if let Event::Fill(f) = &event {
        report.fills += 1;
        report.notional += f.price * f.size;
        report.fees += f.fee;
        match f.side {
            Side::Buy => report.bought += f.size,
            Side::Sell => report.sold += f.size,
        }
        markouts.push_back((now + 1_000_000, f.side, f.price, 1));
        markouts.push_back((now + 10_000_000, f.side, f.price, 10));
    }
    let intents = mm.on_event(&event, at);
    exchange.execute(&intents, now);
}

fn stamp(recv_us: u64, exch_us: u64, received_at: Instant) -> Stamp {
    Stamp {
        received_at,
        received_unix_us: recv_us,
        exchange_unix_us: Some(exch_us),
        exchange_match_unix_us: Some(exch_us),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    fn sim() -> SimExchange {
        SimExchange::new(SimConfig {
            latency_us: 1000,
            fee_rate: Decimal::ZERO,
            fill_at_touch: false,
            ttl_us: 30_000_000,
        })
    }

    fn bid(price: &str, id: u64) -> Intent {
        Intent::Place(Order {
            side: Side::Buy,
            price: d(price),
            size: d("1"),
            client_id: id,
        })
    }

    fn trade(price: &str, qty: &str, buyer_is_maker: bool) -> Trade {
        Trade {
            recv_us: 0,
            exch_us: 0,
            price: d(price),
            qty: d(qty),
            buyer_is_maker,
        }
    }

    fn sizes(events: &[Event]) -> Vec<Decimal> {
        events
            .iter()
            .filter_map(|e| {
                if let Event::Fill(f) = e {
                    Some(f.size)
                } else {
                    None
                }
            })
            .collect()
    }

    #[test]
    fn an_order_is_not_live_until_latency_has_passed() {
        let mut ex = sim();
        ex.on_book(d("99"), d("101"));
        ex.execute(&[bid("100", 1)], 0);
        assert!(
            ex.on_trade(&trade("99", "5", true)).is_empty(),
            "still on its way"
        );
        ex.advance(1000);
        assert_eq!(sizes(&ex.on_trade(&trade("99", "5", true))), [d("1")]);
    }

    #[test]
    fn trades_at_our_price_fill_only_with_fill_at_touch() {
        let mut ex = sim();
        ex.on_book(d("99"), d("101"));
        ex.execute(&[bid("100", 1)], 0);
        ex.advance(1000);
        assert!(ex.on_trade(&trade("100", "5", true)).is_empty());
        ex.config.fill_at_touch = true;
        assert_eq!(sizes(&ex.on_trade(&trade("100", "5", true))), [d("1")]);
    }

    #[test]
    fn buys_hit_asks_not_bids_and_size_is_capped_by_the_trade() {
        let mut ex = sim();
        ex.on_book(d("99"), d("101"));
        ex.execute(&[bid("100", 1)], 0);
        ex.advance(1000);
        assert!(
            ex.on_trade(&trade("99", "5", false)).is_empty(),
            "a buyer lifted the asks"
        );
        let fills = ex.on_trade(&trade("99", "0.3", true));
        assert_eq!(sizes(&fills), [d("0.3")]);
        let Event::Fill(f) = &fills[0] else { panic!() };
        assert!(!f.order_finished, "0.7 still rests");
    }

    #[test]
    fn an_order_can_fill_while_its_cancel_is_in_flight() {
        let mut ex = sim();
        ex.on_book(d("99"), d("101"));
        ex.execute(&[bid("100", 1)], 0);
        ex.advance(1000);
        ex.execute(&[Intent::Cancel { client_id: 1 }], 2000);
        ex.advance(2500);
        assert_eq!(
            sizes(&ex.on_trade(&trade("99", "5", true))).len(),
            1,
            "cancel lands at 3000"
        );
        ex.execute(&[bid("100", 2)], 4000);
        ex.advance(5000);
        ex.execute(&[Intent::CancelAll], 5000);
        ex.advance(6000);
        assert!(ex.on_trade(&trade("99", "5", true)).is_empty(), "cancelled");
    }

    #[test]
    fn the_book_moving_through_us_fills_in_full() {
        let mut ex = sim();
        ex.on_book(d("99"), d("101"));
        ex.execute(&[bid("100", 1)], 0);
        ex.advance(1000);
        assert_eq!(sizes(&ex.on_book(d("98"), d("100"))), [d("1")]);
    }

    #[test]
    fn crossing_orders_are_rejected_on_arrival() {
        let mut ex = sim();
        ex.execute(&[bid("100", 7)], 0);
        ex.on_book(d("99"), d("99.5"));
        let events = ex.advance(1000);
        assert!(matches!(&events[..], [Event::PlaceFailed { client_ids }] if client_ids == &[7]));
        assert_eq!(ex.rejected, 1);
    }

    #[test]
    fn orders_expire_after_their_ttl() {
        let mut ex = sim();
        ex.on_book(d("99"), d("101"));
        ex.execute(&[bid("100", 3)], 0);
        ex.advance(1000);
        assert!(matches!(
            &ex.advance(30_000_000)[..],
            [Event::OrderGone { client_id: 3 }]
        ));
    }

    /// End to end: a calm market with sellers hitting the bid and buyers
    /// lifting the ask. The bot warms up, quotes, and earns its spread.
    #[test]
    fn replays_a_calm_market_and_earns_the_spread() {
        let mut config = Config::load(Path::new("bot.toml")).unwrap();
        config.spread_bps = d("10");
        config.skew_bps = Decimal::ZERO;
        let mut rows = Vec::new();
        let s = 1_000_000u64;
        for i in 0..300u64 {
            let t = 1_700_000_000 * s + i * s;
            rows.push(Market::Reference(Quote {
                recv_us: t,
                exch_us: t,
                bid: d("99.99"),
                bid_qty: d("1"),
                ask: d("100.01"),
                ask_qty: d("1"),
            }));
            rows.push(Market::BulletBook(Quote {
                recv_us: t + 10,
                exch_us: t,
                bid: d("99.5"),
                bid_qty: d("1"),
                ask: d("100.5"),
                ask_qty: d("1"),
            }));
            if i > 90 {
                // Alternate: someone sells through our bid, then buys through our ask.
                let (price, sell) = if i % 2 == 0 {
                    ("99.8", true)
                } else {
                    ("100.2", false)
                };
                rows.push(Market::BulletTrade(Trade {
                    recv_us: t + 500_000,
                    exch_us: t,
                    price: d(price),
                    qty: d("1"),
                    buyer_is_maker: sell,
                }));
            }
        }
        let report = replay(
            config,
            d("0.001"),
            SimConfig {
                latency_us: 25_000,
                fee_rate: Decimal::ZERO,
                fill_at_touch: false,
                ttl_us: 30 * s,
            },
            vec![Box::new(rows.into_iter().map(Ok))],
            None,
        )
        .unwrap();
        assert!(report.fills > 100, "{report}");
        assert!(report.final_pnl.unwrap() > Decimal::ZERO, "{report}");
        assert!(
            report.max_abs_position <= d("0.1"),
            "never past max_position: {report}"
        );
    }
}
