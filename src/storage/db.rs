use chrono::{Duration, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

pub struct FillRow {
    pub runner_id: i64,
    pub exchange_id: Option<i64>,
    pub direction: String,
    pub price: f64,
    pub quantity: f64,
    pub realized_pnl: Option<f64>,
    pub filled_at: String,
}

/// A fill joined to its runner's symbol, for the TUI last-trades preload.
pub struct RecentFill {
    pub symbol: String,
    pub direction: String,
    pub price: f64,
    pub quantity: f64,
    pub realized_pnl: Option<f64>,
    pub filled_at: String,
}

/// High-frequency per-runner sample. Pruned after `snapshot_retention_days`.
pub struct SnapshotRow {
    pub runner_id: i64,
    pub ts: String,
    pub mid: f64,
    pub bid: f64,
    pub ask: f64,
    pub position: f64,
    pub realized_pnl: f64,
    pub unrealized_pnl: f64,
    pub open_buys: i64,
    pub open_sells: i64,
    pub equity: f64,
}

/// One row per runner per UTC day. Kept long-term.
pub struct DailyRollup {
    pub runner_id: i64,
    pub day: String,
    pub realized_pnl: f64,
    pub fees: f64,
    pub trades: i64,
    pub ending_equity: f64,
    pub ending_position: f64,
}

/// Latest serialized resume blob for a symbol. One row per symbol, upserted.
pub struct RunnerStateRow {
    pub symbol: String,
    pub algorithm: String,
    pub mode: String,
    pub options: String,
    pub algo_state: Option<String>,
    pub pending_buys: String,
    pub pending_sells: String,
    pub updated_at: String,
}

pub struct Db {
    conn: Connection,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self, rusqlite::Error> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")?;
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS runners (
                id         INTEGER PRIMARY KEY AUTOINCREMENT,
                symbol     TEXT NOT NULL,
                algorithm  TEXT NOT NULL,
                mode       TEXT NOT NULL,
                started_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS fills (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                runner_id    INTEGER NOT NULL,
                exchange_id  INTEGER,
                direction    TEXT NOT NULL,
                price        REAL NOT NULL,
                quantity     REAL NOT NULL,
                realized_pnl REAL,
                filled_at    TEXT NOT NULL,
                FOREIGN KEY (runner_id) REFERENCES runners(id)
            );
            CREATE TABLE IF NOT EXISTS snapshots (
                id             INTEGER PRIMARY KEY AUTOINCREMENT,
                runner_id      INTEGER NOT NULL,
                ts             TEXT NOT NULL,
                mid            REAL NOT NULL,
                bid            REAL NOT NULL,
                ask            REAL NOT NULL,
                position       REAL NOT NULL,
                realized_pnl   REAL NOT NULL,
                unrealized_pnl REAL NOT NULL,
                open_buys      INTEGER NOT NULL,
                open_sells     INTEGER NOT NULL,
                equity         REAL NOT NULL,
                FOREIGN KEY (runner_id) REFERENCES runners(id)
            );
            CREATE INDEX IF NOT EXISTS idx_snapshots_runner_ts ON snapshots (runner_id, ts);
            CREATE TABLE IF NOT EXISTS daily_rollups (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                runner_id       INTEGER NOT NULL,
                day             TEXT NOT NULL,
                realized_pnl    REAL NOT NULL,
                fees            REAL NOT NULL,
                trades          INTEGER NOT NULL,
                ending_equity   REAL NOT NULL,
                ending_position REAL NOT NULL,
                UNIQUE (runner_id, day),
                FOREIGN KEY (runner_id) REFERENCES runners(id)
            );
            CREATE TABLE IF NOT EXISTS runner_state (
                symbol        TEXT PRIMARY KEY,
                algorithm     TEXT NOT NULL,
                mode          TEXT NOT NULL,
                options       TEXT NOT NULL,
                algo_state    TEXT,
                pending_buys  TEXT NOT NULL,
                pending_sells TEXT NOT NULL,
                updated_at    TEXT NOT NULL
            );
        ",
        )?;
        Ok(Db { conn })
    }

    pub fn insert_runner(
        &self,
        symbol: &str,
        algorithm: &str,
        mode: &str,
        started_at: &str,
    ) -> Result<i64, rusqlite::Error> {
        self.conn.execute(
            "INSERT INTO runners (symbol, algorithm, mode, started_at) VALUES (?1, ?2, ?3, ?4)",
            params![symbol, algorithm, mode, started_at],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn insert_fill(&self, row: &FillRow) -> Result<i64, rusqlite::Error> {
        self.conn.execute(
            "INSERT INTO fills (runner_id, exchange_id, direction, price, quantity, realized_pnl, filled_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                row.runner_id,
                row.exchange_id,
                row.direction,
                row.price,
                row.quantity,
                row.realized_pnl,
                row.filled_at,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    #[allow(dead_code)]
    pub fn query_fills(&self, runner_id: i64) -> Result<Vec<FillRow>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "SELECT runner_id, exchange_id, direction, price, quantity, realized_pnl, filled_at FROM fills WHERE runner_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![runner_id], |row| {
            Ok(FillRow {
                runner_id: row.get(0)?,
                exchange_id: row.get(1)?,
                direction: row.get(2)?,
                price: row.get(3)?,
                quantity: row.get(4)?,
                realized_pnl: row.get(5)?,
                filled_at: row.get(6)?,
            })
        })?;
        rows.collect()
    }

    /// Most recent fills across all runners, newest first, joined to the
    /// runner's symbol. Feeds the TUI last-trades preload at startup.
    pub fn recent_fills(&self, limit: u32) -> Result<Vec<RecentFill>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "SELECT r.symbol, f.direction, f.price, f.quantity, f.realized_pnl, f.filled_at \
             FROM fills f JOIN runners r ON r.id = f.runner_id \
             ORDER BY f.id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |row| {
            Ok(RecentFill {
                symbol: row.get(0)?,
                direction: row.get(1)?,
                price: row.get(2)?,
                quantity: row.get(3)?,
                realized_pnl: row.get(4)?,
                filled_at: row.get(5)?,
            })
        })?;
        rows.collect()
    }

    pub fn insert_snapshot(&self, row: &SnapshotRow) -> Result<i64, rusqlite::Error> {
        self.conn.execute(
            "INSERT INTO snapshots (runner_id, ts, mid, bid, ask, position, realized_pnl, unrealized_pnl, open_buys, open_sells, equity) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                row.runner_id,
                row.ts,
                row.mid,
                row.bid,
                row.ask,
                row.position,
                row.realized_pnl,
                row.unrealized_pnl,
                row.open_buys,
                row.open_sells,
                row.equity,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn upsert_daily_rollup(&self, row: &DailyRollup) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "INSERT INTO daily_rollups (runner_id, day, realized_pnl, fees, trades, ending_equity, ending_position) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
             ON CONFLICT (runner_id, day) DO UPDATE SET \
                 realized_pnl = excluded.realized_pnl, \
                 fees = excluded.fees, \
                 trades = excluded.trades, \
                 ending_equity = excluded.ending_equity, \
                 ending_position = excluded.ending_position",
            params![
                row.runner_id,
                row.day,
                row.realized_pnl,
                row.fees,
                row.trades,
                row.ending_equity,
                row.ending_position,
            ],
        )?;
        Ok(())
    }

    /// Oldest daily-rollup equity within the trailing `days` window for a
    /// symbol, joined across that symbol's runner sessions — the baseline for
    /// the TUI's trailing PnL% readout (plan/10).
    pub fn equity_baseline(
        &self,
        symbol: &str,
        days: u32,
    ) -> Result<Option<(String, f64)>, rusqlite::Error> {
        let cutoff = (Utc::now() - Duration::days(days as i64))
            .format("%Y-%m-%d")
            .to_string();
        self.conn
            .query_row(
                "SELECT d.day, d.ending_equity FROM daily_rollups d \
                 JOIN runners r ON r.id = d.runner_id \
                 WHERE r.symbol = ?1 AND d.day >= ?2 \
                 ORDER BY d.day ASC, d.id ASC LIMIT 1",
                params![symbol, cutoff],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
    }

    pub fn save_runner_state(&self, row: &RunnerStateRow) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "INSERT INTO runner_state (symbol, algorithm, mode, options, algo_state, pending_buys, pending_sells, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
             ON CONFLICT (symbol) DO UPDATE SET \
                 algorithm = excluded.algorithm, \
                 mode = excluded.mode, \
                 options = excluded.options, \
                 algo_state = excluded.algo_state, \
                 pending_buys = excluded.pending_buys, \
                 pending_sells = excluded.pending_sells, \
                 updated_at = excluded.updated_at",
            params![
                row.symbol,
                row.algorithm,
                row.mode,
                row.options,
                row.algo_state,
                row.pending_buys,
                row.pending_sells,
                row.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn load_runner_state(
        &self,
        symbol: &str,
    ) -> Result<Option<RunnerStateRow>, rusqlite::Error> {
        self.conn
            .query_row(
                "SELECT symbol, algorithm, mode, options, algo_state, pending_buys, pending_sells, updated_at \
                 FROM runner_state WHERE symbol = ?1",
                params![symbol],
                |row| {
                    Ok(RunnerStateRow {
                        symbol: row.get(0)?,
                        algorithm: row.get(1)?,
                        mode: row.get(2)?,
                        options: row.get(3)?,
                        algo_state: row.get(4)?,
                        pending_buys: row.get(5)?,
                        pending_sells: row.get(6)?,
                        updated_at: row.get(7)?,
                    })
                },
            )
            .optional()
    }

    pub fn clear_runner_state(&self, symbol: &str) -> Result<usize, rusqlite::Error> {
        self.conn
            .execute("DELETE FROM runner_state WHERE symbol = ?1", params![symbol])
    }

    /// Delete snapshot rows older than `older_than_days`. Fills and rollups are
    /// never touched. Returns the number of rows removed.
    pub fn prune_snapshots(&self, older_than_days: u32) -> Result<usize, rusqlite::Error> {
        let cutoff = (Utc::now() - Duration::days(older_than_days as i64)).to_rfc3339();
        self.conn
            .execute("DELETE FROM snapshots WHERE ts < ?1", params![cutoff])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_db() -> Db {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE runners (
                id INTEGER PRIMARY KEY AUTOINCREMENT, symbol TEXT NOT NULL,
                algorithm TEXT NOT NULL, mode TEXT NOT NULL, started_at TEXT NOT NULL
            );
            CREATE TABLE snapshots (
                id INTEGER PRIMARY KEY AUTOINCREMENT, runner_id INTEGER NOT NULL, ts TEXT NOT NULL,
                mid REAL NOT NULL, bid REAL NOT NULL, ask REAL NOT NULL, position REAL NOT NULL,
                realized_pnl REAL NOT NULL, unrealized_pnl REAL NOT NULL, open_buys INTEGER NOT NULL,
                open_sells INTEGER NOT NULL, equity REAL NOT NULL
            );
            CREATE TABLE fills (
                id INTEGER PRIMARY KEY AUTOINCREMENT, runner_id INTEGER NOT NULL, exchange_id INTEGER,
                direction TEXT NOT NULL, price REAL NOT NULL, quantity REAL NOT NULL,
                realized_pnl REAL, filled_at TEXT NOT NULL
            );
            CREATE TABLE daily_rollups (
                id INTEGER PRIMARY KEY AUTOINCREMENT, runner_id INTEGER NOT NULL, day TEXT NOT NULL,
                realized_pnl REAL NOT NULL, fees REAL NOT NULL, trades INTEGER NOT NULL,
                ending_equity REAL NOT NULL, ending_position REAL NOT NULL, UNIQUE (runner_id, day)
            );
            CREATE TABLE runner_state (
                symbol TEXT PRIMARY KEY, algorithm TEXT NOT NULL, mode TEXT NOT NULL,
                options TEXT NOT NULL, algo_state TEXT, pending_buys TEXT NOT NULL,
                pending_sells TEXT NOT NULL, updated_at TEXT NOT NULL
            );
        ",
        )
        .unwrap();
        Db { conn }
    }

    fn snap(runner_id: i64, ts: &str) -> SnapshotRow {
        SnapshotRow {
            runner_id,
            ts: ts.to_string(),
            mid: 100.0,
            bid: 99.0,
            ask: 101.0,
            position: 1.0,
            realized_pnl: 5.0,
            unrealized_pnl: 1.0,
            open_buys: 2,
            open_sells: 3,
            equity: 1000.0,
        }
    }

    #[test]
    fn runner_state_round_trips() {
        let db = mem_db();
        let row = RunnerStateRow {
            symbol: "tBTCUSD".to_string(),
            algorithm: "grid".to_string(),
            mode: "paper".to_string(),
            options: r#"{"levels":"3"}"#.to_string(),
            algo_state: Some(r#"{"spacing":10.0}"#.to_string()),
            pending_buys: r#"{"1":[100.0,0.5]}"#.to_string(),
            pending_sells: r#"{"2":[110.0,0.5]}"#.to_string(),
            updated_at: "2026-06-18T00:00:00Z".to_string(),
        };
        db.save_runner_state(&row).unwrap();

        let loaded = db.load_runner_state("tBTCUSD").unwrap().unwrap();
        assert_eq!(loaded.algorithm, "grid");
        assert_eq!(loaded.mode, "paper");
        assert_eq!(loaded.options, row.options);
        assert_eq!(loaded.algo_state.as_deref(), Some(r#"{"spacing":10.0}"#));
        assert_eq!(loaded.pending_buys, row.pending_buys);
        assert_eq!(loaded.pending_sells, row.pending_sells);

        // Upsert overwrites in place rather than duplicating.
        let mut row2 = row;
        row2.mode = "live".to_string();
        db.save_runner_state(&row2).unwrap();
        let loaded = db.load_runner_state("tBTCUSD").unwrap().unwrap();
        assert_eq!(loaded.mode, "live");

        db.clear_runner_state("tBTCUSD").unwrap();
        assert!(db.load_runner_state("tBTCUSD").unwrap().is_none());
    }

    #[test]
    fn prune_snapshots_respects_cutoff_and_spares_fills_rollups() {
        let db = mem_db();
        let old_ts = (Utc::now() - Duration::days(40)).to_rfc3339();
        let fresh_ts = Utc::now().to_rfc3339();
        db.insert_snapshot(&snap(1, &old_ts)).unwrap();
        db.insert_snapshot(&snap(1, &fresh_ts)).unwrap();
        db.insert_fill(&FillRow {
            runner_id: 1,
            exchange_id: None,
            direction: "buy".to_string(),
            price: 100.0,
            quantity: 1.0,
            realized_pnl: Some(0.0),
            filled_at: old_ts.clone(),
        })
        .unwrap();
        db.upsert_daily_rollup(&DailyRollup {
            runner_id: 1,
            day: "2026-01-01".to_string(),
            realized_pnl: 1.0,
            fees: 0.0,
            trades: 1,
            ending_equity: 1.0,
            ending_position: 0.0,
        })
        .unwrap();

        let removed = db.prune_snapshots(30).unwrap();
        assert_eq!(removed, 1, "only the 40-day-old snapshot should be pruned");

        let remaining: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM snapshots", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 1);
        let fills: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM fills", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fills, 1, "fills must be untouched");
        let rollups: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM daily_rollups", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rollups, 1, "rollups must be untouched");
    }

    #[test]
    fn open_migrates_legacy_db_in_place() {
        // Simulate a ted.db from before this changeset: only runners + fills.
        let mut path = std::env::temp_dir();
        path.push(format!("ted_migrate_test_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE runners (id INTEGER PRIMARY KEY AUTOINCREMENT, symbol TEXT NOT NULL, algorithm TEXT NOT NULL, mode TEXT NOT NULL, started_at TEXT NOT NULL);
                 CREATE TABLE fills (id INTEGER PRIMARY KEY AUTOINCREMENT, runner_id INTEGER NOT NULL, exchange_id INTEGER, direction TEXT NOT NULL, price REAL NOT NULL, quantity REAL NOT NULL, realized_pnl REAL, filled_at TEXT NOT NULL);
                 INSERT INTO fills (runner_id, direction, price, quantity, filled_at) VALUES (1, 'buy', 100.0, 1.0, '2026-01-01T00:00:00Z');",
            )
            .unwrap();
        }

        // Opening with the new code must add the new tables without error and
        // leave the existing fill intact.
        let db = Db::open(&path).unwrap();
        let fills: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM fills", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fills, 1, "legacy data must survive migration");
        let runner_id = db
            .insert_runner("tBTCUSD", "grid", "paper", "2026-06-18T00:00:00Z")
            .unwrap();
        db.insert_snapshot(&snap(runner_id, &Utc::now().to_rfc3339()))
            .unwrap();
        db.save_runner_state(&RunnerStateRow {
            symbol: "tBTCUSD".to_string(),
            algorithm: "grid".to_string(),
            mode: "paper".to_string(),
            options: "{}".to_string(),
            algo_state: None,
            pending_buys: "{}".to_string(),
            pending_sells: "{}".to_string(),
            updated_at: Utc::now().to_rfc3339(),
        })
        .unwrap();

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn recent_fills_joins_symbol_newest_first_and_respects_limit() {
        let db = mem_db();
        let sol_id = db
            .insert_runner("tSOLUSD", "grid", "paper", "2026-07-07T00:00:00Z")
            .unwrap();
        let xaut_id = db
            .insert_runner("tXAUT:USD", "grid", "live", "2026-07-07T00:00:00Z")
            .unwrap();
        let fills = [
            (sol_id, "buy", 160.0, 0.5, Some(0.0), "2026-07-07T10:00:00Z"),
            (xaut_id, "buy", 3300.0, 0.01, None, "2026-07-07T11:00:00Z"),
            (sol_id, "sell", 163.2, 0.5, Some(1.6), "2026-07-07T12:00:00Z"),
        ];
        for (runner_id, direction, price, quantity, realized_pnl, filled_at) in fills {
            db.insert_fill(&FillRow {
                runner_id,
                exchange_id: None,
                direction: direction.to_string(),
                price,
                quantity,
                realized_pnl,
                filled_at: filled_at.to_string(),
            })
            .unwrap();
        }

        let recent = db.recent_fills(2).unwrap();
        assert_eq!(recent.len(), 2, "limit must cap the result");
        assert_eq!(recent[0].symbol, "tSOLUSD");
        assert_eq!(recent[0].direction, "sell");
        assert_eq!(recent[0].price, 163.2);
        assert_eq!(recent[0].quantity, 0.5);
        assert_eq!(recent[0].realized_pnl, Some(1.6));
        assert_eq!(recent[0].filled_at, "2026-07-07T12:00:00Z");
        assert_eq!(recent[1].symbol, "tXAUT:USD");
        assert_eq!(recent[1].realized_pnl, None);
    }

    #[test]
    fn daily_rollup_upsert_overwrites() {
        let db = mem_db();
        let mut roll = DailyRollup {
            runner_id: 1,
            day: "2026-06-18".to_string(),
            realized_pnl: 1.0,
            fees: 0.1,
            trades: 2,
            ending_equity: 100.0,
            ending_position: 0.5,
        };
        db.upsert_daily_rollup(&roll).unwrap();
        roll.realized_pnl = 9.0;
        roll.trades = 7;
        db.upsert_daily_rollup(&roll).unwrap();

        let (pnl, trades, count): (f64, i64, i64) = db
            .conn
            .query_row(
                "SELECT realized_pnl, trades, (SELECT COUNT(*) FROM daily_rollups) FROM daily_rollups",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(count, 1, "upsert must not duplicate the day");
        assert_eq!(pnl, 9.0);
        assert_eq!(trades, 7);
    }
}
