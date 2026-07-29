use crate::algorithm::Algorithm;
use crate::algorithm::position::{AvgCostBook, Lot, LotBook};
use crate::api::{MarketData, TradeSignal};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

const REBUILD_COOLDOWN_SECS: i64 = 300;
const GRID_STATE_VERSION: u32 = 2;

#[derive(Clone, Copy, PartialEq)]
enum TrendFilter {
    Off,
    Ema,
}

/// Dynamic grid state persisted for resume. Most configuration-derived fields
/// (fees, trend/risk settings, the plan/09 downtrend/cap knobs) are rebuilt from
/// options on spawn; but the capital-derived `qty` and the possibly-reduced
/// `levels_per_side` are computed once at runtime from a live midpoint, so they
/// (and the sizing/seeding latches) are serialized to survive a restart.
/// Order maps carry `(price, qty)` per level since plan/09 (buy levels can be
/// downtrend-scaled; sell levels aggregate lot exits).
#[derive(Serialize, Deserialize)]
struct GridState {
    version: u32,
    buy_orders: HashMap<u64, (f64, f64)>,
    sell_orders: HashMap<u64, (f64, f64)>,
    grid_lower_bound: f64,
    grid_upper_bound: f64,
    book: LotBook,
    total_buys: u32,
    total_sells: u32,
    spacing: f64,
    price_decimals: u32,
    trend_ema: Option<f64>,
    last_price: Option<f64>,
    emitted_buy_prices: HashSet<u64>,
    emitted_sell_prices: HashSet<u64>,
    qty: f64,
    levels_per_side: u32,
    sized: bool,
    base_seeded: bool,
    unfundable: bool,
}

/// Pre-plan/09 state shape (avg-cost book, price-only order maps), accepted on
/// restore and converted: the open position becomes a single seed lot at its
/// average cost with a breakeven-floored exit.
#[derive(Deserialize)]
struct GridStateLegacy {
    buy_orders: HashMap<u64, f64>,
    #[allow(dead_code)]
    sell_orders: HashMap<u64, f64>,
    grid_lower_bound: f64,
    grid_upper_bound: f64,
    book: AvgCostBook,
    total_buys: u32,
    total_sells: u32,
    spacing: f64,
    price_decimals: u32,
    #[serde(default, alias = "ema")]
    trend_ema: Option<f64>,
    last_price: Option<f64>,
    emitted_buy_prices: HashSet<u64>,
    qty: f64,
    levels_per_side: u32,
    sized: bool,
    base_seeded: bool,
    unfundable: bool,
}

pub struct GridBot {
    levels_per_side: u32,
    // The level count the user asked for. `levels_per_side` is the effective
    // count derived each sizing pass, so a budget-capped reduction is
    // non-destructive and a grown budget can restore levels later.
    levels_requested: u32,
    qty: f64,
    spacing: f64,
    price_decimals: u32,

    // Balance-aware sizing (plan/06 step 1). `qty` above is derived from these at
    // the first `build_grid` when a midpoint is known, unless an explicit `qty`
    // option was supplied (back-compat), in which case `sized` is true from `new`.
    capital: Option<f64>,
    buy_reserve_frac: f64,
    min_notional: f64,
    initial_base_balance: f64,
    sized: bool,
    base_seeded: bool,
    unfundable: bool,

    grid_lower_bound: f64,
    grid_upper_bound: f64,

    buy_orders: HashMap<u64, (f64, f64)>,
    sell_orders: HashMap<u64, (f64, f64)>,

    base_balance: f64,
    quote_balance: f64,

    last_price: Option<f64>,
    book: LotBook,
    total_buys: u32,
    total_sells: u32,

    maker_fee: f64,
    taker_fee: f64,
    allow_unprofitable: bool,

    trend_filter: TrendFilter,
    trend_ema_period: f64,
    trend_threshold: f64,
    // Candle-close EMA pushed in by the runner/backtester via `on_trend_update`
    // (plan/08 step 2) — not derived from ticks.
    trend_ema: Option<f64>,

    max_position: f64,
    stop_loss_pct: Option<f64>,

    // Plan/09: per-round-trip profit floor and downtrend scalping caps.
    min_profit_frac: f64,
    downtrend_levels: u32,
    downtrend_qty_frac: f64,
    max_inventory_frac: f64,
    max_inventory_frac_down: f64,
    // Latch so a blocked buy logs once per rebuild, not per tick.
    cap_logged: bool,

    recenter_band: f64,
    last_rebuild_at: Option<DateTime<Utc>>,

    emitted_buy_prices: HashSet<u64>,
    emitted_sell_prices: HashSet<u64>,
}

impl GridBot {
    pub fn new(options: &HashMap<String, String>) -> Result<Self, String> {
        let levels_per_side = options
            .get("levels")
            .ok_or("Missing required option: levels")?
            .parse::<u32>()
            .map_err(|_| "Option 'levels' must be a positive integer".to_string())?;

        if levels_per_side < 1 {
            return Err("Option 'levels' must be at least 1".to_string());
        }

        let qty_override = match options.get("qty") {
            Some(v) => {
                let q = v
                    .parse::<f64>()
                    .map_err(|_| "Option 'qty' must be a valid number".to_string())?;
                if q <= 0.0 {
                    return Err("Option 'qty' must be positive".to_string());
                }
                Some(q)
            }
            None => None,
        };

        let capital = match options.get("capital") {
            Some(v) => {
                let c = v
                    .parse::<f64>()
                    .map_err(|_| "Option 'capital' must be a valid number".to_string())?;
                if c <= 0.0 {
                    return Err("Option 'capital' must be positive".to_string());
                }
                Some(c)
            }
            None => None,
        };

        // `qty` (explicit, fixed) wins for back-compat; otherwise `capital` drives
        // a midpoint-derived `qty` at the first build. One of the two is required.
        let (qty, sized, capital) = match (qty_override, capital) {
            (Some(q), _) => (q, true, None),
            (None, Some(c)) => (0.0, false, Some(c)),
            (None, None) => {
                return Err(
                    "Missing required option: provide either 'qty' or 'capital'".to_string()
                );
            }
        };

        let buy_reserve_frac = options
            .get("buy_reserve_frac")
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| *v > 0.0 && *v <= 1.0)
            .unwrap_or(0.5);

