# Methodology

This document describes what `src/lib.rs` actually computes, derived from the
code (the module doc comment in `lib.rs` pins the same conventions). Nothing
here is aspirational: every bullet below corresponds to executed code paths.

## Scope and honesty boundary

The simulator models **only the funding income leg** plus execution costs.
The frozen snapshot contains a single table `funding_history(ts, coin,
funding_rate, premium)` — **no price series**, so the price PnL of the short
leg is not modeled, proxied or fabricated. The output JSON always sets
`not_price_pnl: true`. The crate is analytics-only: it opens the database
read-only, routes no orders, touches no exchange, writes no ledger entries.

## Universe selection

1. The window is the last `days * 24` hourly timestamps (unix seconds, UTC,
   hourly grid) ending at the newest `ts` present in the snapshot. With
   `--days 45` that is 1080 hours.
2. All rows in `funding_history` within `[window_start, window_end]` are
   loaded, ordered by `(coin, ts)`. NULL or non-finite rates count as
   **absent** observations.
3. A coin is included iff its number of present observations divided by the
   window's hour count is at least `min_coverage` (default `0.9`). Below
   that, the coin is excluded entirely — even if its rates would have been
   profitable (this is tested in the unit tests).
4. `rows_used` counts the window rows belonging to included coins.

## Entry / exit signal

For each coin and each window hour `t`:

- Compute the **trailing mean** of the 24 hourly rates at `t-24h .. t-1h`.
  It requires at least 18 of 24 observations; otherwise no decision is made
  at that hour (`MIN_TRAILING_OBS = 18`).
- **Enter short** if flat, the mean is at least `entry_floor`
  (default `0.000015` per hour, i.e. ≥ 0.0015%/hour ≈ 13.1% APR equivalent
  per position at that rate), and portfolio gross exposure after adding
  `order_usd` stays within `max_gross_usd`. A skipped entry is **never
  queued** — it is simply re-evaluated next hour.
- **Exit** if short and the mean is **strictly** below `exit_floor`
  (default `0.0000125`). A mean exactly equal to `exit_floor` holds the
  position. Both boundaries are unit-tested with exactly-representable dyadic
  rates.
- Processing order is coins in sorted order (deterministic skip under the
  portfolio cap).

## Position sizing and portfolio cap

Each position is a fixed `order_usd` (default $1,000) of notional per coin.
The portfolio cap `max_gross_usd` (default $10,000) limits concurrent shorts;
`max_gross_used_usd` in the output is the **peak** concurrent gross notional
actually reached, and the APR denominator.

## Funding accrual

After the state update at hour `t`, a coin that is short credits
`order_usd * funding_rate(t)` — the actual hourly rate, never interpolated.
A missing rate at `t` credits 0. A coin that exits at hour `t` stops accruing
at `t`; its entry hour is its first accrual hour.

## Fee and slippage model

Every entry and every exit is charged `fee_bps + slippage_bps` on the
notional, as a single flat expression — there is no partial-fill or queue
model:

- **Taker (default):** `taker_fee_bps` (default 3.5) + `slippage_bps`
  (default 5.0). The taker sensitivity figure in the README uses
  `--taker-fee-bps 4.5`, the Hyperliquid base-tier taker fee (the 3.5 bps
  default corresponds to a >$25M-volume tier).
- **Maker (`--maker`):** `maker_fee_bps` (default 1.5) + **zero slippage**.

These come only from CLI flags — no environment variables, no config files —
and there is a single code path: `--maker` only selects which parameters are
charged. This is explicitly a **sensitivity model, not an execution
simulation**: the maker case assumes every fill is passive with no queue
position risk and no adverse selection, and that assumption was never
validated live (see README).

## Annualization

```
apr_pct = net_carry_pnl / max_gross_used * (annualization_hours / hours) * 100
```

with `annualization_hours` defaulting to 8760 (hourly funding). For the
45-day run: 8760 / 1080 ≈ 8.111× linear scaling of the 45-day net carry
return on peak gross. If no position was ever opened (`peak_gross == 0`),
APR is reported as 0. `fees_usd` in the output is commission only;
slippage is reported separately in `slippage_usd`.

## Determinism — why a frozen snapshot

- **Frozen snapshot:** the SQLite file shipped in `data/` is never written to
  (opened `SQLITE_OPEN_READ_ONLY`) and is pinned by `CHECKSUMS.sha256`. The
  window is anchored to the newest `ts` *in the file*, not to the wall clock,
  so re-running next week reproduces the identical window.
- **Deterministic iteration:** all maps are `BTreeMap` and all sorts have
  explicit tie-breakers (per-coin results: net PnL descending, ties by coin
  ascending), with fixed accumulation order for every sum.
- **Output:** floats are rounded to 6 decimals at serialization time only;
  the same snapshot plus the same flags produce a **byte-identical** JSON
  object (covered by a unit test).
- **Fails loudly:** a missing or non-SQLite database is an error, never an
  empty result; all output floats are checked finite before serialization.

## Reporting

The JSON object includes the full configuration, window bounds, universe
sizes, execution style, fills, peak gross, funding/fees/slippage/net PnL,
APR, `not_price_pnl: true`, and a `per_coin` list of the top 20 coins by net
PnL among coins that were short for at least one hour.
