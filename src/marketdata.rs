//! Market data on disk: the files `fetch-binance` and `record` write and
//! `backtest` reads.
//!
//! Two CSV shapes, the same for both venues:
//!
//! ```text
//! quotes: recv_us,exch_us,bid,bid_qty,ask,ask_qty     best bid/ask after every change
//! trades: recv_us,exch_us,price,qty,buyer_is_maker    every public trade
//! ```
//!
//! `recv_us` is when the message reached us (µs since 1970), which is what
//! the bot reacts to. `exch_us` is the venue's own timestamp. Files from
//! Binance's archive have no receive time, so there `recv_us = exch_us`.
//! `buyer_is_maker = true` means the seller crossed the spread (a sell hit
//! the bids).

use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use rust_decimal::Decimal;

pub const QUOTES_HEADER: &str = "recv_us,exch_us,bid,bid_qty,ask,ask_qty";
pub const TRADES_HEADER: &str = "recv_us,exch_us,price,qty,buyer_is_maker";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quote {
    pub recv_us: u64,
    pub exch_us: u64,
    pub bid: Decimal,
    pub bid_qty: Decimal,
    pub ask: Decimal,
    pub ask_qty: Decimal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trade {
    pub recv_us: u64,
    pub exch_us: u64,
    pub price: Decimal,
    pub qty: Decimal,
    pub buyer_is_maker: bool,
}

impl Quote {
    pub fn csv(&self) -> String {
        format!(
            "{},{},{},{},{},{}",
            self.recv_us, self.exch_us, self.bid, self.bid_qty, self.ask, self.ask_qty
        )
    }

    pub fn parse(line: &str) -> Result<Self> {
        let f: Vec<&str> = line.trim().split(',').collect();
        if f.len() != 6 {
            bail!("expected 6 fields, got {}", f.len());
        }
        Ok(Self {
            recv_us: f[0].parse()?,
            exch_us: f[1].parse()?,
            bid: Decimal::from_str(f[2])?,
            bid_qty: Decimal::from_str(f[3])?,
            ask: Decimal::from_str(f[4])?,
            ask_qty: Decimal::from_str(f[5])?,
        })
    }
}

impl Trade {
    pub fn csv(&self) -> String {
        format!(
            "{},{},{},{},{}",
            self.recv_us, self.exch_us, self.price, self.qty, self.buyer_is_maker
        )
    }

    pub fn parse(line: &str) -> Result<Self> {
        let f: Vec<&str> = line.trim().split(',').collect();
        if f.len() != 5 {
            bail!("expected 5 fields, got {}", f.len());
        }
        Ok(Self {
            recv_us: f[0].parse()?,
            exch_us: f[1].parse()?,
            price: Decimal::from_str(f[2])?,
            qty: Decimal::from_str(f[3])?,
            buyer_is_maker: parse_bool(f[4])?,
        })
    }
}

pub fn parse_bool(s: &str) -> Result<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        other => bail!("not a boolean: {other:?}"),
    }
}

/// Appends rows to a CSV, writing the header if the file is new.
pub struct CsvWriter {
    out: BufWriter<File>,
}

impl CsvWriter {
    pub fn create(path: &Path, header: &str) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let is_new = !path.exists();
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        let mut out = BufWriter::new(file);
        if is_new {
            writeln!(out, "{header}")?;
        }
        Ok(Self { out })
    }

    pub fn row(&mut self, line: &str) -> Result<()> {
        writeln!(self.out, "{line}")?;
        Ok(())
    }

    pub fn flush(&mut self) -> Result<()> {
        self.out.flush()?;
        Ok(())
    }
}

/// Reads one of our CSVs row by row, across several files in order. Rows
/// are parsed lazily so a day of data never has to fit in memory.
pub struct CsvReader<T> {
    files: std::vec::IntoIter<PathBuf>,
    lines: Option<(PathBuf, std::io::Lines<BufReader<File>>)>,
    parse: fn(&str) -> Result<T>,
}

impl<T> CsvReader<T> {
    pub fn open(mut files: Vec<PathBuf>, parse: fn(&str) -> Result<T>) -> Self {
        files.sort(); // our file names sort by date
        Self {
            files: files.into_iter(),
            lines: None,
            parse,
        }
    }
}

impl<T> Iterator for CsvReader<T> {
    type Item = Result<T>;

    fn next(&mut self) -> Option<Result<T>> {
        loop {
            if let Some((path, lines)) = &mut self.lines {
                match lines.next() {
                    Some(Ok(line)) => {
                        if line.is_empty() || line.starts_with("recv_us") {
                            continue; // blank or header
                        }
                        return Some(
                            (self.parse)(&line)
                                .with_context(|| format!("{}: {line}", path.display())),
                        );
                    }
                    Some(Err(e)) => return Some(Err(e.into())),
                    None => self.lines = None,
                }
            }
            let path = self.files.next()?;
            match File::open(&path) {
                Ok(f) => self.lines = Some((path, BufReader::new(f).lines())),
                Err(e) => return Some(Err(anyhow::anyhow!("opening {}: {e}", path.display()))),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_survive_a_round_trip() {
        let q = Quote {
            recv_us: 2,
            exch_us: 1,
            bid: Decimal::new(15010, 2),
            bid_qty: Decimal::new(3, 0),
            ask: Decimal::new(15012, 2),
            ask_qty: Decimal::new(4, 1),
        };
        assert_eq!(Quote::parse(&q.csv()).unwrap(), q);
        let t = Trade {
            recv_us: 2,
            exch_us: 1,
            price: Decimal::new(15011, 2),
            qty: Decimal::new(5, 2),
            buyer_is_maker: true,
        };
        assert_eq!(Trade::parse(&t.csv()).unwrap(), t);
    }

    #[test]
    fn reads_across_files_skipping_headers() {
        let dir = std::env::temp_dir().join(format!("hft-md-{}", std::process::id()));
        let (a, b) = (dir.join("a.csv"), dir.join("b.csv"));
        let mut w = CsvWriter::create(&a, TRADES_HEADER).unwrap();
        w.row("1,1,100,1,true").unwrap();
        w.flush().unwrap();
        let mut w = CsvWriter::create(&b, TRADES_HEADER).unwrap();
        w.row("2,2,101,1,false").unwrap();
        w.flush().unwrap();
        let rows: Vec<Trade> = CsvReader::open(vec![b, a], Trade::parse)
            .collect::<Result<_>>()
            .unwrap();
        assert_eq!(rows.iter().map(|t| t.recv_us).collect::<Vec<_>>(), [1, 2]);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
