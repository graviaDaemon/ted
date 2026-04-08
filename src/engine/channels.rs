use crate::api::candles::Candle;
use crate::api::types::{MarketData, OrderResult, TradeSignal};
use tokio::sync::{mpsc, oneshot};

pub enum EngineRequest {
    Subscribe {
        symbol: String,
        event_tx: mpsc::Sender<EngineEvent>,
    },
    Unsubscribe {
        symbol: String,
    },
    PlaceOrder {
        signal: TradeSignal,
        symbol: String,
        reply: oneshot::Sender<Result<OrderResult, String>>,
    },
    CancelOrder {
        order_id: i64,
        reply: oneshot::Sender<Result<(), String>>,
    },
    FetchOpenOrders {
        symbol: String,
        reply: oneshot::Sender<Result<Vec<i64>, String>>,
    },
    FetchOrderHistory {
        symbol: String,
        reply: oneshot::Sender<Result<Vec<(i64, String)>, String>>,
    },
    FetchCandles {
        symbol: String,
        timeframe: String,
        period: usize,
        reply: oneshot::Sender<Result<Vec<Candle>, String>>,
    },
}

#[derive(Clone)]
pub enum EngineEvent {
    Tick(MarketData),
    OrderFilled {
        order_id: i64,
    },
    OrderCancelled {
        order_id: i64,
    },
    OrderNew,
    OrderSnapshot {
        order_ids: Vec<i64>,
    },
    WalletSnapshot {
        balances: Vec<(String, String, f64)>,
    },
    WalletUpdate {
        wallet_type: String,
        currency: String,
        available: f64,
    },
    AuthConnected,
    AuthFailed {
        code: u32,
        message: String,
    },
    PublicWsReconnected,
    Maintenance,
    EngineShutdown,
}
