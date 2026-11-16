# Cutting adverse selection (card #7), 2026-11-16

REAL replays of the Bullet recordings, 2026-11-10 → 2026-11-13 (~80 h),
HYPE, SOL, ZEC, NEAR, $10 orders, fees 0. $/day is scaled from the hours
recorded; markouts are per fill, weighted by fills (negative = picked off).

## 1. The replays had ~108 ms of hindsight

Bullet rows reach our recorder about 110 ms after Bullet stamps them
(p10 ~97 ms, p50 ~110 ms, p90 ~120 ms), while Binance archive rows are
replayed only 2 ms after Binance's stamp. So every Bullet trade was checked
against quotes we had already moved ~108 ms after the trade happened.
Earlier reports (`reports/2026-11-1*-markets*`) are optimistic for that
reason. Honest runs add `--reference-delay-ms 110`.

What the hindsight was worth (2 bp, $/day): ZEC -34.7 → -52.3,
NEAR -24.3 → -32.3, HYPE -12.4 → -16.3, SOL -9.3 → -11.3. It also made
"requote faster" look like a winner (ZEC 2 bp +2.5/day), which it isn't.

## 2. What was tried (honest timing)

| variant | what |
|---|---|
| base110 | the bot as it was: requote on a 3 bp move, at most every 500 ms |
| fast110 | 1 bp / 100 ms |
| basis20 / basis60 | basis half-life 20 s / 60 s instead of 300 s (without the delay fix) |
| asym110 | **new rule**: a quote the price moved *towards* is replaced at once; other moves wait for the rate limit. 3 bp / 500 ms |
| asym1_110 | new rule, 1 bp / 500 ms |
| asymfast110 | new rule, 1 bp / 100 ms |
| lean05 / lean1 | prototype: fair value moved 50% / 100% towards Bullet's mid (not kept) |

