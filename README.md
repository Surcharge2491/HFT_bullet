# HFT_bullet

A SOL-USD market maker on [Bullet](https://app.bullet.xyz), priced off Binance's
SOL perp, plus the tools to backtest it: `fetch-binance` (Binance archive),
`record` (live Binance and Bullet market data) and `backtest` (replays history
through the bot's unchanged `MarketMaker` with a simulated exchange).

Based on [bulletxyz/hft-for-dummies](https://github.com/bulletxyz/hft-for-dummies)
(Apache-2.0, upstream commit `11ba8eb`). The original README follows.

Work is tracked as cards moving from idea to execution: see
[`docs/pipeline.md`](docs/pipeline.md).

`bullet-data/` holds Bullet SOL-USD recordings from `record` (top of book after
every update, and every public trade), one CSV per UTC day, plus the market's
tick and lot size in `bullet-SOL-USD-meta.toml`. Bullet has no public market
history, so these files are the only Bullet history this project has.

---

## HFT for dummies

Code for the *HFT for dummies* series: a market maker that quotes SOL-USD on
[Bullet](https://app.bullet.xyz/trade?r=HFT4DUMMIES), priced off Binance's SOL
perpetual future (perp). Each post is one change to this repo.

> Market making can lose money. The bot starts in **dry run**: no key, no
> orders.

## What we're building

```mermaid
flowchart LR
    BIN["<b>Binance</b><br/>market data WS<br/><i>Parts 4, 5, 10</i>"]
    subgraph BOT["Your bot"]
        direction LR
        RX["<b>Receive + parse</b><br/>typed events<br/><i>Parts 5, 9</i>"]
        FAIR["<b>Fair price</b><br/>Binance mid + basis<br/><i>Part 2</i>"]
        STRAT["<b>Strategy</b><br/>quotes + inventory skew<br/><i>Parts 2, 9</i>"]
        EXEC["<b>Executor</b><br/>non-blocking submit<br/><i>Parts 6, 7, 11</i>"]
    end
    BUL["<b>Bullet</b><br/>order API (HTTPS)<br/><i>Parts 4, 10</i>"]
    BOOK["<b>Bullet book</b><br/>venue mid / trades<br/><i>Part 5</i>"]
    FILLS["<b>Acks + fills</b><br/>event listener<br/><i>Parts 5, 6</i>"]
    STATE["<b>Inventory / state</b><br/>feeds Strategy<br/><i>Part 2</i>"]

    BIN -- WS --> RX --> FAIR --> STRAT --> EXEC -- orders --> BUL
    BUL --> BOOK
    BOOK -. "book WS (secondary)" .-> RX
    BUL -- "acks / fills" --> FILLS --> STATE -- state --> STRAT

    classDef ref stroke:#58a6ff,stroke-width:2px
    classDef venue stroke:#3fb950,stroke-width:2px
    classDef bot stroke:#bc8cff,stroke-width:2px
    classDef state stroke:#d29922,stroke-width:2px
    class BIN,RX,FAIR ref
    class BUL,BOOK,FILLS venue
    class STRAT,EXEC bot
    class STATE state
```

Binance moves, we reprice, requote on Bullet, and fills feed back into
inventory. The series makes that loop fast: from a Binance update to our new
order resting on Bullet.

## Run it

Needs [Rust](https://rust-lang.org/install.html). Tested on Mac and Linux.

```sh
cargo run
```

After a minute of warm-up it quotes, on paper:

```text
quoting
paper: place side=Buy price=100.556 size=0.05
paper: place side=Sell price=100.758 size=0.05
bullet mid 100.656 | fair 100.6573 | our bid 100.556 ask 100.758 | position 0 | pnl $0 (fees $0)
```

Settings are in `bot.toml`. Using a coding agent? Ask it to *"set me up and
run the bot"*; `AGENTS.md` has its instructions.

## Going live

1. **Deposit $20 USDC** on [Bullet](https://app.bullet.xyz/trade?r=HFT4DUMMIES)
   (referral code `HFT4DUMMIES`).
2. **Make the bot a key:** `cargo run -- keygen`. It writes `bot.key`
   (git-ignored) and prints a link to
   [Delegate Accounts](https://app.bullet.xyz/delegate-accounts) with the
   key filled in: click **Confirm Delegation**. A delegate can trade but
   never withdraw.
3. **Run it live:** `cargo run -- --live`. Two orders appear either side of
   the price, marked ▸ in the app's order book (set its
   granularity to 0.1 or finer). Ctrl-C cancels them.

Limits in `bot.toml`: at most 0.1 SOL held, stops after a $2 loss. Stopping
doesn't close your position, and the bot cancels any SOL-USD orders you
place by hand on the same account.

## Backtesting

Extra commands replay history through the same `MarketMaker` the live bot
runs. Only the clock and the exchange are simulated. Use `backtest.toml`: the
same strategy as `bot.toml`, minus the $2 loss stop, so long runs show every
day.

```sh
# 1. Binance history (data.binance.vision; --mirror uses its S3 bucket if the main host is blocked)
cargo run --release -- fetch-binance --market perp --symbol SOLUSDT --dataset agg-trades --from 2026-08-09 --to 2026-11-09
cargo run --release -- fetch-binance --market spot --symbol SOLUSDC --dataset agg-trades --from 2026-11-09 --to 2026-11-09

# 2. Bullet history: there is none to download, so record it (until Ctrl-C).
#    All markets except the blue chips (BTC, ETH, XRP, DOGE); add --no-binance where Binance answers 403.
cargo run --release -- record --markets all
scripts/sync-recordings.sh     # copy finished hours into bullet-data/ (then commit)

# 3a. REAL backtest: the recorded hours, against Binance for the same day
cargo run --release -- backtest --config backtest.toml \
  --binance-trades data/binance-perp-SOLUSDT-trades-2026-11-09.csv \
  --bullet-quotes  bullet-data/SOL-USD/bullet-SOL-USD-quotes-2026-11-09T*.csv.gz \
  --bullet-trades  bullet-data/SOL-USD/bullet-SOL-USD-trades-2026-11-09T*.csv.gz

# 3b. PROXY backtest over months: measure Bullet once, then imitate it on Binance history
cargo run --release -- calibrate --bullet-quotes bullet-data/SOL-USD/bullet-SOL-USD-quotes-*.csv.gz \
  --bullet-trades bullet-data/SOL-USD/bullet-SOL-USD-trades-*.csv.gz \
  --tape data/binance-perp-SOLUSDT-trades-2026-11-09.csv --tape-label "perp SOLUSDT" \
  --out bullet-data/SOL-USD/profile-vs-perp-SOLUSDT.toml
cargo run --release -- backtest --config backtest.toml \
  --binance-trades data/binance-perp-SOLUSDT-trades-2026-*.csv \
  --proxy-tape     data/binance-perp-SOLUSDT-trades-2026-*.csv \
  --profile bullet-data/SOL-USD/profile-vs-perp-SOLUSDT.toml --report-out reports/proxy.txt
```

| File | |
|------|--|
| `src/marketdata.rs` | The CSV format everything shares (`recv_us,exch_us,...`) |
| `src/fetch.rs` | Binance archive download (perp or spot) and conversion |
| `src/record.rs` | Live recorder: Binance bookTicker + aggTrade, Bullet depth (top of book) + trades |
| `src/calibrate.rs` | Measures Bullet from recordings: spread, trade rate and sizes, gap and lag to Binance |
| `src/proxy.rs` | A stand-in Bullet built from a Binance tape and that profile (PROXY runs) |
| `src/backtest.rs` | Time-ordered replay, `SimExchange` (latency, post-only rejects, TTL, fills), report |

**Fills.** A Bullet trade *through* our price fills us (up to its size); the
book moving through our price fills us in full; `--fill-at-touch` also fills on
trades *at* our price (assumes we're first in the queue, so it's optimistic).
The report gives PnL, drawdown, fills, time quoting, fill markouts at
+1 s / +10 s (negative means we're being picked off) and, over several days,
daily PnL and Sharpe.

**REAL vs PROXY.** A REAL run replays Bullet as we recorded it: exact for
Bullet, but only as long as we've recorded. A PROXY run imitates Bullet over
any Binance history: the book follows the tape with the measured lag, gap
(which wanders as much as measured) and spread; trades are a sample of real
Binance trades thinned to Bullet's rate, with Bullet-like sizes. It tests
whether the pricing logic survives months of markets, not what Bullet would
have paid. Check it against REAL on the recorded days before trusting it.

**Which Binance market is the fair price.** Live, the bot reads Binance's
best bid/ask, which updates many times a second. The archive only has trades,
and the bot pulls quotes, and re-learns the Binance-to-Bullet gap for 60 s,
whenever its reference goes 5 s without an update. Over a day, the SOLUSDT perp
tape is silent for more than 5 s only 0.5% of the time; SOLUSDC perp 36%, and
SOLUSDC spot 43%. So backtests use the SOLUSDT perp. A USDC reference can only
be judged live (or from your own `record`ings).

**Where the data lives.**
- `bullet-data/<MARKET>/`: our Bullet recordings, committed. One gzipped CSV per
  market, stream and UTC hour (named by its first row's time, so names sort in
  time order), plus the market's tick and lot size (`*-meta.toml`). About
  30 MB a day for 26 markets. Bullet has no history to download, so these are
  irreplaceable: commit them (`scripts/sync-recordings.sh`, then `git add`).
  When the repo grows past a few GB, move older months to GitHub Releases or a
  storage bucket.
- `bullet-data/binance-map.toml`: which Binance perp tracks each Bullet market
  (checked by price; US100/US500 only have ETFs at a different price level).
- `data/`: Binance archive downloads and the recorder's working folder
  (`data/bullet-live/`). Not committed: Binance's archive can be downloaded
  again any time.

**Where to record.** Binance answers 403 to some regions and cloud IP ranges
(US ones among them), so run `record` where the live bot would run (the Tokyo
box from Part 4), not in a US cloud sandbox. Bullet's API has no market
history at all: `/fapi/v1/trades` holds only the last 1000 trades (about
1.5 h of SOL-USD), and the Celestia data-availability route is still "in
development" in Bullet's docs. Recording is the only source.

Binance's archive has no receive times, so `backtest` adds
`--reference-delay-ms` (default 2 ms, Binance to Tokyo) to archive rows.
Binance publishes each day around 07:00-09:00 UTC the next morning.

## The code

Feeds turn the world into **events**, the market maker answers with
**intents**, an executor carries them out.

| File | |
|------|--|
| `src/events.rs` | Every `Event` in and `Intent` out. Start here. |
| `src/bot.rs` | The loop that wires it together |
| `src/binance.rs`, `src/bullet.rs` | Feeds; `bullet.rs` also sends orders |
| `src/book.rs` | Our copy of the Bullet order book |
| `src/fair_value.rs` | Binance mid + average Binance→Bullet gap |
| `src/strategy.rs` | Bid and ask prices: spread, skew, limits |
| `src/market_maker.rs` | Events in, intents out. No I/O, fully tested |
| `src/inventory.rs` | Position and PnL, changed only by fills |
| `src/executor.rs` | Live orders, or paper ones in dry run |

## Following along

Each part is one PR on `master`, tagged `part-N`, with a short guide in
[`docs/parts/`](docs/parts/). `git checkout part-N` runs the bot as of that post.

| Part | Post | Guide |
|------|------|------|
| 1 | Introduction: the tick-to-trade loop | — |
| 2 | Hello World | [guide](docs/parts/02-hello-world.md) · [`part-2`](https://github.com/bulletxyz/hft-for-dummies/tree/part-2) |
| 3 | Telemetry and latency measurement | [guide](docs/parts/03-telemetry.md) · [`part-3`](https://github.com/bulletxyz/hft-for-dummies/tree/part-3) |
| 4 | Colocation on AWS | [guide](docs/parts/04-colocation.md) · [`part-4`](https://github.com/bulletxyz/hft-for-dummies/tree/part-4) |
| 5 | Streaming market data | [guide](docs/parts/05-streaming.md) · [`part-5`](https://github.com/bulletxyz/hft-for-dummies/tree/part-5) |
| 6 | Order execution | coming |
| 7 | Rate limits | coming |
| 8 | Linux tuning | coming |
| 9 | Optimising the hot path | coming |
| 10 | Network optimisation on AWS | coming |
| 11 | Nonce management | coming |

## Scoreboard

The [standard benchmark](docs/parts/03-telemetry.md#the-scoreboard) at the end
of each part, live with `bench-live.toml`: Part 3 on a laptop, the rest on the
Tokyo server over Bullet's direct endpoint. It splits one trip around the loop:

```text
 Binance update ──► reaches us ──► waits in our queue ──► we decide ──► order to Bullet ──► our order is open
                   binance→us         queue             tick-to-trade    order-to-live
 └─────────────────────────────────────── end-to-end ───────────────────────────────────────┘
```

| Part | end-to-end p50 / p99 | Binance → us p50 | tick-to-trade p50 | order-to-live p50 / p99 | queue p99 / max |
|---|---|---|---|---|---|
| 3 (laptop baseline) | 211 / 369 ms | (clock off) | 6 µs | (clock off) | 337 / 657 ms |
| 4 | 26.8 / 110 ms | 1.6 ms | 1 µs | 24.6 / 89 ms | 93 / 350 ms |
| 5 | 25.5 / 121 ms | 1.6 ms | 1 µs | 23.2 / 119 ms | 100 / 540 ms |

Every server run used the same machine, Part 8's 4-core c8g.xlarge (Part 4
sets up a c8g.large). Parts 4 and 5 are single 10-minute runs.

## Incidents

Things that went wrong while building this live, and what each taught us.
Details in each part's guide.

| Part | Incident | Lesson |
|------|----------|--------|
| 2 | Binance socket went silent, no error | Silence isn't "nothing happened": time out and reconnect |
| 2 | Bullet's websocket dropped a fill | Streams notify, records are truth: reconcile fills |
| 2 | An order filled during shutdown | Check fills once more before reporting |
| 2 | A cancel raced a fill | "Order not found" is normal; handle it (Part 6) |
| 3 | Laptop clock 288 ms behind: messages "from the future" | Sync clocks before measuring across machines |
| 4 | Our latency probe was timing DNS lookups | Know exactly what your tool measures |
| 5 | A 3 s heartbeat timeout fired falsely on wifi | Timeouts trade false alarms against reaction time |

## License

Apache-2.0. Not financial advice.
