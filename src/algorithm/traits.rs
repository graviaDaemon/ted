use std::collections::HashMap;
use crate::algorithm::GridBot;
use crate::algorithm::PassiveObserver;
use crate::api::{MarketData, TradeSignal};

pub trait Algorithm: Send {
    fn name(&self) -> &str;
    fn on_tick(&mut self, tick: &MarketData) -> Vec<TradeSignal>;
    fn on_fill(&mut self, _price: f64, _is_buy: bool) {}
    fn on_reconnect(&mut self) {}
    fn summary(&self) -> Option<String> { None }
}

type AlgoBuilder = fn(&HashMap<String, String>) -> Result<Box<dyn Algorithm>, String>;

struct AlgoEntry {
    name: &'static str,
    build: AlgoBuilder,
}

static BUILTIN_REGISTRY: &[AlgoEntry] = &[
    AlgoEntry { name: "passive", build: |_| Ok(Box::new(PassiveObserver::new())) },
    AlgoEntry { name: "grid",    build: |o| GridBot::new(o).map(|g| Box::new(g) as _) },
];

pub fn build_algorithm(
    name: &str,
    options: &HashMap<String, String>,
) -> Result<Box<dyn Algorithm>, String> {
    let key = if name.is_empty() { "passive" } else { name };

    if let Some(entry) = BUILTIN_REGISTRY.iter().find(|e| e.name.eq_ignore_ascii_case(key)) {
        return (entry.build)(options);
    }

    crate::algorithm::script::build_script_algorithm(key, options)
        .unwrap_or_else(|| Err(format!("Unknown algorithm: '{}'", name)))
}
