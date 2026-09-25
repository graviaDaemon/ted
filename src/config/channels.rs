use chrono::{DateTime, Utc};
use std::collections::HashMap;
use tokio::sync::oneshot;

#[derive(Debug, Clone, PartialEq)]
pub enum RunnerMode {
    Simulation,
    Paper,
    Live,
}

/// Events feeding the TUI dashboard, emitted from the runner's existing hook
/// points (ticker ticks, fills, periodic snapshots, exit paths).
#[derive(Debug, Clone)]
pub enum TuiEvent {
    Ticker {
        symbol: String,
        bid: f64,
    },
    Fill {
        symbol: String,
        is_buy: bool,
        qty: f64,
        price: f64,
        realized_pnl: Option<f64>,
        ts: DateTime<Utc>,
    },
    Status {
        symbol: String,
        mode: String,
        realized: f64,
        unrealized: f64,
        equity: f64,
        position: f64,
        open_buys: usize,
        open_sells: usize,
        paused: bool,
        halted: bool,
        fees_paid: f64,
        open_lots: usize,
        trend: Option<String>,
        /// Trailing 7-day PnL% from daily rollups; None with < 2 rollup days.
        pnl_7d_pct: Option<f64>,
        /// RFC 3339 time since which no order has rested; None while any does.
        idle_since: Option<String>,
    },
    RunnerStopped {
        symbol: String,
    },
}

pub enum RunnerControl {
    SetAlgorithm {
        name: String,
        options: HashMap<String, String>,
    },
    GenerateOverview {
        verbose: bool,
        reply: oneshot::Sender<String>,
    },
    Pause,
    Resume,
    Kill,
    /// Circuit-breaker halt (plan/12): stop opening new buys, cancel resting buys,
    /// keep the exit ladder resting. Same state as the drawdown halt; cleared by
    /// `Resume`. Triggered account-wide by the monthly breaker.
    HaltBuys,
    /// Retire this pair cleanly (plan/12): stop opening, keep working the exit
    /// ladder, and shut the runner down once flat (position ≈ 0, no resting sells).
    FinishExits,
    /// Operator nudge (plan/14): cancel resting buys and re-size + rebuild the
    /// buy ladder on the next tick. Exits stay resting; config is unchanged.
    Rebuild,
    /// Clean app-wide shutdown (Ctrl-D / restart): persist resume state and exit
    /// WITHOUT cancelling resting orders, so the next launch reconciles against
    /// them. Distinct from `Kill`, which cancels orders and forgets the runner.
    Shutdown,
    #[allow(dead_code)]
    PruneOrder(i64),
}
