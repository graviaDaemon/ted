use crate::algorithm::Algorithm;
use crate::api::MarketData;
use crate::api::TradeSignal;

pub struct PassiveObserver;

impl PassiveObserver {
    pub fn new() -> Self {
        PassiveObserver
    }
}

impl Algorithm for PassiveObserver {
    fn name(&self) -> &str {
        "passive"
    }

    fn on_tick(&mut self, _tick: &MarketData) -> Vec<TradeSignal> {
        vec![]
    }
}
