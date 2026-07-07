# Decisions Log

Append-only running log of decisions across **all** changesets for T.E.D.
Newest entries go at the bottom. Never rewrite or delete earlier entries —
this is a historical record, not a snapshot. Each changeset's full plan lives in
`NN-<slug>.md`; the reasoning for individual decisions lives here.

---

## 2026-06-18 — Profitability overhaul, foundations (see plan/01–05)

Context: T.E.D ran live for months and earned nothing. Root-cause analysis of the code (not the
infrastructure) found the money problem lives in strategy/validation/risk, not transport. The
async actor core (engine + runners) is sound and is kept. Request: `requests/2026-06-initialization.md`.

- **Decision:** Evolve the existing codebase rather than rewrite from scratch.
  **Why:** The Tokio engine/runner actor model is well-built and is not the cause of losses. A
  rewrite would burn weeks recreating working transport. The defects are in PnL accounting (fee-blind),
  the absence of an offline validation loop, and missing risk controls.

- **Decision:** Trading fees are configurable per runner via `maker_fee` / `taker_fee` options
  (fractions, e.g. `0.001`), defaulting to `0`.
  **Why:** User confirmed their Bitfinex situation has no trade fee, but other markets do. Per-runner
  config keeps Bitfinex at zero while letting fee-bearing markets model costs correctly. Fees feed
  both live PnL and the backtester so a strategy is only ever judged on net-of-fee profit.

- **Decision:** Fix the grid to be fee-aware and trend-aware first; add new strategy *types* only
  later, once the backtester can rank them.
  **Why:** Lowest-risk path to stop the bleeding. A grid that captures less than `2 × fee × price`
  per cycle is structurally unprofitable, and a grid with no trend filter bleeds inventory in trends.
  Both are fixable in the existing `GridBot` without new strategy machinery.

- **Decision:** Build an offline backtesting / historical-replay harness that runs the *same*
  `Algorithm` trait against historical candles, fee-aware.
  **Why:** Highest-leverage addition. The bot was never validated before risking real money. Replaying
  through the real `Algorithm` trait guarantees backtest behaviour matches live behaviour.

- **Decision:** Persist time-series for analytics in tiers — per-runner snapshots (high-frequency,
  pruned after N days), all fills (kept long-term), and daily rollups (kept long-term). Retention
  days are configurable.
  **Why:** Limited disk on the live box. Raw high-frequency data is only useful short-term; fills and
  daily PnL rollups are small and valuable forever. Feeds a future read-only web dashboard.

- **Decision:** Persist runner + strategy state to the DB so a runner resumes exactly where it left
  off after a rebuild/restart. Builtins serialize real state; Rhai scripts get best-effort resume
  (re-adopt open orders + options; internal script state resets).
  **Why:** User requires "rebuild and pick up where it left off." Rhai state lives in the engine's
  scope and is not cleanly serializable, so best-effort is the honest contract there.

- **Decision:** Introduce an `Exchange` trait seam now, with Bitfinex as the sole implementation.
  **Why:** User wants multi-market eventually. Abstracting while already refactoring is cheap;
  retrofitting later is painful. No second exchange is implemented in this overhaul.

- **Decision:** Web dashboard is **out of scope** for this overhaul. Control stays in the T.E.D TUI.
  **Why:** User explicitly deferred it. Storage tiering (plan/04) is designed so a future read-only
  dashboard can read the same SQLite DB without further schema work.

- **Decision:** Add leveled logging (trace/debug/info/warn/error/critical) with a configurable
  `log_level`, and per-mode (paper/live) credentials in config.
  **Why:** Stated must-have (leveled logging) is currently unmet — `logger.rs` is flat. The README
  already promises per-mode credentials that the code lacks; aligning code to the documented contract.

- **Decision:** Deprecate but do not yet remove `Simulation` runner mode.
  **Why:** It models perfect instant fills and is misleading for validation; the backtester replaces
  its purpose. Removing it touches many call sites — defer to avoid scope creep. Keep it compiling.

Changeset order (by dependency): 01 foundations (config + logging) → 02 fee/trend/risk grid →
03 backtester → 04 storage tiering + resume → 05 exchange trait seam.

---

## 2026-06-23 — Make the grid deployable: balance-aware sizing, no-short fills, live risk (see plan/06-grid-deployable.md)

