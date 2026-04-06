use crate::config::channels::RunnerMode;
use clap::{Args, Parser, Subcommand};
use std::collections::HashMap;

#[derive(Parser, Debug)]
#[command(name = "ted")]
pub struct Cli {
    #[command(subcommand)]
    pub command: RunCommand,
}

impl Cli {
    pub fn handle_command(&self) -> Result<CliAction, Box<dyn std::error::Error>> {
        match &self.command {
            RunCommand::Runner(run) => {
                let options = parse_options(&run.option)?;
                let symbol = run.symbol.clone();

                if run.kill {
                    return Ok(CliAction::Kill { symbol });
                }
                if run.pause {
                    return Ok(CliAction::Pause { symbol });
                }
                if run.resume {
                    return Ok(CliAction::Resume { symbol });
                }
                if let Some(alg) = &run.configure {
                    return Ok(CliAction::Configure {
                        symbol,
                        algorithm: alg.clone(),
                        options,
                    });
                }
                if let Some(mode_str) = &run.set_mode {
                    let mode = parse_mode(mode_str)?;
                    return Ok(CliAction::SetMode { symbol, mode });
                }

                Ok(CliAction::Spawn {
                    symbol,
                    algorithm: run.algorithm.clone().unwrap_or_default(),
                    options,
                    paper: run.paper,
                })
            }
            RunCommand::Generate(generate) => Ok(CliAction::Generate {
                symbol: generate.runner.clone(),
                all: generate.all,
                verbose: generate.verbose,
            }),
            RunCommand::Exit => Ok(CliAction::Exit),
        }
    }
}

fn parse_mode(s: &str) -> Result<RunnerMode, Box<dyn std::error::Error>> {
    match s.to_ascii_lowercase().as_str() {
        "simulation" => Ok(RunnerMode::Simulation),
        "paper" => Ok(RunnerMode::Paper),
        "live" => Ok(RunnerMode::Live),
        other => Err(format!(
            "Unknown mode '{}': expected simulation, paper, or live",
            other
        )
        .into()),
    }
}

fn parse_options(raw: &[String]) -> Result<HashMap<String, String>, Box<dyn std::error::Error>> {
    let mut map = HashMap::new();
    for entry in raw {
        let (k, v) = entry
            .split_once('=')
            .ok_or_else(|| format!("Invalid option '{}': expected key=value format", entry))?;
        map.insert(k.to_string(), v.to_string());
    }
    Ok(map)
}

#[derive(Subcommand, Debug)]
pub enum RunCommand {
    Runner(RunnerCommand),
    Generate(GenerateCommand),
    Exit,
}

#[derive(Args, Debug)]
pub struct RunnerCommand {
    #[arg(short = 's', long)]
    pub symbol: String,

    #[arg(short = 'a', long)]
    pub algorithm: Option<String>,

    #[arg(long, short = 'o', value_name = "KEY=VALUE", num_args = 0..)]
    pub option: Vec<String>,

    #[arg(short = 'p', long, conflicts_with = "resume")]
    pub pause: bool,

    #[arg(short = 'r', long, conflicts_with = "pause")]
    pub resume: bool,

    #[arg(short = 'k', long, conflicts_with_all(["pause", "resume"]))]
    pub kill: bool,

    #[arg(short = 'c', long, value_name = "ALGORITHM")]
    pub configure: Option<String>,

    #[arg(long, short = 'm', value_name = "simulation|paper|live")]
    pub set_mode: Option<String>,

    #[arg(long)]
    pub paper: bool,
}

#[derive(Args, Debug)]
pub struct GenerateCommand {
    #[arg(short = 'r', long, value_name = "SYMBOL", conflicts_with = "all")]
    pub runner: Option<String>,

    #[arg(short = 'a', long, conflicts_with = "runner")]
    pub all: bool,

    #[arg(short = 'v', long)]
    pub verbose: bool,
}

pub enum CliAction {
    Spawn {
        symbol: String,
        algorithm: String,
        options: HashMap<String, String>,
        paper: bool,
    },
    Pause {
        symbol: String,
    },
    Resume {
        symbol: String,
    },
    Kill {
        symbol: String,
    },
    Configure {
        symbol: String,
        algorithm: String,
        options: HashMap<String, String>,
    },
    SetMode {
        symbol: String,
        mode: RunnerMode,
    },
    Generate {
        symbol: Option<String>,
        all: bool,
        verbose: bool,
    },
    Exit,
}
