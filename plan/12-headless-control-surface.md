# plan/12 — Headless mode, control surface, account circuit breaker

## Goal

Let T.E.D run unattended on a Linux VPS and be operated at runtime by an external
agent (Lara), while T.E.D itself enforces un-bypassable guardrails and an
account-wide monthly-loss circuit breaker. No new trading strategy — this is
operations plumbing over the existing engine/runner/dispatch code.

## Context

Source request: `requests/2026-09-headless-operator.md`. Companion (the agent that
drives this): `lara-raith/plan/2026-09-ted-operator.md`.

Grounding facts from the current code:

- `main.rs::dispatch()` / `dispatch_line()` / `send_control()` / `graceful_shutdown()`
  are already TUI-independent — they operate on `runner_txs` / `runner_handles`
  HashMaps and parse a command line via `Cli::try_parse_from`. A headless loop can
  reuse them verbatim.
- The **"halt new buys, keep exits" state already exists**: `check_risk()` sets
  `state.halted = true` and calls `cancel_live_buy_orders()` (cancels resting buys,
  leaves sell/exit ladder), and `process_tick()` early-returns while halted
  (runner/mod.rs:748-859). `RunnerControl::Resume` clears it (runner/mod.rs:705-714).
  The monthly breaker reuses this exact machine — it does not reinvent halting.
- `Config` deserializes from `config.json` with `#[serde(default)]` sub-structs, so a
  new `operator` section keeps existing configs valid (config/config.rs).
- DB already exposes `equity_baseline()`, `upsert_daily_rollup()`/`DailyRollup`,
  `recent_fills()`, `save/load_runner_state()` (storage/db.rs) — enough for
  month-to-date PnL and status without new trading data.
- `TuiEvent::Status` already carries per-runner equity/realized/unrealized/position/
  open_buys/open_sells/paused/halted/fees/lots/trend/pnl_7d (channels.rs:28-44) — the
  status endpoint caches these; no new runner instrumentation needed.

## Implementation plan

### 1. `src/commands/cli.rs` — process-launch flag + two new runner ops

- Add a **process-level** arg parse (separate from the line-parsing `Cli`, which
  stays as-is): `ted --headless` selects the headless loop; no flag = TUI as today.
  `main()` currently ignores process args, so this is additive.
- `RunnerCommand`: add `--finish-exits` (`-f`) flag → `CliAction::FinishExits { symbol }`
  (conflicts with pause/resume/kill). This is Lara's "retire this pair cleanly" verb.
- Add control-surface-only actions parsed from their own verbs (not wired to the TUI):
  `status` → `CliAction::Status`, `hold --clear` → `CliAction::ClearHold`. Keep them in
  the same `Cli` grammar so the control surface reuses `Cli::try_parse_from`.

### 2. `src/config/channels.rs` + `src/runner/mod.rs` — FinishExits + external halt

- `RunnerControl`: add `FinishExits` and `HaltBuys`.
- `HaltBuys` handler: set `state.halted = true` + `cancel_live_buy_orders()` — identical
  to the drawdown path, but triggered externally by the breaker. `Resume` already clears
  `halted`, so no resume changes needed.
- `FinishExits` handler: set a new `state.finishing = true`, cancel buys. Add a retire
  check in `process_fill()` (and periodic): when `finishing && position ≈ 0 && no open
  sells`, `save_state()` + `engine.unsubscribe()` + `RunnerStopped` + break — the runner
  exits itself once flat. Guard the ≈0 with the existing dust epsilon used elsewhere.
- `RunnerState`: add `finishing: bool` (default false; not persisted — a restart during
  finishing simply resumes normal, which is safe).

### 3. `src/config/config.rs` — `OperatorConfig` (all serde-default)

New optional `operator` section:
- `runners`: `Vec<RunnerSpec { symbol, algorithm, options: Map, mode }>` — the startup
  manifest (headless spawns these).
- `guardrails`: `max_monthly_loss_pct`, `max_capital_per_pair`, `whitelisted_pairs:
  Vec<String>`, `min_days_between_config_changes`.
- `control`: `{ bind: "127.0.0.1:PORT", token }` — loopback TCP + shared token
  (cross-platform; Lara runs on the same box). ponytail: loopback TCP over a unix
  socket to avoid a `#[cfg(unix)]` split with the Windows dev machine.
- `email`: `{ smtp_host, smtp_port, user, pass, from, to: Vec<String> }`.
- `ollama`: `{ endpoint, model }`.
Absent `operator` ⇒ headless refuses to start with a clear message; TUI unaffected.

