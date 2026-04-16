# T.E.D — Trading Exchange Driver

---

DISCLAIMER: AI has been used as a tool to fix some bugs, streamline data-flow, and write parts of this README.

---

![Static Badge](https://img.shields.io/badge/T.E.D_--_Trading_Exchange_Driver-2.2.9-green)

A Rust-based algorithmic trading daemon for the Bitfinex exchange. Connects to the Bitfinex WebSocket v2 API, streams live market data, and executes configurable trading strategies.

---

## Prerequisites

- Rust toolchain (stable, 2024 edition)
- A Bitfinex account with API credentials
- `config.json` in the working directory (see below)

---

## Configuration

Copy `config.template.json` to `config.json` and fill in your credentials:

```json
{
  "api": {
    "auth_endpoint":    "https://api.bitfinex.com",
    "pub_endpoint":     "https://api-pub.bitfinex.com",
    "ws_endpoint":      "wss://api-pub.bitfinex.com/ws/2",
    "auth_ws_endpoint": "wss://api.bitfinex.com/ws/2",
    "key": "YOUR_API_KEY",
    "secret": "YOUR_API_SECRET"
  },
  "startup_defaults": {
    "throttle_ms":      300,
    "log_retention":    7,
    "atr_refresh_mins": 60
  }
}
```

| Field                               | Description                                                                                         |
|-------------------------------------|-----------------------------------------------------------------------------------------------------|
| `api.auth_endpoint`                 | Base URL for authenticated REST calls                                                               |
| `api.pub_endpoint`                  | Base URL for public REST calls (candle fetches)                                                     |
| `api.ws_endpoint`                   | Public WebSocket endpoint (ticker streaming)                                                        |
| `api.auth_ws_endpoint`              | Authenticated WebSocket endpoint (order events)                                                     |
| `api.key/secret`                    | API key and secret for your live Bitfinex account. This can be the paper-trading api or regular api |
| `startup_defaults.throttle_ms`      | Minimum milliseconds between ticks processed                                                        |
| `startup_defaults.log_retention`    | Days of log files to keep                                                                           |
| `startup_defaults.atr_refresh_mins` | How often (in minutes) to refetch ATR for dynamic grid spacing                                      |

### Setting up paper trading credentials

1. Log into your Bitfinex account
2. Go to **Account → Paper Trading** and enable the paper trading sub-account
3. Generate an API key specifically for the paper account (separate from your live key/secret)
4. Fill `credentials.paper_key` and `credentials.paper_secret` with those values
5. Set `startup_defaults.paper: true` to start runners in paper mode by default, or pass `--paper` at runtime

Paper mode uses the same Bitfinex endpoints as live trading — only the credentials differ. If paper credentials are absent, the runner refuses to start rather than silently falling back to simulation.

---

## Building and running

```
cargo build --release
./target/release/ted
```

The binary enters an interactive terminal session. Log output scrolls above the input line; the `> ` prompt stays pinned at the bottom. Use `Ctrl+C`, `Ctrl+D`, or type `exit` to quit gracefully.

---

## Commands

All commands are typed at the `> ` prompt and dispatched on Enter.

### `runner` — manage trading runners

Each runner is an independent async task that streams market data for one symbol and runs a single algorithm on each tick.

**Spawn a new runner:**
```
runner [-s, --symbol] BTCUSD
runner [-s, --symbol] BTCUSD [-a, --algorithm] grid [-o, --option] levels=5 qty=0.001 atr_period=14
runner [-s, --symbol] ETHUSD [-a, --algorithm] example [-o, --option] atr_period=30 qty=0.01
runner [-s, --symbol] LTCUSD [-a, --algorithm] grid [-o, --option] levels=5 qty=0.1 atr_period=14 [-l, --live]
```

| Flag                   | Short | Description                                                                |
|------------------------|------|----------------------------------------------------------------------------|
| `--symbol <SYM>`       | `-s` | Trading pair symbol (required)                                             |
| `--algorithm <NAME>`   | `-a` | Algorithm name: `passive`, `grid`, or any script name (default: `passive`) |
| `--option <KEY=VALUE>` |      | Algorithm options; repeat for multiple                                     |
| `--live`               | `-l` | Start this runner in live mode (defaults to simulation)                    |

**Lifecycle:**
```
runner -s BTCUSD --pause          # pause tick processing (WS stays connected)
runner -s BTCUSD --resume         # resume
runner -s BTCUSD --kill           # stop runner and cancel any open orders
```

**Switch algorithm on a running runner:**
```
runner -s BTCUSD --configure grid --option levels=5 qty=0.002 atr_period=14
```

There are two modes:
- **simulation** — no API calls; fills are simulated immediately on signal. Deprecated — use paper-trading API credentials instead for accurate testing.
- **live** — places and tracks real orders against the exchange account.

---

### `generate` — produce an overview report

Writes a Markdown report of a runner's activity to disk.

```
generate --runner BTCUSD           # single runner
generate --runner BTCUSD --verbose # include full signal table and PnL breakdown
generate --all                     # all running runners, combined file
generate --all --verbose
```

Output files are written to the current directory (`overview_BTCUSD_2026-03-22.md`, `overview_all_2026-03-22.md`).

---

### `exit`

```
exit
```

Gracefully stops all runners (cancels open orders), closes WebSocket connections, and exits.

---

## Algorithms

### Built-in: `passive`

Observes and logs every tick. Places no orders. Useful for monitoring without any strategy active.

```
runner -s BTCUSD --algorithm passive
```

### Built-in: `grid`

Places limit buy and sell orders at equidistant price levels. The grid is centred on the live price at the moment the first tick arrives. In paper/live mode, the initial grid size is constrained by available wallet balance.

The grid slides with price: when a buy fills, a counter sell is placed near the current price and a new buy is added below the lowest remaining buy, maintaining exactly `levels` orders on each side. The inverse applies when a sell fills. This allows the grid to walk indefinitely with a trending market without exhausting either side.

**Dynamic spacing via ATR**: when `atr_period` is supplied, spacing is derived from the Average True Range of recent candles (`spacing = ATR × atr_multiplier`). This makes the grid adapt to current market volatility. ATR is refreshed periodically based on `startup_defaults.atr_refresh_mins`.

| Option | Description | Required |
|--------|-------------|----------|
| `qty=<float>` | Order quantity per level in base currency | Yes |
| `levels=<int>` | Number of grid levels per side (minimum 1) | Yes |
| `spacing=<float>` | Fixed distance between levels in quote currency | Required if no ATR options |
| `atr_period=<int>` | ATR period in candles (enables dynamic spacing) | No |
| `atr_timeframe=<str>` | Candle timeframe for ATR (`1h`, `4h`, `1D`, etc.; default: `1h`) | No |
| `atr_multiplier=<float>` | Multiplier applied to ATR to get spacing (default: `1.0`) | No |
| `spread_ratio=<float>` | Fraction of spacing to offset buys below and sells above midpoint (default: `0.0`) | No |

```
runner -s BTCUSD --algorithm grid --option levels=5 qty=0.001 spacing=500
runner -s BTCUSD --algorithm grid --option levels=5 qty=0.001 atr_period=14 atr_timeframe=4h atr_multiplier=1.5
```

With `levels=5` and `spacing=500` the grid places 5 buy levels below and 5 sell levels above the midpoint, 500 quote units apart. Each fill seeds the opposite side and replenishes the same side at the extremity.

---

## Script algorithms (Rhai)

Place `.rhai` files in the `algorithms/` directory next to the executable. They are compiled and registered automatically — no restart required. The directory is watched continuously; dropping a new file in makes it available within ~500ms.

### Using a script

The script name is the filename without the `.rhai` extension:

```
runner -s BTCUSD --algorithm example --option period=20 qty=0.001
```

### Writing a script

Every script has access to an `options` map pre-populated with the values passed via `--option`. All top-level (non-function) code runs once when the runner spawns. State variables declared at the top level persist across all calls to `on_tick`.

**Required function:**
```rhai
fn on_tick(price, bid, ask, volume) {
    // Return () for no signal, or a signal map, or an array of signal maps
    return ();
    return #{ kind: "buy",  price: price, qty: qty, reason: "some reason" };
    return #{ kind: "sell", price: price, qty: qty, reason: "some reason" };
    return [
        #{ kind: "buy",  price: level_a, qty: qty, reason: "crossed A" },
        #{ kind: "sell", price: level_b, qty: qty, reason: "crossed B" },
    ];
}
```

**Optional function:**
```rhai
fn summary() {
    `My strategy state: ...`
}
```

**Full example — SMA crossover** (`algorithms/example.rhai`):
```rhai
let threshold = if "threshold" in options { options["threshold"].parse_float() } else { 0.01 };
let period    = if "period"    in options { options["period"].parse_int()   } else { 20 };
let qty       = if "qty"       in options { options["qty"].parse_float()    } else { 0.001 };
let prices    = [];

fn on_tick(price, bid, ask, volume) {
    prices.push(price);
    if prices.len() > period { prices.remove(0); }
    if prices.len() < period { return (); }

    let avg = prices.reduce(|a, b| a + b, 0.0) / prices.len();

    if price < avg * (1.0 - threshold) {
        return #{ kind: "buy",  price: price, qty: qty, reason: `below SMA` };
    }
    if price > avg * (1.0 + threshold) {
        return #{ kind: "sell", price: price, qty: qty, reason: `above SMA` };
    }
    ()
}

fn summary() {
    let avg = if prices.len() > 0 { prices.reduce(|a, b| a + b, 0.0) / prices.len() } else { 0.0 };
    `SMA(${period}): avg=${avg}`
}
```

### Hot-reload behaviour

| Event | Effect |
|-------|--------|
| New `.rhai` file added | Compiled and registered within 500ms; available for new runners immediately |
| Existing `.rhai` file modified | Recompiled and registry updated; running runners using the old version are unaffected |
| `.rhai` file with a syntax error | Compile error logged; existing registry entry (if any) is preserved |
| `.rhai` file deleted | Removed from registry; running runners continue with their in-memory AST |

---

## Terminal controls

| Key | Action |
|-----|--------|
| Type characters | Build command at prompt |
| `Backspace` | Delete character left of cursor |
| `Left` / `Right` | Move cursor within the input |
| `Home` / `End` | Jump to start / end of input |
| `Enter` | Submit command |
| `Ctrl+C` / `Ctrl+D` | Graceful shutdown |

Log lines scroll above the prompt; the input line is always preserved at the bottom.

---

## File layout

Data and logs are written to the platform data directory, not the working directory.

| Platform | Path |
|----------|------|
| Windows | `%LOCALAPPDATA%\ted\` |
| macOS | `~/Library/Application Support/ted/` |
| Linux | `~/.local/share/ted/` |

```
<data dir>/ted/
├── ted.db               # SQLite database (runners, ticks, orders, fills)
├── logs/
│   ├── ted.log          # current log file
│   └── ted_2026-03-22.log  # rotated logs (kept for log_retention days)
└── trades/
    └── trades_BTCUSD.jsonl  # per-symbol trade log (legacy, kept for compatibility)

<working dir>/
├── config.json          # API credentials and endpoints
├── config.template.json # template — copy and fill in
├── algorithms/          # drop .rhai strategy scripts here
│   └── example.rhai     # reference SMA implementation
└── overview_*.md        # generated reports
```
