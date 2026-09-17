# Headless Executor + Control Surface ("earn her keep" on a VPS)

## Why

I want T.E.D to run unattended on a small Linux VPS and be driven by an autonomous
agent (Lara) that operates it — spawning/killing runners, following new pairs,
finishing exits, re-tuning — starting from the ~$320 it holds now, compounding all
growth, no new capital. The goal for now is **reliability, not riches**: a
dependably net-positive bot with a hard loss stop that escalates to Claude Code and
me for re-evaluation.

Today T.E.D is TUI-only and human-driven. On a VPS there is no human at the prompt,
so T.E.D must (a) run headless and (b) accept the same operations at runtime from an
external agent instead of the keyboard.

## Division of labour (two loops)

- **T.E.D owns the fast loop (per-tick, deterministic).** Order placement, fill
  handling, exit ladder, drawdown/loss tripwire. No LLM here, ever.
- **Lara owns the slow loop (periodic, strategic)** — see
  `lara-raith/request/2026-09-ted-operator.md`. It issues commands to T.E.D.

This request covers only what T.E.D must add to *be operated*: a headless mode, a
control surface, a runtime-safe circuit breaker, and un-bypassable guardrails.

## What to build

1. **Headless run-mode** (`ted --headless` / `ted operator`). No TUI raw-mode;
   spawns runners from a manifest at startup (reuse the runner registry + spawn
   path), logs to file, resumes state from `ted.db` (plan/04), graceful SIGTERM
   (systemd-managed, restart-on-crash).

2. **Control surface** — a local socket/HTTP endpoint exposing the verbs the TUI
   already dispatches via the `[CTRL]` handler in `main.rs`:
   - `spawn` a runner (`-s SYMBOL -a ALGO -o OPTS`), `kill`, **`finish-exits`**
     (stop opening, keep working the sell ladder until flat, then retire the runner),
   - `sweep` (returns the ranked report), `status` (equity, per-runner PnL,
     month-to-date, drawdown, hold-flag, last-change ts — JSON),
   - `resume` / `clear-hold` (only meaningful after a breach).
   Reuse the existing dispatch functions; the surface is a second caller onto them,
   not new trading logic.

3. **Runtime reconfig, no restart.** Config/pair changes apply by spawn/kill on the
   live process. The process and its runners are never bounced to pick up a new
   config.

4. **Circuit breaker that halts trading, not the process.** If month-to-date loss
   exceeds `max_monthly_loss_pct`: **halt new buys across runners, keep exits
   working, set a persistent hold flag, email me + Claude Code.** The process and
   control surface stay up so we can inspect and resume/reconfigure live. **No
   auto-resume** — `resume`/`clear-hold` is a deliberate human/agent action gated by
   the guardrails.

5. **Email** (escalation on breach + scheduled monthly report). Narrative written by
   the local Ollama model; delivery via plain SMTP (creds in config/env).

## Guardrails — enforced in T.E.D, un-bypassable by Lara

T.E.D is the safe governor: any control-surface command that would breach these is
**rejected** (and logged/escalated), no matter who sent it.
- `max_monthly_loss_pct` → breaker (above).
- Drawdown kill-switch (reuse existing `drawdown halt`).
- `max_capital_per_pair`, whitelisted pairs only.
- `min_days_between_config_changes` (rejects churn / overfit-chasing).
- **Withdrawals: not a capability.** No transfer code exists; the Bitfinex API key
  itself has withdrawals disabled.

## Security (non-negotiable)

- Bitfinex API key on the VPS **must have withdrawals disabled**.
- Control surface bound to localhost / unix socket only (Lara runs on the same box).
- Lock down `config.json` perms (key/secret + SMTP creds).

## Sequencing (respects the eval hold — no config changes before 2026-09-29)

- **Phase 0 (now → Sep 29): build + validate, zero live changes.** Headless mode,
  control surface, breaker, guardrails — validated against paper mode + backtester;
  Lara drives it against paper/historical data only.
- **Phase 1 (after Sep 29): live headless on the VPS**, breaker armed, Lara operating.

## Acceptance

- `ted --headless` runs with no TTY, spawns from manifest, survives SIGTERM+restart,
  resumes from `ted.db`.
- Every TUI operation (spawn/kill/finish-exits/sweep/status) is reachable over the
  control surface and does the same thing.
- A control command that breaches a guardrail is rejected, not executed.
- In a paper/backtest breach scenario: buys halt, exits keep running, hold flag set,
  email sent, process stays up, `resume` restores trading.
- Config/pair change applies with no process restart.

## Out of scope

- No LLM in the fast loop.
- No withdrawals / fund transfers, ever.
- No new trading strategy — operations plumbing over the existing engine.
- TUI stays as-is for local use; headless is an additional run-mode.
