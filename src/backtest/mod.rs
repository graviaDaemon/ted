//! Offline backtesting / historical-replay harness.
//!
//! Replays historical OHLC candles through the *same* `Algorithm` trait
//! implementations used live, with a deterministic, fee-aware, conservative fill
//! model, and produces a PnL/stats report. See `plan/03-backtester.md`.
//!
//! Fill model (the key modeling choice): a resting limit **buy** at `p` fills
//! during a candle iff `candle.low <= p` (price traded down through it); a resting
//! limit **sell** at `p` fills iff `candle.high >= p`. This is the standard
//! conservative OHLC assumption. Counter-orders created by `on_fill` (opposite
//! side to the fill that spawned them) are frozen for the remainder of the candle
//! so a buy and its counter-sell can never round-trip for free inside one candle;
//! same-side grid *extensions* are free to chain as price wicks through them.

use crate::algorithm::build_algorithm;
use crate::algorithm::position::AvgCostBook;
use crate::api::candles::Candle;
use crate::api::{MarketData, TradeSignal};
use chrono::{TimeZone, Utc};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

const MAX_FILLS_PER_CANDLE: usize = 10_000;

pub struct BacktestConfig {
    pub symbol: String,
    pub algorithm: String,
    pub options: HashMap<String, String>,
    pub timeframe: String,
    /// Provenance of the candle set (live-fetch limit / source file). Recorded on
    /// the config for reproducibility; the replay itself operates on the loaded
    /// candle slice, so these are not read by `run_backtest`.
    #[allow(dead_code)]
    pub candles_limit: usize,
    #[allow(dead_code)]
    pub from_file: Option<String>,
    pub start_quote_balance: f64,
    pub start_base_balance: f64,
    pub spread: f64,
    pub maker_fee: f64,
    pub taker_fee: f64,
}

pub struct TradeRow {
    pub timestamp: i64,
    pub is_buy: bool,
    pub price: f64,
    pub qty: f64,
    pub fee: f64,
    pub realized: f64,
    pub position_after: f64,
}

pub struct BacktestReport {
    pub symbol: String,
    pub algorithm: String,
    pub timeframe: String,
    pub candles: usize,
    pub start_ts: i64,
    pub end_ts: i64,
    pub spread: f64,
    pub maker_fee: f64,
    pub taker_fee: f64,
    pub start_quote_balance: f64,
    pub start_base_balance: f64,
    pub trades: Vec<TradeRow>,
    pub realized_pnl_net: f64,
    pub fees_paid: f64,
    pub max_drawdown: f64,
    pub win_rate: f64,
    pub starting_equity: f64,
    pub ending_equity: f64,
    pub ending_position: f64,
    pub last_price: f64,
    pub return_pct: f64,
    pub final_summary: Option<String>,
}

/// Virtual order book + wallet. Limit orders lock funds on placement (matching
/// Bitfinex, which reserves balance for resting orders), so the backtest cannot
/// over-commit balance it does not hold.
struct Sim {
    quote_balance: f64,
    base_balance: f64,
    reserved_quote: f64,
    reserved_base: f64,
    open_buys: Vec<(f64, f64)>,
    open_sells: Vec<(f64, f64)>,
    book: AvgCostBook,
    maker_fee: f64,
    trades: Vec<TradeRow>,
}

fn price_key(price: f64) -> i64 {
    (price * 1e8).round() as i64
}

impl Sim {
    fn new(quote: f64, base: f64, maker_fee: f64) -> Self {
        Sim {
            quote_balance: quote,
            base_balance: base,
            reserved_quote: 0.0,
            reserved_base: 0.0,
            open_buys: Vec::new(),
            open_sells: Vec::new(),
            book: AvgCostBook::new(maker_fee),
            maker_fee,
            trades: Vec::new(),
        }
    }

    fn free_quote(&self) -> f64 {
        self.quote_balance - self.reserved_quote
    }

    fn free_base(&self) -> f64 {
        self.base_balance - self.reserved_base
    }

