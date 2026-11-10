# Strategy pipeline: idea to execution

How a trading idea moves from a thought to real money here, the way a prop
firm runs it. Every idea is one GitHub issue (a "card"), labelled with its
stage. The Kanban board (GitHub Projects) has one column per stage.

A card moves right only when it meets the **gate** for the next stage, and
the evidence (a report, a link, numbers) goes in a comment on the card. Most
ideas should die early and cheaply: killing a card is a good outcome.

| # | Stage | What happens | Gate to move on (evidence on the card) |
|---|-------|--------------|----------------------------------------|
| 0 | **Idea** | A hypothesis in one paragraph: what edge, why it should exist, who's on the other side. | The idea can be tested with data we can get. |
| 1 | **Research** | Data sourcing, market microstructure, fees, venue rules. Cheap checks. | A data set exists covering the test period, and a written test plan (metric, period, costs). |
| 2 | **Backtest** | Code the strategy (Rust, same code as live where possible) and replay history with costs and latency. | Positive after fees and realistic fills, in-sample. Report committed in `reports/`. |
| 3 | **Validation** | Out-of-sample period, parameter robustness, stress days, proxy vs real check, risk review: max loss, position limits, kill switch. | Holds out of sample; no single parameter or day makes all the profit; risk limits written down. |
| 4 | **Paper** | Run live against the real venue without orders (`dry_run`), from the production server. | ≥ 1 week of paper trading in line with the backtest (fills, markouts, PnL). |
| 5 | **Pilot live** | Minimum size, real money, hard loss limit. Daily review. | ≥ 2 weeks live; live fills and markouts match paper; no incidents left open. |
| 6 | **Production** | Scaled size, monitoring and alerts, weekly review of PnL against expectations. | Ongoing. Demote to Pilot (or retire) when live drifts from expectations. |
| x | **Retired** | Killed at any stage, or switched off. Keep the card: write down why. | — |

## Rules

- **One card per idea.** Big projects get child cards (sub-issues) for the
  pieces, each moving through the same stages.
- **Going live is a human decision** at stages 5 and 6, with the amount
  written on the card first.
- **Every number has a source.** Backtest results link to a report file;
  live results link to the account or logs.
- **Kill criteria up front.** Each card says, before testing, what result
  would kill it.

## Labels

`stage: 0 idea`, `stage: 1 research`, `stage: 2 backtest`, `stage: 3 validation`,
`stage: 4 paper`, `stage: 5 pilot live`, `stage: 6 production`, `stage: x retired`.
Plus `strategy` for strategy cards and `infra` for supporting work (data,
servers, tooling) that serves one.
