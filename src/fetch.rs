//! Download Binance's public history (data.binance.vision) and convert it to
//! our CSVs (see `marketdata.rs`).
//!
//! Two daily datasets for the USD-M perp:
//! - `bookTicker`: best bid/ask after every change. Exactly what the live bot
//!   listens to, but Binance stopped publishing it after 2024-03-30.
//! - `aggTrades`: every trade, still published daily (up to yesterday).

use std::io::{BufRead, BufReader, Cursor, Read};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use rust_decimal::Decimal;

use crate::marketdata::{CsvWriter, QUOTES_HEADER, Quote, TRADES_HEADER, Trade, parse_bool};

const BASE: &str = "https://data.binance.vision/data/futures/um/daily";
/// The S3 bucket behind data.binance.vision, for networks that block the main host.
const MIRROR: &str =
    "https://s3-ap-northeast-1.amazonaws.com/data.binance.vision/data/futures/um/daily";

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Dataset {
    BookTicker,
    AggTrades,
}

impl Dataset {
    fn remote_name(self) -> &'static str {
        match self {
            Dataset::BookTicker => "bookTicker",
            Dataset::AggTrades => "aggTrades",
        }
    }

    fn local_name(self) -> &'static str {
        match self {
            Dataset::BookTicker => "quotes",
            Dataset::AggTrades => "trades",
        }
    }
}

/// Fetch `dataset` for every day from `from` to `to` (inclusive, YYYY-MM-DD).
pub async fn run(
    symbol: &str,
    dataset: Dataset,
    from: &str,
    to: &str,
    out: &Path,
    mirror: bool,
) -> Result<()> {
    let http = reqwest::Client::new();
    let base = if mirror { MIRROR } else { BASE };
    for day in days(from, to)? {
        let target = out.join(format!(
            "binance-{symbol}-{}-{day}.csv",
            dataset.local_name()
        ));
        if target.exists() {
            tracing::info!("{} already there, skipping", target.display());
            continue;
        }
        let name = dataset.remote_name();
        let url = format!("{base}/{name}/{symbol}/{symbol}-{name}-{day}.zip");
        tracing::info!("downloading {url}");
        let response = http.get(&url).send().await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            tracing::warn!("{day}: not published (bookTicker stops after 2024-03-30)");
            continue;
        }
        let bytes = response.error_for_status()?.bytes().await?;
        let rows = convert(dataset, &bytes, &tmp_path(&target))?;
        std::fs::rename(tmp_path(&target), &target)?;
        tracing::info!("{}: {rows} rows", target.display());
    }
    Ok(())
}

fn tmp_path(target: &Path) -> PathBuf {
    target.with_extension("csv.part")
}

/// Unzip one day and rewrite it in our format. Returns the row count.
fn convert(dataset: Dataset, zip_bytes: &[u8], out: &Path) -> Result<usize> {
    let mut archive = zip::ZipArchive::new(Cursor::new(zip_bytes)).context("opening zip")?;
    let file = archive.by_index(0)?;
    let header = match dataset {
        Dataset::BookTicker => QUOTES_HEADER,
        Dataset::AggTrades => TRADES_HEADER,
    };
    let _ = std::fs::remove_file(out);
    let mut writer = CsvWriter::create(out, header)?;
    let rows = convert_lines(dataset, BufReader::new(file), |line| writer.row(&line))?;
    writer.flush()?;
    Ok(rows)
}

fn convert_lines(
    dataset: Dataset,
    input: impl Read,
    mut emit: impl FnMut(String) -> Result<()>,
) -> Result<usize> {
    let mut rows = 0;
    for line in BufReader::new(input).lines() {
        let line = line?;
        // Older files have no header; newer ones do. Skip anything not numeric.
        if !line.starts_with(|c: char| c.is_ascii_digit()) {
            continue;
        }
        emit(match dataset {
            Dataset::BookTicker => book_ticker_row(&line)?.csv(),
            Dataset::AggTrades => agg_trade_row(&line)?.csv(),
        })?;
        rows += 1;
    }
    Ok(rows)
}

/// `update_id,best_bid_price,best_bid_qty,best_ask_price,best_ask_qty,transaction_time,event_time` (ms)
fn book_ticker_row(line: &str) -> Result<Quote> {
    let f: Vec<&str> = line.split(',').collect();
    if f.len() < 7 {
        bail!("bookTicker row has {} fields: {line}", f.len());
    }
    let exch_us = f[5].parse::<u64>()? * 1000;
    Ok(Quote {
        recv_us: f[6].parse::<u64>()? * 1000,
        exch_us,
        bid: Decimal::from_str(f[1])?,
        bid_qty: Decimal::from_str(f[2])?,
        ask: Decimal::from_str(f[3])?,
        ask_qty: Decimal::from_str(f[4])?,
    })
}

/// `agg_trade_id,price,quantity,first_trade_id,last_trade_id,transact_time,is_buyer_maker` (ms)
fn agg_trade_row(line: &str) -> Result<Trade> {
    let f: Vec<&str> = line.split(',').collect();
    if f.len() < 7 {
        bail!("aggTrades row has {} fields: {line}", f.len());
    }
    let us = f[5].parse::<u64>()? * 1000;
    Ok(Trade {
        recv_us: us,
        exch_us: us,
        price: Decimal::from_str(f[1])?,
        qty: Decimal::from_str(f[2])?,
        buyer_is_maker: parse_bool(f[6])?,
    })
}

/// Every date from `from` to `to`, as YYYY-MM-DD.
fn days(from: &str, to: &str) -> Result<Vec<String>> {
    let (start, end) = (day_number(from)?, day_number(to)?);
    if end < start {
        bail!("--to is before --from");
    }
    Ok((start..=end).map(date_string).collect())
}

/// Days since 1970-01-01 (Howard Hinnant's civil-date algorithm).
fn day_number(date: &str) -> Result<i64> {
    let p: Vec<i64> = date
        .split('-')
        .map(|s| s.parse())
        .collect::<Result<_, _>>()
        .with_context(|| format!("bad date {date:?}, want YYYY-MM-DD"))?;
    let [y, m, d] = p[..] else {
        bail!("bad date {date:?}, want YYYY-MM-DD")
    };
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Ok(era * 146_097 + doe - 719_468)
}

pub fn date_string(z: i64) -> String {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walks_dates_across_month_and_leap_day() {
        assert_eq!(
            days("2024-02-28", "2024-03-01").unwrap(),
            ["2024-02-28", "2024-02-29", "2024-03-01"]
        );
        assert_eq!(day_number("1970-01-01").unwrap(), 0);
    }

    #[test]
    fn converts_archive_rows_with_and_without_header() {
        let book = "update_id,best_bid_price,best_bid_qty,best_ask_price,best_ask_qty,transaction_time,event_time\n\
                    1,150.10,20,150.12,7,1700000000000,1700000000005\n";
        let mut out = Vec::new();
        convert_lines(Dataset::BookTicker, book.as_bytes(), |l| {
            out.push(l);
            Ok(())
        })
        .unwrap();
        assert_eq!(
            out,
            ["1700000000005000,1700000000000000,150.10,20,150.12,7"]
        );

        let trades = "5,150.11,0.5,9,10,1700000000000,true\n";
        let mut out = Vec::new();
        convert_lines(Dataset::AggTrades, trades.as_bytes(), |l| {
            out.push(l);
            Ok(())
        })
        .unwrap();
        assert_eq!(out, ["1700000000000000,1700000000000000,150.11,0.5,true"]);
    }
}
