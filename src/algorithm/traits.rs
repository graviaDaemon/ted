use crate::algorithm::GridBot;
use crate::algorithm::PassiveObserver;
use crate::api::{MarketData, TradeSignal};
use std::collections::HashMap;

pub trait Algorithm: Send {
    fn name(&self) -> &str;
    fn on_tick(&mut self, tick: &MarketData) -> Vec<TradeSignal>;
    fn on_fill(&mut self, _price: f64, _is_buy: bool, _current_price: f64) -> Vec<TradeSignal> {
        vec![]
    }
    fn on_order_failed(&mut self, _price: f64, _is_buy: bool) {}
    fn on_reconnect(&mut self) {}
    fn on_spacing_update(&mut self, _new_spacing: f64) {}
    /// Push a fresh candle-close trend EMA into the strategy. Fed by the
    /// runner's periodic candle refresh live and by the backtester per candle,
    /// so both paths exercise the same trend filter.
    fn on_trend_update(&mut self, _ema: f64) {}
    fn on_balance_update(&mut self, _base: f64, _quote: f64) {}
    fn summary(&self) -> Option<String> {
        None
    }
    /// Net base inventory held by the strategy (positive long, negative short).
    /// Lets the runner compute equity for risk control generically.
    fn position(&self) -> f64 {
        0.0
    }
    /// Mark-to-market unrealized PnL of the open position at the given mid price.
    fn unrealized_pnl(&self, _mid: f64) -> f64 {
        0.0
    }
    /// Lifetime realized PnL booked by the strategy (net of fees where the
    /// strategy accounts for them). Lets the runner record per-fill realized PnL
    /// and daily rollups generically.
    fn realized_pnl(&self) -> f64 {
        0.0
    }
    /// Lifetime fees paid by the strategy.
    fn fees_paid(&self) -> f64 {
        0.0
    }
    /// Total number of fills the strategy has booked (buys + sells).
    fn trade_count(&self) -> u64 {
        0
    }
    /// Serialize the strategy's internal dynamic state to JSON for resume.
    /// Returns `None` when the strategy cannot meaningfully persist state
    /// (e.g. Rhai scripts, whose state lives in the engine scope).
    fn serialize_state(&self) -> Option<String> {
        None
    }
    /// Best-effort restore of state produced by `serialize_state`. Implementations
    /// must tolerate malformed or stale JSON by leaving themselves unchanged.
    fn restore_state(&mut self, _json: &str) {}
}

type AlgoBuilder = fn(&HashMap<String, String>) -> Result<Box<dyn Algorithm>, String>;

struct AlgoEntry {
    name: &'static str,
    build: AlgoBuilder,
}

static BUILTIN_REGISTRY: &[AlgoEntry] = &[
    AlgoEntry {
        name: "passive",
        build: |_| Ok(Box::new(PassiveObserver::new())),
    },
    AlgoEntry {
        name: "grid",
        build: |o| GridBot::new(o).map(|g| Box::new(g) as _),
    },
];

pub fn build_algorithm(
    name: &str,
    options: &HashMap<String, String>,
) -> Result<Box<dyn Algorithm>, String> {
    let key = if name.is_empty() { "passive" } else { name };

    if let Some(entry) = BUILTIN_REGISTRY
        .iter()
        .find(|e| e.name.eq_ignore_ascii_case(key))
    {
        return (entry.build)(options);
    }

    crate::algorithm::script::build_script_algorithm(key, options)
        .unwrap_or_else(|| Err(format!("Unknown algorithm: '{}'", name)))
}
