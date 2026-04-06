use super::RunnerState;
use crate::config::channels::RunnerMode;
use chrono::Utc;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

pub fn build_content(state: &RunnerState, verbose: bool) -> String {
    let mode_str = match state.mode {
        RunnerMode::Simulation => "simulation",
        RunnerMode::Paper => "paper",
        RunnerMode::Live => "**LIVE**",
    };

    let mut md = String::new();
    md.push_str(&format!("# T.E.D — {}\n\n", state.symbol));
    md.push_str("| Field | Value |\n|---|---|\n");
    md.push_str(&format!(
        "| Generated | {} UTC |\n",
        Utc::now().format("%Y-%m-%d %H:%M:%S")
    ));
    md.push_str(&format!("| Algorithm | {} |\n", state.algorithm.name()));
    md.push_str(&format!("| Mode | {} |\n", mode_str));
    md.push_str(&format!(
        "| Started | {} UTC |\n",
        state.started_at.format("%Y-%m-%d %H:%M:%S")
    ));

    let (db, runner_id) = match (&state.db, state.runner_db_id) {
        (Some(db), Some(id)) => (db, id),
        _ => {
            md.push_str("| Data | No database session active |\n");
            if let Some(summary) = state.algorithm.summary() {
                md.push_str("\n## Algorithm Summary\n\n```\n");
                md.push_str(&summary);
                md.push_str("\n```\n");
            }
            return md;
        }
    };

    let ticks = db.query_ticks(runner_id).unwrap_or_default();
    let orders = db.query_orders(runner_id).unwrap_or_default();
    let fills = db.query_fills(runner_id).unwrap_or_default();

    let buy_orders = orders.iter().filter(|o| o.direction == "buy").count();
    let sell_orders = orders.iter().filter(|o| o.direction == "sell").count();

    match (ticks.first(), ticks.last()) {
        (Some(f), Some(l)) => {
            md.push_str(&format!(
                "| Period | {} UTC – {} UTC |\n",
                f.ts.get(..19).unwrap_or(&f.ts),
                l.ts.get(..19).unwrap_or(&l.ts)
            ));
        }
        _ => {
            md.push_str("| Period | no ticks recorded |\n");
        }
    }

    md.push_str(&format!("| Ticks | {} |\n", ticks.len()));
    md.push_str(&format!(
        "| Signals | {} total ({} buy, {} sell) |\n",
        buy_orders + sell_orders,
        buy_orders,
        sell_orders
    ));

    if !ticks.is_empty() {
        let last_price = ticks.last().unwrap().last_price;
        let (lo, hi) = ticks.iter().fold((f64::MAX, f64::MIN), |(lo, hi), t| {
            (lo.min(t.last_price), hi.max(t.last_price))
        });
        md.push_str(&format!("| Price range | {:.2} – {:.2} |\n", lo, hi));
        md.push_str(&format!("| Last price | {:.2} |\n", last_price));

        let cutoff = (Utc::now() - chrono::TimeDelta::hours(24)).to_rfc3339();
        let day_ticks: Vec<_> = ticks.iter().filter(|t| t.ts >= cutoff).collect();
        if day_ticks.len() >= 2 {
            let open = day_ticks.first().unwrap().last_price;
            if open != 0.0 {
                let pct = (last_price - open) / open * 100.0;
                md.push_str(&format!("| 24h price change | {:+.5}% |\n", pct));
            } else {
                md.push_str("| 24h price change | N/A |\n");
            }
        } else {
            md.push_str("| 24h price change | N/A (< 24h data) |\n");
        }

        let pnl = net_pnl_from_fills(&fills, last_price);
        md.push_str(&format!("| Est. PnL | {:+.8} |\n", pnl));
    } else {
        md.push_str("| 24h price change | N/A (no data) |\n");
        md.push_str("| Est. PnL | — |\n");
    }
    md.push('\n');

    if buy_orders + sell_orders == 0 {
        md.push_str("*No signals in this period.*\n");
    }

    if verbose && buy_orders + sell_orders > 0 {
        md.push_str("## Orders\n\n");
        md.push_str("| Placed | Type | Price | Qty | Status |\n|---|---|---|---|---|\n");
        for o in &orders {
            md.push_str(&format!(
                "| {} UTC | {} | {:.2} | {:.8} | {} |\n",
                o.placed_at.get(..19).unwrap_or(&o.placed_at),
                o.direction.to_uppercase(),
                o.price,
                o.quantity,
                o.status,
            ));
        }
        md.push('\n');

        if !fills.is_empty() {
            md.push_str(&pnl_breakdown_from_fills(&fills));
        }
    }

    if let Some(summary) = state.algorithm.summary() {
        md.push_str("## Algorithm Summary\n\n```\n");
        md.push_str(&summary);
        md.push_str("\n```\n");
    }

    md
}