    /// Route one signal into the book, honoring the same guards the live
    /// dispatcher applies that are meaningful offline: insufficient balance,
    /// duplicate price, spread-cross. Returns true if a new order was placed.
    fn route_signal(&mut self, sig: &TradeSignal, bid: f64, ask: f64) -> bool {
        match sig {
            TradeSignal::Buy { price, quantity, .. } => {
                let (price, quantity) = (*price, *quantity);
                if self.open_buys.iter().any(|(p, _)| (p - price).abs() < 1e-9) {
                    return false; // duplicate
                }
                if price >= ask {
                    return false; // would cross spread
                }
                if self.free_quote() < price * quantity {
                    return false; // insufficient virtual balance
                }
                self.open_buys.push((price, quantity));
                self.reserved_quote += price * quantity;
                true
            }
            TradeSignal::Sell { price, quantity, .. } => {
                let (price, quantity) = (*price, *quantity);
                if self.open_sells.iter().any(|(p, _)| (p - price).abs() < 1e-9) {
                    return false; // duplicate
                }
                if price <= bid {
                    return false; // would cross spread
                }
                if self.free_base() < quantity {
                    return false; // insufficient virtual balance
                }
                self.open_sells.push((price, quantity));
                self.reserved_base += quantity;
                true
            }
            TradeSignal::Cancel { price, is_buy, .. } => {
                let price = *price;
                if *is_buy {
                    if let Some(idx) = self.open_buys.iter().position(|(p, _)| (p - price).abs() < 1e-6) {
                        let (p, q) = self.open_buys.remove(idx);
                        self.reserved_quote -= p * q;
                    }
                } else if let Some(idx) = self.open_sells.iter().position(|(p, _)| (p - price).abs() < 1e-6) {
                    let (_p, q) = self.open_sells.remove(idx);
                    self.reserved_base -= q;
                }
                false
            }
        }
    }

    /// Apply a buy fill at `price` for `qty`: release the reservation, spend
    /// quote (incl. fee), receive base, update average-cost accounting.
    fn fill_buy(&mut self, price: f64, qty: f64, ts: i64) {
        if let Some(idx) = self.open_buys.iter().position(|(p, _)| (p - price).abs() < 1e-9) {
            self.open_buys.remove(idx);
        }
        self.reserved_quote -= price * qty;
        let fee = price * qty * self.maker_fee;
        self.quote_balance -= price * qty + fee;
        self.base_balance += qty;
        self.book.record_buy(price, qty);
        self.trades.push(TradeRow {
            timestamp: ts,
            is_buy: true,
            price,
            qty,
            fee,
            realized: 0.0,
            position_after: self.book.position,
        });
    }

    /// Apply a sell fill at `price` for `qty`: release the reservation, receive
    /// quote (net of fee), give up base, realize PnL.
    fn fill_sell(&mut self, price: f64, qty: f64, ts: i64) {
        if let Some(idx) = self.open_sells.iter().position(|(p, _)| (p - price).abs() < 1e-9) {
            self.open_sells.remove(idx);
        }
        self.reserved_base -= qty;
        let fee = price * qty * self.maker_fee;
        self.quote_balance += price * qty - fee;
        self.base_balance -= qty;
        let realized = self.book.record_sell(price, qty);
        self.trades.push(TradeRow {
            timestamp: ts,
            is_buy: false,
            price,
            qty,
            fee,
            realized,
            position_after: self.book.position,
        });
    }
}

/// One fillable resting order selected for this iteration.
struct Fill {
    is_buy: bool,
    price: f64,
    qty: f64,
}

