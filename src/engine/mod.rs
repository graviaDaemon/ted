pub mod channels;

use std::collections::HashMap;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{sleep, timeout, Duration};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_tungstenite::tungstenite::Message;
use futures_util::{SinkExt, StreamExt};

use crate::api::candles::{fetch_candles, Candle};
use crate::api::endpoints::{cancel_order, fetch_open_orders, fetch_order_history, place_order};
use crate::api::types::{OrderResult, TradeSignal};
use crate::api::websocket::{connect_authenticated, parse_auth_ws_message, parse_ws_message};
use crate::api::WsEvent;
use crate::config::config::Config;
use channels::{EngineEvent, EngineRequest};

#[derive(Clone)]
pub struct EngineHandle {
    pub(crate) request_tx: mpsc::Sender<EngineRequest>,
}

impl EngineHandle {
    pub async fn subscribe(&self, symbol: String, event_tx: mpsc::Sender<EngineEvent>) {
        let _ = self.request_tx.send(EngineRequest::Subscribe { symbol, event_tx }).await;
    }

    pub async fn unsubscribe(&self, symbol: String) {
        let _ = self.request_tx.send(EngineRequest::Unsubscribe { symbol }).await;
    }

    pub async fn place_order(&self, signal: TradeSignal, symbol: String) -> Result<OrderResult, String> {
        let (tx, rx) = oneshot::channel();
        let _ = self.request_tx.send(EngineRequest::PlaceOrder { signal, symbol, reply: tx }).await;
        rx.await.map_err(|_| "engine dropped reply".to_string())?
    }

    pub async fn cancel_order(&self, order_id: i64) -> Result<(), String> {
        let (tx, rx) = oneshot::channel();
        let _ = self.request_tx.send(EngineRequest::CancelOrder { order_id, reply: tx }).await;
        rx.await.map_err(|_| "engine dropped reply".to_string())?
    }

    pub async fn fetch_open_orders(&self, symbol: String) -> Result<Vec<i64>, String> {
        let (tx, rx) = oneshot::channel();
        let _ = self.request_tx.send(EngineRequest::FetchOpenOrders { symbol, reply: tx }).await;
        rx.await.map_err(|_| "engine dropped reply".to_string())?
    }

    pub async fn fetch_order_history(&self, symbol: String) -> Result<Vec<(i64, String)>, String> {
        let (tx, rx) = oneshot::channel();
        let _ = self.request_tx.send(EngineRequest::FetchOrderHistory { symbol, reply: tx }).await;
        rx.await.map_err(|_| "engine dropped reply".to_string())?
    }

    pub async fn fetch_candles(&self, symbol: String, timeframe: String, period: usize) -> Result<Vec<Candle>, String> {
        let (tx, rx) = oneshot::channel();
        let _ = self.request_tx.send(EngineRequest::FetchCandles { symbol, timeframe, period, reply: tx }).await;
        rx.await.map_err(|_| "engine dropped reply".to_string())?
    }
}

pub fn spawn_engine(config: Config) -> (EngineHandle, tokio::task::JoinHandle<()>) {
    let (req_tx, req_rx) = mpsc::channel::<EngineRequest>(256);
    let handle = EngineHandle { request_tx: req_tx };
    let join = tokio::spawn(async move {
        Engine::run(config, req_rx).await;
    });
    (handle, join)
}

enum RestJob {
    PlaceOrder   { signal: TradeSignal, symbol: String, reply: oneshot::Sender<Result<OrderResult, String>> },
    CancelOrder  { order_id: i64,                       reply: oneshot::Sender<Result<(), String>> },
    FetchOrders  { symbol: String,                      reply: oneshot::Sender<Result<Vec<i64>, String>> },
    FetchHistory { symbol: String,                      reply: oneshot::Sender<Result<Vec<(i64, String)>, String>> },
    FetchCandles { symbol: String, timeframe: String, period: usize, reply: oneshot::Sender<Result<Vec<Candle>, String>> },
}

