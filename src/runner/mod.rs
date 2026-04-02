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
    MarketData, TradeSignal, WsEvent,
};
use crate::config::channels::RunnerControl;
use crate::config::config::Config;
use trade_log::{TradeEntry, TradeLog};

pub struct RunnerState {
    pub symbol: String,
    pub algorithm: Box<dyn Algorithm>,
    pub dry_run: bool,
    pub paused: bool,
    pub trade_log: TradeLog,
    pub started_at: DateTime<Utc>,
    pub config: Config,
    pub live_order_ids: HashSet<i64>,
    pub http_client: reqwest::Client,
    pub last_order_time: Option<Instant>,
    pub pending_buy_orders: HashMap<i64, f64>,
    pub pending_sell_orders: HashMap<i64, f64>,
    pub trade_store: Option<crate::storage::TradeStore>,
    pub wallet_balances: HashMap<String, f64>,
}

pub async fn run_runner(
    symbol: String,
    algorithm: Box<dyn Algorithm>,
    mut control_rx: Receiver<RunnerControl>,
    config: Config,
) {
    let src = format!("RUNNER:{}", symbol);

    let (mut ws_stream, chan_id) = match connect_and_subscribe(&symbol, &config).await {
        Ok(v) => v,
        Err(e) => {
            crate::logger::log(&src, &format!("Failed to connect: {} — runner exiting.", e));
            return;
        }
    };

    let mut chan_map: HashMap<u64, String> = HashMap::new();
    chan_map.insert(chan_id, symbol.clone());

    let algo_name = algorithm.name().to_string();
    let mut state = RunnerState {
        symbol: symbol.clone(),
        algorithm,
        dry_run: true,
        paused: false,
        trade_log: TradeLog::new(),
        started_at: Utc::now(),
        config: config.clone(),
        live_order_ids: HashSet::new(),
        http_client: reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_default(),
        last_order_time: None,
        pending_buy_orders: HashMap::new(),
        pending_sell_orders: HashMap::new(),
        wallet_balances: HashMap::new(),
        trade_store: match crate::storage::TradeStore::open(&symbol) {
            Ok(s) => {
                crate::logger::log(&src, &format!("Trade history: {}", s.path.display()));
                Some(s)
            }
            Err(e) => {
                crate::logger::log(&src, &format!("Warning: could not open trade store: {} — history will not be persisted.", e));
                None
            }
        },
    };

    crate::logger::log(
        &src,
        &format!("Runner started, algorithm: {}, mode: dry-run", algo_name),
    );

    let (base_check, quote_check) = extract_currencies(&symbol);
    if base_check.is_empty() || quote_check.is_empty() {
        crate::logger::log(
            &src,
            "Warning: symbol format unrecognised — wallet balance checks will be unavailable.",
        );
    }

    let mut auth_stream: Option<WebSocketStream<MaybeTlsStream<TcpStream>>> = None;

    loop {
        tokio::select! {
            ws_result = timeout(Duration::from_secs(30), ws_stream.next()) => {
                match ws_result {
                    Err(_elapsed) => {
                        crate::logger::log(&src, "WebSocket heartbeat timeout — reconnecting.");
                        match reconnect(&symbol, &config).await {
                            Some((new_stream, new_chan)) => {
                                ws_stream = new_stream;
                                chan_map.clear();
                                chan_map.insert(new_chan, symbol.clone());
                                state.algorithm.on_reconnect();
                                crate::logger::log(&src, "Reconnected after timeout.");
                            }
                            None => {
                                crate::logger::log(&src, "Reconnect failed — cancelling live orders and exiting.");
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
                            crate::logger::log(&src, &format!("WebSocket error: {}", e));
                            match reconnect(&symbol, &config).await {
                                Some((new_stream, new_chan)) => {
                                    ws_stream = new_stream;
                                    chan_map.clear();
                                    chan_map.insert(new_chan, symbol.clone());
                                    state.algorithm.on_reconnect();
                                    crate::logger::log(&src, "Reconnected.");
                                }
                                None => {
                                    crate::logger::log(&src, "Reconnect failed — cancelling live orders and exiting.");
                                    cancel_all_live_orders(&mut state).await;
                                    if let Some(mut s) = auth_stream.take() { let _ = s.close(None).await; }
                                    return;
                                }
                            }
                        }

                        None => {
                            crate::logger::log(&src, "WebSocket stream closed.");
                            match reconnect(&symbol, &config).await {
                                Some((new_stream, new_chan)) => {
                                    ws_stream = new_stream;
                                    chan_map.clear();
                                    chan_map.insert(new_chan, symbol.clone());
                                    state.algorithm.on_reconnect();
                                    crate::logger::log(&src, "Reconnected.");
                                }
                                None => {
                                    crate::logger::log(&src, "Reconnect failed — cancelling live orders and exiting.");
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
                        process_auth_event(&mut state, event);
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

            ctrl = control_rx.recv() => {
                match ctrl {
                    None => {
                        crate::logger::log(&src, "Control channel closed — runner exiting.");
                        cancel_all_live_orders(&mut state).await;
                        if let Some(mut s) = auth_stream.take() {
                            let _ = s.close(None).await;
                        }
                        if let Err(e) = ws_stream.close(None).await {
                            crate::logger::log(&src, &format!("Warning: WebSocket close error: {}", e));
                        }
                        break;
                    }

                    Some(RunnerControl::Kill) => {
                        crate::logger::log(&src, "Kill received — stopping runner.");
                        cancel_all_live_orders(&mut state).await;
                        if let Some(mut s) = auth_stream.take() {
                            let _ = s.close(None).await;
                        }
                        if let Err(e) = ws_stream.close(None).await {
                            crate::logger::log(&src, &format!("Warning: WebSocket close error: {}", e));
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

                    Some(RunnerControl::EnableLive) => {
                        state.dry_run = false;
                        state.algorithm.on_live_enabled();
                        state.pending_buy_orders.clear();
                        state.pending_sell_orders.clear();
                        crate::logger::log(
                            &src,
                            "LIVE TRADING ENABLED — algorithm reset. Initial orders will be placed on the next tick.",
                        );
                        match connect_authenticated(&state.config).await {
                            Ok(stream) => {
                                auth_stream = Some(stream);
                                crate::logger::log(&src, "Authenticated WebSocket connected.");
                            }
                            Err(e) => crate::logger::log(
                                &src,
                                &format!("Warning: auth WS failed: {} — fill tracking unavailable.", e),
                            ),
                        }
                    }

                    Some(RunnerControl::DisableLive) => {
                        state.dry_run = true;
                        if let Some(mut s) = auth_stream.take() {
                            let _ = s.close(None).await;
                        }
                        crate::logger::log(&src, "Live trading disabled — back to dry-run.");
                    }

                    Some(RunnerControl::SetAlgorithm { name, options }) => {
                        match build_algorithm(&name, &options) {
                            Ok(new_algo) => {
                                crate::logger::log(
                                    &src,
                                    &format!("Algorithm switched to '{}'.", name),
                                );
                                state.algorithm = new_algo;
                            }
                            Err(e) => {
                                crate::logger::log(
                                    &src,
                                    &format!(
                                        "Failed to switch algorithm to '{}': {} — keeping current.",
                                        name, e
                                    ),
                                );
                            }
                        }
                    }

                    Some(RunnerControl::GenerateOverview { verbose, reply }) => {
                        let content = report::build_content(&state, verbose);
                        if reply.send(content).is_err() {
                            crate::logger::log(&src, "Overview request timed out before content was sent.");
                        }
                    }

                    Some(RunnerControl::PruneOrder(id)) => {
                        if state.live_order_ids.remove(&id) {
                            crate::logger::log(&src, &format!("Order {} pruned from live tracking.", id));
                        }
                        state.pending_buy_orders.remove(&id);
                        state.pending_sell_orders.remove(&id);
                    }
                }
            }
        }
    }
}

async fn cancel_all_live_orders(state: &mut RunnerState) {
    if state.dry_run || state.live_order_ids.is_empty() {
        return;
    }
    let src = format!("RUNNER:{}", state.symbol);
    crate::logger::log(
        &src,
        &format!("Cancelling {} open order(s) before shutdown…", state.live_order_ids.len()),
    );
    for &order_id in &state.live_order_ids {
        match cancel_order(order_id, &state.config, &state.http_client).await {
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
                crate::logger::log(
                    &format!("RUNNER:{}", state.symbol),
                    "Bitfinex platform entered maintenance mode.",
                );
            }
        }
        WsEvent::Subscribed { chan_id, symbol } => {
            crate::logger::log(
                &format!("RUNNER:{}", state.symbol),
                &format!("Subscription confirmed: chanId={} symbol={}", chan_id, symbol),
            );
        }
        WsEvent::Error { code, message } => {
            crate::logger::log(
                &format!("RUNNER:{}", state.symbol),
                &format!("Bitfinex error {}: {}", code, message),
            );
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

async fn process_tick(state: &mut RunnerState, market_data: MarketData) {
    if state.paused {
        return;
    }

    crate::logger::log(
        &format!("RUNNER:{}", state.symbol),
        &format!(
            "[{}] last={:.2} bid={:.2} ask={:.2} vol={:.4}",
            market_data.timestamp.format("%H:%M:%S"),
            market_data.last_price,
            market_data.bid,
            market_data.ask,
            market_data.volume,
        ),
    );

    let signals = state.algorithm.on_tick(&market_data);

    for sig in &signals {
        let src = format!("RUNNER:{}", state.symbol);
        match sig {
            TradeSignal::Buy { price, quantity, reason, .. } => {
                if state.dry_run {
                    crate::logger::log(
                        &src,
                        &format!(
                            "[DRY RUN] Would place LIMIT BUY  {:.8} @ {:.2} — {}",
                            quantity, price, reason
                        ),
                    );
                    // Simulate an immediate fill so the algorithm seeds the sell side.
                    state.algorithm.on_fill(*price, true);
                } else {
                    let (_, quote) = extract_currencies(&state.symbol);
                    if !state.wallet_balances.is_empty() && !quote.is_empty()
                        && let Some(&bal) = state.wallet_balances.get(&quote)
                        && bal < price * quantity
                    {
                        crate::logger::log(
                            &src,
                            &format!(
                                "[LIVE] Insufficient {} ({:.4} < {:.4}) — skipping BUY.",
                                quote, bal, price * quantity
                            ),
                        );
                        continue;
                    }
                    if state.pending_buy_orders.values().any(|&p| (p - price).abs() < 1e-6) {
                        crate::logger::log(
                            &src,
                            &format!("[LIVE] Buy at {:.2} already pending — skipping duplicate.", price),
                        );
                        continue;
                    }
                    if market_data.bid > 0.0 && market_data.ask > 0.0 && *price >= market_data.ask {
                        crate::logger::log(
                            &src,
                            &format!("[LIVE] Buy at {:.2} would cross the spread (ask={:.2}) — skipping.", price, market_data.ask),
                        );
                        continue;
                    }
                    throttle_order(&mut state.last_order_time, state.config.throttle_ms).await;
                    crate::logger::log(
                        &src,
                        &format!("[LIVE] Placing LIMIT BUY {:.8} @ {:.2} — {}", quantity, price, reason),
                    );
                    match place_order(sig, &state.symbol, &state.config, &state.http_client).await {
                        Ok(result) => {
                            crate::logger::log(
                                &src,
                                &format!("[LIVE] BUY order placed — id={} status={} {}", result.order_id, result.status, result.text),
                            );
                            if result.order_id != 0 {
                                state.live_order_ids.insert(result.order_id);
                                state.pending_buy_orders.insert(result.order_id, *price);
                            }
                        }
                        Err(e) => crate::logger::log(
                            &src,
                            &format!("[LIVE] BUY order FAILED: {}", e),
                        ),
                    }
                }
            }
            TradeSignal::Sell { price, quantity, reason, .. } => {
                if state.dry_run {
                    crate::logger::log(
                        &src,
                        &format!(
                            "[DRY RUN] Would place LIMIT SELL {:.8} @ {:.2} — {}",
                            quantity, price, reason
                        ),
                    );
                    state.algorithm.on_fill(*price, false);
                } else {
                    let (base, _) = extract_currencies(&state.symbol);
                    if !state.wallet_balances.is_empty() && !base.is_empty()
                        && let Some(&bal) = state.wallet_balances.get(&base)
                        && bal < *quantity
                    {
                        crate::logger::log(
                            &src,
                            &format!(
                                "[LIVE] Insufficient {} ({:.8} < {:.8}) — skipping SELL.",
                                base, bal, quantity
                            ),
                        );
                        continue;
                    }
                    if state.pending_sell_orders.values().any(|&p| (p - price).abs() < 1e-6) {
                        crate::logger::log(
                            &src,
                            &format!("[LIVE] Sell at {:.2} already pending — skipping duplicate.", price),
                        );
                        continue;
                    }
                    if market_data.bid > 0.0 && market_data.ask > 0.0 && *price <= market_data.bid {
                        crate::logger::log(
                            &src,
                            &format!("[LIVE] Sell at {:.2} would cross the spread (bid={:.2}) — skipping.", price, market_data.bid),
                        );
                        continue;
                    }
                    throttle_order(&mut state.last_order_time, state.config.throttle_ms).await;
                    crate::logger::log(
                        &src,
                        &format!("[LIVE] Placing LIMIT SELL {:.8} @ {:.2} — {}", quantity, price, reason),
                    );
                    match place_order(sig, &state.symbol, &state.config, &state.http_client).await {
                        Ok(result) => {
                            crate::logger::log(
                                &src,
                                &format!("[LIVE] SELL order placed — id={} status={} {}", result.order_id, result.status, result.text),
                            );
                            if result.order_id != 0 {
                                state.live_order_ids.insert(result.order_id);
                                state.pending_sell_orders.insert(result.order_id, *price);
                            }
                        }
                        Err(e) => crate::logger::log(
                            &src,
                            &format!("[LIVE] SELL order FAILED: {}", e),
                        ),
                    }
                }
            }
        }
    }

    let entry = TradeEntry {
        timestamp: market_data.timestamp,
        symbol: market_data.symbol.clone(),
        last_price: market_data.last_price,
        bid: market_data.bid,
        ask: market_data.ask,
        volume: market_data.volume,
        signals,
        dry_run: state.dry_run,
    };

    if let Some(ref mut store) = state.trade_store
        && let Err(e) = store.append(&entry)
    {
        crate::logger::log(
            &format!("RUNNER:{}", state.symbol),
            &format!("Warning: failed to persist trade entry: {}", e),
        );
    }

    state.trade_log.push(entry);
}

async fn reconnect(
    symbol: &str,
    config: &Config,
) -> Option<(WebSocketStream<MaybeTlsStream<TcpStream>>, u64)> {
    const MAX_ATTEMPTS: u32 = 20;
    let src = format!("RUNNER:{}", symbol);

    for attempt in 1..=MAX_ATTEMPTS {
        let delay_secs = (2u64.pow(attempt - 1)).min(60);
        crate::logger::log(
            &src,
            &format!(
                "Reconnecting in {}s (attempt {}/{})…",
                delay_secs, attempt, MAX_ATTEMPTS
            ),
        );
        sleep(Duration::from_secs(delay_secs)).await;

        match connect_and_subscribe(symbol, config).await {
            Ok(result) => return Some(result),
            Err(e) => crate::logger::log(
                &src,
                &format!(
                    "Reconnect attempt {}/{} failed: {}",
                    attempt, MAX_ATTEMPTS, e
                ),
            ),
        }
    }

    crate::logger::log(
        &src,
        &format!("All {} reconnect attempts failed.", MAX_ATTEMPTS),
    );
    None
}

async fn handle_auth_reconnect(
    src: &str,
    symbol: &str,
    state: &mut RunnerState,
    auth_stream: &mut Option<WebSocketStream<MaybeTlsStream<TcpStream>>>,
) {
    let reconnected = reconnect_auth(symbol, &state.config, auth_stream).await;

    if reconnected {
        state.dry_run = true;

        match connect_authenticated(&state.config).await {
            Ok(stream) => {
                *auth_stream = Some(stream);
                state.dry_run = false;
                crate::logger::log(src, "Auth WS recovered — live trading resumed. Open orders preserved.");
            }
            Err(e) => crate::logger::log(
                src,
                &format!("Auth WS re-enable failed after reconnect: {} — staying in dry-run.", e),
            ),
        }
    } else {
        crate::logger::log(src, "!!! Auth WS permanently failed — cancelling all live orders and disabling live trading !!!");
        cancel_all_live_orders(state).await;
        state.dry_run = true;
        if let Some(mut s) = auth_stream.take() {
            let _ = s.close(None).await;
        }
    }
}

async fn reconnect_auth(
    symbol: &str,
    config: &Config,
    auth_stream: &mut Option<WebSocketStream<MaybeTlsStream<TcpStream>>>,
) -> bool {
    if let Some(mut s) = auth_stream.take() {
        let _ = s.close(None).await;
    }

    const MAX_ATTEMPTS: u32 = 10;
    let src = format!("RUNNER:{}", symbol);

    for attempt in 1..=MAX_ATTEMPTS {
        let delay_secs = (2u64.pow(attempt - 1)).min(30);
        crate::logger::log(
            &src,
            &format!(
                "Auth WS reconnecting in {}s (attempt {}/{})…",
                delay_secs, attempt, MAX_ATTEMPTS
            ),
        );
        sleep(Duration::from_secs(delay_secs)).await;

        match connect_authenticated(config).await {
            Ok(stream) => {
                *auth_stream = Some(stream);
                crate::logger::log(&src, "Auth WS reconnected.");
                return true;
            }
            Err(e) => crate::logger::log(
                &src,
                &format!("Auth WS reconnect attempt {}/{} failed: {}", attempt, MAX_ATTEMPTS, e),
            ),
        }
    }

    crate::logger::log(&src, "Auth WS reconnect failed — all attempts exhausted.");
    false
}

fn process_auth_event(state: &mut RunnerState, event: WsEvent) {
    let src = format!("RUNNER:{}", state.symbol);
    match event {
        WsEvent::AuthConfirmed => {
            crate::logger::log(&src, "Auth WS: authentication confirmed.");
        }
        WsEvent::AuthFailed { code, message } => {
            crate::logger::log(
                &src,
                &format!("Auth WS: authentication failed ({}: {}) — fill tracking unavailable.", code, message),
            );
        }
        WsEvent::OrderSnapshot { order_ids } => {
            let snapshot: HashSet<i64> = order_ids.into_iter().collect();

            let filled_buys: Vec<(i64, f64)> = state.pending_buy_orders.iter()
                .filter(|(id, _)| !snapshot.contains(*id))
                .map(|(&id, &price)| (id, price))
                .collect();
            let filled_sells: Vec<(i64, f64)> = state.pending_sell_orders.iter()
                .filter(|(id, _)| !snapshot.contains(*id))
                .map(|(&id, &price)| (id, price))
                .collect();

            for (id, price) in &filled_buys {
                state.live_order_ids.remove(id);
                state.pending_buy_orders.remove(id);
                state.algorithm.on_fill(*price, true);
                crate::logger::log(&src, &format!("Order {} absent from snapshot — assumed filled @ {:.2}.", id, price));
            }
            for (id, price) in &filled_sells {
                state.live_order_ids.remove(id);
                state.pending_sell_orders.remove(id);
                state.algorithm.on_fill(*price, false);
                crate::logger::log(&src, &format!("Order {} absent from snapshot — assumed filled @ {:.2}.", id, price));
            }

            let stale: Vec<i64> = state.live_order_ids.iter().copied()
                .filter(|id| !snapshot.contains(id))
                .collect();
            for id in &stale {
                state.live_order_ids.remove(id);
            }
            if !stale.is_empty() {
                crate::logger::log(
                    &src,
                    &format!("Auth WS snapshot: pruned {} stale live order id(s).", stale.len()),
                );
            }
        }
        WsEvent::OrderFilled { order_id } => {
            state.live_order_ids.remove(&order_id);
            if let Some(price) = state.pending_buy_orders.remove(&order_id) {
                state.algorithm.on_fill(price, true);
                crate::logger::log(
                    &src,
                    &format!("Buy order {} filled @ {:.2} — opposite side seeded.", order_id, price),
                );
            } else if let Some(price) = state.pending_sell_orders.remove(&order_id) {
                state.algorithm.on_fill(price, false);
                crate::logger::log(
                    &src,
                    &format!("Sell order {} filled @ {:.2} — opposite side seeded.", order_id, price),
                );
            } else {
                crate::logger::log(
                    &src,
                    &format!("Order {} filled — removed from live tracking.", order_id),
                );
            }
        }
        WsEvent::OrderCancelled { order_id } => {
            state.live_order_ids.remove(&order_id);
            state.pending_buy_orders.remove(&order_id);
            state.pending_sell_orders.remove(&order_id);
            crate::logger::log(&src, &format!("Order {} cancelled — removed from live tracking.", order_id));
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
            crate::logger::log(
                &src,
                &format!("Wallet snapshot: {} balance(s) loaded.", state.wallet_balances.len()),
            );
        }
        WsEvent::WalletUpdate { wallet_type, currency, available } => {
            if wallet_type == "exchange" {
                let (base, quote) = extract_currencies(&state.symbol);
                if base.is_empty() || quote.is_empty() || currency == base || currency == quote {
                    state.wallet_balances.insert(currency.clone(), available);
                    crate::logger::log(
                        &src,
                        &format!("Wallet update: {} available = {:.8}", currency, available),
                    );
                }
            }
        }
        WsEvent::Heartbeat => {}
        _ => {}
    }
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
    crate::logger::log(
        "[RUNNER]",
        &format!(
            "Warning: could not parse currencies from symbol '{}' — balance checks will be skipped.",
            symbol
        ),
    );
    (String::new(), String::new())
}
