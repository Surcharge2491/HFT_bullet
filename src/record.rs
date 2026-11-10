//! Record live market data to disk, for backtesting.
//!
//! Neither venue gives us the history we need: Binance's archive stopped
//! publishing `bookTicker` in 2024, and Bullet's API only returns its last
//! 1000 trades. So we record it ourselves: Bullet's order book (top of book
//! after every update) and trades, for as many Bullet markets as we like, plus
//! Binance's best bid/ask and trades for the bot's reference market.
//!
//! Files are `marketdata.rs` CSVs, one per market, stream and UTC hour, in a
//! folder per market. When an hour is over its file is gzipped, so finished
//! files never change again (and can be committed once):
//!
//! ```text
//! data/bullet-live/SOL-USD/bullet-SOL-USD-quotes-2026-11-10T040712.csv.gz
//! data/bullet-live/SOL-USD/bullet-SOL-USD-trades-2026-11-10T040712.csv.gz
//! data/bullet-live/SOL-USD/bullet-SOL-USD-meta.toml    (tick and lot size, for the backtest)
//! data/bullet-live/SOLUSDT/binance-perp-SOLUSDT-quotes-2026-11-10T040712.csv.gz
//! ```
//!
//! The time in the name is the file's first row, so names sort in time order.
//! Every row is stamped with when *we* received it, so the backtest sees the
//! data with the same delays the live bot would. Ctrl-C stops it cleanly.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use bullet_rust_sdk::{
    Client, ManagedWsConfig, Network, OrderbookDepth, ServerMessage, Topic, WsEvent,
};
use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::book::{BookEvent, BookKeeper};
use crate::config::Config;
use crate::marketdata::{CsvWriter, QUOTES_HEADER, Quote, TRADES_HEADER, Trade};

#[derive(clap::Args, Debug)]
pub struct Args {
    #[arg(long, default_value = "bot.toml")]
    pub config: PathBuf,
    #[arg(long, default_value = "data/bullet-live")]
    pub out: PathBuf,
    /// Stop after this many minutes (default: until Ctrl-C).
    #[arg(long)]
    pub minutes: Option<u64>,
    /// Which Bullet markets: "config" (the bot's market), "all", or a
    /// comma-separated list like SOL-USD,HYPE-USD.
    #[arg(long, default_value = "config")]
    pub markets: String,
    /// Markets to leave out with --markets all (blue chips by default).
    #[arg(
        long,
        value_delimiter = ',',
        default_value = "BTC-USD,ETH-USD,XRP-USD,DOGE-USD"
    )]
    pub exclude: Vec<String>,
    /// Don't record Binance (it answers 403 from some regions, e.g. US clouds).
    #[arg(long)]
    pub no_binance: bool,
}

/// Bullet markets per websocket connection: a busy connection can't delay
/// another, and a drop only interrupts a few markets.
const MARKETS_PER_CONNECTION: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Stream {
    BinanceQuotes,
    BinanceTrades,
    BulletQuotes,
    BulletTrades,
}

impl Stream {
    fn file_prefix(self, symbol: &str) -> (String, &'static str) {
        match self {
            Stream::BinanceQuotes => (format!("binance-perp-{symbol}-quotes"), QUOTES_HEADER),
            Stream::BinanceTrades => (format!("binance-perp-{symbol}-trades"), TRADES_HEADER),
            Stream::BulletQuotes => (format!("bullet-{symbol}-quotes"), QUOTES_HEADER),
            Stream::BulletTrades => (format!("bullet-{symbol}-trades"), TRADES_HEADER),
        }
    }
}

struct Row {
    stream: Stream,
    symbol: String,
    recv_us: u64,
    line: String,
}

type Rows = mpsc::UnboundedSender<Row>;

/// One open file: the hour it holds, where it is, and the writer.
struct Open {
    hour: u64,
    path: PathBuf,
    writer: CsvWriter,
}