async fn rest_worker(config: Config, http_client: reqwest::Client, mut rest_rx: mpsc::Receiver<RestJob>) {
    while let Some(job) = rest_rx.recv().await {
        match job {
            RestJob::PlaceOrder { signal, symbol, reply } => {
                let result = place_order(&signal, &symbol, &config, &http_client)
                    .await.map_err(|e| e.to_string());
                let _ = reply.send(result);
            }
            RestJob::CancelOrder { order_id, reply } => {
                let result = cancel_order(order_id, &config, &http_client)
                    .await.map_err(|e| e.to_string());
                let _ = reply.send(result);
            }
            RestJob::FetchOrders { symbol, reply } => {
                let result = fetch_open_orders(&symbol, &config, &http_client)
                    .await.map_err(|e| e.to_string());
                let _ = reply.send(result);
            }
            RestJob::FetchHistory { symbol, reply } => {
                let result = fetch_order_history(&symbol, &config, &http_client)
                    .await.map_err(|e| e.to_string());
                let _ = reply.send(result);
            }
            RestJob::FetchCandles { symbol, timeframe, period, reply } => {
                let result = fetch_candles(&symbol, &timeframe, period, &config, &http_client)
                    .await.map_err(|e| e.to_string());
                let _ = reply.send(result);
            }
        }
    }
}

struct Engine {
    config: Config,
    request_rx: mpsc::Receiver<EngineRequest>,
    rest_tx: mpsc::Sender<RestJob>,
    pub_ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    chan_map: HashMap<u64, String>,
    subscribers: HashMap<String, mpsc::Sender<EngineEvent>>,
    auth_ws: Option<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    auth_subscribers: Vec<mpsc::Sender<EngineEvent>>,
    last_wallet_snapshot: Option<Vec<(String, String, f64)>>,
}

impl Engine {
    async fn run(config: Config, request_rx: mpsc::Receiver<EngineRequest>) {
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_default();

        let (rest_tx, rest_rx) = mpsc::channel::<RestJob>(128);
        tokio::spawn(rest_worker(config.clone(), http_client, rest_rx));

        let auth_ws = connect_authenticated(&config).await.ok();

        let pub_ws = match connect_pub_ws(&config).await {
            Some(ws) => ws,
            None => {
                crate::logger::log("[ENGINE]", "Initial public WS connection failed — engine exiting.");
                return;
            }
        };

        let mut engine = Engine {
            config,
            request_rx,
            rest_tx,
            pub_ws,
            chan_map: HashMap::new(),
            subscribers: HashMap::new(),
            auth_ws,
            auth_subscribers: Vec::new(),
            last_wallet_snapshot: None,
        };

        engine.event_loop().await;
    }

    async fn event_loop(&mut self) {
        loop {
            tokio::select! {
                req = self.request_rx.recv() => {
                    match req {
                        None => {
                            self.broadcast_all(EngineEvent::EngineShutdown).await;
                            break;
                        }
                        Some(r) => self.handle_request(r).await,
                    }
                }

                ws_result = timeout(Duration::from_secs(30), self.pub_ws.next()) => {
                    match ws_result {
                        Err(_) => self.reconnect_pub_ws().await,
                        Ok(Some(Ok(Message::Text(text)))) => {
                            let event = parse_ws_message(&text, &self.chan_map);
                            self.route_pub_event(event).await;
                        }
                        Ok(Some(Ok(Message::Ping(p)))) => {
                            let _ = self.pub_ws.send(Message::Pong(p)).await;
                        }
                        Ok(Some(Err(_))) | Ok(None) => self.reconnect_pub_ws().await,
                        Ok(Some(Ok(_))) => {}
                    }
                }

                auth_msg = Self::next_auth(&mut self.auth_ws) => {
                    match auth_msg {
                        Some(Ok(Message::Text(text))) => {
                            let event = parse_auth_ws_message(&text);
                            self.broadcast_auth_event(event).await;
                        }
                        Some(Ok(Message::Ping(p))) => {
                            if let Some(ws) = &mut self.auth_ws {
                                let _ = ws.send(Message::Pong(p)).await;
                            }
                        }
                        Some(Err(_)) | None => self.reconnect_auth_ws().await,
                        Some(Ok(_)) => {}
                    }
                }
            }
        }
    }

