use chrono::{DateTime, Utc};
use serde::Serialize;

#[allow(dead_code)]
pub struct MarketData {
    pub symbol: String,
    pub bid: f64,
    pub bid_size: f64,
    pub ask: f64,
    pub ask_size: f64,
    pub last_price: f64,
    pub volume: f64,
    pub high: f64,
    pub low: f64,
    pub daily_change: f64,
    pub daily_change_pct: f64,
    pub timestamp: DateTime<Utc>
}

#[derive(Serialize)]
pub enum TradeSignal {
    Buy { price: f64, quantity: f64, reason: String, price_decimals: u32 },
    Sell { price: f64, quantity: f64, reason: String, price_decimals: u32 },
}

/// Result returned from a successful REST order submission.
pub struct OrderResult {
    pub order_id: i64,
    pub status: String,
    pub text: String,
}

pub enum WsEvent {
    // ── Public channel events ─────────────────────────────────────────────
    TickerData(MarketData),
    Heartbeat,
    Info { maintenance: bool },
    Subscribed { chan_id: u64, symbol: String },
    Error { code: u32, message: String },
    Unknown,

    // ── Authenticated channel events ──────────────────────────────────────
    /// AUTH frame accepted by the exchange.
    AuthConfirmed,
    /// AUTH frame rejected.
    AuthFailed { code: u32, message: String },
    /// Initial snapshot of open order ids upon auth connect.
    OrderSnapshot { order_ids: Vec<i64> },
    /// Exchange acknowledged a new order.
    #[allow(dead_code)]
    OrderNew { order_id: i64 },
    /// Order fully executed (filled).
    OrderFilled { order_id: i64 },
    /// Order cancelled.
    OrderCancelled { order_id: i64 },
    /// Initial wallet balances snapshot: `(wallet_type, currency, available)`.
    WalletSnapshot { balances: Vec<(String, String, f64)> },
    /// Incremental wallet balance update.
    WalletUpdate { wallet_type: String, currency: String, available: f64 },
}