        let min_notional = options
            .get("min_notional")
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| *v > 0.0)
            .unwrap_or(25.0);

        let spacing_str = options
            .get("spacing")
            .ok_or("Missing option: spacing (must be provided directly or computed from ATR by the runner)")?;

        let price_decimals: u32 = match spacing_str.split_once('.') {
            Some((_, frac)) => frac.len() as u32,
            None => 0,
        };

        let spacing = spacing_str
            .parse::<f64>()
            .map_err(|_| "Option 'spacing' must be a valid number".to_string())?;

        if spacing <= 0.0 {
            return Err("Option 'spacing' must be positive".to_string());
        }

        let base_balance = options
            .get("initial_base_balance")
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0);

        let quote_balance = options
            .get("initial_quote_balance")
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0);

        let maker_fee = options
            .get("maker_fee")
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0);

        let taker_fee = options
            .get("taker_fee")
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0);

        let allow_unprofitable = options
            .get("allow_unprofitable")
            .map(|v| v.eq_ignore_ascii_case("true") || v == "1")
            .unwrap_or(false);

        let trend_filter = match options.get("trend_filter").map(|s| s.to_lowercase()) {
            Some(ref s) if s == "off" => TrendFilter::Off,
            _ => TrendFilter::Ema,
        };

        let trend_ema_period = options
            .get("trend_ema_period")
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|p| *p > 0.0)
            .unwrap_or(50.0);

        let trend_threshold = options
            .get("trend_threshold")
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.005);

        let min_profit_frac = options
            .get("min_profit_frac")
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| *v >= 0.0)
            .unwrap_or(0.001);

        let downtrend_levels = options
            .get("downtrend_levels")
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|v| *v >= 1)
            .unwrap_or(1);

        let downtrend_qty_frac = options
            .get("downtrend_qty_frac")
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| *v > 0.0 && *v <= 1.0)
            .unwrap_or(0.5);

        let max_inventory_frac = options
            .get("max_inventory_frac")
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| *v > 0.0)
            .unwrap_or(0.75);

        let max_inventory_frac_down = options
            .get("max_inventory_frac_down")
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| *v > 0.0)
            .unwrap_or(0.5);

        let recenter_band = options
            .get("recenter_band")
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| *v > 0.0)
            .unwrap_or(2.5);

        let max_position = options
            .get("max_position")
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| *v > 0.0)
            .unwrap_or(f64::INFINITY);

        let stop_loss_pct = options
            .get("stop_loss_pct")
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| *v > 0.0);

        Ok(GridBot {
            levels_per_side,
            levels_requested: levels_per_side,
            qty,
            spacing,
            price_decimals,
            capital,
            buy_reserve_frac,
            min_notional,
            initial_base_balance: base_balance,
            sized,
            base_seeded: false,
            unfundable: false,
            grid_lower_bound: 0.0,
            grid_upper_bound: 0.0,
            buy_orders: HashMap::new(),
            sell_orders: HashMap::new(),
            base_balance,
            quote_balance,
            last_price: None,
            book: LotBook::new(maker_fee),
            total_buys: 0,
            total_sells: 0,
            maker_fee,
            taker_fee,
            allow_unprofitable,
            trend_filter,
            trend_ema_period,
            trend_threshold,
            trend_ema: None,
            max_position,
            stop_loss_pct,
            min_profit_frac,
            downtrend_levels,
            downtrend_qty_frac,
            max_inventory_frac,
            max_inventory_frac_down,
            cap_logged: false,
            recenter_band,
            last_rebuild_at: None,
            emitted_buy_prices: HashSet::new(),
            emitted_sell_prices: HashSet::new(),
        })
    }

    fn avg_cost(&self) -> f64 {
        self.book.avg_cost()
    }

    fn trend_label(&self) -> &'static str {
        match self.trend_filter {
            TrendFilter::Off => "off",
            TrendFilter::Ema => match (self.trend_ema, self.last_price) {
                (Some(ema), Some(price)) if ema > 0.0 => {
                    let dev = (price - ema) / ema;
                    if dev > self.trend_threshold {
                        "up"
                    } else if dev < -self.trend_threshold {
                        "down"
                    } else {
                        "flat"
                    }
                }
                _ => "warming up",
            },
        }
    }

    /// True when the trend EMA is known and `price` sits below it beyond the
    /// threshold — a confirmed downtrend. Drives extension-buy blocking and the
    /// plan/09 downtrend sizing/caps.
    fn trend_is_down(&self, price: f64) -> bool {
        if self.trend_filter != TrendFilter::Ema {
            return false;
        }
        match self.trend_ema {
            Some(ema) if ema > 0.0 => (price - ema) / ema < -self.trend_threshold,
            _ => false,
        }
    }

    /// True when the strategy may add buy exposure: inside inventory cap and not
    /// stopped out. Counter-buys and planned ladder levels are gated by this
    /// alone — never by the trend filter (plan/08 step 1).
    fn risk_allows_buy(&self, price: f64) -> bool {
        self.book.position() < self.max_position && !self.stopped_out(price)
    }

    /// True when the trend filter forbids *extending* the buy ladder. The only
    /// move this blocks is adding fresh exposure below the ladder into a falling
    /// market; exits are never trend-gated.
    fn trend_blocks_extension_buy(&self, price: f64) -> bool {
        self.trend_is_down(price)
    }

    /// True when the unrealized loss on the open long exceeds `stop_loss_pct`
    /// of the capital deployed into it. Suppresses new orders on both sides.
    fn stopped_out(&self, price: f64) -> bool {
        match self.stop_loss_pct {
            Some(pct) if self.book.position() > 0.0 && self.book.notional() > 0.0 && price > 0.0 => {
                let unrealized = (price - self.avg_cost()) * self.book.position();
                unrealized < 0.0 && (-unrealized) > pct * self.book.notional()
            }
            _ => false,
        }
    }

    fn price_key(&self, price: f64) -> u64 {
        let m = 10_f64.powi(self.price_decimals as i32);
        (price * m).round() as u64
    }

    fn decimals_from_price(price: f64) -> u32 {
        if price <= 0.0 {
            return 2;
        }
        let magnitude = price.log10().floor() as i32;
        match magnitude {
            m if m >= 2 => 2,
            m if m >= 0 => 4,
            m if m >= -2 => 6,
            _ => 8,
        }
    }

    fn round_qty(qty: f64) -> f64 {
        (qty * 1e8).floor() / 1e8
    }

    /// The per-level buy quantity for the current trend regime: full `qty`
    /// normally, `qty × downtrend_qty_frac` in a confirmed downtrend — unless
    /// the scaled notional would fall below `min_notional`, in which case the
    /// full `qty` is kept (an unplaceable order is worse than a full-size one).
    fn buy_qty(&self, price: f64) -> f64 {
        if !self.trend_is_down(price) {
            return self.qty;
        }
        let scaled = Self::round_qty(self.qty * self.downtrend_qty_frac);
        if scaled > 0.0 && scaled * price >= self.min_notional - 1e-9 {
            scaled
        } else {
            self.qty
        }
    }

    /// Quote committed to buy levels still resting — worst-case additional
    /// inventory if every tracked buy fills. Counted against the cap so stacked
    /// resting orders cannot jointly blow past it.
    fn pending_buy_notional(&self) -> f64 {
        self.buy_orders.values().map(|&(p, q)| p * q).sum()
    }

    /// Inventory cap (plan/09): open lot notional plus everything already
    /// resting on the buy side plus the prospective buy must stay within
    /// `capital × max_inventory_frac[_down]`. Grids sized by explicit `qty`
    /// (no capital budget) skip the cap; `max_position` still applies.
    /// Logs once per rebuild when the cap blocks a buy.
    fn cap_allows_buy(&mut self, trend_price: f64, add_notional: f64) -> bool {
        let Some(capital) = self.capital else {
            return true;
        };
        let frac = if self.trend_is_down(trend_price) {
            self.max_inventory_frac_down
        } else {
            self.max_inventory_frac
        };
        let cap = capital * frac;
        let committed = self.book.notional() + self.pending_buy_notional();
        let ok = committed + add_notional <= cap + 1e-9;
        if !ok && !self.cap_logged {
            self.cap_logged = true;
            crate::logger::log_info(
                "[GRID]",
                &format!(
                    "Inventory cap: committed {:.2} (lots {:.2} + resting buys {:.2}) + new buy {:.2} would exceed {:.2} (capital {:.2} × {:.2}{}) — holding off new buys.",
                    committed,
                    self.book.notional(),
                    self.pending_buy_notional(),
                    add_notional,
                    cap,
                    capital,
                    frac,
                    if self.trend_is_down(trend_price) { ", downtrend" } else { "" },
                ),
            );
        }
        ok
    }

    /// Spacing floor: a round trip must clear both maker legs plus the minimum
    /// profit. Unlike the pre-plan/09 code this never refuses to build — spacing
    /// is clamped up to the floor (a misconfigured bot that silently does
    /// nothing is worse than one trading wider than asked).
    fn spacing_floor(&self, midpoint: f64) -> f64 {
        midpoint * (2.0 * self.maker_fee + self.min_profit_frac)
    }

    fn clamp_spacing(&mut self, midpoint: f64) {
        if self.allow_unprofitable || midpoint <= 0.0 {
            return;
        }
        let floor = self.spacing_floor(midpoint);
        if self.spacing < floor {
            crate::logger::log_warn(
                "[GRID]",
                &format!(
                    "Spacing {:.8} is below the profit floor {:.8} (midpoint {:.2} × (2×maker_fee {:.6} + min_profit_frac {:.6})) — clamping up. Set allow_unprofitable=true to override.",
                    self.spacing, floor, midpoint, self.maker_fee, self.min_profit_frac
                ),
            );
            self.spacing = floor;
        }
    }

    /// The exit invariant (plan/09): every lot's exit is
    /// `max(fill + spacing, breakeven × (1 + min_profit_frac))`, rounded UP to
    /// `price_decimals`, where `unit_cost` is the fee-inclusive per-unit cost.
    /// A lot exit can therefore never rest below its own cost.
    fn exit_for(&self, fill_price: f64, unit_cost: f64) -> f64 {
        let breakeven = unit_cost * (1.0 + self.min_profit_frac) / (1.0 - self.maker_fee);
        let raw = (fill_price + self.spacing).max(breakeven);
        let m = 10_f64.powi(self.price_decimals as i32);
        (raw * m - 1e-9).ceil() / m
    }

    /// Track and emit a lot exit at `exit`. If another lot already rests at the
    /// same level, the resting order is cancelled and replaced with the combined
    /// quantity (one price level = one exchange order).
    fn emit_exit(&mut self, exit: f64, qty: f64, signals: &mut Vec<TradeSignal>) {
        let prec = self.price_decimals as usize;
        let key = self.price_key(exit);
        if let Some(&(price, resting)) = self.sell_orders.get(&key) {
            let total = resting + qty;
            self.sell_orders.insert(key, (price, total));
            self.emitted_sell_prices.remove(&key);
            signals.push(TradeSignal::Cancel {
                price,
                is_buy: false,
                reason: format!("Lot exit: combine into {:.8} @ {:.prec$}", total, price, prec = prec),
            });
            signals.push(TradeSignal::Sell {
                price,
                quantity: total,
                reason: format!("Lot exit at {:.prec$} (combined)", price, prec = prec),
                price_decimals: self.price_decimals,
            });
        } else {
            self.sell_orders.insert(key, (exit, qty));
            signals.push(TradeSignal::Sell {
                price: exit,
                quantity: qty,
                reason: format!("Lot exit at {:.prec$}", exit, prec = prec),
                price_decimals: self.price_decimals,
            });
        }
        self.check_exit_invariant();
    }

    /// Base earmarked by resting lot exits. By construction this equals the
    /// position (every lot has exactly one exit) — a divergence means the exit
    /// bookkeeping drifted from the lots, which is worth a warning, not a panic.
    fn exit_qty_resting(&self) -> f64 {
        self.sell_orders.values().map(|&(_, q)| q).sum()
    }

    fn check_exit_invariant(&self) {
        let resting = self.exit_qty_resting();
        let position = self.book.position();
        if resting > position + 1e-6 {
            crate::logger::log_warn(
                "[GRID]",
                &format!(
                    "Exit invariant: {:.8} base resting in lot exits exceeds held position {:.8} — a fill would go net short.",
                    resting, position
                ),
            );
        }
    }

    /// Derive a uniform per-level `qty` from the effective quote budget at the
    /// given midpoint (plan/06 step 1). The budget compounds net realized PnL
    /// (plan/08 step 4): profits grow it, losses shrink it, clamped at zero.
    /// Reserves `buy_reserve_frac` to fund the buy ladder; derives the effective
    /// `levels_per_side` from `levels_requested` so each level clears
    /// `min_notional` (non-destructive — a grown budget restores levels); caps
    /// `qty` by held base so the sell ladder is fundable too. Flags the runner
    /// `unfundable` (and warns) if the budget cannot fund even one level.
    fn size_from_capital(&mut self, midpoint: f64) {
        let Some(capital) = self.capital else {
            return;
        };
        if midpoint <= 0.0 {
            return;
        }

        let effective = (capital + self.book.realized_pnl).max(0.0);
        let buy_budget = effective * self.buy_reserve_frac;
        // Each level costs ~ buy_budget / levels in quote; cap levels so that
        // per-level notional stays >= min_notional.
        let max_fundable = (buy_budget / self.min_notional).floor() as u32;
        if max_fundable < 1 {
            self.unfundable = true;
            self.qty = 0.0;
            crate::logger::log_warn(
                "[GRID]",
                &format!(
                    "Budget {:.2} (capital {:.2} + realized {:+.2}; buy budget {:.2} = budget × {:.2}) cannot fund a single level at min_notional {:.2} — emitting no orders. Increase capital or lower min_notional.",
                    effective, capital, self.book.realized_pnl, buy_budget, self.buy_reserve_frac, self.min_notional
                ),
            );
            return;
        }

        let levels = self.levels_requested.min(max_fundable);
        if levels < self.levels_requested {
            crate::logger::log_warn(
                "[GRID]",
                &format!(
                    "Budget {:.2} (buy budget {:.2}) funds only {} of {} requested levels/side at min_notional {:.2} — reducing to {}.",
                    effective, buy_budget, levels, self.levels_requested, self.min_notional, levels
                ),
            );
        }
        self.levels_per_side = levels;

        let qty_from_quote = buy_budget / (levels as f64 * midpoint);
        let qty = if self.initial_base_balance > 0.0 {
            // Hold base already: size so both ladders are fundable from inventory.
            let qty_from_base = self.initial_base_balance / levels as f64;
            qty_from_quote.min(qty_from_base)
        } else {
            qty_from_quote
        };
        let qty = Self::round_qty(qty);

        if qty <= 0.0 || qty * midpoint < self.min_notional - 1e-9 {
            self.unfundable = true;
            self.qty = 0.0;
            crate::logger::log_warn(
                "[GRID]",
                &format!(
                    "Derived per-level notional {:.2} (qty {:.8} × midpoint {:.2}) is below min_notional {:.2} — emitting no orders.",
                    qty * midpoint, qty, midpoint, self.min_notional
                ),
            );
            return;
        }

        self.qty = qty;
        self.unfundable = false;
        crate::logger::log_info(
            "[GRID]",
            &format!(
                "Sized from capital {:.2} + realized {:+.2} → budget {:.2}: qty {:.8}/level × {} levels/side (buy budget {:.2}, reserve {:.0}% for buys, midpoint {:.2}).",
                capital,
                self.book.realized_pnl,
                effective,
                qty,
                self.levels_per_side,
                buy_budget,
                self.buy_reserve_frac * 100.0,
                midpoint,
            ),
        );
    }

    /// Build (only) the buy ladder around `midpoint`. Sell orders are lot exits
    /// created at fill time and are never rebuilt here — existing exits persist
    /// untouched (plan/09). Seeds opening base inventory once, as a lot whose
    /// exit is emitted alongside the ladder.
    fn build_grid(&mut self, midpoint: f64) -> Vec<TradeSignal> {
        // Derive sizing from the capital budget the first time we have a midpoint.
        if !self.sized {
            self.size_from_capital(midpoint);
            self.sized = true;
        }

        let price_min = Self::decimals_from_price(midpoint);
        if self.price_decimals < price_min {
            self.price_decimals = price_min;
        }

        self.clamp_spacing(midpoint);
        self.cap_logged = false;

        let prec = self.price_decimals as usize;
        let mut signals: Vec<TradeSignal> = Vec::new();

        // Seed any pre-existing base inventory as a lot with a breakeven-floored
        // exit, emitted exactly once (plan/09).
        if !self.base_seeded {
            self.base_seeded = true;
            if self.initial_base_balance > 0.0 && midpoint > 0.0 {
                let exit = self.exit_for(midpoint, midpoint);
                self.book.seed(self.initial_base_balance, midpoint, exit);
                crate::logger::log(
                    "[GRID]",
                    &format!(
                        "Seeded {:.8} base @ {:.prec$} as opening lot — exit at {:.prec$}.",
                        self.initial_base_balance,
                        midpoint,
                        exit,
                        prec = prec
                    ),
                );
                self.emit_exit(exit, self.initial_base_balance, &mut signals);
            }
        }

        if self.unfundable {
            return signals;
        }

        let m = 10_f64.powi(self.price_decimals as i32);

        self.buy_orders.clear();
        self.emitted_buy_prices.clear();

        let down = self.trend_is_down(midpoint);
        let depth = if down {
            self.downtrend_levels.min(self.levels_per_side)
        } else {
            self.levels_per_side
        };
        let ladder_qty = self.buy_qty(midpoint);

        for i in 0..depth {
            let price = ((midpoint - (i as f64 + 1.0) * self.spacing) * m).round() / m;
            if price <= 0.0 {
                continue;
            }
            // Levels already inserted count via pending_buy_notional.
            if !self.cap_allows_buy(midpoint, ladder_qty * price) {
                break;
            }
            self.buy_orders.insert(self.price_key(price), (price, ladder_qty));
        }

        self.grid_lower_bound = self
            .buy_orders
            .values()
            .map(|&(p, _)| p)
            .reduce(f64::min)
            .unwrap_or(midpoint - self.spacing * depth.max(1) as f64);
        self.grid_upper_bound = midpoint + self.spacing;

        crate::logger::log(
            "[GRID]",
            &format!(
                "Grid built: midpoint {:.prec$}, spacing {:.prec$}, {} buys{}, {} lot exit(s) resting",
                midpoint,
                self.spacing,
                self.buy_orders.len(),
                if down { " (downtrend depth/size)" } else { "" },
                self.sell_orders.len(),
                prec = prec,
            ),
        );

        let mut buy_levels: Vec<(f64, f64)> = self.buy_orders.values().copied().collect();
        buy_levels.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
        for (price, qty) in buy_levels {
            signals.push(TradeSignal::Buy {
                price,
                quantity: qty,
                reason: format!("Grid initial buy at {:.prec$}", price, prec = prec),
                price_decimals: self.price_decimals,
            });
        }

        signals
    }

    fn grid_lower(&self) -> Option<f64> {
        self.buy_orders.values().map(|&(p, _)| p).reduce(f64::min)
    }

    fn grid_upper(&self) -> Option<f64> {
        self.sell_orders.values().map(|&(p, _)| p).reduce(f64::max)
    }

    fn format_range(lower: Option<f64>, upper: Option<f64>) -> String {
        let side = |v: Option<f64>, name: &str| {
            v.map(|p| format!("{:.2}", p))
                .unwrap_or_else(|| format!("no {}", name))
        };
        format!("[{}–{}]", side(lower, "buys"), side(upper, "sells"))
    }

    /// Drain the buy ladder into `Cancel` signals so a rebuild does not orphan
    /// resting exchange orders (plan/08 step 3). Sell orders are lot exits and
    /// are never drained by maintenance — they persist until they fill
    /// (plan/09). Callers prepend these to `build_grid`'s output.
    fn drain_buys_to_cancels(&mut self) -> Vec<TradeSignal> {
        let prec = self.price_decimals as usize;
        let mut signals: Vec<TradeSignal> = Vec::with_capacity(self.buy_orders.len());
        for &(price, _) in self.buy_orders.values() {
            signals.push(TradeSignal::Cancel {
                price,
                is_buy: true,
                reason: format!("Rebuild: cancel stale buy {:.prec$}", price, prec = prec),
            });
        }
        self.buy_orders.clear();
        self.emitted_buy_prices.clear();
        signals
    }

    /// True when no *buy* level rests within `recenter_band × spacing` of the
    /// mid — the fill-less deadlock plan/08 step 3 re-centers out of. Lot exits
    /// don't count: an exit far above mid is intentional patience, not
    /// staleness (plan/09).
    fn in_dead_zone(&self, mid: f64) -> bool {
        let band = self.recenter_band * self.spacing;
        !self
            .buy_orders
            .values()
            .any(|&(p, _)| (p - mid).abs() <= band)
    }

    fn rebuild_cooldown_active(&self, now: DateTime<Utc>) -> bool {
        self.last_rebuild_at
            .map(|t| now - t < chrono::Duration::seconds(REBUILD_COOLDOWN_SECS))
            .unwrap_or(false)
    }

    fn restore_legacy(&mut self, legacy: GridStateLegacy) {
        self.grid_lower_bound = legacy.grid_lower_bound;
        self.grid_upper_bound = legacy.grid_upper_bound;
        self.total_buys = legacy.total_buys;
        self.total_sells = legacy.total_sells;
        self.spacing = legacy.spacing;
        self.price_decimals = legacy.price_decimals;
        self.trend_ema = legacy.trend_ema;
        self.last_price = legacy.last_price;
        self.emitted_buy_prices = legacy.emitted_buy_prices;
        self.emitted_sell_prices = HashSet::new();
        self.qty = legacy.qty;
        self.levels_per_side = legacy.levels_per_side;
        self.sized = legacy.sized;
        self.base_seeded = legacy.base_seeded;
        self.unfundable = legacy.unfundable;

        self.buy_orders = legacy
            .buy_orders
            .into_iter()
            .map(|(k, p)| (k, (p, legacy.qty)))
            .collect();
        // The legacy generic sell ladder is dropped: those levels carry no lot
        // identity. Any still resting on the exchange will fill through the
        // out-of-band FIFO fallback or be reconciled away.
        self.sell_orders = HashMap::new();

        let mut book = LotBook::new(self.maker_fee);
        book.realized_pnl = legacy.book.realized_pnl;
        book.fees_paid = legacy.book.fees_paid;
        if legacy.book.position > 0.0 && legacy.book.position_cost > 0.0 {
            let avg = legacy.book.position_cost / legacy.book.position;
            let exit = self.exit_for(avg, avg);
            book.lots.push(Lot {
                qty: legacy.book.position,
                entry_price: avg,
                entry_cost: legacy.book.position_cost,
                exit_price: exit,
            });
            self.sell_orders
                .insert(self.price_key(exit), (exit, legacy.book.position));
        }
        self.book = book;

        crate::logger::log_info(
            "[GRID]",
            &format!(
                "Legacy grid state converted to per-lot: position {:.8} became one seed lot (exit {:.4}), realized {:+.4} carried over.",
                self.book.position(),
                self.grid_upper().unwrap_or(0.0),
                self.book.realized_pnl,
            ),
        );
    }
}

