# T.E.D + Lara on a Linux VPS — setup guide

How to run the headless self-guarding T.E.D (ted/plan/12) with Lara as its operator
(lara plan/2026-09) on a Linux VPS. **Start on the paper account** — everything below
uses `paper: true` until you deliberately switch.

Order: Ollama → build T.E.D → config → start headless (paper) → SMTP → Lara operator.

```
your Bitfinex (paper)      ┌──────────────── VPS (systemd) ─────────────────┐
        ▲  orders/fills     │  T.E.D (headless)  ──HaltBuys/exits──▶ runners  │
        └───────────────────┼─▶ control surface (127.0.0.1:8787, token)      │
                            │        ▲ commands            ▲ status          │
   email ◀── breaker ───────┤        │                     │                 │
                            │      Lara operator ──judgment──▶ Ollama (local) │
                            └─────────────────────────────────────────────────┘
```

---

## 1. Ollama + a small model

The model only writes alert prose and makes the occasional edge-vs-artifact judgment
— it is **never** in the trade path, so a small model is fine.

```bash
curl -fsSL https://ollama.com/install.sh | sh
sudo systemctl enable --now ollama          # serves on 127.0.0.1:11434
ollama pull llama3.2:3b                      # ~2 GB; the operator's chat model
ollama pull nomic-embed-text                 # only if you also run Lara's vault RAG here
```

Verify: `curl 127.0.0.1:11434/api/tags` lists the models. If RAM is tight, `llama3.2:1b`
also works — quality of the *narrative* drops, the trading logic is unaffected.

## 2. Build T.E.D on the VPS

```bash
sudo apt update && sudo apt install -y build-essential pkg-config libssl-dev git
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"
git clone git@github.com:graviaDaemon/ted.git && cd ted
cargo build --release                        # binary at target/release/ted
```

## 3. Bitfinex key (paper) + config.json

- On Bitfinex, create an API key and **disable the Withdraw permission** — the key on
  the VPS must be physically unable to move funds. Enable Orders + Wallets (read) only.
- For paper trading, put your paper credentials in `api.paper_key` / `api.paper_secret`
  and set `startup_defaults.paper: true` (the manifest runners below also set `paper: true`).

Copy `config.template.json` → `config.json` and fill in the `api` block and the new
`operator` block. The template already has a working paper example; the parts to set:

```jsonc
"operator": {
  "runners": [
    { "symbol": "tSOLUSD", "algorithm": "grid", "paper": true,
      "options": { "atr_multiplier": "0.5", "atr_timeframe": "30m", "levels": "4", "capital": "150" } }
  ],
  "guardrails": {
    "max_monthly_loss_pct":            10.0,     // breaker trips past this MTD realized loss
    "max_capital_per_pair":            200.0,    // governor rejects bigger allocations
    "whitelisted_pairs":               ["tSOLUSD", "tXMRUSD"],
    "min_days_between_config_changes": 14,       // anti-churn / overfit-chasing
    "month_baseline_capital":          320.0     // denominator for the monthly-loss %
  },
  "control": { "bind": "127.0.0.1:8787", "token": "<LONG RANDOM STRING>" },
  "email":   { "smtp_host": "...", "smtp_port": 587, "starttls": true, "user": "...",
               "pass": "...", "from": "...", "to": ["you@gmail.com"] },
  "ollama":  { "endpoint": "http://127.0.0.1:11434", "model": "llama3.2:3b" },
  "breaker_interval_secs": 900
}
```