```
market     sp variant          $/day fills/h    mk1   mk10 days+
HYPE-USD    2 asym110         -13.48   433.2  -1.27  -1.29     0
HYPE-USD    2 asym1_110        -2.47    90.1  -1.13  -1.14     0
HYPE-USD    2 asymfast110      -2.40    85.1  -1.16  -1.17     0
HYPE-USD    2 base110         -16.27   493.9  -1.36  -1.36     0
HYPE-USD    2 basis20         -12.52   422.4  -1.24  -1.23     0
HYPE-USD    2 basis60         -12.62   427.6  -1.24  -1.22     0
HYPE-USD    2 fast110          -5.20   157.4  -1.38  -1.37     0
HYPE-USD    2 lean05           -1.68    48.6  -1.44  -1.40     0
HYPE-USD    2 lean1            -5.81   141.5  -1.67  -1.68     0
HYPE-USD    5 asym110          -0.12     3.0  -1.36  -1.28     1
HYPE-USD    5 asym1_110        -0.05     1.3  -1.83  -2.30     2
HYPE-USD    5 asymfast110       0.06     1.5  -0.75  -3.27     3
HYPE-USD    5 base110          -1.00    24.1  -1.74  -1.87     0
HYPE-USD    5 basis20          -0.46    18.3  -1.14  -1.08     0
HYPE-USD    5 basis60          -0.53    18.9  -1.44  -1.63     0
HYPE-USD    5 fast110          -0.10     5.4  -1.05  -1.92     2
HYPE-USD    5 lean05           -0.03     1.4  -2.56  -3.19     1
HYPE-USD    5 lean1            -0.47     7.0  -3.16  -3.58     0
HYPE-USD   10 asym110           0.05     0.1  -4.67  -0.72     2
HYPE-USD   10 asym1_110        -0.01     0.0  -7.27  -3.39     0
HYPE-USD   10 asymfast110      -0.04     0.0  -3.75  -5.25     1
HYPE-USD   10 base110           0.34     1.4  -4.95  -3.57     3
HYPE-USD   10 basis20           0.14     1.1  -2.02  -2.20     3
HYPE-USD   10 basis60           0.20     1.0  -1.04  -2.10     3
HYPE-USD   10 fast110           0.11     0.3  -1.12  -2.30     3
HYPE-USD   10 lean05           -0.05     0.0  -3.37   6.04     0
HYPE-USD   10 lean1             0.10     0.6  -6.84  -8.16     2
NEAR-USD    2 asym110         -27.28   484.9  -2.21  -2.23     0
NEAR-USD    2 asym1_110       -10.75   214.7  -1.98  -2.01     0
NEAR-USD    2 asymfast110     -13.28   264.6  -1.97  -1.99     0
NEAR-USD    2 base110         -32.32   541.0  -2.35  -2.36     0
NEAR-USD    2 basis20         -24.05   448.7  -2.10  -2.11     0
NEAR-USD    2 basis60         -24.40   455.8  -2.10  -2.11     0
NEAR-USD    2 fast110         -19.38   350.4  -2.18  -2.19     0
NEAR-USD    2 lean05           -6.97   142.2  -1.98  -1.98     0
NEAR-USD    2 lean1           -15.62   245.9  -2.46  -2.51     0
NEAR-USD    5 asym110          -1.45    33.2  -1.94  -1.89     0
NEAR-USD    5 asym1_110        -0.37     5.2  -1.78  -1.84     2
NEAR-USD    5 asymfast110      -0.37     6.6  -1.67  -1.51     1
NEAR-USD    5 base110          -3.79    63.0  -2.43  -2.38     0
NEAR-USD    5 basis20          -2.19    43.9  -2.23  -2.28     0
NEAR-USD    5 basis60          -2.34    42.8  -2.04  -2.08     0
NEAR-USD    5 fast110          -1.00    15.2  -2.63  -2.63     0
NEAR-USD    5 lean05           -0.22     2.6  -2.26  -3.68     1
NEAR-USD    5 lean1            -1.18     9.0  -4.69  -4.64     0
NEAR-USD   10 asym110           0.05     0.1 -10.26 -16.36     1
NEAR-USD   10 asym1_110         0.09     0.1 -13.86 -19.39     2
NEAR-USD   10 asymfast110       0.09     0.1 -14.35 -19.89     2
NEAR-USD   10 base110          -0.08     2.8  -5.44  -4.36     1
NEAR-USD   10 basis20          -0.39     2.6  -2.65  -2.21     0
NEAR-USD   10 basis60          -0.45     2.4  -2.72  -3.85     0
NEAR-USD   10 fast110          -0.16     0.9  -5.24  -8.98     2
NEAR-USD   10 lean05           -0.08     0.1 -18.62 -24.52     1
NEAR-USD   10 lean1            -0.26     0.8 -13.87 -13.77     1
SOL-USD     2 asym110          -9.61   508.6  -0.82  -0.81     0
SOL-USD     2 asym1_110        -1.25   105.6  -0.48  -0.55     0
SOL-USD     2 asymfast110      -1.22   106.3  -0.45  -0.52     0
SOL-USD     2 base110         -11.33   549.1  -0.90  -0.90     0
SOL-USD     2 basis20          -9.24   500.8  -0.81  -0.79     0
SOL-USD     2 basis60          -9.30   505.8  -0.79  -0.78     0
SOL-USD     2 fast110          -3.69   212.1  -0.77  -0.74     0
SOL-USD     2 lean05           -2.11   132.9  -0.68  -0.70     0
SOL-USD     2 lean1            -4.76   190.4  -1.09  -1.07     0
SOL-USD     5 asym110           0.04     0.9  -0.41  -1.80     2
SOL-USD     5 asym1_110        -0.03     0.1   0.73  -0.14     2
SOL-USD     5 asymfast110       0.09     0.2  22.94  15.51     3
SOL-USD     5 base110          -0.54    15.1  -1.35  -1.33     0
SOL-USD     5 basis20          -0.20     9.7  -1.51  -1.08     0
SOL-USD     5 basis60          -0.12     9.9  -1.43  -0.60     2
SOL-USD     5 fast110           0.03     1.6   2.68   0.23     2
SOL-USD     5 lean05           -0.10     0.2  -2.77  -6.92     0
SOL-USD     5 lean1            -0.11     3.0  -4.06  -3.88     2
SOL-USD    10 asym110           0.00     0.0    nan    nan     0
SOL-USD    10 asym1_110         0.00     0.0    nan    nan     0
SOL-USD    10 asymfast110       0.04     0.0  83.86  22.53     1
SOL-USD    10 base110          -0.14     1.0  -1.41  -3.80     1
SOL-USD    10 basis20           0.12     0.6   2.77   3.71     3
SOL-USD    10 basis60           0.17     0.6   2.13   5.86     3
SOL-USD    10 fast110          -0.05     0.2  28.59  16.51     1
SOL-USD    10 lean05            0.00     0.0    nan    nan     0
SOL-USD    10 lean1            -0.07     0.3  -3.81  -6.09     1
ZEC-USD     2 asym110         -32.46   884.4  -1.58  -1.54     0
ZEC-USD     2 asym1_110        -6.16   185.1  -1.43  -1.44     0
ZEC-USD     2 asymfast110      -8.25   257.7  -1.41  -1.35     0
ZEC-USD     2 base110         -52.28  1190.5  -1.86  -1.83     0
ZEC-USD     2 basis20         -34.36   916.6  -1.56  -1.54     0
ZEC-USD     2 basis60         -34.49   914.3  -1.57  -1.56     0
ZEC-USD     2 fast110         -26.29   599.0  -1.83  -1.85     0
ZEC-USD     2 lean05           -6.76   174.9  -1.67  -1.60     0
ZEC-USD     2 lean1           -23.50   414.8  -2.38  -2.35     0
ZEC-USD     5 asym110          -0.53    14.6  -1.66  -1.64     0
ZEC-USD     5 asym1_110        -0.25     3.7  -1.74  -1.74     1
ZEC-USD     5 asymfast110      -0.20     5.5  -1.98  -1.28     1
ZEC-USD     5 base110          -8.57   138.6  -2.61  -2.52     0
ZEC-USD     5 basis20          -5.41   110.5  -1.94  -1.91     0
ZEC-USD     5 basis60          -5.36   107.9  -2.03  -1.96     0
ZEC-USD     5 fast110          -3.19    40.9  -2.80  -3.19     0
ZEC-USD     5 lean05           -0.40     4.3  -2.75  -3.29     0
ZEC-USD     5 lean1            -3.96    37.5  -4.32  -4.33     0
ZEC-USD    10 asym110           0.25     0.2  -4.67  -3.88     1
ZEC-USD    10 asym1_110        -0.14     0.1  -5.84   6.36     0
ZEC-USD    10 asymfast110      -0.26     0.2   1.57   9.97     0
ZEC-USD    10 base110          -1.48    18.9  -4.20  -4.96     1
ZEC-USD    10 basis20          -1.05    14.1  -3.22  -4.11     1
ZEC-USD    10 basis60          -0.89    13.8  -3.29  -3.73     1
ZEC-USD    10 fast110          -0.23     4.4  -5.40  -7.34     1
ZEC-USD    10 lean05           -0.04     0.2  -1.52   5.63     2
ZEC-USD    10 lean1            -1.26     4.7  -7.74  -8.90     0
```

## 3. Findings

- **Shorter basis half-life: no effect.** The gap isn't what moves.
- **Leaning on Bullet's own mid: worse.** Bullet's price doesn't lead; drop it.
- **Pull fast, add slow (kept, `market_maker.rs`):** losses at 2 bp fall
  75–95% (ZEC -52 → -6/day at 1 bp threshold, HYPE -16 → -2.5, SOL -11 → -1.3),
  mostly by being hit less often. The fills that remain still lose
  0.5–2 bp each, so no market makes money at 2–5 bp yet. At 10 bp there are
  too few fills to tell (HYPE base110 +0.34/day, new rule +0.05/day, ~0–1 fill/h).
- Kill criterion (REAL +10 s markout at 2 bp better than -0.5 bp): not met
  (best: SOL -0.55, HYPE -1.14, ZEC -1.44, NEAR -2.01).

## 4. Next

- Binance bid/ask instead of last trade (#6): the trade tape bounces between
  bid and ask and lags the book; needs our own live recording.
- A move guard: widen for a few hundred ms after a fast Binance move.
- Backtest default: `--reference-delay-ms` should match the Bullet recording
  delay automatically, so no one replays with hindsight by accident.
