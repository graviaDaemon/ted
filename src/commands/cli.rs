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
                Ok(CliAction::Spawn {
                    symbol,
                    algorithm: run.algorithm.clone().unwrap_or_default(),
                    options,
                    live: run.live,
                    paper: run.paper,
                    fresh: run.fresh,
                })
            }
            RunCommand::Generate(generate) => Ok(CliAction::Generate {
                symbol: generate.runner.clone(),
                all: generate.all,
                verbose: generate.verbose,
            }),
            RunCommand::Backtest(bt) => {
                let options = parse_options(&bt.option)?;
                Ok(CliAction::Backtest {
                    symbol: bt.symbol.clone(),
                    algorithm: bt.algorithm.clone().unwrap_or_default(),
                    options,
                    timeframe: bt.timeframe.clone(),
                    limit: bt.limit,
                    from_file: bt.from_file.clone(),
                    spread: bt.spread,
                    maker_fee: bt.maker_fee,
                    taker_fee: bt.taker_fee,
                    start_quote: bt.start_quote,
                    start_base: bt.start_base,
                })
            }
            RunCommand::Sweep(sw) => {
                let spacings = match &sw.spacings {
                    Some(csv) => Some(parse_csv_list::<f64>(csv, "spacings")?),
                    None => None,
                };
                let levels = parse_csv_list::<u32>(&sw.levels, "levels")?;
                Ok(CliAction::Sweep {
                    symbol: sw.symbol.clone(),
                    timeframe: sw.timeframe.clone(),
                    limit: sw.limit,
                    from_file: sw.from_file.clone(),
                    spacings,
                    levels,
                    capital: sw.capital,
                    spread: sw.spread,
                    maker_fee: sw.maker_fee,
                    taker_fee: sw.taker_fee,
                    start_quote: sw.start_quote,
                    start_base: sw.start_base,
                })
            }
            RunCommand::Exit => Ok(CliAction::Exit),
        }
    }
}

fn parse_csv_list<T: std::str::FromStr>(
    raw: &str,
    what: &str,
) -> Result<Vec<T>, Box<dyn std::error::Error>> {
    let values: Result<Vec<T>, _> = raw
        .split(',')
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .map(|v| v.parse::<T>())
        .collect();
    match values {
        Ok(v) if !v.is_empty() => Ok(v),
        _ => Err(format!("Invalid --{} '{}': expected a comma-separated list of numbers", what, raw).into()),
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
    Backtest(BacktestCommand),
    Sweep(SweepCommand),
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

    #[arg(long, short = 'l', conflicts_with = "paper")]
    pub live: bool,

    #[arg(long, conflicts_with = "live")]
    pub paper: bool,

    /// Ignore any saved resume state for this symbol and start a fresh grid.
    #[arg(long)]
    pub fresh: bool,
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

#[derive(Args, Debug)]
pub struct BacktestCommand {
    #[arg(short = 's', long)]
    pub symbol: String,

    #[arg(short = 'a', long)]
    pub algorithm: Option<String>,

    #[arg(long, short = 'o', value_name = "KEY=VALUE", num_args = 0..)]
    pub option: Vec<String>,

    #[arg(long, default_value = "1h")]
    pub timeframe: String,

    #[arg(long, default_value_t = 1000)]
    pub limit: usize,

    #[arg(long, value_name = "PATH")]
    pub from_file: Option<String>,

    #[arg(long, default_value_t = 0.0)]
    pub spread: f64,

    #[arg(long, default_value_t = 0.0)]
    pub maker_fee: f64,

    #[arg(long, default_value_t = 0.0)]
    pub taker_fee: f64,

    #[arg(long, default_value_t = 10000.0)]
    pub start_quote: f64,

    #[arg(long, default_value_t = 0.0)]
    pub start_base: f64,
}

#[derive(Args, Debug)]
pub struct SweepCommand {
    #[arg(short = 's', long)]
    pub symbol: String,

    #[arg(short = 't', long, default_value = "30m")]
    pub timeframe: String,

    #[arg(short = 'l', long, default_value_t = 3000)]
    pub limit: usize,

    #[arg(long, value_name = "PATH")]
    pub from_file: Option<String>,

    /// Comma-separated spacing candidates; derived from the candle set's ATR
    /// when omitted.
    #[arg(long, value_name = "CSV")]
    pub spacings: Option<String>,

    #[arg(long, value_name = "CSV", default_value = "2,3,4,6")]
    pub levels: String,

    /// Quote budget each combination sizes from; defaults to --start-quote.
    #[arg(long)]
    pub capital: Option<f64>,

    #[arg(long, default_value_t = 0.0)]
    pub spread: f64,

    #[arg(long, default_value_t = 0.0)]
    pub maker_fee: f64,

    #[arg(long, default_value_t = 0.0)]
    pub taker_fee: f64,

    /// Starting quote wallet for the replay; defaults to --capital.
    #[arg(long)]
    pub start_quote: Option<f64>,

    #[arg(long, default_value_t = 0.0)]
    pub start_base: f64,
}

pub enum CliAction {
    Spawn {
        symbol: String,
        algorithm: String,
        options: HashMap<String, String>,
        live: bool,
        paper: bool,
        fresh: bool,
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
    Generate {
        symbol: Option<String>,
        all: bool,
        verbose: bool,
    },
    Backtest {
        symbol: String,
        algorithm: String,
        options: HashMap<String, String>,
        timeframe: String,
        limit: usize,
        from_file: Option<String>,
        spread: f64,
        maker_fee: f64,
        taker_fee: f64,
        start_quote: f64,
        start_base: f64,
    },
    Sweep {
        symbol: String,
        timeframe: String,
        limit: usize,
        from_file: Option<String>,
        spacings: Option<Vec<f64>>,
        levels: Vec<u32>,
        capital: Option<f64>,
        spread: f64,
        maker_fee: f64,
        taker_fee: f64,
        start_quote: Option<f64>,
        start_base: f64,
    },
    Exit,
}
