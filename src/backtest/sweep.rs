//! `sweep` command: cartesian grid-search over spacing × levels for one symbol
//! over one candle history, each combination replayed through the plan/03
//! backtester with `capital`-driven sizing. Since plan/11 the sweep is
//! **walk-forward**: each combination is tuned on the first ~70% of candles and
//! ranked by its net realized PnL on the held-out last ~30% (max drawdown as
//! tiebreaker), with configs that look great in tuning but lose out-of-sample
//! flagged `overfit?`. Spacing is also reported as `atr_multiplier` so the
//! recommended config stays volatility-adaptive instead of pinning an absolute
//! spacing. See `plan/08-grid-gains.md` step 5 and `plan/11`.

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
    /// `spacing / ATR` over the candle set's leading window — the
    /// volatility-relative knob the runner should be configured with (plan/11).
    /// `None` when the ATR could not be computed for user-provided spacings.
    pub atr_multiplier: Option<f64>,
    pub levels: u32,
    /// Tuning half (first ~70% of candles) — context, not the ranking basis.
    pub tune_realized: f64,
    pub tune_trades: usize,
    /// Validation half (last ~30%) — the ranking basis.
    pub realized_pnl_net: f64,
    pub fees_paid: f64,
    pub trades: usize,
    pub max_drawdown: f64,
    pub ending_equity: f64,
    /// Top-quartile tuning result that loses money out-of-sample.
    pub overfit: bool,
}

pub struct SweepResult {
    pub symbol: String,
    pub timeframe: String,
    pub capital: f64,
    pub candles: usize,
    pub tune_candles: usize,
    pub start_ts: i64,
    pub end_ts: i64,
    pub atr_derived_spacings: bool,
    /// Ranked best-first: validation net realized PnL desc, then validation
    /// max drawdown asc.
    pub rows: Vec<SweepRow>,
}

/// Fraction of the candle set used for tuning; the remainder validates.
const TUNE_FRAC: f64 = 0.7;

/// Replay every spacing × levels combination walk-forward over the (single,
/// shared) candle set and rank by validation-half results. Deterministic:
/// identical inputs yield an identical ranking.
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
    let atr = leading_atr(&candles).ok();

    let split = (((candles.len() as f64) * TUNE_FRAC).round() as usize)
        .clamp(1, candles.len().saturating_sub(1).max(1));
    let (tune_half, validate_half) = candles.split_at(split);
    let validate_half = if validate_half.is_empty() {
        // Degenerate candle sets (tests, tiny files): validate on the whole set.
        tune_half
    } else {
        validate_half
    };

    let replay = |window: &[Candle], spacing: f64, levels: u32| -> Result<_, String> {
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
            candles_limit: window.len(),
            from_file: None,
            start_quote_balance: cfg.start_quote_balance,
            start_base_balance: cfg.start_base_balance,
            spread: cfg.spread,
            maker_fee: cfg.maker_fee,
            taker_fee: cfg.taker_fee,
        };
        run_backtest(&bt, window)
    };

    let mut rows: Vec<SweepRow> = Vec::with_capacity(spacings.len() * cfg.levels.len());
    for &spacing in &spacings {
        for &levels in &cfg.levels {
            let tune = replay(tune_half, spacing, levels)?;
            let validate = replay(validate_half, spacing, levels)?;
            rows.push(SweepRow {
                spacing,
                atr_multiplier: atr.map(|a| spacing / a),
                levels,
                tune_realized: tune.realized_pnl_net,
                tune_trades: tune.trades.len(),
                realized_pnl_net: validate.realized_pnl_net,
                fees_paid: validate.fees_paid,
                trades: validate.trades.len(),
                max_drawdown: validate.max_drawdown,
                ending_equity: validate.ending_equity,
                overfit: false,
            });
        }
    }

    // Overfit flag: top tuning quartile that loses money on the held-out half.
    let mut tune_scores: Vec<f64> = rows.iter().map(|r| r.tune_realized).collect();
    tune_scores.sort_by(|a, b| b.partial_cmp(a).unwrap_or(Ordering::Equal));
    let quartile_cutoff = tune_scores
        .get((tune_scores.len().saturating_sub(1)) / 4)
        .copied()
        .unwrap_or(f64::MAX);
    for row in &mut rows {
        row.overfit = row.tune_realized >= quartile_cutoff && row.realized_pnl_net < 0.0;
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
        tune_candles: tune_half.len(),
        start_ts: candles.first().unwrap().timestamp,
        end_ts: candles.last().unwrap().timestamp,
        atr_derived_spacings: atr_derived,
        rows,
    })
}

