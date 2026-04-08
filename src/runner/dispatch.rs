use std::time::{Duration, Instant};
use tokio::time::sleep;
use crate::api::TradeSignal;
use crate::config::channels::RunnerMode;
use crate::engine::EngineHandle;
use crate::util::extract_currencies;
use crate::runner::state::RunnerState;
use super::mode_label;

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

pub async fn dispatch_signals(state: &mut RunnerState, signals: &[TradeSignal], engine: &EngineHandle) {
    for sig in signals {
        let src = format!("RUNNER:{}", state.symbol);
        match sig {
            TradeSignal::Buy { price, quantity, reason, .. } => {
                if state.mode == RunnerMode::Simulation {
                    crate::logger::log(&src, &format!("[SIM] LIMIT BUY {:.8} @ {:.2} — {}", quantity, price, reason));
                    let _ = state.algorithm.on_fill(*price, true);
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
                    match engine.place_order(sig.clone(), state.symbol.clone()).await {
                        Ok(result) => {
                            crate::logger::log(&src, &format!("[{}] BUY placed — id={} status={}", mode_label(&state.mode), result.order_id, result.status));
                            if result.order_id != 0 {
                                state.live_order_ids.insert(result.order_id);
                                state.pending_buy_orders.insert(result.order_id, (*price, *quantity));
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
                    match engine.place_order(sig.clone(), state.symbol.clone()).await {
                        Ok(result) => {
                            crate::logger::log(&src, &format!("[{}] SELL placed — id={} status={}", mode_label(&state.mode), result.order_id, result.status));
                            if result.order_id != 0 {
                                state.live_order_ids.insert(result.order_id);
                                state.pending_sell_orders.insert(result.order_id, (*price, *quantity));
                            }
                        }
                        Err(e) => crate::logger::log(&src, &format!("[{}] SELL FAILED: {}", mode_label(&state.mode), e)),
                    }
                }
            }
        }
    }
}
