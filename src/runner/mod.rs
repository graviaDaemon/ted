pub mod trade_log;
pub mod report;

use std::collections::{HashMap, HashSet};
use std::time::Instant;
use tokio::net::TcpStream;
use tokio::sync::mpsc::Receiver;
use tokio::time::{sleep, timeout, Duration};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_tungstenite::tungstenite::Message;
use futures_util::{SinkExt, StreamExt};
use chrono::{DateTime, Utc};

use crate::algorithm::{build_algorithm, Algorithm};
use crate::api::{
    connect_and_subscribe, connect_authenticated,
    parse_ws_message, parse_auth_ws_message,
    place_order, cancel_order,
    fetch_open_orders, fetch_order_history,
    fetch_candles,
    MarketData, TradeSignal, WsEvent,
};
use crate::config::channels::{RunnerControl, RunnerMode};
use crate::config::config::Config;
use crate::storage::db::{Db, FillRow, OrderRow, TickRow};
use trade_log::{TradeEntry, TradeLog};

const DB_FLUSH_BATCH_SIZE: usize = 100;

pub struct RunnerState {
    pub symbol: String,
    pub algorithm: Box<dyn Algorithm>,
    pub algo_name: String,
    pub options: HashMap<String, String>,
    pub mode: RunnerMode,
    pub paper: bool,
    pub paused: bool,
    pub trade_log: TradeLog,
    pub started_at: DateTime<Utc>,
    pub config: Config,
    pub live_order_ids: HashSet<i64>,
    pub http_client: reqwest::Client,
    pub last_order_time: Option<Instant>,
    pub pending_buy_orders: HashMap<i64, (f64, f64)>,
    pub pending_sell_orders: HashMap<i64, (f64, f64)>,
    pub trade_store: Option<crate::storage::TradeStore>,
    pub wallet_balances: HashMap<String, f64>,
    pub tick_buffer: Vec<TickRow>,
    pub runner_db_id: Option<i64>,
    pub db: Option<Db>,
    pub order_db_ids: HashMap<i64, i64>,
    pub last_bid: f64,
    pub last_ask: f64,
}

