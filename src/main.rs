mod commands;
mod config;
mod api;
mod algorithm;
mod backtest;
mod runner;
mod storage;
mod logger;
mod tui;
mod util;
mod engine;
mod exchange;

use crate::config::config::{Config, CredentialMode};
use crate::config::channels::{RunnerControl, RunnerMode};
use crate::logger::LogLevel;
use crate::commands::cli::{Cli, CliAction};
use crate::engine::spawn_engine;
use crate::exchange::Exchange;
use crate::exchange::bitfinex::Bitfinex;
use std::collections::HashMap;
use std::sync::Arc;
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
    let (ticker_tx, mut ticker_rx) = channel::<(String, f64)>(64);

    let mut config = Config::load_config("config.json").expect("Failed to load config.json");
    if let Err(e) = &config.validate() {
        eprintln!("Invalid config.json: {}", e);
        std::process::exit(1);
    }

    // Credential routing is process-wide for this changeset (the engine is
    // spawned once). Derive it from startup_defaults.paper; per-runner --paper
    // affects RunnerMode but not which credentials the shared engine uses.
    config.credential_mode = if config.startup_defaults.paper {
        CredentialMode::Paper
    } else {
        CredentialMode::Live
    };

    crate::storage::data_dir();
    logger::init_ticker(ticker_tx);
    let min_level = LogLevel::from_str(&config.startup_defaults.log_level);
    if let Err(e) = logger::init(log_tx, config.startup_defaults.log_retention, min_level) {
        eprintln!("Warning: could not initialise log file: {}", e);
    }

    // Retention: prune high-frequency snapshots older than the configured cutoff
    // once at startup. Fills and daily rollups are kept forever.
    {
        let db_path = crate::storage::data_dir().join("ted.db");
        match crate::storage::db::Db::open(&db_path) {
            Ok(db) => match db.prune_snapshots(config.startup_defaults.snapshot_retention_days) {
                Ok(0) => {}
                Ok(n) => logger::log_info(
                    "[STORE]",
                    &format!(
                        "Pruned {} snapshot row(s) older than {} day(s).",
                        n, config.startup_defaults.snapshot_retention_days
                    ),
                ),
                Err(e) => logger::log_warn("[STORE]", &format!("Snapshot prune failed: {}", e)),
            },
            Err(e) => logger::log_warn("[STORE]", &format!("Could not open DB for prune: {}", e)),
        }
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

    // Exchange seam: Bitfinex is the sole implementation. A `config.exchange`
    // field would dispatch to a different `impl Exchange` here later — the
    // engine, runners, and algorithms would not change.
    let exchange: Arc<dyn Exchange> = Arc::new(Bitfinex::new(config.clone()));
    let (engine_handle, _engine_join) = spawn_engine(exchange.clone());

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
                                    &engine_handle,
                                    &exchange,
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

            msg = ticker_rx.recv() => {
                if let Some((symbol, bid)) = msg {
                    tui.handle_ticker(symbol, bid);
                }
            }
        }
    }

    tui.exit();
    graceful_shutdown(&mut runner_txs, &mut runner_handles).await;
}

async fn graceful_shutdown(
    runner_txs: &mut HashMap<String, Sender<RunnerControl>>,
    runner_handles: &mut HashMap<String, JoinHandle<()>>,
) {
    // Clean app exit: tell runners to persist resume state and leave resting
    // orders in place (Shutdown), rather than Kill which cancels and forgets.
    let symbols: Vec<String> = runner_txs.keys().cloned().collect();
    for symbol in &symbols {
        send_control(runner_txs, runner_handles, symbol, RunnerControl::Shutdown).await;
    }
    runner_txs.clear();
    for (_, handle) in runner_handles.drain() {
        let _ = handle.await;
    }
}