    async fn handle_request(&mut self, req: EngineRequest) {
        match req {
            EngineRequest::Subscribe { symbol, event_tx } => {
                if !self.subscribers.contains_key(&symbol) {
                    let sub_msg = serde_json::json!({
                        "event": "subscribe",
                        "channel": "ticker",
                        "symbol": format!("t{}", symbol)
                    });
                    let _ = self.pub_ws.send(Message::Text(sub_msg.to_string().into())).await;
                }
                if let Some(balances) = &self.last_wallet_snapshot {
                    let _ = event_tx.send(EngineEvent::WalletSnapshot { balances: balances.clone() }).await;
                }
                self.auth_subscribers.push(event_tx.clone());
                if self.auth_ws.is_some() {
                    let _ = event_tx.send(EngineEvent::AuthConnected).await;
                }
                self.subscribers.insert(symbol, event_tx);
            }
            EngineRequest::Unsubscribe { symbol } => {
                if self.subscribers.remove(&symbol).is_some() {
                    let unsub_msg = serde_json::json!({
                        "event": "unsubscribe",
                        "channel": "ticker",
                        "symbol": format!("t{}", symbol)
                    });
                    let _ = self.pub_ws.send(Message::Text(unsub_msg.to_string().into())).await;
                }
            }
            EngineRequest::PlaceOrder { signal, symbol, reply } => {
                let _ = self.rest_tx.send(RestJob::PlaceOrder { signal, symbol, reply }).await;
            }
            EngineRequest::CancelOrder { order_id, reply } => {
                let _ = self.rest_tx.send(RestJob::CancelOrder { order_id, reply }).await;
            }
            EngineRequest::FetchOpenOrders { symbol, reply } => {
                let _ = self.rest_tx.send(RestJob::FetchOrders { symbol, reply }).await;
            }
            EngineRequest::FetchOrderHistory { symbol, reply } => {
                let _ = self.rest_tx.send(RestJob::FetchHistory { symbol, reply }).await;
            }
            EngineRequest::FetchCandles { symbol, timeframe, period, reply } => {
                let _ = self.rest_tx.send(RestJob::FetchCandles { symbol, timeframe, period, reply }).await;
            }
        }
    }

    async fn route_pub_event(&mut self, event: WsEvent) {
        match event {
            WsEvent::TickerData(md) => {
                if let Some(tx) = self.subscribers.get(&md.symbol) {
                    let _ = tx.send(EngineEvent::Tick(md)).await;
                }
            }
            WsEvent::Subscribed { chan_id, symbol } => {
                self.chan_map.insert(chan_id, symbol);
            }
            WsEvent::Info { maintenance: true } => {
                self.broadcast_all(EngineEvent::Maintenance).await;
            }
            WsEvent::Error { code, message } => {
                crate::logger::log("[ENGINE]", &format!("Public WS error {}: {}", code, message));
            }
            _ => {}
        }
    }

    async fn broadcast_auth_event(&mut self, event: WsEvent) {
        let ev = match event {
            WsEvent::AuthConfirmed                                       => EngineEvent::AuthConnected,
            WsEvent::AuthFailed { code, message }                        => EngineEvent::AuthFailed { code, message },
            WsEvent::OrderSnapshot { order_ids }                         => EngineEvent::OrderSnapshot { order_ids },
            WsEvent::OrderFilled { order_id }                            => EngineEvent::OrderFilled { order_id },
            WsEvent::OrderCancelled { order_id }                         => EngineEvent::OrderCancelled { order_id },
            WsEvent::OrderNew { .. }                                     => EngineEvent::OrderNew,
            WsEvent::WalletSnapshot { balances }                         => {
                self.last_wallet_snapshot = Some(balances.clone());
                EngineEvent::WalletSnapshot { balances }
            }
            WsEvent::WalletUpdate { wallet_type, currency, available }   => {
                if let Some(snapshot) = &mut self.last_wallet_snapshot {
                    if let Some(entry) = snapshot.iter_mut().find(|(wt, cur, _)| wt == &wallet_type && cur == &currency) {
                        entry.2 = available;
                    } else {
                        snapshot.push((wallet_type.clone(), currency.clone(), available));
                    }
                }
                EngineEvent::WalletUpdate { wallet_type, currency, available }
            }
            _ => return,
        };
        self.broadcast_all(ev).await;
    }