/// Pick the next order to fill within a candle. A buy at `p` is fillable iff
/// `low <= p`; a sell at `p` iff `high >= p`. `frozen` price keys (fresh
/// counter-orders) are excluded. When both sides are fillable, fill the one whose
/// price sits nearer the candle open — the side the path is assumed to reach first.
fn next_fill(
    sim: &Sim,
    high: f64,
    low: f64,
    open: f64,
    frozen: &std::collections::HashSet<i64>,
) -> Option<Fill> {
    let best_buy = sim
        .open_buys
        .iter()
        .filter(|(p, _)| low <= *p && !frozen.contains(&price_key(*p)))
        .copied()
        .reduce(|a, b| if a.0 >= b.0 { a } else { b }); // highest buy fills first on the way down

    let best_sell = sim
        .open_sells
        .iter()
        .filter(|(p, _)| high >= *p && !frozen.contains(&price_key(*p)))
        .copied()
        .reduce(|a, b| if a.0 <= b.0 { a } else { b }); // lowest sell fills first on the way up

    match (best_buy, best_sell) {
        (Some((bp, bq)), Some((sp, sq))) => {
            if (open - bp).abs() <= (sp - open).abs() {
                Some(Fill { is_buy: true, price: bp, qty: bq })
            } else {
                Some(Fill { is_buy: false, price: sp, qty: sq })
            }
        }
        (Some((bp, bq)), None) => Some(Fill { is_buy: true, price: bp, qty: bq }),
        (None, Some((sp, sq))) => Some(Fill { is_buy: false, price: sp, qty: sq }),
        (None, None) => None,
    }
}

/// Deterministically replay `candles_in` through the configured algorithm and
/// return a report. Pure (no I/O beyond logging) — identical inputs yield an
/// identical report.
pub fn run_backtest(cfg: &BacktestConfig, candles_in: &[Candle]) -> Result<BacktestReport, String> {
    if candles_in.is_empty() {
        return Err("No candles to replay".to_string());
    }

    let mut candles: Vec<&Candle> = candles_in.iter().collect();
    candles.sort_by_key(|c| c.timestamp);

    // Resolve spacing from the leading candles via ATR when the strategy needs it
    // and the user did not pin it, so the grid replays with a sensible grid width
    // computed the same way the live runner computes it.
    let mut options = cfg.options.clone();
    maybe_inject_atr_spacing(&cfg.algorithm, &mut options, &candles)?;

    let mut algo = build_algorithm(&cfg.algorithm, &options)?;

    let mut sim = Sim::new(cfg.start_quote_balance, cfg.start_base_balance, cfg.maker_fee);
    let mut equity_curve: Vec<f64> = Vec::with_capacity(candles.len());

    let first_mid = candles.first().unwrap().close;
    let starting_equity = cfg.start_quote_balance + cfg.start_base_balance * first_mid;

    for candle in &candles {
        let mid = candle.close;
        let bid = mid * (1.0 - cfg.spread / 2.0);
        let ask = mid * (1.0 + cfg.spread / 2.0);

        let md = MarketData {
            symbol: cfg.symbol.clone(),
            bid,
            bid_size: 0.0,
            ask,
            ask_size: 0.0,
            last_price: mid,
            volume: candle.volume,
            high: candle.high,
            low: candle.low,
            daily_change: 0.0,
            daily_change_pct: 0.0,
            timestamp: Utc.timestamp_millis_opt(candle.timestamp).single().unwrap_or_else(Utc::now),
        };

        for sig in algo.on_tick(&md) {
            sim.route_signal(&sig, bid, ask);
        }

        let mut frozen: std::collections::HashSet<i64> = std::collections::HashSet::new();
        let mut iters = 0usize;
        while let Some(fill) = next_fill(&sim, candle.high, candle.low, candle.open, &frozen) {
            iters += 1;
            if iters > MAX_FILLS_PER_CANDLE {
                crate::logger::log_warn(
                    "[BACKTEST]",
                    "Fill cap hit within a single candle — aborting candle (possible runaway algorithm).",
                );
                break;
            }

            if fill.is_buy {
                sim.fill_buy(fill.price, fill.qty, candle.timestamp);
            } else {
                sim.fill_sell(fill.price, fill.qty, candle.timestamp);
            }

            for sig in algo.on_fill(fill.price, fill.is_buy, mid) {
                // A signal opposite to the fill that produced it is a counter
                // order; freeze it so it cannot round-trip inside this candle.
                let is_counter = matches!(
                    (&sig, fill.is_buy),
                    (TradeSignal::Sell { .. }, true) | (TradeSignal::Buy { .. }, false)
                );
                let placed = sim.route_signal(&sig, bid, ask);
                if placed
                    && is_counter
                    && let TradeSignal::Buy { price, .. } | TradeSignal::Sell { price, .. } = sig
                {
                    frozen.insert(price_key(price));
                }
            }
        }

        equity_curve.push(sim.quote_balance + sim.base_balance * mid);
    }

    let last_mid = candles.last().unwrap().close;
    let ending_equity = sim.quote_balance + sim.base_balance * last_mid;
    let max_drawdown = max_drawdown(&equity_curve);

    let sells = sim.trades.iter().filter(|t| !t.is_buy).count();
    let wins = sim.trades.iter().filter(|t| !t.is_buy && t.realized > 0.0).count();
    let win_rate = if sells > 0 { wins as f64 / sells as f64 } else { 0.0 };

    let return_pct = if starting_equity.abs() > f64::EPSILON {
        (ending_equity - starting_equity) / starting_equity * 100.0
    } else {
        0.0
    };

    Ok(BacktestReport {
        symbol: cfg.symbol.clone(),
        algorithm: cfg.algorithm.clone(),
        timeframe: cfg.timeframe.clone(),
        candles: candles.len(),
        start_ts: candles.first().unwrap().timestamp,
        end_ts: candles.last().unwrap().timestamp,
        spread: cfg.spread,
        maker_fee: cfg.maker_fee,
        taker_fee: cfg.taker_fee,
        start_quote_balance: cfg.start_quote_balance,
        start_base_balance: cfg.start_base_balance,
        realized_pnl_net: sim.book.realized_pnl,
        fees_paid: sim.book.fees_paid,
        max_drawdown,
        win_rate,
        starting_equity,
        ending_equity,
        ending_position: sim.book.position,
        last_price: last_mid,
        return_pct,
        final_summary: algo.summary(),
        trades: sim.trades,
    })
}

