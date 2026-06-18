# 05 — Exchange trait seam (Bitfinex as sole implementation)

## Goal

Abstract all exchange-specific I/O behind an `Exchange` trait so a second market can be added later
without touching the engine, runners, or algorithms. Bitfinex becomes one implementation of that
trait; no second exchange is built. This is a structural refactor with **no behaviour change**.

## Context

- Source request: `requests/2026-06-initialization.md`. Decisions: `plan/00-decisions.md` (2026-06-18).
- Best done last: it touches the most files and benefits from the fee/state/config work being settled.
- Today Bitfinex is hardcoded throughout `src/api/` and consumed directly by `src/engine/mod.rs`:
  - REST: `src/api/endpoints.rs` (`place_order`, `cancel_order`, `fetch_open_orders`,
    `fetch_order_history`), `src/api/candles.rs::fetch_candles` — all take `&Config, &reqwest::Client`.
  - WS: `src/api/websocket.rs` (`connect_authenticated`, `parse_ws_message`, `parse_auth_ws_message`),
    auth signing in `src/api/auth.rs` (HMAC).
  - Types: `src/api/types.rs` (`MarketData`, `TradeSignal`, `OrderResult`, `WsEvent`).
- The engine (`src/engine/mod.rs`) is the **only** consumer of these. It already wraps everything
  behind `EngineHandle` + `EngineRequest`/`EngineEvent`. That handle is the runners' sole interface and
  must remain unchanged — the seam goes *below* the engine, between engine and exchange.

## Implementation plan

### 1. Define the trait — new module `src/exchange/mod.rs`

Model the trait on what the engine actually calls. Keep `MarketData`/`TradeSignal`/`OrderResult`/
`WsEvent`/`Candle` as the shared, exchange-neutral vocabulary (move them to `exchange::types` or
re-export from there; least churn is to keep them in `api::types` and have `exchange` re-export).

```rust
#[async_trait] // add `async-trait` dep, or use return-position impl Trait if the methods allow
pub trait Exchange: Send + Sync {
    fn name(&self) -> &str;

    // REST (called from the engine's rest_worker)
    async fn place_order(&self, signal: &TradeSignal, symbol: &str) -> Result<OrderResult, String>;
    async fn cancel_order(&self, order_id: i64) -> Result<(), String>;
    async fn fetch_open_orders(&self, symbol: &str) -> Result<Vec<i64>, String>;
    async fn fetch_order_history(&self, symbol: &str) -> Result<Vec<(i64, String)>, String>;
    async fn fetch_candles(&self, symbol: &str, timeframe: &str, period: usize)
        -> Result<Vec<Candle>, String>;

    // WS lifecycle — return parsed, exchange-neutral events
    async fn connect_public(&self) -> Result<PublicStream, String>;
    async fn connect_auth(&self) -> Result<AuthStream, String>;
    fn subscribe_frame(&self, symbol: &str) -> String;          // exchange-specific subscribe payload
    fn parse_public(&self, raw: &str) -> WsEvent;
    fn parse_auth(&self, raw: &str) -> WsEvent;
}
```

- Exact method set should be derived by reading `src/engine/mod.rs` in full and listing every call into
  `api::*`; the trait must cover precisely those and nothing more. Adjust the WS portion to match how
  the engine currently drives the sockets (it owns the `WebSocketStream`s directly and calls
  `parse_ws_message`/`parse_auth_message`). Two viable shapes:
  - **(A, recommended) thin seam:** trait provides connection URLs, the subscribe frame, the auth
    signing payload, and the two parse functions; the engine keeps owning the socket select-loop. This
    minimizes risk because the resilient reconnect/backoff loop in the engine stays put.
  - (B) trait owns full streams — larger, riskier refactor. Avoid for this pass.
- Choose **(A)**. Concretely the trait exposes: REST methods above + `public_ws_url()`,
  `auth_ws_url()`, `auth_payload()` (HMAC-signed auth frame), `subscribe_frame(symbol)`,
  `parse_public(raw) -> WsEvent`, `parse_auth(raw) -> WsEvent`.

### 2. Bitfinex implementation — `src/exchange/bitfinex.rs`

- `pub struct Bitfinex { config: Config, http: reqwest::Client, mode: CredentialMode }`.
- Implement the trait by delegating to the existing functions in `api::endpoints`, `api::candles`,
  `api::websocket`, `api::auth` (move them under `exchange::bitfinex` or call them — moving is cleaner
  long-term but call-through is lower-risk; recommend call-through now, physical move deferred).
- Credential selection uses the plan/01 mode-aware accessors.

### 3. Rewire the engine — `src/engine/mod.rs`

- `spawn_engine` takes `exchange: Arc<dyn Exchange>` (constructed in `main.rs` from config) instead of
  reaching into `api::*` directly. Today it takes `Config`; change to take the boxed exchange (which
  itself holds the config it needs).
- `rest_worker` calls `exchange.place_order(...)` etc. instead of `api::endpoints::*`.
- The WS select-loop uses `exchange.public_ws_url()`, `subscribe_frame`, `parse_public`/`parse_auth`,
  `auth_payload`. Reconnect/backoff logic is unchanged.
- `EngineHandle`, `EngineRequest`, `EngineEvent` and all runner-facing APIs stay byte-for-byte the same.

### 4. Construction point — `src/main.rs`

- Build `let exchange: Arc<dyn Exchange> = Arc::new(Bitfinex::new(config.clone(), mode));` and pass to
  `spawn_engine`. Selection of which exchange is, for now, always Bitfinex (no second impl); leave a
  clearly-marked TODO/seam where a `config.exchange` field would dispatch later.

### 5. Backtester alignment (light)

- The backtester (plan/03) does not need the live `Exchange` (it replays candles directly), but its
  candle fetch can route through `exchange.fetch_candles` for consistency. Optional; keep the
  `--from-file` path independent of any exchange.

## Out of scope

- Implementing any non-Bitfinex exchange.
- Per-runner / per-exchange credential isolation beyond the single process-wide mode from plan/01
  (a true multi-account engine is a future changeset — note it).
- Changing `EngineHandle`/runner APIs or algorithm code (must remain untouched — this is the proof the
  seam is in the right place).

## Validation

- `cargo build` / `cargo clippy` clean.
- **Behavioural parity is the acceptance test:** a paper-mode grid run before and after this refactor
  must produce equivalent order placement/fill behaviour and logs. Diff a session's log lines (modulo
  timestamps).
- Confirm reconnect/backoff still works: kill network briefly during a paper run, confirm the engine
  reconnects and re-subscribes (existing `PublicWsReconnected`/`AuthConnected` events still fire).
- `cargo test` (units from prior changesets) still green.
- Grep the codebase to confirm no module outside `src/exchange/` and `src/engine/` imports `api::`
  REST/WS functions directly anymore (types may still be shared).