impl Algorithm for GridBot {
    fn name(&self) -> &str {
        "grid"
    }

    fn on_tick(&mut self, tick: &MarketData) -> Vec<TradeSignal> {
        let price = (tick.bid + tick.ask) / 2.0;

        if self.last_price.is_none() {
            if !self.buy_orders.is_empty() || !self.sell_orders.is_empty() {
                let lower = self.grid_lower();
                let upper = self.grid_upper();
                let range = Self::format_range(lower, upper);
                if price >= lower.unwrap_or(f64::MIN) && price <= upper.unwrap_or(f64::MAX) {
                    crate::logger::log(
                        "[GRID]",
                        &format!("Soft resume at {:.2} — grid {} intact.", price, range),
                    );
                    self.last_price = Some(price);
                    return vec![];
                }
                crate::logger::log(
                    "[GRID]",
                    &format!(
                        "Price {:.2} outside preserved grid {} — rebuilding buy ladder.",
                        price, range
                    ),
                );
                let mut signals = self.drain_buys_to_cancels();
                signals.extend(self.build_grid(price));
                self.last_rebuild_at = Some(tick.timestamp);
                self.last_price = Some(price);
                return signals;
            }

            let signals = self.build_grid(price);
            if signals.is_empty() {
                return vec![];
            }
            self.last_rebuild_at = Some(tick.timestamp);
            self.last_price = Some(price);
            return signals;
        }

        if self.buy_orders.is_empty() && self.sell_orders.is_empty() {
            let signals = self.build_grid(price);
            if !signals.is_empty() {
                self.last_rebuild_at = Some(tick.timestamp);
            }
            self.last_price = Some(price);
            return signals;
        }

        if self.grid_lower_bound > 0.0
            && (price < self.grid_lower_bound * 0.95 || price > self.grid_upper_bound * 1.05)
            && !self.rebuild_cooldown_active(tick.timestamp)
        {
            crate::logger::log(
                "[GRID]",
                &format!(
                    "Price {:.2} is far outside the buy ladder [{:.2}–{:.2}] — rebuilding.",
                    price, self.grid_lower_bound, self.grid_upper_bound
                ),
            );
            let mut signals = self.drain_buys_to_cancels();
            self.sized = false;
            signals.extend(self.build_grid(price));
            self.last_rebuild_at = Some(tick.timestamp);
            self.last_price = Some(price);
            return signals;
        }

        // Fill-less re-centering (plan/08 step 3): a stalled buy ladder that
        // drifted outside the band can never slide on fills again — cancel the
        // stale buys and rebuild around the mid. Lot exits are untouched. The
        // cooldown stops a fast wick from thrashing cancel/replace cycles.
        if self.in_dead_zone(price) && !self.rebuild_cooldown_active(tick.timestamp) {
            crate::logger::log(
                "[GRID]",
                &format!(
                    "No buy level within {:.1} × spacing of mid {:.2} ({}) — re-centering buy ladder.",
                    self.recenter_band,
                    price,
                    Self::format_range(self.grid_lower(), self.grid_upper()),
                ),
            );
            let mut signals = self.drain_buys_to_cancels();
            self.sized = false;
            signals.extend(self.build_grid(price));
            self.last_rebuild_at = Some(tick.timestamp);
            self.last_price = Some(price);
            return signals;
        }

        let prev_price = self.last_price.unwrap();
        let mut signals: Vec<TradeSignal> = Vec::new();
        let prec = self.price_decimals as usize;

        if price > prev_price {
            let triggered: Vec<(u64, f64, f64)> = self
                .sell_orders
                .iter()
                .filter(|&(k, &(p, _))| p <= price && !self.emitted_sell_prices.contains(k))
                .map(|(&k, &(p, q))| (k, p, q))
                .collect();
            for (key, sell_price, qty) in triggered {
                self.emitted_sell_prices.insert(key);
                signals.push(TradeSignal::Sell {
                    price: sell_price,
                    quantity: qty,
                    reason: format!("Grid sell at {:.prec$}", sell_price, prec = prec),
                    price_decimals: self.price_decimals,
                });
            }
        } else if price < prev_price && self.risk_allows_buy(price) {
            let triggered: Vec<(u64, f64, f64)> = self
                .buy_orders
                .iter()
                .filter(|&(k, &(p, _))| p >= price && !self.emitted_buy_prices.contains(k))
                .map(|(&k, &(p, q))| (k, p, q))
                .collect();
            for (key, buy_price, qty) in triggered {
                self.emitted_buy_prices.insert(key);
                signals.push(TradeSignal::Buy {
                    price: buy_price,
                    quantity: qty,
                    reason: format!("Grid buy at {:.prec$}", buy_price, prec = prec),
                    price_decimals: self.price_decimals,
                });
            }
        }

        self.last_price = Some(price);
        signals
    }

