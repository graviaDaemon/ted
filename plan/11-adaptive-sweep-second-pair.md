# plan/11 — Adaptive sweep, scout refresh, second pair rollout

## Goal

Make configuration selection honest and self-adapting — sweep emits volatility-relative
parameters validated out-of-sample — and roll the account out on two pairs with a shared-wallet
budget split.

## Context

Source request: `requests/2026-07-current-status.md`. Decisions: `plan/00-decisions.md`
§ 2026-07-29. Depends on plan/09 (ATR unpin, new grid knobs) and plan/10 (fee defaults).

Postmortem facts this fixes:

- The plan/08 sweep picked an **absolute** `spacing=0.1227`, which (a) was optimized with
  `--maker-fee` defaulting to 0 — a zero-fee replay always favors maximum churn — and (b) pinned
  spacing for the entire 22-day session because explicit `spacing` disabled ATR refresh
  (fixed in plan/09, but sweep/scout must stop emitting absolute spacing for it to matter).
- The sweep ranks by realized PnL on the *same* window it tunes on — overfit by construction.
- The user chose to add a second pair (2026-07-29) to diversify regime risk.
- Scout (`C:\Users\soufi\.claude\commands\scout.md`) instructs adopting the sweep's absolute
  spacing, and its risk options were evidently not passed on the last live spawn.

## Implementation plan

### 1. `src/backtest/sweep.rs` — multiplier output + walk-forward validation

- Compute the candle set's ATR once (existing code already derives spacing candidates from it);
  report each config's spacing **also as `atr_multiplier = spacing / ATR`** in both the terminal
  table and the markdown report.
- **Walk-forward split**: tune on the first ~70% of candles, validate on the last ~30%.
  Each combination is replayed on both halves; ranking is by **validation-half net realized PnL**
  (max drawdown as tiebreaker), with the tuning-half result shown alongside. Configs that are
  top-quartile in tuning but negative in validation get flagged `overfit?` in the report.
- The recommended runner command printed at the end of the report uses
  `atr_multiplier=<x> atr_timeframe=<tf> atr_period=14` — **not** `spacing=` — plus the plan/09
  knobs at their defaults, so the live grid stays volatility-adaptive.
- Sweep candidates should include the plan/09 defaults implicitly (downtrend scalping, caps,
  min_profit_frac) since it replays the same `GridBot`; no extra sweep dimensions this round
  (keep the search space small on purpose).

### 2. `src/runner/mod.rs` — accept `atr_multiplier` as the spacing source

- If `atr_multiplier` is provided (with optional `atr_timeframe`/`atr_period`), treat it exactly
  like today's `atr_*` trio: initial fetch computes `spacing = ATR × multiplier`, periodic
  refresh keeps it updated (plan/09 already un-pinned refresh; this step is mostly naming —
  `atr_multiplier` is an alias of `atr_mult`/`atr_multiplier` handling so sweep/scout/runner all
  speak the same option).

### 3. Scout command refresh (`C:\Users\soufi\.claude\commands\scout.md`)

- Emit `atr_multiplier=` (+ timeframe/period) instead of absolute `spacing=`; state that the
  sweep's top-ranked **multiplier** is what to adopt.
- Always include `--maker-fee`/`--taker-fee` on the printed sweep command, sourced from the
  account's real tier (plan/10 fetch; scout can say "use the fees T.E.D logs at spawn").
- Emit the plan/09 knobs explicitly in every recommended config:
  `min_profit_frac`, `downtrend_levels`, `downtrend_qty_frac`, `max_inventory_frac`,
  `max_inventory_frac_down`, plus the existing risk set (`max_drawdown_pct` now defaults on).
- **Second-pair selection guidance**: when the portfolio should span two runners, shortlist by
  the existing volume/spread/volatility filters, then prefer the candidate with the **lowest
  30-day return correlation to the already-running pair** (compute from daily candles of both);
  split `deployable_total` across both runners (equal split unless one grid is seeded with held
  base inventory).
- Add a warning step: if the last live session's spawn command is known, diff it against the
  recommended option set and call out anything the user dropped (the risk options went missing
  last time).

### 4. Rollout runbook (document at the end of the sweep report or as operator notes)

The rollout itself is the user's (TUI + credentials are theirs):

1. `kill -s SOLUSD`, pull + build the new version.
2. Run scout with the current portfolio → two-pair recommendation with budgets.
3. Inside T.E.D: `sweep -s SOLUSD …` and `sweep -s <PAIR2> …` (fees now default to real values);
   adopt top validated `atr_multiplier`/`levels`.
4. Spawn both runners `--fresh` with the scout budgets (capital split must sum to ≤ the real
   wallet — note the wallet is ~$345 now, not the 362.70 configured last round; the existing
   1.05 SOL seeds the SOLUSD grid via `initial_base_balance`).
5. Paper for ~a day if desired, then live. Watch the new 7-day PnL% readout (plan/10).

## Out of scope

- Any new sweep dimensions (downtrend knobs stay at defaults this round).
- Automated multi-runner budget rebalancing (scout does the split manually).
- Correlation computation inside T.E.D itself (scout does it from public candles).

## Validation

1. `cargo build` + `cargo clippy` clean; `cargo test` green (sweep tests updated for the new
   report columns and ranking).
2. `sweep -s SOLUSD --from-file <fresh candle export>`: report shows tune/validation columns,
   `atr_multiplier`, overfit flags; recommended command contains `atr_multiplier=` and no
   `spacing=`.
3. Spawn a paper runner from the exact recommended command; confirm ATR-derived spacing appears
   in the logs and refreshes on the ATR interval.
4. Dry-run scout end-to-end with the real portfolio string and confirm budgets sum ≤ wallet.