    async fn broadcast_all(&mut self, event: EngineEvent) {
        let mut dead = vec![];
        for (i, tx) in self.auth_subscribers.iter().enumerate() {
            if tx.send(event.clone()).await.is_err() {
                dead.push(i);
            }
        }
        for i in dead.into_iter().rev() {
            self.auth_subscribers.swap_remove(i);
        }
    }

    async fn reconnect_pub_ws(&mut self) {
        crate::logger::log("[ENGINE]", "Public WS disconnected — reconnecting…");
        const MAX_ATTEMPTS: u32 = 20;
        for attempt in 1..=MAX_ATTEMPTS {
            let delay = (2u64.pow(attempt - 1)).min(60);
            sleep(Duration::from_secs(delay)).await;
            if let Some(ws) = connect_pub_ws(&self.config).await {
                self.pub_ws = ws;
                for symbol in self.subscribers.keys() {
                    let sub_msg = serde_json::json!({
                        "event": "subscribe",
                        "channel": "ticker",
                        "symbol": format!("t{}", symbol)
                    });
                    let _ = self.pub_ws.send(Message::Text(sub_msg.to_string().into())).await;
                }
                self.broadcast_all(EngineEvent::PublicWsReconnected).await;
                crate::logger::log("[ENGINE]", "Public WS reconnected.");
                return;
            }
            crate::logger::log("[ENGINE]", &format!("Public WS reconnect attempt {}/{} failed.", attempt, MAX_ATTEMPTS));
        }
        crate::logger::log("[ENGINE]", "Public WS reconnect exhausted — engine shutting down.");
        self.broadcast_all(EngineEvent::EngineShutdown).await;
    }

    async fn reconnect_auth_ws(&mut self) {
        crate::logger::log("[ENGINE]", "Auth WS disconnected — reconnecting…");
        if let Some(mut s) = self.auth_ws.take() {
            let _ = s.close(None).await;
        }
        const MAX_ATTEMPTS: u32 = 10;
        for attempt in 1..=MAX_ATTEMPTS {
            let delay = (2u64.pow(attempt - 1)).min(30);
            sleep(Duration::from_secs(delay)).await;
            match connect_authenticated(&self.config).await {
                Ok(ws) => {
                    self.auth_ws = Some(ws);
                    crate::logger::log("[ENGINE]", "Auth WS reconnected.");
                }
                Err(e) => {
                    crate::logger::log("[ENGINE]", &format!("Auth WS reconnect {}/{} failed: {}", attempt, MAX_ATTEMPTS, e));
                    continue;
                }
            }
            self.broadcast_all(EngineEvent::AuthConnected).await;
            return;
        }
        crate::logger::log("[ENGINE]", "Auth WS reconnect exhausted.");
        self.broadcast_all(EngineEvent::EngineShutdown).await;
    }

    async fn next_auth(
        auth_ws: &mut Option<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    ) -> Option<Result<Message, tokio_tungstenite::tungstenite::Error>> {
        match auth_ws {
            Some(ws) => ws.next().await,
            None => std::future::pending().await,
        }
    }
}

async fn connect_pub_ws(config: &Config) -> Option<WebSocketStream<MaybeTlsStream<TcpStream>>> {
    match tokio_tungstenite::connect_async(config.api.ws_endpoint.as_str()).await {
        Ok((ws, _)) => Some(ws),
        Err(e) => {
            crate::logger::log("[ENGINE]", &format!("Public WS connect failed: {}", e));
            None
        }
    }
}
