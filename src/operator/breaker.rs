//! Monthly circuit breaker (plan/12). Reads net realized PnL month-to-date from
//! the daily rollups and, on breach, the caller halts new buys account-wide, sets
//! a persistent hold, and emails. Realized-only by design: intra-session equity
//! crashes are already caught by the per-runner drawdown halt; this is the honest,
//! additive monthly figure across a wallet shared between pairs.
//!
//! ponytail: denominator is the configured `month_baseline_capital`, not a live
//! account-equity snapshot (which would double-count a shared wallet). Upgrade to
//! a per-month equity baseline if multi-account netting is ever needed.

use crate::config::config::OperatorConfig;
use crate::storage::db::Db;
use chrono::{DateTime, Utc};

pub struct BreakerOutcome {
    pub tripped: bool,
    pub loss_pct: f64,
    pub net_realized: f64,
    pub baseline: f64,
    #[allow(dead_code)]
    pub already_held: bool,
}

/// Evaluate the breaker. Seeds/rolls the month baseline in the DB as a side
/// effect. `tripped` is true only on a fresh breach (not already held).
pub fn evaluate(db: &Db, op: &OperatorConfig, now: DateTime<Utc>) -> Result<BreakerOutcome, String> {
    let month = now.format("%Y-%m").to_string();
    let month_start = now.format("%Y-%m-01").to_string();

    let st = db.operator_state().map_err(|e| e.to_string())?;

    // Seed or roll the baseline on a new month.
    let baseline = match (st.month_baseline_month.as_deref(), st.month_baseline_equity) {
        (Some(m), Some(e)) if m == month => e,
        _ => {
            let seed = op.guardrails.month_baseline_capital;
            db.set_month_baseline(seed, &month).map_err(|e| e.to_string())?;
            seed
        }
    };

    let net_realized = db.realized_since(&month_start).map_err(|e| e.to_string())?;
    let loss_pct = if baseline > 0.0 {
        (-net_realized / baseline) * 100.0
    } else {
        0.0
    };

    let breached = loss_pct > op.guardrails.max_monthly_loss_pct;
    Ok(BreakerOutcome {
        tripped: breached && !st.hold,
        loss_pct,
        net_realized,
        baseline,
        already_held: st.hold,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db::DailyRollup;

    fn op() -> OperatorConfig {
        serde_json::from_str(
            r#"{
                "guardrails": {
                    "max_monthly_loss_pct": 10.0,
                    "max_capital_per_pair": 200.0,
                    "month_baseline_capital": 320.0
                },
                "control": { "bind": "127.0.0.1:8787", "token": "t" }
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn trips_when_monthly_loss_exceeds_limit() {
        let db = crate::storage::db::Db::open(std::path::Path::new(":memory:")).unwrap();
        let rid = db.insert_runner("tSOLUSD", "grid", "paper", "2026-09-01T00:00:00Z").unwrap();
        let now = chrono::Utc::now();
        let day = now.format("%Y-%m-05").to_string();
        // Lose ~11% of the 320 baseline (net of fees): realized -34, fees 2 → -36.
        db.upsert_daily_rollup(&DailyRollup {
            runner_id: rid,
            day,
            realized_pnl: -34.0,
            fees: 2.0,
            trades: 5,
            ending_equity: 284.0,
            ending_position: 0.0,
        })
        .unwrap();

        let out = evaluate(&db, &op(), now).unwrap();
        assert!(out.tripped, "loss_pct was {:.2}", out.loss_pct);
        assert!(out.loss_pct > 10.0);

        // Once held, a re-evaluation does not re-trip.
        db.set_hold("test").unwrap();
        let out2 = evaluate(&db, &op(), now).unwrap();
        assert!(!out2.tripped);
        assert!(out2.already_held);
    }

    #[test]
    fn does_not_trip_within_limit() {
        let db = crate::storage::db::Db::open(std::path::Path::new(":memory:")).unwrap();
        let rid = db.insert_runner("tSOLUSD", "grid", "paper", "2026-09-01T00:00:00Z").unwrap();
        let now = chrono::Utc::now();
        let day = now.format("%Y-%m-05").to_string();
        db.upsert_daily_rollup(&DailyRollup {
            runner_id: rid,
            day,
            realized_pnl: -5.0,
            fees: 1.0,
            trades: 2,
            ending_equity: 314.0,
            ending_position: 0.0,
        })
        .unwrap();
        let out = evaluate(&db, &op(), now).unwrap();
        assert!(!out.tripped, "loss_pct was {:.2}", out.loss_pct);
    }
}