/// ATR over the leading (oldest) window of the candle set — no look-ahead.
fn leading_atr(candles: &[Candle]) -> Result<f64, String> {
    let lead: Vec<Candle> = candles.iter().take(ATR_PERIOD + 1).rev().cloned().collect();
    let atr = crate::algorithm::atr::compute_atr(&lead, ATR_PERIOD)?;
    if atr <= 0.0 {
        return Err(format!("ATR over the leading candles is {:.8} (<= 0)", atr));
    }
    Ok(atr)
}

/// Spacing candidates from the candle set's ATR: leading (oldest) window, like
/// the backtester's own spacing injection, so candidates carry no look-ahead.
fn atr_spacing_candidates(candles: &[Candle]) -> Result<Vec<f64>, String> {
    let atr = leading_atr(candles).map_err(|e| format!("{} — pass explicit --spacings", e))?;
    Ok(ATR_MULTIPLIERS.iter().map(|m| atr * m).collect())
}

impl SweepResult {
    /// The plan/09 grid knobs, spelled out so a config never silently ships
    /// with an unintended default (the Jul 7 live spawn dropped the risk set).
    fn default_knobs(&self) -> String {
        "min_profit_frac=0.001 downtrend_levels=1 downtrend_qty_frac=0.5 \
         max_inventory_frac=0.75 max_inventory_frac_down=0.5 max_drawdown_pct=0.2 \
         trend_filter=ema trend_threshold=0.005 allow_unprofitable=false"
            .to_string()
    }

    fn spawn_line(&self) -> String {
        match self.rows.first() {
            // The recommendation is the volatility-relative multiplier, not an
            // absolute spacing — the runner recomputes spacing from live ATR
            // and keeps refreshing it (plan/11).
            Some(top) => match top.atr_multiplier {
                Some(mult) => format!(
                    "runner -s {} -a grid -o atr_multiplier={:.4} atr_timeframe={} atr_period=14 levels={} capital={:.2} trend_timeframe={} {}",
                    self.symbol, mult, self.timeframe, top.levels, self.capital, self.timeframe, self.default_knobs()
                ),
                None => format!(
                    "runner -s {} -a grid -o spacing={:.8} levels={} capital={:.2} trend_timeframe={} {}",
                    self.symbol, top.spacing, top.levels, self.capital, self.timeframe, self.default_knobs()
                ),
            },
            None => String::new(),
        }
    }