/// Largest peak-to-trough decline of the equity curve, as a fraction of the peak.
fn max_drawdown(equity: &[f64]) -> f64 {
    let mut peak = f64::MIN;
    let mut max_dd = 0.0;
    for &e in equity {
        if e > peak {
            peak = e;
        }
        if peak > 0.0 {
            let dd = (peak - e) / peak;
            if dd > max_dd {
                max_dd = dd;
            }
        }
    }
    max_dd
}

/// If the grid needs a spacing and the user didn't pin one, derive it from ATR
/// over the leading candles (mirroring the live runner's ATR-spacing logic).
fn maybe_inject_atr_spacing(
    algorithm: &str,
    options: &mut HashMap<String, String>,
    candles: &[&Candle],
) -> Result<(), String> {
    if algorithm != "grid" || options.contains_key("spacing") {
        return Ok(());
    }
    let period = options
        .get("atr_period")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(14);
    let multiplier = options
        .get("atr_multiplier")
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(0.5);

    // compute_atr expects newest-first (it reverses internally); candles here are
    // oldest-first, so feed it the reversed leading window.
    let lead: Vec<Candle> = candles
        .iter()
        .take(period + 1)
        .rev()
        .map(|c| Candle {
            timestamp: c.timestamp,
            open: c.open,
            close: c.close,
            high: c.high,
            low: c.low,
            volume: c.volume,
        })
        .collect();

    let atr = crate::algorithm::atr::compute_atr(&lead, period)?;
    let spacing = atr * multiplier;
    if spacing <= 0.0 {
        return Err(format!(
            "ATR-derived spacing is {:.8} (<= 0) — pass an explicit spacing= option",
            spacing
        ));
    }
    crate::logger::log(
        "[BACKTEST]",
        &format!(
            "ATR({}) over leading candles = {:.8}, ×{:.2} → spacing {:.8}",
            period, atr, multiplier, spacing
        ),
    );
    options.insert("spacing".to_string(), format!("{:.8}", spacing));
    Ok(())
}

fn fmt_ts(ms: i64) -> String {
    Utc.timestamp_millis_opt(ms)
        .single()
        .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| ms.to_string())
}