pub async fn run(args: &Args) -> Result<()> {
    let config = Config::load(&args.config)?;
    let out = &args.out;
    std::fs::create_dir_all(out)?;

    let client = Client::builder()
        .network(match &config.api_url {
            Some(url) => Network::Custom(url.clone()),
            None => Network::from(config.network.as_str()),
        })
        .build()
        .await
        .context("connecting to Bullet")?;

    let markets = choose_markets(&client, &config, &args.markets, &args.exclude).await?;
    for symbol in &markets {
        let (tick, step, min_size) = crate::bullet::market_rules(&client, symbol).await?;
        let dir = out.join(symbol);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(
            dir.join(format!("bullet-{symbol}-meta.toml")),
            format!(
                "symbol = \"{symbol}\"\ntick = \"{tick}\"\nstep = \"{step}\"\nmin_size = \"{min_size}\"\n"
            ),
        )?;
    }
    tracing::info!(
        "recording {} Bullet markets to {}: {}",
        markets.len(),
        out.display(),
        markets.join(", ")
    );

    let (rows, mut incoming) = mpsc::unbounded_channel::<Row>();
    if !args.no_binance {
        tokio::spawn(record_binance(config.binance_symbol.clone(), rows.clone()));
    }
    for chunk in markets.chunks(MARKETS_PER_CONNECTION) {
        record_bullet(&client, chunk.to_vec(), rows.clone()).await?;
    }
    drop(rows);

    let mut open: HashMap<(Stream, String), Open> = HashMap::new();
    let mut counts: HashMap<String, u64> = HashMap::new();
    let deadline = args
        .minutes
        .map(|m| tokio::time::Instant::now() + Duration::from_secs(m * 60));
    let mut flush = tokio::time::interval(Duration::from_secs(1));
    let mut report = Instant::now();
    loop {
        tokio::select! {
            row = incoming.recv() => {
                let Some(row) = row else { break };
                let hour = row.recv_us / 3_600_000_000;
                let key = (row.stream, row.symbol.clone());
                // A new UTC hour starts a new file; the finished one is gzipped.
                if open.get(&key).is_some_and(|o| o.hour != hour)
                    && let Some(done) = open.remove(&key)
                {
                    finish(done)?;
                }
                if !open.contains_key(&key) {
                    let (prefix, header) = row.stream.file_prefix(&row.symbol);
                    let dir = out.join(&row.symbol);
                    std::fs::create_dir_all(&dir)?;
                    let path = dir.join(format!("{prefix}-{}.csv", stamp(row.recv_us)));
                    let writer = CsvWriter::create(&path, header)?;
                    open.insert(key.clone(), Open { hour, path, writer });
                }
                open.get_mut(&key).unwrap().writer.row(&row.line)?;
                *counts.entry(row.symbol).or_default() += 1;
            }
            _ = flush.tick() => {
                for o in open.values_mut() { o.writer.flush()?; }
                if report.elapsed() >= Duration::from_secs(300) {
                    report = Instant::now();
                    tracing::info!("rows so far: {counts:?}");
                }
            }
            _ = tokio::signal::ctrl_c() => break,
            _ = async { tokio::time::sleep_until(deadline.unwrap()).await }, if deadline.is_some() => break,
        }
    }
    // Finish (and gzip) whatever is open, so nothing is left half-written.
    for (_, o) in open.drain() {
        finish(o)?;
    }
    tracing::info!("stopped. rows recorded: {counts:?}");
    Ok(())
}

/// "config", "all" (minus `exclude`), or a comma-separated list.
async fn choose_markets(
    client: &Client,
    config: &Config,
    markets: &str,
    exclude: &[String],
) -> Result<Vec<String>> {
    Ok(match markets {
        "config" => vec![config.symbol.clone()],
        "all" => {
            let info = client
                .exchange_info()
                .await
                .context("listing Bullet markets")?
                .into_inner();
            let mut all: Vec<String> = info
                .symbols
                .iter()
                .map(|s| s.symbol.clone())
                .filter(|s| !exclude.contains(s))
                .collect();
            all.sort();
            all
        }
        list => list
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
    })
}

/// `2026-11-10T040712`: the UTC time of a file's first row, for its name.
fn stamp(us: u64) -> String {
    let secs = us / 1_000_000;
    let day = crate::fetch::date_string((secs / 86_400) as i64);
    let s = secs % 86_400;
    format!("{day}T{:02}{:02}{:02}", s / 3600, s / 60 % 60, s % 60)
}

/// Close a file and gzip it (`x.csv` -> `x.csv.gz`), deleting the plain one.
fn finish(mut o: Open) -> Result<()> {
    o.writer.flush()?;
    drop(o.writer);
    gzip(&o.path)
}

pub fn gzip(path: &Path) -> Result<()> {
    let data = std::fs::read(path)?;
    let gz_path = PathBuf::from(format!("{}.gz", path.display()));
    let tmp = PathBuf::from(format!("{}.gz.part", path.display()));
    let mut enc =
        flate2::write::GzEncoder::new(std::fs::File::create(&tmp)?, flate2::Compression::default());
    enc.write_all(&data)?;
    enc.finish()?;
    std::fs::rename(&tmp, &gz_path)?;
    std::fs::remove_file(path)?;
    Ok(())
}

fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as u64)
}

// ---------- Binance ----------

/// Messages on Binance's combined stream: `{"stream": "...", "data": {...}}`.
#[derive(Deserialize)]
struct Combined {
    stream: String,
    data: serde_json::Value,
}