Context: After the 01–05 overhaul shipped, the bot was run live again and still made ~$0. Root-cause
analysis of the uploaded history (`requests/history/`: `ted.db`, `logs/`, `trades/` — 39 runner
sessions, 2,082 fills, 14,299 snapshots, Apr→Jun 2026) found the failures are **operational, not
strategy-design**: the grid concept is sound but is starved of capital, corrupted by a phantom-fill
bug, and its risk controls are left off. Request: `requests/2026-06-profitability.md`. The full
empirical diagnosis is summarized in the plan file.

- **Decision:** Fix the operational foundation before adding any new strategy type. The grid stays;
  no momentum/scalper algo this round.
  **Why:** The data shows a *correctly-running two-sided grid is exactly the "profit on small moves,
  regardless of direction" behaviour the user asked for.* A new algorithm would inherit the same
  capital, sizing, and fill-accounting defects and produce equally untrustworthy results. Validate the
  grid on the existing backtester (plan/03) first; revisit new strategies as a later changeset.

- **Decision:** Order sizing becomes **balance-aware at the grid/runner runtime**, driven by a
  per-runner `capital` (quote budget) option rather than a raw fixed `qty`.
  **Why:** The proximate cause of "no profit" was undercapitalization: the live wallet held ~$3.60 USD
  while each grid level needed ~$24, so **every buy was skipped** (`Insufficient USD … — skipping BUY`)
  and the grid ran sell-only, drifting one direction and never round-tripping (runner 39 ran 5 days
  with `open_buys = 0`). A static `qty` baked at spawn cannot adapt to the real wallet. Scout already
  sizes against a portfolio, but it emitted a fixed `qty` for a portfolio that wasn't actually funded.

- **Decision:** Capital is **one shared ~$183 USD wallet** split across whatever symbols scout selects;
  sizing reserves a fraction of each runner's budget to keep the buy side fundable ("split capital,
  reserve for buys"). Scout assigns per-symbol budgets summing to ≤ total liquid USD.
  **Why:** User confirmed a single ~$183 account (≈ +$100 later), symbols TBD via a fresh `scout` run.
  Independent per-symbol 40%-of-full allocation (scout's old Step 4) over-commits one shared wallet
  when several runners run at once — that recreates the buy-starvation bug in a new form.

- **Decision:** The grid is **spot-only and must never go net short.** Cap emitted sells at held base
  inventory; the bot may not sell base it does not hold.
  **Why:** Runner 39 booked a phantom **−21.91 XRP** short after a `SELL FAILED: not enough exchange
  balance` event, driving reported equity to −$20. Forbidding net-short at the strategy level is the
  clean root fix and matches the account's spot (non-margin) reality.

- **Decision:** Make exchange order state **authoritative for fills**; stop booking a fill from an
  order's mere absence in a snapshot. Resolve "missing" orders via `fetch_order_history` (filled vs
  cancelled) before booking, keeping WS `OrderFilled` as the fast path.
  **Why:** `runner/mod.rs` snapshot-diff self-documents (lines ~443) that an out-of-band cancel is
  booked as a phantom fill. Phantom fills corrupt position, equity, and PnL.

- **Decision:** Turn the plan/02 risk controls **on** in the live config and scout output:
  `max_position`, `max_drawdown_pct`, `stop_loss_pct`, `trend_filter`, `allow_unprofitable=false`.
  **Why:** They shipped but the live config left them at off/unbounded defaults (zero log hits for
  `trend`/`drawdown`/`halt`/`max_position`/`unprofitable`). The safety net designed in the overhaul
  was never actually engaged.

- **Decision:** Drop the invalid `XAUD:USD` symbol (a typo for `XAUT:USD`) and validate a symbol at
  spawn, refusing unknown symbols with a clear error instead of running a perpetually-erroring runner.
  **Why:** `XAUD:USD` ran for days emitting `symbol: invalid` / HTTP 500 on every reconnect.

- **Correction (not a bug):** Realized-PnL **persistence is fine** — `runner/state.rs` writes
  `algorithm.realized_pnl()` into snapshots/rollups. The DB reads ~0 because the capital-starved
  runners never completed round-trips (a sell with no inventory realizes 0 under avg-cost), not
  because the writer drops it. Secondary gap noted: `AvgCostBook::avg_cost()` returns 0 for short
  positions, so unrealized PnL on a (now-forbidden) short is misreported.

