use std::collections::{HashMap, HashSet};
use crate::engine::EngineHandle;
use crate::runner::state::RunnerState;
use crate::runner::dispatch;

/// How to treat a tracked order that is absent from the current open-order set,
/// resolved against its status in the exchange order history.
#[derive(Debug, PartialEq, Eq)]
pub enum AbsenceOutcome {
    /// History confirms the order executed — book the fill exactly once.
    Fill,
    /// History shows a non-executed terminal status (e.g. cancelled) — drop the
    /// tracked order without booking any fill or position change.
    Drop,
    /// Not (yet) present in history — ambiguous; leave it pending and re-check
    /// later rather than guess.
    Leave,
}

/// Classify an absent order from its order-history status string. `EXECUTED*` is
/// the only status that books a fill; everything else is a cancel/terminal, and a
/// missing entry is left pending. This is the authoritative rule behind plan/06
/// step 3 — fills come from exchange state, never from snapshot absence alone.
pub fn classify_absence(status: Option<&str>) -> AbsenceOutcome {
    match status {
        Some(s) if s.starts_with("EXECUTED") => AbsenceOutcome::Fill,
        Some(_) => AbsenceOutcome::Drop,
        None => AbsenceOutcome::Leave,
    }
}

/// Resolve pending orders that are absent from an auth-WS order snapshot.
///
/// Absence alone is ambiguous — the order may have filled or been cancelled
/// out-of-band — so this never books a fill from absence. It fetches the order
/// history once (only when there are absences) and classifies each missing id:
/// `EXECUTED*` → book the fill; any other terminal status → drop without booking;
/// not yet in history → leave pending for a later snapshot/reconnect. This is the
/// root fix for the phantom-fill / phantom-short bug (plan/06 step 3).
pub async fn resolve_snapshot_absences(
    src: &str,
    state: &mut RunnerState,
    engine: &EngineHandle,
    snapshot: &HashSet<i64>,
) {
    let absent: Vec<(i64, f64, f64, bool)> = state
        .pending_buy_orders
        .iter()
        .filter(|(id, _)| !snapshot.contains(*id))
        .map(|(&id, &(p, q))| (id, p, q, true))
        .chain(
            state
                .pending_sell_orders
                .iter()
                .filter(|(id, _)| !snapshot.contains(*id))
                .map(|(&id, &(p, q))| (id, p, q, false)),
        )
        .collect();

    if !absent.is_empty() {
        let history: HashMap<i64, String> = match engine
            .fetch_order_history(state.symbol.clone())
            .await
        {
            Ok(pairs) => pairs.into_iter().collect(),
            Err(e) => {
                crate::logger::log(
                    src,
                    &format!(
                        "Snapshot reconcile: {} order(s) absent but order-history fetch failed ({}) — leaving them pending, not booking fills.",
                        absent.len(),
                        e
                    ),
                );
                return;
            }
        };

        let current_price = if state.last_bid > 0.0 && state.last_ask > 0.0 {
            (state.last_bid + state.last_ask) / 2.0
        } else {
            0.0
        };

        let mut fill_signals = Vec::new();
        let mut changed = false;
        for (order_id, price, qty, is_buy) in absent {
            match classify_absence(history.get(&order_id).map(|s| s.as_str())) {
                AbsenceOutcome::Fill => {
                    if is_buy {
                        state.pending_buy_orders.remove(&order_id);
                    } else {
                        state.pending_sell_orders.remove(&order_id);
                    }
                    state.live_order_ids.remove(&order_id);
                    let before = state.algorithm.realized_pnl();
                    fill_signals.extend(state.algorithm.on_fill(price, is_buy, current_price));
                    let realized = state.algorithm.realized_pnl() - before;
                    crate::logger::log(
                        src,
                        &format!("Snapshot reconcile: order {} confirmed filled @ {:.2}.", order_id, price),
                    );
                    state.write_fill_to_db(Some(order_id), is_buy, price, qty, Some(realized));
                    changed = true;
                }
                AbsenceOutcome::Drop => {
                    if is_buy {
                        state.pending_buy_orders.remove(&order_id);
                    } else {
                        state.pending_sell_orders.remove(&order_id);
                    }
                    state.live_order_ids.remove(&order_id);
                    crate::logger::log(
                        src,
                        &format!("Snapshot reconcile: order {} cancelled out-of-band — dropped, no fill booked.", order_id),
                    );
                    changed = true;
                }
                AbsenceOutcome::Leave => {
                    crate::logger::log(
                        src,
                        &format!("Snapshot reconcile: order {} absent but not yet in history — leaving pending.", order_id),
                    );
                }
            }
        }

        if changed {
            state.save_state();
        }
        if !fill_signals.is_empty() {
            dispatch::dispatch_signals(state, &fill_signals, engine).await;
        }
        if changed {
            replace_missing_exits(src, state, engine).await;
        }
    }

    // Prune any tracked live ids no longer open and not pending (terminal, already
    // booked or never tracked as pending).
    let stale: Vec<i64> = state
        .live_order_ids
        .iter()
        .copied()
        .filter(|id| {
            !snapshot.contains(id)
                && !state.pending_buy_orders.contains_key(id)
                && !state.pending_sell_orders.contains_key(id)
        })
        .collect();
    for id in &stale {
        state.live_order_ids.remove(id);
    }
    if !stale.is_empty() {
        crate::logger::log(src, &format!("Auth WS snapshot: pruned {} stale order id(s).", stale.len()));
    }
}

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
        match classify_absence(history.get(&order_id).map(|s| s.as_str())) {
            AbsenceOutcome::Fill => {
                if is_buy {
                    state.pending_buy_orders.remove(&order_id);
                } else {
                    state.pending_sell_orders.remove(&order_id);
                }
                state.live_order_ids.remove(&order_id);
                let before = state.algorithm.realized_pnl();
                let fill_signals = state.algorithm.on_fill(price, is_buy, 0.0);
                let realized = state.algorithm.realized_pnl() - before;
                crate::logger::log(src, &format!("Reconnect sync: order {} filled @ {:.2}.", order_id, price));
                state.write_fill_to_db(Some(order_id), is_buy, price, qty, Some(realized));
                if !fill_signals.is_empty() {
                    dispatch::dispatch_signals(state, &fill_signals, engine).await;
                }
            }
            AbsenceOutcome::Drop => {
                if is_buy {
                    state.pending_buy_orders.remove(&order_id);
                } else {
                    state.pending_sell_orders.remove(&order_id);
                }
                state.live_order_ids.remove(&order_id);
                crate::logger::log(src, &format!("Reconnect sync: order {} cancelled.", order_id));
            }
            AbsenceOutcome::Leave => {
                crate::logger::log(src, &format!("Reconnect sync: order {} not in history — leaving as pending.", order_id));
            }
        }
    }

    state.save_state();
    replace_missing_exits(src, state, engine).await;
}

