use crate::algorithm::Algorithm;
use crate::config::channels::RunnerMode;
use crate::config::config::Config;
use crate::runner::trade_log::TradeLog;
use crate::storage::db::{Db, FillRow};
use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet};
use std::time::Instant;

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
}

impl RunnerState {
    pub fn write_fill_to_db(&mut self, exchange_id: i64, is_buy: bool, price: f64, qty: f64) {
        if let (Some(db), Some(runner_id)) = (&self.db, self.runner_db_id) {
            let _ = db.insert_fill(&FillRow {
                runner_id,
                exchange_id: Some(exchange_id),
                direction: if is_buy { "buy" } else { "sell" }.to_string(),
                price,
                quantity: qty,
                realized_pnl: None,
                filled_at: Utc::now().to_rfc3339(),
            });
        }
    }
}
