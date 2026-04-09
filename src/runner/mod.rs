pub mod dispatch;
pub mod reconcile;
pub mod report;
pub mod state;
pub mod trade_log;

use chrono::Utc;
use std::collections::{HashMap, HashSet};
use tokio::sync::mpsc::{self, Receiver};
use tokio::time::Duration;

use crate::algorithm::build_algorithm;
use crate::api::MarketData;
use crate::config::channels::{RunnerControl, RunnerMode};
use crate::config::config::Config;
use crate::engine::{EngineHandle, channels::EngineEvent};
use crate::storage::db::Db;
use crate::util::extract_currencies;
use dispatch::dispatch_signals;
use state::RunnerState;
use trade_log::{TradeEntry, TradeLog};

pub(crate) fn mode_label(mode: &RunnerMode) -> &'static str {
    match mode {
        RunnerMode::Simulation => "simulation",
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

async fn wait_for_wallet_event(
    event_rx: &mut Receiver<EngineEvent>,
    options: &mut HashMap<String, String>,
    symbol: &str,
) -> Result<(), String> {
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match event_rx.recv().await {
                Some(EngineEvent::WalletSnapshot { balances }) => {
                    let (base, quote) = extract_currencies(symbol);
                    let mut map: HashMap<String, f64> = HashMap::new();
                    for (wallet_type, currency, available) in balances {
                        if wallet_type == "exchange" {
                            map.insert(currency, available);
                        }
                    }
                    let base_bal = map.get(&base).copied().unwrap_or(0.0);
                    let quote_bal = map.get(&quote).copied().unwrap_or(0.0);
                    crate::logger::log(
                        &format!("RUNNER:{}", symbol),
                        &format!(
                            "Wallet: {} {:.8}, {} {:.8}",
                            base, base_bal, quote, quote_bal
                        ),
                    );
                    options.insert(
                        "initial_base_balance".to_string(),
                        format!("{:.8}", base_bal),
                    );
                    options.insert(
                        "initial_quote_balance".to_string(),
                        format!("{:.8}", quote_bal),
                    );
                    return Ok(());
                }
                Some(_) => {}
                None => {
                    return Err("Engine event channel closed before wallet snapshot".to_string());
                }
            }
        }
    })
    .await;
    result.map_err(|_| "Timed out waiting for wallet snapshot (30s)".to_string())?
}

pub(crate) async fn cancel_all_live_orders(state: &mut RunnerState, engine: &EngineHandle) {
    if state.mode == RunnerMode::Simulation || state.live_order_ids.is_empty() {
        return;
    }
    let src = format!("RUNNER:{}", state.symbol);
    crate::logger::log(
        &src,
        &format!("Cancelling {} open order(s)…", state.live_order_ids.len()),
    );
    let ids: Vec<i64> = state.live_order_ids.iter().copied().collect();
    for order_id in ids {
        match engine.cancel_order(order_id).await {
            Ok(()) => crate::logger::log(&src, &format!("Cancelled order {}.", order_id)),
            Err(e) => {
                crate::logger::log(&src, &format!("Failed to cancel order {}: {}", order_id, e))
            }
        }
    }
    state.live_order_ids.clear();
    state.pending_buy_orders.clear();
    state.pending_sell_orders.clear();
}