---

## 2026-07-07 — TUI dashboard rewrite (see plan/07-tui-dashboard.md)

Context: Request `requests/2026-07-ui-and-gains.md` asks for two things: (a) a clearer TUI
(per-coin profit/loss bar graph + last-trades panel + command line, instead of raw log scroll)
and (b) another round of profitability work ("barely making money" after plan/06 ran live
~2 weeks). The request is split into two changesets.

- **Decision:** Split the request into two plans: `plan/07` (TUI dashboard, this entry) and
  `plan/08` (gains — written later).
  **Why:** The TUI is fully plannable from the current code. The gains work follows the plan/06
  pattern: the user uploads the server's fresh data dir (`ted.db`, `logs/`, `trades/`, everything
  since 2026-06-23) into `requests/history/`, an empirical diagnosis is done during planning, and
  plan/08 is written around what the data shows — not around guesses.

- **Decision:** Plan/08's user-selected levers are: **diagnosis-first**, a **backtest parameter
  sweep** command (grid-search spacing/levels/capital over historical candles, rank by net PnL),
  and **profit compounding** (roll realized PnL back into the runner's `capital` budget).
  Symbol-switch guidance was offered and not selected as a code lever (scout covers it
  operationally).
  **Why:** User choice 2026-07-07. Recorded here so plan/08's scope is fixed before the data
  arrives.

- **Decision:** Rebuild the TUI on **ratatui**, replacing the hand-rolled crossterm cursor
  positioning in `tui.rs`.
  **Why:** The requested layout (bordered panels, a bar graph, a trades list, resize handling)
  is exactly what ratatui provides declaratively. Hand-rolling panel clipping and bars on raw
  cursor moves is *more* code and more fragile, not less — this is the anti-overengineering
  choice despite being a new dependency. ratatui sits on top of crossterm, which is already a
  dependency.

- **Decision:** Two full-screen views toggled with **Tab**: **Dashboard** (PnL bars left,
  last trades right, one-line warn/error notice, command line bottom) and **Logs** (the current
  scrollback + the same command line). The most recent warn/error is surfaced on the dashboard.
  **Why:** The request mockup has no log area, but the log stream is the primary way fills,
  warnings, and halts are observed today. A toggle keeps both without cramping either. User
  chose this over a split layout or file-only logs.

- **Decision:** The dashboard is fed by widening the existing global ticker channel into a
  `TuiEvent` enum (`Ticker`, `Fill`, `Status`, `RunnerStopped`) emitted from the runner's
  existing hook points: `write_fill_to_db` (fills) and the snapshot tick / `persist_periodic`
  (periodic status). No new bookkeeping or polling of runner internals.
  **Why:** The runner already computes realized/unrealized PnL, equity, position, and open-order
  counts every snapshot tick, and every fill already funnels through one method. Emitting events
  from those two points keeps a single source of truth and costs a few lines per hook.

- **Decision:** The bar graph shows per-symbol **session PnL = realized + unrealized**, as
  horizontal bars custom-rendered (colored green/red by sign, scaled to the largest absolute
  value on screen).
  **Why:** Session PnL is what "is the bot making money right now" means; unrealized must be
  included or inventory drift is invisible (the plan/06 diagnosis showed mark-to-market swings
  dwarf realized capture). Custom rendering because ratatui's `BarChart` is unsigned — it cannot
  show losses below a zero axis.

- **Decision:** The last-trades panel **preloads the most recent fills from SQLite at startup**
  (new `recent_fills` query joining `fills` × `runners`), then appends live fills.
  **Why:** Otherwise the panel is empty after every restart even though the DB holds the full
  fill history. The fills table already stores direction, price, quantity, realized PnL, and
  timestamp — no schema change needed.

---

## 2026-07-07 — Gains round 3: ungate exits, rework trend filter, re-center, compound, sweep (see plan/08-grid-gains.md)

Context: After plan/06 deployed (2026-06-23), one live SOLUSD runner earned **+$8.25 realized /
+$10.12 unrealized in 2 weeks** on ~$330 equity — then went **5 straight days with zero fills**.
Fresh history uploaded to `requests/history/` (runners 40–41, 19,887 snapshots, 12 fills).
Diagnosis: **all plan/06 fixes work** (two-sided build, capital sizing, no shorts, no buy-skips);
the remaining defect is in the grid's own maintenance loop. Full evidence in the plan file.

- **Decision:** Counter-orders (the exit side of a round trip) are **never gated by the trend
  filter**. A buy fill always places its counter-sell (subject only to the no-short inventory
  check); a sell fill always places its counter-buy (subject only to `max_position`/stop-loss).
  **Why:** The gate fired at fill time, one-shot, against a tick-EMA with `trend_threshold`
  defaulting to `0.0` — effectively a coin flip biased exactly wrong. Runner 40 bought 6 levels
  down a falling market with **zero counter-sells ever placed** (`open_sells = 0` for its 5-day
  life; not one "counter sell" line in any log); runner 41 sold 4 levels up a rally with **zero
  counter-buys**. Suppressing exits doesn't reduce risk — it strands inventory and kills the
  round-trip engine that produces all realized PnL.

- **Decision:** The trend filter is **reworked, not removed** (user choice over defaulting it
  off): it gates **extension buys only** — adding new exposure below the ladder in a falling
  market — and blocks them only when the trend is *down* beyond a threshold. New trend measure:
  **candle-close EMA** fetched/refreshed by the runner (same pattern as the ATR refresh), not the
  per-tick EMA. `trend_threshold` default becomes **0.005** (was 0.0).
  **Why:** The shipped filter had inverted semantics for a grid: it *allowed* extension buys to
  march down the falling knife (runner 40 deployed ~$150 into the drop) while *blocking* the
  counter-sell exits. A tick-EMA(50) spans minutes and is noise, not trend. The one genuinely
  dangerous move for a spot grid is accumulating fresh exposure against a real downtrend — so
  that is the only thing the filter now touches.

- **Decision:** Add **fill-less re-centering**: when no grid level rests within a configurable
  band of the mid price (default 2.5 × spacing, on both sides, with a 5-minute cooldown between
  rebuilds), cancel the stale ladder and rebuild around the current mid. Rebuilds now **emit
  Cancel signals for every remaining tracked level** (the existing rebuild path just cleared
  internal maps, orphaning live orders on the exchange).
  **Why:** The grid only slid on fills, so a stalled grid could never recover: runner 41 sat
  5 days with 1 buy at 70.27 and 1 sell at 84.06 while mid oscillated ~79–83 — a 17% dead zone
  inside the rebuild trigger's ×0.95/×1.05 bounds, so the trigger could never fire. No fill → no
  slide → no fill, indefinitely.

- **Decision:** **Compounding = `capital + net realized PnL`** (profits grow the budget, losses
  shrink it), applied whenever sizing is re-derived — which now happens at every full rebuild,
  folding compounding into the re-centering mechanism for free.
  **Why:** User choice (over profits-only ratchet or manual). `AvgCostBook.realized_pnl` is
  already net of fees, so it is the honest budget delta; shrinking after losses automatically
  de-risks a small account.

- **Decision:** Add a **`sweep` command**: cartesian grid-search over spacing × levels for one
  symbol over one candle history (fetched once), each combination replayed through the existing
  plan/03 backtester, ranked by net realized PnL with max-drawdown as tiebreaker. Default spacing
  candidates derive from the candle set's ATR.
  **Why:** User-selected lever. Parameters (`spacing=1.97`, `levels=2`) are currently hand-picked
  by scout heuristics; the sweep replaces guesses with replayed evidence using machinery that
  already exists. The backtester already models Cancel signals and freezes counter-orders within
  a candle, so re-centering and the reworked filter are exercised faithfully in replay.

- **Decision:** Scout emits the new knobs explicitly (`trend_threshold`, trend timeframe) and its
  workflow gains a step: run `sweep` on the candidate symbols and take the top-ranked config
  instead of computing spacing by heuristic alone.
  **Why:** Round 2 established scout emits the full option set; leaving new options implicit is
  how `trend_threshold=0.0` happened.

- **Observation (for the record):** "Barely making money" was **not** under-performance of the
  strategy concept: +$8.25 realized in the ~3 days the grid was actually two-sided and trading
  annualizes to a healthy rate on this account size. The loss was the ~11 of 14 days spent
  one-sided or deadlocked. Fixing flow-through, not raising per-trade capture, is the lever.