    fn on_fill(&mut self, fill_price: f64, is_buy: bool, current_price: f64) -> Vec<TradeSignal> {
        let current_price = if current_price <= 0.0 {
            fill_price
        } else {
            current_price
        };
        let m = 10_f64.powi(self.price_decimals as i32);
        let prec = self.price_decimals as usize;
        let mut signals: Vec<TradeSignal> = Vec::new();

        if is_buy {
            let key = self.price_key(fill_price);
            let qty = self
                .buy_orders
                .remove(&key)
                .map(|(_, q)| q)
                .unwrap_or(self.qty);
            self.emitted_buy_prices.remove(&key);

            // The lot's exit is floored at breakeven and anchored to the *fill*
            // price, never the current price — in a fast drop the old code
            // rested the exit below the entry (plan/09).
            let unit_cost = fill_price * (1.0 + self.maker_fee);
            let exit = self.exit_for(fill_price, unit_cost);
            self.book.record_buy(fill_price, qty, exit);
            self.total_buys += 1;
            crate::logger::log(
                "[GRID]",
                &format!(
                    "Buy filled @ {:.prec$} — lot exit at {:.prec$}",
                    fill_price,
                    exit,
                    prec = prec
                ),
            );
            self.emit_exit(exit, qty, &mut signals);

            // Extension buy: fresh exposure below the ladder — the one move the
            // trend filter gates (only against a real downtrend), also subject
            // to the inventory cap.
            if self.risk_allows_buy(current_price)
                && !self.trend_blocks_extension_buy(current_price)
            {
                let lowest_buy = self
                    .buy_orders
                    .values()
                    .map(|&(p, _)| p)
                    .reduce(f64::min)
                    .unwrap_or(fill_price);
                let extension = ((lowest_buy - self.spacing) * m).round() / m;
                let ext_qty = self.buy_qty(current_price);
                if extension > 0.0 && self.cap_allows_buy(current_price, ext_qty * extension) {
                    self.buy_orders
                        .insert(self.price_key(extension), (extension, ext_qty));
                    self.grid_lower_bound = extension;
                    signals.push(TradeSignal::Buy {
                        price: extension,
                        quantity: ext_qty,
                        reason: format!("Grid extension buy at {:.prec$}", extension, prec = prec),
                        price_decimals: self.price_decimals,
                    });
                }
            }
        } else {
            let key = self.price_key(fill_price);
            let qty = self
                .sell_orders
                .remove(&key)
                .map(|(_, q)| q)
                .unwrap_or(self.qty);
            self.emitted_sell_prices.remove(&key);
            self.book.record_sell(fill_price, qty);
            self.total_sells += 1;

            if self.buy_orders.len() >= self.levels_per_side as usize
                && let Some(outer_buy) = self
                    .buy_orders
                    .values()
                    .map(|&(p, _)| p)
                    .reduce(f64::min)
            {
                self.buy_orders.remove(&self.price_key(outer_buy));
                signals.push(TradeSignal::Cancel {
                    price: outer_buy,
                    is_buy: true,
                    reason: format!(
                        "Sliding grid: replace outermost buy {:.prec$}",
                        outer_buy,
                        prec = prec
                    ),
                });
            }

            // Counter-buy: re-arms the ladder for the next round trip. Gated by
            // the risk checks and inventory cap only — never by the trend
            // filter (plan/08 step 1).
            if self.risk_allows_buy(current_price) {
                let counter = ((current_price - self.spacing) * m).round() / m;
                let counter_qty = self.buy_qty(current_price);
                if counter > 0.0 && self.cap_allows_buy(current_price, counter_qty * counter) {
                    self.buy_orders
                        .insert(self.price_key(counter), (counter, counter_qty));
                    crate::logger::log(
                        "[GRID]",
                        &format!(
                            "Sell filled @ {:.prec$} — counter buy at {:.prec$}",
                            fill_price,
                            counter,
                            prec = prec
                        ),
                    );
                    signals.push(TradeSignal::Buy {
                        price: counter,
                        quantity: counter_qty,
                        reason: format!("Grid counter buy at {:.prec$}", counter, prec = prec),
                        price_decimals: self.price_decimals,
                    });
                } else if counter <= 0.0 {
                    crate::logger::log(
                        "[GRID]",
                        &format!(
                            "Sell filled @ {:.prec$} — counter buy skipped (price would be <= 0)",
                            fill_price,
                            prec = prec
                        ),
                    );
                }
            }
        }

        signals
    }

