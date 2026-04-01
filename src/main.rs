mod commands;
mod config;
mod api;
mod algorithm;
mod runner;
mod storage;
mod logger;
mod tui;

use crate::config::config::Config;
use crate::config::channels::RunnerControl;
use crate::commands::cli::{Cli, CliAction};
use crate::algorithm::traits::build_algorithm;
use std::collections::HashMap;
use std::time::Duration;
use clap::Parser;
use crossterm::event::{Event, EventStream};
use futures_util::StreamExt;
use tokio::select;
use tokio::sync::mpsc::{channel, Sender};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::timeout;

#[tokio::main]
async fn main() {
    let (log_tx, mut log_rx) = channel::<String>(256);

    let config = Config::load_config("config.json").expect("Failed to load config.json");
    if let Err(e) = validate_config(&config) {
        eprintln!("Invalid config.json: {}", e);
        std::process::exit(1);
    }

    if let Err(e) = logger::init(log_tx, config.retention) {
        eprintln!("Warning: could not initialise log file: {}", e);
    }

    let script_registry = crate::algorithm::script::init_script_registry("algorithms");
    {
        let count = script_registry.read().map(|r| r.len()).unwrap_or(0);
        if count > 0 {
            logger::log("[ALGO]", &format!("Loaded {} script algorithm(s) from ./algorithms/", count));
        } else {
            logger::log("[ALGO]", "No scripts found in ./algorithms/ — folder watched for new files.");
        }
    }
    tokio::spawn(crate::algorithm::script::watch_algorithms("algorithms", script_registry));
    let mut runner_txs: HashMap<String, Sender<RunnerControl>> = HashMap::new();
    let mut runner_handles: HashMap<String, JoinHandle<()>> = HashMap::new();

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::terminal::disable_raw_mode();
        default_hook(info);
    }));

    let mut tui = match tui::Tui::enter() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Failed to enter TUI mode: {}", e);
            return;
        }
    };

    let mut events = EventStream::new();

    loop {
        select! {
            event = events.next() => {
                match event {
                    Some(Ok(Event::Key(key_event))) => {
                        if let Some(line) = tui.handle_key(key_event) {
                            if line == "\x04" {
                                break;
                            }
                            if !line.trim().is_empty()
                                && dispatch_line(
                                    &mut runner_txs,
                                    &mut runner_handles,
                                    line.trim(),
                                    &config,
                                ).await
                            {
                                break;
                            }
                        }
                    }
                    Some(Ok(Event::Resize(_, _))) => {
                        tui.handle_resize();
                    }
                    _ => {}
                }
            }

            msg = log_rx.recv() => {
                if let Some(line) = msg {
                    tui.handle_log(&line);
                }
            }
        }
    }

    tui.exit();
    graceful_shutdown(&mut runner_txs, &mut runner_handles).await;
}

fn validate_config(c: &Config) -> Result<(), String> {
    if c.key.is_empty()              { return Err("'key' is empty".into()); }
    if c.secret.is_empty()           { return Err("'secret' is empty".into()); }
    if c.ws_endpoint.is_empty()      { return Err("'ws_endpoint' is empty".into()); }
    if c.auth_ws_endpoint.is_empty() { return Err("'auth_ws_endpoint' is empty".into()); }
    if c.auth_endpoint.is_empty()    { return Err("'auth_endpoint' is empty".into()); }
    Ok(())
}

async fn graceful_shutdown(
    runner_txs: &mut HashMap<String, Sender<RunnerControl>>,
    runner_handles: &mut HashMap<String, JoinHandle<()>>,
) {
    let symbols: Vec<String> = runner_txs.keys().cloned().collect();
    for symbol in &symbols {
        send_control(runner_txs, symbol, RunnerControl::Kill).await;
    }
    runner_txs.clear();
    for (_, handle) in runner_handles.drain() {
        let _ = handle.await;
    }
}

async fn send_control(
    runner_txs: &mut HashMap<String, Sender<RunnerControl>>,
    symbol: &str,
    control: RunnerControl,
) {
    match runner_txs.get(symbol) {
        Some(tx) => {
            let tx = tx.clone();
            if tx.send(control).await.is_err() {
                logger::log("[CTRL]", &format!("Runner '{}' is no longer alive; removing.", symbol));
                runner_txs.remove(symbol);
            }
        }
        None => logger::log("[CTRL]", &format!("No runner found for symbol '{}'", symbol)),
    }
}

async fn dispatch_line(
    runner_txs: &mut HashMap<String, Sender<RunnerControl>>,
    runner_handles: &mut HashMap<String, JoinHandle<()>>,
    line: &str,
    config: &Config,
) -> bool {
    let args: Vec<&str> = std::iter::once("ted")
        .chain(line.split_whitespace())
        .collect();
    match Cli::try_parse_from(args) {
        Ok(cmd) => match cmd.handle_command() {
            Ok(CliAction::Exit) => return true,
            Ok(action) => dispatch(runner_txs, runner_handles, action, config).await,
            Err(e) => logger::log("[CTRL]", &format!("Error: {}", e)),
        },
        Err(e) => logger::log("[CTRL]", &format!("{}", e)),
    }
    false
}

