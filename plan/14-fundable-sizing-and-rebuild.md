# plan/14 — Fundable sizing (kill the `unfundable` latch) + operator rebuild/alert verbs

## Goal

Stop a live grid runner from latching `unfundable` forever on a sizing knife-edge
(the 2026-09-19 → 09-25 six-day trading stall), and give the Lara operator two
risk-neutral control verbs — `--rebuild` and `alert` — plus an `idle_since` status
field so she can detect and nudge an "alive but not trading" runner.

## Context

Source: the 2026-09-25 incident handoff (vault note `ted-trading-stall-2026-09-25`;
no `requests/` file — user chose to plan straight from the handoff). Paired Lara
change: `lara-raith/plan/2026-09-ted-operator-liveness.md`.

Evidence (journal, 2026-09-19 10:43:29, XMRUSD, capital 150 / 3 levels):

> `Derived per-level notional 24.81 (qty 0.04212267 × midpoint 589.08) is below min_notional 25.00 — emitting no orders.`

Grounding facts from current code (`src/algorithm/grid.rs`):

- `size_from_capital` (l.620): `qty = min(qty_from_quote, initial_base_balance / levels)`
  (l.659–665). qty_from_quote = 75 / (3 × 589.08) = 0.04244 (→ 25.00), but
  0.04212267 × 3 = 0.12636801 = the held XMR base → **the held-base cap won** and
  set notional to 24.81. The cap is a pre-plan/09 relic: it sized the old symmetric
  sell ladder from inventory. Under the per-lot grid, sells are lot exits sized per
  lot (the held base is seeded as its own lot in `build_grid` l.723) — the cap has
  no remaining purpose.
- The cap is price-independent, so every re-center (`in_dead_zone` l.1066 sets
  `sized = false` and re-sizes) recomputes the same failing qty → `unfundable`
  forever, empty buy ladder, ~950 identical "re-centering buy ladder" log lines.
  Very likely the SOLUSD cause too (held SOL / 3 × price < 25).
- Second knife-edge: `round_qty` floors to 1e-8, so a level at exactly
  `min_notional` (XMR: 150 × 0.5 / 3 = 25.00, price-independent) fails the
  `qty * midpoint >= min_notional - 1e-9` check at l.668.
- Level reduction (l.632) only considers `buy_budget / min_notional`; when the
  derived qty then fails, it latches `unfundable` instead of trying fewer levels.

Control surface (`src/main.rs` `handle_control` l.650, `src/operator/guardrails.rs`):
`Configure`/`Spawn` are churn-gated (14 days) — so Lara cannot re-spawn a stuck
runner. Risk-reducing/neutral verbs (`Pause`/`Resume`/`FinishExits`/`Kill`) are
always allowed. Email exists only T.E.D-side (`notify::send_email`, used by the
breaker at main.rs:762).

## Implementation plan

### 1. `src/algorithm/grid.rs` — `size_from_capital` never latches on a knife-edge

- **Delete the held-base cap** (l.659–665): `qty = round_qty(buy_budget / (levels × midpoint))`.
- **Step levels down until a level clears the floor**: loop `levels` from
  `min(levels_requested, max_fundable)` down to 1; take the first whose rounded qty
  satisfies `qty * midpoint >= min_notional - 1e-9`. Only if none does (including
  levels = 1) set `unfundable`. Log the reduction as today's "funds only N of M"
  warn.
- Rounding: at levels = 1..N the loop naturally escapes the exact-25.00 edge
  (XMR 150/3 → 2 levels × 37.50). No change to `round_qty`.
- **Critical log on the transition only**: when `unfundable` flips false → true,
  `log_critical` ("runner cannot fund a single level … no orders will be placed");
  repeat re-sizes that stay unfundable keep the existing `log_warn`. Avoids 288
  critical lines/day from the re-center cooldown loop.

### 2. `src/algorithm/traits.rs` + `grid.rs` — `on_rebuild`

- Trait: `fn on_rebuild(&mut self) {}` (default no-op).
- Grid: `buy_orders.clear()`, `emitted_buy_prices.clear()`, `sized = false`,
  `last_rebuild_at = None`. Keeps `last_price`, lots and lot exits untouched → the
  next tick sees an empty buy ladder, `in_dead_zone` is true, cooldown is clear →
  immediate re-size + rebuild through the existing path.

### 3. `src/runner/mod.rs` + `RunnerControl` — `Rebuild`

- New `RunnerControl::Rebuild`: `cancel_live_buy_orders(&mut state, &engine).await`
  (exists), then `state.algorithm.on_rebuild()`, log "Rebuild received — buys
  cancelled, grid re-sized on next tick." Exits stay resting.

### 4. CLI + guardrails + dispatch — `runner -s SYM --rebuild`, `alert <msg>`

- `cli.rs`: `--rebuild` flag on `run` (conflicts with pause/resume/kill/finish),
  → `CliAction::Rebuild { symbol }`. New top-level `alert` subcommand taking the
  rest of the line as the message → `CliAction::Alert { message }`.