    fn on_order_failed(&mut self, price: f64, is_buy: bool) {
        if is_buy {
            self.emitted_buy_prices.remove(&self.price_key(price));
        } else {
            self.emitted_sell_prices.remove(&self.price_key(price));
        }
    }

    fn on_balance_update(&mut self, base: f64, quote: f64) {
        self.base_balance = base;
        self.quote_balance = quote;
    }

    fn on_spacing_update(&mut self, new_spacing: f64) {
        self.spacing = new_spacing;
        if let Some(mid) = self.last_price {
            self.clamp_spacing(mid);
        }
        crate::logger::log("[GRID]", &format!("Spacing updated: {:.8}", self.spacing));
    }

    fn on_trend_update(&mut self, ema: f64) {
        if ema > 0.0 {
            self.trend_ema = Some(ema);
        }
    }

    fn on_reconnect(&mut self) {
        self.last_price = None;
        self.emitted_buy_prices.clear();
        self.emitted_sell_prices.clear();
        if self.buy_orders.is_empty() && self.sell_orders.is_empty() {
            crate::logger::log(
                "[GRID]",
                "Reconnected — no grid built yet, will initialise on next tick.",
            );
        } else {
            crate::logger::log(
                "[GRID]",
                &format!(
                    "Reconnected — grid preserved ({} buys, {} lot exits, range ~{}), resuming on next tick.",
                    self.buy_orders.len(),
                    self.sell_orders.len(),
                    Self::format_range(self.grid_lower(), self.grid_upper()),
                ),
            );
        }
    }

    fn position(&self) -> f64 {
        self.book.position()
    }

    fn unrealized_pnl(&self, mid: f64) -> f64 {
        if self.book.position() > 0.0 && mid > 0.0 {
            (mid - self.avg_cost()) * self.book.position()
        } else {
            0.0
        }
    }

    fn realized_pnl(&self) -> f64 {
        self.book.realized_pnl
    }

    fn fees_paid(&self) -> f64 {
        self.book.fees_paid
    }

    fn trade_count(&self) -> u64 {
        self.total_buys as u64 + self.total_sells as u64
    }

    fn open_lots(&self) -> usize {
        self.book.lots.len()
    }

