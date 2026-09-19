# plan/13 — Bounded capitulation: kill the one-sided drift hang

## Goal

Stop a grid runner from drifting into a permanent one-sided, PnL-flat hang, by
giving it a **deterministic, bounded** way to cut a stranded losing lot. No new
strategy — one exit-management rule added to the existing per-lot grid, plus one
fix so the drawdown halt is not a dead end. Fast loop stays Rust/deterministic;
guardrails unchanged.

## Context

Source request: `requests/2026-09-grid-drift-hang.md`. Reopens the plan/09
"never sell below cost" rule — deliberately, because that rule is the direct
cause of the hang (see [[ted-per-lot-grid-2026-07]], [[ted-trend-gate-deadlock]]).

Grounding facts from current code:

- Lot exit is floored at breakeven, anchored to fill price, never re-priced down:
  `exit_for()` grid.rs:523; placed via `emit_exit()` grid.rs:533; recorded in
  `LotBook` (position.rs — `Lot { qty, entry_price, entry_cost, exit_price }`,
  **no timestamp**).
- Buy ladder re-centers on distance (`in_dead_zone` grid.rs:817, rebuild
  grid.rs:940) but the **sell/exit side never moves**. When budget runs out
  `size_from_capital` sets `unfundable` and emits nothing (grid.rs:606).
- `check_risk` latches `state.halted` at 20% drawdown, cancels resting buys, and
  `process_tick` early-returns while halted (runner/mod.rs:784, 847) → `on_tick`
  stops entirely until manual `resume` (runner/mod.rs:708).

## Design — trigger capitulation only when actually stuck

Capitulate a lot **only when the runner can no longer re-fund the buy ladder**
(the precise hang condition), not on ordinary deep dips. This needs no wall clock
and cannot fire on a fast wick that still leaves budget to buy the dip.

A lot is a **capitulation candidate** when ALL hold:
1. the runner is `unfundable` **or** halted (i.e. it can no longer add the buys
   that would otherwise recover the position), and
2. mid price is at least `capitulate_band × spacing` below the lot's entry
   (default `capitulate_band = 6.0` — well outside the normal grid, so a lot
   inside a live ladder is never touched), and
3. the resulting realized loss on that lot is within `max_lot_loss_frac` of its
   cost (the hard cap; see knobs).

For each candidate lot, re-price its resting exit **down** to a marketable level:
`target = max(mid, entry_unit_cost × (1 − max_lot_loss_frac))`, rounded to
`price_decimals`. So:
- if mid is above the loss floor → cut at mid (small/zero loss, frees budget now);
- if mid is below the floor → park the exit at the floor price (a limit sell at
  the worst acceptable loss) so a bounce takes us out at the *bounded* loss
  rather than the runner sitting stranded indefinitely.

Cutting a lot realizes the (bounded) loss, shrinks `unfundable`'s effective
budget pressure, and lets `size_from_capital` fund the buy ladder again on the
next rebuild → the runner is two-sided again instead of hung.

### Why this over the alternatives (recorded, so we don't re-litigate)

- *Distance-only cut, always on:* would cut healthy lots on every deep-but-normal
  dip and bleed the edge. Gating on `unfundable/halted` restricts cutting to the
  actual hang.
- *Time-based staleness:* cleaner intent but needs an `entry_time` on `Lot` →
  serde/state-schema change and a clock in the algorithm. Deferred; the
  fund-pressure gate already prevents wick-triggered cuts. (`ponytail:` gate is a
  proxy for "stuck"; add lot timestamps if a pair needs time-based capitulation.)
- *Regime-flip → finish-exits + re-seed lower:* heavier, throws away the whole
  ladder. Bounded per-lot capitulation is the smaller diff and keeps scalping.

## Implementation plan

### 1. `src/algorithm/grid.rs` — capitulation maintenance

