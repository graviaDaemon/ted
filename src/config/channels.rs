use std::collections::HashMap;
use tokio::sync::oneshot;

pub enum RunnerControl {
    SetAlgorithm { name: String, options: HashMap<String, String> },
    EnableLive,
    DisableLive,
    GenerateOverview { verbose: bool, reply: oneshot::Sender<String> },
    Pause,
    Resume,
    Kill,
    #[allow(dead_code)]
    PruneOrder(i64),
}
