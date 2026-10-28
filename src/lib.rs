//! Deterministic historical vector backtest of a **short funding-carry**
//! strategy on Hyperliquid, run over a frozen SQLite funding snapshot.
//!
//! # Honesty scope — `not_price_pnl` (this is NOT all-in PnL)
//!
//! This backtest models ONLY the funding income leg plus execution costs
//! (taker/maker fees and slippage) on entries and exits. The frozen snapshot
//! contains **no price series**, so the price PnL of the short leg is **not
//! modeled, proxied or fabricated**; the output field `not_price_pnl` is
//! always `true`. The all-in PnL (funding + short-leg price PnL) is what the
//! forward paper-trading job (`jobs/hl-paper-funding-carry`) measures.
//!
//! # Analytics only — no order routing, no risk gate
//!
//! This crate routes no orders, touches no exchange, and writes nothing back
//! to the snapshot (it opens the database read-only). It is therefore an
//! analytics-only crate: no risk-gate call
//! (`mp_quant_core::risk::check_order`) is required or performed, and no
//! ledger entries are written. Any strategy promoted from these numbers to
//! paper or live trading must go through the paper job, which does call the
//! risk gate.
//!
//! # Model conventions (pinned here so output stays interpretable)
//!
//! - Timestamps are unix seconds UTC on an hourly grid; the window is the
//!   last `days * 24` hourly timestamps ending at the newest `ts` in the
//!   snapshot (`hours = days * 24`).
//! - **Universe:** coins with at least `min_coverage` of the window's hours
//!   present (each hour counted once; NULL/non-finite rates count as absent).
//! - **Entry signal at hour `t`:** trailing mean of the 24 hourly rates at
//!   `t-24h .. t-1h`, requiring at least 18 of 24 observations (else no
//!   decision). Enter SHORT when not short and `mean >= entry_floor`; exit
//!   when short and `mean < exit_floor` (strict, so `mean == exit_floor`
//!   holds the position).
//! - **Position sizing:** `order_usd` notional per coin; a new entry is
//!   skipped (never queued) if `current_gross + order_usd > max_gross_usd`.
//! - **Funding accrual:** after the state update at hour `t`, a coin that is
//!   short credits `order_usd * funding_rate(t)`; a missing rate at `t`
//!   credits 0 (never interpolated). A coin that exits at hour `t` stops
//!   accruing at `t` (its entry hour is its first accrual hour).
//! - **Costs:** every entry and every exit is charged
//!   `fee_bps_used + slippage_bps_used` on the notional. Taker case:
//!   `--taker-fee-bps` + `--slippage-bps`. Maker case (`--maker`):
//!   `--maker-fee-bps` + zero slippage. These are conservative sensitivity
//!   parameters supplied purely by flags — there is no second code path.
//! - **Annualization:** `apr_pct = net_carry_pnl / max_gross_used *
//!   (annualization_hours / hours) * 100`, where `max_gross_used` is the
//!   peak gross exposure (concurrent shorts × `order_usd`) during the run.
//! - **Reporting fields:** `fees_usd` is commission only; slippage is
//!   reported separately in `slippage_usd`. Per-coin
//!   `net_pnl_usd = funding_income_usd - fees_usd - slippage_usd` (the
//!   per-coin `fees_usd` field is commission only, matching the global
//!   field). `rows_used` counts snapshot rows inside the window that belong
//!   to coins passing the coverage filter. `per_coin` lists only coins that
//!   were short for at least one hour, sorted by net PnL desc (ties by coin
//!   asc), top 20.
//! - **Determinism:** all iteration is over sorted structures (`BTreeMap`,
//!   sorted vectors) with fixed accumulation order; the same snapshot and
//!   the same flags produce byte-identical output. All floats are rounded to
//!   6 decimals at serialization time only.

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;

/// Default frozen snapshot (read-only) this backtest runs against.
pub const DEFAULT_DB: &str = "var/data/hyperliquid/runs/2026-10-27/funding_history.db";

const HOUR_SECONDS: i64 = 3_600;
const TRAILING_HOURS: i64 = 24;
const MIN_TRAILING_OBS: usize = 18;
const PER_COIN_TOP_N: usize = 20;