**About the `control` block** — this is the headless control surface: the only way to
drive a `--headless` T.E.D (there's no prompt). `bind` is the loopback address:port it
listens on — keep it `127.0.0.1` so nothing off the VPS can reach it (Lara runs on the same
box). `token` is a shared secret every command must carry (`<token> status`), and it **must
match** Lara's `TED_CONTROL_TOKEN`. Generate one with `openssl rand -hex 32`. Keep
`config.json` private: `chmod 600 config.json`.

## 4. Start T.E.D headless (paper)

```bash
./target/release/ted --headless      # spawns the manifest, arms the breaker, opens the control surface
```

You should see `Headless operator running.` and `Control surface listening on 127.0.0.1:8787.`

> **Headless vs. the TUI (and your tmux habit).** `./ted` with no flag is the interactive
> TUI you drive in `tmux attach -t ted`. `./ted --headless` is a *different* run-mode: no
> prompt — you interact over the control surface (below), not by attaching. Run only **one**
> at a time on the same account/DB (a single engine owns the exchange connection). You can
> still run `--headless` inside tmux if you prefer watching stdout and doing without systemd;
> you just give up auto-restart and start-on-boot. Keep the plain-TUI tmux session for manual
> poking — but stop the headless process first so they don't fight over the account.

Test the surface (replace TOKEN):

```bash
printf 'TOKEN status\n' | nc -N 127.0.0.1 8787      # JSON: equity, MTD PnL, per-runner, hold flag
printf 'TOKEN runner -s tSOLUSD -f\n' | nc -N 127.0.0.1 8787   # finish-exits (retire when flat)
```

### Run it under systemd

`/etc/systemd/system/ted.service`:

```ini
[Unit]
Description=T.E.D headless trader
After=network-online.target ollama.service

[Service]
Type=simple
WorkingDirectory=/home/YOU/ted
ExecStart=/home/YOU/ted/target/release/ted --headless
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload && sudo systemctl enable --now ted
journalctl -u ted -f            # live logs (headless prints to stdout)
```

## 5. SMTP so it can email you (Gmail example)

Gmail needs an **App Password**, not your normal password:

1. Enable 2-Step Verification on the Google account.
2. Google Account → Security → **App passwords** → generate one for "Mail".
3. Put the 16-character password in `operator.email.pass`, your address in `user`/`from`,
   and your inbox in `to`. Host `smtp.gmail.com`, port `587`, `starttls: true`.

The breaker emails on a monthly-loss trip; the body is written by the local model with a
plain-text fallback, so it sends even if Ollama is down. Other providers work too — set
`smtp_host`/`smtp_port`; use port 465 with `starttls: false` for implicit TLS.

## 6. Lara operator

```bash
git clone git@github.com:graviaDaemon/lara-raith.git && cd lara-raith
npm ci --include=dev      # typescript/tsx are devDependencies; --include=dev builds even under NODE_ENV=production
cp .env.example .env      # set TED_CONTROL_TOKEN to match config.json; TED_CHAT_MODEL=llama3.2:3b
npm run build             # tsc → dist/ (if you get "tsc: not found", dev deps didn't install — see above)
node dist/operator-main.js ted-status   # read-only sanity check against the running T.E.D
node dist/operator-main.js operate      # one health-check pass (Phase 0: read-only unless TED_OPERATOR_RETUNE=true)
# Runtime runs the compiled dist (no tsx needed). Optional: `npm prune --omit=dev` after building.
#
# NOTE: the operator has its own entry point (operator-main.js) that does NOT load the
# vault's native sqlite stack (better-sqlite3 / sqlite-vec). Do not use `dist/cli.js` for
# the operator — that entry pulls in the RAG DB you don't run on the VPS.
```

Run `operate` on a timer (better than an in-process loop). `/etc/systemd/system/lara-operate.service`:

```ini
[Unit]
Description=Lara T.E.D operator pass
After=ted.service
[Service]
Type=oneshot
WorkingDirectory=/home/YOU/lara-raith
ExecStart=/usr/bin/node dist/operator-main.js operate
EnvironmentFile=/home/YOU/lara-raith/.env
```

`/etc/systemd/system/lara-operate.timer`:

```ini
[Unit]
Description=Run Lara operator periodically
[Timer]
OnBootSec=5min
OnUnitActiveSec=6h
[Install]
WantedBy=timers.target
```

```bash
sudo systemctl daemon-reload && sudo systemctl enable --now lara-operate.timer
```

Keep `TED_OPERATOR_RETUNE` unset (or false) until you are live and have watched a few
health passes — with it off, `operate` only reads and reports.

## 7. When the breaker trips

- New buys halt account-wide, resting exits keep working, a **persistent hold** is set
  (a restart re-halts on boot — it never silently resumes), and you get an email.
- Investigate, then clear it deliberately once month-to-date loss is back within the limit:

```bash
printf 'TOKEN clear-hold\n' | nc -N 127.0.0.1 8787
```

The governor refuses `clear-hold` while the month is still beyond `max_monthly_loss_pct`,
so you can't accidentally re-arm a losing month.

## Going live (later, not now)

Flip `paper` to false on the manifest runners and set live `api.key`/`secret` (still
**withdrawals disabled**). Respect the T.E.D evaluation hold: no live config changes
before 2026-09-29.
```