impl BacktestReport {
    /// Full markdown report written to disk.
    pub fn render_markdown(&self) -> String {
        let mut md = String::new();
        md.push_str(&format!("# T.E.D Backtest — {}\n\n", self.symbol));
        md.push_str("| Field | Value |\n|---|---|\n");
        md.push_str(&format!(
            "| Generated | {} UTC |\n",
            Utc::now().format("%Y-%m-%d %H:%M:%S")
        ));
        md.push_str(&format!("| Algorithm | {} |\n", self.algorithm));
        md.push_str(&format!("| Timeframe | {} |\n", self.timeframe));
        md.push_str(&format!("| Candles | {} |\n", self.candles));
        md.push_str(&format!(
            "| Period | {} – {} UTC |\n",
            fmt_ts(self.start_ts),
            fmt_ts(self.end_ts)
        ));
        md.push_str(&format!(
            "| Fees | maker {:.6}, taker {:.6} |\n",
            self.maker_fee, self.taker_fee
        ));
        md.push_str(&format!("| Spread model | {:.6} |\n", self.spread));
        md.push_str(&format!(
            "| Start balance | {:.8} quote, {:.8} base |\n",
            self.start_quote_balance, self.start_base_balance
        ));
        md.push_str(&format!("| Trades | {} |\n", self.trades.len()));
        md.push_str(&format!(
            "| Realized PnL (net) | {:+.8} |\n",
            self.realized_pnl_net
        ));
        md.push_str(&format!("| Fees paid | {:.8} |\n", self.fees_paid));
        md.push_str(&format!("| Win rate | {:.2}% |\n", self.win_rate * 100.0));
        md.push_str(&format!(
            "| Max drawdown | {:.2}% |\n",
            self.max_drawdown * 100.0
        ));
        md.push_str(&format!(
            "| Ending position | {:.8} base @ last {:.2} |\n",
            self.ending_position, self.last_price
        ));
        md.push_str(&format!(
            "| Equity | {:.8} → {:.8} |\n",
            self.starting_equity, self.ending_equity
        ));
        md.push_str(&format!("| Return | {:+.4}% |\n\n", self.return_pct));

        if let Some(summary) = &self.final_summary {
            md.push_str("## Algorithm Summary\n\n```\n");
            md.push_str(summary);
            md.push_str("\n```\n\n");
        }

        md.push_str("## Trades\n\n");
        md.push_str("| # | Time | Side | Price | Qty | Fee | Realized | Position |\n");
        md.push_str("|---|---|---|---|---|---|---|---|\n");
        for (i, t) in self.trades.iter().enumerate() {
            md.push_str(&format!(
                "| {} | {} | {} | {:.2} | {:.8} | {:.8} | {:+.8} | {:.8} |\n",
                i + 1,
                fmt_ts(t.timestamp),
                if t.is_buy { "BUY" } else { "SELL" },
                t.price,
                t.qty,
                t.fee,
                t.realized,
                t.position_after,
            ));
        }
        if self.trades.is_empty() {
            md.push_str("| — | No trades executed | | | | | | |\n");
        }
        md.push('\n');
        md
    }

    /// Compact multi-line summary logged to the TUI.
    pub fn render_console(&self) -> String {
        format!(
            "Backtest {} [{}] {} candles ({} – {})\n  \
             Trades: {} | Realized PnL (net): {:+.8} | Fees: {:.8}\n  \
             Win rate: {:.1}% | Max drawdown: {:.2}% | Return: {:+.4}%\n  \
             Equity: {:.4} → {:.4} | Ending position: {:.8} @ {:.2}",
            self.symbol,
            self.algorithm,
            self.candles,
            fmt_ts(self.start_ts),
            fmt_ts(self.end_ts),
            self.trades.len(),
            self.realized_pnl_net,
            self.fees_paid,
            self.win_rate * 100.0,
            self.max_drawdown * 100.0,
            self.return_pct,
            self.starting_equity,
            self.ending_equity,
            self.ending_position,
            self.last_price,
        )
    }
}

