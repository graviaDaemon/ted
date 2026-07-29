# plan/10 — Fee truth & equity truth

## Goal

Make the bot's numbers match reality: real account fees fetched from Bitfinex and used
everywhere (accounting, floors, sweeps), and displayed equity derived from total wallet balances
so the TUI number is the number the user could actually withdraw.

## Context

Source request: `requests/2026-07-current-status.md`. Decisions: `plan/00-decisions.md`
§ 2026-07-29. Depends on plan/09 (LotBook, spacing floor) being implemented first.

Two findings from the Jul 7–29 live history (`requests/history/`):

- The runner logged `Fees: maker 0.000000, taker 0.000000` — `config.json` left
  `default_maker_fee`/`default_taker_fee` at the scaffold's 0.0, and the `sweep`/`backtest` CLI
  defaults are also 0.0 (`src/commands/cli.rs`). Everything — live accounting, the fee floor,
  and the sweep that picked the config — ran fee-blind. Wallet-delta reconciliation of actual
  fills shows Bitfinex currently charges this account **~zero** fees (a buy of
  1.10562414 @ 81.89 credited exactly 1.10562414 SOL and debited exactly price×qty), so this was
  latent rather than the active leak — but the account's fee tier is state, not a guarantee.
- The user watched a "calculated value" of 300–320 while the real wallet held ~$345
  (267.10 USD + 1.05 SOL). Equity is computed as `quote_bal + position × mid`
  (`src/runner/state.rs:139`) where `quote_bal` tracks *available* balance updates (excludes
  order-locked funds) and `position` is the book's view, which can drift from the wallet.

## Implementation plan

### 1. `src/api/endpoints.rs` (+ `src/api/auth.rs` as needed) — account fee fetch

- Add an authenticated REST call to Bitfinex `POST /v2/auth/r/summary`. The response's fee
  section contains the account's exchange maker/taker rates
  (`fees_snapshot`: maker array / taker array — take the exchange-trading entries; log the raw
  fee block at debug level the first time so the field mapping is verifiable against the live
  account). Return `(maker_fee, taker_fee)`.
- Route it through the existing REST worker in the engine like other auth endpoints.

### 2. `src/runner/mod.rs` — fee precedence at spawn

- Precedence when resolving fees for a live/paper runner:
  1. explicit `-o maker_fee=`/`taker_fee=` option,
  2. live-fetched account fees (step 1),
  3. `config.startup_defaults.default_*_fee`.
- Log the resolved values **with their source**: `Fees: maker 0.001000, taker 0.002000 (account)`.
- If the fetch fails and the fallback is 0.0 in live mode, `log_warn` prominently (fee-blind live
  trading is exactly what shipped last round).

### 3. `src/config/config.rs` + `src/commands/cli.rs` — non-zero defaults

- Config scaffold template (`config.rs:134`): `default_maker_fee: 0.001`, `default_taker_fee: 0.002`.
- `BacktestCommand` and `SweepCommand`: `maker_fee` default `0.001`, `taker_fee` default `0.002`
  (instead of 0.0). Zero-fee replays now require passing `--maker-fee 0` explicitly.
- Update the config tests that assert the scaffold defaults.

### 4. `src/runner/state.rs` — wallet-truth equity + reconciliation

- Track **total** wallet balances (Bitfinex WS wallet updates carry the full wallet array;
  keep both total and available — total for equity, available for funding checks).
- Equity = `total_quote + total_base × mid` for the runner's symbol currencies.
- Periodic reconciliation (piggyback on `persist_periodic`): compare `book.position()` against
  the wallet's total base balance; if they diverge by more than one grid `qty` (tolerance for
  in-flight fills), `log_warn` with both numbers. Never auto-correct silently.
- `peak_equity`/drawdown-halt and daily rollups use the wallet-truth equity.

### 5. `src/tui.rs` — show the truth

Dashboard per-symbol line gains: fees paid, open-lot count, trend state (up/flat/down), and a
**trailing 7-day PnL%** computed from `daily_rollups` (SQLite) — `(equity_now − equity_7d_ago) /
equity_7d_ago`, falling back to "n/a" with fewer than 2 rollup days. This makes the
0.5–2%/week target directly observable on the screen the user already watches.

## Out of scope

- Strategy changes (plan/09). Sweep output format / scout / second pair (plan/11).
- Historical backfill of `daily_rollups` from the old session.
- Multi-currency fiat conversion (EUR balance stays out of equity; it's $0.53).

## Validation

1. `cargo build` + `cargo clippy` clean; `cargo test` green (config default tests updated).
2. Paper session: spawn `runner -s SOLUSD --paper`, confirm the `Fees: … (account)` log line shows
   the real fetched tier, and TUI equity ≈ Bitfinex wallet page (USD total + SOL×mid) within a
   few cents.
3. Kill/respawn: reconciliation logs book-vs-wallet position without warning under normal flow.
4. `sweep -s SOLUSD` with no fee flags now reports non-zero fees in its table.
