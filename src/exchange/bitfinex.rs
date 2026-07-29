use std::collections::HashMap;

use async_trait::async_trait;
use tokio::time::Duration;

use crate::api::candles::{fetch_candle_history, fetch_candles, Candle};
use crate::api::endpoints::{
    cancel_order, fetch_account_fees, fetch_open_orders, fetch_order_history, place_order,
};
use crate::api::types::{OrderResult, TradeSignal, WsEvent};
use crate::api::websocket::{connect_authenticated, parse_auth_ws_message, parse_ws_message};
use crate::config::config::Config;

use super::{Exchange, WsStream};

/// Bitfinex implementation of the `Exchange` seam. Holds the config (which
/// carries the active `credential_mode`) and a shared HTTP client; all logic is
/// delegated to the existing `api::*` functions so behaviour is unchanged.
pub struct Bitfinex {
    config: Config,
    http: reqwest::Client,
}

impl Bitfinex {
    pub fn new(config: Config) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        Self { config, http }
    }
}

#[async_trait]
impl Exchange for Bitfinex {
    fn name(&self) -> &str {
        "bitfinex"
    }

    async fn place_order(&self, signal: &TradeSignal, symbol: &str) -> Result<OrderResult, String> {
        place_order(signal, symbol, &self.config, &self.http)
            .await
            .map_err(|e| e.to_string())
    }

    async fn cancel_order(&self, order_id: i64) -> Result<(), String> {
        cancel_order(order_id, &self.config, &self.http)
            .await
            .map_err(|e| e.to_string())
    }

    async fn fetch_open_orders(&self, symbol: &str) -> Result<Vec<i64>, String> {
        fetch_open_orders(symbol, &self.config, &self.http)
            .await
            .map_err(|e| e.to_string())
    }

    async fn fetch_order_history(&self, symbol: &str) -> Result<Vec<(i64, String)>, String> {
        fetch_order_history(symbol, &self.config, &self.http)
            .await
            .map_err(|e| e.to_string())
    }

    async fn fetch_account_fees(&self) -> Result<(f64, f64), String> {
        fetch_account_fees(&self.config, &self.http)
            .await
            .map_err(|e| e.to_string())
    }

    async fn fetch_candles(
        &self,
        symbol: &str,
        timeframe: &str,
        period: usize,
    ) -> Result<Vec<Candle>, String> {
        fetch_candles(symbol, timeframe, period, &self.config, &self.http)
            .await
            .map_err(|e| e.to_string())
    }

    async fn fetch_candle_history(
        &self,
        symbol: &str,
        timeframe: &str,
        limit: usize,
    ) -> Result<Vec<Candle>, String> {
        fetch_candle_history(symbol, timeframe, limit, &self.config, &self.http)
            .await
            .map_err(|e| e.to_string())
    }

    async fn connect_public(&self) -> Result<WsStream, String> {
        tokio_tungstenite::connect_async(self.config.api.ws_endpoint.as_str())
            .await
            .map(|(ws, _)| ws)
            .map_err(|e| e.to_string())
    }

    async fn connect_auth(&self) -> Result<WsStream, String> {
        connect_authenticated(&self.config)
            .await
            .map_err(|e| e.to_string())
    }

    fn subscribe_frame(&self, symbol: &str) -> String {
        serde_json::json!({
            "event": "subscribe",
            "channel": "ticker",
            "symbol": format!("t{}", symbol)
        })
        .to_string()
    }

    fn unsubscribe_frame(&self, symbol: &str) -> String {
        serde_json::json!({
            "event": "unsubscribe",
            "channel": "ticker",
            "symbol": format!("t{}", symbol)
        })
        .to_string()
    }

    fn parse_public(&self, raw: &str, chan_map: &HashMap<u64, String>) -> WsEvent {
        parse_ws_message(raw, chan_map)
    }

    fn parse_auth(&self, raw: &str) -> WsEvent {
        parse_auth_ws_message(raw)
    }
}