/// Write the report markdown next to the overview files, returning the path.
pub fn write_report(report: &BacktestReport) -> std::io::Result<PathBuf> {
    let base = format!(
        "backtest_{}_{}",
        report.symbol.replace(':', "_"),
        Utc::now().format("%Y-%m-%d")
    );
    let mut path = PathBuf::from(format!("{}.md", base));
    let mut n = 2u32;
    while path.exists() {
        path = PathBuf::from(format!("{}_{}.md", base, n));
        n += 1;
    }
    fs::write(&path, report.render_markdown())?;
    Ok(path)
}

/// Load candles from a local CSV or JSONL/JSON file for reproducible, offline
/// backtests. CSV: header `timestamp,open,close,high,low,volume`. JSONL: one
/// Bitfinex-style array `[mts,open,close,high,low,volume]` per line, or one object
/// with those field names per line. Returns candles sorted oldest-first.
pub fn load_candles_from_file(path: &str) -> Result<Vec<Candle>, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("Could not read {}: {}", path, e))?;
    let lower = path.to_lowercase();
    let mut candles = if lower.ends_with(".csv") {
        parse_csv(&text)?
    } else {
        parse_jsonl(&text)?
    };
    if candles.is_empty() {
        return Err(format!("No candles parsed from {}", path));
    }
    candles.sort_by_key(|c| c.timestamp);
    Ok(candles)
}

fn parse_csv(text: &str) -> Result<Vec<Candle>, String> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let cols: Vec<&str> = line.split(',').map(|c| c.trim()).collect();
        // Skip a header row if the first column isn't numeric.
        if i == 0 && cols[0].parse::<i64>().is_err() {
            continue;
        }
        if cols.len() < 6 {
            return Err(format!("CSV line {} has fewer than 6 columns: {}", i + 1, line));
        }
        out.push(Candle {
            timestamp: cols[0].parse().map_err(|_| format!("Bad timestamp on line {}", i + 1))?,
            open: cols[1].parse().map_err(|_| format!("Bad open on line {}", i + 1))?,
            close: cols[2].parse().map_err(|_| format!("Bad close on line {}", i + 1))?,
            high: cols[3].parse().map_err(|_| format!("Bad high on line {}", i + 1))?,
            low: cols[4].parse().map_err(|_| format!("Bad low on line {}", i + 1))?,
            volume: cols[5].parse().map_err(|_| format!("Bad volume on line {}", i + 1))?,
        });
    }
    Ok(out)
}

fn parse_jsonl(text: &str) -> Result<Vec<Candle>, String> {
    // Allow either a single JSON array-of-arrays document or newline-delimited rows.
    let trimmed = text.trim_start();
    if trimmed.starts_with('[')
        && let Ok(serde_json::Value::Array(rows)) = serde_json::from_str::<serde_json::Value>(text)
        && rows.first().map(|r| r.is_array()).unwrap_or(false)
    {
        return rows.iter().map(candle_from_value).collect();
    }

    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let val: serde_json::Value =
            serde_json::from_str(line).map_err(|e| format!("Bad JSON on line {}: {}", i + 1, e))?;
        out.push(candle_from_value(&val)?);
    }
    Ok(out)
}