    /// Ranked table logged to the TUI, top result highlighted with a
    /// ready-to-paste spawn line (add max_position/stop options from scout).
    pub fn render_console(&self) -> String {
        let mut out = format!(
            "Sweep {} [grid] {} candles ({} – {}), capital {:.2} — {} combination(s){}, walk-forward tune {} / validate {}",
            self.symbol,
            self.candles,
            super::fmt_ts(self.start_ts),
            super::fmt_ts(self.end_ts),
            self.capital,
            self.rows.len(),
            if self.atr_derived_spacings { ", ATR-derived spacings" } else { "" },
            self.tune_candles,
            self.candles - self.tune_candles,
        );
        out.push_str("\n  rank  spacing        ATR×    levels  tune          validated     fees        trades  maxDD    equity");
        for (i, r) in self.rows.iter().enumerate() {
            out.push_str(&format!(
                "\n  {:<4}  {:<13.8}  {:<6}  {:<6}  {:<+12.4}  {:<+12.4}  {:<10.4}  {:<6}  {:<6.2}%  {:.2}{}",
                i + 1,
                r.spacing,
                r.atr_multiplier
                    .map(|m| format!("{:.2}", m))
                    .unwrap_or_else(|| "—".to_string()),
                r.levels,
                r.tune_realized,
                r.realized_pnl_net,
                r.fees_paid,
                r.trades,
                r.max_drawdown * 100.0,
                r.ending_equity,
                if r.overfit { "  overfit?" } else { "" },
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
        md.push_str(&format!(
            "| Candles | {} (walk-forward: tune {} / validate {}) |\n",
            self.candles,
            self.tune_candles,
            self.candles - self.tune_candles
        ));
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

        md.push_str("## Ranked results (by validation-half realized PnL)\n\n");
        md.push_str("| Rank | Spacing | ATR× | Levels | Tune realized (trades) | Validated realized | Fees | Trades | Max DD | Ending equity | Flags |\n");
        md.push_str("|---|---|---|---|---|---|---|---|---|---|---|\n");
        for (i, r) in self.rows.iter().enumerate() {
            md.push_str(&format!(
                "| {} | {:.8} | {} | {} | {:+.8} ({}) | {:+.8} | {:.8} | {} | {:.2}% | {:.8} | {} |\n",
                i + 1,
                r.spacing,
                r.atr_multiplier
                    .map(|m| format!("{:.2}", m))
                    .unwrap_or_else(|| "—".to_string()),
                r.levels,
                r.tune_realized,
                r.tune_trades,
                r.realized_pnl_net,
                r.fees_paid,
                r.trades,
                r.max_drawdown * 100.0,
                r.ending_equity,
                if r.overfit { "overfit?" } else { "" },
            ));
        }
        md.push('\n');
        md.push_str("## Rollout runbook\n\n");
        md.push_str(
            "1. `runner -s <SYM> -k` the old runner, pull + build the new version.\n\
             2. Run scout for the current portfolio → pair selection + per-runner budgets (must sum ≤ the real wallet).\n\
             3. `sweep -s <SYM>` per pair (fees default to the standard tier — pass `--maker-fee 0` to match a zero-fee account) and adopt the top **validated** `atr_multiplier`/`levels`.\n\
             4. Spawn each runner `--fresh` from the printed top-config line with the scout budget as `capital` (held base seeds via `initial_base_balance` automatically).\n\
             5. Paper for a day if desired, then live. Watch the 7-day PnL% readout on the dashboard.\n\n",
        );
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
        // Walk-forward split covers the whole set.
        assert_eq!(result.tune_candles, 14);
        // Every row carries the volatility-relative multiplier, and the
        // recommended command emits it instead of an absolute spacing.
        for r in &result.rows {
            let m = r.atr_multiplier.expect("ATR known → multiplier reported");
            assert!((m - r.spacing / 2.0).abs() < 1e-9, "ATR is 2.0 here, got ATR× {}", m);
        }
        let line = result.spawn_line();
        assert!(line.contains("atr_multiplier="), "spawn line: {}", line);
        assert!(!line.contains(" spacing="), "no absolute spacing in: {}", line);
        assert!(line.contains("atr_period=14") && line.contains("atr_timeframe="));
        assert!(
            line.contains("min_profit_frac=") && line.contains("max_drawdown_pct="),
            "plan/09 knobs must be spelled out: {}",
            line
        );
    }

    #[test]
    #[ignore]
    fn replay_diagnosis_window() {
        // Manual harness: set TED_REPLAY_FILE to a candle file (or a T.E.D
        // trades_*.jsonl tick file, aggregated automatically) and run
        // `cargo test replay_diagnosis_window -- --ignored --nocapture`.
        let Ok(path) = std::env::var("TED_REPLAY_FILE") else {
            println!("TED_REPLAY_FILE not set — skipping");
            return;
        };
        let candles = crate::backtest::load_candles_from_file(&path).unwrap();

        // The Jul 7–29 live session's config: capital 362.70, levels 2, sweep-
        // pinned spacing 0.12266071, zero account fees. Replayed through the
        // current (per-lot) grid for the plan/09 acceptance checks.
        let capital = 362.70;
        let mut options: HashMap<String, String> = HashMap::new();
        options.insert("spacing".to_string(), "0.12266071".to_string());
        options.insert("levels".to_string(), "2".to_string());
        options.insert("capital".to_string(), format!("{}", capital));
        let bt = BacktestConfig {
            symbol: "SOLUSD".to_string(),
            algorithm: "grid".to_string(),
            options,
            timeframe: "30m".to_string(),
            candles_limit: candles.len(),
            from_file: None,
            start_quote_balance: capital,
            start_base_balance: 0.0,
            spread: 0.0,
            maker_fee: 0.0,
            taker_fee: 0.0,
        };
        let report = run_backtest(&bt, &candles).unwrap();
        println!("--- live config replay (per-lot grid) ---");
        println!("{}", report.render_console());
        let mut last_fill: Option<i64> = None;
        let mut max_gap_hours = 0.0f64;
        let mut negative_sells = 0usize;
        let mut max_marked_notional = 0.0f64;
        for t in &report.trades {
            if let Some(prev) = last_fill {
                max_gap_hours = max_gap_hours.max((t.timestamp - prev) as f64 / 3_600_000.0);
            }
            last_fill = Some(t.timestamp);
            if !t.is_buy && t.realized < -1e-9 {
                negative_sells += 1;
            }
            println!(
                "  {} {} @ {:.4} qty {:.4} realized {:+.4} position {:.4}",
                crate::backtest::fmt_ts(t.timestamp),
                if t.is_buy { "BUY " } else { "SELL" },
                t.price,
                t.qty,
                t.realized,
                t.position_after
            );
            max_marked_notional = max_marked_notional.max(t.position_after * t.price);
        }
        println!(
            "trades {} | max fill gap {:.1}h | negative sells {} | max marked notional {:.2} (cap {:.2})",
            report.trades.len(),
            max_gap_hours,
            negative_sells,
            max_marked_notional,
            capital * 0.75,
        );
        assert_eq!(negative_sells, 0, "plan/09: no sell fill may book negative realized");
        assert!(
            report.realized_pnl_net >= -1e-9,
            "plan/09: realized PnL must be >= 0 on the diagnosis window, got {}",
            report.realized_pnl_net
        );
        assert!(
            max_marked_notional <= capital * 0.75 + 1.0,
            "plan/09: inventory notional {} exceeded the cap {}",
            max_marked_notional,
            capital * 0.75
        );

        // Fee-bearing sanity (plan/09 validation #3): with standard fees the
        // exit floor widens, and still no sell may book negative realized.
        let mut fee_options: HashMap<String, String> = HashMap::new();
        fee_options.insert("levels".to_string(), "2".to_string());
        fee_options.insert("capital".to_string(), format!("{}", capital));
        let fee_bt = BacktestConfig {
            symbol: "SOLUSD".to_string(),
            algorithm: "grid".to_string(),
            options: fee_options,
            timeframe: "30m".to_string(),
            candles_limit: candles.len(),
            from_file: None,
            start_quote_balance: capital,
            start_base_balance: 0.0,
            spread: 0.0,
            maker_fee: 0.001,
            taker_fee: 0.002,
        };
        let fee_report = run_backtest(&fee_bt, &candles).unwrap();
        println!("--- fee-bearing sanity (maker 0.001 / taker 0.002, ATR spacing) ---");
        println!("{}", fee_report.render_console());
        for t in fee_report.trades.iter().filter(|t| !t.is_buy) {
            assert!(
                t.realized >= -1e-9,
                "fee-bearing replay booked a negative sell: {:+.6} @ {:.4}",
                t.realized,
                t.price
            );
        }

        let mut c = cfg(None, vec![1, 2, 3, 4]);
        c.symbol = "SOLUSD".to_string();
        c.timeframe = "30m".to_string();
        c.capital = capital;
        c.start_quote_balance = capital;
        let result = run_sweep(&c, &candles).unwrap();
        println!("{}", result.render_console());
    }

    #[test]
    fn report_renders_ranked_table_and_spawn_line() {
        let candles = oscillation();
        let result = run_sweep(&cfg(Some(vec![10.0]), vec![1]), &candles).unwrap();
        let md = result.render_markdown();
        assert!(md.contains("# T.E.D Sweep — TESTUSD"));
        assert!(md.contains("## Ranked results"));
        // 5 candles cannot seed a 14-period ATR → the spawn line falls back to
        // the absolute spacing, still carrying the plan/09 knobs.
        assert!(md.contains("runner -s TESTUSD -a grid -o spacing="));
        assert!(md.contains("min_profit_frac="));
        assert!(md.contains("## Rollout runbook"));
        assert!(result.render_console().lines().count() >= 3);
    }
}
