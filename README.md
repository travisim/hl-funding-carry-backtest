# hl-funding-carry-backtest

A deterministic, self-contained Rust backtest of a **short funding-carry
strategy on Hyperliquid**, run over a frozen 45-day SQLite snapshot of
Hyperliquid hourly funding rates that ships with this repository.

This is a **research artifact**, published for reproducibility. It is **not
financial advice**, not a product pitch, and not an offer to manage money.

## What it is — and what it is NOT

This is a **funding-leg-ONLY historical vector backtest**. It simulates the
funding income a short-perp carry position would have earned, minus modeled
execution costs (fees and slippage). The frozen snapshot contains **no price
series**, so the **price PnL of the short leg is NOT modeled** — the output
field `not_price_pnl` is always `true`.

**Read that again: price PnL is NOT modeled.** A short-perp carry position
that is not hedged has **unbounded adverse price risk**: a coin whose funding
rate is persistently positive can still rally 100% and wipe out months of
funding income in a day. The headline number below is the funding leg in
isolation. **It is NOT an achievable strategy return.** The all-in PnL
(funding + short-leg price PnL) was measured separately in forward paper
trading, and it killed the strategy — see [Negative results](#negative-results).

## Headline result (funding leg only, maker execution assumption)

Over the 45-day frozen snapshot (2026-09-12 → 2026-10-27), with the default
parameters listed below and `--maker`:

| Metric | Value |
|---|---|
| Net carry PnL | **+$203.86** ($203.858715) |
| APR (on peak gross $10,000) | **16.535207%** |
| Funding income | +$237.01 |
| Fees (maker, 1.5 bps/fill) | −$33.15 |
| Fills | 221 |
| Coins traded (short ≥ 1 hour) | 178 |
| Price PnL | **NOT modeled** (`not_price_pnl: true`) |

## Execution sensitivity: maker fills are load-bearing

The maker case assumes every entry and exit fills as a **maker** at 1.5 bps
with **zero slippage**. That assumption is doing enormous work. Switching to
**taker execution** (Hyperliquid base-tier taker fee of 4.5 bps plus 5 bps of
modeled slippage) collapses the same 45-day run to:

- **Net carry +$27.06 = 2.19% APR** (funding +$237.01, fees −$99.45,
  slippage −$110.50)

i.e. an ~87% reduction in net PnL purely from execution. The entire edge in
this backtest exists only if fills are maker fills. **The live maker-fill
path was never demonstrated** — no live or paper trading run confirmed that
these entries would actually have been filled passively. Treat the maker
headline as a best-case execution assumption, not an expectation.

## Negative results

Stated plainly, because they matter more than the headline:

1. **The forward paper-trading gate for this strategy class was KILLED**
   (2026-10-28). The forward test produced an unmeasurably small sample and
   negative expectancy once price PnL was included. The gate was killed
   as-pinned and the job was paused.
2. **A hedged event-study rescue of the strategy class is DEAD.** We tried
   four hedged variants (unhedged, beta-hedged, vol-scaled, funding-spread)
   over the same window. **No variant passed a bootstrap confidence-interval
   test**: the beta-hedged variant (V1, 8.24% APR) had a bootstrap CI of
   [−27.13, +31.64] $/episode which includes 0, and the funding-spread
   variant (V3, 38% APR) had a valid hourly CI of [−0.66, +1.11] which also
   includes 0. We deliberately do not print any hedged-net dollar figure:
   none is verified, and a point estimate whose CI includes zero is not a
   result.

Bottom line: the funding leg looks positive in isolation over this window,
but the class as a whole failed its forward and hedged evaluations. This
repo exists so the one number we *did* verify can be independently
reproduced — not as a recommendation.

## Reproduce

Requirements: Rust (stable), `shasum` (or `sha256sum`).

```sh
# 1. Verify the frozen snapshot is byte-identical to what produced the
#    numbers above.
shasum -a 256 -c data/CHECKSUMS.sha256

# 2. Run the backtest (all parameters below are the code's defaults, shown
#    explicitly so the command fully pins the run).
cargo run --release -- --maker --db data/funding_history.db \
  --days 45 --entry-floor 0.000015 --exit-floor 0.0000125 \
  --taker-fee-bps 3.5 --maker-fee-bps 1.5 --order-usd 1000 \
  --max-gross-usd 10000 --annualization-hours 8760
```

The run prints exactly one JSON object between the markers
`===PNL-JSON-START===` / `===PNL-JSON-END===`. The values
`net_carry_pnl_usd: 203.858715`, `apr_pct: 16.535207` and `fills: 221` must
appear byte-identically. (All floats are rounded to 6 decimals and all
iteration is over sorted structures, so the same snapshot plus the same
flags is fully deterministic.)

To reproduce the taker sensitivity figure, drop `--maker` and pass
`--taker-fee-bps 4.5` (keep the other defaults).

## Repository layout

```
src/lib.rs          the simulator (vector backtest + JSON renderer + unit tests)
src/main.rs         CLI wrapper (clap); prints the JSON result block
data/               frozen 45-day snapshot: funding_history.db (SQLite,
                    read-only), meta.json (provenance), CHECKSUMS.sha256
METHODOLOGY.md      what the code actually computes, parameter by parameter
DATA.md             where the data came from and why it is frozen
```

## Data

Public Hyperliquid funding-rate history (Info API), 235-coin universe,
56 delisted coins excluded, 192,256 rows, window
2026-09-12T22:29Z → 2026-10-27T22:29Z. See `DATA.md`. No personal data is
included; only public API endpoints were used, read-only.

## License

MIT — see `LICENSE`.
