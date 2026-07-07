use crate::config::channels::TuiEvent;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use tokio::sync::mpsc::Sender;
use chrono::{NaiveDate, Utc};

#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Eq, Ord)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
    Critical,
}

impl LogLevel {
    pub fn from_str(s: &str) -> LogLevel {
        match s.trim().to_lowercase().as_str() {
            "trace" => LogLevel::Trace,
            "debug" => LogLevel::Debug,
            "info" => LogLevel::Info,
            "warn" | "warning" => LogLevel::Warn,
            "error" => LogLevel::Error,
            "critical" | "crit" => LogLevel::Critical,
            _ => LogLevel::Info,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            LogLevel::Trace => "TRACE",
            LogLevel::Debug => "DEBUG",
            LogLevel::Info => "INFO",
            LogLevel::Warn => "WARN",
            LogLevel::Error => "ERROR",
            LogLevel::Critical => "CRITICAL",
        }
    }
}

struct LogState {
    file: std::fs::File,
    date: String,
}

static LOG_STATE: OnceLock<Mutex<LogState>> = OnceLock::new();
static LOG_TX: OnceLock<Sender<String>> = OnceLock::new();
static TUI_TX: OnceLock<Sender<TuiEvent>> = OnceLock::new();
static MIN_LEVEL: OnceLock<LogLevel> = OnceLock::new();

fn logs_dir() -> PathBuf {
    crate::storage::data_dir().join("logs")
}

fn log_path_for(date: &str) -> PathBuf {
    logs_dir().join(format!("ted_{}.log", date))
}

fn archive_old_logs(retention: u32) {
    if retention == 0 {
        return;
    }
    let logs = logs_dir();
    let archive = logs.join("archive");
    if let Err(e) = std::fs::create_dir_all(&archive) {
        eprintln!("[LOGGER] Failed to create logs/archive: {}", e);
        return;
    }
    let entries = match std::fs::read_dir(&logs) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("[LOGGER] Failed to read logs/: {}", e);
            return;
        }
    };
    let today = Utc::now().date_naive();
    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("ted_") || !name.ends_with(".log") {
            continue;
        }
        let date_str = &name["ted_".len()..name.len() - ".log".len()];
        let parsed = match NaiveDate::parse_from_str(date_str, "%d-%m-%Y") {
            Ok(d) => d,
            Err(_) => continue,
        };
        if (today - parsed).num_days() <= retention as i64 {
            continue;
        }
        let src  = entry.path();
        let dest = archive.join(&name);
        if std::fs::rename(&src, &dest).is_err() {
            if std::fs::copy(&src, &dest).is_ok() {
                let _ = std::fs::remove_file(&src);
            } else {
                eprintln!("[LOGGER] Failed to archive {}", name);
            }
        }
    }
}

pub fn init(tx: Sender<String>, retention: u32, min_level: LogLevel) -> io::Result<()> {
    std::fs::create_dir_all(logs_dir())?;
    let today = Utc::now().format("%d-%m-%Y").to_string();
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path_for(&today))?;
    archive_old_logs(retention);
    LOG_STATE
        .set(Mutex::new(LogState { file, date: today }))
        .map_err(|_| io::Error::other("Logger already initialised"))?;
    let _ = LOG_TX.set(tx);
    let _ = MIN_LEVEL.set(min_level);
    Ok(())
}

pub fn init_tui_events(tx: Sender<TuiEvent>) {
    let _ = TUI_TX.set(tx);
}

pub fn update_ticker(symbol: String, bid: f64) {
    notify_tui(TuiEvent::Ticker { symbol, bid });
}

pub fn notify_tui(event: TuiEvent) {
    if let Some(tx) = TUI_TX.get() {
        let _ = tx.try_send(event);
    }
}

fn level_enabled(level: LogLevel) -> bool {
    let min = MIN_LEVEL.get().copied().unwrap_or(LogLevel::Info);
    level >= min
}

pub fn logl(level: LogLevel, source: &str, msg: &str) {
    if !level_enabled(level) {
        return;
    }

    let line = format!(
        "[{} UTC] [{}] {} {}\n",
        Utc::now().format("%Y-%m-%d %H:%M:%S"),
        level.as_str(),
        source,
        msg,
    );

    if let Some(tx) = LOG_TX.get() {
        let _ = tx.try_send(line.clone());
    }

    if let Some(lock) = LOG_STATE.get()
        && let Ok(mut state) = lock.lock()
    {
        let today = Utc::now().format("%d-%m-%Y").to_string();
        if state.date != today {
            match OpenOptions::new().create(true).append(true).open(log_path_for(&today)) {
                Ok(new_file) => {
                    state.file = new_file;
                    state.date = today;
                }
                Err(e) => eprintln!("[LOGGER] Failed to open new log file: {}", e),
            }
        }
        if let Err(e) = state.file.write_all(line.as_bytes()) {
            eprintln!("[LOGGER WRITE ERROR] {}: {}", e, line.trim());
        }
    }
}

pub fn log(source: &str, msg: &str) {
    logl(LogLevel::Info, source, msg);
}

#[allow(dead_code)] // convenience helper; consumed by later changesets (plan 02–05)
pub fn log_trace(source: &str, msg: &str) {
    logl(LogLevel::Trace, source, msg);
}

#[allow(dead_code)] // convenience helper; consumed by later changesets (plan 02–05)
pub fn log_debug(source: &str, msg: &str) {
    logl(LogLevel::Debug, source, msg);
}

pub fn log_info(source: &str, msg: &str) {
    logl(LogLevel::Info, source, msg);
}

pub fn log_warn(source: &str, msg: &str) {
    logl(LogLevel::Warn, source, msg);
}

#[allow(dead_code)] // convenience helper; consumed by later changesets (plan 02–05)
pub fn log_error(source: &str, msg: &str) {
    logl(LogLevel::Error, source, msg);
}

#[allow(dead_code)] // convenience helper; consumed by later changesets (plan 02–05)
pub fn log_critical(source: &str, msg: &str) {
    logl(LogLevel::Critical, source, msg);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_str_is_case_insensitive() {
        assert_eq!(LogLevel::from_str("trace"), LogLevel::Trace);
        assert_eq!(LogLevel::from_str("DEBUG"), LogLevel::Debug);
        assert_eq!(LogLevel::from_str("Info"), LogLevel::Info);
        assert_eq!(LogLevel::from_str("  WARN  "), LogLevel::Warn);
        assert_eq!(LogLevel::from_str("error"), LogLevel::Error);
        assert_eq!(LogLevel::from_str("CRITICAL"), LogLevel::Critical);
    }

    #[test]
    fn from_str_unknown_defaults_to_info() {
        assert_eq!(LogLevel::from_str("nonsense"), LogLevel::Info);
        assert_eq!(LogLevel::from_str(""), LogLevel::Info);
    }

    #[test]
    fn level_ordering_filters_correctly() {
        // With a minimum of Info, a Trace line is below threshold and dropped.
        assert!(LogLevel::Trace < LogLevel::Info);
        assert!(LogLevel::Debug < LogLevel::Info);
        assert!(LogLevel::Info >= LogLevel::Info);
        assert!(LogLevel::Warn >= LogLevel::Info);
        assert!(LogLevel::Critical >= LogLevel::Error);
    }
}
