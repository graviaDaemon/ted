# 02 — Fee-aware, trend-aware, risk-controlled grid

## Goal

Make the grid structurally capable of net-positive PnL: account for fees in realized PnL and in the
decision to place a level, stop the grid from fighting strong trends, and add hard risk controls
(max inventory, max drawdown halt, optional stop-loss) that protect the account regardless of
strategy. This is the changeset that directly addresses "makes no money."

## Context

- Source request: `requests/2026-06-initialization.md`. Decisions: `plan/00-decisions.md` (2026-06-18).
- Depends on **plan/01** (fee fields on `RunnerState`, leveled logging).
- `GridBot` (`src/algorithm/grid.rs`) currently books `realized_pnl += spacing * qty` per sell with
  **no fee term** — this is the core profitability bug. A round trip pays fees on both legs; if
  `spacing × qty ≤ fees`, every "win" is a loss.
- The grid already slides/extends on fills (`on_fill`) and rebuilds when price moves ±5% outside
  bounds (`on_tick`). It has no trend awareness and no inventory cap — in a sustained trend it keeps
  buying into a falling market (or selling into a rising one) and the ±5% rebuild realizes the loss.
- `MarketData` (`src/api/types.rs`) carries `bid, ask, last_price, volume, high, low, daily_change,
  daily_change_pct, timestamp` — enough for a simple trend signal without extra fetches.
- The `Algorithm` trait (`src/algorithm/traits.rs`) has default no-op hooks; new hooks must keep
  default impls so `passive`, Rhai scripts, etc. compile unchanged.

## Implementation plan

### 1. Fee model into the grid — `src/algorithm/grid.rs`

- Add `maker_fee: f64` and `taker_fee: f64` fields to `GridBot`, parsed in `GridBot::new` from
  options (`maker_fee`, `taker_fee`), default `0.0`. (The runner already resolves these from
  config defaults in plan/01; pass them down via the options map so `build_algorithm` stays uniform —
  `run_runner` should inject the resolved fee values into the options map before `build_algorithm`,
  same pattern as `initial_*_balance` and `spacing`.)
