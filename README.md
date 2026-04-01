# T.E.D — Trading Exchange Driver
---

DISCLAIMER: AI has been used as a tool to fix some bugs, and streamline some of the data-flow.
As well as writing parts of this README.md

---
![Static Badge](https://img.shields.io/badge/T.E.D_--_Trading_Exchange_Driver-1.0.1-green)

A Rust-based algorithmic trading daemon for the Bitfinex exchange. Connects to the Bitfinex WebSocket v2 API, streams live market data, and executes configurable trading strategies.

---

## Prerequisites

- Rust toolchain (stable, 2024 edition)
- A crypto-platform account with API credentials
- `config.json` in the working directory (see below)

If you have a cleaner, or maybe more dynamic means of adjusting the config.json, please make a Pull Request or fork

---

## Configuration

Create `config.json` next to the executable:

```json
{
  "auth_endpoint": "https://api.example.com/",
  "pub_endpoint":  "https://api-pub.example.com/v2/",
  "ws_endpoint":   "wss://api-pub.example.com/ws/2",
  "key":    "YOUR_API_KEY",
  "secret": "YOUR_API_SECRET"
}
```

The key and secret are only used when live trading is enabled. Dry-run mode (the default) never touches the exchange REST API.

---

## Building and running

```
cargo build --release
./target/release/ted
```

The binary enters an interactive terminal session. Log output scrolls above the input line; the `> ` prompt stays pinned at the bottom. Use `Ctrl+C` or `Ctrl+D` or type `exit` to quit gracefully.

A log file `ted.log` is written in the working directory alongside all terminal output.

---

## Commands

All commands are typed at the `> ` prompt and dispatched on Enter. Commands follow standard CLI syntax — flags can be combined freely.

### `runner` — manage trading runners

Each runner is an independent async task that streams market data for one symbol and runs a single algorithm on each tick.

**Spawn a new runner:**
```
runner --symbol BTCUSD
runner --symbol BTCUSD --algorithm grid --option spacing=500 qty=0.001
runner --symbol ETHUSD --algorithm example --option period=30 qty=0.01
```

| Flag | Short | Description |
|------|-------|-------------|
| `--symbol <SYM>` | `-s` | Trading pair symbol (required) |
| `--algorithm <NAME>` | `-a` | Algorithm name: `passive`, `grid`, or any script name (default: `passive`) |
| `--option <KEY=VALUE>` | | Algorithm options, repeat for multiple |

**Lifecycle:**
```
runner -s BTCUSD --pause          # pause tick processing (WS stays connected)
runner -s BTCUSD --resume         # resume
runner -s BTCUSD --kill           # stop runner, cancel any open live orders
```

**Switch algorithm on a running runner:**
```
runner -s BTCUSD --configure grid --option spacing=250 qty=0.002
```

**Enable/disable live trading:**
```
runner -s BTCUSD --enable-live    # real orders will be placed
runner -s BTCUSD --disable-live   # back to dry-run
```

Live trading is **disabled by default**. In dry-run mode all signals are logged as `[DRY RUN]` and no orders are placed.

---

### `generate` — produce an overview report

Writes a Markdown report of a runner's activity to disk.

```
generate --runner BTCUSD           # single runner
generate --runner BTCUSD --verbose # include full signal table
generate --all                     # all running runners, combined file
generate --all --verbose
```

Output files are written to the current directory with timestamped names (`overview_BTCUSD_2026-03-22.md`, `overview_all_2026-03-22.md`).

---

### `exit`

```
exit
```

Gracefully stops all runners (cancels open live orders), closes WebSocket connections, and exits.

---

## Algorithms

### Built-in: `passive`

Observes and logs every tick. Places no orders. Useful for monitoring without any strategy active.

```
runner -s BTCUSD --algorithm passive
```

### Built-in: `grid`

Places limit buy and sell orders at equidistant price levels. The grid is centred on the live price at the moment the first tick arrives, so no manual range configuration is needed.

| Option | Description | Required |
|--------|-------------|----------|
| `spacing=<float>` | Distance between grid levels in quote currency | Yes |
| `qty=<float>` | Order quantity per level in base currency | Yes |
| `levels=<int>` | Total number of grid intervals (minimum 2) | Yes |
| `initial_base=<float>` | Existing base asset to seed initial sell orders | No |

```
runner -s BTCUSD --algorithm grid --option spacing=500 qty=0.001 levels=10
runner -s BTCUSD --algorithm grid --option spacing=200 qty=0.005 levels=6 initial_base=0.1
```

With `levels=10` and `spacing=500` the grid spans 5000 units of quote currency, centred on the first price seen. Without `initial_base` the bot buys first; each filled buy places a sell one level above, and vice versa.

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

```
ted/
├── config.json          # API credentials and endpoints
├── ted.log              # append-only log file
├── algorithms/          # drop .rhai strategy scripts here
│   └── example.rhai     # reference SMA implementation
└── overview_*.md        # generated reports
```
