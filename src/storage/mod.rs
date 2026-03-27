use std::fs::OpenOptions;
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use crate::runner::trade_log::TradeEntry;

/// Retention period for active trade log files before rotation.
const ROTATION_AGE_SECS: u64 = 7 * 24 * 3600; // 7 days

/// Append-only JSONL store for trade history.
///
/// Each call to `append` writes one JSON object followed by a newline.
/// The file is created (or opened for appending) at construction time;
/// history survives process restarts.
pub struct TradeStore {
    writer: BufWriter<std::fs::File>,
    pub path: PathBuf,
}

impl TradeStore {
    /// Open (or create) the JSONL file for `symbol`.
    /// Rotates the file if it is older than 7 days before opening.
    /// File name: `trades_<SYMBOL>.jsonl` in the current working directory.
    pub fn open(symbol: &str) -> io::Result<Self> {
        rotate_trades_if_needed(symbol)?;
        let path = PathBuf::from(format!("trades_{}.jsonl", symbol.replace(':', "_")));
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        Ok(TradeStore {
            writer: BufWriter::new(file),
            path,
        })
    }

    /// Serialise `entry` to JSON and append it as a single line.
    pub fn append(&mut self, entry: &TradeEntry) -> io::Result<()> {
        let line = serde_json::to_string(entry)
            .map_err(|e| io::Error::other(e.to_string()))?;
        writeln!(self.writer, "{}", line)?;
        self.writer.flush()
    }
}

/// Rotates `trades_<symbol>.jsonl` if older than 7 days.
/// Archives to `trades_<symbol>_<YYYY-MM-DD>.jsonl` and deletes archives older than 7 days.
fn rotate_trades_if_needed(symbol: &str) -> io::Result<()> {
    let safe_sym = symbol.replace(':', "_");
    let active   = PathBuf::from(format!("trades_{}.jsonl", safe_sym));
    if !active.exists() {
        return Ok(());
    }

    let age = active
        .metadata()?
        .modified()?
        .elapsed()
        .unwrap_or(std::time::Duration::ZERO);

    if age > std::time::Duration::from_secs(ROTATION_AGE_SECS) {
        let archive = format!(
            "trades_{}_{}.jsonl",
            safe_sym,
            chrono::Utc::now().format("%Y-%m-%d")
        );
        std::fs::rename(&active, &archive)?;
        crate::logger::log(
            "[STORE]",
            &format!("Rotated {} → {}", active.display(), archive),
        );

        // Sweep: delete archived files older than 7 days.
        if let Ok(entries) = std::fs::read_dir(".") {
            let prefix = format!("trades_{}_", safe_sym);
            for entry in entries.filter_map(|e| e.ok()) {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with(&prefix) && name.ends_with(".jsonl") && name != archive {
                    let old = entry
                        .metadata()
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .and_then(|t| t.elapsed().ok())
                        .unwrap_or(std::time::Duration::ZERO);
                    if old > std::time::Duration::from_secs(ROTATION_AGE_SECS) {
                        let _ = std::fs::remove_file(entry.path());
                    }
                }
            }
        }
    }

    Ok(())
}