async fn dispatch(
    runner_txs: &mut HashMap<String, Sender<RunnerControl>>,
    runner_handles: &mut HashMap<String, JoinHandle<()>>,
    action: CliAction,
    config: &Config,
) {
    match action {
        CliAction::Spawn { symbol, algorithm, options } => {
            if runner_txs.contains_key(&symbol) {
                logger::log("[CTRL]", &format!("Runner for '{}' is already running.", symbol));
                return;
            }
            let algo = match build_algorithm(&algorithm, &options) {
                Ok(a) => a,
                Err(e) => {
                    logger::log(
                        "[CTRL]",
                        &format!("Failed to build algorithm '{}': {}", algorithm, e),
                    );
                    return;
                }
            };
            let (tx, rx) = channel::<RunnerControl>(64);
            let sym = symbol.clone();
            let cfg = config.clone();
            let handle = tokio::spawn(async move {
                crate::runner::run_runner(sym, algo, rx, cfg).await;
            });
            runner_txs.insert(symbol.clone(), tx);
            runner_handles.insert(symbol.clone(), handle);
            logger::log("[CTRL]", &format!("Spawned runner for '{}' (mode: dry-run).", symbol));
        }

        CliAction::Pause { symbol } => {
            send_control(runner_txs, &symbol, RunnerControl::Pause).await;
        }

        CliAction::Resume { symbol } => {
            send_control(runner_txs, &symbol, RunnerControl::Resume).await;
        }

        CliAction::Kill { symbol } => {
            send_control(runner_txs, &symbol, RunnerControl::Kill).await;
            runner_txs.remove(&symbol);
            if let Some(handle) = runner_handles.remove(&symbol) {
                let _ = handle.await;
            }
        }

        CliAction::Configure { symbol, algorithm, options } => {
            send_control(
                runner_txs,
                &symbol,
                RunnerControl::SetAlgorithm { name: algorithm, options },
            )
            .await;
        }

        CliAction::EnableLive { symbol } => {
            send_control(runner_txs, &symbol, RunnerControl::EnableLive).await;
        }

        CliAction::DisableLive { symbol } => {
            send_control(runner_txs, &symbol, RunnerControl::DisableLive).await;
        }

        CliAction::Generate { symbol, all, verbose } => {
            if all {
                let symbols: Vec<String> = runner_txs.keys().cloned().collect();
                let mut contents: Vec<(String, String)> = Vec::new();

                for sym in &symbols {
                    let (tx, rx) = oneshot::channel::<String>();
                    send_control(runner_txs, sym, RunnerControl::GenerateOverview { verbose, reply: tx }).await;
                    match timeout(Duration::from_secs(10), rx).await {
                        Ok(Ok(content)) => contents.push((sym.clone(), content)),
                        Ok(Err(_)) => logger::log(
                            "[CTRL]",
                            &format!("Runner '{}' dropped overview reply channel.", sym),
                        ),
                        Err(_) => logger::log(
                            "[CTRL]",
                            &format!("Runner '{}' did not respond within 10s — skipping.", sym),
                        ),
                    }
                }

                match crate::runner::report::write_combined(&contents) {
                    Ok(path) => logger::log(
                        "[CTRL]",
                        &format!("Combined overview written to {}", path.display()),
                    ),
                    Err(e) => logger::log(
                        "[CTRL]",
                        &format!("Failed to write combined overview: {}", e),
                    ),
                }
            } else if let Some(sym) = symbol {
                let (tx, rx) = oneshot::channel::<String>();
                send_control(runner_txs, &sym, RunnerControl::GenerateOverview { verbose, reply: tx }).await;
                match timeout(Duration::from_secs(10), rx).await {
                    Ok(Ok(content)) => {
                        match crate::runner::report::write_report(&sym, &content) {
                            Ok(path) => logger::log(
                                "[CTRL]",
                                &format!("Overview written to {}", path.display()),
                            ),
                            Err(e) => logger::log(
                                "[CTRL]",
                                &format!("Failed to write overview: {}", e),
                            ),
                        }
                    }
                    Ok(Err(_)) => logger::log(
                        "[CTRL]",
                        &format!("Runner '{}' dropped overview reply channel.", sym),
                    ),
                    Err(_) => logger::log(
                        "[CTRL]",
                        &format!("Runner '{}' did not respond within 10s.", sym),
                    ),
                }
            } else {
                logger::log("[CTRL]", "generate: specify --runner <SYMBOL> or --all");
            }
        }

        CliAction::Exit => {}
    }
}
