//! CLI wrapper around `mp_hl_funding_backtest`.
//!
//! Prints exactly one JSON result object between the markers
//! `===PNL-JSON-START===` / `===PNL-JSON-END===` on stdout. Honesty scope:
//! funding-income leg + fees + slippage only; the snapshot has no price
//! series, so short-leg price PnL is NOT modeled (`not_price_pnl: true`).
//! Analytics-only: no orders are routed, so no risk-gate call is performed.

use std::process::ExitCode;

use anyhow::Result;
use clap::Parser;

use mp_hl_funding_backtest::{render_json, run_sim, SimConfig, DEFAULT_DB};

#[derive(Parser, Debug)]
#[command(
    name = "hl-funding-backtest",
    about = "Deterministic short funding-carry vector backtest over a frozen Hyperliquid \
             funding snapshot. Models ONLY the funding income leg + fees + slippage; \
             short-leg price PnL is NOT modeled (not_price_pnl: true)."
)]
struct Cli {
    /// Path to the frozen SQLite funding snapshot (opened read-only).
    #[arg(long, default_value = DEFAULT_DB)]
    db: String,

    /// Window length in days (45 = the snapshot's full 45-day window).
    #[arg(long, default_value_t = 45)]
    days: u32,

    /// Enter short when trailing 24h mean hourly rate >= this (fraction).
    #[arg(long, default_value_t = 0.000_015)]
    entry_floor: f64,

    /// Exit short when trailing 24h mean hourly rate < this (strict).
    #[arg(long, default_value_t = 0.000_012_5)]
    exit_floor: f64,

    /// Taker fee, basis points of notional, charged per fill.
    #[arg(long, default_value_t = 3.5)]
    taker_fee_bps: f64,

    /// Maker fee, basis points of notional, charged per fill.
    #[arg(long, default_value_t = 1.5)]
    maker_fee_bps: f64,

    /// Slippage, basis points of notional, charged per fill (taker only).
    #[arg(long, default_value_t = 5.0)]
    slippage_bps: f64,

    /// Notional per coin per position, USD.
    #[arg(long, default_value_t = 1000.0)]
    order_usd: f64,

    /// Portfolio cap on gross short notional, USD.
    #[arg(long, default_value_t = 10_000.0)]
    max_gross_usd: f64,

    /// Minimum fraction of window hours a coin must cover to be traded.
    #[arg(long, default_value_t = 0.9)]
    min_coverage: f64,

    /// Hours per year used for annualization.
    #[arg(long, default_value_t = 8760.0)]
    annualization_hours: f64,

    /// Use maker fees with zero slippage instead of taker execution.
    #[arg(long, default_value_t = false)]
    maker: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let cfg = SimConfig {
        db: cli.db,
        days: cli.days,
        entry_floor: cli.entry_floor,
        exit_floor: cli.exit_floor,
        taker_fee_bps: cli.taker_fee_bps,
        maker_fee_bps: cli.maker_fee_bps,
        slippage_bps: cli.slippage_bps,
        order_usd: cli.order_usd,
        max_gross_usd: cli.max_gross_usd,
        min_coverage: cli.min_coverage,
        annualization_hours: cli.annualization_hours,
        maker: cli.maker,
    };
    match run(&cfg) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cfg: &SimConfig) -> Result<()> {
    let out = run_sim(cfg)?;
    println!("===PNL-JSON-START===");
    println!("{}", render_json(&out)?);
    println!("===PNL-JSON-END===");
    Ok(())
}