pub async fn run_runner(
    symbol: String,
    algo_name: String,
    mut options: HashMap<String, String>,
    mode: RunnerMode,
    mut control_rx: Receiver<RunnerControl>,
    config: Config,
) {
    let src = format!("RUNNER:{}", symbol);
    let paper = mode == RunnerMode::Paper;

    let db_path = crate::storage::data_dir().join("ted.db");
    let db = match Db::open(&db_path) {
        Ok(db) => db,
        Err(e) => {
            crate::logger::log(&src, &format!("Failed to open DB: {} — runner exiting.", e));
            return;
        }
    };

    let started_at = Utc::now();
    let runner_db_id = match db.insert_runner(&symbol, &algo_name, mode_label(&mode), &started_at.to_rfc3339()) {
        Ok(id) => id,
        Err(e) => {
            crate::logger::log(&src, &format!("Failed to insert runner row: {} — runner exiting.", e));
            return;
        }
    };

    let http_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_default();

    let mut auth_stream: Option<WebSocketStream<MaybeTlsStream<TcpStream>>> = None;

    if mode != RunnerMode::Simulation {
        match connect_authenticated(&config, paper).await {
            Ok(stream) => auth_stream = Some(stream),
            Err(e) => {
                crate::logger::log(&src, &format!("Auth WS connection failed: {} — runner exiting.", e));
                return;
            }
        }

        match wait_for_wallet_snapshot(auth_stream.as_mut().unwrap()).await {
            Ok(balances) => {
                let (base, quote) = extract_currencies(&symbol);
                let base_bal = balances.get(&base).copied().unwrap_or(0.0);
                let quote_bal = balances.get(&quote).copied().unwrap_or(0.0);
                crate::logger::log(
                    &src,
                    &format!("Wallet: {} {:.8}, {} {:.8}", base, base_bal, quote, quote_bal),
                );
                options.insert("initial_base_balance".to_string(), format!("{:.8}", base_bal));
                options.insert("initial_quote_balance".to_string(), format!("{:.8}", quote_bal));
            }
            Err(e) => {
                crate::logger::log(&src, &format!("Wallet snapshot failed: {} — runner exiting.", e));
                return;
            }
        }

        if should_fetch_atr(&algo_name, &options) {
            let timeframe = options.get("atr_timeframe").cloned().unwrap_or_else(|| "1h".to_string());
            let period = options.get("atr_period").and_then(|v| v.parse::<usize>().ok()).unwrap_or(14);
            let multiplier = options.get("atr_multiplier").and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.5);
            match fetch_candles(&symbol, &timeframe, period, &config, &http_client).await {
                Ok(candles) => match crate::algorithm::atr::compute_atr(&candles, period) {
                    Ok(atr) => {
                        let spacing = atr * multiplier;
                        crate::logger::log(&src, &format!("ATR({}) on {} = {:.8}, ×{:.2} → spacing {:.8}", period, timeframe, atr, multiplier, spacing));
                        options.insert("spacing".to_string(), format!("{:.8}", spacing));
                    }
                    Err(e) => {
                        crate::logger::log(&src, &format!("ATR computation failed: {} — runner exiting.", e));
                        return;
                    }
                },
                Err(e) => {
                    crate::logger::log(&src, &format!("Candle fetch failed: {} — runner exiting.", e));
                    return;
                }
            }
        }
    } else {
        options.entry("initial_base_balance".to_string()).or_insert_with(|| "0.0".to_string());
        options.entry("initial_quote_balance".to_string()).or_insert_with(|| "1000000.0".to_string());
    }

    let algorithm = match build_algorithm(&algo_name, &options) {
        Ok(a) => a,
        Err(e) => {
            crate::logger::log(&src, &format!("Failed to build algorithm '{}': {} — runner exiting.", algo_name, e));
            return;
        }
    };

    let (mut ws_stream, chan_id) = match connect_and_subscribe(&symbol, &config).await {
        Ok(v) => v,
        Err(e) => {
            crate::logger::log(&src, &format!("Failed to connect public WS: {} — runner exiting.", e));
            return;
        }
    };

    let mut chan_map: HashMap<u64, String> = HashMap::new();
    chan_map.insert(chan_id, symbol.clone());

    let (base_check, quote_check) = extract_currencies(&symbol);
    if base_check.is_empty() || quote_check.is_empty() {
        crate::logger::log(&src, "Warning: symbol format unrecognised — wallet balance checks will be unavailable.");
    }

    let atr_refresh_secs = config.startup_defaults.atr_refresh_mins * 60;
    let mut atr_refresh_interval: Option<tokio::time::Interval> = if should_fetch_atr(&algo_name, &options) {
        let mut iv = tokio::time::interval(Duration::from_secs(atr_refresh_secs));
        iv.tick().await;
        Some(iv)
    } else {
        None
    };

    let mut db_flush_interval = tokio::time::interval(Duration::from_secs(30));
    db_flush_interval.tick().await;

    let mut state = RunnerState {
        symbol: symbol.clone(),
        algorithm,
        algo_name: algo_name.clone(),
        options: options.clone(),
        mode,
        paper,
        paused: false,
        trade_log: TradeLog::new(),
        started_at,
        config: config.clone(),
        live_order_ids: HashSet::new(),
        http_client,
        last_order_time: None,
        pending_buy_orders: HashMap::new(),
        pending_sell_orders: HashMap::new(),
        trade_store: match crate::storage::TradeStore::open(&symbol) {
            Ok(s) => {
                crate::logger::log(&src, &format!("Trade history: {}", s.path.display()));
                Some(s)
            }
            Err(e) => {
                crate::logger::log(&src, &format!("Warning: could not open trade store: {}", e));
                None
            }
        },
        wallet_balances: HashMap::new(),
        tick_buffer: Vec::new(),
        runner_db_id: Some(runner_db_id),
        db: Some(db),
        order_db_ids: HashMap::new(),
        last_bid: 0.0,
        last_ask: 0.0,
    };

    crate::logger::log(&src, &format!("Runner started, algorithm: {}, mode: {}", algo_name, mode_label(&state.mode)));

    loop {
        tokio::select! {
            ws_result = timeout(Duration::from_secs(30), ws_stream.next()) => {
                match ws_result {
                    Err(_) => {
                        crate::logger::log(&src, "Public WS heartbeat timeout — reconnecting.");
                        match reconnect(&symbol, &config).await {
                            Some((new_stream, new_chan)) => {
                                ws_stream = new_stream;
                                chan_map.clear();
                                chan_map.insert(new_chan, symbol.clone());
                                state.algorithm.on_reconnect();
                                crate::logger::log(&src, "Reconnected.");
                            }
                            None => {
                                crate::logger::log(&src, "Reconnect failed — exiting.");
                                flush_tick_buffer(&mut state);
                                cancel_all_live_orders(&mut state).await;
                                if let Some(mut s) = auth_stream.take() { let _ = s.close(None).await; }
                                return;
                            }
                        }
                    }
                    Ok(ws_msg) => match ws_msg {
                        Some(Ok(Message::Text(text))) => {
                            let event = parse_ws_message(&text, &chan_map);
                            process_event(&mut state, event).await;
                        }
                        Some(Ok(Message::Ping(payload))) => {
                            if let Err(e) = ws_stream.send(Message::Pong(payload)).await {
                                crate::logger::log(&src, &format!("Failed to send Pong: {}", e));
                            }
                        }
                        Some(Ok(_)) => {}
                        Some(Err(e)) => {
                            crate::logger::log(&src, &format!("Public WS error: {}", e));
                            match reconnect(&symbol, &config).await {
                                Some((new_stream, new_chan)) => {
                                    ws_stream = new_stream;
                                    chan_map.clear();
                                    chan_map.insert(new_chan, symbol.clone());
                                    state.algorithm.on_reconnect();
                                    crate::logger::log(&src, "Reconnected.");
                                }
                                None => {
                                    crate::logger::log(&src, "Reconnect failed — exiting.");
                                    flush_tick_buffer(&mut state);
                                    cancel_all_live_orders(&mut state).await;
                                    if let Some(mut s) = auth_stream.take() { let _ = s.close(None).await; }
                                    return;
                                }
                            }
                        }
                        None => {
                            crate::logger::log(&src, "Public WS stream closed.");
                            match reconnect(&symbol, &config).await {
                                Some((new_stream, new_chan)) => {
                                    ws_stream = new_stream;
                                    chan_map.clear();
                                    chan_map.insert(new_chan, symbol.clone());
                                    state.algorithm.on_reconnect();
                                    crate::logger::log(&src, "Reconnected.");
                                }
                                None => {
                                    crate::logger::log(&src, "Reconnect failed — exiting.");
                                    flush_tick_buffer(&mut state);
                                    cancel_all_live_orders(&mut state).await;
                                    if let Some(mut s) = auth_stream.take() { let _ = s.close(None).await; }
                                    return;
                                }
                            }
                        }
                    }
                }
            }

            auth_msg = async {
                match auth_stream.as_mut() {
                    Some(s) => s.next().await,
                    None    => std::future::pending().await,
                }
            } => {
                match auth_msg {
                    Some(Ok(Message::Text(text))) => {
                        let event = parse_auth_ws_message(&text);
                        let fill_signals = process_auth_event(&mut state, event);
                        if !fill_signals.is_empty() {
                            dispatch_signals(&mut state, &fill_signals).await;
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        if let Some(ref mut s) = auth_stream {
                            let _ = s.send(Message::Pong(payload)).await;
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        crate::logger::log(&src, &format!("Auth WS error: {} — reconnecting.", e));
                        handle_auth_reconnect(&src, &symbol, &mut state, &mut auth_stream).await;
                    }
                    None => {
                        crate::logger::log(&src, "Auth WS stream closed — reconnecting.");
                        handle_auth_reconnect(&src, &symbol, &mut state, &mut auth_stream).await;
                    }
                }
            }

            _ = async {
                match atr_refresh_interval.as_mut() {
                    Some(iv) => { iv.tick().await; }
                    None => std::future::pending::<()>().await,
                }
            } => {
                let timeframe = state.options.get("atr_timeframe").cloned().unwrap_or_else(|| "1h".to_string());
                let period = state.options.get("atr_period").and_then(|v| v.parse::<usize>().ok()).unwrap_or(14);
                let multiplier = state.options.get("atr_multiplier").and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.5);
                match fetch_candles(&state.symbol, &timeframe, period, &state.config, &state.http_client).await {
                    Ok(candles) => match crate::algorithm::atr::compute_atr(&candles, period) {
                        Ok(atr) => {
                            let new_spacing = atr * multiplier;
                            crate::logger::log(&src, &format!("ATR refresh: {:.8} ×{:.2} → spacing {:.8}", atr, multiplier, new_spacing));
                            state.algorithm.on_spacing_update(new_spacing);
                        }
                        Err(e) => crate::logger::log(&src, &format!("ATR refresh: compute failed: {}", e)),
                    },
                    Err(e) => crate::logger::log(&src, &format!("ATR refresh: candle fetch failed: {}", e)),
                }
            }

            _ = db_flush_interval.tick() => {
                flush_tick_buffer(&mut state);
            }

            ctrl = control_rx.recv() => {
                match ctrl {
                    None => {
                        crate::logger::log(&src, "Control channel closed — runner exiting.");
                        flush_tick_buffer(&mut state);
                        cancel_all_live_orders(&mut state).await;
                        if let Some(mut s) = auth_stream.take() { let _ = s.close(None).await; }
                        if let Err(e) = ws_stream.close(None).await {
                            crate::logger::log(&src, &format!("Warning: WS close error: {}", e));
                        }
                        break;
                    }

                    Some(RunnerControl::Kill) => {
                        crate::logger::log(&src, "Kill received — stopping runner.");
                        flush_tick_buffer(&mut state);
                        cancel_all_live_orders(&mut state).await;
                        if let Some(mut s) = auth_stream.take() { let _ = s.close(None).await; }
                        if let Err(e) = ws_stream.close(None).await {
                            crate::logger::log(&src, &format!("Warning: WS close error: {}", e));
                        }
                        break;
                    }

                    Some(RunnerControl::Pause) => {
                        state.paused = true;
                        crate::logger::log(&src, "Runner paused.");
                    }

                    Some(RunnerControl::Resume) => {
                        state.paused = false;
                        crate::logger::log(&src, "Runner resumed.");
                    }

                    Some(RunnerControl::SetMode(new_mode)) => {
                        match new_mode {
                            RunnerMode::Live => {
                                cancel_all_live_orders(&mut state).await;
                                state.algorithm.on_live_enabled();
                                state.pending_buy_orders.clear();
                                state.pending_sell_orders.clear();
                                if let Some(mut s) = auth_stream.take() { let _ = s.close(None).await; }
                                state.paper = false;
                                state.mode = RunnerMode::Live;
                                crate::logger::log(&src, "LIVE TRADING ENABLED — algorithm reset.");
                                match connect_authenticated(&state.config, false).await {
                                    Ok(stream) => {
                                        auth_stream = Some(stream);
                                        crate::logger::log(&src, "Live auth WS connected.");
                                    }
                                    Err(e) => crate::logger::log(&src, &format!("Warning: live auth WS failed: {} — fill tracking unavailable.", e)),
                                }
                            }
                            RunnerMode::Paper => {
                                cancel_all_live_orders(&mut state).await;
                                state.algorithm.on_live_enabled();
                                state.pending_buy_orders.clear();
                                state.pending_sell_orders.clear();
                                if let Some(mut s) = auth_stream.take() { let _ = s.close(None).await; }
                                state.paper = true;
                                state.mode = RunnerMode::Paper;
                                match connect_authenticated(&state.config, true).await {
                                    Ok(stream) => {
                                        auth_stream = Some(stream);
                                        crate::logger::log(&src, "Paper mode — connected with paper credentials.");
                                    }
                                    Err(e) => crate::logger::log(&src, &format!("Warning: paper auth WS failed: {} — fill tracking unavailable.", e)),
                                }
                            }
                            RunnerMode::Simulation => {
                                cancel_all_live_orders(&mut state).await;
                                state.mode = RunnerMode::Simulation;
                                state.paper = false;
                                if let Some(mut s) = auth_stream.take() { let _ = s.close(None).await; }
                                crate::logger::log(&src, "Mode set to simulation.");
                            }
                        }
                    }

                    Some(RunnerControl::SetAlgorithm { name, options }) => {
                        match build_algorithm(&name, &options) {
                            Ok(new_algo) => {
                                crate::logger::log(&src, &format!("Algorithm switched to '{}'.", name));
                                state.algorithm = new_algo;
                                state.algo_name = name;
                                state.options = options;
                            }
                            Err(e) => crate::logger::log(&src, &format!("Failed to switch algorithm to '{}': {} — keeping current.", name, e)),
                        }
                    }

                    Some(RunnerControl::GenerateOverview { verbose, reply }) => {
                        flush_tick_buffer(&mut state);
                        let content = report::build_content(&state, verbose);
                        if reply.send(content).is_err() {
                            crate::logger::log(&src, "Overview request timed out.");
                        }
                    }

                    Some(RunnerControl::PruneOrder(id)) => {
                        if state.live_order_ids.remove(&id) {
                            crate::logger::log(&src, &format!("Order {} pruned.", id));
                        }
                        state.pending_buy_orders.remove(&id);
                        state.pending_sell_orders.remove(&id);
                        state.order_db_ids.remove(&id);
                    }
                }
            }
        }
    }
}

