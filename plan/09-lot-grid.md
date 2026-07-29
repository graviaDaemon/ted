# plan/09 — Per-lot grid: profitable scalps in any regime

## Goal

Stop the grid from ever selling inventory below its cost. Replace avg-cost ladder maintenance
with per-lot tracking — every buy fill gets its own resting exit order that survives rebuilds
and is floored at breakeven-plus-minimum-profit — while the grid keeps scalping fluctuations in
downtrends with reduced size and a hard inventory cap.

## Context

Source request: `requests/2026-07-current-status.md`. Decisions: `plan/00-decisions.md`
§ 2026-07-29. Evidence base: `requests/history/` (logs Jul 7–29, `trades/trades_SOLUSD.jsonl`,
`ted.db` snapshot).

Diagnosis of the Jul 7–29 live session (SOL −10%, account −4.9%, **−$35 realized pre-fee over
1,335 fills**): the grid's own maintenance flushes inventory below cost through three mechanisms,
all in `src/algorithm/grid.rs`:

1. **Rebuild cancels the whole sell ladder.** Re-centering (which fired every 15 min–2 h) drains
   *all* tracked levels into Cancel signals (`drain_cancels`, ~line 597) and `build_grid` re-creates
   sells around the new (lower) mid. Inventory bought higher gets re-listed lower.
2. **Buy fill replaces the outermost sell.** In `on_fill` (buy branch, ~line 793): when the sell
   ladder is full, the *highest* sell — the exit for the most expensive inventory — is cancelled
   and effectively replaced by a lower one.
3. **Counter-sell priced off current price, not fill price.** `on_fill` places the counter at
   `current_price + spacing` (~line 812); in a fast drop `current_price < fill_price`, so the exit
   can rest *below* the entry.

Also fixed here: explicit `spacing` disables ATR fetch/refresh entirely
(`should_fetch_atr`, `src/runner/mod.rs:31-36`) — the live session's spacing was pinned at
0.12266071 for 22 days; and `max_drawdown_pct` defaulted to off.

The user's chosen downtrend behavior (2026-07-29): keep scalping — "down/up fluctuations in a
downward trend could still make minute profits" — not hibernate, not flatten. So buys continue in
downtrends, but smaller and capped; exits are always profitable by construction.

## Implementation plan

### 1. `src/algorithm/position.rs` — add `LotBook`

