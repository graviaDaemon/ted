use chrono::{DateTime, TimeDelta, Utc};
use serde::Serialize;
use crate::api::TradeSignal;

#[derive(Serialize)]
#[allow(dead_code)]
pub struct TradeEntry {
    pub timestamp: DateTime<Utc>,
    pub symbol: String,
    pub last_price: f64,
    pub bid: f64,
    pub ask: f64,
    pub volume: f64,
    pub signals: Vec<TradeSignal>,
    pub dry_run: bool,
}

/// Maximum number of entries kept in memory.
/// At 1-second ticks this covers ~28 hours. Older entries are dropped in
/// batches of 10% when the cap is hit to amortise the drain cost.
const MAX_ENTRIES: usize = 100_000;

pub struct TradeLog {
    pub entries: Vec<TradeEntry>,
}

impl TradeLog {
    pub fn new() -> Self {
        TradeLog { entries: Vec::new() }
    }

    pub fn push(&mut self, entry: TradeEntry) {
        self.entries.push(entry);
        if self.entries.len() > MAX_ENTRIES {
            self.entries.drain(0..MAX_ENTRIES / 10);
        }
    }

    pub fn last_24h(&self) -> &[TradeEntry] {
        let cutoff = Utc::now() - TimeDelta::hours(24);
        let start = self.entries.partition_point(|e| e.timestamp < cutoff);
        &self.entries[start..]
    }

    pub fn signal_entries(&self) -> Vec<&TradeEntry> {
        self.entries.iter().filter(|e| !e.signals.is_empty()).collect()
    }
}
