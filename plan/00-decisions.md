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