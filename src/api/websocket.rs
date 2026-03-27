use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use tokio_tungstenite::tungstenite::Message;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::error::Error;
use tokio::net::TcpStream;
use tokio::time::{timeout, Duration};
use chrono::Utc;
use crate::api::auth::sign_auth_payload;
use crate::api::types::{MarketData, WsEvent};
use crate::config::config::Config;

/// Connects to the Bitfinex public WebSocket, waits for the `info` event confirming
/// the platform is operational, subscribes to the ticker channel for `symbol`,
/// and returns the stream together with the assigned `chanId`.
pub async fn connect_and_subscribe(
    symbol: &str,
    config: &Config,
) -> Result<(WebSocketStream<MaybeTlsStream<TcpStream>>, u64), Box<dyn Error>> {
    let (mut ws_stream, _) = connect_async(config.ws_endpoint.as_str()).await?;

    // Wait for the info event and verify the platform is operational
    loop {
        let msg = ws_stream
            .next()
            .await
            .ok_or("Connection closed before info event")??;
        if let Message::Text(text) = msg {
            let val: Value = serde_json::from_str(&text)?;
            if val["event"] == "info" {
                let status = val["platform"]["status"].as_u64().unwrap_or(0);
                if status != 1 {
                    return Err("Bitfinex platform is in maintenance mode".into());
                }
                break;
            }
        }
    }

    // Send subscription request; Bitfinex requires the `t`-prefixed symbol
    let subscribe_msg = json!({
        "event": "subscribe",
        "channel": "ticker",
        "symbol": format!("t{}", symbol)
    });
    ws_stream
        .send(Message::Text(subscribe_msg.to_string().into()))
        .await?;

    // Wait for the `subscribed` confirmation to extract chanId
    let chan_id = loop {
        let msg = ws_stream
            .next()
            .await
            .ok_or("Connection closed before subscribed event")??;
        if let Message::Text(text) = msg {
            let val: Value = serde_json::from_str(&text)?;
            if val["event"] == "subscribed" && val["channel"] == "ticker" {
                let id = val["chanId"]
                    .as_u64()
                    .ok_or("Missing chanId in subscribed event")?;
                break id;
            } else if val["event"] == "error" {
                let code = val["code"].as_u64().unwrap_or(0);
                let msg_text = val["msg"].as_str().unwrap_or("unknown");
                return Err(format!("Subscription error {}: {}", code, msg_text).into());
            }
        }
    };

    Ok((ws_stream, chan_id))
}

/// Maps a 10-element Bitfinex v2 ticker array to `MarketData`.
/// Returns `Err` if fewer than 10 elements are present.
///
/// Bitfinex ticker field order:
///   [0] BID  [1] BID_SIZE  [2] ASK  [3] ASK_SIZE  [4] DAILY_CHANGE
///   [5] DAILY_CHANGE_RELATIVE  [6] LAST_PRICE  [7] VOLUME  [8] HIGH  [9] LOW
pub fn parse_ticker(symbol: &str, raw: &[f64]) -> Result<MarketData, String> {
    if raw.len() < 10 {
        return Err(format!("Expected 10 ticker fields, got {}", raw.len()));
    }
    Ok(MarketData {
        symbol: symbol.to_string(),
        bid: raw[0],
        bid_size: raw[1],
        ask: raw[2],
        ask_size: raw[3],
        daily_change: raw[4],
        daily_change_pct: raw[5],
        last_price: raw[6],
        volume: raw[7],
        high: raw[8],
        low: raw[9],
        timestamp: Utc::now(),
    })
}

