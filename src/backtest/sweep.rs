//! `sweep` command: cartesian grid-search over spacing × levels for one symbol
//! over one candle history, each combination replayed through the plan/03
//! backtester with `capital`-driven sizing, ranked by net realized PnL
//! (max drawdown as tiebreaker). Replaces heuristic spacing/levels guesses with
//! replayed evidence. See `plan/08-grid-gains.md` step 5.

use super::{BacktestConfig, run_backtest};
use crate::api::candles::Candle;
use chrono::Utc;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

const ATR_PERIOD: usize = 14;
const ATR_MULTIPLIERS: [f64; 5] = [0.25, 0.5, 1.0, 1.5, 2.0];

pub struct SweepConfig {
    pub symbol: String,
    pub timeframe: String,
    /// Quote budget each combination sizes from (the grid's `capital` option).
    pub capital: f64,
    /// Explicit spacing candidates; when `None` they derive from the candle
    /// set's ATR × `ATR_MULTIPLIERS`.
    pub spacings: Option<Vec<f64>>,
    pub levels: Vec<u32>,
    pub spread: f64,
    pub maker_fee: f64,
    pub taker_fee: f64,
    pub start_quote_balance: f64,
    pub start_base_balance: f64,
}

pub struct SweepRow {
    pub spacing: f64,
    pub levels: u32,
    pub realized_pnl_net: f64,
    pub fees_paid: f64,
    pub trades: usize,
    pub max_drawdown: f64,
    pub ending_equity: f64,
}

pub struct SweepResult {
    pub symbol: String,
    pub timeframe: String,
    pub capital: f64,
    pub candles: usize,
    pub start_ts: i64,
    pub end_ts: i64,
    pub atr_derived_spacings: bool,
    /// Ranked best-first: net realized PnL desc, then max drawdown asc.
    pub rows: Vec<SweepRow>,
}

/// Replay every spacing × levels combination over the (single, shared) candle
/// set and rank the results. Deterministic: identical inputs yield an
/// identical ranking.
pub fn run_sweep(cfg: &SweepConfig, candles_in: &[Candle]) -> Result<SweepResult, String> {
    if candles_in.is_empty() {
        return Err("No candles to sweep".to_string());
    }
    if cfg.levels.is_empty() {
        return Err("No levels candidates".to_string());
    }

    let mut candles: Vec<Candle> = candles_in.to_vec();
    candles.sort_by_key(|c| c.timestamp);

    let (spacings, atr_derived) = match &cfg.spacings {
        Some(s) if !s.is_empty() => (s.clone(), false),
        _ => (atr_spacing_candidates(&candles)?, true),
    };
    if let Some(bad) = spacings.iter().find(|s| **s <= 0.0) {
        return Err(format!("Spacing candidate {} must be positive", bad));
    }

    let mut rows: Vec<SweepRow> = Vec::with_capacity(spacings.len() * cfg.levels.len());
    for &spacing in &spacings {
        for &levels in &cfg.levels {
            let mut options: HashMap<String, String> = HashMap::new();
            options.insert("spacing".to_string(), format!("{:.8}", spacing));
            options.insert("levels".to_string(), levels.to_string());
            options.insert("capital".to_string(), format!("{:.8}", cfg.capital));
            options.insert("allow_unprofitable".to_string(), "false".to_string());
            options.insert("trend_filter".to_string(), "ema".to_string());

            let bt = BacktestConfig {
                symbol: cfg.symbol.clone(),
                algorithm: "grid".to_string(),
                options,
                timeframe: cfg.timeframe.clone(),
                candles_limit: candles.len(),
                from_file: None,
                start_quote_balance: cfg.start_quote_balance,
                start_base_balance: cfg.start_base_balance,
                spread: cfg.spread,
                maker_fee: cfg.maker_fee,
                taker_fee: cfg.taker_fee,
            };
            let report = run_backtest(&bt, &candles)?;
            rows.push(SweepRow {
                spacing,
                levels,
                realized_pnl_net: report.realized_pnl_net,
                fees_paid: report.fees_paid,
                trades: report.trades.len(),
                max_drawdown: report.max_drawdown,
                ending_equity: report.ending_equity,
            });
        }
    }

    rows.sort_by(|a, b| {
        b.realized_pnl_net
            .partial_cmp(&a.realized_pnl_net)
            .unwrap_or(Ordering::Equal)
            .then(
                a.max_drawdown
                    .partial_cmp(&b.max_drawdown)
                    .unwrap_or(Ordering::Equal),
            )
    });

    Ok(SweepResult {
        symbol: cfg.symbol.clone(),
        timeframe: cfg.timeframe.clone(),
        capital: cfg.capital,
        candles: candles.len(),
        start_ts: candles.first().unwrap().timestamp,
        end_ts: candles.last().unwrap().timestamp,
        atr_derived_spacings: atr_derived,
        rows,
    })
}