    fn trend_state(&self) -> Option<&'static str> {
        Some(self.trend_label())
    }

    fn expected_exits(&self) -> Vec<TradeSignal> {
        let prec = self.price_decimals as usize;
        let mut exits: Vec<(f64, f64)> = self.sell_orders.values().copied().collect();
        exits.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        exits
            .into_iter()
            .map(|(price, qty)| TradeSignal::Sell {
                price,
                quantity: qty,
                reason: format!("Reconcile: re-place lot exit at {:.prec$}", price, prec = prec),
                price_decimals: self.price_decimals,
            })
            .collect()
    }

    fn serialize_state(&self) -> Option<String> {
        let state = GridState {
            version: GRID_STATE_VERSION,
            buy_orders: self.buy_orders.clone(),
            sell_orders: self.sell_orders.clone(),
            grid_lower_bound: self.grid_lower_bound,
            grid_upper_bound: self.grid_upper_bound,
            book: self.book.clone(),
            total_buys: self.total_buys,
            total_sells: self.total_sells,
            spacing: self.spacing,
            price_decimals: self.price_decimals,
            trend_ema: self.trend_ema,
            last_price: self.last_price,
            emitted_buy_prices: self.emitted_buy_prices.clone(),
            emitted_sell_prices: self.emitted_sell_prices.clone(),
            qty: self.qty,
            levels_per_side: self.levels_per_side,
            sized: self.sized,
            base_seeded: self.base_seeded,
            unfundable: self.unfundable,
        };
        serde_json::to_string(&state).ok()
    }

    fn restore_state(&mut self, json: &str) {
        let state: GridState = match serde_json::from_str(json) {
            Ok(s) => s,
            Err(_) => match serde_json::from_str::<GridStateLegacy>(json) {
                Ok(legacy) => {
                    self.restore_legacy(legacy);
                    return;
                }
                Err(e) => {
                    crate::logger::log_warn(
                        "[GRID]",
                        &format!("Could not restore grid state ({}) — starting fresh.", e),
                    );
                    return;
                }
            },
        };
        self.buy_orders = state.buy_orders;
        self.sell_orders = state.sell_orders;
        self.grid_lower_bound = state.grid_lower_bound;
        self.grid_upper_bound = state.grid_upper_bound;
        self.book = state.book;
        self.total_buys = state.total_buys;
        self.total_sells = state.total_sells;
        self.spacing = state.spacing;
        self.price_decimals = state.price_decimals;
        self.trend_ema = state.trend_ema;
        self.last_price = state.last_price;
        self.emitted_buy_prices = state.emitted_buy_prices;
        self.emitted_sell_prices = state.emitted_sell_prices;
        self.qty = state.qty;
        self.levels_per_side = state.levels_per_side;
        self.sized = state.sized;
        self.base_seeded = state.base_seeded;
        self.unfundable = state.unfundable;
        crate::logger::log_info(
            "[GRID]",
            &format!(
                "Grid state restored: {} buys, {} lot exits, {} open lot(s), qty {:.8}, position {:.8}, realized {:.8}.",
                self.buy_orders.len(),
                self.sell_orders.len(),
                self.book.lots.len(),
                self.qty,
                self.book.position(),
                self.book.realized_pnl,
            ),
        );
    }

    fn summary(&self) -> Option<String> {
        let lower = self.grid_lower().unwrap_or(0.0);
        let upper = self.grid_upper().unwrap_or(0.0);
        let prec = self.price_decimals as usize;
        let unrealized = self
            .last_price
            .map(|p| self.unrealized_pnl(p))
            .unwrap_or(0.0);
        let max_pos = if self.max_position.is_finite() {
            format!("{:.8}", self.max_position)
        } else {
            "unbounded".to_string()
        };
        let sizing = match self.capital {
            Some(c) => format!(
                "capital {:.2} (reserve {:.0}% for buys, min_notional {:.2})",
                c,
                self.buy_reserve_frac * 100.0,
                self.min_notional
            ),
            None => "fixed qty".to_string(),
        };
        let cap_state = match self.capital {
            Some(c) => {
                let down = self
                    .last_price
                    .map(|p| self.trend_is_down(p))
                    .unwrap_or(false);
                let frac = if down {
                    self.max_inventory_frac_down
                } else {
                    self.max_inventory_frac
                };
                format!(
                    "{:.2} of {:.2} (frac {:.2}{}; normal {:.2} / down {:.2})",
                    self.book.notional(),
                    c * frac,
                    frac,
                    if down { ", downtrend" } else { "" },
                    self.max_inventory_frac,
                    self.max_inventory_frac_down,
                )
            }
            None => "off (fixed qty sizing)".to_string(),
        };
        let status = if self.unfundable {
            "NO — capital cannot fund a level at min_notional, grid not building"
        } else {
            "yes"
        };
        Some(format!(
            "GridBot\n  Levels/side:  {}\n  Range:        {:.prec$} – {:.prec$}\n  Spacing:      {:.prec$} (floor: 2×maker_fee + min_profit_frac {:.4})\n  Sizing:       {}\n  Qty/level:    {:.8}\n  Fees:         maker {:.6}, taker {:.6}\n  Min profit:   {:.4}/round trip\n  Trades:       {} buys, {} sells\n  Orders:       {} buy open, {} lot exit(s)\n  Lots:         {} open, notional {:.2}\n  Inventory:    {}\n  Position:     {:.8} (net qty), max {}\n  Avg cost:     {:.prec$}\n  Trend:        {} (EMA period {:.0}, threshold {:.4}; downtrend levels {}, qty ×{:.2})\n  Realized PnL: {:.8}\n  Fees paid:    {:.8}\n  Unrealized:   {:.8} (at last mid)\n  Profitable:   {}",
            self.levels_per_side,
            lower, upper,
            self.spacing,
            self.min_profit_frac,
            sizing,
            self.qty,
            self.maker_fee, self.taker_fee,
            self.min_profit_frac,
            self.total_buys, self.total_sells,
            self.buy_orders.len(), self.sell_orders.len(),
            self.book.lots.len(), self.book.notional(),
            cap_state,
            self.book.position(), max_pos,
            self.avg_cost(),
            self.trend_label(), self.trend_ema_period, self.trend_threshold,
            self.downtrend_levels, self.downtrend_qty_frac,
            self.book.realized_pnl,
            self.book.fees_paid,
            unrealized,
            status,
            prec = prec,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn base_opts() -> HashMap<String, String> {
        opts(&[
            ("levels", "3"),
            ("qty", "1"),
            ("spacing", "10"),
            ("trend_filter", "off"),
        ])
    }

    fn has_buy(signals: &[TradeSignal]) -> bool {
        signals.iter().any(|s| matches!(s, TradeSignal::Buy { .. }))
    }

    fn count_sells(signals: &[TradeSignal]) -> usize {
        signals.iter().filter(|s| matches!(s, TradeSignal::Sell { .. })).count()
    }

    fn count_cancels(signals: &[TradeSignal]) -> usize {
        signals.iter().filter(|s| matches!(s, TradeSignal::Cancel { .. })).count()
    }

    fn sell_prices(signals: &[TradeSignal]) -> Vec<f64> {
        signals
            .iter()
            .filter_map(|s| match s {
                TradeSignal::Sell { price, .. } => Some(*price),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn round_trip_pnl_zero_fee_equals_spacing_times_qty() {
        let mut g = GridBot::new(&base_opts()).unwrap();
        g.on_fill(100.0, true, 100.0);
        g.on_fill(110.0, false, 110.0);
        // spacing (10) × qty (1), no fees deducted.
        assert!((g.book.realized_pnl - 10.0).abs() < 1e-9, "got {}", g.book.realized_pnl);
    }

    #[test]
    fn round_trip_pnl_with_fee_subtracts_both_legs() {
        let mut o = base_opts();
        o.insert("maker_fee".to_string(), "0.001".to_string());
        let mut g = GridBot::new(&o).unwrap();
        g.on_fill(100.0, true, 100.0);
        g.on_fill(110.0, false, 110.0);
        // spacing×qty minus the maker fee on each leg.
        let expected = 10.0 - 100.0 * 1.0 * 0.001 - 110.0 * 1.0 * 0.001;
        assert!(
            (g.book.realized_pnl - expected).abs() < 1e-9,
            "got {}, expected {}",
            g.book.realized_pnl,
            expected
        );
    }

    #[test]
    fn spacing_below_profit_floor_clamps_up_and_builds() {
        // floor = 100 × (2 × 0.01 + 0.001) = 2.1; spacing 0.5 is below it. The
        // pre-plan/09 grid refused to build; now it clamps and trades.
        let o = opts(&[
            ("levels", "3"),
            ("qty", "1"),
            ("spacing", "0.5"),
            ("maker_fee", "0.01"),
            ("trend_filter", "off"),
        ]);
        let mut g = GridBot::new(&o).unwrap();
        let signals = g.build_grid(100.0);
        assert!(!signals.is_empty(), "clamped grid must still build");
        let floor = 100.0 * (2.0 * 0.01 + 0.001);
        assert!((g.spacing - floor).abs() < 1e-9, "spacing clamped to {}, got {}", floor, g.spacing);
    }

    #[test]
    fn allow_unprofitable_skips_spacing_clamp() {
        let o = opts(&[
            ("levels", "3"),
            ("qty", "1"),
            ("spacing", "0.5"),
            ("maker_fee", "0.01"),
            ("allow_unprofitable", "true"),
            ("trend_filter", "off"),
        ]);
        let mut g = GridBot::new(&o).unwrap();
        let signals = g.build_grid(100.0);
        assert!(!signals.is_empty());
        assert!((g.spacing - 0.5).abs() < 1e-9, "spacing must stay 0.5, got {}", g.spacing);
    }

    #[test]
    fn spacing_refresh_clamps_to_floor_too() {
        let o = opts(&[
            ("levels", "3"),
            ("qty", "1"),
            ("spacing", "10"),
            ("maker_fee", "0.01"),
            ("trend_filter", "off"),
        ]);
        let mut g = GridBot::new(&o).unwrap();
        g.last_price = Some(100.0);
        g.on_spacing_update(0.5);
        let floor = 100.0 * (2.0 * 0.01 + 0.001);
        assert!((g.spacing - floor).abs() < 1e-9, "refresh must clamp, got {}", g.spacing);
    }

    #[test]
    fn downtrend_places_counter_sell_but_blocks_extension_buy() {
        // Regression for runner 40: 6 buy fills down a falling market produced
        // zero counter-sells because exits were trend-gated at fill time.
        let o = opts(&[("levels", "3"), ("qty", "1"), ("spacing", "10")]);
        let mut g = GridBot::new(&o).unwrap();
        assert!((g.trend_threshold - 0.005).abs() < 1e-12, "default threshold");
        g.on_trend_update(100.0);
        // Fill at 90 = 10% below the trend EMA — a real downtrend.
        let sigs = g.on_fill(90.0, true, 90.0);
        assert_eq!(count_sells(&sigs), 1, "lot exit must always follow a buy fill");
        assert!(!has_buy(&sigs), "extension buy blocked in a downtrend");
    }

    #[test]
    fn uptrend_places_counter_buy() {
        // Regression for runner 41: 4 sell fills up a rally produced zero
        // counter-buys, so the ladder never re-armed.
        let o = opts(&[("levels", "3"), ("qty", "1"), ("spacing", "10")]);
        let mut g = GridBot::new(&o).unwrap();
        g.on_fill(100.0, true, 100.0); // acquire inventory to sell
        g.on_trend_update(100.0);
        // Fill at 110 = 10% above the trend EMA — a real uptrend.
        let sigs = g.on_fill(110.0, false, 110.0);
        assert!(has_buy(&sigs), "counter buy must always follow a sell fill");
    }

    #[test]
    fn unknown_trend_allows_extension_buys() {
        let o = opts(&[("levels", "3"), ("qty", "1"), ("spacing", "10")]);
        let mut g = GridBot::new(&o).unwrap();
        // No trend EMA yet (warming up) → extensions are allowed.
        let sigs = g.on_fill(90.0, true, 90.0);
        assert!(has_buy(&sigs), "warming-up filter must not block extensions");
    }

    #[test]
    fn fast_drop_exit_anchors_to_fill_price_not_current() {
        // Fill at 80 while price already dropped to 79: the exit must rest at
        // fill + spacing (90), not current + spacing (89) — the plan/09 fix for
        // exits landing below the entry in a fast drop.
        let o = opts(&[("levels", "3"), ("qty", "1"), ("spacing", "10"), ("trend_filter", "off")]);
        let mut g = GridBot::new(&o).unwrap();
        let sigs = g.on_fill(80.0, true, 79.0);
        let exits = sell_prices(&sigs);
        assert_eq!(exits.len(), 1);
        assert!(exits[0] >= 90.0 - 1e-9, "exit {} must anchor to fill 80 + spacing", exits[0]);
    }

    #[test]
    fn exit_floors_at_breakeven_when_spacing_is_tight() {
        // spacing 0.1 with 1% maker fee: fill+spacing (100.1) is below the
        // breakeven floor — the exit must clear cost + min profit.
        let o = opts(&[
            ("levels", "3"),
            ("qty", "1"),
            ("spacing", "0.1"),
            ("maker_fee", "0.01"),
            ("allow_unprofitable", "true"),
            ("trend_filter", "off"),
        ]);
        let mut g = GridBot::new(&o).unwrap();
        let sigs = g.on_fill(100.0, true, 100.0);
        let exits = sell_prices(&sigs);
        assert_eq!(exits.len(), 1);
        let breakeven = 100.0 * 1.01 * 1.001 / 0.99;
        assert!(
            exits[0] >= breakeven - 1e-9,
            "exit {} must be >= breakeven {}",
            exits[0],
            breakeven
        );
        // And the round trip must realize at least the min profit.
        let realized = g.book.record_sell(exits[0], 1.0);
        assert!(realized > 0.0, "round trip must be profitable, got {}", realized);
    }

    #[test]
    fn rebuild_cancels_buys_only_and_preserves_exits() {
        let o = opts(&[("levels", "2"), ("qty", "1"), ("spacing", "1"), ("trend_filter", "off")]);
        let mut g = GridBot::new(&o).unwrap();
        g.build_grid(100.0); // buys at 99/98
        g.on_fill(99.0, true, 99.0); // lot exit at 100
        assert_eq!(g.sell_orders.len(), 1);

        // Price far below the ladder → rebuild path. Only buys get cancelled.
        let sigs = g.on_tick(&tick(90.0, 0));
        let buy_cancels = sigs
            .iter()
            .filter(|s| matches!(s, TradeSignal::Cancel { is_buy: true, .. }))
            .count();
        let sell_cancels = sigs
            .iter()
            .filter(|s| matches!(s, TradeSignal::Cancel { is_buy: false, .. }))
            .count();
        assert!(buy_cancels > 0, "stale buys must be cancelled");
        assert_eq!(sell_cancels, 0, "lot exits must never be cancelled by a rebuild");
        assert_eq!(g.sell_orders.len(), 1, "exit preserved across rebuild");
    }

    #[test]
    fn buy_fill_with_full_sell_side_keeps_all_exits() {
        // Pre-plan/09, a buy fill with a full sell ladder cancelled the
        // outermost (most expensive lot's) sell. Now every lot keeps its exit.
        let o = opts(&[("levels", "1"), ("qty", "1"), ("spacing", "10"), ("trend_filter", "off")]);
        let mut g = GridBot::new(&o).unwrap();
        g.on_fill(100.0, true, 100.0); // exit at 110; sell side "full" for levels=1
        let sigs = g.on_fill(90.0, true, 90.0); // second lot, exit at 100
        assert_eq!(count_cancels(&sigs), 0, "no exit may be cancelled on a buy fill");
        assert_eq!(g.sell_orders.len(), 2, "both lots keep their exits");
    }

    #[test]
    fn shared_exit_level_combines_into_one_order() {
        let o = opts(&[("levels", "3"), ("qty", "1"), ("spacing", "10"), ("trend_filter", "off")]);
        let mut g = GridBot::new(&o).unwrap();
        g.on_fill(100.0, true, 100.0); // exit 110 qty 1
        let sigs = g.on_fill(100.0, true, 100.0); // same exit level again
        let cancels = sigs
            .iter()
            .filter(|s| matches!(s, TradeSignal::Cancel { is_buy: false, .. }))
            .count();
        assert_eq!(cancels, 1, "resting exit replaced by the combined order");
        let sells: Vec<(f64, f64)> = sigs
            .iter()
            .filter_map(|s| match s {
                TradeSignal::Sell { price, quantity, .. } => Some((*price, *quantity)),
                _ => None,
            })
            .collect();
        assert_eq!(sells.len(), 1);
        assert!((sells[0].1 - 2.0).abs() < 1e-9, "combined qty 2, got {}", sells[0].1);

        // The combined level fills: both lots close, realized 2 × spacing.
        g.on_fill(110.0, false, 110.0);
        assert!((g.book.realized_pnl - 20.0).abs() < 1e-9, "got {}", g.book.realized_pnl);
        assert!(g.book.lots.is_empty());
        assert!(g.sell_orders.is_empty());
    }

    #[test]
    fn downtrend_reduces_depth_and_qty() {
        // capital 600 → qty 1.0 × 3 levels at mid 100. In a downtrend the
        // ladder shrinks to downtrend_levels (1) at qty × 0.5.
        let o = opts(&[("levels", "3"), ("capital", "600"), ("spacing", "10")]);
        let mut g = GridBot::new(&o).unwrap();
        g.on_trend_update(120.0); // mid 100 is 16.7% below → downtrend
        g.build_grid(100.0);
        assert_eq!(g.buy_orders.len(), 1, "downtrend depth defaults to 1");
        let &(_, qty) = g.buy_orders.values().next().unwrap();
        assert!((qty - 0.5).abs() < 1e-9, "downtrend qty halves, got {}", qty);
    }

    #[test]
    fn downtrend_keeps_full_qty_below_min_notional() {
        // qty ≈ 0.2667 → scaled 0.1333 × 100 ≈ 13.3 < min_notional 25 → keep full.
        let o = opts(&[("levels", "3"), ("capital", "160"), ("spacing", "10")]);
        let mut g = GridBot::new(&o).unwrap();
        g.on_trend_update(120.0);
        g.build_grid(100.0);
        assert_eq!(g.buy_orders.len(), 1);
        let &(_, qty) = g.buy_orders.values().next().unwrap();
        assert!(
            (qty - g.qty).abs() < 1e-9,
            "scaled qty below min_notional must fall back to full qty, got {} vs {}",
            qty,
            g.qty
        );
    }

    #[test]
    fn inventory_cap_blocks_extension_buys() {
        // capital 200 → cap 150 (frac 0.75). qty 1 @ mid 100. After the 90 buy
        // (notional 90), an extension at 80 would reach 170 > 150 → blocked.
        let o = opts(&[("levels", "1"), ("capital", "200"), ("spacing", "10"), ("trend_filter", "off")]);
        let mut g = GridBot::new(&o).unwrap();
        g.build_grid(100.0);
        let sigs = g.on_fill(90.0, true, 90.0);
        assert!(!has_buy(&sigs), "extension must be blocked by the inventory cap");
        assert_eq!(count_sells(&sigs), 1, "the lot exit still goes out");

        // Baseline: with the cap raised to the full budget the same extension fits.
        let o = opts(&[
            ("levels", "1"),
            ("capital", "200"),
            ("spacing", "10"),
            ("trend_filter", "off"),
            ("max_inventory_frac", "1.0"),
        ]);
        let mut g = GridBot::new(&o).unwrap();
        g.build_grid(100.0);
        let sigs = g.on_fill(90.0, true, 90.0);
        assert!(has_buy(&sigs), "within the cap the extension is allowed");
    }

    #[test]
    fn serialize_restore_round_trips_orders_and_pnl() {
        let mut g = GridBot::new(&base_opts()).unwrap();
        // Build some live state: a grid, a round-trip, and an open position.
        g.build_grid(100.0);
        g.on_fill(90.0, true, 90.0);
        g.on_fill(110.0, false, 110.0);
        g.on_fill(80.0, true, 80.0);

        let json = g.serialize_state().expect("grid should serialize");

        let mut restored = GridBot::new(&base_opts()).unwrap();
        restored.restore_state(&json);

        assert_eq!(restored.buy_orders, g.buy_orders);
        assert_eq!(restored.sell_orders, g.sell_orders);
        assert_eq!(restored.emitted_buy_prices, g.emitted_buy_prices);
        assert_eq!(restored.emitted_sell_prices, g.emitted_sell_prices);
        assert!((restored.book.realized_pnl - g.book.realized_pnl).abs() < 1e-9);
        assert!((restored.book.position() - g.book.position()).abs() < 1e-9);
        assert!((restored.book.notional() - g.book.notional()).abs() < 1e-9);
        assert_eq!(restored.book.lots.len(), g.book.lots.len());
        assert_eq!(restored.total_buys, g.total_buys);
        assert_eq!(restored.total_sells, g.total_sells);
        assert_eq!(restored.last_price, g.last_price);
    }

    #[test]
    fn legacy_avg_cost_state_restores_as_seed_lot() {
        // A pre-plan/09 snapshot: avg-cost book with an open position and
        // price-only order maps. It must convert to one seed lot at avg cost
        // with a breakeven-floored exit registered in sell_orders.
        let legacy = serde_json::json!({
            "buy_orders": { "9000": 90.0 },
            "sell_orders": { "11000": 110.0 },
            "grid_lower_bound": 90.0,
            "grid_upper_bound": 110.0,
            "book": {
                "position": 2.0,
                "position_cost": 180.0,
                "realized_pnl": 5.0,
                "fees_paid": 1.0,
                "maker_fee": 0.0
            },
            "total_buys": 3,
            "total_sells": 1,
            "spacing": 10.0,
            "price_decimals": 2,
            "trend_ema": null,
            "last_price": 100.0,
            "unprofitable": false,
            "emitted_buy_prices": [],
            "emitted_sell_prices": [],
            "qty": 1.0,
            "levels_per_side": 3,
            "sized": true,
            "base_seeded": true,
            "unfundable": false
        });
        let mut g = GridBot::new(&base_opts()).unwrap();
        g.restore_state(&legacy.to_string());

        assert!((g.book.position() - 2.0).abs() < 1e-9);
        assert_eq!(g.book.lots.len(), 1, "position converts to a single seed lot");
        assert!((g.book.lots[0].entry_price - 90.0).abs() < 1e-9, "lot at avg cost");
        assert!((g.book.realized_pnl - 5.0).abs() < 1e-9);
        assert!((g.book.fees_paid - 1.0).abs() < 1e-9);
        assert_eq!(g.sell_orders.len(), 1, "seed lot's exit is registered");
        let &(exit, qty) = g.sell_orders.values().next().unwrap();
        assert!(exit >= 90.0, "exit {} must be at or above the lot's cost", exit);
        assert!((qty - 2.0).abs() < 1e-9);
        assert_eq!(g.buy_orders.len(), 1, "legacy buy levels carry the saved qty");
        assert!((g.buy_orders.values().next().unwrap().1 - 1.0).abs() < 1e-9);
    }

    #[test]
    fn restore_state_tolerates_garbage() {
        let mut g = GridBot::new(&base_opts()).unwrap();
        g.build_grid(100.0);
        let before = g.buy_orders.clone();
        g.restore_state("not json at all");
        // Left unchanged on malformed input.
        assert_eq!(g.buy_orders, before);
    }

    #[test]
    fn max_position_suppresses_buys() {
        let o = opts(&[
            ("levels", "3"),
            ("qty", "1"),
            ("spacing", "10"),
            ("trend_filter", "off"),
            ("max_position", "1"),
        ]);
        let mut g = GridBot::new(&o).unwrap();
        // After this fill position == max_position, so no new buy extension.
        let sigs = g.on_fill(100.0, true, 100.0);
        assert!(!has_buy(&sigs), "buys should be suppressed at max_position");

        // Baseline: unbounded inventory does emit a buy extension on a buy fill.
        let mut unbounded = GridBot::new(&base_opts()).unwrap();
        let sigs = unbounded.on_fill(100.0, true, 100.0);
        assert!(has_buy(&sigs), "unbounded grid should emit a buy extension");
    }

    #[test]
    fn requires_qty_or_capital() {
        let o = opts(&[("levels", "2"), ("spacing", "10")]);
        assert!(GridBot::new(&o).is_err(), "neither qty nor capital should be rejected");
    }

    #[test]
    fn explicit_qty_is_back_compat() {
        let o = opts(&[("levels", "2"), ("qty", "0.5"), ("spacing", "10"), ("trend_filter", "off")]);
        let mut g = GridBot::new(&o).unwrap();
        assert!(g.sized, "explicit qty means already sized");
        assert!((g.qty - 0.5).abs() < 1e-9);
        g.build_grid(100.0);
        assert!((g.qty - 0.5).abs() < 1e-9, "explicit qty must not be overwritten by sizing");
        assert!(!g.unfundable);
    }

    #[test]
    fn capital_derives_qty_from_buy_budget() {
        // buy budget = 600 × 0.5 = 300 → qty = 300 / (3 × 100) = 1.0
        let o = opts(&[("levels", "3"), ("capital", "600"), ("spacing", "10"), ("trend_filter", "off")]);
        let mut g = GridBot::new(&o).unwrap();
        assert!(!g.sized, "capital-based grid sizes lazily at first build");
        let sigs = g.build_grid(100.0);
        assert!((g.qty - 1.0).abs() < 1e-9, "derived qty {}", g.qty);
        let buy_budget = 600.0 * 0.5;
        assert!(
            (g.qty * 100.0 * g.levels_per_side as f64 - buy_budget).abs() < 1e-6,
            "qty × midpoint × levels ({}) should ≈ buy budget ({})",
            g.qty * 100.0 * g.levels_per_side as f64,
            buy_budget
        );
        assert!(!g.unfundable);
        assert!(!sigs.is_empty());
    }

    #[test]
    fn capital_below_one_level_is_unfundable() {
        // buy budget = 40 × 0.5 = 20 < min_notional 25 → cannot fund any level.
        let o = opts(&[("levels", "3"), ("capital", "40"), ("spacing", "10"), ("trend_filter", "off")]);
        let mut g = GridBot::new(&o).unwrap();
        let sigs = g.build_grid(100.0);
        assert!(sigs.is_empty(), "unfundable grid must emit no orders");
        assert!(g.unfundable);
        assert!(g.summary().unwrap().contains("NO"));
    }

    #[test]
    fn capital_reduces_levels_to_what_budget_funds() {
        // buy budget = 120 × 0.5 = 60 → max fundable = floor(60 / 25) = 2 levels.
        let o = opts(&[("levels", "6"), ("capital", "120"), ("spacing", "10"), ("trend_filter", "off")]);
        let mut g = GridBot::new(&o).unwrap();
        g.build_grid(100.0);
        assert_eq!(g.levels_per_side, 2, "levels should reduce to what the budget funds");
        assert!(!g.unfundable);
        // Each level clears min_notional: qty = 60 / (2 × 100) = 0.3 → 0.3 × 100 = 30 ≥ 25.
        assert!(g.qty * 100.0 >= 25.0 - 1e-9, "per-level notional {}", g.qty * 100.0);
    }

    #[test]
    fn no_base_is_buy_first_with_empty_sell_ladder() {
        let o = opts(&[("levels", "3"), ("capital", "600"), ("spacing", "10"), ("trend_filter", "off")]);
        let mut g = GridBot::new(&o).unwrap();
        let sigs = g.build_grid(100.0);
        assert_eq!(g.sell_orders.len(), 0, "no held base → no exits yet");
        assert!(!g.buy_orders.is_empty(), "buy ladder should be populated");
        assert_eq!(count_sells(&sigs), 0, "no initial sell signals without inventory");
    }

    #[test]
    fn no_short_invariant_caps_sell_emission() {
        // Hold exactly one unit of base: it becomes a seed lot with one exit,
        // and once that exit fills the grid is flat — no further sell may be
        // emitted (no net short).
        let o = opts(&[
            ("levels", "3"),
            ("qty", "1"),
            ("spacing", "10"),
            ("trend_filter", "off"),
            ("initial_base_balance", "1"),
        ]);
        let mut g = GridBot::new(&o).unwrap();
        let sigs = g.build_grid(100.0);
        assert_eq!(count_sells(&sigs), 1, "only 1 exit backed by 1 unit of held base");
        assert!((g.book.position() - 1.0).abs() < 1e-9, "seeded position {}", g.book.position());

        let exit = sell_prices(&sigs)[0];
        let after = g.on_fill(exit, false, exit);
        assert!(g.book.position() >= -1e-9, "position must not go net short: {}", g.book.position());
        assert_eq!(count_sells(&after), 0, "no sell emitted when flat");
        assert!(g.book.realized_pnl > 0.0, "seed round trip realizes a profit");
    }

    #[test]
    fn seeded_base_round_trips_in_serialize_restore() {
        let o = opts(&[
            ("levels", "2"),
            ("capital", "600"),
            ("spacing", "10"),
            ("trend_filter", "off"),
        ]);
        let mut g = GridBot::new(&o).unwrap();
        g.build_grid(100.0);
        g.on_fill(90.0, true, 90.0);
        let json = g.serialize_state().expect("should serialize");

        // Rebuild from the same options (qty/levels not yet derived) then restore.
        let mut restored = GridBot::new(&o).unwrap();
        restored.restore_state(&json);
        assert!((restored.qty - g.qty).abs() < 1e-9, "derived qty must survive restore");
        assert_eq!(restored.levels_per_side, g.levels_per_side);
        assert!(restored.sized && restored.base_seeded);
        assert!((restored.book.position() - g.book.position()).abs() < 1e-9);
    }

    fn tick(price: f64, ts_secs: i64) -> MarketData {
        use chrono::TimeZone;
        MarketData {
            symbol: "TESTUSD".to_string(),
            bid: price,
            bid_size: 0.0,
            ask: price,
            ask_size: 0.0,
            last_price: price,
            volume: 0.0,
            high: price,
            low: price,
            daily_change: 0.0,
            daily_change_pct: 0.0,
            timestamp: chrono::Utc.timestamp_opt(ts_secs, 0).single().unwrap(),
        }
    }

    #[test]
    fn dead_zone_recenters_with_cancels_then_respects_cooldown() {
        // spacing 1, levels 2 → buys at 99/98, no exits (no held base). Mid 105
        // is inside the ×0.95/×1.05 far-outside bounds but > 2.5 × spacing from
        // every buy level: the pre-plan/08 deadlock (no fill → no slide → no fill).
        let o = opts(&[("levels", "2"), ("qty", "1"), ("spacing", "1"), ("trend_filter", "off")]);
        let mut g = GridBot::new(&o).unwrap();
        g.build_grid(100.0);
        assert!(g.on_tick(&tick(102.0, 0)).is_empty(), "first tick soft-resumes");

        let sigs = g.on_tick(&tick(102.0, 10));
        assert_eq!(count_cancels(&sigs), 2, "both stale buys cancelled");
        assert!(has_buy(&sigs), "fresh ladder built around mid");

        // Drift into another dead zone inside the cooldown → no rebuild.
        let sigs = g.on_tick(&tick(106.0, 20));
        assert_eq!(count_cancels(&sigs), 0, "no rebuild within cooldown");

        // After the cooldown elapses the re-center fires.
        let sigs = g.on_tick(&tick(106.0, 10 + REBUILD_COOLDOWN_SECS + 1));
        assert!(count_cancels(&sigs) > 0, "re-center after cooldown");
    }

    #[test]
    fn no_recenter_while_a_level_is_near() {
        let o = opts(&[("levels", "2"), ("qty", "1"), ("spacing", "1"), ("trend_filter", "off")]);
        let mut g = GridBot::new(&o).unwrap();
        g.build_grid(100.0); // buys at 99/98
        g.on_tick(&tick(100.0, 0));
        // 100.5 is within 2.5 × spacing of the 99 buy → grid is workable, no rebuild.
        let sigs = g.on_tick(&tick(100.5, 10));
        assert_eq!(count_cancels(&sigs), 0, "near level must suppress re-center");
    }

    #[test]
    fn rebuild_resizes_with_net_realized_pnl() {
        // capital 600, reserve 0.5, levels 3 @ mid 100 → qty 1. With +60 net
        // realized, a re-size derives from budget 660 → qty 1.1 (compounding).
        let o = opts(&[("levels", "3"), ("capital", "600"), ("spacing", "10"), ("trend_filter", "off")]);
        let mut g = GridBot::new(&o).unwrap();
        g.build_grid(100.0);
        assert!((g.qty - 1.0).abs() < 1e-9, "baseline qty {}", g.qty);

        g.book.realized_pnl = 60.0;
        g.sized = false;
        g.build_grid(100.0);
        assert!((g.qty - 1.1).abs() < 1e-9, "compounded qty {}", g.qty);
    }

    #[test]
    fn losses_beyond_budget_clamp_to_unfundable() {
        let o = opts(&[("levels", "3"), ("capital", "600"), ("spacing", "10"), ("trend_filter", "off")]);
        let mut g = GridBot::new(&o).unwrap();
        g.build_grid(100.0);

        // Effective budget 600 − 560 = 40 → buy budget 20 < min_notional 25.
        g.book.realized_pnl = -560.0;
        g.sized = false;
        let sigs = g.build_grid(100.0);
        assert!(sigs.is_empty(), "shrunk budget must emit no orders");
        assert!(g.unfundable);
    }

    #[test]
    fn levels_recover_when_budget_grows() {
        // buy budget 60 funds 2 of 6 requested levels; +180 realized grows the
        // budget to 300 (buy budget 150) → all 6 levels restored.
        let o = opts(&[("levels", "6"), ("capital", "120"), ("spacing", "10"), ("trend_filter", "off")]);
        let mut g = GridBot::new(&o).unwrap();
        g.build_grid(100.0);
        assert_eq!(g.levels_per_side, 2, "budget-capped level count");

        g.book.realized_pnl = 180.0;
        g.sized = false;
        g.build_grid(100.0);
        assert_eq!(g.levels_per_side, 6, "grown budget must restore levels");
    }

    #[test]
    fn expected_exits_mirror_resting_lot_exits() {
        let o = opts(&[("levels", "3"), ("qty", "1"), ("spacing", "10"), ("trend_filter", "off")]);
        let mut g = GridBot::new(&o).unwrap();
        assert!(g.expected_exits().is_empty());
        g.on_fill(100.0, true, 100.0);
        g.on_fill(90.0, true, 90.0);
        let exits = g.expected_exits();
        assert_eq!(exits.len(), 2);
        let prices = sell_prices(&exits);
        assert!(prices.contains(&110.0) && prices.contains(&100.0), "got {:?}", prices);
    }
}