pub fn write_report(symbol: &str, content: &str) -> Result<PathBuf, io::Error> {
    let base = format!(
        "overview_{}_{}",
        symbol.replace(':', "_"),
        Utc::now().format("%Y-%m-%d")
    );
    let path = unique_path(&base, "md");
    write_file(&path, content)?;
    Ok(path)
}

pub fn write_combined(entries: &[(String, String)]) -> Result<PathBuf, io::Error> {
    let base = format!("overview_all_{}", Utc::now().format("%Y-%m-%d"));
    let path = unique_path(&base, "md");

    let mut md = String::new();
    md.push_str("# T.E.D — All Runners\n\n");
    md.push_str(&format!(
        "*Generated: {} UTC — {} runner(s)*\n",
        Utc::now().format("%Y-%m-%d %H:%M:%S"),
        entries.len(),
    ));

    for (_, content) in entries {
        md.push_str("\n---\n\n");
        md.push_str(content);
    }

    write_file(&path, &md)?;
    Ok(path)
}

fn net_pnl_from_fills(fills: &[crate::storage::db::FillRow], last_price: f64) -> f64 {
    let mut cash = 0.0_f64;
    let mut pos = 0.0_f64;
    for fill in fills {
        if fill.direction == "buy" {
            cash -= fill.price * fill.quantity;
            pos += fill.quantity;
        } else {
            cash += fill.price * fill.quantity;
            pos -= fill.quantity;
        }
    }
    cash + pos * last_price
}

fn pnl_breakdown_from_fills(fills: &[crate::storage::db::FillRow]) -> String {
    let mut md = String::from("## Closed Round-trips\n\n");
    md.push_str("| # | Buy Price | Sell Price | Qty | PnL |\n|---|---|---|---|---|\n");

    let mut total = 0.0;
    let mut n = 1u32;
    let mut pending: Vec<(f64, f64)> = Vec::new();

    for fill in fills {
        if fill.direction == "buy" {
            pending.push((fill.price, fill.quantity));
        } else if let Some((bp, bq)) = pending.first().copied() {
            pending.remove(0);
            let qty = bq.min(fill.quantity);
            let pnl = (fill.price - bp) * qty;
            total += pnl;
            md.push_str(&format!(
                "| {} | {:.2} | {:.2} | {:.8} | {:+.8} |\n",
                n, bp, fill.price, qty, pnl,
            ));
            n += 1;
        }
    }

    if n == 1 {
        md.push_str("| — | No completed round-trips yet | | | |\n");
    }
    md.push_str(&format!(
        "\n**Total closed round-trips: {:+.8}**\n\n",
        total
    ));
    md
}

fn unique_path(base: &str, ext: &str) -> PathBuf {
    let candidate = PathBuf::from(format!("{}.{}", base, ext));
    if !candidate.exists() {
        return candidate;
    }
    for n in 2u32..=999 {
        let c = PathBuf::from(format!("{}_{}.{}", base, n, ext));
        if !c.exists() {
            return c;
        }
    }
    PathBuf::from(format!(
        "{}_{}.{}",
        base,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0),
        ext
    ))
}

fn write_file(path: &PathBuf, content: &str) -> io::Result<()> {
    let mut file = fs::File::create(path)?;
    file.write_all(content.as_bytes())?;
    Ok(())
}
