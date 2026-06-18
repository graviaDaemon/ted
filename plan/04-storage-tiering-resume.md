# 04 — Storage tiering, retention, and runner/strategy state resume

## Goal

Persist enough time-series to power analytics (and a future read-only dashboard) without letting the
SQLite DB grow unbounded, and persist runner + strategy state so a runner resumes exactly where it
left off after a rebuild/restart/crash.

## Context

- Source request: `requests/2026-06-initialization.md`. Decisions: `plan/00-decisions.md` (2026-06-18).
- Depends on **plan/01** (`snapshot_retention_days` config) and **plan/02** (`position()`,
  `realized_pnl`/avg-cost on the algorithm).
- Current DB (`src/storage/db.rs`) has only `runners(id, symbol, algorithm, mode, started_at)` and
  `fills(id, runner_id, exchange_id, direction, price, quantity, realized_pnl, filled_at)`. WAL is on.
  `realized_pnl` is currently always written `None` (`write_fill_to_db`). No retention/pruning anywhere.
- `data_dir()/ted.db` is the single DB, opened per runner in `run_runner`. JSONL `TradeStore` per
  symbol also exists (legacy) with its own 7-day rotation — leave it, it's independent.
- The README claims a `ticks`/`orders` schema that does not exist; this changeset makes the schema
  real (snapshots + rollups) and updates the README.

## Implementation plan

### 1. Schema additions — `src/storage/db.rs`

Add tables (all `CREATE TABLE IF NOT EXISTS`, so existing DBs upgrade in place):

- `snapshots` — high-frequency per-runner samples (pruned):
  `id, runner_id, ts TEXT, mid REAL, bid REAL, ask REAL, position REAL, realized_pnl REAL,
   unrealized_pnl REAL, open_buys INTEGER, open_sells INTEGER, equity REAL`. Index on `(runner_id, ts)`.
- `daily_rollups` — one row per runner per UTC day (kept long-term):
  `id, runner_id, day TEXT, realized_pnl REAL, fees REAL, trades INTEGER, ending_equity REAL,
   ending_position REAL`. Unique `(runner_id, day)` — upsert.
- `runner_state` — latest serialized resume blob per symbol (kept, one row per symbol, upsert):
  `symbol TEXT PRIMARY KEY, algorithm TEXT, mode TEXT, options TEXT (json), algo_state TEXT (json),
   pending_buys TEXT (json), pending_sells TEXT (json), updated_at TEXT`.
- Backfill: also start writing real `realized_pnl` into `fills` (plan/02 makes it available).

Add `Db` methods: `insert_snapshot`, `upsert_daily_rollup`, `save_runner_state`, `load_runner_state`,
`prune_snapshots(older_than_days)`, plus a `clear_runner_state(symbol)` for clean shutdown/kill.

### 2. Snapshot writing — `src/runner/mod.rs`

- In the runner loop, add a periodic snapshot interval (e.g. configurable `snapshot_interval_secs`,
  default `60`; reuse the existing `tokio::time::interval` pattern alongside the ATR interval). On each
  tick of it, write a `snapshots` row from current state (mid from last bid/ask, `position()` from the
  algorithm, realized/unrealized PnL, open order counts, equity). Gate snapshots to non-Simulation
  modes, or write them in all modes — recommended: all modes except pure Simulation.
- Maintain an in-memory running daily aggregate and upsert `daily_rollups` at UTC-day boundaries and on
  shutdown.

### 3. Retention pruning

- On `Db::open` (or once at startup in `main.rs`), call `prune_snapshots(snapshot_retention_days)` and
  `VACUUM`-lite (avoid full `VACUUM` on every start; just delete old rows — WAL keeps file size
  manageable, optionally `PRAGMA incremental_vacuum`). Fills and rollups are never pruned.
- Make `snapshot_retention_days` (from plan/01 config) the cutoff. Log how many rows were pruned at
  `info`.

### 4. State persistence + resume

- **Serialize algorithm state.** Add to the `Algorithm` trait (default impls so non-grid algos still
  compile):
  - `fn serialize_state(&self) -> Option<String> { None }` — JSON of internal state.
  - `fn restore_state(&mut self, _json: &str) {}` — best-effort load.
  Implement real ones for `GridBot` (serialize `buy_orders`, `sell_orders`, bounds, `position_qty`,
  `position_cost`, `realized_pnl`, counters, fees, EMA). Rhai `script` algo returns `None`
  (best-effort: resume re-adopts open orders + options only — documented limitation).
- **Save on change/shutdown.** In `run_runner`, after fills/order changes and on Kill/EngineShutdown,
  call `save_runner_state(symbol, algo_name, mode, options_json, algo.serialize_state(),
  pending_buys_json, pending_sells_json)`. Throttle saves (e.g. on every fill + every snapshot tick) to
  avoid write amplification.
- **Load on spawn.** In `run_runner`, before `build_algorithm`, attempt `load_runner_state(symbol)`. If
  a row exists and `algorithm`+`mode` match the requested spawn (and the user didn't pass a fresh
  `--fresh` flag — add it to `RunnerCommand`):
  - rebuild the algorithm, then `restore_state(json)`,
  - restore `pending_buy_orders`/`pending_sell_orders`,
  - then run the existing reconcile path (`reconcile::sync_orders_after_reconnect` / the
    `AuthConnected` flow) so restored pending orders are reconciled against the exchange's actual open
    orders — this catches fills that happened while the process was down.
  - The grid's existing "soft resume" in `on_tick` (re-adopt intact grid if price still in range)
    complements this; ensure restored in-memory orders feed that logic rather than triggering a rebuild.
- **Clear on clean kill.** `RunnerControl::Kill` (intentional stop) → `clear_runner_state(symbol)` after
  cancelling orders, so a deliberate stop doesn't auto-resume next launch. EngineShutdown / crash leaves
  the row for resume. (Confirm this matches the user's mental model: kill = forget, crash/rebuild =
  resume.)

### 5. Reconciliation hardening (gotcha)

- The snapshot-diff "assumed filled" logic (`runner/mod.rs` `OrderSnapshot` handler) can book a phantom
  fill if an order was cancelled out-of-band. On resume, prefer `fetch_order_history` to distinguish
  filled vs cancelled for restored pending orders where possible, rather than blindly assuming fill.
  Keep this conservative — document any remaining risk in code comments.

## Out of scope

- The web dashboard itself (deferred). Schema is designed to be dashboard-ready (snapshots for charts,
  rollups for summaries, fills for the trade blotter).
- Migrating away from SQLite.
- Removing the legacy JSONL `TradeStore`.

## Validation

- `cargo build` / `cargo clippy` clean.
- Schema migration: open an existing `ted.db` from before this change; confirm new tables are created
  and old data intact.
- Unit tests: `save_runner_state` → `load_runner_state` round-trips options/pending/algo_state;
  `prune_snapshots` deletes only rows older than cutoff and leaves fills/rollups untouched.
- `GridBot` `serialize_state` → `restore_state` round-trip reproduces identical order maps and PnL.
- Manual resume test: start a paper grid, let it place orders and take a fill, kill the **process**
  (not `--kill`), restart, spawn the same runner → confirm it restores orders, reconciles against the
  exchange, and continues without rebuilding from scratch. Then `--kill` and confirm the state row is
  cleared (no resume on next launch).
- Manual retention test: set `snapshot_retention_days=0` (or small), run, confirm old snapshot rows are
  pruned and DB file size stays bounded while fills/rollups persist.
