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