- Add options (all with `#[serde]`-style parse in `from_options`, defaults in
  code so existing configs stay valid): `max_lot_loss_frac` (default TBD — see
  knobs), `capitulate_band` (default 6.0), `capitulate_enabled` (default true).
- New `fn capitulate(&mut self, mid) -> Vec<TradeSignal>`: iterate `book.lots`,
  find candidates per the rule above, and for each cancel-and-replace its resting
  exit in `sell_orders` at `target` (reuse the cancel/replace shape from
  `emit_exit`). Update the lot's `exit_price`. Emit a `log_warn` naming the lot,
  entry, target, and realized-loss estimate.
- Call it from `on_tick` **after** the existing re-center/rebuild block, so a
  normal rebuild takes precedence and capitulation only runs when still stuck.
- `check_exit_invariant` still holds (one exit per lot; qty unchanged).

### 2. `src/runner/mod.rs` — halt is exit-management, not a full stop

- While `state.halted`, do **not** skip the whole tick. Instead run an
  **exit-only** path: call `on_tick`, then filter its signals through the
  existing `finishing_filter` (keep Sells + buy-Cancels, drop new Buys /
  sell-Cancels) so capitulation and lot exits keep working but no new exposure is
  added. Un-halted ticks are unchanged.
  - Concretely: replace the `if check_risk() { return; }` early-return
    (runner/mod.rs:784) with a `halted` flag that switches the post-`on_tick`
    signal set to the exit-only filter (mirrors the `finishing` branch already at
    runner/mod.rs:789).
- Net effect: the drawdown/breaker halt still means "no new buys", but a halted
  runner now cuts stranded lots down to the bounded floor and can go flat, instead
  of freezing until a human resumes.

### 3. Persistence / state

- No new persisted fields required (capitulation reads live `book.lots` +
  `unfundable`/`halted`). `GridState` serde unchanged → existing `runner_state`
  rows load fine. Confirm round-trip in the existing state test.

### 4. Sweep / scout / backtest

- Thread the three new options through the sweep grid and the backtest option
  injection so replays exercise capitulation. Add `max_lot_loss_frac` to the
  scout-emitted spawn line (default value). No sweep-ranking change.

### 5. Tests (assert-based, in `grid.rs` #[cfg(test)])

- `capitulation_ignored_while_fundable`: deep dip but budget still funds buys →
  no exit re-price.
- `capitulation_cuts_stranded_lot_when_unfundable`: unfundable + mid far below
  entry → exit re-priced down, realized loss ≤ cap.
- `capitulation_floor_caps_loss`: mid far below the floor → exit parked at floor,
  not at mid; loss bounded by `max_lot_loss_frac`.
- `halted_runner_still_manages_exits` (runner-level or via filter unit): halted →
  Buys dropped, Sells/capitulation pass.

## Knobs the user must set (risk appetite — defaults proposed)

- **`max_lot_loss_frac` — the cap on realized loss per capitulated lot.** This IS
  the "how far below cost will we ever sell" number. Proposed default **0.03**
  (3%). Lower = more never-sell-below-cost purity but slower to free a hang;
  higher = frees hangs faster but bigger per-cut losses.
- **`capitulate_band` (× spacing)** — how far below entry a lot must be before
  it's eligible. Proposed **6.0** (safely outside a live ladder).
- **`capitulate_enabled`** — master switch, default **true**. Set false to keep
  strict plan/09 behaviour on a given pair.

## Out of scope (follow-on)

- **Lara operator backstop** (resume / rotate a stuck runner with judgment) — the
  thin Phase-2 slice paired with this. Once capitulation lands, halts should be
  rare; the operator backstop handles the residual. Separate change on the Lara
  side.

## Acceptance

- Build + clippy clean; new tests pass; existing 78 tests stay green.
- Replay of a sustained-downtrend window (e.g. the live SOL drift that hung)
  shows the runner realizing bounded losses and staying two-sided instead of
  going `open_sells>0, open_buys=0` and flat. Rollout = user: rebuild, respawn.
