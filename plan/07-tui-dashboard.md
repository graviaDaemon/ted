# 07 — TUI dashboard: per-coin PnL bars, last-trades panel, tabbed logs (ratatui)

## Goal

Replace the log-scroll TUI with a two-view ratatui interface: a **Dashboard** view (left: per-coin
session-PnL bar graph; right: last trades per coin; bottom: the command line, with a one-line
warn/error notice above it) and a **Logs** view (the current scrollback), toggled with **Tab**.
The command line works identically in both views.

## Context

- **Source request:** `requests/2026-07-ui-and-gains.md` (the TUI half; the gains half becomes plan/08).
- **Decisions:** `plan/00-decisions.md`, section `2026-07-07 — TUI dashboard rewrite`.
- **Current state:** `src/tui.rs` is ~200 lines of hand-rolled crossterm cursor positioning: a
  scrolling log region, one ticker row (`[ SYM: bid | … ]`), a `> ` input line. `main.rs` runs a
  `select!` loop over three channels: crossterm `EventStream`, a log line channel, and a
  `(String, f64)` ticker channel. Both channels are fed from global `OnceLock` senders in
  `src/logger.rs` (`LOG_TX`, `TICKER_TX`; runners call `logger::update_ticker`).
- **Data already available (no new bookkeeping):**
  - Every fill funnels through `RunnerState::write_fill_to_db` (`src/runner/state.rs:50`) with
    direction, price, qty, and per-fill realized PnL.
  - Every snapshot tick, `RunnerState::persist_periodic` (`state.rs:115`) computes position,
    realized/unrealized PnL, equity, and open-order counts. It is called from the runner loop's
    snapshot-interval arm (`src/runner/mod.rs:542`).
  - SQLite `fills` table (joined to `runners` for the symbol) holds the full fill history.
- **Mode indicator:** `RunnerState.mode` (`RunnerMode::Paper`/`Live`/`Simulation`) is available at
  both hook points; show it in the dashboard so a paper session is never mistaken for live.

## Implementation plan

Ordered; each step compiles on its own.

### 1. Dependency: add ratatui, keep exactly one crossterm version — `Cargo.toml`

- Add `ratatui` (current stable release).
- ratatui pins its own crossterm. **There must be exactly one crossterm version in the tree** or
  `EventStream` events and ratatui's backend types won't interoperate. Do whichever of these the
  chosen ratatui version supports:
  - Preferred: drop the direct `crossterm` dependency and import everything (`event`, `terminal`,
    `cursor`, `execute!`/`queue!`) via `ratatui::crossterm` re-export, enabling ratatui's feature
    that turns on crossterm's `event-stream` if needed; or
  - Pin our explicit `crossterm = { version = "<same as ratatui's>", features = ["event-stream"] }`.
- Verify with `cargo tree -i crossterm` → exactly one version.

### 2. `TuiEvent` enum — `src/config/channels.rs`

The existing home for channel enums. Replace the raw `(String, f64)` ticker payload:

```rust
pub enum TuiEvent {
    Ticker { symbol: String, bid: f64 },
    Fill {
        symbol: String,
        is_buy: bool,
        qty: f64,
        price: f64,
        realized_pnl: Option<f64>,
        ts: DateTime<Utc>,
    },
    Status {
        symbol: String,
        mode: String,          // "paper" | "live" | "simulation" (reuse runner::mode_label)
        realized: f64,
        unrealized: f64,
        equity: f64,
        position: f64,
        open_buys: usize,
        open_sells: usize,
        paused: bool,
        halted: bool,
    },
    RunnerStopped { symbol: String },
}
```

### 3. Widen the global channel — `src/logger.rs`

- `TICKER_TX: OnceLock<Sender<(String, f64)>>` becomes `TUI_TX: OnceLock<Sender<TuiEvent>>`;
  `init_ticker` → `init_tui_events`.
- Keep `update_ticker(symbol, bid)` with its current signature (wraps `TuiEvent::Ticker`) so the
  engine call sites don't change. Add `notify_tui(event: TuiEvent)` for the rest. All sends stay
  `try_send` — a full channel drops the event rather than blocking a runner.

### 4. Emit events from the runner — `src/runner/state.rs`, `src/runner/mod.rs`

- **Fills:** at the top of `write_fill_to_db`, before the `db`/`runner_db_id` guard (so paper
  runners without a DB row still show trades), send `TuiEvent::Fill { .. }`.
- **Status:** restructure `persist_periodic` so the metric block (mid, position, realized,
  unrealized, equity, open counts — `state.rs:122-135`) is computed once when a price is known,
  a `TuiEvent::Status` is sent, and *then* the existing snapshot/rollup writes run only when
  `db`/`runner_db_id` are present. Today the fn returns early when `db` is `None`; the status
  event must not be lost to that early return. `paused`/`halted` come from the state fields.
- **Stop:** in the runner loop's exit paths (`mod.rs:546-568` — channel closed, `Shutdown`,
  `Kill`) send `TuiEvent::RunnerStopped` so the dashboard marks the symbol inactive (keep its
  last bar, dim it).

### 5. DB preload query — `src/storage/db.rs`

```rust
pub fn recent_fills(&self, limit: u32) -> Result<Vec<RecentFill>, rusqlite::Error>
// SELECT r.symbol, f.direction, f.price, f.quantity, f.realized_pnl, f.filled_at
// FROM fills f JOIN runners r ON r.id = f.runner_id
// ORDER BY f.id DESC LIMIT ?1
```