/// Re-dispatch the strategy's expected lot exits (plan/09). The dispatcher
/// skips exits still pending on the exchange, so only exits lost to an
/// out-of-band cancel or a missed placement are actually re-placed — this
/// keeps the "every lot's exit is always resting" invariant across
/// disconnects and snapshot reconciles.
async fn replace_missing_exits(src: &str, state: &mut RunnerState, engine: &EngineHandle) {
    let exits = state.algorithm.expected_exits();
    if exits.is_empty() {
        return;
    }
    let missing: Vec<_> = exits
        .into_iter()
        .filter(|sig| match sig {
            crate::api::TradeSignal::Sell { price, .. } => !state
                .pending_sell_orders
                .values()
                .any(|(p, _)| (p - price).abs() < 1e-6),
            _ => false,
        })
        .collect();
    if missing.is_empty() {
        return;
    }
    crate::logger::log(
        src,
        &format!("Reconcile: {} lot exit(s) not resting — re-placing.", missing.len()),
    );
    dispatch::dispatch_signals(state, &missing, engine).await;
}

#[cfg(test)]
mod tests {
    use super::{classify_absence, AbsenceOutcome};

    #[test]
    fn executed_status_books_a_fill() {
        assert_eq!(classify_absence(Some("EXECUTED @ 100.0(1.0)")), AbsenceOutcome::Fill);
        assert_eq!(classify_absence(Some("EXECUTED")), AbsenceOutcome::Fill);
    }

    #[test]
    fn cancelled_status_drops_without_fill() {
        assert_eq!(classify_absence(Some("CANCELED")), AbsenceOutcome::Drop);
        assert_eq!(classify_absence(Some("POSTONLY CANCELED")), AbsenceOutcome::Drop);
    }

    #[test]
    fn missing_from_history_is_left_pending() {
        assert_eq!(classify_absence(None), AbsenceOutcome::Leave);
    }
}
