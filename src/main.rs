//! HFT for dummies: a market maker on Bullet that borrows its price from Binance.
//!
//! Start reading at `events.rs` (the vocabulary), then `bot.rs` (the loop),
//! `market_maker.rs` (the decisions) and `strategy.rs` (the prices).

mod backtest;
mod binance;
mod book;
mod bot;
mod bullet;
mod config;
mod events;
mod executor;
mod fair_value;
mod fetch;
mod inventory;
mod key;
mod market_maker;
mod marketdata;
mod record;
mod strategy;
mod telemetry;

use std::path::PathBuf;

use anyhow::{Result, bail};
use bullet_rust_sdk::Keypair;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(about = "A simple market maker for the HFT for dummies series")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// Trade with real money, whatever `dry_run` says in the config.
    #[arg(long, global = true)]
    live: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Run the bot (the default).
    Run {
        #[arg(default_value = "bot.toml")]
        config: PathBuf,
    },
    /// Make a new trading key for the bot, to delegate to in the Bullet app.
    Keygen {
        #[arg(default_value = "bot.key")]
        path: PathBuf,
        #[arg(long, default_value = "mainnet")]
        network: String,
    },
    /// Download Binance history from data.binance.vision into our CSV format.
    FetchBinance {
        /// The Binance USD-M perp, e.g. SOLUSDT.
        #[arg(long, default_value = "SOLUSDT")]
        symbol: String,
        /// book-ticker (best bid/ask; published up to 2024-03-30) or agg-trades.
        #[arg(long, value_enum)]
        dataset: fetch::Dataset,
        /// First day, YYYY-MM-DD.
        #[arg(long)]
        from: String,
        /// Last day, YYYY-MM-DD (inclusive).
        #[arg(long)]
        to: String,
        #[arg(long, default_value = "data")]
        out: PathBuf,
        /// Download from the S3 bucket behind data.binance.vision instead.
        #[arg(long)]
        mirror: bool,
    },
    /// Record live Binance and Bullet market data to CSV, for backtesting.
    Record {
        #[arg(long, default_value = "bot.toml")]
        config: PathBuf,
        #[arg(long, default_value = "data")]
        out: PathBuf,
        /// Stop after this many minutes (default: until Ctrl-C).
        #[arg(long)]
        minutes: Option<u64>,
    },
    /// Replay recorded data through the market maker with a simulated exchange.
    Backtest(backtest::Args),
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    // A backtest replays hours in seconds: only warnings, unless RUST_LOG says otherwise.
    let default_level = match cli.command {
        Some(Command::Backtest(_)) => "warn",
        _ => "info",
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| default_level.into()),
        )
        // Colours only in a terminal, not in a server's log files.
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .init();

    match cli.command.unwrap_or(Command::Run {
        config: "bot.toml".into(),
    }) {
        Command::Run { config } => bot::run(&config, cli.live).await,
        Command::Keygen { path, network } => keygen(&path, &network),
        Command::FetchBinance {
            symbol,
            dataset,
            from,
            to,
            out,
            mirror,
        } => fetch::run(&symbol, dataset, &from, &to, &out, mirror).await,
        Command::Record {
            config,
            out,
            minutes,
        } => record::run(&config, &out, minutes).await,
        Command::Backtest(args) => backtest::run(&args),
    }
}

fn keygen(path: &PathBuf, network: &str) -> Result<()> {
    if path.exists() {
        bail!("{} already exists; not overwriting a key", path.display());
    }
    let keypair = Keypair::generate();
    keypair.write_to_file(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Only you can read it.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    let address = keypair.address();
    println!(
        "Wrote a new key to {}. Keep it secret; never commit it.\n",
        path.display()
    );
    println!("Its address: {address}\n");
    println!("Next: open this link (logged in to Bullet) and click Confirm Delegation:");
    println!("  {}", bullet::delegate_link(network, &address));
    Ok(())
}
