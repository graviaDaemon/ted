pub mod bitfinex;

use std::collections::HashMap;

use async_trait::async_trait;
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::api::candles::Candle;
use crate::api::types::{OrderResult, TradeSignal, WsEvent};

/// Concrete websocket stream type. The engine owns the select-loop and
/// reconnect/backoff; the exchange only hands it freshly connected (and, for
/// auth, fully authenticated) streams.
pub type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// The seam between the engine and a market. Everything the engine does that is
/// exchange-specific goes through here; the engine keeps owning the socket
/// select-loop, channel-id map, reconnect/backoff and event routing.
///
/// Bitfinex is currently the sole implementation. A second market would be a
/// new `impl Exchange` plus a `config.exchange` dispatch in `main.rs` — no
/// engine, runner, or algorithm changes.
#[async_trait]
pub trait Exchange: Send + Sync {
    #[allow(dead_code)]
    fn name(&self) -> &str;

    // REST — driven from the engine's rest_worker.
    async fn place_order(&self, signal: &TradeSignal, symbol: &str) -> Result<OrderResult, String>;
    async fn cancel_order(&self, order_id: i64) -> Result<(), String>;
    async fn fetch_open_orders(&self, symbol: &str) -> Result<Vec<i64>, String>;
    async fn fetch_order_history(&self, symbol: &str) -> Result<Vec<(i64, String)>, String>;
    /// The account's exchange-trading (maker, taker) fee rates as fractions.
    async fn fetch_account_fees(&self) -> Result<(f64, f64), String>;
    async fn fetch_candles(&self, symbol: &str, timeframe: &str, period: usize)
        -> Result<Vec<Candle>, String>;
    async fn fetch_candle_history(&self, symbol: &str, timeframe: &str, limit: usize)
        -> Result<Vec<Candle>, String>;

    // WS — the engine drives the sockets; these produce connected streams,
    // the subscribe/unsubscribe payloads, and exchange-neutral parsed events.
    async fn connect_public(&self) -> Result<WsStream, String>;
    async fn connect_auth(&self) -> Result<WsStream, String>;
    fn subscribe_frame(&self, symbol: &str) -> String;
    fn unsubscribe_frame(&self, symbol: &str) -> String;
    fn parse_public(&self, raw: &str, chan_map: &HashMap<u64, String>) -> WsEvent;
    fn parse_auth(&self, raw: &str) -> WsEvent;
}
