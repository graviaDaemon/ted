use rusqlite::{params, Connection};
use std::path::Path;

pub struct TickRow {
    pub runner_id: i64,
    pub ts: String,
    pub last_price: f64,
    pub bid: f64,
    pub ask: f64,
    pub volume: f64,
    pub has_signals: bool,
}

pub struct OrderRow {
    pub runner_id: i64,
    pub exchange_id: Option<i64>,
    pub direction: String,
    pub price: f64,
    pub quantity: f64,
    pub status: String,
    pub placed_at: String,
    pub filled_at: Option<String>,
}

pub struct FillRow {
    pub runner_id: i64,
    pub order_db_id: Option<i64>,
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
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS runners (
                id         INTEGER PRIMARY KEY AUTOINCREMENT,
                symbol     TEXT NOT NULL,
                algorithm  TEXT NOT NULL,
                mode       TEXT NOT NULL,
                started_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS ticks (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                runner_id   INTEGER NOT NULL,
                ts          TEXT NOT NULL,
                last_price  REAL NOT NULL,
                bid         REAL NOT NULL,
                ask         REAL NOT NULL,
                volume      REAL NOT NULL,
                has_signals INTEGER NOT NULL,
                FOREIGN KEY (runner_id) REFERENCES runners(id)
            );
            CREATE TABLE IF NOT EXISTS orders (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                runner_id   INTEGER NOT NULL,
                exchange_id INTEGER,
                direction   TEXT NOT NULL,
                price       REAL NOT NULL,
                quantity    REAL NOT NULL,
                status      TEXT NOT NULL,
                placed_at   TEXT NOT NULL,
                filled_at   TEXT,
                FOREIGN KEY (runner_id) REFERENCES runners(id)
            );
            CREATE TABLE IF NOT EXISTS fills (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                runner_id    INTEGER NOT NULL,
                order_db_id  INTEGER,
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

    pub fn insert_tick_batch(&self, rows: &[TickRow]) -> Result<(), rusqlite::Error> {
        let mut stmt = self.conn.prepare_cached(
            "INSERT INTO ticks (runner_id, ts, last_price, bid, ask, volume, has_signals) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        for row in rows {
            stmt.execute(params![
                row.runner_id,
                row.ts,
                row.last_price,
                row.bid,
                row.ask,
                row.volume,
                row.has_signals as i32,
            ])?;
        }
        Ok(())
    }

    pub fn insert_order(&self, row: &OrderRow) -> Result<i64, rusqlite::Error> {
        self.conn.execute(
            "INSERT INTO orders (runner_id, exchange_id, direction, price, quantity, status, placed_at, filled_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                row.runner_id,
                row.exchange_id,
                row.direction,
                row.price,
                row.quantity,
                row.status,
                row.placed_at,
                row.filled_at,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn update_order_status(
        &self,
        order_db_id: i64,
        status: &str,
        filled_at: Option<&str>,
    ) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "UPDATE orders SET status = ?1, filled_at = ?2 WHERE id = ?3",
            params![status, filled_at, order_db_id],
        )?;
        Ok(())
    }

    pub fn insert_fill(&self, row: &FillRow) -> Result<i64, rusqlite::Error> {
        self.conn.execute(
            "INSERT INTO fills (runner_id, order_db_id, exchange_id, direction, price, quantity, realized_pnl, filled_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                row.runner_id,
                row.order_db_id,
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

    pub fn query_ticks(&self, runner_id: i64) -> Result<Vec<TickRow>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "SELECT runner_id, ts, last_price, bid, ask, volume, has_signals FROM ticks WHERE runner_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![runner_id], |row| {
            Ok(TickRow {
                runner_id: row.get(0)?,
                ts: row.get(1)?,
                last_price: row.get(2)?,
                bid: row.get(3)?,
                ask: row.get(4)?,
                volume: row.get(5)?,
                has_signals: row.get::<_, i32>(6)? != 0,
            })
        })?;
        rows.collect()
    }

    pub fn query_orders(&self, runner_id: i64) -> Result<Vec<OrderRow>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "SELECT runner_id, exchange_id, direction, price, quantity, status, placed_at, filled_at FROM orders WHERE runner_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![runner_id], |row| {
            Ok(OrderRow {
                runner_id: row.get(0)?,
                exchange_id: row.get(1)?,
                direction: row.get(2)?,
                price: row.get(3)?,
                quantity: row.get(4)?,
                status: row.get(5)?,
                placed_at: row.get(6)?,
                filled_at: row.get(7)?,
            })
        })?;
        rows.collect()
    }

    pub fn query_fills(&self, runner_id: i64) -> Result<Vec<FillRow>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "SELECT runner_id, order_db_id, exchange_id, direction, price, quantity, realized_pnl, filled_at FROM fills WHERE runner_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![runner_id], |row| {
            Ok(FillRow {
                runner_id: row.get(0)?,
                order_db_id: row.get(1)?,
                exchange_id: row.get(2)?,
                direction: row.get(3)?,
                price: row.get(4)?,
                quantity: row.get(5)?,
                realized_pnl: row.get(6)?,
                filled_at: row.get(7)?,
            })
        })?;
        rows.collect()
    }
}
