# 03 — Offline backtesting / historical-replay harness

## Goal

A deterministic, fee-aware backtester that replays historical market data through the **same**
`Algorithm` trait implementations used live, producing a PnL/stats report. This is the validation
loop the bot never had: prove a strategy and its parameters are net-profitable before risking money.

## Context

- Source request: `requests/2026-06-initialization.md`. Decisions: `plan/00-decisions.md` (2026-06-18).
- Depends on **plan/02** (fee-aware grid, `position()` hook) and **plan/01** (logging).
- The `Algorithm` trait (`src/algorithm/traits.rs`) is the seam: `on_tick(&MarketData) -> Vec<TradeSignal>`,
  `on_fill(price, is_buy, current_price) -> Vec<TradeSignal>`, plus `on_spacing_update`,
  `on_balance_update`, `summary`. Live dispatch logic lives in `src/runner/dispatch.rs` (order guards:
  insufficient balance, duplicate price, spread-cross, throttle). The backtester must reimplement a
  *simplified, honest* fill model — it does not reuse the async dispatch path.
- Candle fetching exists: `src/api/candles.rs::fetch_candles(symbol, timeframe, period, &Config,
  &client) -> Vec<Candle>` where `Candle { timestamp, open, close, high, low, volume }`. Bitfinex
  `hist` returns newest-first (`sort=-1`); the backtester needs oldest-first.
- `MarketData` requires bid/ask; candles have no spread. Model spread as a config parameter
  (`--spread` fraction, default `0.0` for Bitfinex parity; `bid = close*(1-spread/2)`,
  `ask = close*(1+spread/2)`).

## Implementation plan

### 1. New module `src/backtest/mod.rs` (+ submodules as needed)

- `pub struct BacktestConfig { symbol, algorithm, options, timeframe, candles_limit (or from/to),
  start_quote_balance, start_base_balance, spread, maker_fee, taker_fee }`.
- `pub struct BacktestReport { trades, realized_pnl_net, fees_paid, max_drawdown, win_rate,
  ending_equity, return_pct, final_summary: Option<String> }` with a `Display`/markdown renderer.

### 2. Data source

- Add a function to fetch a longer candle history (paginated if needed; Bitfinex caps ~10000/req).
  Reuse `fetch_candles` but allow a larger limit and a configurable timeframe. Sort ascending by
  timestamp. Optionally support loading candles from a local CSV/JSONL file
  (`--from-file path`) so backtests are reproducible offline without network — recommended, since the
  live box has limited connectivity guarantees and reproducibility matters for ranking strategies.

### 3. Replay engine — the honest fill model

Drive the algorithm candle-by-candle:

1. Build `Box<dyn Algorithm>` via `build_algorithm(name, options)` (same registry as live, so Rhai
   scripts and builtins both work).
2. Maintain a virtual book: `open_buys: Vec<(price, qty)>`, `open_sells: Vec<(price, qty)>`,
   `quote_balance`, `base_balance`, `realized_pnl`, `fees_paid`, `equity_curve: Vec<f64>`.
3. For each candle (ascending):
   - Synthesize `MarketData` from the candle (mid = close; bid/ask from spread; high/low/volume passed
     through; timestamp from candle).
   - Call `on_tick`; route returned signals into the virtual book honoring the **same guards** the live
     dispatcher uses where they make sense offline: skip if insufficient virtual balance, skip
     duplicate price, skip spread-crossing orders. (Throttle is irrelevant offline — ignore.)
   - **Fill resolution (the key modeling choice):** a resting limit **buy** at `p` fills during this
     candle iff `candle.low <= p` (price traded down through it); a resting limit **sell** at `p` fills
     iff `candle.high >= p`. This is the standard conservative OHLC fill assumption. On fill:
     - apply fee (`maker_fee` for limit fills), update balances, call `algorithm.on_fill(p, is_buy,
       mid)` and route any follow-on signals into the book (allowing chained grid extensions within the
       same candle, with a sane max-iterations cap to prevent infinite loops on a buggy algorithm).
     - record a trade row and update `realized_pnl`/`fees_paid` with the same average-cost accounting as
       plan/02 (factor that accounting into a shared helper if practical, so live and backtest PnL are
       computed identically — strongly preferred to avoid drift).
   - Append current equity (`quote + base*mid`) to `equity_curve`.
4. After the loop: compute max drawdown from `equity_curve`, win rate from trades, return %, and call
   `algorithm.summary()` for the strategy's own view.

### 4. CLI / TUI entry point

- Add a `backtest` subcommand to `src/commands/cli.rs` (`RunCommand::Backtest(BacktestCommand)`) and a
  `CliAction::Backtest { … }`, mirroring the existing `runner`/`generate` parsing (reuse
  `parse_options`). Flags: `-s/--symbol`, `-a/--algorithm`, `-o/--option`, `--timeframe`,
  `--limit` (candles), `--from-file`, `--spread`, `--maker-fee`, `--taker-fee`,
  `--start-quote`, `--start-base`.
- Wire it in `main.rs`'s command dispatch alongside the others. Backtest runs **synchronously** (it's
  CPU/IO-bound, not a long-lived task) and prints the report to the TUI and writes a markdown file
  (`backtest_<SYM>_<date>.md`) next to the overview files.

### 5. Make algorithms replay-safe

- Audit builtins for wall-clock or I/O dependence inside `on_tick`/`on_fill`. `GridBot` uses
  `crate::logger::log` — that's fine (logging works in-process). Ensure no `Instant::now`/network calls
  leak into algorithm logic. ATR-derived spacing: in backtest, compute spacing from the leading candles
  (same `compute_atr`) before replay starts, or accept an explicit `spacing` option.

## Out of scope

- Parameter sweep / optimizer (grid-search over option ranges) — valuable follow-up, but ship the
  single-run backtester first. Note it as a future changeset.
- Tick-level (sub-candle) replay — OHLC fill model is sufficient and matches available data.
- Multi-symbol portfolio backtests — single symbol per run for now.

## Validation

- `cargo build` / `cargo clippy` clean.
- Determinism test: same inputs → byte-identical report twice.
- Sanity test on a synthetic candle series with a fixed `spacing` grid and `maker_fee=0`: a known
  oscillation produces the hand-computed number of round trips and PnL.
- Fee test: same series with a nonzero fee yields strictly lower PnL by the expected fee total.
- Cross-check: a backtest with `maker_fee=0`/`spread=0` over a flat-then-up series should never show a
  fake profit that the plan/02 average-cost accounting wouldn't also show live.
- Manual: backtest `grid` on BTCUSD over a few months of `1h` candles and confirm the report renders
  with PnL, drawdown, win rate, and the grid's `summary()`.