- `guardrails::check`: `Rebuild` and `Alert` → `Ok(())` (risk-neutral: same config,
  no capital change, no new exposure beyond the configured ladder). Not a config
  change → does not touch `last_config_change`.
- `main.rs dispatch`: `Rebuild` → `send_control(.., RunnerControl::Rebuild)`.
- `main.rs handle_control`: intercept `Alert` like `Status`/`ClearHold` — spawn
  `notify::send_email(op.email, "T.E.D operator alert", &message)` (no Ollama
  narration: Lara already wrote the text), return `OK alert sent`. TUI dispatch of
  `Alert` just logs it.

### 5. Status — `idle_since`

- `RunnerState` gains `idle_since: Option<DateTime<Utc>>` (in-memory only; not
  persisted — a T.E.D restart resets it, which is fine because a restart is itself
  a rebuild).
- At the status emission (`runner/state.rs` ~l.210): if `pending_buy_orders` and
  `pending_sell_orders` are both empty and not `finishing`, set
  `idle_since.get_or_insert(now)`; otherwise `None`.
- Thread `idle_since: Option<String>` (RFC 3339) through `TuiEvent::Status`,
  `operator::StatusSnapshot`, `apply_tui_event` and `status_json`.

### 6. Control-surface `sweep` returns the report and writes no file

Found 2026-09-25: over the control surface, `handle_control` dispatches `Sweep`
(main.rs:710), the dispatch arm (main.rs ~l.463–527) writes
`sweep_<SYM>_<date>[_N].md` into the process CWD via `write_sweep_report`
(backtest/sweep.rs:345), and the reply is a bare `"OK"` (main.rs:714). Lara never
receives the report → "sweep produced no recommended options" every pass (retune
and cold-start have never worked), and each pass leaves a file per swept pair
(user saw 20–30 on the server).

- Extract the body of the `CliAction::Sweep` dispatch arm into
  `async fn run_sweep_action(.., exchange) -> Result<SweepResult, String>`
  (candle load + `run_sweep`).
- TUI `dispatch`: unchanged behaviour — log `render_console()` lines + write the
  markdown report (a human asked for it).
- `handle_control`: intercept `CliAction::Sweep` like `Status`/`ClearHold`; call
  `run_sweep_action`, return `format!("OK\n{}", result.render_console())` or
  `ERR <msg>`. **No file written.** (Lara's parsers already target the console
  format: `rank … validated` header, rank-1 row, `atr_multiplier=` spawn line.)
- Check the reply fits Lara's 15 s socket timeout on the server's candle fetch; if
  not, the Lara plan raises its sweep timeout (not a T.E.D change).

### 7. Tests (assert-based, `#[cfg(test)]`)

- `xmr_knife_edge_steps_down_levels`: capital 150, levels 3, min_notional 25,
  midpoint 589.08, `initial_base_balance` 0.12636801 → not unfundable, 2 levels,
  qty × mid ≥ 25. (Regression for the 09-19 freeze; fails on current code.)
- `held_base_does_not_cap_qty`: capital 170, levels 3, midpoint 100, base 0.3 →
  qty == round_qty(85 / 300).
- `rebuild_resizes_and_rebuilds_ladder`: build, force `unfundable`/empty ladder,
  `on_rebuild()`, next `on_tick` emits buys.
- `guardrails`: `Rebuild` and `Alert` allowed inside the churn window.
- Existing tests stay green (`capital_below_one_level_is_unfundable`,
  `capital_reduces_levels_to_what_budget_funds`, `losses_beyond_budget_clamp_to_unfundable`).

## Out of scope

- `last_fill_at` in status (user: skip until a "resting-but-dead" case appears).
- Real Bitfinex `min_notional` lookup per pair (config value stays authoritative).
- The systemd `Failed to determine user credentials` unit error — ops, not code;
  check `ted.service` `User=` on the server.
- Scout/sweep changes: none needed — the sizing fix removes the need to avoid the
  exact-floor capital.

## Validation

- `cargo build`, `cargo clippy`, `cargo test` clean; new tests pass.
- On the server after deploy: `journalctl -u ted -f` shows both runners "Sized from
  capital … N levels/side" and "Grid built … N buys" (N > 0) within one tick;
  `status` over the control surface shows `open_buys > 0`, `idle_since: null`.
- `sweep -s XMRUSD` over the control surface returns the ranked table + `Top config →`
  line, and no new `sweep_*.md` appears in the working dir. One-off cleanup of the
  existing files = user: `rm sweep_*.md` in `ted.service`'s `WorkingDirectory`.
- `runner -s XMRUSD --rebuild` over the control surface returns OK inside the churn
  window; `alert test` delivers an email.
- Rollout = user: pull, `cargo build --release`, restart `ted.service` (resume
  state, **not** `--fresh` — the lots/exits must be kept).
