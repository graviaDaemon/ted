use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use tokio::sync::mpsc::Sender;
use chrono::{NaiveDate, Utc};

struct LogState {
    file: std::fs::File,
    date: String,
}

static LOG_STATE: OnceLock<Mutex<LogState>> = OnceLock::new();
static LOG_TX: OnceLock<Sender<String>> = OnceLock::new();

fn log_path_for(date: &str) -> PathBuf {
    PathBuf::from(format!("logs/ted_{}.log", date))
}

fn archive_old_logs(retention: u32) {
    if retention == 0 {
        return;
    }
    if let Err(e) = std::fs::create_dir_all("logs/archive") {
        eprintln!("[LOGGER] Failed to create logs/archive: {}", e);
        return;
    }
    let entries = match std::fs::read_dir("logs") {
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
        let src = entry.path();
        let dest = PathBuf::from(format!("logs/archive/{}", name));
        if std::fs::rename(&src, &dest).is_err() {
            if std::fs::copy(&src, &dest).is_ok() {
                let _ = std::fs::remove_file(&src);
            } else {
                eprintln!("[LOGGER] Failed to archive {}", name);
            }
        }
    }
}

pub fn init(tx: Sender<String>, retention: u32) -> io::Result<()> {
    std::fs::create_dir_all("logs")?;
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
    Ok(())
}

pub fn log(source: &str, msg: &str) {
    let line = format!(
        "[{} UTC] {} {}\n",
        Utc::now().format("%Y-%m-%d %H:%M:%S"),
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
