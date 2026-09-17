//! Loopback control surface (plan/12). Accepts one token-authenticated command
//! per connection and forwards it to the headless run loop, which owns the runner
//! registry. Line protocol: `<token> <command...>\n` → single reply line.

use crate::config::config::ControlConfig;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::{mpsc::Sender, oneshot};

pub struct ControlRequest {
    pub command: String,
    pub reply: oneshot::Sender<String>,
}

/// Bind the control surface and forward authenticated commands. Runs until the
/// process exits.
pub async fn listen(cfg: ControlConfig, cmd_tx: Sender<ControlRequest>) {
    let listener = match TcpListener::bind(&cfg.bind).await {
        Ok(l) => l,
        Err(e) => {
            crate::logger::log_critical("[CTRL]", &format!("Could not bind control surface {}: {}", cfg.bind, e));
            return;
        }
    };
    crate::logger::log("[CTRL]", &format!("Control surface listening on {}.", cfg.bind));

    loop {
        let (stream, _peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                crate::logger::log_warn("[CTRL]", &format!("accept failed: {}", e));
                continue;
            }
        };
        let token = cfg.token.clone();
        let cmd_tx = cmd_tx.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            if reader.read_line(&mut line).await.is_err() {
                return;
            }
            let line = line.trim();
            let reply = match line.split_once(char::is_whitespace) {
                Some((tok, rest)) if tok == token => {
                    let (tx, rx) = oneshot::channel();
                    if cmd_tx.send(ControlRequest { command: rest.trim().to_string(), reply: tx }).await.is_err() {
                        "ERR operator not accepting commands".to_string()
                    } else {
                        rx.await.unwrap_or_else(|_| "ERR no reply".to_string())
                    }
                }
                _ => "ERR unauthorized".to_string(),
            };
            let mut stream = reader.into_inner();
            let _ = stream.write_all(reply.as_bytes()).await;
            let _ = stream.write_all(b"\n").await;
            let _ = stream.shutdown().await;
        });
    }
}