/// Routes a raw WebSocket message string to the appropriate `WsEvent` variant.
///
/// Handles:
/// - JSON objects with an `"event"` field → `Info`, `Subscribed`, `Error`
/// - Data arrays `[chanId, "hb"]` → `Heartbeat`
/// - Data arrays `[chanId, [...ticker]]` → `TickerData` (symbol looked up via `chan_map`)
/// - Everything else → `Unknown`
pub fn parse_ws_message(msg: &str, chan_map: &HashMap<u64, String>) -> WsEvent {
    let val: Value = match serde_json::from_str(msg) {
        Ok(v) => v,
        Err(_) => return WsEvent::Unknown,
    };

    // Event object path
    if let Some(event_type) = val.get("event").and_then(|e| e.as_str()) {
        return match event_type {
            "info" => {
                let maintenance = val["platform"]["status"].as_u64().unwrap_or(1) != 1;
                WsEvent::Info { maintenance }
            }
            "subscribed" => {
                let chan_id = val["chanId"].as_u64().unwrap_or(0);
                let symbol = val["pair"]
                    .as_str()
                    .or_else(|| val["symbol"].as_str())
                    .unwrap_or("")
                    .to_string();
                WsEvent::Subscribed { chan_id, symbol }
            }
            "error" => {
                let code = val["code"].as_u64().unwrap_or(0) as u32;
                let message = val["msg"].as_str().unwrap_or("unknown").to_string();
                WsEvent::Error { code, message }
            }
            _ => WsEvent::Unknown,
        };
    }

    // Data array path: [chanId, payload]
    if let Some(arr) = val.as_array()
        && arr.len() >= 2
    {
        let chan_id = match arr[0].as_u64() {
            Some(id) => id,
            None => return WsEvent::Unknown,
        };

        // Heartbeat
        if arr[1].as_str() == Some("hb") {
            return WsEvent::Heartbeat;
        }

        // Ticker data
        if let Some(ticker_arr) = arr[1].as_array() {
            let raw: Option<Vec<f64>> = ticker_arr.iter().map(|v| v.as_f64()).collect();
            match raw {
                None => {
                    crate::logger::log("[WS]", "Ticker array contained non-numeric field — skipping.");
                }
                Some(raw) => {
                    if let Some(symbol) = chan_map.get(&chan_id) {
                        match parse_ticker(symbol, &raw) {
                            Ok(market_data) => return WsEvent::TickerData(market_data),
                            Err(e) => crate::logger::log(
                                "[WS]",
                                &format!("Failed to parse ticker for {}: {}", symbol, e),
                            ),
                        }
                    }
                }
            }
        }
    }

    WsEvent::Unknown
}

// ── Authenticated WebSocket ───────────────────────────────────────────────────

/// Connects to the Bitfinex *authenticated* WebSocket endpoint, performs the
/// HMAC-signed AUTH handshake, and returns the open stream.
///
/// The stream will receive order and wallet events for the account associated
/// with the API credentials in `config`.
pub async fn connect_authenticated(
    config: &Config,
) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>, Box<dyn Error>> {
    let (mut ws, _) = connect_async(config.auth_ws_endpoint.as_str()).await?;

    // Wait for the info event (15s timeout).
    let info_deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let remaining = info_deadline.saturating_duration_since(tokio::time::Instant::now());
        let msg = timeout(remaining, ws.next())
            .await
            .map_err(|_| "Auth WS: timed out waiting for info event")?
            .ok_or("Auth WS: connection closed before info event")??;
        if let Message::Text(text) = msg {
            let val: Value = serde_json::from_str(&text)?;
            if val["event"] == "info" {
                let status = val["platform"]["status"].as_u64().unwrap_or(0);
                if status != 1 {
                    return Err("Bitfinex platform is in maintenance mode".into());
                }
                break;
            }
        }
    }

    // Build and send the AUTH frame.
    let nonce = Utc::now().timestamp_millis().to_string();
    let sig   = sign_auth_payload(&config.secret, &nonce);
    let auth_msg = json!({
        "event":       "auth",
        "apiKey":      config.key,
        "authSig":     sig,
        "authNonce":   nonce,
        "authPayload": format!("AUTH{}", nonce),
        "filter":      ["trading", "wallet"]
    });
    ws.send(Message::Text(auth_msg.to_string().into())).await?;

    // Wait for the auth confirmation (15s timeout).
    let auth_deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let remaining = auth_deadline.saturating_duration_since(tokio::time::Instant::now());
        let msg = timeout(remaining, ws.next())
            .await
            .map_err(|_| "Auth WS: timed out waiting for auth confirmation")?
            .ok_or("Auth WS: connection closed before auth confirmation")??;
        if let Message::Text(text) = msg {
            let val: Value = serde_json::from_str(&text)?;
            if val["event"] == "auth" {
                if val["status"] == "OK" {
                    return Ok(ws);
                }
                let code    = val["code"].as_u64().unwrap_or(0);
                let message = val["msg"].as_str().unwrap_or("unknown").to_string();
                return Err(format!("Auth failed ({}): {}", code, message).into());
            }
        }
    }
}

