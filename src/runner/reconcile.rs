use std::collections::{HashMap, HashSet};
use crate::engine::EngineHandle;
use crate::runner::state::RunnerState;
use crate::runner::dispatch;

pub async fn sync_orders_after_reconnect(src: &str, symbol: &str, state: &mut RunnerState, engine: &EngineHandle) {
    let open_ids: HashSet<i64> =
        match engine.fetch_open_orders(symbol.to_string()).await {
            Ok(ids) => ids.into_iter().collect(),
            Err(e) => {
                crate::logger::log(src, &format!("Reconnect sync: fetch open orders failed: {}", e));
                return;
            }
        };

    let history: HashMap<i64, String> =
        match engine.fetch_order_history(symbol.to_string()).await {
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
                state.write_fill_to_db(Some(order_id), is_buy, price, qty);
                if !fill_signals.is_empty() {
                    dispatch::dispatch_signals(state, &fill_signals, engine).await;
                }
            }
            Some(_) => {
                if is_buy {
                    state.pending_buy_orders.remove(&order_id);
                } else {
                    state.pending_sell_orders.remove(&order_id);
                }
                state.live_order_ids.remove(&order_id);
                crate::logger::log(src, &format!("Reconnect sync: order {} cancelled.", order_id));
            }
            None => {
                crate::logger::log(src, &format!("Reconnect sync: order {} not in history — leaving as pending.", order_id));
            }
        }
    }
}