#[derive(Deserialize)]
struct BinanceBookTicker {
    b: String,
    #[serde(rename = "B")]
    bq: String,
    a: String,
    #[serde(rename = "A")]
    aq: String,
    #[serde(rename = "T")]
    match_ms: u64,
}

#[derive(Deserialize)]
struct BinanceAggTrade {
    p: String,
    q: String,
    #[serde(rename = "T")]
    trade_ms: u64,
    m: bool,
}

fn binance_row(text: &str, symbol: &str, recv_us: u64) -> Result<Row> {
    let msg: Combined = serde_json::from_str(text)?;
    if msg.stream.ends_with("@bookTicker") {
        let t: BinanceBookTicker = serde_json::from_value(msg.data)?;
        let q = Quote {
            recv_us,
            exch_us: t.match_ms * 1000,
            bid: Decimal::from_str(&t.b)?,
            bid_qty: Decimal::from_str(&t.bq)?,
            ask: Decimal::from_str(&t.a)?,
            ask_qty: Decimal::from_str(&t.aq)?,
        };
        Ok(Row {
            stream: Stream::BinanceQuotes,
            symbol: symbol.to_string(),
            recv_us,
            line: q.csv(),
        })
    } else {
        let t: BinanceAggTrade = serde_json::from_value(msg.data)?;
        let tr = Trade {
            recv_us,
            exch_us: t.trade_ms * 1000,
            price: Decimal::from_str(&t.p)?,
            qty: Decimal::from_str(&t.q)?,
            buyer_is_maker: t.m,
        };
        Ok(Row {
            stream: Stream::BinanceTrades,
            symbol: symbol.to_string(),
            recv_us,
            line: tr.csv(),
        })
    }
}

