use rusqlite::{params, Connection};
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
}
