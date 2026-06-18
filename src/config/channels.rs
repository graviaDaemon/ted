use std::collections::HashMap;
use tokio::sync::oneshot;

#[derive(Debug, Clone, PartialEq)]
pub enum RunnerMode {
    Simulation,
    Paper,
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
    /// Clean app-wide shutdown (Ctrl-D / restart): persist resume state and exit
    /// WITHOUT cancelling resting orders, so the next launch reconciles against
    /// them. Distinct from `Kill`, which cancels orders and forgets the runner.
    Shutdown,
    #[allow(dead_code)]
    PruneOrder(i64),
}