`RecentFill` is a small struct (symbol, direction, price, quantity, realized_pnl, filled_at).
Unit test against the in-memory test DB (same pattern as the existing tests in the file).

### 6. Rewrite `src/tui.rs` on ratatui

State held by `Tui`:

- `view: View` (`Dashboard` | `Logs`), toggled by `Tab`.
- `input_buf` / `cursor_pos` / `prompt` — keep the existing editing logic (chars, Backspace,
  Left/Right/Home/End, Enter submits, Ctrl+C/Ctrl+D exit) and the `handle_key -> Option<String>`
  contract so `main.rs` dispatch is untouched.
- `log_lines: VecDeque<String>` — **capped** (e.g. 5 000 lines; the current `Vec` grows unbounded
  over multi-week sessions). Plus a scroll offset for the Logs view (PgUp/PgDn; any new line while
  scrolled to bottom keeps autoscroll).
- `statuses: HashMap<String, StatusEntry>` — last `Status` per symbol + last ticker bid + an
  `active: bool` flipped by `RunnerStopped`.
- `trades: VecDeque<TradeEntry>` — capped (e.g. 200), preloaded at startup from
  `Db::recent_fills(50)` (newest first), then fed by `Fill` events.
- `last_alert: Option<String>` — most recent line whose level is WARN or above (match on the
  `] WARN `/`] ERROR `/`] CRITICAL ` marker in the formatted log line), shown on the dashboard.

Terminal lifecycle:

- `enter()`: raw mode + **alternate screen**, build `Terminal<CrosstermBackend<Stdout>>`.
- `exit()`: leave alternate screen, disable raw mode.
- Update the panic hook in `main.rs` (currently only disables raw mode, `main.rs:100-104`) to also
  leave the alternate screen.

Dashboard layout (ratatui `Layout`):

```
┌ PnL (session) ────────────┐┌ Last trades ──────────────────────────┐
│ SOLUSD  ██████▏  +$1.23   ││ 14:02:11 SOLUSD   SOLD  0.15 @ 163.20 │
│  paper · 163.20 · pos 0.3 ││          (+$0.12)                     │
│ XAUTUSD ▏███     -$0.41   ││ 13:58:40 XAUTUSD  BOUGHT 0.008 @ 3312 │
│  live · 3 312.4 · b3/s3   ││ …                                     │
└───────────────────────────┘└───────────────────────────────────────┘
 ⚠ [RUNNER:SOLUSD] Drawdown halt …            ← notice line (1 row)
─────────────────────────────────────────────────────────────────────
 > kill --runner SOLUSD                       ← input line
```

- Left panel (~40%): one entry per known symbol. Bar = session PnL (`realized + unrealized` from
  the last `Status`), horizontal, scaled to the max `|pnl|` currently shown, green for ≥ 0 / red
  for < 0, value label alongside. Second line per symbol: mode, last bid, position, `bN/sM` open
  orders; `halted`/`paused` flag when set; dimmed when `active == false`. Custom-rendered as
  styled `Line`s in a `Paragraph` — ratatui's `BarChart` is unsigned and can't show losses.
- Right panel: newest-first trades, `HH:MM:SS SYMBOL BOUGHT/SOLD qty @ price (±$realized)`,
  green/red by side, realized shown when `Some`.
- Notice line: `last_alert` in red/dim; empty line when none.
- Input line: prompt + buffer, ratatui cursor positioning.

Logs view: full-screen log scrollback (respecting scroll offset) above the same input line.

Rendering triggers: keep the current model — redraw after each handled event in the `main.rs`
loop. If fill/status bursts cause visible flicker, coalesce with a dirty-flag + ~100 ms tick, but
don't build that preemptively.

### 7. Wire `main.rs`

- `channel::<(String, f64)>` → `channel::<TuiEvent>`; `logger::init_ticker` →
  `logger::init_tui_events`.
- The `ticker_rx` select arm becomes a `tui_rx` arm calling `tui.handle_event(event)`.
- After `Tui::enter()`, open the DB (same path as the prune block, `main.rs:63`) and pass
  `recent_fills(50)` into the TUI for the trades preload; a DB error just logs a warn and starts
  the panel empty.

## Out of scope

- The gains/profitability work — that's plan/08 (diagnosis + backtest sweep + compounding).
- Web dashboard (still deferred per plan/00, 2026-06-18).
- New commands, changes to command syntax, or changes to the log *file* format/rotation.
- Historical PnL charts / time-series graphs — the bar graph shows current session PnL only.
- Mouse support, themes, configurable layouts.

## Validation

- `cargo build`, `cargo clippy`, `cargo test` all clean; existing tests untouched and passing.
- `cargo tree -i crossterm` shows exactly one crossterm version.
- New unit test: `Db::recent_fills` returns joined symbol + fill fields, newest first, respects
  the limit.
- Manual (paper mode): spawn a runner; confirm (a) its symbol appears in the PnL panel with mode
  `paper` and the bar/label updates on each snapshot tick, (b) a fill immediately appends to the
  trades panel with side/qty/price coloring, (c) Tab flips Dashboard ⇄ Logs and back, input line
  keeps working in both, (d) PgUp/PgDn scrolls logs and new lines resume autoscroll at the bottom,
  (e) a WARN log surfaces on the dashboard notice line, (f) resizing the terminal reflows both
  views without artifacts, (g) `kill` dims the symbol's dashboard entry, (h) exit (Ctrl+D) and a
  panic both restore the terminal (no raw-mode leak, shell scrollback intact), (i) restart shows
  the previous trades preloaded from the DB.
