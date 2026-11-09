//! Record live market data to disk, for backtesting.
//!
//! Neither venue gives us the history we need: Binance's archive stopped
//! publishing `bookTicker` in 2024, and Bullet's API only returns its last
//! 1000 trades. So we record it ourselves: Binance best bid/ask and trades,
//! Bullet's order book (top of book after every update) and trades. Files
//! are `marketdata.rs` CSVs, one per stream per UTC day:
//!
//! ```text
//! data/binance-SOLUSDT-quotes-2026-11-02.csv   data/bullet-SOL-USD-quotes-2026-11-02.csv
//! data/binance-SOLUSDT-trades-2026-11-02.csv   data/bullet-SOL-USD-trades-2026-11-02.csv
//! data/bullet-SOL-USD-meta.toml                (tick and lot size, for the backtest)
//! ```
//!
//! Every row is stamped with when *we* received it, so the backtest sees the
//! data with the same delays the live bot would. Run it on the same machine
//! (ideally the Tokyo box from Part 4) for days; Ctrl-C stops it cleanly.

use std::collections::HashMap;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Stream {
    BinanceQuotes,
    BinanceTrades,
    BulletQuotes,
    BulletTrades,
}

struct Row {
    stream: Stream,
    recv_us: u64,
    line: String,
}

type Rows = mpsc::UnboundedSender<Row>;

