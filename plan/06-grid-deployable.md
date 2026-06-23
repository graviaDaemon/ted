# 06 — Make the grid actually deployable: balance-aware sizing, spot-only (no-short) fills, live risk

## Goal

Turn the existing grid from "structurally sound but un-deployable" into a grid that can actually run
two-sided and profitably on a small, shared (~$183) spot account: size every runner from a quote
**capital budget** instead of a fixed `qty`, forbid net-short positions (the source of the phantom
−$20 equity), make exchange order-state authoritative for fills, and turn on the risk controls that
already shipped but are left off in the live config. **No new strategy type** — a correctly-running
two-sided grid *is* the "profit on small moves regardless of direction" behaviour the request asked
for.

## Context

- **Source request:** `requests/2026-06-profitability.md`.
- **Decisions:** `plan/00-decisions.md`, section `2026-06-23 — Make the grid deployable`. Also relevant:
  the `2026-06-18` overhaul decisions (fees per-runner, AvgCostBook shared accounting, risk controls,
  backtester).
- **This builds on:** plan/02 (fee/trend/risk grid — the risk/trend options already exist), plan/03
  (the backtester — used here to validate before going live), plan/04 (storage/resume).

### Empirical diagnosis (from `requests/history/`: `ted.db`, `logs/`, `trades/`)

39 runner sessions, 2,082 fills, 14,299 snapshots, Apr→Jun 2026. Findings, in priority order:

1. **Under-capitalization → one-sided grid (the headline cause of "no profit").** The live wallet held
   ~**$3.60 USD** while each grid buy level needed ~**$24** (`qty` × price). The logs are full of
   `Insufficient USD (3.6023 < 23.9936) — skipping BUY`. Every **buy** was skipped; only **sells** of
   existing base went through. The grid ran **sell-only**, dumped inventory, and never round-tripped.
   Runner 39 (XRPUSD) ran **5 days with `open_buys = 0` for its entire life** (snapshots confirm).
   `qty` is a *fixed required option* (`grid.rs:84`) — it cannot adapt to the actual wallet.
2. **Phantom net-short → fake −$20 equity.** Runner 39's order was **rejected** by the exchange
   (`SELL FAILED: ... not enough exchange balance for -21.91 XRPUSD`) yet a **−21.91 XRP** position was
   booked internally, driving reported equity to **−$20.53**. The class of bug is self-documented at
   `src/runner/mod.rs:443-449`: the snapshot-diff path books a fill from an order's mere *absence*
   (out-of-band cancels become phantom fills). On a spot account the grid should never be net short.
3. **Risk controls present but OFF.** Zero log hits for `trend`, `drawdown`, `halt`, `max_position`,
   `unprofitable`. The plan/02 options (`max_position`, `stop_loss_pct`, `trend_filter`,
   `allow_unprofitable`) and the runner's `max_drawdown_pct` all exist but the live config / scout
   leave them at off/unbounded defaults.
4. **Invalid symbol running.** `XAUD:USD` (a typo for `XAUT:USD`) was live for days, erroring
   `symbol: invalid` / HTTP 500 on every reconnect.
5. **Margins are real but thin.** Runners that *did* return to flat netted pennies–~$1 (XAUT runner 33:
   **+$1.41** over 2 days; SOL: **+$0.30**). Net economic PnL across the whole history ≈ **+$12** —
   noise. The big ±$20–30 swings are mark-to-market of inventory the bot never closed, not grid capture.
6. **Not a bug (correction):** realized-PnL persistence works — `src/runner/state.rs:129,163` writes
   `algorithm.realized_pnl()` into snapshots/rollups. The DB reads ~0 because nothing round-tripped.

### Capital constraints to honour

- **One shared USD wallet, ~$183 total** (≈ +$100 added later, one planned addition). Symbols will be
  chosen by a fresh `scout` run, not hard-coded here.
- Per-runner `capital` budgets must **sum to ≤ total liquid USD** across all concurrently-running
  runners (they share one wallet — independent "40% of full balance each" over-commits and recreates
  the buy-starvation bug).
- **Bitfinex enforces a minimum order size** (~$25 USD-equivalent / `qty × price ≥ ~25`). Below ~1
  fundable level per side a grid cannot work; the bot must **refuse + warn**, not silently run one-sided.

### Existing options the implementation must reuse (already parsed — do not re-invent)

- `grid.rs`: `levels`, `qty`, `spacing`, `initial_base_balance`, `initial_quote_balance`, `maker_fee`,
  `taker_fee`, `allow_unprofitable`, `trend_filter`, `trend_ema_period`, `trend_threshold`,
  `max_position`, `stop_loss_pct`.
