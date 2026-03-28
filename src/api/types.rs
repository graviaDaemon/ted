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

pub struct OrderResult {
    pub order_id: i64,
    pub status: String,
    pub text: String,
}

pub enum WsEvent {
    TickerData(MarketData),
    Heartbeat,
    Info { maintenance: bool },
    Subscribed { chan_id: u64, symbol: String },
    Error { code: u32, message: String },
    Unknown,
    AuthConfirmed,
    AuthFailed { code: u32, message: String },
    OrderSnapshot { order_ids: Vec<i64> },
    #[allow(dead_code)]
    OrderNew { order_id: i64 },
    OrderFilled { order_id: i64 },
    OrderCancelled { order_id: i64 },
    WalletSnapshot { balances: Vec<(String, String, f64)> },
    WalletUpdate { wallet_type: String, currency: String, available: f64 },
}