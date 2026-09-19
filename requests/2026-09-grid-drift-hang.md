# Request — grid drifts to a one-sided hang; PnL stalls (round 5)

Date: 2026-09-19. Raised after weeks of live running (SOLUSD + XMRUSD headless,
post plan/12 launch 2026-09-17).

## Symptom (user's words)

The grid algorithm drifts toward a "hang". After a couple of trades the grid has
either only buys or only sells resting, while the symbol's price has drifted far
enough away that the resting side will never be reached again (or only on luck).
The TUI / `journalctl` warns it has drifted too far and won't re-center. At that
point PnL flatlines — the percentage looks fine, but nothing more happens. Same
stall is visible now on the live pairs (Lara's `ted_status` reads confirm it).

This is why the user is lifting the "no config changes before 2026-09-29" eval
hold early: waiting a month yields no more signal once a runner has hung.

## Root cause (diagnosed from code 2026-09-19)

Two mechanisms terminate in the same stranded, one-sided state:

1. **The drawdown / circuit-breaker halt is a one-way latch with no autonomous
   recovery.** `process_tick` early-returns while `state.halted` (runner/mod.rs:784)
   — so `on_tick` never runs, meaning no re-center, no rebuild — until a *manual*
   `resume`. On the VPS nobody resumes it; Lara only reads status. Left holding
   lot-exit sells stranded far above a market that kept falling.
2. **The grid has no bounded way to exit a losing lot.** Lot exits are floored at
   breakeven and never re-priced down ("never sell below cost", plan/09,
   grid.rs:523). In a sustained drift, lots pile up with exits stranded above,
   the buy ladder re-centers down burning budget until `unfundable`
   (grid.rs:606), then stops. Fully one-sided; `in_dead_zone` re-center only
   moves the *buy* side, so it cannot help.

Root cause is the inventory-risk gap every prior round deferred: a pure grid with
a hard never-sell-below-cost rule cannot survive a trend. It scalps ranges and
strands inventory on drifts.

## Decision (2026-09-19)

Direction chosen: **Round 5 — bound the inventory risk inside T.E.D (fast loop,
Rust, deterministic).** Reopen "never sell below cost": accept *bounded* realized
losses so a runner stays two-sided-capable and never hangs. Pair with a thin
operator backstop (Lara can resume/rotate a stuck runner) as a later slice.

Invariant preserved: the fast loop stays deterministic Rust; the LLM never sizes
a trade; guardrails stay in T.E.D.

Full design: `plan/13-drift-hang-capitulation.md`.