- `runner/mod.rs` / `state.rs`: `max_drawdown_pct` (+ `peak_equity`, `halted`), `atr_timeframe`,
  `atr_period`, `atr_multiplier`. Drawdown halt logic already exists (`mod.rs:656-700`); `--resume`
  clears halt (`mod.rs:574-576`).

## Implementation plan

Ordered so each step is independently buildable/committable. Steps 1–3 are the profit-critical core.

### 1. Balance-aware sizing from a `capital` quote budget — `src/algorithm/grid.rs` (+ `runner/mod.rs` option plumbing)

- Add a new option **`capital`** (quote-currency budget this runner may deploy, e.g. `capital=60`).
  Keep `qty` as an optional **override**: if `qty` is given, use it as today (back-compat); if `qty`
  is absent and `capital` is present, **derive** `qty` (and possibly reduce `levels`) from the budget.
- Add option **`buy_reserve_frac`** (default `0.5`): the fraction of `capital` reserved to fund the
  buy ladder; the remainder is held as dry powder so buy-side replenishment after fills stays fundable
  ("split capital, reserve for buys" decision).
- **Sizing model** (compute at first `build_grid`, when a midpoint price is known):
  - Per-side buy notional budget `B = capital × buy_reserve_frac`.
  - `qty_from_quote = B / (levels_per_side × midpoint)`.
  - `qty_from_base  = initial_base_balance / levels_per_side` (sell ladder is funded by held base; if
    no base held, the grid is buy-first and the sell side grows from counter-sells as buys fill).
  - Uniform grid qty = `qty_from_quote` when no base is held, else `min(qty_from_quote, qty_from_base)`
    so **both** ladders are fundable. Round to the symbol's quantity precision.
  - **Minimum-order guard:** require `qty × midpoint ≥ min_notional` (configurable `min_notional`,
    default `25.0`). If the budget can't fund even one level at `min_notional`, **reduce
    `levels_per_side`** until it can; if it can't fund a single level, **emit no orders, flag the runner
    unprofitable/unfundable in `summary()`, and log a `warn`** (mirror the existing
    `allow_unprofitable=false` refuse-to-build path).
- **Runner plumbing (`runner/mod.rs`):** resolve `capital`/`buy_reserve_frac`/`min_notional` the same
  way fees are resolved and inject into the options map before `build_algorithm` (same pattern as
  `initial_*_balance`). At spawn, compare `capital` to live free quote in `wallet_balances`; if
  `capital` exceeds currently-free quote, log a `warn` (other runners may legitimately hold the rest)
  — do not hard-refuse, since the wallet is shared.

### 2. Spot-only: never go net short — `src/algorithm/grid.rs`

- Track available base the grid may sell = `initial_base_balance + net bought via fills − net sold`
  (the `AvgCostBook.position` already represents net base; treat **position ≥ 0 as the invariant**).
- Before emitting any **sell** signal (initial `build_grid` sells, counter-sells in `on_fill`,
  cross-level sells in `on_tick`), require that the cumulative sell quantity does not drive
  `position` below `0`. Skip/clamp sells that would short. This is the strategy-level root fix for the
  phantom-short; the dispatcher's base-balance guard (`dispatch.rs:108-115`) remains the backstop.
- Consequence: `AvgCostBook::record_sell`'s "sell with no inventory" branch (books a 0-basis short)
  should now be unreachable in normal operation; add a `debug_assert!`/`warn` if it ever fires.

### 3. Exchange order-state authoritative for fills — `src/runner/mod.rs` (snapshot-diff path, ~line 420-480)

- **Stop auto-booking a fill from snapshot absence alone.** When a tracked pending order is absent from
  an `OrderSnapshot`, do **not** immediately call `on_fill` + `write_fill_to_db`. Instead resolve its
  fate against the exchange:
  - Reuse `fetch_order_history` (already used by `runner/reconcile.rs::sync_orders_after_reconnect`,
    the documented authoritative reconciler) to classify the missing id as **filled** or
    **cancelled**. Book a fill only when history confirms a fill; if cancelled, drop the order from the
    tracking maps **without** booking a fill.
  - Keep the direct WS `OrderFilled` event (`process_fill`, `mod.rs:703`) as the fast, authoritative
    path — it already only books orders present in the pending maps, which is correct.
- Keep the change minimal and well-commented; the existing NOTE at `mod.rs:443-449` should be updated
  to describe the new behaviour.

### 4. Turn on risk controls in the live config + scout output

- Confirm the runner enforces `max_position` and `max_drawdown_pct` (they do — `grid.rs` gates on
  `max_position`, `mod.rs:656` halts on drawdown). No new code expected; if a wire is missing, fix it.