/// Spacing candidates from the candle set's ATR: leading (oldest) window, like
/// the backtester's own spacing injection, so candidates carry no look-ahead.
fn atr_spacing_candidates(candles: &[Candle]) -> Result<Vec<f64>, String> {
    // compute_atr expects newest-first (it reverses internally).
    let lead: Vec<Candle> = candles.iter().take(ATR_PERIOD + 1).rev().cloned().collect();
    let atr = crate::algorithm::atr::compute_atr(&lead, ATR_PERIOD)?;
    if atr <= 0.0 {
        return Err(format!(
            "ATR over the leading candles is {:.8} (<= 0) — pass explicit --spacings",
            atr
        ));
    }
    Ok(ATR_MULTIPLIERS.iter().map(|m| atr * m).collect())
}

impl SweepResult {
    fn spawn_line(&self) -> String {
        match self.rows.first() {
            Some(top) => format!(
                "runner -s {} -a grid -o spacing={:.8} levels={} capital={:.2} trend_filter=ema trend_threshold=0.005 trend_timeframe={} allow_unprofitable=false",
                self.symbol, top.spacing, top.levels, self.capital, self.timeframe
            ),
            None => String::new(),
        }
    }

    /// Ranked table logged to the TUI, top result highlighted with a
    /// ready-to-paste spawn line (add max_position/stop options from scout).
    pub fn render_console(&self) -> String {
        let mut out = format!(
            "Sweep {} [grid] {} candles ({} – {}), capital {:.2} — {} combination(s){}",
            self.symbol,
            self.candles,
            super::fmt_ts(self.start_ts),
            super::fmt_ts(self.end_ts),
            self.capital,
            self.rows.len(),
            if self.atr_derived_spacings { ", ATR-derived spacings" } else { "" },
        );
        out.push_str("\n  rank  spacing        levels  realized      fees        trades  maxDD    equity");
        for (i, r) in self.rows.iter().enumerate() {
            out.push_str(&format!(
                "\n  {:<4}  {:<13.8}  {:<6}  {:<+12.4}  {:<10.4}  {:<6}  {:<6.2}%  {:.2}",
                i + 1,
                r.spacing,
                r.levels,
                r.realized_pnl_net,
                r.fees_paid,
                r.trades,
                r.max_drawdown * 100.0,
                r.ending_equity,
            ));
        }
        if !self.rows.is_empty() {
            out.push_str(&format!("\n  Top config → {}", self.spawn_line()));
        }
        out
    }

    /// Full markdown report written to disk.
    pub fn render_markdown(&self) -> String {
        let mut md = String::new();
        md.push_str(&format!("# T.E.D Sweep — {}\n\n", self.symbol));
        md.push_str("| Field | Value |\n|---|---|\n");
        md.push_str(&format!(
            "| Generated | {} UTC |\n",
            Utc::now().format("%Y-%m-%d %H:%M:%S")
        ));
        md.push_str(&format!("| Timeframe | {} |\n", self.timeframe));
        md.push_str(&format!("| Candles | {} |\n", self.candles));
        md.push_str(&format!(
            "| Period | {} – {} UTC |\n",
            super::fmt_ts(self.start_ts),
            super::fmt_ts(self.end_ts)
        ));
        md.push_str(&format!("| Capital | {:.2} |\n", self.capital));
        md.push_str(&format!(
            "| Spacings | {} |\n",
            if self.atr_derived_spacings { "ATR-derived" } else { "user-provided" }
        ));
        md.push_str(&format!("| Combinations | {} |\n\n", self.rows.len()));

        if !self.rows.is_empty() {
            md.push_str("## Top config\n\n```\n");
            md.push_str(&self.spawn_line());
            md.push_str("\n```\n\n");
        }

        md.push_str("## Ranked results\n\n");
        md.push_str("| Rank | Spacing | Levels | Realized (net) | Fees | Trades | Max DD | Ending equity |\n");
        md.push_str("|---|---|---|---|---|---|---|---|\n");
        for (i, r) in self.rows.iter().enumerate() {
            md.push_str(&format!(
                "| {} | {:.8} | {} | {:+.8} | {:.8} | {} | {:.2}% | {:.8} |\n",
                i + 1,
                r.spacing,
                r.levels,
                r.realized_pnl_net,
                r.fees_paid,
                r.trades,
                r.max_drawdown * 100.0,
                r.ending_equity,
            ));
        }
        md.push('\n');
        md
    }
}

