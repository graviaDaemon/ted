# 08 — Grid gains: ungate exits, rework trend filter, fill-less re-centering, compounding, sweep

## Goal

Make the grid trade *continuously* instead of decaying one-sided and deadlocking: counter-orders
(exits) are never suppressed, the trend filter only blocks extension buys against a real downtrend,
a stale grid re-centers on the price without needing a fill, sizing compounds net realized PnL,
and a new `sweep` command picks spacing/levels from backtested evidence instead of heuristics.

## Context

- **Source request:** `requests/2026-07-ui-and-gains.md` (the gains half; the TUI half is plan/07 —
  the two changesets are independent and can be implemented in either order).
- **Decisions:** `plan/00-decisions.md`, section `2026-07-07 — Gains round 3`.
- **Diagnosis data:** `requests/history/` (ted.db + WAL, logs, trades; server state as of 2026-07-07).

### Empirical diagnosis (evidence for each change below)

One live SOLUSD runner since plan/06 deployed (runner 40: 06-23→06-29; runner 41: 06-29→now).
**Plan/06's fixes all verified working** — capital-derived qty, two-sided initial build (2 buys +
2 sells on 06-29), no phantom shorts, zero `Insufficient USD` skips, risk options set. Result:
**+$8.25 realized, +$10.12 unrealized, ~$333 equity** (incl. the +$100 deposit ~06-29). But:

1. **Counter-orders were trend-gated at fill time and never retried.** `on_fill` places the
   counter-sell only if `sells_enabled(current_price)` at that instant (`grid.rs:728`), the
   counter-buy only if `buys_enabled` (`grid.rs:788`). With `trend_threshold` defaulting to `0.0`
   against a per-tick EMA(50), the gate is a coin flip biased exactly wrong: falling market →
   sells disabled → no exits; rising market → buys disabled → ladder never re-arms.
   - Runner 40 (Jun 24, falling): 6 buy fills 68.32→65.12, each followed *only* by a lower
     extension buy — ~$150 deployed into the drop, **zero counter-sells its entire 5-day life**
     (`open_sells = 0` in all 8,467 snapshots; no "counter sell" line in any log).
   - Runner 41 (Jun 29–Jul 2, rising): sells at 76.18/78.15/80.12/82.09, **zero counter-buys**
     (no "counter buy" line in any post-06-23 log; `open_buys` stuck at 1 — a stale 70.27 order).
2. **The filter's semantics are inverted for a grid.** It allowed extension buys marching down a
   crash (adding exposure) while blocking counter-sells (reducing exposure).
3. **No re-centering without fills.** Runner 41 ended with 1 buy @ 70.27 and 1 sell @ 84.06,
   mid ~81: a 17%-wide dead zone, *inside* the rebuild trigger's `×0.95/×1.05` bounds
   (`grid.rs:638`), so the trigger can never fire → **zero fills for 5 days**, position frozen at
   1.4171 SOL. Deadlock: no fill → no slide → no fill.
4. **Rebuilds orphan live orders.** Both rebuild paths clear `buy_orders`/`sell_orders` maps
   without emitting `Cancel` signals — the resting exchange orders survive as artifacts.
5. Cosmetic: soft-resume/rebuild logs print `range ~65.76–0.00` and a raw `f64::MAX`
   (`grid_upper()` unwrap defaults) — fix while in there.

**Interpretation:** per-trade capture is fine; the grid earned +$8.25 in the ~3 days it was
actually two-sided. The lever is uptime of the round-trip loop, not trade size.

## Implementation plan

Ordered; steps 1–3 are the profit-critical core. All grid changes are in
`src/algorithm/grid.rs` unless stated.

### 1. Never gate counter-orders; split risk gates from the trend gate

Refactor `buys_enabled`/`sells_enabled` into orthogonal checks:

