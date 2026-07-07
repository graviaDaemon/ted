use crate::algorithm::Algorithm;
use crate::config::channels::{RunnerMode, TuiEvent};
use crate::config::config::Config;
use crate::runner::trade_log::TradeLog;
use crate::storage::db::{DailyRollup, Db, FillRow, RunnerStateRow, SnapshotRow};
use crate::util::extract_currencies;
use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet};
use std::time::Instant;

/// Running per-UTC-day aggregate used to upsert `daily_rollups`. Tracks the
/// lifetime cumulative values at the start of the current day so the rollup can
/// store that day's deltas.
pub struct DailyAgg {
    pub day: String,
    pub realized_start: f64,
    pub fees_start: f64,
    pub trades_start: u64,
}

pub struct RunnerState {
    pub symbol: String,
    pub algorithm: Box<dyn Algorithm>,
    pub algo_name: String,
    pub options: HashMap<String, String>,
    pub mode: RunnerMode,
    pub paused: bool,
    pub trade_log: TradeLog,
    pub started_at: DateTime<Utc>,
    pub config: Config,
    pub live_order_ids: HashSet<i64>,
    pub last_order_time: Option<Instant>,
    pub pending_buy_orders: HashMap<i64, (f64, f64)>,
    pub pending_sell_orders: HashMap<i64, (f64, f64)>,
    pub trade_store: Option<crate::storage::TradeStore>,
    pub wallet_balances: HashMap<String, f64>,
    pub runner_db_id: Option<i64>,
    pub db: Option<Db>,
    pub last_bid: f64,
    pub last_ask: f64,
    pub maker_fee: f64,
    pub taker_fee: f64,
    pub max_drawdown_pct: Option<f64>,
    pub peak_equity: f64,
    pub halted: bool,
    pub daily: Option<DailyAgg>,
}

impl RunnerState {
    pub fn write_fill_to_db(
        &mut self,
        exchange_id: Option<i64>,
        is_buy: bool,
        price: f64,
        qty: f64,
        realized_pnl: Option<f64>,
    ) {
        crate::logger::notify_tui(TuiEvent::Fill {
            symbol: self.symbol.clone(),
            is_buy,
            qty,
            price,
            realized_pnl,
            ts: Utc::now(),
        });
        if let (Some(db), Some(runner_id)) = (&self.db, self.runner_db_id) {
            let _ = db.insert_fill(&FillRow {
                runner_id,
                exchange_id,
                direction: if is_buy { "buy" } else { "sell" }.to_string(),
                price,
                quantity: qty,
                realized_pnl,
                filled_at: Utc::now().to_rfc3339(),
            });
        }
    }

    fn src(&self) -> String {
        format!("RUNNER:{}", self.symbol)
    }

    /// Persist the resume blob (options, serialized algorithm state, pending
    /// orders) so the runner can pick up where it left off after a restart.
    /// Called on every fill, on each snapshot tick, and on clean shutdown.
    pub fn save_state(&self) {
        let Some(db) = &self.db else { return };
        let options = serde_json::to_string(&self.options).unwrap_or_else(|_| "{}".to_string());
        let pending_buys =
            serde_json::to_string(&self.pending_buy_orders).unwrap_or_else(|_| "{}".to_string());
        let pending_sells =
            serde_json::to_string(&self.pending_sell_orders).unwrap_or_else(|_| "{}".to_string());
        let row = RunnerStateRow {
            symbol: self.symbol.clone(),
            algorithm: self.algo_name.clone(),
            mode: crate::runner::mode_label(&self.mode).to_string(),
            options,
            algo_state: self.algorithm.serialize_state(),
            pending_buys,
            pending_sells,
            updated_at: Utc::now().to_rfc3339(),
        };
        if let Err(e) = db.save_runner_state(&row) {
            crate::logger::log_warn(&self.src(), &format!("Failed to save runner state: {}", e));
        }
    }

    /// Remove the saved resume blob. Used on a deliberate `--kill` so the runner
    /// does not auto-resume on the next launch.
    pub fn clear_state(&self) {
        if let Some(db) = &self.db
            && let Ok(n) = db.clear_runner_state(&self.symbol)
            && n > 0
        {
            crate::logger::log_info(&self.src(), "Cleared saved resume state.");
        }
    }

    /// Write a high-frequency snapshot row and upsert the current UTC day's
    /// rollup. No-op until a price is known. Intraday restarts rebase the day's
    /// starting point, so a rollup can under-count a day across restarts;
    /// `fills` remains the exact source of truth for PnL. Also emits the
    /// periodic `TuiEvent::Status`, which must not be lost when no DB row
    /// exists.
    pub fn persist_periodic(&mut self) {
        let mid = if self.last_bid > 0.0 && self.last_ask > 0.0 {
            (self.last_bid + self.last_ask) / 2.0
        } else {
            return;
        };

        let position = self.algorithm.position();
        let realized = self.algorithm.realized_pnl();
        let fees = self.algorithm.fees_paid();
        let trades = self.algorithm.trade_count();
        let unrealized = self.algorithm.unrealized_pnl(mid);
        let (_, quote) = extract_currencies(&self.symbol);
        let quote_bal = self.wallet_balances.get(&quote).copied().unwrap_or(0.0);
        let equity = quote_bal + position * mid;

        crate::logger::notify_tui(TuiEvent::Status {
            symbol: self.symbol.clone(),
            mode: crate::runner::mode_label(&self.mode).to_string(),
            realized,
            unrealized,
            equity,
            position,
            open_buys: self.pending_buy_orders.len(),
            open_sells: self.pending_sell_orders.len(),
            paused: self.paused,
            halted: self.halted,
        });

        let Some(runner_id) = self.runner_db_id else {
            return;
        };
        if self.db.is_none() {
            return;
        }

        let now = Utc::now();
        let ts = now.to_rfc3339();
        let day = now.format("%Y-%m-%d").to_string();

        let needs_rebase = self.daily.as_ref().map(|d| d.day != day).unwrap_or(true);
        if needs_rebase {
            self.daily = Some(DailyAgg {
                day: day.clone(),
                realized_start: realized,
                fees_start: fees,
                trades_start: trades,
            });
        }
        let (realized_start, fees_start, trades_start) = self
            .daily
            .as_ref()
            .map(|d| (d.realized_start, d.fees_start, d.trades_start))
            .unwrap_or((0.0, 0.0, 0));

        let snapshot = SnapshotRow {
            runner_id,
            ts,
            mid,
            bid: self.last_bid,
            ask: self.last_ask,
            position,
            realized_pnl: realized,
            unrealized_pnl: unrealized,
            open_buys: self.pending_buy_orders.len() as i64,
            open_sells: self.pending_sell_orders.len() as i64,
            equity,
        };
        let rollup = DailyRollup {
            runner_id,
            day,
            realized_pnl: realized - realized_start,
            fees: fees - fees_start,
            trades: trades.saturating_sub(trades_start) as i64,
            ending_equity: equity,
            ending_position: position,
        };

        let db = self.db.as_ref().unwrap();
        if let Err(e) = db.insert_snapshot(&snapshot) {
            crate::logger::log_warn(&self.src(), &format!("Snapshot write failed: {}", e));
        }
        if let Err(e) = db.upsert_daily_rollup(&rollup) {
            crate::logger::log_warn(&self.src(), &format!("Daily rollup upsert failed: {}", e));
        }
    }
}