/// Parses a raw text message from the authenticated WebSocket channel into a
/// `WsEvent` variant.
///
/// Handles:
/// - `{event: "auth"}` → `AuthConfirmed` / `AuthFailed`
/// - `[0, "os",  [...]]` → `OrderSnapshot`
/// - `[0, "on",  [...]]` → `OrderNew`
/// - `[0, "ou",  [...]]` or `[0, "oc", [...]]` → `OrderFilled` / `OrderCancelled`
/// - `[0, "ws",  [...]]` → `WalletSnapshot`
/// - `[0, "wu",  [...]]` → `WalletUpdate`
/// - `[0, "hb"]`         → `Heartbeat`
/// - Everything else     → `Unknown`
pub fn parse_auth_ws_message(msg: &str) -> WsEvent {
    let val: Value = match serde_json::from_str(msg) {
        Ok(v) => v,
        Err(_) => return WsEvent::Unknown,
    };

    // Event objects (auth handshake result).
    if let Some(event) = val.get("event").and_then(|e| e.as_str()) {
        if event == "auth" {
            return if val["status"] == "OK" {
                WsEvent::AuthConfirmed
            } else {
                let code    = val["code"].as_u64().unwrap_or(0) as u32;
                let message = val["msg"].as_str().unwrap_or("unknown").to_string();
                WsEvent::AuthFailed { code, message }
            };
        }
        return WsEvent::Unknown;
    }

    // Data arrays: [channel_id, type, payload]
    let arr = match val.as_array() {
        Some(a) if a.len() >= 2 => a,
        _ => return WsEvent::Unknown,
    };

    // Heartbeat on the auth channel.
    if arr[1].as_str() == Some("hb") {
        return WsEvent::Heartbeat;
    }

    let msg_type = match arr[1].as_str() {
        Some(t) => t,
        None => return WsEvent::Unknown,
    };

    match msg_type {
        // ── Order snapshot ────────────────────────────────────────────────
        "os" => {
            let ids: Vec<i64> = arr
                .get(2)
                .and_then(|v| v.as_array())
                .map(|orders| {
                    orders
                        .iter()
                        .filter_map(|o| o.get(0).and_then(|id| id.as_i64()))
                        .collect()
                })
                .unwrap_or_default();
            WsEvent::OrderSnapshot { order_ids: ids }
        }

        // ── New order acknowledged ────────────────────────────────────────
        "on" => {
            let order_id = arr
                .get(2)
                .and_then(|o| o.get(0))
                .and_then(|id| id.as_i64())
                .unwrap_or(0);
            WsEvent::OrderNew { order_id }
        }

        // ── Order cancel / fill ───────────────────────────────────────────
        "oc" | "ou" => {
            let order_id = arr
                .get(2)
                .and_then(|o| o.get(0))
                .and_then(|id| id.as_i64())
                .unwrap_or(0);
            // Field index 13 is the order status string, e.g. "EXECUTED @ …" or "CANCELED".
            let status = arr
                .get(2)
                .and_then(|o| o.get(13))
                .and_then(|s| s.as_str())
                .unwrap_or("");
            if status.starts_with("EXECUTED") {
                WsEvent::OrderFilled { order_id }
            } else {
                WsEvent::OrderCancelled { order_id }
            }
        }

        // ── Wallet snapshot ───────────────────────────────────────────────
        "ws" => {
            let balances: Vec<(String, String, f64)> = arr
                .get(2)
                .and_then(|v| v.as_array())
                .map(|entries| {
                    entries
                        .iter()
                        .filter_map(|e| {
                            let wallet_type = e.get(0)?.as_str()?.to_string();
                            let currency    = e.get(1)?.as_str()?.to_string();
                            // Index 4 is "available balance" (after margin etc.).
                            let available   = e.get(4).and_then(|v| v.as_f64()).unwrap_or(0.0);
                            Some((wallet_type, currency, available))
                        })
                        .collect()
                })
                .unwrap_or_default();
            WsEvent::WalletSnapshot { balances }
        }

        // ── Wallet update ─────────────────────────────────────────────────
        "wu" => {
            let entry = arr.get(2);
            let wallet_type = entry
                .and_then(|e| e.get(0))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let currency = entry
                .and_then(|e| e.get(1))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let available = entry
                .and_then(|e| e.get(4))
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            WsEvent::WalletUpdate { wallet_type, currency, available }
        }

        _ => WsEvent::Unknown,
    }
}
