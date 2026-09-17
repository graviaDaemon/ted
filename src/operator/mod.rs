//! Headless operator support (plan/12): control-surface listener, the
//! un-bypassable guardrail governor, the monthly circuit breaker, and status
//! snapshots. The run loop that ties these together lives in `main::run_headless`
//! so it can reuse the existing command dispatch. The strategic decisions
//! (what to trade, when to re-tune) live in Lara, not here — T.E.D only executes
//! and guards.

pub mod breaker;
pub mod control;
pub mod guardrails;

use serde::Serialize;
use std::collections::HashMap;

/// Latest per-runner status, cached from the `TuiEvent::Status` stream and served
/// over the control surface. Mirrors the fields the TUI dashboard already emits.
#[derive(Debug, Clone, Serialize)]
pub struct StatusSnapshot {
    pub symbol: String,
    pub mode: String,
    pub realized: f64,
    pub unrealized: f64,
    pub equity: f64,
    pub position: f64,
    pub open_buys: usize,
    pub open_sells: usize,
    pub paused: bool,
    pub halted: bool,
    pub fees_paid: f64,
    pub open_lots: usize,
    pub trend: Option<String>,
    pub pnl_7d_pct: Option<f64>,
}

pub type StatusCache = HashMap<String, StatusSnapshot>;

/// Build the JSON reply for a `status` command: per-runner snapshots plus
/// account-level month-to-date PnL, the breaker hold flag, and last-change ts.
pub fn status_json(
    cache: &StatusCache,
    hold: bool,
    hold_reason: Option<&str>,
    last_config_change: Option<&str>,
    month_net_realized: f64,
    month_loss_pct: f64,
) -> String {
    let mut runners: Vec<&StatusSnapshot> = cache.values().collect();
    runners.sort_by(|a, b| a.symbol.cmp(&b.symbol));
    let value = serde_json::json!({
        "hold": hold,
        "hold_reason": hold_reason,
        "last_config_change": last_config_change,
        "month_net_realized": month_net_realized,
        "month_loss_pct": month_loss_pct,
        "runners": runners,
    });
    serde_json::to_string(&value).unwrap_or_else(|_| "{\"error\":\"serialize\"}".to_string())
}