- Replace the PnL line. Grid limit orders are makers, so per closed round trip:
  `net = spacing * qty - (buy_price * qty * maker_fee) - (sell_price * qty * maker_fee)`.
  Accumulate `realized_pnl += net` on the sell leg (keep the current per-sell accrual point, but use
  the fee-adjusted value; track the matching buy price via the position's average cost — see step 2).
- **Minimum-spacing guard:** in `new`, if `spacing` is provided directly and
  `spacing < (maker_fee + taker_fee) * midpoint_estimate` we cannot validate at construction (no price
  yet), so instead enforce at first `build_grid`: if `spacing <= 2 * maker_fee * midpoint`, log a
  `warn` that the grid is structurally unprofitable at current fees and (config option
  `allow_unprofitable=false` default) refuse to build, emitting no orders and a clear error in summary.
  Add `allow_unprofitable` option to override for experimentation.

### 2. Average-cost position tracking (for honest PnL)

- Replace the implicit "every sell earns one spacing" with average-cost accounting:
  - Track `position_qty: f64` and `position_cost: f64` (total quote spent net of fees on the open
    base position).
  - On buy fill: `position_cost += fill_price * qty * (1 + maker_fee)`, `position_qty += qty`.
  - On sell fill: realized `+= (fill_price * (1 - maker_fee) - avg_cost) * qty` where
    `avg_cost = position_cost / position_qty` (guard `position_qty > 0`); decrement
    `position_cost` by `avg_cost * qty` and `position_qty` by `qty`.
- This makes `realized_pnl` and `summary()` reflect true net PnL and naturally handles trend losses
  (selling below average cost shows a real loss instead of a fake `+spacing`).

### 3. Trend filter — suspend the losing side in a strong trend

- Add `trend_filter` option (`off` | `ema` ; default `ema`) and `trend_ema_period` (default `50`,
  in ticks) and `trend_threshold` (default `0.0` slope, i.e. any nonzero slope counts; tune later).
- Maintain an EMA of mid price across ticks in `GridBot` (`ema: Option<f64>`, updated each `on_tick`).
  Slope sign = sign of `(price - ema)`.
- Behaviour when trend is **up** beyond threshold: stop placing/replenishing **buy** extensions
  (don't chase a rising market with buys that will be left underwater); let sells fill and counter-buys
  remain (so the grid still takes profit on the way up). When trend is **down**: stop **sell**
  extensions. When `|price-ema|/ema <= trend_threshold`: normal two-sided behaviour.
- Implementation: gate the extension-order insertion in `on_fill` and the cross-level emission in
  `on_tick` on a `fn buys_enabled(&self)->bool` / `sells_enabled(&self)->bool` derived from the EMA
  state. Keep it side-suspension only — do **not** cancel the resting opposite side (that would churn
  fees); just stop *adding* to the losing side.

### 4. Risk controls — new `Algorithm` hook + runner enforcement

The grid can express limits, but a hard kill must live in the runner so it applies even to misbehaving
scripts. Do both:

- **In `GridBot`:** add options `max_position` (max absolute base inventory; default unbounded) and
  `stop_loss_pct` (e.g. `0.1` = halt if unrealized loss exceeds 10% of deployed capital; default off).
  Stop placing buys when `position_qty >= max_position`; stop sells when `position_qty <= -max_position`.
- **In the runner (`src/runner/state.rs` + `src/runner/mod.rs`):** add a generic risk guard
  independent of the algorithm:
  - New `RunnerState` fields: `max_drawdown_pct: Option<f64>`, `peak_equity: f64`, `halted: bool`.
  - Compute equity each tick as `quote_balance + position_qty * mid` (use wallet + a position estimate
    the runner already can derive from fills, or expose `algorithm.position() -> f64` via a new
    optional trait method defaulting to `0.0`). Track `peak_equity`; if
    `(peak_equity - equity)/peak_equity > max_drawdown_pct`, set `halted = true`, cancel all live
    orders (`cancel_all_live_orders`), log `critical`, and stop dispatching new signals until a
    manual `--resume`. Surface halt state in `report`/summary.
  - Options sourced from runner options `max_drawdown_pct` (default off).
- Add optional trait methods to `Algorithm` (default impls): `fn position(&self) -> f64 { 0.0 }` and
  `fn unrealized_pnl(&self, _mid: f64) -> f64 { 0.0 }` so the runner can read inventory/PnL generically.

### 5. Reporting — `src/runner/report.rs`

- Include net realized PnL, fees paid (cumulative), average cost, current unrealized PnL at last mid,
  trend state, and halt status in the generated overview. (Read the existing report builder and extend.)

## Out of scope

- New strategy *types* (market-making, momentum) — plan after backtester (separate future changeset).
- Backtesting these changes — that harness is plan/03 (but design these so they are deterministic and
  replayable: no wall-clock dependence inside the algorithm).
- Storage of risk events to DB (plan/04 can add a `risk_events` table later if wanted).

## Validation

- `cargo build` / `cargo clippy` clean.
- Unit tests in `grid.rs`:
  - Round-trip PnL with `maker_fee = 0` equals `spacing * qty` (parity with old behaviour).
  - Round-trip PnL with a nonzero fee is reduced by exactly both fee legs.
  - `allow_unprofitable=false` + `spacing` below the fee floor → `build_grid` emits no orders and flags
    unprofitable in `summary()`.
  - Trend up → no new buy extensions emitted on a sell fill; trend flat → buy extension emitted.
  - `max_position` reached → buy signals suppressed.
- Runner-level test (or manual paper run): force a drawdown past `max_drawdown_pct` and confirm the
  runner halts, cancels orders, and logs `critical`.
- Manual paper-mode run on a real symbol for a session; confirm `generate --runner SYM --verbose`
  shows fee-adjusted PnL and trend/halt fields.