### 4. `src/storage/db.rs` — persistent operator state

- New `operator_state` table (single row KV): `hold BOOL`, `hold_reason TEXT`,
  `hold_since TEXT`, `last_config_change TEXT`, `month_baseline_equity REAL`,
  `month_baseline_date TEXT`.
- Methods: `get_operator_state()`, `set_hold(reason)`, `clear_hold()`,
  `record_config_change(ts)`, `set_month_baseline(equity, date)`.
- The hold **must** persist: a restart while held must NOT auto-resume trading.

### 5. `src/operator/mod.rs` (new) — the headless supervisor

The headless loop (called from `main()` when `--headless`), owning the same
`runner_txs`/`runner_handles` maps:
1. **Startup:** open DB; read `operator_state`. Spawn each manifest runner by building
   its `runner -s … -a … -o …` line and calling `dispatch_line()`. **If `hold` is set,
   immediately `HaltBuys` every runner after spawn** (resume is a deliberate act).
2. **Status cache:** consume `tui_rx` (the `TuiEvent::Status` stream) into a
   `HashMap<symbol, Status>` for the status endpoint.
3. **Control surface** (`src/operator/control.rs`): tokio task on `control.bind`,
   token-checked, one command per connection: read line → **guardrail governor** (step 6)
   → forward `(line, reply)` to the supervisor over an mpsc → supervisor runs
   `dispatch_line()` (or builds a `status`/`clear-hold` JSON reply) → write reply.
4. **Circuit-breaker tick** (`src/operator/breaker.rs`): every N minutes compute
   month-to-date PnL% from `operator_state.month_baseline_equity` vs current wallet
   equity (roll the baseline on month change). If loss% > `max_monthly_loss_pct`:
   `HaltBuys` all runners, `set_hold("monthly loss …")`, fire the breach email. While
   held, `dispatch_line` resume/spawn is rejected by the governor until `clear-hold`.
5. **Monthly report tick:** on month rollover, compose + send the report email.
6. **SIGTERM/SIGINT:** `graceful_shutdown()` (existing) — persists resume state, leaves
   orders resting.

### 6. `src/operator/guardrails.rs` (new) — the un-bypassable governor

A pure function `check(action: &CliAction, cfg, db_state) -> Result<(), Reject>` run on
every control-surface command before dispatch:
- `Spawn`/`Configure` with a `capital`/budget option > `max_capital_per_pair` → reject.
- `Spawn` of a symbol not in `whitelisted_pairs` → reject.
- `Configure`/`Spawn` when `now - last_config_change < min_days_between_config_changes`
  → reject (anti-churn / overfit-chasing).
- `Resume` / `ClearHold` while `hold` set and month-to-date still beyond limit → reject.
- Everything reject-logged + returned to the caller (Lara surfaces it). The TUI path does
  **not** go through the governor (local human override stays).

### 7. `src/notify/mod.rs` (new) — email + narrative

- SMTP via `lettre` (new dep). No-op with a warning if `email` unconfigured.
- Ollama narrative via the existing HTTP client (`reqwest`): feed status + recent rollups,
  get prose for breach + monthly report. Never in the trade path.

## Testing (self-checks, paper/backtest only — no live before 2026-09-29)

- **Governor** (unit, pure fn): over-capital spawn, non-whitelisted pair, churn within
  min-days, resume-while-held all rejected; valid ones pass.
- **Breaker** (integration on paper/backtest): drive month-to-date past the limit → all
  runners `halted`, buys cancelled, sells intact, `hold` persisted, email attempted;
  resume blocked until `clear-hold`; restart-while-held re-halts on boot.
- **finish-exits** (paper): runner stops opening, works sells, retires itself at flat.
- **headless smoke:** `--headless` spawns the manifest, control commands round-trip,
  SIGTERM → graceful shutdown, relaunch resumes from `ted.db`.

## Rollout

- Phase 0 (now → 2026-09-29): build + all tests green in paper/backtest. No live config
  changes (respects the eval hold).
- Phase 1 (after 2026-09-29): deploy binary + `operator` config on the VPS under systemd
  (`Restart=on-failure`), Bitfinex key **with withdrawals disabled**, control bound to
  loopback. Start with the current live pair(s), breaker armed.

## Out of scope

- No LLM in the fast loop; no withdrawal/transfer capability anywhere.
- No new strategy or grid change.
- Interactive TUI unchanged — headless is an added run-mode.
- The strategic decision loop (sweep judgment, pair selection, re-tune cadence) lives in
  Lara (companion plan), not here. T.E.D only executes + guards.