fn candle_from_value(val: &serde_json::Value) -> Result<Candle, String> {
    if let Some(arr) = val.as_array() {
        if arr.len() < 6 {
            return Err(format!("Candle array has fewer than 6 fields: {}", val));
        }
        return Ok(Candle {
            timestamp: arr[0].as_i64().unwrap_or(0),
            open: arr[1].as_f64().unwrap_or(0.0),
            close: arr[2].as_f64().unwrap_or(0.0),
            high: arr[3].as_f64().unwrap_or(0.0),
            low: arr[4].as_f64().unwrap_or(0.0),
            volume: arr[5].as_f64().unwrap_or(0.0),
        });
    }
    let obj = val
        .as_object()
        .ok_or_else(|| format!("Expected candle array or object, got: {}", val))?;
    let f = |k: &str| obj.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
    Ok(Candle {
        timestamp: obj
            .get("timestamp")
            .and_then(|v| v.as_i64())
            .unwrap_or_else(|| f("timestamp") as i64),
        open: f("open"),
        close: f("close"),
        high: f("high"),
        low: f("low"),
        volume: f("volume"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candle(ts: i64, open: f64, close: f64, high: f64, low: f64) -> Candle {
        Candle { timestamp: ts, open, close, high, low, volume: 1.0 }
    }

    fn opt(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    /// A 1-level grid (spacing 10) seeded at 100 that dips to 90 then recovers to
    /// 100 closes exactly one round-trip: buy @90, sell @100, PnL = spacing × qty.
    fn oscillation() -> Vec<Candle> {
        vec![
            candle(1, 100.0, 100.0, 100.0, 100.0), // build grid: buy 90, sell 110
            candle(2, 100.0, 90.0, 100.0, 90.0),   // dip: buy 90 fills
            candle(3, 90.0, 100.0, 100.0, 90.0),   // recover: sell 100 (counter) fills
        ]
    }

    fn cfg(options: HashMap<String, String>, maker_fee: f64) -> BacktestConfig {
        BacktestConfig {
            symbol: "BTCUSD".to_string(),
            algorithm: "grid".to_string(),
            options,
            timeframe: "1h".to_string(),
            candles_limit: 0,
            from_file: None,
            start_quote_balance: 1000.0,
            start_base_balance: 10.0,
            spread: 0.0,
            maker_fee,
            taker_fee: 0.0,
        }
    }

    #[test]
    fn sanity_zero_fee_round_trip_equals_spacing() {
        let options = opt(&[("levels", "1"), ("qty", "1"), ("spacing", "10"), ("trend_filter", "off")]);
        let report = run_backtest(&cfg(options, 0.0), &oscillation()).unwrap();
        assert_eq!(report.trades.len(), 2, "expected one buy + one sell");
        assert!(
            (report.realized_pnl_net - 10.0).abs() < 1e-9,
            "expected PnL 10, got {}",
            report.realized_pnl_net
        );
        assert!((report.win_rate - 1.0).abs() < 1e-9);
    }

    #[test]
    fn fee_reduces_pnl_by_expected_amount() {
        let options = opt(&[("levels", "1"), ("qty", "1"), ("spacing", "10"), ("trend_filter", "off")]);
        let report = run_backtest(&cfg(options, 0.001), &oscillation()).unwrap();
        // 10 minus the maker fee on the 90 buy and the 100 sell.
        let expected = 10.0 - 90.0 * 0.001 - 100.0 * 0.001;
        assert!(
            (report.realized_pnl_net - expected).abs() < 1e-9,
            "expected {}, got {}",
            expected,
            report.realized_pnl_net
        );
    }

    #[test]
    fn deterministic_same_inputs_same_output() {
        let options = opt(&[("levels", "1"), ("qty", "1"), ("spacing", "10"), ("trend_filter", "off")]);
        let a = run_backtest(&cfg(options.clone(), 0.0), &oscillation()).unwrap();
        let b = run_backtest(&cfg(options, 0.0), &oscillation()).unwrap();
        assert_eq!(a.trades.len(), b.trades.len());
        assert_eq!(a.realized_pnl_net.to_bits(), b.realized_pnl_net.to_bits());
        assert_eq!(a.ending_equity.to_bits(), b.ending_equity.to_bits());
        assert_eq!(a.fees_paid.to_bits(), b.fees_paid.to_bits());
    }

    #[test]
    fn flat_then_up_no_fake_profit() {
        // A monotonically rising market with no starting inventory: the grid may
        // follow price up and accumulate inventory (buys), but with zero fee and
        // no completed round-trips it must never realize a *profit* — average-cost
        // accounting only books PnL when inventory is sold above its cost.
        let candles = vec![
            candle(1, 100.0, 100.0, 100.0, 100.0),
            candle(2, 100.0, 105.0, 106.0, 100.0),
            candle(3, 105.0, 120.0, 121.0, 105.0),
        ];
        let options = opt(&[("levels", "1"), ("qty", "1"), ("spacing", "10"), ("trend_filter", "off")]);
        let mut c = cfg(options, 0.0);
        c.start_base_balance = 0.0; // no inventory to sell
        let report = run_backtest(&c, &candles).unwrap();
        assert!(
            report.realized_pnl_net <= 1e-12,
            "no fake profit allowed, got {}",
            report.realized_pnl_net
        );
        let winning_sells = report.trades.iter().filter(|t| !t.is_buy && t.realized > 0.0).count();
        assert_eq!(winning_sells, 0, "no winning sells without a real round-trip");
    }

    #[test]
    fn report_renders_markdown_with_key_sections() {
        let options = opt(&[("levels", "1"), ("qty", "1"), ("spacing", "10"), ("trend_filter", "off")]);
        let report = run_backtest(&cfg(options, 0.0), &oscillation()).unwrap();
        let md = report.render_markdown();
        assert!(md.contains("# T.E.D Backtest — BTCUSD"));
        assert!(md.contains("Realized PnL (net)"));
        assert!(md.contains("Max drawdown"));
        assert!(md.contains("Win rate"));
        assert!(md.contains("## Trades"));
        assert!(md.contains("Algorithm Summary"));
        // Console summary is non-empty and multi-line.
        assert!(report.render_console().lines().count() >= 3);
    }

    #[test]
    fn capital_grid_round_trips_non_negative_and_no_short() {
        // Balance-aware sizing (plan/06 step 1) + spot-only no-short (step 2) in a
        // full backtester replay: a buy-first grid (no starting base) sized from a
        // capital budget should buy the dip, counter-sell the recovery, end
        // non-negative, and never hold a net-short position at any point.
        let options = opt(&[
            ("levels", "1"),
            ("capital", "50"), // buy budget 25 funds exactly one $25 level
            ("spacing", "10"),
            ("trend_filter", "off"),
        ]);
        let mut c = cfg(options, 0.0);
        c.start_base_balance = 0.0; // buy-first
        let report = run_backtest(&c, &oscillation()).unwrap();

        assert!(report.trades.len() >= 2, "expected a buy + counter-sell round-trip, got {}", report.trades.len());
        assert!(
            report.realized_pnl_net >= -1e-9,
            "realized PnL must be non-negative, got {}",
            report.realized_pnl_net
        );
        assert!(
            report.ending_position >= -1e-9,
            "spot grid must never end net short, got {}",
            report.ending_position
        );
        for t in &report.trades {
            assert!(
                t.position_after >= -1e-9,
                "no-short invariant violated mid-replay: position {} after trade",
                t.position_after
            );
        }
    }

    #[test]
    fn capital_below_min_notional_emits_no_trades() {
        // Buy budget 10 (= 20 × 0.5) is below the $25 min_notional → unfundable, so
        // the grid builds nothing and the replay produces no trades.
        let options = opt(&[
            ("levels", "3"),
            ("capital", "20"),
            ("spacing", "10"),
            ("trend_filter", "off"),
        ]);
        let report = run_backtest(&cfg(options, 0.0), &oscillation()).unwrap();
        assert_eq!(report.trades.len(), 0, "unfundable grid must not trade");
    }

    #[test]
    fn loads_csv_and_jsonl() {
        let csv = "timestamp,open,close,high,low,volume\n1,100,101,102,99,5\n2,101,102,103,100,6\n";
        let from_csv = parse_csv(csv).unwrap();
        assert_eq!(from_csv.len(), 2);
        assert_eq!(from_csv[0].timestamp, 1);
        assert!((from_csv[1].close - 102.0).abs() < 1e-9);

        let jsonl = "[1,100,101,102,99,5]\n[2,101,102,103,100,6]\n";
        let from_jsonl = parse_jsonl(jsonl).unwrap();
        assert_eq!(from_jsonl.len(), 2);
        assert!((from_jsonl[1].high - 103.0).abs() < 1e-9);
    }
}