- Establish a **recommended live option set** (documented in the plan handoff and emitted by scout):
  `allow_unprofitable=false`, `max_position=<≈ levels × qty>`, `max_drawdown_pct=<e.g. 0.15>`,
  `stop_loss_pct=<e.g. 0.10>`, `trend_filter=ema`. Tune defaults conservatively for a small account.

### 5. Symbol hygiene — drop `XAUD:USD`, validate symbols at spawn

- Remove any `XAUD:USD` runner from `config.json` / `runner_state` resume (it's a typo for `XAUT:USD`).
- At runner spawn, **validate the symbol** against Bitfinex (e.g. a public ticker probe, or reuse the
  engine's existing symbol error) and **refuse to start** an unknown symbol with a clear one-line error
  instead of letting it run and spew `symbol: invalid` / HTTP 500 on every reconnect.

### 6. Update the `scout` skill — emit a shared `capital` budget, not a raw `qty`

File: `C:/Users/soufi/.claude/commands/scout.md` (the `/scout` command).

- **Step 4 (sizing):** allocate a **single shared budget across the final recommended set** so the
  per-symbol budgets **sum to ≤ total liquid USD** (today's `alloc = total × 0.40` *per symbol*
  over-commits a shared wallet). Emit `capital=<usd>` per symbol instead of (or alongside) a raw `qty`,
  and let the bot derive `qty` at runtime (step 1).
- **Step 6 (output):** include the recommended risk options from step 4 above in the `-o` line
  (`max_position`, `max_drawdown_pct`, `stop_loss_pct`, `trend_filter`, `allow_unprofitable=false`).
- Keep the `$25` minimum-order check and the "skip + explain if unfundable" behaviour; align the
  threshold name with the bot's `min_notional`.

### 7. (Secondary) short-position unrealized PnL — `src/algorithm/position.rs`

- With step 2 forbidding net short, this is low-priority, but note that `AvgCostBook::avg_cost()`
  returns `0` for `position ≤ 0`, so unrealized PnL on a short is misreported. If touched, mark a short
  position to market against its proceeds basis rather than 0. Otherwise leave with a `// TODO` and a
  one-line comment — do not expand scope.

## Out of scope

- **New strategy types** (momentum, scalper, market-maker) — deferred by decision; the grid stays.
  Revisit only after this lands and a backtest/forward-test shows the fixed grid is net-positive.
- **Margin / short selling** — the account is spot; net-short is forbidden, not supported.
- **Web dashboard** — still deferred (plan/00, 2026-06-18).
- **Exchange trait seam changes** (plan/05) — unrelated; don't refactor transport here.
- Rewriting historical `realized_pnl`/equity in the existing `ted.db` — leave history as-is; the fix is
  forward-looking.

## Validation

- `cargo build` and `cargo clippy` clean.
- **Unit tests (`grid.rs`):**
  - `capital` + no `qty` → derived `qty` such that `qty × midpoint × levels ≈ capital × buy_reserve_frac`.
  - Budget below one `min_notional` level → no orders emitted, `summary()` flags unfundable, `warn` logged.
  - Budget funds 2 of a requested 6 levels → `levels_per_side` reduced to 2 (or as designed), each level
    ≥ `min_notional`.
  - Held base = 0 → grid is buy-first, sell ladder empty until a buy fills (sanity: `open_sells` starts 0).
  - **No-short invariant:** with `initial_base_balance = N × qty`, the grid never emits a sell that
    drives `position` below 0; an over-sell signal is skipped/clamped.
  - `qty` provided explicitly → behaves exactly as before (back-compat).
- **Unit/integration test for fills (`runner`):** an order absent from a snapshot whose
  `fetch_order_history` reports *cancelled* → **no** fill booked and **no** position change; reported
  *filled* → fill booked once. (Mock the history fetch.)
- **Risk:** existing drawdown-halt test still passes; add/confirm `max_position` suppresses buys.
- **Backtester (plan/03):** run `backtest -s <SYM> --from-file <history csv/jsonl>` for a candidate
  symbol at the derived sizing and confirm the report shows net-positive (or at least non-negative)
  realized PnL over the sample, and that the no-short invariant holds in replay.
- **Manual paper run:** fund/point at the paper account, run a `scout`-recommended config for a session;
  confirm (a) **both** `open_buys` and `open_sells` are non-zero (two-sided!), (b) no `Insufficient USD`
  buy-skips at the intended sizing, (c) `position` never goes negative, (d) `generate`/report shows
  fee-adjusted realized PnL and trend/halt fields, (e) no `XAUD`/`symbol: invalid` errors.