/// Write the sweep report next to the backtest reports, returning the path.
pub fn write_sweep_report(result: &SweepResult) -> std::io::Result<PathBuf> {
    let base = format!(
        "sweep_{}_{}",
        result.symbol.replace(':', "_"),
        Utc::now().format("%Y-%m-%d")
    );
    let mut path = PathBuf::from(format!("{}.md", base));
    let mut n = 2u32;
    while path.exists() {
        path = PathBuf::from(format!("{}_{}.md", base, n));
        n += 1;
    }
    fs::write(&path, result.render_markdown())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candle(ts: i64, open: f64, close: f64, high: f64, low: f64) -> Candle {
        Candle { timestamp: ts, open, close, high, low, volume: 1.0 }
    }

    fn oscillation() -> Vec<Candle> {
        vec![
            candle(1, 100.0, 100.0, 100.0, 100.0),
            candle(2, 100.0, 90.0, 100.0, 90.0),
            candle(3, 90.0, 100.0, 100.0, 90.0),
            candle(4, 100.0, 90.0, 100.0, 90.0),
            candle(5, 90.0, 100.0, 100.0, 90.0),
        ]
    }

    fn cfg(spacings: Option<Vec<f64>>, levels: Vec<u32>) -> SweepConfig {
        SweepConfig {
            symbol: "TESTUSD".to_string(),
            timeframe: "1h".to_string(),
            capital: 600.0,
            spacings,
            levels,
            spread: 0.0,
            maker_fee: 0.0,
            taker_fee: 0.0,
            start_quote_balance: 1000.0,
            start_base_balance: 0.0,
        }
    }

    #[test]
    fn sweep_is_deterministic_and_ranked() {
        let candles = oscillation();
        let c = cfg(Some(vec![10.0, 5.0]), vec![1, 2]);
        let a = run_sweep(&c, &candles).unwrap();
        let b = run_sweep(&c, &candles).unwrap();

        assert_eq!(a.rows.len(), 4, "2 spacings × 2 levels");
        assert_eq!(a.rows.len(), b.rows.len());
        for (ra, rb) in a.rows.iter().zip(b.rows.iter()) {
            assert_eq!(ra.spacing.to_bits(), rb.spacing.to_bits());
            assert_eq!(ra.levels, rb.levels);
            assert_eq!(ra.realized_pnl_net.to_bits(), rb.realized_pnl_net.to_bits());
            assert_eq!(ra.max_drawdown.to_bits(), rb.max_drawdown.to_bits());
            assert_eq!(ra.trades, rb.trades);
        }
        for w in a.rows.windows(2) {
            assert!(
                w[0].realized_pnl_net >= w[1].realized_pnl_net,
                "ranking must be non-increasing in realized PnL"
            );
        }
    }

    #[test]
    fn spacings_derive_from_atr_when_omitted() {
        let mut candles = Vec::new();
        for i in 0..20 {
            candles.push(candle(i, 100.0, 100.0, 101.0, 99.0));
        }
        let result = run_sweep(&cfg(None, vec![1]), &candles).unwrap();
        assert!(result.atr_derived_spacings);
        assert_eq!(result.rows.len(), ATR_MULTIPLIERS.len());
    }

    #[test]
    #[ignore]
    fn replay_diagnosis_window() {
        // Manual harness: set TED_REPLAY_FILE to a candle file and run
        // `cargo test replay_diagnosis_window -- --ignored --nocapture`.
        let Ok(path) = std::env::var("TED_REPLAY_FILE") else {
            println!("TED_REPLAY_FILE not set — skipping");
            return;
        };
        let candles = crate::backtest::load_candles_from_file(&path).unwrap();

        let mut c = cfg(None, vec![2, 3, 4, 6]);
        c.symbol = "SOLUSD".to_string();
        c.timeframe = "30m".to_string();
        c.capital = 118.91;
        c.start_quote_balance = 118.91;
        let result = run_sweep(&c, &candles).unwrap();
        println!("{}", result.render_console());

        // The live config the diagnosis window actually ran.
        let mut options: HashMap<String, String> = HashMap::new();
        options.insert("spacing".to_string(), "1.97".to_string());
        options.insert("levels".to_string(), "2".to_string());
        options.insert("capital".to_string(), "118.91".to_string());
        let bt = BacktestConfig {
            symbol: "SOLUSD".to_string(),
            algorithm: "grid".to_string(),
            options,
            timeframe: "30m".to_string(),
            candles_limit: candles.len(),
            from_file: None,
            start_quote_balance: 118.91,
            start_base_balance: 0.0,
            spread: 0.0,
            maker_fee: 0.0,
            taker_fee: 0.0,
        };
        let report = run_backtest(&bt, &candles).unwrap();
        println!("--- live config replay ---");
        println!("{}", report.render_console());
        let mut last_fill: Option<i64> = None;
        let mut max_gap_hours = 0.0f64;
        for t in &report.trades {
            if let Some(prev) = last_fill {
                max_gap_hours = max_gap_hours.max((t.timestamp - prev) as f64 / 3_600_000.0);
            }
            last_fill = Some(t.timestamp);
            println!(
                "  {} {} @ {:.2} realized {:+.4}",
                crate::backtest::fmt_ts(t.timestamp),
                if t.is_buy { "BUY " } else { "SELL" },
                t.price,
                t.realized
            );
        }
        println!("max gap between fills: {:.1}h over {} trades", max_gap_hours, report.trades.len());
    }

    #[test]
    fn report_renders_ranked_table_and_spawn_line() {
        let candles = oscillation();
        let result = run_sweep(&cfg(Some(vec![10.0]), vec![1]), &candles).unwrap();
        let md = result.render_markdown();
        assert!(md.contains("# T.E.D Sweep — TESTUSD"));
        assert!(md.contains("## Ranked results"));
        assert!(md.contains("runner -s TESTUSD -a grid -o spacing="));
        assert!(result.render_console().lines().count() >= 3);
    }
}
