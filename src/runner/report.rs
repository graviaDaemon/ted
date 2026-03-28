use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use chrono::Utc;
use crate::api::TradeSignal;
use super::RunnerState;
use super::trade_log::TradeEntry;

pub fn build_content(state: &RunnerState, verbose: bool) -> String {
    let entries  = &state.trade_log.entries;
    let signals  = state.trade_log.signal_entries();

    let buy_count: usize = signals.iter()
        .flat_map(|e| e.signals.iter())
        .filter(|s| matches!(s, TradeSignal::Buy { .. }))
        .count();
    let sell_count: usize = signals.iter()
        .flat_map(|e| e.signals.iter())
        .filter(|s| matches!(s, TradeSignal::Sell { .. }))
        .count();

    let mut md = String::new();

    md.push_str(&format!("# T.E.D — {}\n\n", state.symbol));
    md.push_str("| Field | Value |\n|---|---|\n");
    md.push_str(&format!(
        "| Generated | {} UTC |\n",
        Utc::now().format("%Y-%m-%d %H:%M:%S")
    ));
    md.push_str(&format!("| Algorithm | {} |\n", state.algorithm.name()));
    md.push_str(&format!(
        "| Mode | {} |\n",
        if state.dry_run { "dry-run" } else { "**LIVE**" }
    ));
    md.push_str(&format!(
        "| Started | {} UTC |\n",
        state.started_at.format("%Y-%m-%d %H:%M:%S")
    ));

    match (entries.first(), entries.last()) {
        (Some(f), Some(l)) => {
            md.push_str(&format!(
                "| Period | {} UTC – {} UTC |\n",
                f.timestamp.format("%Y-%m-%d %H:%M:%S"),
                l.timestamp.format("%Y-%m-%d %H:%M:%S"),
            ));
        }
        _ => { md.push_str("| Period | no ticks recorded |\n"); }
    }

    md.push_str(&format!("| Ticks | {} |\n", entries.len()));
    md.push_str(&format!(
        "| Signals | {} total ({} buy, {} sell) |\n",
        buy_count + sell_count, buy_count, sell_count,
    ));

    if !entries.is_empty() {
        let last_price = entries.last().unwrap().last_price;
        let (lo, hi) = price_range(entries);
        md.push_str(&format!("| Price range | {:.2} – {:.2} |\n", lo, hi));
        md.push_str(&format!("| Last price | {:.2} |\n", last_price));

        let day = state.trade_log.last_24h();
        if day.len() >= 2 {
            let open = day.first().unwrap().last_price;
            if open != 0.0 {
                let pct = (last_price - open) / open * 100.0;
                md.push_str(&format!("| 24h price change | {:+.5}% |\n", pct));
            } else {
                md.push_str("| 24h price change | N/A |\n");
            }
        } else {
            md.push_str("| 24h price change | N/A (< 24h data) |\n");
        }

        let pnl = net_pnl(&signals, last_price);
        md.push_str(&format!("| Est. PnL | {:+.8} |\n", pnl));
    } else {
        md.push_str("| 24h price change | N/A (no data) |\n");
        md.push_str("| Est. PnL | — |\n");
    }
    md.push('\n');

    if buy_count + sell_count == 0 {
        md.push_str("*No signals in this period.*\n");
    }

    if verbose && (buy_count + sell_count > 0) {
        md.push_str("## Signals\n\n");
        md.push_str("| Timestamp | Type | Price | Qty | Reason |\n|---|---|---|---|---|\n");
        for entry in &signals {
            for sig in &entry.signals {
                let (kind, price, qty, reason) = match sig {
                    TradeSignal::Buy  { price, quantity, reason, .. } => ("BUY",  price, quantity, reason.as_str()),
                    TradeSignal::Sell { price, quantity, reason, .. } => ("SELL", price, quantity, reason.as_str()),
                };
                md.push_str(&format!(
                    "| {} UTC | {} | {:.2} | {:.8} | {} |\n",
                    entry.timestamp.format("%Y-%m-%d %H:%M:%S"),
                    kind, price, qty, reason,
                ));
            }
        }
        md.push('\n');
        md.push_str(&pnl_breakdown(&signals));
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

fn unique_path(base: &str, ext: &str) -> PathBuf {
    let candidate = PathBuf::from(format!("{}.{}", base, ext));
    if !candidate.exists() { return candidate; }
    for n in 2u32..=999 {
        let c = PathBuf::from(format!("{}_{}.{}", base, n, ext));
        if !c.exists() { return c; }
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

fn price_range(entries: &[TradeEntry]) -> (f64, f64) {
    entries.iter().fold((f64::MAX, f64::MIN), |(lo, hi), e| {
        (lo.min(e.last_price), hi.max(e.last_price))
    })
}

fn net_pnl(signals: &[&TradeEntry], last_price: f64) -> f64 {
    let mut cash = 0.0_f64;
    let mut pos  = 0.0_f64;

    for e in signals {
        for sig in &e.signals {
            match sig {
                TradeSignal::Buy  { price, quantity, .. } => { cash -= price * quantity; pos += quantity; }
                TradeSignal::Sell { price, quantity, .. } => { cash += price * quantity; pos -= quantity; }
            }
        }
    }

    cash + pos * last_price
}

fn pnl_breakdown(signals: &[&TradeEntry]) -> String {
    let mut md = String::from("## Closed Round-trips\n\n");
    md.push_str("| # | Buy Price | Sell Price | Qty | PnL |\n|---|---|---|---|---|\n");

    let mut total = 0.0;
    let mut n = 1u32;
    let mut pending: Vec<(f64, f64)> = Vec::new();

    for e in signals {
        for sig in &e.signals {
            match sig {
                TradeSignal::Buy  { price, quantity, .. } => pending.push((*price, *quantity)),
                TradeSignal::Sell { price: sp, quantity: sq, .. } => {
                    if let Some((bp, bq)) = pending.first().copied() {
                        pending.remove(0);
                        let qty = bq.min(*sq);
                        let pnl = (sp - bp) * qty;
                        total += pnl;
                        md.push_str(&format!(
                            "| {} | {:.2} | {:.2} | {:.8} | {:+.8} |\n",
                            n, bp, sp, qty, pnl,
                        ));
                        n += 1;
                    }
                }
            }
        }
    }

    if n == 1 {
        md.push_str("| — | No completed round-trips yet | | | |\n");
    }

    md.push_str(&format!("\n**Total closed round-trips: {:+.8}**\n\n", total));
    md
}
