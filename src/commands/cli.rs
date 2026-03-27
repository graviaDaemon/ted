use std::collections::HashMap;
use clap::{Args, Parser, Subcommand};
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
                    return Ok(CliAction::Configure { symbol, algorithm: alg.clone(), options });
                }
                if run.enable_live {
                    return Ok(CliAction::EnableLive { symbol });
                }
                if run.disable_live {
                    return Ok(CliAction::DisableLive { symbol });
                }

                // Default: no lifecycle flag set — spawn a new runner
                Ok(CliAction::Spawn {
                    symbol,
                    algorithm: run.algorithm.clone().unwrap_or_default(),
                    options,
                })
            }
            RunCommand::Generate(generate) => {
                Ok(CliAction::Generate {
                    symbol: generate.runner.clone(),
                    all: generate.all,
                    verbose: generate.verbose,
                })
            }
            RunCommand::Exit => Ok(CliAction::Exit),
        }
    }
}

fn parse_options(raw: &[String]) -> Result<HashMap<String, String>, Box<dyn std::error::Error>> {
    let mut map = HashMap::new();
    for entry in raw {
        let (k, v) = entry.split_once('=')
            .ok_or_else(|| format!("Invalid option '{}': expected key=value format", entry))?;
        map.insert(k.to_string(), v.to_string());
    }
    Ok(map)
}

#[derive(Subcommand, Debug)]
pub enum RunCommand {
    Runner(RunnerCommand),
    Generate(GenerateCommand),
    Exit
}

#[derive(Args, Debug)]
pub struct RunnerCommand {
    #[arg(short = 's', long)]
    pub symbol: String,

    #[arg(short = 'a', long)]
    pub algorithm: Option<String>,

    #[arg(long, short = 'o', value_name = "KEY=VALUE", num_args = 0..)]
    pub  option: Vec<String>,

    #[arg(short = 'p', long, conflicts_with = "resume")]
    pub pause: bool,

    #[arg(short = 'r', long, conflicts_with = "pause")]
    pub resume: bool,

    #[arg(short = 'k', long, conflicts_with_all(["pause","resume"]) )]
    pub kill: bool,

    #[arg(short = 'c', long, value_name = "ALGORITHM")]
    pub configure: Option<String>,

    #[arg(short = 'l', long, conflicts_with = "disable_live")]
    pub enable_live: bool,

    #[arg(short = 'd', long, conflicts_with = "enable_live")]
    pub disable_live: bool,
}

#[derive(Args, Debug)]
pub struct GenerateCommand {
    #[arg(short = 'r', long, value_name = "SYMBOL", conflicts_with = "all")]
    pub runner: Option<String>,   // generate for a specific runner

    #[arg(short = 'a', long, conflicts_with = "runner")]
    pub all: bool,                // generate for all running runners

    #[arg(short = 'v', long)]
    pub verbose: bool,
}

pub enum CliAction {
    Spawn    { symbol: String, algorithm: String, options: HashMap<String, String> },
    Pause    { symbol: String },
    Resume   { symbol: String },
    Kill     { symbol: String },
    Configure { symbol: String, algorithm: String, options: HashMap<String, String> },
    EnableLive  { symbol: String },
    DisableLive { symbol: String },
    Generate { symbol: Option<String>, all: bool, verbose: bool },
    Exit,
}