fn mode_label(mode: &RunnerMode) -> &'static str {
    match mode {
        RunnerMode::Simulation => "simulation",
        RunnerMode::Paper => "paper",
        RunnerMode::Live => "live",
    }
}

fn should_fetch_atr(algo_name: &str, options: &HashMap<String, String>) -> bool {
    algo_name == "grid"
        && !options.contains_key("spacing")
        && (options.contains_key("atr_period")
            || options.contains_key("atr_timeframe")
            || options.contains_key("atr_multiplier"))
}

async fn wait_for_wallet_snapshot(
    auth_ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
) -> Result<HashMap<String, f64>, String> {
    let result = timeout(Duration::from_secs(30), async {
        loop {
            match auth_ws.next().await {
                Some(Ok(Message::Text(text))) => {
                    if let WsEvent::WalletSnapshot { balances } = parse_auth_ws_message(&text) {
                        let mut map = HashMap::new();
                        for (wallet_type, currency, available) in balances {
                            if wallet_type == "exchange" {
                                map.insert(currency, available);
                            }
                        }
                        return Ok(map);
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(format!("Auth WS error while waiting for wallet snapshot: {}", e)),
                None => return Err("Auth WS closed before wallet snapshot".to_string()),
            }
        }
    })
    .await;

    match result {
        Ok(r) => r,
        Err(_) => Err("Timed out waiting for wallet snapshot (30s)".to_string()),
    }
}

fn flush_tick_buffer(state: &mut RunnerState) {
    if state.tick_buffer.is_empty() {
        return;
    }
    if let Some(db) = &state.db
        && let Err(e) = db.insert_tick_batch(&state.tick_buffer)
    {
        crate::logger::log(
            &format!("RUNNER:{}", state.symbol),
            &format!("Warning: DB tick flush failed: {}", e),
        );
    }
    state.tick_buffer.clear();
}

fn write_sim_order(state: &mut RunnerState, direction: &str, price: f64, quantity: f64) {
    if let (Some(db), Some(runner_id)) = (&state.db, state.runner_db_id) {
        let now = Utc::now().to_rfc3339();
        let _ = db.insert_order(&OrderRow {
            runner_id,
            exchange_id: None,
            direction: direction.to_string(),
            price,
            quantity,
            status: "filled".to_string(),
            placed_at: now.clone(),
            filled_at: Some(now),
        });
    }
}

fn write_live_order(state: &mut RunnerState, exchange_id: i64, direction: &str, price: f64, quantity: f64) {
    if let (Some(db), Some(runner_id)) = (&state.db, state.runner_db_id)
        && let Ok(db_id) = db.insert_order(&OrderRow {
            runner_id,
            exchange_id: Some(exchange_id),
            direction: direction.to_string(),
            price,
            quantity,
            status: "pending".to_string(),
            placed_at: Utc::now().to_rfc3339(),
            filled_at: None,
        })
    {
        state.order_db_ids.insert(exchange_id, db_id);
    }
}

async fn cancel_all_live_orders(state: &mut RunnerState) {
    if state.mode == RunnerMode::Simulation || state.live_order_ids.is_empty() {
        return;
    }
    let src = format!("RUNNER:{}", state.symbol);
    crate::logger::log(&src, &format!("Cancelling {} open order(s)…", state.live_order_ids.len()));
    let ids: Vec<i64> = state.live_order_ids.iter().copied().collect();
    for order_id in ids {
        match cancel_order(order_id, &state.config, &state.http_client, state.paper).await {
            Ok(()) => crate::logger::log(&src, &format!("Cancelled order {}.", order_id)),
            Err(e) => crate::logger::log(&src, &format!("Failed to cancel order {}: {}", order_id, e)),
        }
    }
    state.live_order_ids.clear();
    state.pending_buy_orders.clear();
    state.pending_sell_orders.clear();
}

async fn process_event(state: &mut RunnerState, event: WsEvent) {
    match event {
        WsEvent::TickerData(market_data) => process_tick(state, market_data).await,
        WsEvent::Heartbeat => {}
        WsEvent::Info { maintenance } => {
            if maintenance {
                crate::logger::log(&format!("RUNNER:{}", state.symbol), "Bitfinex platform entered maintenance mode.");
            }
        }
        WsEvent::Subscribed { chan_id, symbol } => {
            crate::logger::log(&format!("RUNNER:{}", state.symbol), &format!("Subscription confirmed: chanId={} symbol={}", chan_id, symbol));
        }
        WsEvent::Error { code, message } => {
            crate::logger::log(&format!("RUNNER:{}", state.symbol), &format!("Bitfinex error {}: {}", code, message));
        }
        WsEvent::Unknown => {}
        _ => {}
    }
}

async fn throttle_order(last_order_time: &mut Option<Instant>, throttle_ms: u64) {
    let min_gap = Duration::from_millis(throttle_ms);
    if let Some(t) = *last_order_time {
        let elapsed = t.elapsed();
        if elapsed < min_gap {
            sleep(min_gap - elapsed).await;
        }
    }
    *last_order_time = Some(Instant::now());
}

async fn dispatch_signals(state: &mut RunnerState, signals: &[TradeSignal]) {
    for sig in signals {
        let src = format!("RUNNER:{}", state.symbol);
        match sig {
            TradeSignal::Buy { price, quantity, reason, .. } => {
                if state.mode == RunnerMode::Simulation {
                    crate::logger::log(&src, &format!("[SIM] LIMIT BUY {:.8} @ {:.2} — {}", quantity, price, reason));
                    let _ = state.algorithm.on_fill(*price, true);
                    write_sim_order(state, "buy", *price, *quantity);
                } else {
                    let (_, quote) = extract_currencies(&state.symbol);
                    if !state.wallet_balances.is_empty() && !quote.is_empty()
                        && let Some(&bal) = state.wallet_balances.get(&quote)
                        && bal < price * quantity
                    {
                        crate::logger::log(&src, &format!("[{}] Insufficient {} ({:.4} < {:.4}) — skipping BUY.", mode_label(&state.mode), quote, bal, price * quantity));
                        continue;
                    }
                    if state.pending_buy_orders.values().any(|(p, _)| (p - price).abs() < 1e-6) {
                        crate::logger::log(&src, &format!("[{}] Buy at {:.2} already pending — skipping duplicate.", mode_label(&state.mode), price));
                        continue;
                    }
                    if state.last_bid > 0.0 && state.last_ask > 0.0 && *price >= state.last_ask {
                        crate::logger::log(&src, &format!("[{}] Buy at {:.2} would cross spread (ask={:.2}) — skipping.", mode_label(&state.mode), price, state.last_ask));
                        continue;
                    }
                    throttle_order(&mut state.last_order_time, state.config.startup_defaults.throttle_ms).await;
                    crate::logger::log(&src, &format!("[{}] Placing LIMIT BUY {:.8} @ {:.2} — {}", mode_label(&state.mode), quantity, price, reason));
                    match place_order(sig, &state.symbol, &state.config, &state.http_client, state.paper).await {
                        Ok(result) => {
                            crate::logger::log(&src, &format!("[{}] BUY placed — id={} status={}", mode_label(&state.mode), result.order_id, result.status));
                            if result.order_id != 0 {
                                state.live_order_ids.insert(result.order_id);
                                state.pending_buy_orders.insert(result.order_id, (*price, *quantity));
                                write_live_order(state, result.order_id, "buy", *price, *quantity);
                            }
                        }
                        Err(e) => crate::logger::log(&src, &format!("[{}] BUY FAILED: {}", mode_label(&state.mode), e)),
                    }
                }
            }
            TradeSignal::Sell { price, quantity, reason, .. } => {
                if state.mode == RunnerMode::Simulation {
                    crate::logger::log(&src, &format!("[SIM] LIMIT SELL {:.8} @ {:.2} — {}", quantity, price, reason));
                    let _ = state.algorithm.on_fill(*price, false);
                    write_sim_order(state, "sell", *price, *quantity);
                } else {
                    let (base, _) = extract_currencies(&state.symbol);
                    if !state.wallet_balances.is_empty() && !base.is_empty()
                        && let Some(&bal) = state.wallet_balances.get(&base)
                        && bal < *quantity
                    {
                        crate::logger::log(&src, &format!("[{}] Insufficient {} ({:.8} < {:.8}) — skipping SELL.", mode_label(&state.mode), base, bal, quantity));
                        continue;
                    }
                    if state.pending_sell_orders.values().any(|(p, _)| (p - price).abs() < 1e-6) {
                        crate::logger::log(&src, &format!("[{}] Sell at {:.2} already pending — skipping duplicate.", mode_label(&state.mode), price));
                        continue;
                    }
                    if state.last_bid > 0.0 && state.last_ask > 0.0 && *price <= state.last_bid {
                        crate::logger::log(&src, &format!("[{}] Sell at {:.2} would cross spread (bid={:.2}) — skipping.", mode_label(&state.mode), price, state.last_bid));
                        continue;
                    }
                    throttle_order(&mut state.last_order_time, state.config.startup_defaults.throttle_ms).await;
                    crate::logger::log(&src, &format!("[{}] Placing LIMIT SELL {:.8} @ {:.2} — {}", mode_label(&state.mode), quantity, price, reason));
                    match place_order(sig, &state.symbol, &state.config, &state.http_client, state.paper).await {
                        Ok(result) => {
                            crate::logger::log(&src, &format!("[{}] SELL placed — id={} status={}", mode_label(&state.mode), result.order_id, result.status));
                            if result.order_id != 0 {
                                state.live_order_ids.insert(result.order_id);
                                state.pending_sell_orders.insert(result.order_id, (*price, *quantity));
                                write_live_order(state, result.order_id, "sell", *price, *quantity);
                            }
                        }
                        Err(e) => crate::logger::log(&src, &format!("[{}] SELL FAILED: {}", mode_label(&state.mode), e)),
                    }
                }
            }
        }
    }
}

async fn process_tick(state: &mut RunnerState, market_data: MarketData) {
    if state.paused {
        return;
    }

    state.last_bid = market_data.bid;
    state.last_ask = market_data.ask;

    crate::logger::log(
        &format!("RUNNER:{}", state.symbol),
        &format!(
            "[{}] last={:.2} bid={:.2} ask={:.2} vol={:.4}",
            market_data.timestamp.format("%H:%M:%S"),
            market_data.last_price, market_data.bid, market_data.ask, market_data.volume,
        ),
    );

    let signals = state.algorithm.on_tick(&market_data);
    let has_signals = !signals.is_empty();

    dispatch_signals(state, &signals).await;

    let entry = TradeEntry {
        timestamp: market_data.timestamp,
        symbol: market_data.symbol.clone(),
        last_price: market_data.last_price,
        bid: market_data.bid,
        ask: market_data.ask,
        volume: market_data.volume,
        signals,
        dry_run: state.mode == RunnerMode::Simulation,
    };
    if let Some(ref mut store) = state.trade_store
        && let Err(e) = store.append(&entry)
    {
        crate::logger::log(&format!("RUNNER:{}", state.symbol), &format!("Warning: trade store write failed: {}", e));
    }
    state.trade_log.push(entry);

    if let Some(runner_id) = state.runner_db_id {
        state.tick_buffer.push(TickRow {
            runner_id,
            ts: market_data.timestamp.to_rfc3339(),
            last_price: market_data.last_price,
            bid: market_data.bid,
            ask: market_data.ask,
            volume: market_data.volume,
            has_signals,
        });
    }
    if state.tick_buffer.len() >= DB_FLUSH_BATCH_SIZE {
        flush_tick_buffer(state);
    }
}

async fn reconnect(
    symbol: &str,
    config: &Config,
) -> Option<(WebSocketStream<MaybeTlsStream<TcpStream>>, u64)> {
    const MAX_ATTEMPTS: u32 = 20;
    let src = format!("RUNNER:{}", symbol);

    for attempt in 1..=MAX_ATTEMPTS {
        let delay_secs = (2u64.pow(attempt - 1)).min(60);
        crate::logger::log(&src, &format!("Reconnecting in {}s (attempt {}/{})…", delay_secs, attempt, MAX_ATTEMPTS));
        sleep(Duration::from_secs(delay_secs)).await;
        match connect_and_subscribe(symbol, config).await {
            Ok(result) => return Some(result),
            Err(e) => crate::logger::log(&src, &format!("Reconnect attempt {}/{} failed: {}", attempt, MAX_ATTEMPTS, e)),
        }
    }
    None
}

async fn handle_auth_reconnect(
    src: &str,
    symbol: &str,
    state: &mut RunnerState,
    auth_stream: &mut Option<WebSocketStream<MaybeTlsStream<TcpStream>>>,
) {
    let paper = state.paper;
    let reconnected = reconnect_auth(symbol, &state.config, paper, auth_stream).await;

    if reconnected {
        sync_orders_after_reconnect(src, symbol, state).await;
        crate::logger::log(src, "Auth WS recovered — live trading resumed.");
    } else {
        crate::logger::log(src, "!!! Auth WS permanently failed — cancelling all live orders !!!");
        cancel_all_live_orders(state).await;
        if let Some(mut s) = auth_stream.take() {
            let _ = s.close(None).await;
        }
    }
}

async fn sync_orders_after_reconnect(src: &str, symbol: &str, state: &mut RunnerState) {
    let open_ids: HashSet<i64> =
        match fetch_open_orders(symbol, &state.config, &state.http_client, state.paper).await {
            Ok(ids) => ids.into_iter().collect(),
            Err(e) => {
                crate::logger::log(src, &format!("Reconnect sync: fetch open orders failed: {}", e));
                return;
            }
        };

    let history: HashMap<i64, String> =
        match fetch_order_history(symbol, &state.config, &state.http_client, state.paper).await {
            Ok(pairs) => pairs.into_iter().collect(),
            Err(e) => {
                crate::logger::log(src, &format!("Reconnect sync: fetch order history failed: {}", e));
                return;
            }
        };

    let all_pending: Vec<(i64, f64, f64, bool)> = state
        .pending_buy_orders
        .iter()
        .map(|(&id, &(p, q))| (id, p, q, true))
        .chain(state.pending_sell_orders.iter().map(|(&id, &(p, q))| (id, p, q, false)))
        .collect();

    for (order_id, price, qty, is_buy) in all_pending {
        if open_ids.contains(&order_id) {
            continue;
        }
        match history.get(&order_id).map(|s| s.as_str()) {
            Some(status) if status.starts_with("EXECUTED") => {
                if is_buy {
                    state.pending_buy_orders.remove(&order_id);
                } else {
                    state.pending_sell_orders.remove(&order_id);
                }
                state.live_order_ids.remove(&order_id);
                let fill_signals = state.algorithm.on_fill(price, is_buy);
                crate::logger::log(src, &format!("Reconnect sync: order {} filled @ {:.2}.", order_id, price));
                write_fill_to_db(state, order_id, is_buy, price, qty);
                state.order_db_ids.remove(&order_id);
                if !fill_signals.is_empty() {
                    dispatch_signals(state, &fill_signals).await;
                }
            }
            Some(_) => {
                if is_buy {
                    state.pending_buy_orders.remove(&order_id);
                } else {
                    state.pending_sell_orders.remove(&order_id);
                }
                state.live_order_ids.remove(&order_id);
                if let Some(db) = &state.db
                    && let Some(&db_id) = state.order_db_ids.get(&order_id)
                {
                    let _ = db.update_order_status(db_id, "cancelled", None);
                }
                state.order_db_ids.remove(&order_id);
                crate::logger::log(src, &format!("Reconnect sync: order {} cancelled.", order_id));
            }
            None => {
                crate::logger::log(src, &format!("Reconnect sync: order {} not in history — leaving as pending.", order_id));
            }
        }
    }
}

fn write_fill_to_db(state: &mut RunnerState, exchange_id: i64, is_buy: bool, price: f64, qty: f64) {
    if let (Some(db), Some(runner_id)) = (&state.db, state.runner_db_id) {
        let filled_at = Utc::now().to_rfc3339();
        let db_order_id = state.order_db_ids.get(&exchange_id).copied();
        if let Some(oid) = db_order_id {
            let _ = db.update_order_status(oid, "filled", Some(&filled_at));
        }
        let _ = db.insert_fill(&FillRow {
            runner_id,
            order_db_id: db_order_id,
            exchange_id: Some(exchange_id),
            direction: if is_buy { "buy" } else { "sell" }.to_string(),
            price,
            quantity: qty,
            realized_pnl: None,
            filled_at,
        });
    }
}

async fn reconnect_auth(
    symbol: &str,
    config: &Config,
    paper: bool,
    auth_stream: &mut Option<WebSocketStream<MaybeTlsStream<TcpStream>>>,
) -> bool {
    if let Some(mut s) = auth_stream.take() {
        let _ = s.close(None).await;
    }

    const MAX_ATTEMPTS: u32 = 10;
    let src = format!("RUNNER:{}", symbol);

    for attempt in 1..=MAX_ATTEMPTS {
        let delay_secs = (2u64.pow(attempt - 1)).min(30);
        crate::logger::log(&src, &format!("Auth WS reconnecting in {}s (attempt {}/{})…", delay_secs, attempt, MAX_ATTEMPTS));
        sleep(Duration::from_secs(delay_secs)).await;
        match connect_authenticated(config, paper).await {
            Ok(stream) => {
                *auth_stream = Some(stream);
                crate::logger::log(&src, "Auth WS reconnected.");
                return true;
            }
            Err(e) => crate::logger::log(&src, &format!("Auth WS reconnect attempt {}/{} failed: {}", attempt, MAX_ATTEMPTS, e)),
        }
    }

    crate::logger::log(&src, "Auth WS reconnect failed — all attempts exhausted.");
    false
}

fn process_auth_event(state: &mut RunnerState, event: WsEvent) -> Vec<TradeSignal> {
    let src = format!("RUNNER:{}", state.symbol);
    let mut fill_signals: Vec<TradeSignal> = Vec::new();
    match event {
        WsEvent::AuthConfirmed => {
            crate::logger::log(&src, "Auth WS: authentication confirmed.");
        }
        WsEvent::AuthFailed { code, message } => {
            crate::logger::log(&src, &format!("Auth WS: authentication failed ({}: {}).", code, message));
        }
        WsEvent::OrderSnapshot { order_ids } => {
            let snapshot: HashSet<i64> = order_ids.into_iter().collect();

            let filled_buys: Vec<(i64, f64, f64)> = state
                .pending_buy_orders
                .iter()
                .filter(|(id, _)| !snapshot.contains(*id))
                .map(|(&id, &(p, q))| (id, p, q))
                .collect();
            let filled_sells: Vec<(i64, f64, f64)> = state
                .pending_sell_orders
                .iter()
                .filter(|(id, _)| !snapshot.contains(*id))
                .map(|(&id, &(p, q))| (id, p, q))
                .collect();

            for (id, price, qty) in &filled_buys {
                state.live_order_ids.remove(id);
                state.pending_buy_orders.remove(id);
                fill_signals.extend(state.algorithm.on_fill(*price, true));
                crate::logger::log(&src, &format!("Order {} absent from snapshot — assumed filled @ {:.2}.", id, price));
                write_fill_to_db(state, *id, true, *price, *qty);
                state.order_db_ids.remove(id);
            }
            for (id, price, qty) in &filled_sells {
                state.live_order_ids.remove(id);
                state.pending_sell_orders.remove(id);
                fill_signals.extend(state.algorithm.on_fill(*price, false));
                crate::logger::log(&src, &format!("Order {} absent from snapshot — assumed filled @ {:.2}.", id, price));
                write_fill_to_db(state, *id, false, *price, *qty);
                state.order_db_ids.remove(id);
            }

            let stale: Vec<i64> = state.live_order_ids.iter().copied().filter(|id| !snapshot.contains(id)).collect();
            for id in &stale {
                state.live_order_ids.remove(id);
            }
            if !stale.is_empty() {
                crate::logger::log(&src, &format!("Auth WS snapshot: pruned {} stale order id(s).", stale.len()));
            }
        }
        WsEvent::OrderFilled { order_id } => {
            state.live_order_ids.remove(&order_id);
            if let Some((price, qty)) = state.pending_buy_orders.remove(&order_id) {
                fill_signals.extend(state.algorithm.on_fill(price, true));
                crate::logger::log(&src, &format!("Buy order {} filled @ {:.2}.", order_id, price));
                write_fill_to_db(state, order_id, true, price, qty);
                state.order_db_ids.remove(&order_id);
            } else if let Some((price, qty)) = state.pending_sell_orders.remove(&order_id) {
                fill_signals.extend(state.algorithm.on_fill(price, false));
                crate::logger::log(&src, &format!("Sell order {} filled @ {:.2}.", order_id, price));
                write_fill_to_db(state, order_id, false, price, qty);
                state.order_db_ids.remove(&order_id);
            } else {
                crate::logger::log(&src, &format!("Order {} filled — not in pending maps.", order_id));
            }
        }
        WsEvent::OrderCancelled { order_id } => {
            state.live_order_ids.remove(&order_id);
            state.pending_buy_orders.remove(&order_id);
            state.pending_sell_orders.remove(&order_id);
            if let Some(db) = &state.db
                && let Some(&db_id) = state.order_db_ids.get(&order_id)
            {
                let _ = db.update_order_status(db_id, "cancelled", None);
            }
            state.order_db_ids.remove(&order_id);
            crate::logger::log(&src, &format!("Order {} cancelled.", order_id));
        }
        WsEvent::WalletSnapshot { balances } => {
            let (base, quote) = extract_currencies(&state.symbol);
            state.wallet_balances.clear();
            for (wallet_type, currency, available) in balances {
                if wallet_type == "exchange"
                    && (base.is_empty() || quote.is_empty() || currency == base || currency == quote)
                {
                    state.wallet_balances.insert(currency, available);
                }
            }
            crate::logger::log(&src, &format!("Wallet snapshot: {} balance(s) loaded.", state.wallet_balances.len()));
            let base_bal  = state.wallet_balances.get(&base).copied().unwrap_or(0.0);
            let quote_bal = state.wallet_balances.get(&quote).copied().unwrap_or(0.0);
            state.algorithm.on_balance_update(base_bal, quote_bal);
        }
        WsEvent::WalletUpdate { wallet_type, currency, available } => {
            if wallet_type == "exchange" {
                let (base, quote) = extract_currencies(&state.symbol);
                if base.is_empty() || quote.is_empty() || currency == base || currency == quote {
                    state.wallet_balances.insert(currency.clone(), available);
                    crate::logger::log(&src, &format!("Wallet update: {} available = {:.8}", currency, available));
                    let base_bal  = state.wallet_balances.get(&base).copied().unwrap_or(0.0);
                    let quote_bal = state.wallet_balances.get(&quote).copied().unwrap_or(0.0);
                    state.algorithm.on_balance_update(base_bal, quote_bal);
                }
            }
        }
        WsEvent::Heartbeat => {}
        _ => {}
    }
    fill_signals
}

fn extract_currencies(symbol: &str) -> (String, String) {
    if let Some(pos) = symbol.find(':') {
        return (symbol[..pos].to_string(), symbol[pos + 1..].to_string());
    }
    if symbol.len() == 6 {
        return (symbol[..3].to_string(), symbol[3..].to_string());
    }
    const KNOWN_QUOTES: &[&str] = &["USD", "UST", "EUR", "BTC", "ETH", "EOS", "XCH"];
    for q in KNOWN_QUOTES {
        if symbol.len() > q.len() && symbol.ends_with(q) {
            let base = &symbol[..symbol.len() - q.len()];
            return (base.to_string(), q.to_string());
        }
    }
    crate::logger::log("[RUNNER]", &format!("Warning: could not parse currencies from symbol '{}'.", symbol));
    (String::new(), String::new())
}