pub async fn run_runner(
    symbol: String,
    algo_name: String,
    mut options: HashMap<String, String>,
    mode: RunnerMode,
    mut control_rx: Receiver<RunnerControl>,
    engine: EngineHandle,
    config: Config,
) {
    let src = format!("RUNNER:{}", symbol);

    let db_path = crate::storage::data_dir().join("ted.db");
    let db = match Db::open(&db_path) {
        Ok(db) => db,
        Err(e) => {
            crate::logger::log(&src, &format!("Failed to open DB: {} — runner exiting.", e));
            return;
        }
    };

    let started_at = Utc::now();
    let runner_db_id = match db.insert_runner(
        &symbol,
        &algo_name,
        mode_label(&mode),
        &started_at.to_rfc3339(),
    ) {
        Ok(id) => id,
        Err(e) => {
            crate::logger::log(
                &src,
                &format!("Failed to insert runner row: {} — runner exiting.", e),
            );
            return;
        }
    };

    let (event_tx, mut event_rx) = mpsc::channel::<EngineEvent>(256);
    engine.subscribe(symbol.clone(), event_tx).await;

    if mode != RunnerMode::Simulation {
        match wait_for_wallet_event(&mut event_rx, &mut options, &symbol).await {
            Ok(()) => {}
            Err(e) => {
                crate::logger::log(
                    &src,
                    &format!("Wallet snapshot failed: {} — runner exiting.", e),
                );
                engine.unsubscribe(symbol.clone()).await;
                return;
            }
        }

        if should_fetch_atr(&algo_name, &options) {
            let timeframe = options
                .get("atr_timeframe")
                .cloned()
                .unwrap_or_else(|| "1h".to_string());
            let period = options
                .get("atr_period")
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(14);
            let multiplier = options
                .get("atr_multiplier")
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(0.5);
            match engine
                .fetch_candles(symbol.clone(), timeframe.clone(), period)
                .await
            {
                Ok(candles) => match crate::algorithm::atr::compute_atr(&candles, period) {
                    Ok(atr) => {
                        let spacing = atr * multiplier;
                        crate::logger::log(
                            &src,
                            &format!(
                                "ATR({}) on {} = {:.8}, ×{:.2} → spacing {:.8}",
                                period, timeframe, atr, multiplier, spacing
                            ),
                        );
                        options.insert("spacing".to_string(), format!("{:.8}", spacing));
                    }
                    Err(e) => {
                        crate::logger::log(
                            &src,
                            &format!("ATR computation failed: {} — runner exiting.", e),
                        );
                        engine.unsubscribe(symbol.clone()).await;
                        return;
                    }
                },
                Err(e) => {
                    crate::logger::log(
                        &src,
                        &format!("Candle fetch failed: {} — runner exiting.", e),
                    );
                    engine.unsubscribe(symbol.clone()).await;
                    return;
                }
            }
        }
    } else {
        options
            .entry("initial_base_balance".to_string())
            .or_insert_with(|| "0.0".to_string());
        options
            .entry("initial_quote_balance".to_string())
            .or_insert_with(|| "1000000.0".to_string());
    }

    let algorithm = match build_algorithm(&algo_name, &options) {
        Ok(a) => a,
        Err(e) => {
            crate::logger::log(
                &src,
                &format!(
                    "Failed to build algorithm '{}': {} — runner exiting.",
                    algo_name, e
                ),
            );
            engine.unsubscribe(symbol.clone()).await;
            return;
        }
    };

    let (base_check, quote_check) = extract_currencies(&symbol);
    if base_check.is_empty() || quote_check.is_empty() {
        crate::logger::log(
            &src,
            "Warning: symbol format unrecognised — wallet balance checks will be unavailable.",
        );
    }

    let atr_refresh_secs = config.startup_defaults.atr_refresh_mins * 60;
    let mut atr_refresh_interval: Option<tokio::time::Interval> =
        if should_fetch_atr(&algo_name, &options) {
            let mut iv = tokio::time::interval(Duration::from_secs(atr_refresh_secs));
            iv.tick().await;
            Some(iv)
        } else {
            None
        };

    let mut state = RunnerState {
        symbol: symbol.clone(),
        algorithm,
        algo_name: algo_name.clone(),
        options: options.clone(),
        mode,
        paused: false,
        trade_log: TradeLog::new(),
        started_at,
        config: config.clone(),
        live_order_ids: HashSet::new(),
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
        runner_db_id: Some(runner_db_id),
        db: Some(db),
        last_bid: 0.0,
        last_ask: 0.0,
    };

    crate::logger::log(
        &src,
        &format!(
            "Runner started, algorithm: {}, mode: {}",
            algo_name,
            mode_label(&state.mode)
        ),
    );

    loop {
        tokio::select! {
            event = event_rx.recv() => {
                match event {
                    None | Some(EngineEvent::EngineShutdown) => {
                        crate::logger::log(&src, "Engine shut down — runner exiting.");
                        cancel_all_live_orders(&mut state, &engine).await;
                        engine.unsubscribe(symbol.clone()).await;
                        break;
                    }

                    Some(EngineEvent::Tick(md)) => {
                        process_tick(&mut state, &engine, md).await;
                    }

                    Some(EngineEvent::OrderFilled { order_id }) => {
                        process_fill(&mut state, &engine, order_id).await;
                    }

                    Some(EngineEvent::OrderCancelled { order_id }) => {
                        process_cancelled(&mut state, order_id);
                    }

                    Some(EngineEvent::OrderNew) => {}

                    Some(EngineEvent::OrderSnapshot { order_ids }) => {
                        let snapshot: HashSet<i64> = order_ids.into_iter().collect();
                        let filled_buys: Vec<(i64, f64, f64)> = state.pending_buy_orders
                            .iter()
                            .filter(|(id, _)| !snapshot.contains(*id))
                            .map(|(&id, &(p, q))| (id, p, q))
                            .collect();
                        let filled_sells: Vec<(i64, f64, f64)> = state.pending_sell_orders
                            .iter()
                            .filter(|(id, _)| !snapshot.contains(*id))
                            .map(|(&id, &(p, q))| (id, p, q))
                            .collect();
                        let current_price = if state.last_bid > 0.0 && state.last_ask > 0.0 {
                            (state.last_bid + state.last_ask) / 2.0
                        } else {
                            0.0
                        };
                        let mut fill_signals = vec![];
                        for (id, price, qty) in &filled_buys {
                            state.live_order_ids.remove(id);
                            state.pending_buy_orders.remove(id);
                            fill_signals.extend(state.algorithm.on_fill(*price, true, current_price));
                            crate::logger::log(&src, &format!("Order {} absent from snapshot — assumed filled @ {:.2}.", id, price));
                            state.write_fill_to_db(Some(*id), true, *price, *qty);
                        }
                        for (id, price, qty) in &filled_sells {
                            state.live_order_ids.remove(id);
                            state.pending_sell_orders.remove(id);
                            fill_signals.extend(state.algorithm.on_fill(*price, false, current_price));
                            crate::logger::log(&src, &format!("Order {} absent from snapshot — assumed filled @ {:.2}.", id, price));
                            state.write_fill_to_db(Some(*id), false, *price, *qty);
                        }
                        let stale: Vec<i64> = state.live_order_ids.iter().copied().filter(|id| !snapshot.contains(id)).collect();
                        for id in &stale { state.live_order_ids.remove(id); }
                        if !stale.is_empty() {
                            crate::logger::log(&src, &format!("Auth WS snapshot: pruned {} stale order id(s).", stale.len()));
                        }
                        if !fill_signals.is_empty() {
                            dispatch_signals(&mut state, &fill_signals, &engine).await;
                        }
                    }

                    Some(EngineEvent::WalletSnapshot { balances }) => {
                        process_wallet_snapshot(&mut state, balances);
                    }

                    Some(EngineEvent::WalletUpdate { wallet_type, currency, available }) => {
                        process_wallet_update(&mut state, wallet_type, currency, available);
                    }

                    Some(EngineEvent::PublicWsReconnected) => {
                        state.algorithm.on_reconnect();
                    }

                    Some(EngineEvent::AuthConnected) => {
                        crate::logger::log(&src, "Auth WS connected.");
                        if state.mode == RunnerMode::Live {
                            reconcile::sync_orders_after_reconnect(&src, &state.symbol.clone(), &mut state, &engine).await;
                        }
                    }

                    Some(EngineEvent::AuthFailed { code, message }) => {
                        crate::logger::log(&src, &format!("Auth WS authentication failed ({}: {}).", code, message));
                    }

                    Some(EngineEvent::Maintenance) => {
                        crate::logger::log(&src, "Bitfinex platform entered maintenance mode.");
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
                match engine.fetch_candles(state.symbol.clone(), timeframe.clone(), period).await {
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

            ctrl = control_rx.recv() => {
                match ctrl {
                    None => {
                        crate::logger::log(&src, "Control channel closed — runner exiting.");
                        cancel_all_live_orders(&mut state, &engine).await;
                        engine.unsubscribe(symbol.clone()).await;
                        break;
                    }

                    Some(RunnerControl::Kill) => {
                        crate::logger::log(&src, "Kill received — stopping runner.");
                        cancel_all_live_orders(&mut state, &engine).await;
                        engine.unsubscribe(symbol.clone()).await;
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
                    }
                }
            }
        }
    }
}

async fn process_tick(state: &mut RunnerState, engine: &EngineHandle, market_data: MarketData) {
    if state.paused {
        return;
    }

    state.last_bid = market_data.bid;
    state.last_ask = market_data.ask;

    crate::logger::update_ticker(state.symbol.clone(), market_data.bid);

    let signals = state.algorithm.on_tick(&market_data);

    dispatch_signals(state, &signals, engine).await;

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
        crate::logger::log(
            &format!("RUNNER:{}", state.symbol),
            &format!("Warning: trade store write failed: {}", e),
        );
    }
    state.trade_log.push(entry);
}

async fn process_fill(state: &mut RunnerState, engine: &EngineHandle, order_id: i64) {
    let src = format!("RUNNER:{}", state.symbol);
    let current_price = if state.last_bid > 0.0 && state.last_ask > 0.0 {
        (state.last_bid + state.last_ask) / 2.0
    } else {
        0.0
    };
    let fill_signals = if let Some((price, qty)) = state.pending_buy_orders.remove(&order_id) {
        state.live_order_ids.remove(&order_id);
        let sigs = state.algorithm.on_fill(price, true, current_price);
        crate::logger::log(
            &src,
            &format!("Buy order {} filled @ {:.2}.", order_id, price),
        );
        state.write_fill_to_db(Some(order_id), true, price, qty);
        sigs
    } else if let Some((price, qty)) = state.pending_sell_orders.remove(&order_id) {
        state.live_order_ids.remove(&order_id);
        let sigs = state.algorithm.on_fill(price, false, current_price);
        crate::logger::log(
            &src,
            &format!("Sell order {} filled @ {:.2}.", order_id, price),
        );
        state.write_fill_to_db(Some(order_id), false, price, qty);
        sigs
    } else {
        if state.live_order_ids.remove(&order_id) {
            crate::logger::log(
                &src,
                &format!("Order {} filled — tracked but not in pending maps (internal inconsistency).", order_id),
            );
        }
        vec![]
    };
    if !fill_signals.is_empty() {
        dispatch::dispatch_signals(state, &fill_signals, engine).await;
    }
}

fn process_cancelled(state: &mut RunnerState, order_id: i64) {
    let src = format!("RUNNER:{}", state.symbol);
    let was_tracked = state.live_order_ids.remove(&order_id);
    state.pending_buy_orders.remove(&order_id);
    state.pending_sell_orders.remove(&order_id);
    if was_tracked {
        crate::logger::log(&src, &format!("Order {} cancelled.", order_id));
    }
}

fn process_wallet_snapshot(state: &mut RunnerState, balances: Vec<(String, String, f64)>) {
    let src = format!("RUNNER:{}", state.symbol);
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
        &format!(
            "Wallet snapshot: {} balance(s) loaded.",
            state.wallet_balances.len()
        ),
    );
    let base_bal = state.wallet_balances.get(&base).copied().unwrap_or(0.0);
    let quote_bal = state.wallet_balances.get(&quote).copied().unwrap_or(0.0);
    state.algorithm.on_balance_update(base_bal, quote_bal);
}

fn process_wallet_update(
    state: &mut RunnerState,
    wallet_type: String,
    currency: String,
    available: f64,
) {
    if wallet_type == "exchange" {
        let src = format!("RUNNER:{}", state.symbol);
        let (base, quote) = extract_currencies(&state.symbol);
        if base.is_empty() || quote.is_empty() || currency == base || currency == quote {
            state.wallet_balances.insert(currency.clone(), available);
            crate::logger::log(
                &src,
                &format!("Wallet update: {} available = {:.8}", currency, available),
            );
            let base_bal = state.wallet_balances.get(&base).copied().unwrap_or(0.0);
            let quote_bal = state.wallet_balances.get(&quote).copied().unwrap_or(0.0);
            state.algorithm.on_balance_update(base_bal, quote_bal);
        }
    }
}
