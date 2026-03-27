use std::collections::HashMap;
use tokio::sync::oneshot;

pub enum RunnerControl {
    SetAlgorithm { name: String, options: HashMap<String, String> },
    EnableLive,
    DisableLive,
    /// Request an overview report. The runner builds the markdown content and
    /// sends it back through `reply`; the controller decides where to write it.
    GenerateOverview { verbose: bool, reply: oneshot::Sender<String> },
    Pause,
    Resume,
    Kill,
    /// Remove a confirmed-filled or confirmed-cancelled order id from live tracking.
    /// Sent by the authenticated WebSocket handler when a fill/cancel event is received.
    #[allow(dead_code)]
    PruneOrder(i64),
}
