# 01 — Foundations: leveled logging, config restructure, fee plumbing

## Goal

Lay the cross-cutting groundwork the rest of the overhaul depends on: a leveled logging system,
a restructured `config.json` (per-mode credentials, log level, retention, fee defaults), and the
per-runner fee options threaded through to `RunnerState`. No trading-behaviour change yet — this
changeset only adds capability that 02–05 build on.

## Context

- Source request: `requests/2026-06-initialization.md`. Decisions: `plan/00-decisions.md` (2026-06-18).
- Today `src/logger.rs` exposes a single flat `log(source, msg)` with no severity. The stated
  must-have for trace/debug/info/warn/error/critical is unmet. Log files rotate by date and archive
  after `log_retention` days already (`archive_old_logs`).
- `src/config/config.rs` has `ApiConfig { auth_endpoint, pub_endpoint, ws_endpoint, auth_ws_endpoint,
  key, secret }` and `StartupDefaults { throttle_ms, log_retention, atr_refresh_mins }`. There is a
  single key/secret; the README documents `credentials.paper_*` and `startup_defaults.paper` that do
  **not** exist in code. `Config::active_key()/active_secret()` already exist as the single seam where
  credentials are read.
- `RunnerMode` (`src/config/channels.rs`) is `Simulation | Live`. `--live`/`-l` selects Live.

## Implementation plan

### 1. Leveled logging — `src/logger.rs`

- Add `#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Eq, Ord)] pub enum LogLevel { Trace,
  Debug, Info, Warn, Error, Critical }` with `from_str` (case-insensitive; unknown → `Info`) and a
  `as_str()` for printing (`TRACE`/`DEBUG`/…).
- Add `static MIN_LEVEL: OnceLock<LogLevel>` set in `init` from a new `init(tx, retention, min_level)`
  signature. Default `Info` if unset.
- Add `pub fn logl(level: LogLevel, source: &str, msg: &str)` that drops the line when
  `level < MIN_LEVEL`, otherwise formats `[<ts> UTC] [<LEVEL>] <source> <msg>` and writes/sends as today.
- Keep the existing `pub fn log(source, msg)` as a thin wrapper = `logl(LogLevel::Info, source, msg)`
  so the ~hundreds of existing call sites keep compiling unchanged.
- Add convenience macros or free fns (`trace!/debug!/info!/warn!/error!/critical!`-style helper fns
  `log_trace(src,msg)` … ) — prefer simple free functions over macros to match the codebase's plain style.

### 2. Config restructure — `src/config/config.rs`

- Extend `ApiConfig` with optional per-mode credentials. Keep `key`/`secret` as the live pair for
  backward compatibility; add `paper_key: Option<String>`, `paper_secret: Option<String>` (serde
  `#[serde(default)]`). Endpoints stay shared (Bitfinex uses the same URLs for paper and live).
- Add to `StartupDefaults` (all `#[serde(default = ...)]` so existing configs still parse):
  - `log_level: String` (default `"info"`).
  - `snapshot_retention_days: u32` (default `30`) — consumed in plan/04.
  - `default_maker_fee: f64` (default `0.0`), `default_taker_fee: f64` (default `0.0`) — fallback when
    a runner doesn't pass `maker_fee`/`taker_fee` options.
  - `paper: bool` (default `false`) — start runners in paper mode by default.
- Replace `active_key`/`active_secret` with mode-aware accessors:
  `active_key(mode: RunnerMode) -> &str` / `active_secret(mode: RunnerMode)`. For `Live` return
  `api.key`; for a new `Paper` mode (added in step 4) return `paper_key` falling back to `key` with a
  one-time warning. To avoid a `config → channels` dependency cycle, take a small `enum CredentialMode
  { Live, Paper }` defined in `config.rs`, or accept a `&RunnerMode` if the import is clean. Pick
  whichever keeps the module graph acyclic; document the choice inline.
- Update `validate()` to check `paper_*` only when paper mode is selected, and to validate
  `log_level` parses to a known level.
- Update the error-message template string in `load_config` to mention the new fields.

### 3. Wire logging init — `src/main.rs`

- Where `logger::init(...)` is currently called, parse `config.startup_defaults.log_level` into
  `LogLevel` and pass it through the new `init` signature.

### 4. Paper mode — `src/config/channels.rs` + call sites

- Add `Paper` to `RunnerMode` (`Simulation | Paper | Live`). Update `mode_label` in
  `src/runner/mod.rs` (`"paper"`) and any exhaustive `match` on `RunnerMode` (search the crate).
- Paper behaves exactly like Live in `dispatch.rs`/`runner` (same code path, real orders) but reads
  paper credentials. The only branch difference from Live is which credentials the engine uses — see
  note below.
- **Credential routing caveat:** the `Engine` is spawned once with a single `Config` and currently
  reads `config.active_key()/secret()` for *all* REST/auth-WS. True per-runner credential switching is
  a larger change (multiple auth sockets) and is **out of scope here**. For this changeset: the engine
  selects credentials based on a single process-wide mode derived from `startup_defaults.paper` (or a
  `--paper` startup flag), not per-runner. Document this limitation in the README. Per-runner
  credential isolation can come with the exchange seam (plan/05) if needed.
- CLI: add `--paper` to `RunnerCommand` (`src/commands/cli.rs`) mutually exclusive with `--live`;
  thread into `CliAction::Spawn`. If neither flag and `startup_defaults.paper` is true → Paper, else
  Simulation (unchanged default).

### 5. Per-runner fee options — `src/runner/state.rs`

- Add `pub maker_fee: f64` and `pub taker_fee: f64` to `RunnerState`.
- In `run_runner` (`src/runner/mod.rs`), resolve fees from options first, else
  `config.startup_defaults.default_*_fee`:
  `options.get("maker_fee").and_then(parse).unwrap_or(config…default_maker_fee)`.
- Do **not** apply fees to behaviour yet — plan/02 consumes these fields. This changeset just makes
  them available and logged at runner start (`info`).

## Out of scope

- Applying fees to PnL or order logic (plan/02).
- Per-runner credential isolation across different exchange accounts (noted above).
- Removing `Simulation` mode.
- Any storage schema change (plan/04).

## Validation

- `cargo build` and `cargo clippy` clean.
- Existing `config.json` (old shape) still loads — verify by running with the current file; new fields
  fall back to defaults.
- Add a unit test for `LogLevel::from_str` (case-insensitive, unknown → Info) and for level filtering
  (a `Trace` line is dropped when min level is `Info`).
- Add a unit test that `Config` parses both the legacy shape (no `paper_*`, no `log_level`) and the
  new shape.
- Manual: set `log_level: "debug"`, start a `passive` runner, confirm debug lines appear; set
  `"warn"`, confirm info/debug suppressed.
- Update `README.md` config table and `config.template.json` with the new fields.