pub async fn run(config_path: &Path, out: &Path, minutes: Option<u64>) -> Result<()> {
    let config = Config::load(config_path)?;
    std::fs::create_dir_all(out)?;

    let client = Client::builder()
        .network(match &config.api_url {
            Some(url) => Network::Custom(url.clone()),
            None => Network::from(config.network.as_str()),
        })
        .build()
        .await
        .context("connecting to Bullet")?;
    let (tick, step, min_size) = crate::bullet::market_rules(&client, &config.symbol).await?;
    let meta = out.join(format!("bullet-{}-meta.toml", config.symbol));
    std::fs::write(
        &meta,
        format!(
            "symbol = \"{}\"\ntick = \"{tick}\"\nstep = \"{step}\"\nmin_size = \"{min_size}\"\n",
            config.symbol
        ),
    )?;
    tracing::info!(%tick, %step, %min_size, "recording {} and {} to {}", config.binance_symbol, config.symbol, out.display());

    let (rows, mut incoming) = mpsc::unbounded_channel::<Row>();
    tokio::spawn(record_binance(config.binance_symbol.clone(), rows.clone()));
    record_bullet(&client, &config.symbol, rows.clone()).await?;
    drop(rows);

    let names = HashMap::from([
        (
            Stream::BinanceQuotes,
            (
                format!("binance-{}-quotes", config.binance_symbol),
                QUOTES_HEADER,
            ),
        ),
        (
            Stream::BinanceTrades,
            (
                format!("binance-{}-trades", config.binance_symbol),
                TRADES_HEADER,
            ),
        ),
        (
            Stream::BulletQuotes,
            (format!("bullet-{}-quotes", config.symbol), QUOTES_HEADER),
        ),
        (
            Stream::BulletTrades,
            (format!("bullet-{}-trades", config.symbol), TRADES_HEADER),
        ),
    ]);
    let mut writers: HashMap<Stream, (String, CsvWriter)> = HashMap::new();
    let mut counts: HashMap<Stream, u64> = HashMap::new();
    let deadline = minutes.map(|m| tokio::time::Instant::now() + Duration::from_secs(m * 60));
    let mut flush = tokio::time::interval(Duration::from_secs(1));
    let mut report = Instant::now();
    loop {
        tokio::select! {
            row = incoming.recv() => {
                let Some(row) = row else { break };
                let day = crate::fetch::date_string((row.recv_us / 86_400_000_000) as i64);
                let (name, header) = &names[&row.stream];
                // A new UTC day starts a new file.
                if writers.get(&row.stream).is_none_or(|(d, _)| *d != day) {
                    let path: PathBuf = out.join(format!("{name}-{day}.csv"));
                    if let Some((_, mut old)) = writers.remove(&row.stream) {
                        old.flush()?;
                    }
                    writers.insert(row.stream, (day, CsvWriter::create(&path, header)?));
                }
                writers.get_mut(&row.stream).unwrap().1.row(&row.line)?;
                *counts.entry(row.stream).or_default() += 1;
            }
            _ = flush.tick() => {
                for (_, w) in writers.values_mut() { w.flush()?; }
                if report.elapsed() >= Duration::from_secs(60) {
                    report = Instant::now();
                    tracing::info!("rows so far: {counts:?}");
                }
            }
            _ = tokio::signal::ctrl_c() => break,
            _ = async { tokio::time::sleep_until(deadline.unwrap()).await }, if deadline.is_some() => break,
        }
    }
    for (_, w) in writers.values_mut() {
        w.flush()?;
    }
    tracing::info!("stopped. rows recorded: {counts:?}");
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

fn binance_row(text: &str, recv_us: u64) -> Result<Row> {
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
                        Ok(Message::Text(text)) => match binance_row(&text, now_us()) {
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

async fn record_bullet(client: &Client, symbol: &str, rows: Rows) -> Result<()> {
    let config = ManagedWsConfig::builder()
        .idle_timeout(Duration::from_secs(3))
        .build();
    let mut ws = client
        .connect_ws_managed()
        .config(config)
        .call()
        .await
        .context("opening Bullet websocket")?;
    ws.subscribe(
        vec![
            Topic::depth(symbol, OrderbookDepth::D20),
            Topic::agg_trade(symbol),
        ],
        None,
    )
    .context("subscribing")?;
    let symbol = symbol.to_string();
    tokio::spawn(async move {
        let mut book = BookKeeper::default();
        let mut ping = tokio::time::interval(Duration::from_secs(1));
        loop {
            let event = tokio::select! {
                event = ws.recv() => event,
                _ = ping.tick() => { let _ = ws.ping(None); continue; }
            };
            let recv_us = now_us();
            let row = match event {
                None => {
                    tracing::error!("bullet: websocket closed for good");
                    return;
                }
                Some(WsEvent::Reconnecting) => {
                    tracing::warn!("bullet: reconnecting (gap in the data)");
                    book.reset();
                    None
                }
                Some(WsEvent::Disconnected(why)) => {
                    tracing::error!("bullet: disconnected: {why}");
                    return;
                }
                Some(WsEvent::Message(msg)) => match *msg {
                    ServerMessage::DepthUpdate(depth) if depth.symbol == symbol => {
                        match book.on_message(&depth) {
                            BookEvent::Updated(b) => {
                                top_of_book(&b, recv_us, depth.transaction_time)
                            }
                            BookEvent::Lost => {
                                tracing::warn!("bullet: book out of sync, waiting for a snapshot");
                                None
                            }
                            BookEvent::None => None,
                        }
                    }
                    ServerMessage::AggTrade(t) if t.symbol == symbol => {
                        match (Decimal::from_str(&t.price), Decimal::from_str(&t.quantity)) {
                            (Ok(price), Ok(qty)) => Some(Row {
                                stream: Stream::BulletTrades,
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
fn top_of_book(book: &crate::book::Book, recv_us: u64, exch_us: u64) -> Option<Row> {
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
        recv_us,
        line: q.csv(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_binance_combined_stream_messages() {
        let bt = r#"{"stream":"solusdt@bookTicker","data":{"e":"bookTicker","u":1,"s":"SOLUSDT","b":"150.10","B":"20","a":"150.12","A":"7","T":1700000000000,"E":1700000000001}}"#;
        let row = binance_row(bt, 5).unwrap();
        assert_eq!(row.stream, Stream::BinanceQuotes);
        assert_eq!(row.line, "5,1700000000000000,150.10,20,150.12,7");
        let at = r#"{"stream":"solusdt@aggTrade","data":{"e":"aggTrade","E":1,"s":"SOLUSDT","a":9,"p":"150.11","q":"0.5","f":1,"l":2,"T":1700000000000,"m":true}}"#;
        let row = binance_row(at, 5).unwrap();
        assert_eq!(row.stream, Stream::BinanceTrades);
        assert_eq!(row.line, "5,1700000000000000,150.11,0.5,true");
    }
}
