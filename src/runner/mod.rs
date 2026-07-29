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
use crate::config::channels::{RunnerControl, RunnerMode, TuiEvent};
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
        RunnerMode::Paper => "paper",
        RunnerMode::Live => "live",
    }
}

/// Whether the grid's spacing is ATR-driven. An explicit `spacing` option no
/// longer disables this (plan/09): it seeds the initial value and skips only
/// the initial fetch, while the periodic refresh keeps spacing
/// volatility-adaptive instead of pinning it for the runner's whole life.
fn should_fetch_atr(algo_name: &str, options: &HashMap<String, String>) -> bool {
    algo_name == "grid"
        && (options.contains_key("atr_period")
            || options.contains_key("atr_timeframe")
            || options.contains_key("atr_multiplier"))
}

fn should_refresh_trend(algo_name: &str, options: &HashMap<String, String>) -> bool {
    algo_name == "grid"
        && options
            .get("trend_filter")
            .map(|v| !v.eq_ignore_ascii_case("off"))
            .unwrap_or(true)
}

/// EMA of candle closes over `trend_ema_period` candles of `trend_timeframe`
/// (plan/08 step 2) — the trend measure fed to the algorithm via
/// `on_trend_update`, replacing the old per-tick EMA.
async fn fetch_trend_ema(
    engine: &EngineHandle,
    symbol: &str,
    options: &HashMap<String, String>,
) -> Result<f64, String> {
    let timeframe = options
        .get("trend_timeframe")
        .cloned()
        .unwrap_or_else(|| "30m".to_string());
    let period = options
        .get("trend_ema_period")
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|p| *p > 0)
        .unwrap_or(50);
    let mut candles = engine
        .fetch_candles(symbol.to_string(), timeframe, period)
        .await?;
    if candles.is_empty() {
        return Err("no candles returned".to_string());
    }
    candles.sort_by_key(|c| c.timestamp);
    let alpha = 2.0 / (period as f64 + 1.0);
    let mut ema = candles[0].close;
    for c in &candles[1..] {
        ema += alpha * (c.close - ema);
    }
    Ok(ema)
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
                    let mut map: HashMap<String, (f64, f64)> = HashMap::new();
                    for (wallet_type, currency, total, available) in balances {
                        if wallet_type == "exchange" {
                            map.insert(currency, (total, available));
                        }
                    }
                    // Total for base (all held inventory seeds the grid, even
                    // if order-locked); available for quote (what can actually
                    // fund the buy ladder).
                    let base_bal = map.get(&base).map(|&(t, _)| t).unwrap_or(0.0);
                    let quote_bal = map.get(&quote).map(|&(_, a)| a).unwrap_or(0.0);
                    crate::logger::log(
                        &format!("RUNNER:{}", symbol),
                        &format!(
                            "Wallet: {} {:.8} (total), {} {:.8} (available)",
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

#[allow(clippy::too_many_arguments)]
pub async fn run_runner(
    symbol: String,
    algo_name: String,
    mut options: HashMap<String, String>,
    mode: RunnerMode,
    mut control_rx: Receiver<RunnerControl>,
    engine: EngineHandle,
    config: Config,
    fresh: bool,
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

    // Saved resume blob, if any. Only honour it when the requested algorithm and
    // mode match what was persisted and the user did not pass `--fresh`.
    let resume_row = match db.load_runner_state(&symbol) {
        Ok(r) => r,
        Err(e) => {
            crate::logger::log(&src, &format!("Could not load saved state: {}", e));
            None
        }
    };
    let resume = !fresh
        && resume_row
            .as_ref()
            .map(|r| r.algorithm == algo_name && r.mode == mode_label(&mode))
            .unwrap_or(false);

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

    let needs_atr = should_fetch_atr(&algo_name, &options);
    // An explicit spacing seeds the initial value: skip the initial ATR fetch
    // but keep the periodic refresh (plan/09 — the live session's spacing was
    // otherwise pinned for 22 days across changing volatility).
    let spacing_seeded = options.contains_key("spacing");
    let needs_trend = should_refresh_trend(&algo_name, &options);

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

        if needs_atr && !spacing_seeded {
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
        } else {
            // No ATR fetch to implicitly validate the symbol, so probe it
            // explicitly (plan/06 step 5): refuse to start an unknown symbol with a
            // clear one-line error instead of letting it run and spew
            // `symbol: invalid` / HTTP 500 on every reconnect (cf. the `XAUD:USD`
            // typo that ran for days).
            if needs_atr {
                crate::logger::log_info(
                    &src,
                    &format!(
                        "Spacing {} seeded from options → ATR refresh active (every {} min).",
                        options.get("spacing").map(String::as_str).unwrap_or("?"),
                        config.startup_defaults.atr_refresh_mins
                    ),
                );
            }
            match engine.fetch_candles(symbol.clone(), "1m".to_string(), 1).await {
                Ok(candles) if !candles.is_empty() => {}
                Ok(_) => {
                    crate::logger::log(
                        &src,
                        &format!(
                            "Symbol '{}' returned no market data — likely invalid/unsupported. Refusing to start.",
                            symbol
                        ),
                    );
                    engine.unsubscribe(symbol.clone()).await;
                    return;
                }
                Err(e) => {
                    crate::logger::log(
                        &src,
                        &format!("Symbol '{}' validation failed: {} — refusing to start.", symbol, e),
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

    // Fee precedence (plan/10): explicit option > live-fetched account fees >
    // config default. The Jul 7–29 session traded live with the 0.0 scaffold
    // default, so the fee floor and the accounting were fee-blind.
    let explicit_maker = options.get("maker_fee").and_then(|v| v.parse::<f64>().ok());
    let explicit_taker = options.get("taker_fee").and_then(|v| v.parse::<f64>().ok());
    let fetched_fees = if (explicit_maker.is_none() || explicit_taker.is_none())
        && mode != RunnerMode::Simulation
    {
        match engine.fetch_account_fees().await {
            Ok(fees) => Some(fees),
            Err(e) => {
                crate::logger::log_warn(
                    &src,
                    &format!("Account fee fetch failed ({}) — falling back to config defaults.", e),
                );
                None
            }
        }
    } else {
        None
    };
    let maker_fee = explicit_maker
        .or(fetched_fees.map(|f| f.0))
        .unwrap_or(config.startup_defaults.default_maker_fee);
    let taker_fee = explicit_taker
        .or(fetched_fees.map(|f| f.1))
        .unwrap_or(config.startup_defaults.default_taker_fee);
    let fee_source = if explicit_maker.is_some() && explicit_taker.is_some() {
        "option"
    } else if fetched_fees.is_some() {
        "account"
    } else {
        "config default"
    };
    if fee_source == "config default" && mode == RunnerMode::Live && maker_fee == 0.0 {
        crate::logger::log_warn(
            &src,
            "Live runner is fee-blind: account fee fetch unavailable and config default_maker_fee is 0.0 — PnL floors assume free trading.",
        );
    }
    options.insert("maker_fee".to_string(), format!("{}", maker_fee));
    options.insert("taker_fee".to_string(), format!("{}", taker_fee));

    // On by default since plan/09: the halt only stops new buys (lot exits keep
    // resting), so it is a disaster brake, not a routine trip hazard. Pass
    // `max_drawdown_pct=0` to disable explicitly.
    let max_drawdown_pct = match options.get("max_drawdown_pct") {
        Some(v) => v.parse::<f64>().ok().filter(|x| *x > 0.0),
        None => Some(0.20),
    };

    // Sanity-check the capital budget against the quote actually free in the wallet
    // (plan/06 step 1). The wallet is shared across runners, so this is a warning,
    // not a hard refusal — other runners may legitimately hold the remainder.
    if let Some(capital) = options.get("capital").and_then(|v| v.parse::<f64>().ok()) {
        let free_quote = options
            .get("initial_quote_balance")
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0);
        if capital > free_quote {
            crate::logger::log_warn(
                &src,
                &format!(
                    "capital={:.2} exceeds currently-free quote {:.2}. If other runners hold the rest this is fine; otherwise the buy ladder may be under-funded.",
                    capital, free_quote
                ),
            );
        }
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

    // One shared refresh interval drives both the ATR-derived spacing update and
    // the candle-close trend EMA update (plan/08 step 2).
    let refresh_secs = config.startup_defaults.atr_refresh_mins * 60;
    let mut refresh_interval: Option<tokio::time::Interval> = if needs_atr || needs_trend {
        let mut iv = tokio::time::interval(Duration::from_secs(refresh_secs));
        iv.tick().await;
        Some(iv)
    } else {
        None
    };

    // Periodic high-frequency snapshots feed analytics/the future dashboard.
    // Skipped in Simulation, which models perfect instant fills and is not a
    // real time-series worth persisting.
    let snapshot_secs = config.startup_defaults.snapshot_interval_secs.max(1);
    let mut snapshot_interval: Option<tokio::time::Interval> = if mode != RunnerMode::Simulation {
        let mut iv = tokio::time::interval(Duration::from_secs(snapshot_secs));
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
        maker_fee,
        taker_fee,
        max_drawdown_pct,
        peak_equity: 0.0,
        halted: false,
        daily: None,
    };

    if resume {
        if let Some(row) = &resume_row {
            if let Some(algo_state) = &row.algo_state {
                state.algorithm.restore_state(algo_state);
            }
            if let Ok(buys) =
                serde_json::from_str::<HashMap<i64, (f64, f64)>>(&row.pending_buys)
            {
                state.pending_buy_orders = buys;
            }
            if let Ok(sells) =
                serde_json::from_str::<HashMap<i64, (f64, f64)>>(&row.pending_sells)
            {
                state.pending_sell_orders = sells;
            }
            state.live_order_ids = state
                .pending_buy_orders
                .keys()
                .chain(state.pending_sell_orders.keys())
                .copied()
                .collect();
            crate::logger::log_info(
                &src,
                &format!(
                    "Resumed from saved state ({} buy / {} sell pending order(s) restored) — will reconcile against the exchange on auth connect.",
                    state.pending_buy_orders.len(),
                    state.pending_sell_orders.len()
                ),
            );
        }
    } else if fresh && resume_row.is_some() {
        crate::logger::log_info(&src, "--fresh specified — ignoring saved resume state.");
    } else if let Some(row) = &resume_row {
        crate::logger::log_info(
            &src,
            &format!(
                "Saved state exists but does not match this spawn (saved {}/{} vs requested {}/{}) — starting fresh.",
                row.algorithm,
                row.mode,
                algo_name,
                mode_label(&state.mode)
            ),
        );
    }

    crate::logger::log(
        &src,
        &format!(
            "Runner started, algorithm: {}, mode: {}",
            algo_name,
            mode_label(&state.mode)
        ),
    );
    crate::logger::log_info(
        &src,
        &format!(
            "Fees: maker {:.6}, taker {:.6} ({})",
            state.maker_fee, state.taker_fee, fee_source
        ),
    );
    match state.max_drawdown_pct {
        Some(pct) => crate::logger::log_info(
            &src,
            &format!("Risk: max drawdown halt at {:.2}% below peak equity.", pct * 100.0),
        ),
        None => crate::logger::log_info(&src, "Risk: max drawdown halt disabled."),
    }

    // Seed the trend filter at spawn (like the initial ATR fetch) so it isn't
    // blind until the first refresh. On failure extensions stay allowed
    // (warming-up behaviour) until a refresh succeeds.
    if needs_trend && state.mode != RunnerMode::Simulation {
        match fetch_trend_ema(&engine, &symbol, &options).await {
            Ok(ema) => {
                crate::logger::log_info(&src, &format!("Trend EMA initialised: {:.8}", ema));
                state.algorithm.on_trend_update(ema);
            }
            Err(e) => crate::logger::log_warn(
                &src,
                &format!("Initial trend EMA fetch failed ({}) — extensions allowed until the first refresh.", e),
            ),
        }
    }

    loop {
        tokio::select! {
            event = event_rx.recv() => {
                match event {
                    None | Some(EngineEvent::EngineShutdown) => {
                        crate::logger::log(&src, "Engine shut down — saving state for resume, leaving orders resting.");
                        state.save_state();
                        engine.unsubscribe(symbol.clone()).await;
                        crate::logger::notify_tui(TuiEvent::RunnerStopped { symbol: symbol.clone() });
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
                        // An order tracked as pending but absent from the snapshot is
                        // either filled OR cancelled out-of-band. Exchange order state
                        // is authoritative (plan/06 step 3): never book a fill from mere
                        // absence — resolve each missing id against
                        // `fetch_order_history` (filled vs cancelled) before booking.
                        // The fast path (WS `OrderFilled` → `process_fill`) still books
                        // confirmed fills immediately.
                        let snapshot: HashSet<i64> = order_ids.into_iter().collect();
                        reconcile::resolve_snapshot_absences(&src, &mut state, &engine, &snapshot).await;
                    }

                    Some(EngineEvent::WalletSnapshot { balances }) => {
                        process_wallet_snapshot(&mut state, balances);
                    }

                    Some(EngineEvent::WalletUpdate { wallet_type, currency, total, available }) => {
                        process_wallet_update(&mut state, wallet_type, currency, total, available);
                    }

                    Some(EngineEvent::PublicWsReconnected) => {
                        state.algorithm.on_reconnect();
                    }

                    Some(EngineEvent::AuthConnected) => {
                        crate::logger::log(&src, "Auth WS connected.");
                        if state.mode != RunnerMode::Simulation {
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
                match refresh_interval.as_mut() {
                    Some(iv) => { iv.tick().await; }
                    None => std::future::pending::<()>().await,
                }
            } => {
                if needs_atr {
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
                if needs_trend {
                    match fetch_trend_ema(&engine, &state.symbol, &state.options).await {
                        Ok(ema) => {
                            crate::logger::log(&src, &format!("Trend refresh: candle EMA {:.8}", ema));
                            state.algorithm.on_trend_update(ema);
                        }
                        Err(e) => crate::logger::log(&src, &format!("Trend refresh: candle fetch failed: {}", e)),
                    }
                }
            }

            _ = async {
                match snapshot_interval.as_mut() {
                    Some(iv) => { iv.tick().await; }
                    None => std::future::pending::<()>().await,
                }
            } => {
                state.persist_periodic();
                state.save_state();
            }

            ctrl = control_rx.recv() => {
                match ctrl {
                    None => {
                        crate::logger::log(&src, "Control channel closed — saving state, leaving orders resting.");
                        state.save_state();
                        engine.unsubscribe(symbol.clone()).await;
                        crate::logger::notify_tui(TuiEvent::RunnerStopped { symbol: symbol.clone() });
                        break;
                    }

                    Some(RunnerControl::Shutdown) => {
                        crate::logger::log(&src, "Shutdown — saving state for resume, leaving orders resting.");
                        state.save_state();
                        engine.unsubscribe(symbol.clone()).await;
                        crate::logger::notify_tui(TuiEvent::RunnerStopped { symbol: symbol.clone() });
                        break;
                    }

                    Some(RunnerControl::Kill) => {
                        crate::logger::log(&src, "Kill received — cancelling orders and clearing resume state.");
                        cancel_all_live_orders(&mut state, &engine).await;
                        state.clear_state();
                        engine.unsubscribe(symbol.clone()).await;
                        crate::logger::notify_tui(TuiEvent::RunnerStopped { symbol: symbol.clone() });
                        break;
                    }

                    Some(RunnerControl::Pause) => {
                        state.paused = true;
                        crate::logger::log(&src, "Runner paused.");
                    }

                    Some(RunnerControl::Resume) => {
                        state.paused = false;
                        if state.halted {
                            state.halted = false;
                            state.peak_equity = 0.0;
                            crate::logger::log(&src, "Runner resumed — drawdown halt cleared, peak equity reset.");
                        } else {
                            crate::logger::log(&src, "Runner resumed.");
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

    if check_risk(state, engine).await {
        return;
    }

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

/// Cancel resting buy orders only, leaving sells (lot exits) on the exchange —
/// the drawdown-halt semantics since plan/09: exits only reduce risk, so they
/// keep resting through a halt.
async fn cancel_live_buy_orders(state: &mut RunnerState, engine: &EngineHandle) {
    if state.mode == RunnerMode::Simulation || state.pending_buy_orders.is_empty() {
        return;
    }
    let src = format!("RUNNER:{}", state.symbol);
    let ids: Vec<i64> = state.pending_buy_orders.keys().copied().collect();
    crate::logger::log(&src, &format!("Cancelling {} open buy order(s)…", ids.len()));
    for order_id in ids {
        match engine.cancel_order(order_id).await {
            Ok(()) => {
                state.live_order_ids.remove(&order_id);
                state.pending_buy_orders.remove(&order_id);
                crate::logger::log(&src, &format!("Cancelled buy order {}.", order_id));
            }
            Err(e) => crate::logger::log(
                &src,
                &format!("Failed to cancel buy order {}: {}", order_id, e),
            ),
        }
    }
}

/// Algorithm-independent drawdown guard. Computes current equity
/// (quote balance + position marked at mid) each tick, tracks the peak, and if
/// the relative drawdown exceeds `max_drawdown_pct` halts the runner: cancels
/// resting buys (lot exits keep resting — they only reduce risk) and stops
/// dispatching new signals until a manual resume.
/// Returns `true` while the runner is halted.
async fn check_risk(state: &mut RunnerState, engine: &EngineHandle) -> bool {
    if state.halted {
        return true;
    }
    let Some(max_dd) = state.max_drawdown_pct else {
        return false;
    };
    let mid = if state.last_bid > 0.0 && state.last_ask > 0.0 {
        (state.last_bid + state.last_ask) / 2.0
    } else {
        return false;
    };

    // Wallet-truth equity (plan/10): total balances including order-locked
    // funds, so the halt tracks the number the user could actually withdraw.
    let equity = state.wallet_equity(mid);

    if equity > state.peak_equity {
        state.peak_equity = equity;
    }

    if state.peak_equity > 0.0 {
        let drawdown = (state.peak_equity - equity) / state.peak_equity;
        if drawdown > max_dd {
            state.halted = true;
            let src = format!("RUNNER:{}", state.symbol);
            crate::logger::log_critical(
                &src,
                &format!(
                    "MAX DRAWDOWN BREACHED: equity {:.2} is {:.2}% below peak {:.2} (limit {:.2}%). Halting new buys — cancelling resting buys, lot exits stay on the exchange. Send resume to continue.",
                    equity,
                    drawdown * 100.0,
                    state.peak_equity,
                    max_dd * 100.0
                ),
            );
            cancel_live_buy_orders(state, engine).await;
            return true;
        }
    }
    false
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
        let before = state.algorithm.realized_pnl();
        let sigs = state.algorithm.on_fill(price, true, current_price);
        let realized = state.algorithm.realized_pnl() - before;
        crate::logger::log(
            &src,
            &format!("Buy order {} filled @ {:.2}.", order_id, price),
        );
        state.write_fill_to_db(Some(order_id), true, price, qty, Some(realized));
        state.save_state();
        sigs
    } else if let Some((price, qty)) = state.pending_sell_orders.remove(&order_id) {
        state.live_order_ids.remove(&order_id);
        let before = state.algorithm.realized_pnl();
        let sigs = state.algorithm.on_fill(price, false, current_price);
        let realized = state.algorithm.realized_pnl() - before;
        crate::logger::log(
            &src,
            &format!("Sell order {} filled @ {:.2}.", order_id, price),
        );
        state.write_fill_to_db(Some(order_id), false, price, qty, Some(realized));
        state.save_state();
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

fn process_wallet_snapshot(state: &mut RunnerState, balances: Vec<(String, String, f64, f64)>) {
    let src = format!("RUNNER:{}", state.symbol);
    let (base, quote) = extract_currencies(&state.symbol);
    state.wallet_balances.clear();
    for (wallet_type, currency, total, available) in balances {
        if wallet_type == "exchange"
            && (base.is_empty() || quote.is_empty() || currency == base || currency == quote)
        {
            state.wallet_balances.insert(currency, (total, available));
        }
    }
    crate::logger::log(
        &src,
        &format!(
            "Wallet snapshot: {} balance(s) loaded.",
            state.wallet_balances.len()
        ),
    );
    let base_bal = state.wallet_balances.get(&base).map(|&(_, a)| a).unwrap_or(0.0);
    let quote_bal = state.wallet_balances.get(&quote).map(|&(_, a)| a).unwrap_or(0.0);
    state.algorithm.on_balance_update(base_bal, quote_bal);
}

fn process_wallet_update(
    state: &mut RunnerState,
    wallet_type: String,
    currency: String,
    total: f64,
    available: f64,
) {
    if wallet_type == "exchange" {
        let src = format!("RUNNER:{}", state.symbol);
        let (base, quote) = extract_currencies(&state.symbol);
        if base.is_empty() || quote.is_empty() || currency == base || currency == quote {
            state
                .wallet_balances
                .insert(currency.clone(), (total, available));
            crate::logger::log(
                &src,
                &format!(
                    "Wallet update: {} total = {:.8}, available = {:.8}",
                    currency, total, available
                ),
            );
            let base_bal = state.wallet_balances.get(&base).map(|&(_, a)| a).unwrap_or(0.0);
            let quote_bal = state.wallet_balances.get(&quote).map(|&(_, a)| a).unwrap_or(0.0);
            state.algorithm.on_balance_update(base_bal, quote_bal);
        }
    }
}