- `risk_allows_buy(price)` = `position < max_position && !stopped_out(price)`.
- Inventory check for sells stays `can_place_sell()` (the plan/06 no-short invariant).
- `trend_blocks_extension_buy(price)` = trend filter is EMA **and** trend EMA is known **and**
  `(price − ema) / ema < −trend_threshold` (i.e. blocks only in a *down* trend beyond threshold —
  note this is the opposite comparison from today's `buys_enabled`).

Then in `on_fill`:

- **Buy fill → counter-sell:** gate on `can_place_sell()` **only**. Sells reduce exposure; they
  are never trend- or stop-gated.
- **Buy fill → extension buy:** gate on `risk_allows_buy && !trend_blocks_extension_buy`.
- **Sell fill → counter-buy:** gate on `risk_allows_buy` **only** (no trend gate — this re-arms
  the ladder for the next round trip; it was the missing order in runner 41's rally).
- **Sell fill → extension sell:** gate on `can_place_sell()` only (unchanged in effect).

In `on_tick`, the cross-level triggers (`grid.rs:659-691`) fire for *pre-planned* ladder levels:
replace `sells_enabled` with `can_place_sell()`-consistent behaviour (emit; the dispatcher's base
check backstops) and `buys_enabled` with `risk_allows_buy` — **no trend gate on planned levels**
(a grid filling its own ladder is normal operation, bounded by `levels` and `max_position`).

`build_grid`'s initial ladders stay ungated (designed exposure, already bounded).

### 2. Candle-based trend EMA (replaces the tick EMA as the filter input)

- **`Algorithm` trait:** add `fn on_trend_update(&mut self, ema: f64) {}` (default no-op), mirroring
  `on_spacing_update`. `GridBot` stores it as `trend_ema: Option<f64>`; the tick-EMA fields
  (`ema`, `update_ema`) are removed along with their use in the filter (delete, don't deprecate —
  nothing else consumes them; `trend_label()` in `summary()` switches to `trend_ema`).
- **Runner (`src/runner/mod.rs`):** extend the existing ATR-refresh pattern (`mod.rs:514-534`):
  when `trend_filter` is not `off`, fetch `trend_ema_period` (default 50) candles of
  `trend_timeframe` (new option, default `"30m"`) on the same refresh interval as ATR (create the
  interval when either ATR *or* trend refresh is needed), compute the EMA of closes, call
  `on_trend_update`. Also compute once at spawn (like the initial ATR fetch) so the filter isn't
  blind until the first refresh. Until the first value arrives, extensions are **allowed**
  (warming-up behaviour unchanged).
- **Backtester (`src/backtest/mod.rs`):** maintain a rolling close-EMA over the replay candles
  with period `trend_ema_period` and call `on_trend_update` before each candle's `on_tick`, so
  replay and live exercise the same filter.
- Defaults: `trend_threshold` **0.005** (was 0.0) — parsed in `GridBot::new`.

### 3. Fill-less re-centering + cancel-on-rebuild

- **Trigger (in `on_tick`, after the existing far-outside check):** compute the distance from mid
  to the nearest resting buy level and nearest resting sell level. If **no level on either side**
  lies within `recenter_band × spacing` of mid (new option `recenter_band`, default `2.5`), or one
  side is empty and the other side's nearest level is outside the band, the grid is in a dead
  zone → re-center. Guard with a cooldown: skip if the last rebuild was under 5 minutes ago
  (store an `Instant`/tick timestamp) so a fast wick can't thrash cancel/replace cycles.
- **Cancel-on-rebuild:** add a helper that drains `buy_orders`/`sell_orders` into
  `TradeSignal::Cancel` signals (price + side) and prepends them to `build_grid`'s output. Use it
  in **all three** rebuild paths: the resume-rebuild (`grid.rs:613-622`), the far-outside rebuild
  (`grid.rs:638-653`), and the new dead-zone re-center. This fixes the artifact-order bug — the
  dispatcher already resolves Cancel → live order id (`dispatch.rs:28-57`).
- **Re-size at rebuild:** reset `sized = false` before calling `build_grid` in the re-center path
  (and the far-outside path) so `size_from_capital` re-derives `qty` at the new midpoint. This is
  where compounding (step 4) takes effect. Note `size_from_capital` permanently shrinks
  `levels_per_side` when budget-capped — make the reduction non-destructive (keep the requested
  level count in a separate field, e.g. `levels_requested`, and derive the effective count each
  sizing pass) so a grown budget can restore levels later.
- Fix the cosmetic log bugs while here: soft-resume/rebuild messages must render "no sells/buys"
  sensibly instead of `0.00`/`f64::MAX` (`grid.rs:600-618`).

### 4. Compounding: effective budget = `capital + net realized`

- In `size_from_capital`, replace `capital` with `effective = capital + book.realized_pnl`
  (`realized_pnl` is already net of fees). Clamp at ≥ 0; the existing unfundable path handles a
  budget that has shrunk below one `min_notional` level (warn + no orders).
- Log the effective budget in the existing "Sized from capital" info line
  (`capital X + realized Y → budget Z`).
- No new option: this is the (user-chosen) default behaviour of `capital` sizing.

### 5. `sweep` command — `src/commands/cli.rs`, `src/backtest/` (new `sweep.rs` submodule)

- CLI: `sweep -s <SYMBOL> [-t <timeframe=30m>] [-l <limit=3000>] [--from-file <path>]
  [--spacings <csv>] [--levels <csv=2,3,4,6>] [--capital <usd>] [--spread ..] [--maker-fee ..]
  [--taker-fee ..] [--start-quote ..] [--start-base ..]` (reuse the `backtest` arg definitions
  where they exist).
- Behaviour: load/fetch the candle set **once**; when `--spacings` is omitted, derive candidates
  from the candle set's ATR (`compute_atr`, period 14) × `{0.25, 0.5, 1.0, 1.5, 2.0}`. Run
  `backtest::run_backtest` for each spacing × levels combination with `capital`-driven sizing and
  the standard risk options. Rank by **net realized PnL** desc, tiebreak lower max drawdown.
- Output: console table (rank, spacing, levels, realized, fees, trades, max DD, ending equity) via
  the logger, top result highlighted with a ready-to-paste `runner` spawn line; full results
  written to a report file (reuse the `backtest::write_report` pattern/dir).
- Keep it sequential (a few dozen replays over ≤ a few thousand candles is fast); no parallelism.

### 6. Update the `scout` skill — `C:/Users/soufi/.claude/skills/scout/` (the `/scout` command; adjust to its actual file layout on disk)

- Emit the new/changed options explicitly in recommended spawn lines: `trend_threshold=0.005`,
  `trend_timeframe=30m` (and keep the plan/06 set: `capital`, `allow_unprofitable=false`,
  `max_position`, `max_drawdown_pct`, `stop_loss_pct`, `trend_filter=ema`).
- Add a step after symbol selection: run `sweep -s <SYM> --capital <budget>` inside T.E.D for each
  finalist and adopt the top-ranked spacing/levels instead of the heuristic spacing computation.

### 7. Rollout note (manual, for the user — record in the report/handoff)

After deploying, **`kill` the existing SOLUSD runner and spawn fresh** (`--fresh`): a plain
restart soft-resumes the preserved dead-zone grid (price 81 inside [70.27–84.06]) and would wait
for the re-center cooldown path to clean it up; a fresh spawn cancels the stale 70.27/84.06
orders and rebuilds at mid immediately. Re-run scout/sweep first to pick the config.

## Out of scope

- **New strategy types** — still deferred (plan/00, 2026-06-23); this round is grid flow-through.
- **The TUI dashboard** — plan/07, independent changeset.
- **Margin/short selling**, **web dashboard**, **exchange trait changes** — unchanged deferrals.
- Auto-restarting or reconfiguring the live runner from code — deployment stays manual (step 7).
- Parallel/threaded sweep execution.

## Validation

- `cargo build`, `cargo clippy`, `cargo test` clean.
- **Unit tests (grid):**
  - Buy fill while price is *below* the trend EMA (downtrend) → counter-sell **is** emitted
    (regression for runner 40); extension buy is **not** (blocked beyond threshold).
  - Sell fill while price is *above* the trend EMA (uptrend) → counter-buy **is** emitted
    (regression for runner 41).
  - Trend EMA unknown (warming up) → extensions allowed.
  - Dead-zone re-center: grid with only far levels (> `recenter_band × spacing` both sides) →
    rebuild emits Cancels for every stale level plus a fresh two-sided ladder around mid; a second
    tick inside the cooldown does **not** rebuild again.
  - Compounding: after fills produce net realized ±X, a rebuild re-sizes with `capital ± X`
    (assert derived qty moves accordingly); negative-beyond-budget clamps to the unfundable path.
  - Level-count restoration: budget-capped levels recover when the effective budget grows.
- **Unit test (backtest):** rolling trend EMA is fed to the algorithm during replay (e.g. a
  scripted candle series where the old code would strand inventory now round-trips).
- **Sweep:** run on a fixed `--from-file` candle set → deterministic ranking; identical
  (spacing, levels) input twice → identical report (reuses the backtester's determinism test
  pattern).
- **Replay the diagnosis window:** `sweep -s SOLUSD -t 30m -l 700` (~2 weeks) and a single
  `backtest` at the live config (`spacing=1.97, levels=2, capital=118.91`) — confirm the fixed
  grid shows fills distributed across the window (no multi-day zero-fill gap) and net realized
  ≥ the old code's on the same candles.
- **Manual paper/live session** (user, after deploy): fresh spawn per step 7; confirm counter
  orders appear in the log after every fill ("counter sell"/"counter buy" lines — absent from
  every log since 06-23), and that a quiet drift period triggers a "re-center" rebuild with
  cancels instead of a silent stall.