async fn send_control(
    runner_txs: &mut HashMap<String, Sender<RunnerControl>>,
    runner_handles: &mut HashMap<String, JoinHandle<()>>,
    symbol: &str,
    control: RunnerControl,
) {
    match runner_txs.get(symbol) {
        Some(tx) => {
            let tx = tx.clone();
            if tx.send(control).await.is_err() {
                logger::log("[CTRL]", &format!("Runner '{}' is no longer alive; removing.", symbol));
                runner_txs.remove(symbol);
                runner_handles.remove(symbol);
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
    engine_handle: &crate::engine::EngineHandle,
    exchange: &Arc<dyn Exchange>,
) -> bool {
    let args: Vec<&str> = std::iter::once("ted")
        .chain(line.split_whitespace())
        .collect();
    match Cli::try_parse_from(args) {
        Ok(cmd) => match cmd.handle_command() {
            Ok(CliAction::Exit) => return true,
            Ok(action) => dispatch(runner_txs, runner_handles, action, config, engine_handle, exchange).await,
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
    engine_handle: &crate::engine::EngineHandle,
    exchange: &Arc<dyn Exchange>,
) {
    match action {
        CliAction::Spawn { symbol, algorithm, options, live, paper, fresh } => {
            if runner_txs.contains_key(&symbol) {
                logger::log("[CTRL]", &format!("Runner for '{}' is already running.", symbol));
                return;
            }
            let mode = if live {
                RunnerMode::Live
            } else if paper || config.startup_defaults.paper {
                RunnerMode::Paper
            } else {
                RunnerMode::Simulation
            };
            let (tx, rx) = channel::<RunnerControl>(64);
            let sym = symbol.clone();
            let algo = algorithm.clone();
            let cfg = config.clone();
            let eng = engine_handle.clone();
            let mode_label = format!("{:?}", mode).to_lowercase();
            let handle = tokio::spawn(async move {
                crate::runner::run_runner(sym, algo, options, mode, rx, eng, cfg, fresh).await;
            });
            runner_txs.insert(symbol.clone(), tx);
            runner_handles.insert(symbol.clone(), handle);
            logger::log("[CTRL]", &format!("Spawned runner for '{}' (mode: {}).", symbol, mode_label));
        }

        CliAction::Pause { symbol } => {
            send_control(runner_txs, runner_handles, &symbol, RunnerControl::Pause).await;
        }

        CliAction::Resume { symbol } => {
            send_control(runner_txs, runner_handles, &symbol, RunnerControl::Resume).await;
        }

        CliAction::Kill { symbol } => {
            send_control(runner_txs, runner_handles, &symbol, RunnerControl::Kill).await;
            runner_txs.remove(&symbol);
            if let Some(handle) = runner_handles.remove(&symbol) {
                let _ = handle.await;
            }
        }

        CliAction::Configure { symbol, algorithm, options } => {
            send_control(
                runner_txs,
                runner_handles,
                &symbol,
                RunnerControl::SetAlgorithm { name: algorithm, options },
            )
            .await;
        }

        CliAction::Generate { symbol, all, verbose } => {
            if all {
                let symbols: Vec<String> = runner_txs.keys().cloned().collect();
                let mut contents: Vec<(String, String)> = Vec::new();

                for sym in &symbols {
                    let (tx, rx) = oneshot::channel::<String>();
                    send_control(runner_txs, runner_handles, sym, RunnerControl::GenerateOverview { verbose, reply: tx }).await;
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
                send_control(runner_txs, runner_handles, &sym, RunnerControl::GenerateOverview { verbose, reply: tx }).await;
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

        CliAction::Backtest {
            symbol,
            algorithm,
            options,
            timeframe,
            limit,
            from_file,
            spread,
            maker_fee,
            taker_fee,
            start_quote,
            start_base,
        } => {
            let algo_name = if algorithm.is_empty() {
                "passive".to_string()
            } else {
                algorithm
            };

            let candles = if let Some(path) = &from_file {
                crate::backtest::load_candles_from_file(path)
            } else {
                exchange.fetch_candle_history(&symbol, &timeframe, limit).await
            };

            let candles = match candles {
                Ok(c) => c,
                Err(e) => {
                    logger::log("[BACKTEST]", &format!("Failed to load candles: {}", e));
                    return;
                }
            };

            logger::log(
                "[BACKTEST]",
                &format!(
                    "Loaded {} candles for {} — replaying '{}'…",
                    candles.len(),
                    symbol,
                    algo_name
                ),
            );

            let bt_cfg = crate::backtest::BacktestConfig {
                symbol: symbol.clone(),
                algorithm: algo_name,
                options,
                timeframe,
                candles_limit: limit,
                from_file,
                start_quote_balance: start_quote,
                start_base_balance: start_base,
                spread,
                maker_fee,
                taker_fee,
            };

            match crate::backtest::run_backtest(&bt_cfg, &candles) {
                Ok(report) => {
                    for line in report.render_console().lines() {
                        logger::log("[BACKTEST]", line);
                    }
                    match crate::backtest::write_report(&report) {
                        Ok(path) => logger::log(
                            "[BACKTEST]",
                            &format!("Report written to {}", path.display()),
                        ),
                        Err(e) => logger::log(
                            "[BACKTEST]",
                            &format!("Failed to write report: {}", e),
                        ),
                    }
                }
                Err(e) => logger::log("[BACKTEST]", &format!("Backtest failed: {}", e)),
            }
        }

        CliAction::Exit => {}
    }
}
