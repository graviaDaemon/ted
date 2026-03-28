use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use tokio::sync::mpsc::Sender;
use chrono::Utc;

pub fn rotate_log(path: &str) {
    let active = PathBuf::from(path);
    if !active.exists() {
        return;
    }
    let age = active
        .metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.elapsed().ok())
        .unwrap_or(std::time::Duration::ZERO);

    if age > std::time::Duration::from_secs(24 * 3600) {
        let stem = active
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("ted");
        let archive = format!("{}_{}.log", stem, Utc::now().format("%Y-%m-%d"));
        if let Err(e) = std::fs::rename(&active, &archive) {
            eprintln!("[LOGGER] Failed to rotate {}: {}", path, e);
            return;
        }

        let prefix = format!("{}_", stem);
        if let Ok(entries) = std::fs::read_dir(".") {
            for entry in entries.filter_map(|e| e.ok()) {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with(&prefix) && name.ends_with(".log") && name != archive {
                    let old = entry
                        .metadata()
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .and_then(|t| t.elapsed().ok())
                        .unwrap_or(std::time::Duration::ZERO);
                    if old > std::time::Duration::from_secs(7 * 24 * 3600) {
                        let _ = std::fs::remove_file(entry.path());
                    }
                }
            }
        }
    }
}

static LOG_FILE: OnceLock<Mutex<std::fs::File>> = OnceLock::new();
static LOG_TX: OnceLock<Sender<String>> = OnceLock::new();

pub fn init(path: &str, tx: Sender<String>) -> io::Result<()> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    LOG_FILE
        .set(Mutex::new(file))
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

    if let Some(lock) = LOG_FILE.get()
        && let Ok(mut file) = lock.lock()
    {
        if let Err(e) = file.write_all(line.as_bytes()) {
            eprintln!("[LOGGER WRITE ERROR] {}: {}", e, line.trim());
        }
    }
}
