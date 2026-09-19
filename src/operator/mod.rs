//! Headless operator support (plan/12): control-surface listener, the
//! un-bypassable guardrail governor, the monthly circuit breaker, and status
//! snapshots. The run loop that ties these together lives in `main::run_headless`
//! so it can reuse the existing command dispatch. The strategic decisions
//! (what to trade, when to re-tune) live in Lara, not here — T.E.D only executes
//! and guards.

pub mod breaker;
pub mod control;
pub mod guardrails;

use crate::config::config::Guardrails;
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

impl StatusSnapshot {
    /// A zeroed placeholder inserted the moment a runner is spawned, so `status`
    /// reports the runner immediately instead of an empty list until its first
    /// periodic Status event arrives. Without it the operator reads zero runners
    /// in the seconds after a (crash-)restart and acts blind. The first real
    /// Status event overwrites this.
    pub fn seed(symbol: &str, mode: &str) -> Self {
        StatusSnapshot {
            symbol: symbol.to_string(),
            mode: mode.to_string(),
            realized: 0.0,
            unrealized: 0.0,
            equity: 0.0,
            position: 0.0,
            open_buys: 0,
            open_sells: 0,
            paused: false,
            halted: false,
            fees_paid: 0.0,
            open_lots: 0,
            trend: None,
            pnl_7d_pct: None,
        }
    }
}

pub type StatusCache = HashMap<String, StatusSnapshot>;

/// Build the JSON reply for a `status` command: per-runner snapshots, account-level
/// month-to-date PnL, the breaker hold flag, last-change ts, and the guardrail
/// limits. The guardrails are T.E.D's single source of truth for pair/capital
/// policy — the operator (Lara) reads them here instead of keeping its own copy.
pub fn status_json(
    cache: &StatusCache,
    hold: bool,
    hold_reason: Option<&str>,
    last_config_change: Option<&str>,
    month_net_realized: f64,
    month_loss_pct: f64,
    guardrails: &Guardrails,
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
        "guardrails": {
            "whitelisted_pairs": guardrails.whitelisted_pairs,
            "max_capital_per_pair": guardrails.max_capital_per_pair,
            "min_days_between_config_changes": guardrails.min_days_between_config_changes,
            "max_monthly_loss_pct": guardrails.max_monthly_loss_pct,
        },
    });
    serde_json::to_string(&value).unwrap_or_else(|_| "{\"error\":\"serialize\"}".to_string())
}
