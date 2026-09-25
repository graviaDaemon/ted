# plan/14a — Addendum: re-size on restart, honest `configure`, capital-true sweeps

## Goal

Close four gaps found while deploying plan/14 on 2026-09-25: a restarted runner can come
back latched idle; Lara's sweeps/applies carry a 10000 capital; Lara compounds from the
shared wallet; and `configure` over the control surface silently does nothing while reporting
success. Paired Lara changes are in §5–§6 of this file (one addendum for both repos; the Lara
plan `lara-raith/plan/2026-09-ted-operator-liveness.md` links here).

## Context

Found on the server after the plan/14 deploy (2026-09-25 ~20:42 UTC):

- Both runners restarted with 0 buys / 0 sells and `idle_since` set; a manual
  `runner -s SYM --rebuild` fixed both (XMR 3 × 0.045 @ ~555, SOL 3 × 0.233 @ ~121).
- Lara's first post-deploy pass tried to cold-start `XAUT:USD` with `capital 10000.00`
  (rejected by T.E.D: > `max_capital_per_pair` 200). User removed XAUT/XRP from the whitelist
  and set re-tune aside (`TED_OPERATOR_RETUNE` off) pending this addendum.

Grounding facts from current code:

1. **Restart latch** — `GridBot::restore_state` (grid.rs l.1394–1428) restores `sized`,
   `unfundable` *and* `last_price` from the saved blob. On the first tick after restart the
   ladder and exits are empty and `last_price` is `Some` → `on_tick` l.1034 calls `build_grid`,
   which skips sizing (`sized == true`) and returns early (`unfundable == true`) — forever. The
   plan/14 sizing fix therefore never runs for a resumed runner. `restore_legacy` (l.947–949)
   has the same flags. (plan/14 §5's "a restart is itself a rebuild" was wrong.)
2. **10000 capital** — `run_sweep_action` (main.rs) defaults capital to 10000 when neither
   `--capital` nor `--start-quote` is passed; `spawn_line()` (sweep.rs l.216) prints
   `capital={:.2}` into the `Top config →` line; Lara lifts every `key=value` off that line
   (`extractRecommendedOptions`) and spawns/configures with it. Lara never passes `--capital`.
3. **Double compounding** — `size_from_capital` already sizes from `capital + realized_pnl`
   (grid.rs). Lara's `reTune` additionally sets `capital = min(equity × 0.9, max_per_pair)`,
   where `equity` is `RunnerState::wallet_equity` = **account** quote total + this pair's base
   (state.rs l.91) — both runners report the same 401.98. Two runners would each be set to
   200 (= the whole wallet).
4. **`configure` is a silent no-op that burns the churn window** — `RunnerControl::SetAlgorithm`
   (runner/mod.rs ~l.740) calls `build_algorithm(name, options)` with Lara's raw options. Those
   carry `atr_multiplier`, not `spacing`; `GridBot::new` requires `spacing` (grid.rs l.209,
   injected from ATR only at spawn) → the runner logs "Failed to switch … keeping current".
   But `handle_control_command` has already replied `OK` and called `record_config_change`
   (main.rs ~l.742), so Lara logs "APPLIED" and the 7/14-day churn window starts for nothing.
   Even a *successful* swap would drop the lot book (fresh `GridBot`, no `restore_state`), lose
   the injected fees/wallet balances, orphan resting orders from the algorithm's view, and be
   undone by the next T.E.D restart (the headless manifest respawns with its own options).

## Implementation plan

### 1. T.E.D `grid.rs` — a restart always re-sizes a capital grid

- At the end of `restore_state` and `restore_legacy`: `if self.capital.is_some() { self.sized
  = false; }`. Leaves explicit-`qty` grids untouched. No other change: the soft-resume path
  (preserved buy ladder in range) still skips `build_grid`, so resting buys are not churned;
  the next build/rebuild re-sizes with plan/14 logic, which also clears a stale `unfundable`.
- Test `restored_unfundable_state_resizes_on_next_tick`: build a capital grid, force
  `unfundable = true`, `sized = true`, empty ladder, `serialize_state` → fresh `GridBot` →
  `restore_state` → `on_tick` emits buys and `!unfundable`. Fails on current code.

### 2. T.E.D — `configure` over the control surface returns `ERR` until it is designed

- `guardrails::check`: `CliAction::Configure` → `Err("configure is not supported over the
  control surface yet (see plan/15) — kill + spawn instead")`. TUI `configure` unchanged (a
  human sees the runner's log line).
- Consequence: `record_config_change` can no longer be stamped by a no-op.
- Test: `Configure` is rejected by `check` regardless of the churn window.
- **Out of this addendum:** a real in-place reconfigure (plan/15) — needs decisions on option
  merging (manifest vs saved options vs Lara's overlay), persistence across restart, and lot
  carry-over. Re-tune stays off until then.

### 3. T.E.D status — expose the runner's configured capital

- `TuiEvent::Status` / `StatusSnapshot` / `apply_tui_event` gain `capital: Option<f64>`
  (the parsed `capital` option; `None` for explicit-qty runners). Seed snapshot: `None`.
- Lets Lara sweep with the runner's real budget instead of guessing from shared equity.

### 4. T.E.D status — account free quote

- `status_json` gains account-level `quote_available: Option<f64>`: the `available` USD from
  any runner's latest wallet update (shared wallet → identical across runners). Threaded via a
  `quote_available: Option<f64>` on `TuiEvent::Status` / `StatusSnapshot` and read from the
  first runner in `status_json`. Needed by the cold-start budget gate (§6).

### 5. Lara — sweeps carry real capital; drop Lara-side compounding

- `ted.sweep(cfg, symbol, extra)` callers always pass `--capital <c>`:
  - re-tune: `c = runner.capital` (skip re-tune with a log line when `capital` is null);
  - cold-start: `c = guardrails.max_capital_per_pair`.
- `reTune`: remove the `compound` parameter and `nextCapital` use (T.E.D already compounds
  realized PnL into sizing). Before applying, assert the parsed `capital` option ≤
  `max_capital_per_pair` and equals the requested `c` (defence against parser drift).
- `TedRunnerStatus.capital: number | null`, `TedStatus.quote_available: number | null`.
- Re-tune still can't apply anything until plan/15 — `configureRunner` now gets a clean
  `ERR` (logged as "retune failed: …") instead of a fake `APPLIED`.

### 6. Lara — cold-start budget gate

- New config flag `TED_OPERATOR_COLDSTART`, default **off** (user decision 2026-09-25: cold-start
  stays off for now). When on, skip cold-start (log why) when `quote_available === null` or
  `quote_available < max_capital_per_pair`.

### 7. Tests

- T.E.D: §1 restore test; §2 configure-rejected guardrail test; existing suites green.
- Lara: re-tune/cold-start sweep command strings carry `--capital` (unit-test a small pure
  `sweepArgs(runner | null, guardrails)` helper); `reTune` refuses options whose `capital`
  exceeds the cap; cold-start skipped when `quote_available` is short.

## Out of scope

- plan/15 in-place `configure` (see §2).
- Tagging `[GRID]` log lines with the symbol (sweep replays are indistinguishable from live
  lines in `journalctl`) — noise, not correctness; separate small change.
- Per-runner equity attribution of the shared wallet.

## Validation

- `cargo build`, `cargo clippy --all-targets`, `cargo test`; `npm run typecheck`, `npm test`.
- Server: `systemctl restart ted` **without** a manual rebuild → within one tick both runners
  log "Sized from capital …" / "Grid built … N buys" (or soft-resume with buys intact), and
  `ted-status` shows `open_buys > 0`, `idle_since: null`, `capital` = 170 / 150.
- `runner -s SOLUSD -c grid -o levels=3` over the control surface → `ERR configure is not
  supported…`; `last_config_change` stays null.
- One manual `lara operate` with re-tune on: logs the sweep ran with `--capital 170/150` and a
  clean "retune failed: … configure is not supported" — no `APPLIED`, no config stamp.
