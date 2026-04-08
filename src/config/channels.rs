use std::collections::HashMap;
use tokio::sync::oneshot;

#[derive(Debug, Clone, PartialEq)]
pub enum RunnerMode {
    Simulation,
    Live,
}

pub enum RunnerControl {
    SetAlgorithm {
        name: String,
        options: HashMap<String, String>,
    },
    GenerateOverview {
        verbose: bool,
        reply: oneshot::Sender<String>,
    },
    Pause,
    Resume,
    Kill,
    #[allow(dead_code)]
    PruneOrder(i64),
}