/// All strategy and cost parameters. Fees, slippage, window and
/// annualization come ONLY from these flags (no env, no config files) so a
/// reproduce command fully pins the run.
#[derive(Debug, Clone)]
pub struct SimConfig {
    /// Path to the frozen SQLite snapshot (opened read-only, never written).
    pub db: String,
    /// Window length in days; `hours = days * 24` ending at the newest ts.
    pub days: u32,
    /// Enter short when trailing 24h mean hourly rate >= this (fraction/hour).
    pub entry_floor: f64,
    /// Exit short when trailing 24h mean hourly rate < this (strict).
    pub exit_floor: f64,
    /// Taker commission in basis points of notional, charged per fill.
    pub taker_fee_bps: f64,
    /// Maker commission in basis points of notional, charged per fill.
    pub maker_fee_bps: f64,
    /// Slippage in basis points of notional, charged per fill (taker only).
    pub slippage_bps: f64,
    /// Notional per coin per position, in USD.
    pub order_usd: f64,
    /// Portfolio cap on gross short notional, in USD.
    pub max_gross_usd: f64,
    /// Minimum fraction of window hours a coin must have to be traded.
    pub min_coverage: f64,
    /// Hours per year used to annualize (default 8760 for hourly funding).
    pub annualization_hours: f64,
    /// When true, use maker fees and zero slippage instead of taker.
    pub maker: bool,
}

impl SimConfig {
    fn validate(&self) -> Result<()> {
        if self.days == 0 {
            bail!("--days must be >= 1");
        }
        if !(self.order_usd.is_finite() && self.order_usd > 0.0) {
            bail!("--order-usd must be a finite number > 0");
        }
        if !(self.max_gross_usd.is_finite() && self.max_gross_usd >= 0.0) {
            bail!("--max-gross-usd must be a finite number >= 0");
        }
        if !(self.annualization_hours.is_finite() && self.annualization_hours > 0.0) {
            bail!("--annualization-hours must be a finite number > 0");
        }
        if !(self.min_coverage.is_finite() && (0.0..=1.0).contains(&self.min_coverage)) {
            bail!("--min-coverage must be a finite number within [0, 1]");
        }
        if !self.entry_floor.is_finite() || !self.exit_floor.is_finite() {
            bail!("--entry-floor and --exit-floor must be finite");
        }
        if !(self.taker_fee_bps.is_finite() && self.maker_fee_bps.is_finite()) {
            bail!("fee bps must be finite");
        }
        if !(self.slippage_bps.is_finite() && self.slippage_bps >= 0.0) {
            bail!("--slippage-bps must be a finite number >= 0");
        }
        Ok(())
    }

    /// Commission bps actually charged under the selected execution style.
    fn fee_bps_used(&self) -> f64 {
        if self.maker {
            self.maker_fee_bps
        } else {
            self.taker_fee_bps
        }
    }

    /// Slippage bps actually charged (maker execution assumes zero slippage).
    fn slippage_bps_used(&self) -> f64 {
        if self.maker {
            0.0
        } else {
            self.slippage_bps
        }
    }
}

/// Per-coin result (only coins that were short for >= 1 hour appear).
#[derive(Debug, Clone)]
pub struct CoinPnl {
    pub coin: String,
    pub hours_short: u64,
    pub funding_income_usd: f64,
    /// Commission only (matches the global `fees_usd` field).
    pub fees_usd: f64,
    /// funding - fees - slippage for this coin.
    pub net_pnl_usd: f64,
}

/// Full, unrounded result of one deterministic run.
#[derive(Debug, Clone)]
pub struct SimOutput {
    pub db: String,
    pub days: u32,
    pub window_start_ts: i64,
    pub window_end_ts: i64,
    pub coins_in_db: usize,
    pub coins_used: usize,
    pub rows_used: u64,
    pub hours: u64,
    pub entry_floor: f64,
    pub exit_floor: f64,
    pub fee_bps_used: f64,
    pub slippage_bps_used: f64,
    pub execution: &'static str,
    pub fills: u64,
    pub max_gross_used_usd: f64,
    pub funding_income_usd: f64,
    pub fees_usd: f64,
    pub slippage_usd: f64,
    pub net_carry_pnl_usd: f64,
    pub apr_pct: f64,
    pub per_coin: Vec<CoinPnl>,
}

