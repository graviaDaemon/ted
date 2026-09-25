//! The un-bypassable guardrail governor (plan/12). Every control-surface command
//! passes through `check` before dispatch; anything outside the configured limits
//! is rejected and surfaced to the operating agent. Pure functions — unit-tested,
//! no I/O.

use crate::commands::cli::CliAction;
use crate::config::config::OperatorConfig;
use chrono::{DateTime, Utc};

/// Reject a command that breaches the guardrails. `now`/`last_config_change`
/// drive the anti-churn min-interval; `options` are inspected for capital caps.
pub fn check(
    action: &CliAction,
    op: &OperatorConfig,
    now: DateTime<Utc>,
    last_config_change: Option<DateTime<Utc>>,
) -> Result<(), String> {
    let g = &op.guardrails;
    match action {
        CliAction::Spawn { symbol, options, .. } => {
            reject_unwhitelisted(symbol, op)?;
            reject_over_capital(options, g.max_capital_per_pair)?;
            reject_churn(now, last_config_change, g.min_days_between_config_changes)?;
            Ok(())
        }
        CliAction::Configure { options, .. } => {
            reject_over_capital(options, g.max_capital_per_pair)?;
            reject_churn(now, last_config_change, g.min_days_between_config_changes)?;
            Ok(())
        }
        // Reducing/closing risk and read-only ops are always allowed.
        CliAction::Kill { .. }
        | CliAction::Pause { .. }
        | CliAction::Resume { .. }
        | CliAction::FinishExits { .. }
        | CliAction::Rebuild { .. }
        | CliAction::Alert { .. }
        | CliAction::Generate { .. }
        | CliAction::Backtest { .. }
        | CliAction::Sweep { .. }
        | CliAction::Status => Ok(()),
        // ClearHold is gated separately (needs the live monthly-loss figure).
        CliAction::ClearHold => Ok(()),
        CliAction::Exit => Err("exit is not available over the control surface".into()),
    }
}

/// A held account may only be resumed once month-to-date loss is back within the
/// limit — otherwise the agent would just re-arm a losing month.
pub fn can_clear_hold(op: &OperatorConfig, month_loss_pct: f64) -> Result<(), String> {
    if month_loss_pct > op.guardrails.max_monthly_loss_pct {
        return Err(format!(
            "cannot clear hold: month-to-date loss {:.2}% still exceeds the {:.2}% limit",
            month_loss_pct, op.guardrails.max_monthly_loss_pct
        ));
    }
    Ok(())
}

fn reject_unwhitelisted(symbol: &str, op: &OperatorConfig) -> Result<(), String> {
    let wl = &op.guardrails.whitelisted_pairs;
    if wl.is_empty() || wl.iter().any(|s| s == symbol) {
        Ok(())
    } else {
        Err(format!("pair '{}' is not in the whitelist", symbol))
    }
}

fn reject_over_capital(
    options: &std::collections::HashMap<String, String>,
    max_capital: f64,
) -> Result<(), String> {
    if let Some(raw) = options.get("capital") {
        let cap: f64 = raw
            .parse()
            .map_err(|_| format!("capital option '{}' is not a number", raw))?;
        if cap > max_capital {
            return Err(format!(
                "capital {:.2} exceeds max_capital_per_pair {:.2}",
                cap, max_capital
            ));
        }
    }
    Ok(())
}

fn reject_churn(
    now: DateTime<Utc>,
    last: Option<DateTime<Utc>>,
    min_days: i64,
) -> Result<(), String> {
    if min_days <= 0 {
        return Ok(());
    }
    if let Some(last) = last {
        let days = (now - last).num_days();
        if days < min_days {
            return Err(format!(
                "config change rejected: only {} of {} required days since the last change",
                days, min_days
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn op() -> OperatorConfig {
        serde_json::from_str(
            r#"{
                "runners": [],
                "guardrails": {
                    "max_monthly_loss_pct": 10.0,
                    "max_capital_per_pair": 200.0,
                    "whitelisted_pairs": ["tSOLUSD"],
                    "min_days_between_config_changes": 14,
                    "month_baseline_capital": 320.0
                },
                "control": { "bind": "127.0.0.1:8787", "token": "t" }
            }"#,
        )
        .unwrap()
    }

    fn spawn(symbol: &str, capital: Option<&str>) -> CliAction {
        let mut options = HashMap::new();
        if let Some(c) = capital {
            options.insert("capital".to_string(), c.to_string());
        }
        CliAction::Spawn {
            symbol: symbol.to_string(),
            algorithm: "grid".to_string(),
            options,
            live: false,
            paper: true,
            fresh: true,
        }
    }

    #[test]
    fn rejects_non_whitelisted_pair() {
        let now = Utc::now();
        assert!(check(&spawn("tXMRUSD", None), &op(), now, None).is_err());
        assert!(check(&spawn("tSOLUSD", None), &op(), now, None).is_ok());
    }

    #[test]
    fn rejects_over_capital() {
        let now = Utc::now();
        assert!(check(&spawn("tSOLUSD", Some("250")), &op(), now, None).is_err());
        assert!(check(&spawn("tSOLUSD", Some("150")), &op(), now, None).is_ok());
    }

    #[test]
    fn rejects_churn_within_min_days() {
        let now = Utc::now();
        let recent = now - chrono::Duration::days(3);
        assert!(check(&spawn("tSOLUSD", None), &op(), now, Some(recent)).is_err());
        let old = now - chrono::Duration::days(20);
        assert!(check(&spawn("tSOLUSD", None), &op(), now, Some(old)).is_ok());
    }

    #[test]
    fn rebuild_and_alert_allowed_inside_churn_window() {
        let now = Utc::now();
        let recent = Some(now - chrono::Duration::days(1));
        let rebuild = CliAction::Rebuild { symbol: "tXMRUSD".to_string() };
        let alert = CliAction::Alert { message: "idle".to_string() };
        assert!(check(&rebuild, &op(), now, recent).is_ok());
        assert!(check(&alert, &op(), now, recent).is_ok());
    }

    #[test]
    fn clear_hold_gated_on_recovery() {
        assert!(can_clear_hold(&op(), 12.0).is_err());
        assert!(can_clear_hold(&op(), 5.0).is_ok());
    }
}
