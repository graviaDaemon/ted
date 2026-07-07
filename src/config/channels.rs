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
    /// Clean app-wide shutdown (Ctrl-D / restart): persist resume state and exit
    /// WITHOUT cancelling resting orders, so the next launch reconciles against
    /// them. Distinct from `Kill`, which cancels orders and forgets the runner.
    Shutdown,
    #[allow(dead_code)]
    PruneOrder(i64),
}