/// Run the deterministic vector backtest over the frozen snapshot.
///
/// Opens the snapshot read-only, loads the window, filters the universe by
/// coverage, then walks the window hour by hour in sorted (coin, ts) order.
pub fn run_sim(cfg: &SimConfig) -> Result<SimOutput> {
    cfg.validate()?;

    // --- Load the frozen snapshot (READ-ONLY; we never write to it). ---
    let conn = Connection::open_with_flags(&cfg.db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open funding snapshot `{}` read-only", cfg.db))?;

    let max_ts: Option<i64> = conn
        .query_row("SELECT MAX(ts) FROM funding_history", [], |r| r.get(0))
        .with_context(|| format!("read MAX(ts) from `funding_history` in `{}`", cfg.db))?;
    let window_end_ts = max_ts
        .with_context(|| format!("snapshot `{}` has no rows in `funding_history`", cfg.db))?;

    let hours = u64::from(cfg.days) * 24;
    let window_start_ts = window_end_ts - (hours as i64 - 1) * HOUR_SECONDS;

    let mut stmt = conn
        .prepare(
            "SELECT coin, ts, funding_rate FROM funding_history \
             WHERE ts >= ?1 AND ts <= ?2 ORDER BY coin, ts",
        )
        .with_context(|| format!("prepare funding_history query on `{}`", cfg.db))?;
    let mut rates_by_coin: BTreeMap<String, BTreeMap<i64, f64>> = BTreeMap::new();
    let mut rows = stmt
        .query(rusqlite::params![window_start_ts, window_end_ts])
        .with_context(|| format!("query funding_history on `{}`", cfg.db))?;
    while let Some(row) = rows.next().context("iterate funding_history rows")? {
        let coin: String = row.get(0).context("read coin column")?;
        let ts: i64 = row.get(1).context("read ts column")?;
        let rate: Option<f64> = row.get(2).context("read funding_rate column")?;
        // NULL / non-finite rates count as a missing observation.
        if let Some(rate) = rate.filter(|r| r.is_finite()) {
            rates_by_coin.entry(coin).or_default().insert(ts, rate);
        }
    }

    let coins_in_db = rates_by_coin.len();
    if coins_in_db == 0 {
        bail!(
            "snapshot `{}` window [{}, {}] contains no usable funding rows",
            cfg.db,
            window_start_ts,
            window_end_ts
        );
    }

    // --- Universe filter: coverage over the window's hours. ---
    let hours_f = hours as f64;
    let universe: BTreeMap<String, BTreeMap<i64, f64>> = rates_by_coin
        .into_iter()
        .filter(|(_, rates)| (rates.len() as f64) / hours_f >= cfg.min_coverage)
        .collect();
    let coins_used = universe.len();
    let rows_used: u64 = universe.values().map(|rates| rates.len() as u64).sum();

    // --- Deterministic hourly vector loop over sorted coins. ---
    struct CoinState {
        short: bool,
        funding: f64,
        fees: f64,
        slippage: f64,
        hours_short: u64,
    }
    let mut states: BTreeMap<&str, CoinState> = universe
        .keys()
        .map(|coin| {
            (
                coin.as_str(),
                CoinState {
                    short: false,
                    funding: 0.0,
                    fees: 0.0,
                    slippage: 0.0,
                    hours_short: 0,
                },
            )
        })
        .collect();

    let order = cfg.order_usd;
    let fee_rate = cfg.fee_bps_used() / 10_000.0;
    let slip_rate = cfg.slippage_bps_used() / 10_000.0;
    let mut gross = 0.0f64;
    let mut peak_gross = 0.0f64;
    let mut fills = 0u64;

    for h in 0..hours {
        let ts = window_start_ts + h as i64 * HOUR_SECONDS;
        for (coin, rates) in &universe {
            let Some(st) = states.get_mut(coin.as_str()) else {
                continue; // unreachable: states is keyed by universe coins
            };

            // Trailing 24h mean over hours t-24..t-1; need >= 18 observations.
            let mut sum = 0.0f64;
            let mut obs = 0usize;
            for k in 1..=TRAILING_HOURS {
                if let Some(r) = rates.get(&(ts - k * HOUR_SECONDS)) {
                    sum += *r;
                    obs += 1;
                }
            }
            let mean = if obs >= MIN_TRAILING_OBS {
                Some(sum / obs as f64)
            } else {
                None
            };

            if st.short {
                if let Some(m) = mean {
                    if m < cfg.exit_floor {
                        st.fees += order * fee_rate;
                        st.slippage += order * slip_rate;
                        st.short = false;
                        gross -= order;
                        fills += 1;
                    }
                }
            } else if let Some(m) = mean {
                if m >= cfg.entry_floor && gross + order <= cfg.max_gross_usd {
                    st.fees += order * fee_rate;
                    st.slippage += order * slip_rate;
                    st.short = true;
                    gross += order;
                    fills += 1;
                    if gross > peak_gross {
                        peak_gross = gross;
                    }
                }
            }

            if st.short {
                // Funding accrual at the actual hourly rate; missing rate -> 0.
                let rate_t = rates.get(&ts).copied().unwrap_or(0.0);
                st.funding += order * rate_t;
                st.hours_short += 1;
            }
        }
    }

    // --- Aggregate (fixed accumulation order via BTreeMap iteration). ---
    let funding_income_usd = states.values().map(|s| s.funding).sum();
    let fees_usd = states.values().map(|s| s.fees).sum();
    let slippage_usd = states.values().map(|s| s.slippage).sum();
    let net_carry_pnl_usd = funding_income_usd - fees_usd - slippage_usd;
    let apr_pct = if peak_gross > 0.0 {
        net_carry_pnl_usd / peak_gross * (cfg.annualization_hours / hours_f) * 100.0
    } else {
        0.0
    };

    let mut per_coin: Vec<CoinPnl> = states
        .iter()
        .filter(|(_, s)| s.hours_short > 0)
        .map(|(coin, s)| CoinPnl {
            coin: (*coin).to_string(),
            hours_short: s.hours_short,
            funding_income_usd: s.funding,
            fees_usd: s.fees,
            net_pnl_usd: s.funding - s.fees - s.slippage,
        })
        .collect();
    per_coin.sort_by(|a, b| {
        b.net_pnl_usd
            .partial_cmp(&a.net_pnl_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.coin.cmp(&b.coin))
    });
    per_coin.truncate(PER_COIN_TOP_N);

    let out = SimOutput {
        db: cfg.db.clone(),
        days: cfg.days,
        window_start_ts,
        window_end_ts,
        coins_in_db,
        coins_used,
        rows_used,
        hours,
        entry_floor: cfg.entry_floor,
        exit_floor: cfg.exit_floor,
        fee_bps_used: cfg.fee_bps_used(),
        slippage_bps_used: cfg.slippage_bps_used(),
        execution: if cfg.maker { "maker" } else { "taker" },
        fills,
        max_gross_used_usd: peak_gross,
        funding_income_usd,
        fees_usd,
        slippage_usd,
        net_carry_pnl_usd,
        apr_pct,
        per_coin,
    };
    ensure_all_finite(&out)?;
    Ok(out)
}

fn ensure_all_finite(out: &SimOutput) -> Result<()> {
    let checks: [(&str, f64); 6] = [
        ("max_gross_used_usd", out.max_gross_used_usd),
        ("funding_income_usd", out.funding_income_usd),
        ("fees_usd", out.fees_usd),
        ("slippage_usd", out.slippage_usd),
        ("net_carry_pnl_usd", out.net_carry_pnl_usd),
        ("apr_pct", out.apr_pct),
    ];
    for (name, v) in checks {
        if !v.is_finite() {
            bail!("simulator produced a non-finite value for `{name}`");
        }
    }
    for c in &out.per_coin {
        if !c.funding_income_usd.is_finite()
            || !c.fees_usd.is_finite()
            || !c.net_pnl_usd.is_finite()
        {
            bail!("simulator produced a non-finite per-coin value for `{}`", c.coin);
        }
    }
    Ok(())
}

/// Round to 6 decimals for output; normalize -0.0 to 0.0.
fn r6(x: f64) -> f64 {
    let v = (x * 1e6).round() / 1e6;
    if v == 0.0 {
        0.0
    } else {
        v
    }
}

#[derive(Serialize)]
struct CoinJson<'a> {
    coin: &'a str,
    hours_short: u64,
    funding_income_usd: f64,
    fees_usd: f64,
    net_pnl_usd: f64,
}