New struct alongside (replacing the grid's use of) `AvgCostBook`:

```rust
pub struct Lot {
    pub qty: f64,
    pub entry_price: f64,       // fill price
    pub entry_cost: f64,        // price*qty + entry fee (fee folded in, like AvgCostBook)
    pub exit_price: f64,        // the resting counter-sell level for this lot
}
pub struct LotBook {
    pub lots: Vec<Lot>,         // open lots, oldest first
    pub realized_pnl: f64,      // net of fees
    pub fees_paid: f64,
    maker_fee: f64,
}
```

- `record_buy(price, qty, exit_price) -> &Lot` — creates a lot, books the fee.
- `record_sell(price, qty) -> f64` — matches the lot(s) whose `exit_price` equals the fill level
  (same price-key rounding the grid uses); FIFO among equal keys; realized =
  `(price*(1-maker_fee) - entry_cost/qty) * qty`. If no lot matches the level (out-of-band fill),
  fall back to global FIFO and `log_warn` — never silently drop.
- `seed(qty, price)` — opening inventory becomes a normal lot (no fee), `exit_price` provided by
  the caller.
- Derived views so trait reporting keeps working: `position()` (Σ qty), `avg_cost()`
  (Σ entry_cost / Σ qty), `notional()` (Σ entry_cost).
- serde derive (snapshot/restore), unit tests mirroring the existing `AvgCostBook` tests plus
  lot-matching and fallback cases.

`AvgCostBook` stays in the file for the backtester's `Sim` internal wallet accounting only; the
grid and the backtested grid both run on `LotBook` (same algorithm code — the backtester drives
the `Algorithm` trait, so it inherits the change automatically).

### 2. `src/algorithm/grid.rs` — lot-based exits, buy-only maintenance

- Replace `book: AvgCostBook` with `book: LotBook`. `sell_orders` map becomes lot-exit-driven:
  key → aggregate qty resting at that level (multiple lots may share an exit level).
- **Exit invariant** — single helper used everywhere a counter-sell is priced:
  `exit = max(fill_price + spacing, breakeven)` where
  `breakeven = (entry_cost/qty) * (1 + min_profit_frac) / (1 - maker_fee)`, rounded UP to
  `price_decimals`. New option `min_profit_frac`, default `0.001`.
- `on_fill` buy branch: create the lot, emit its exit. **Delete the "replace outermost sell"
  block.** Extension-buy logic unchanged (still trend-gated).
- `on_fill` sell branch: `book.record_sell` matches and closes lot(s); counter-buy re-arm
  unchanged; **delete the "extension sell" block** (sells exist only as lot exits).
- `build_grid`: builds **only the buy ladder**. No generic sell ladder; existing lot exits are
  left untouched. Seeding opening base inventory creates a seed lot with exit at
  `max(mid + spacing, breakeven)` and emits that sell once.
- Rebuild/recenter (`drain_cancels` and every rebuild path in `on_tick`): cancel **buy orders
  only**; sell orders persist. `in_dead_zone` becomes buy-side-only: rebuild when no *buy* level
  rests within `recenter_band × spacing` below mid (sells no longer count — a lot exit far above
  mid is intentional patience, not staleness).
- `can_place_sell`/`sell_headroom`: re-derive from `book.position()` minus qty already resting in
  lot exits (should net to ~zero by construction; keep as an invariant check + warn).
- **Downtrend sizing** (trend label already computed from the candle EMA via `on_trend_update`):
  when trend is `down`:
  - buy ladder depth = `downtrend_levels` (new option, default `1`);
  - buy qty = `qty × downtrend_qty_frac` (new option, default `0.5`), but never below
    `min_notional / price` — if the scaled qty would violate min notional, keep full `qty`;
  - extension buys remain blocked (existing behavior).
- **Inventory cap**: before emitting any buy (ladder, counter, extension), check
  `book.notional() + qty×price ≤ capital × cap` where `cap = max_inventory_frac`
  (new option, default `0.75`) in flat/up trend and `max_inventory_frac_down` (default `0.5`)
  in downtrend. When the cap blocks a buy, log once per rebuild, not per tick. Existing
  `max_position` (base units) still honored if set.
- **Fee/profit floor**: replace the `maker_fee > 0.0`-gated fee-floor check in `build_grid`
  (~line 478) with an unconditional floor:
  `spacing_min = midpoint × (2×maker_fee + min_profit_frac)`. If `spacing < spacing_min`, **clamp
  spacing up to `spacing_min`** and log — do not refuse to build (refusing is how a
  misconfigured bot silently does nothing). `allow_unprofitable=true` skips the clamp.
  Apply the same clamp in `on_spacing_update`.
- `snapshot`/`restore`: serialize the `LotBook` (bump the state version; old snapshots without
  lots restore by converting the avg-cost position into a single seed lot at `avg_cost`).
- Default `max_drawdown_pct` handling lives in the runner (step 4).

### 3. `src/runner/mod.rs` — ATR unpin + reconcile lot exits

- `should_fetch_atr` (~line 31): remove the `!options.contains_key("spacing")` condition.
  Explicit `spacing` now seeds the initial value and skips only the *initial* ATR fetch when
  present; the periodic refresh still runs whenever any `atr_*` option is present, calling
  `on_spacing_update` (which clamps to the floor per step 2). Log the transition
  (`spacing seeded 0.1227 → ATR refresh active`).
- Reconciliation path (post-reconnect snapshot diffing): after diffing open orders, re-place any
  lot exit that is missing from the exchange (the grid exposes its expected exits; runner compares
  against open sells and re-emits). This protects the "exits always resting" invariant across
  disconnects.

### 4. Defaults & options plumbing

- `max_drawdown_pct`: default `0.20` when the option is absent (runner spawn path). `Risk:` log
  line reflects it. Halt semantics: stop placing **buys**; lot exits keep resting; notify.
- Register/parse new options in `GridBot::from_options`: `min_profit_frac`, `downtrend_levels`,
  `downtrend_qty_frac`, `max_inventory_frac`, `max_inventory_frac_down` (all with the defaults
  above, all serialized in state and printed by the grid's describe/report output).

### 5. Tests

- `LotBook`: buy/sell matching, FIFO on shared exit level, out-of-band fallback, seed, serde
  round-trip.
- Grid: exit ≥ breakeven in a fast-drop scenario (fill at 80, current 79 → exit above 80×fee
  breakeven, not 79+spacing); rebuild cancels buys only and preserves exits; buy fill with full
  sell side does NOT cancel the outer sell; downtrend depth/qty/cap behavior; spacing clamp; old
  snapshot restore.
- Update every existing grid/backtest test broken by the sell-ladder removal (expected: the
  sell-extension and outer-sell-replacement tests are deleted/rewritten as their inverses).

## Out of scope

- Fee auto-fetch, config/CLI fee defaults, equity/TUI truth (plan/10).
- Sweep `atr_multiplier` output, walk-forward, scout update, second pair (plan/11).
- Partial-fill accounting (runner books on full order fill; unchanged).
- Rhai/passive/atr algorithms (untouched; trait default impls keep them compiling).

## Validation

1. `cargo build` + `cargo clippy` clean; `cargo test` green.
2. Replay the fresh live window through the manual harness:
   `TED_REPLAY_FILE="requests/history/trades/trades_SOLUSD.jsonl" cargo test replay_diagnosis_window -- --nocapture --ignored`
   (same harness as plan/08). Acceptance on this −10% window: **realized PnL ≥ 0** (every closed
   round trip nets ≥ min_profit_frac by construction), inventory notional never exceeds the cap,
   and ending equity ≥ the old code's ending equity on the same file.
3. Backtest sanity: `backtest -s SOLUSD --maker-fee 0.001 --taker-fee 0.002` over 3000×30m candles
   — confirm no sell fill ever books negative realized (assert in the harness or grep the report).
4. Report/`generate` output shows lots, cap state, and the clamped spacing.