/// Same connection handling as `binance.rs`: ping every second, treat 3 s of
/// silence as a dead socket, reconnect with backoff.
async fn record_binance(symbol: String, rows: Rows) {
    let s = symbol.to_lowercase();
    let url = format!("wss://fstream.binance.com/stream?streams={s}@bookTicker/{s}@aggTrade");
    let dead_after = Duration::from_secs(3);
    let mut backoff = Duration::from_secs(1);
    loop {
        match tokio::time::timeout(dead_after, connect_async(&url)).await {
            Ok(Ok((mut ws, _))) => {
                tracing::info!("binance: connected");
                backoff = Duration::from_secs(1);
                let mut ping = tokio::time::interval(Duration::from_secs(1));
                let mut last_heard = Instant::now();
                loop {
                    let msg = tokio::select! {
                        msg = ws.next() => match msg { Some(m) => m, None => break },
                        _ = ping.tick() => {
                            if last_heard.elapsed() > dead_after { break; }
                            if ws.send(Message::Ping(Vec::new().into())).await.is_err() { break; }
                            continue;
                        }
                    };
                    last_heard = Instant::now();
                    match msg {
                        Ok(Message::Text(text)) => match binance_row(&text, &symbol, now_us()) {
                            Ok(row) => {
                                if rows.send(row).is_err() {
                                    return;
                                }
                            }
                            Err(e) => tracing::warn!("binance: bad message: {e}"),
                        },
                        Ok(Message::Ping(p)) => {
                            if ws.send(Message::Pong(p)).await.is_err() {
                                break;
                            }
                        }
                        Ok(Message::Close(_)) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
                tracing::warn!("binance: disconnected, reconnecting (gap in the data)");
            }
            Ok(Err(e)) => tracing::warn!("binance: connect failed: {e}"),
            Err(_) => tracing::warn!("binance: connect timed out"),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

// ---------- Bullet ----------

/// One websocket for these markets: each market's book and trades.
async fn record_bullet(client: &Client, symbols: Vec<String>, rows: Rows) -> Result<()> {
    let config = ManagedWsConfig::builder()
        .idle_timeout(Duration::from_secs(3))
        .build();
    let mut ws = client
        .connect_ws_managed()
        .config(config)
        .call()
        .await
        .context("opening Bullet websocket")?;
    let topics: Vec<Topic> = symbols
        .iter()
        .flat_map(|s| {
            [
                Topic::depth(s.as_str(), OrderbookDepth::D20),
                Topic::agg_trade(s.as_str()),
            ]
        })
        .collect();
    ws.subscribe(topics, None).context("subscribing")?;
    tokio::spawn(async move {
        // Our copy of each market's book.
        let mut books: HashMap<String, BookKeeper> = symbols
            .iter()
            .map(|s| (s.clone(), BookKeeper::default()))
            .collect();
        let mut ping = tokio::time::interval(Duration::from_secs(1));
        loop {
            let event = tokio::select! {
                event = ws.recv() => event,
                _ = ping.tick() => { let _ = ws.ping(None); continue; }
            };
            let recv_us = now_us();
            let row = match event {
                None => {
                    tracing::error!("bullet: websocket closed for good ({})", symbols.join(","));
                    return;
                }
                Some(WsEvent::Reconnecting) => {
                    tracing::warn!(
                        "bullet: reconnecting (gap in the data for {})",
                        symbols.join(",")
                    );
                    books.values_mut().for_each(BookKeeper::reset);
                    None
                }
                Some(WsEvent::Disconnected(why)) => {
                    tracing::error!("bullet: disconnected: {why}");
                    return;
                }
                Some(WsEvent::Message(msg)) => match *msg {
                    ServerMessage::DepthUpdate(depth) => match books.get_mut(&depth.symbol) {
                        Some(book) => match book.on_message(&depth) {
                            BookEvent::Updated(b) => {
                                top_of_book(&depth.symbol, &b, recv_us, depth.transaction_time)
                            }
                            BookEvent::Lost => {
                                tracing::warn!(
                                    "bullet: {} book out of sync, waiting for a snapshot",
                                    depth.symbol
                                );
                                None
                            }
                            BookEvent::None => None,
                        },
                        None => None,
                    },
                    ServerMessage::AggTrade(t) if books.contains_key(&t.symbol) => {
                        match (Decimal::from_str(&t.price), Decimal::from_str(&t.quantity)) {
                            (Ok(price), Ok(qty)) => Some(Row {
                                stream: Stream::BulletTrades,
                                symbol: t.symbol.clone(),
                                recv_us,
                                line: Trade {
                                    recv_us,
                                    exch_us: t.trade_time,
                                    price,
                                    qty,
                                    buyer_is_maker: t.is_buyer_maker,
                                }
                                .csv(),
                            }),
                            _ => {
                                tracing::warn!("bullet: unreadable trade {t:?}");
                                None
                            }
                        }
                    }
                    _ => None,
                },
            };
            if let Some(row) = row
                && rows.send(row).is_err()
            {
                return;
            }
        }
    });
    Ok(())
}

/// One row per book update, even when the top didn't change: the backtest
/// uses these to know the book is alive (the bot pulls quotes after
/// `book_stale_secs` of silence).
fn top_of_book(symbol: &str, book: &crate::book::Book, recv_us: u64, exch_us: u64) -> Option<Row> {
    let (bid, ask) = (book.best_bid()?, book.best_ask()?);
    let q = Quote {
        recv_us,
        exch_us,
        bid,
        bid_qty: book.bids[&bid],
        ask,
        ask_qty: book.asks[&ask],
    };
    Some(Row {
        stream: Stream::BulletQuotes,
        symbol: symbol.to_string(),
        recv_us,
        line: q.csv(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_sort_in_time_order() {
        // 2026-11-10 04:07:12 UTC and a second later.
        let t = 1_794_283_632_000_000;
        assert_eq!(stamp(t), "2026-11-10T040712");
        assert!(stamp(t) < stamp(t + 1_000_000));
    }

    #[test]
    fn gzip_replaces_the_plain_file() {
        let dir = std::env::temp_dir().join(format!("hft-rec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("x.csv");
        std::fs::write(&p, "a,b\n1,2\n").unwrap();
        gzip(&p).unwrap();
        assert!(!p.exists());
        let mut text = String::new();
        std::io::Read::read_to_string(
            &mut flate2::read::GzDecoder::new(std::fs::File::open(dir.join("x.csv.gz")).unwrap()),
            &mut text,
        )
        .unwrap();
        assert_eq!(text, "a,b\n1,2\n");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn parses_binance_combined_stream_messages() {
        let bt = r#"{"stream":"solusdt@bookTicker","data":{"e":"bookTicker","u":1,"s":"SOLUSDT","b":"150.10","B":"20","a":"150.12","A":"7","T":1700000000000,"E":1700000000001}}"#;
        let row = binance_row(bt, "SOLUSDT", 5).unwrap();
        assert_eq!(row.stream, Stream::BinanceQuotes);
        assert_eq!(row.line, "5,1700000000000000,150.10,20,150.12,7");
        let at = r#"{"stream":"solusdt@aggTrade","data":{"e":"aggTrade","E":1,"s":"SOLUSDT","a":9,"p":"150.11","q":"0.5","f":1,"l":2,"T":1700000000000,"m":true}}"#;
        let row = binance_row(at, "SOLUSDT", 5).unwrap();
        assert_eq!(row.stream, Stream::BinanceTrades);
        assert_eq!(row.line, "5,1700000000000000,150.11,0.5,true");
    }
}