#[derive(Serialize)]
struct JsonOut<'a> {
    db: &'a str,
    days: u32,
    window_start_ts: i64,
    window_end_ts: i64,
    coins_in_db: usize,
    coins_used: usize,
    rows_used: u64,
    hours: u64,
    entry_floor: f64,
    exit_floor: f64,
    fee_bps_used: f64,
    slippage_bps_used: f64,
    execution: &'static str,
    fills: u64,
    max_gross_used_usd: f64,
    funding_income_usd: f64,
    fees_usd: f64,
    slippage_usd: f64,
    net_carry_pnl_usd: f64,
    apr_pct: f64,
    per_coin: Vec<CoinJson<'a>>,
    not_price_pnl: bool,
}

/// Serialize the run result: exactly one compact JSON object, all floats
/// rounded to 6 decimals. Same input + same flags => byte-identical output.
pub fn render_json(out: &SimOutput) -> Result<String> {
    let json = JsonOut {
        db: &out.db,
        days: out.days,
        window_start_ts: out.window_start_ts,
        window_end_ts: out.window_end_ts,
        coins_in_db: out.coins_in_db,
        coins_used: out.coins_used,
        rows_used: out.rows_used,
        hours: out.hours,
        entry_floor: r6(out.entry_floor),
        exit_floor: r6(out.exit_floor),
        fee_bps_used: r6(out.fee_bps_used),
        slippage_bps_used: r6(out.slippage_bps_used),
        execution: out.execution,
        fills: out.fills,
        max_gross_used_usd: r6(out.max_gross_used_usd),
        funding_income_usd: r6(out.funding_income_usd),
        fees_usd: r6(out.fees_usd),
        slippage_usd: r6(out.slippage_usd),
        net_carry_pnl_usd: r6(out.net_carry_pnl_usd),
        apr_pct: r6(out.apr_pct),
        per_coin: out
            .per_coin
            .iter()
            .map(|c| CoinJson {
                coin: &c.coin,
                hours_short: c.hours_short,
                funding_income_usd: r6(c.funding_income_usd),
                fees_usd: r6(c.fees_usd),
                net_pnl_usd: r6(c.net_pnl_usd),
            })
            .collect(),
        not_price_pnl: true,
    };
    serde_json::to_string(&json).context("serialize result JSON")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Real snapshot's window start (2026-09-12 00:00:00 UTC) — any fixed
    /// hourly grid anchor works; tests only need consistency.
    const BASE_TS: i64 = 1_789_254_000;
    /// Exact dyadic rate (2^-14 = 6.103515625e-05/hour) so hand-computed
    /// sums/means are exact in f64 for threshold-boundary assertions.
    const R_HI: f64 = 6.103_515_625e-5;
    const R_ZERO: f64 = 0.0;

    struct TempDb(PathBuf);
    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn temp_db(tag: &str) -> TempDb {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "mp-hl-fb-{}-{}-{n}.db",
            tag,
            std::process::id()
        ));
        TempDb(path)
    }

    /// rows: (hour_offset, coin, rate). An hour simply absent from the list
    /// models a gap (the real schema has funding_rate REAL NOT NULL, so
    /// missing rows are the only way data is absent).
    fn create_db(path: &Path, rows: &[(i64, &str, f64)]) {
        let conn = Connection::open(path).expect("create test db");
        conn.execute_batch(
            "CREATE TABLE funding_history(
                 ts INTEGER NOT NULL,
                 coin TEXT NOT NULL,
                 funding_rate REAL NOT NULL,
                 premium REAL,
                 PRIMARY KEY(ts, coin)
             );",
        )
        .expect("create table");
        conn.execute_batch("BEGIN;").expect("begin");
        for (h, coin, rate) in rows {
            let ts = BASE_TS + h * HOUR_SECONDS;
            conn.execute(
                "INSERT INTO funding_history(ts, coin, funding_rate, premium) \
                 VALUES (?1, ?2, ?3, NULL)",
                rusqlite::params![ts, coin, rate],
            )
            .expect("insert row");
        }
        conn.execute_batch("COMMIT;").expect("commit");
    }

    fn base_cfg(path: &Path, days: u32) -> SimConfig {
        SimConfig {
            db: path.to_string_lossy().into_owned(),
            days,
            entry_floor: 1.5e-5,
            exit_floor: 1.25e-5,
            taker_fee_bps: 3.5,
            maker_fee_bps: 1.5,
            slippage_bps: 5.0,
            order_usd: 1000.0,
            max_gross_usd: 10_000.0,
            min_coverage: 0.9,
            annualization_hours: 8760.0,
            maker: false,
        }
    }

    fn run_json(cfg: &SimConfig) -> (SimOutput, String) {
        let out = run_sim(cfg).expect("run_sim should succeed");
        let json = render_json(&out).expect("render_json should succeed");
        (out, json)
    }

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-9,
            "expected {expected}, got {actual}"
        );
    }

    /// One coin, 3 days, rate 2e-5 for every hour. The trailing 24h mean at
    /// hour t covers hours t-24..t-1; at t=18 exactly 18 observations exist
    /// (hours 0..17), which meets the ">= 18 of 24 available" rule, so the
    /// short is entered at hour 18 and accrues hours 18..71.
    /// Hand-computed: funding = 54 * 1000 * 2e-5 = 1.08;
    /// commission = 1000 * 3.5/1e4 = 0.35; slippage = 1000 * 5/1e4 = 0.50
    /// (one entry fill); net = 1.08 - 0.35 - 0.50 = 0.23.
    #[test]
    fn synthetic_known_rates_hand_computed() {
        let db = temp_db("known");
        let rows: Vec<(i64, &str, f64)> = (0..72).map(|h| (h, "TEST", 2e-5)).collect();
        create_db(&db.0, &rows);
        let cfg = base_cfg(&db.0, 3);
        let (out, json) = run_json(&cfg);

        assert_eq!(out.execution, "taker");
        assert_eq!(out.coins_in_db, 1);
        assert_eq!(out.coins_used, 1);
        assert_eq!(out.rows_used, 72);
        assert_eq!(out.hours, 72);
        assert_eq!(out.window_start_ts, BASE_TS);
        assert_eq!(out.window_end_ts, BASE_TS + 71 * HOUR_SECONDS);
        assert_eq!(out.fills, 1);
        assert_close(out.max_gross_used_usd, 1000.0);
        assert_close(out.funding_income_usd, 1.08);
        assert_close(out.fees_usd, 0.35);
        assert_close(out.slippage_usd, 0.50);
        assert_close(out.net_carry_pnl_usd, 0.23);
        assert_close(out.fee_bps_used, 3.5);
        assert_close(out.slippage_bps_used, 5.0);
        // APR = 0.23 / 1000 * (8760 / 72) * 100 = 2.798333...%
        assert_close(out.apr_pct, 0.23 / 1000.0 * (8760.0 / 72.0) * 100.0);
        assert_eq!(out.per_coin.len(), 1);
        let pc = &out.per_coin[0];
        assert_eq!(pc.coin, "TEST");
        assert_eq!(pc.hours_short, 54);
        assert_close(pc.funding_income_usd, 1.08);
        assert_close(pc.fees_usd, 0.35);
        assert_close(pc.net_pnl_usd, 0.23);
        assert!(json.contains("\"not_price_pnl\":true"));
        assert!(json.contains("\"execution\":\"taker\""));
    }

    /// Maker flag: maker fees, zero slippage, execution = "maker".
    /// Same data as the hand-computed test: cost = 1000 * 1.5/1e4 = 0.15,
    /// net = 1.08 - 0.15 = 0.93.
    #[test]
    fn maker_case_uses_maker_fees_and_zero_slippage() {
        let db = temp_db("maker");
        let rows: Vec<(i64, &str, f64)> = (0..72).map(|h| (h, "TEST", 2e-5)).collect();
        create_db(&db.0, &rows);
        let mut cfg = base_cfg(&db.0, 3);
        cfg.maker = true;
        let (out, json) = run_json(&cfg);

        assert_eq!(out.execution, "maker");
        assert_close(out.fee_bps_used, 1.5);
        assert_close(out.slippage_bps_used, 0.0);
        assert_close(out.funding_income_usd, 1.08);
        assert_close(out.fees_usd, 0.15);
        assert_close(out.slippage_usd, 0.0);
        assert_close(out.net_carry_pnl_usd, 0.93);
        assert!(json.contains("\"execution\":\"maker\""));
        assert!(json.contains("\"slippage_usd\":0.0"));
    }

    /// A coin covering only half the window hours must be excluded by
    /// --min-coverage 0.9, even though its rates would be very profitable.
    #[test]
    fn sparse_coin_below_min_coverage_excluded() {
        let db = temp_db("sparse");
        let mut rows: Vec<(i64, &str, f64)> = (0..72).map(|h| (h, "GOOD", 2e-5)).collect();
        // SPARSE: only every other hour (36/72 = 0.5 coverage).
        for h in (0..72).step_by(2) {
            rows.push((h, "SPARSE", 5e-5));
        }
        create_db(&db.0, &rows);
        let cfg = base_cfg(&db.0, 3);
        let (out, json) = run_json(&cfg);

        assert_eq!(out.coins_in_db, 2);
        assert_eq!(out.coins_used, 1);
        assert_eq!(out.rows_used, 72);
        assert_close(out.funding_income_usd, 1.08); // GOOD only
        assert_eq!(out.per_coin.len(), 1);
        assert_eq!(out.per_coin[0].coin, "GOOD");
        assert!(json.contains("GOOD"));
        assert!(!json.contains("SPARSE"));
    }

    /// Same input + same flags => byte-identical serialized output.
    #[test]
    fn determinism_run_twice_byte_identical() {
        let db = temp_db("determinism");
        let mut rows: Vec<(i64, &str, f64)> = Vec::new();
        for coin in ["AAA", "BBB"] {
            for h in 0..72 {
                rows.push((h, coin, if h % 7 == 3 { 3e-5 } else { 2e-5 }));
            }
        }
        create_db(&db.0, &rows);
        let cfg = base_cfg(&db.0, 3);
        let (_, json_a) = run_json(&cfg);
        let (_, json_b) = run_json(&cfg);
        assert_eq!(json_a, json_b);
    }

    /// Portfolio cap: 3 qualifying coins but --max-gross-usd 2000 with
    /// --order-usd 1000 admits exactly 2 concurrent shorts (deterministic
    /// skip, coins processed in sorted order). Peak gross = 2000.
    #[test]
    fn max_gross_cap_respected() {
        let db = temp_db("cap");
        let mut rows: Vec<(i64, &str, f64)> = Vec::new();
        for coin in ["AAA", "BBB", "CCC"] {
            for h in 0..72 {
                rows.push((h, coin, 2e-5));
            }
        }
        create_db(&db.0, &rows);
        let mut cfg = base_cfg(&db.0, 3);
        cfg.max_gross_usd = 2000.0;
        let (out, _) = run_json(&cfg);

        assert_eq!(out.coins_used, 3);
        assert_eq!(out.fills, 2); // CCC can never enter while cap is held
        assert_close(out.max_gross_used_usd, 2000.0);
        assert!(out.max_gross_used_usd <= cfg.max_gross_usd + 1e-9);
        // Only the two admitted coins appear in per_coin.
        assert_eq!(out.per_coin.len(), 2);
        assert!(out.per_coin.iter().all(|c| c.coin != "CCC"));
        // 2 concurrent shorts for hours 18..71: 2 * 54 * 1000 * 2e-5 = 2.16.
        assert_close(out.funding_income_usd, 2.16);
    }

    /// A gap hour (missing rate) credits 0 funding — never interpolated —
    /// and the missing observation does not break the trailing mean.
    /// Entry at hour 18; 54 short hours minus the gap = 53 accruals:
    /// 53 * 1000 * 2e-5 = 1.06.
    #[test]
    fn gap_hour_credits_zero_funding() {
        let db = temp_db("gap");
        let rows: Vec<(i64, &str, f64)> = (0..72)
            .filter(|&h| h != 40)
            .map(|h| (h, "TEST", 2e-5))
            .collect();
        create_db(&db.0, &rows);
        let cfg = base_cfg(&db.0, 3);
        let (out, _) = run_json(&cfg);

        assert_eq!(out.rows_used, 71);
        assert_eq!(out.fills, 1); // entered once, never exited
        assert_close(out.funding_income_usd, 1.06);
        let pc = &out.per_coin[0];
        assert_eq!(pc.hours_short, 54); // still short during the gap hour
        assert_close(pc.funding_income_usd, 1.06);
    }

    /// Threshold boundaries, exactly representable via dyadic rates
    /// (R_HI = 2^-14): entry fires when mean == entry_floor (>=), the
    /// position is HELD while mean == exit_floor (exit is strict <), and the
    /// exit fires once mean drops strictly below exit_floor.
    /// Timeline: R_HI hours 0..35, zero hours 36..71; entry_floor = R_HI,
    /// exit_floor = R_HI/2. Entry at ts-hour 18 (18 obs of hours 0..17,
    /// mean == entry_floor exactly). Mean at ts-hour 48 == exit_floor
    /// exactly (hold); mean at ts-hour 49 (13 zero obs) < exit_floor
    /// (exit). Short ts-hours 18..48 => 31 short hours, of which 18
    /// (hours 18..35) have positive rates and hours 36..48 credit 0.
    #[test]
    fn entry_and_exit_threshold_boundaries() {
        let db = temp_db("boundary");
        let rows: Vec<(i64, &str, f64)> = (0..72)
            .map(|h| {
                if h < 36 {
                    (h, "TEST", R_HI)
                } else {
                    (h, "TEST", R_ZERO)
                }
            })
            .collect();
        create_db(&db.0, &rows);
        let mut cfg = base_cfg(&db.0, 3);
        cfg.entry_floor = R_HI;
        cfg.exit_floor = R_HI / 2.0;
        let (out, _) = run_json(&cfg);

        assert_eq!(out.fills, 2); // one entry + one exit
        let pc = &out.per_coin[0];
        assert_eq!(pc.hours_short, 31);
        assert_close(pc.funding_income_usd, 18.0 * 1000.0 * R_HI);
        assert_close(out.funding_income_usd, 18.0 * 1000.0 * R_HI);
        assert_close(out.fees_usd, 2.0 * 1000.0 * 3.5 / 1e4); // entry + exit
        assert_close(out.slippage_usd, 2.0 * 1000.0 * 5.0 / 1e4);
        assert_close(
            out.net_carry_pnl_usd,
            18.0 * 1000.0 * R_HI - 2.0 * 1000.0 * 8.5 / 1e4,
        );
    }

    /// Coverage boundary: with 72 window hours and --min-coverage 0.9, a
    /// coin with 65 rows (0.9027...) is included, one with 64 rows
    /// (0.888...) is excluded. The window ends at the newest ts in the DB
    /// (COIN65's last row), so COIN65 enters at its ts-hour 18 (18 obs of
    /// hours 0..17) and accrues hours 18..64 = 47 * 0.02.
    #[test]
    fn coverage_boundary_inclusion() {
        let db = temp_db("coverage");
        let mut rows: Vec<(i64, &str, f64)> = (0..65).map(|h| (h, "COIN65", 2e-5)).collect();
        for h in 0..64 {
            rows.push((h, "COIN64", 2e-5));
        }
        create_db(&db.0, &rows);
        let cfg = base_cfg(&db.0, 3);
        let (out, _) = run_json(&cfg);

        assert_eq!(out.coins_in_db, 2);
        assert_eq!(out.coins_used, 1);
        assert_eq!(out.rows_used, 65);
        assert_close(out.funding_income_usd, 47.0 * 1000.0 * 2e-5);
        assert_eq!(out.per_coin.len(), 1);
        assert_eq!(out.per_coin[0].coin, "COIN65");
        assert_eq!(out.per_coin[0].hours_short, 47);
    }

    /// A coin with no rows at all in the window (fully delisted) is simply
    /// absent; the run still succeeds deterministically.
    #[test]
    fn delisted_coin_absent_from_window() {
        let db = temp_db("delisted");
        let rows: Vec<(i64, &str, f64)> = (0..72).map(|h| (h, "ALIVE", 2e-5)).collect();
        create_db(&db.0, &rows);
        let cfg = base_cfg(&db.0, 3);
        let (out, json) = run_json(&cfg);

        assert_eq!(out.coins_in_db, 1);
        assert_eq!(out.coins_used, 1);
        assert!(json.contains("ALIVE"));
    }

    /// Missing DB file must fail loudly (nonzero path: Err, never empty JSON).
    #[test]
    fn missing_db_is_an_error() {
        let cfg = base_cfg(Path::new("/nonexistent/mp-hl-fb/does-not-exist.db"), 3);
        let err = run_sim(&cfg).expect_err("must fail on missing db");
        let msg = format!("{err:#}");
        assert!(msg.contains("open funding snapshot"), "got: {msg}");
    }

    /// An unreadable (non-SQLite) snapshot must fail loudly too.
    #[test]
    fn corrupt_db_is_an_error() {
        let db = temp_db("corrupt");
        std::fs::write(&db.0, b"this is definitely not a sqlite database").expect("write junk");
        let cfg = base_cfg(&db.0, 3);
        let err = run_sim(&cfg).expect_err("must fail on corrupt db");
        let msg = format!("{err:#}");
        assert!(!msg.is_empty());
    }
}
