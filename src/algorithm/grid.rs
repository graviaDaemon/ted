use crate::algorithm::Algorithm;
use crate::algorithm::position::AvgCostBook;
use crate::api::{MarketData, TradeSignal};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

const REBUILD_COOLDOWN_SECS: i64 = 300;

#[derive(Clone, Copy, PartialEq)]
enum TrendFilter {
    Off,
    Ema,
}

/// Dynamic grid state persisted for resume. Most configuration-derived fields
/// (fees, trend/risk settings) are rebuilt from options on spawn; but the
/// capital-derived `qty` and the possibly-reduced `levels_per_side` are computed
/// once at runtime from a live midpoint, so they (and the sizing/seeding latches)
/// are serialized to survive a restart.
#[derive(Serialize, Deserialize)]
struct GridState {
    buy_orders: HashMap<u64, f64>,
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
    unprofitable: bool,
    emitted_buy_prices: HashSet<u64>,
    emitted_sell_prices: HashSet<u64>,
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

    buy_orders: HashMap<u64, f64>,
    sell_orders: HashMap<u64, f64>,

    base_balance: f64,
    quote_balance: f64,

    last_price: Option<f64>,
    book: AvgCostBook,
    total_buys: u32,
    total_sells: u32,

    maker_fee: f64,
    taker_fee: f64,
    allow_unprofitable: bool,
    unprofitable: bool,

    trend_filter: TrendFilter,
    trend_ema_period: f64,
    trend_threshold: f64,
    // Candle-close EMA pushed in by the runner/backtester via `on_trend_update`
    // (plan/08 step 2) — not derived from ticks.
    trend_ema: Option<f64>,

    max_position: f64,
    stop_loss_pct: Option<f64>,

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
            book: AvgCostBook::new(maker_fee),
            total_buys: 0,
            total_sells: 0,
            maker_fee,
            taker_fee,
            allow_unprofitable,
            unprofitable: false,
            trend_filter,
            trend_ema_period,
            trend_threshold,
            trend_ema: None,
            max_position,
            stop_loss_pct,
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

    /// True when the strategy may add buy exposure: inside inventory cap and not
    /// stopped out. Counter-buys and planned ladder levels are gated by this
    /// alone — never by the trend filter (plan/08 step 1).
    fn risk_allows_buy(&self, price: f64) -> bool {
        self.book.position < self.max_position && !self.stopped_out(price)
    }

    /// True when the trend filter forbids *extending* the buy ladder: the trend
    /// EMA is known and price sits below it beyond the threshold (a real
    /// downtrend). The only move this blocks is adding fresh exposure below the
    /// ladder into a falling market; exits are never trend-gated.
    fn trend_blocks_extension_buy(&self, price: f64) -> bool {
        if self.trend_filter != TrendFilter::Ema {
            return false;
        }
        match self.trend_ema {
            Some(ema) if ema > 0.0 => (price - ema) / ema < -self.trend_threshold,
            _ => false,
        }
    }

    /// True when the unrealized loss on the open long exceeds `stop_loss_pct`
    /// of the capital deployed into it. Suppresses new orders on both sides.
    fn stopped_out(&self, price: f64) -> bool {
        match self.stop_loss_pct {
            Some(pct) if self.book.position > 0.0 && self.book.position_cost > 0.0 && price > 0.0 => {
                let unrealized = (price - self.avg_cost()) * self.book.position;
                unrealized < 0.0 && (-unrealized) > pct * self.book.position_cost
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

    /// Net base the grid currently holds: seeded opening inventory plus
    /// grid-acquired position. The spot-only invariant is that this never goes
    /// below zero.
    fn held_base(&self) -> f64 {
        self.book.position
    }

    /// Base not already earmarked by resting sell orders — the room available for a
    /// *new* sell without risking a net-short position once everything fills.
    fn sell_headroom(&self) -> f64 {
        self.held_base() - self.qty * self.sell_orders.len() as f64
    }

    /// True when one more `qty` sell can rest without driving net base below zero.
    fn can_place_sell(&self) -> bool {
        self.qty > 0.0 && self.sell_headroom() >= self.qty - 1e-9
    }

    fn build_grid(&mut self, midpoint: f64) -> Vec<TradeSignal> {
        // Derive sizing from the capital budget the first time we have a midpoint,
        // and seed any pre-existing base inventory into the book so the no-short
        // invariant (position >= 0) accounts for base we already hold.
        if !self.sized {
            self.size_from_capital(midpoint);
            self.sized = true;
        }
        if !self.base_seeded {
            self.book.seed_position(self.initial_base_balance, midpoint);
            self.base_seeded = true;
        }
        if self.unfundable {
            return vec![];
        }

        if !self.allow_unprofitable && self.maker_fee > 0.0 {
            let fee_floor = 2.0 * self.maker_fee * midpoint;
            if self.spacing <= fee_floor {
                if !self.unprofitable {
                    self.unprofitable = true;
                    crate::logger::log_warn(
                        "[GRID]",
                        &format!(
                            "Spacing {:.8} <= fee floor {:.8} (2 × maker_fee × midpoint {:.2}) — grid is structurally unprofitable at current fees; refusing to build. Set allow_unprofitable=true to override.",
                            self.spacing, fee_floor, midpoint
                        ),
                    );
                }
                return vec![];
            }
        }
        self.unprofitable = false;

        let price_min = Self::decimals_from_price(midpoint);
        if self.price_decimals < price_min {
            self.price_decimals = price_min;
        }

        let m = 10_f64.powi(self.price_decimals as i32);

        self.buy_orders.clear();
        self.sell_orders.clear();
        self.emitted_buy_prices.clear();
        self.emitted_sell_prices.clear();

        for i in 0..self.levels_per_side {
            let price = ((midpoint - (i as f64 + 1.0) * self.spacing) * m).round() / m;
            if price > 0.0 {
                self.buy_orders.insert(self.price_key(price), price);
            }
        }

        // Spot-only (plan/06 step 2): only place sell levels backed by held base.
        // With no base held the grid is buy-first — the sell ladder stays empty
        // until a buy fills and creates inventory to sell against.
        let max_sells = if self.qty > 0.0 {
            (self.held_base() / self.qty + 1e-9).floor().max(0.0) as u32
        } else {
            0
        };
        for i in 0..self.levels_per_side.min(max_sells) {
            let price = ((midpoint + (i as f64 + 1.0) * self.spacing) * m).round() / m;
            self.sell_orders.insert(self.price_key(price), price);
        }

        self.grid_lower_bound = self
            .buy_orders
            .values()
            .copied()
            .reduce(f64::min)
            .unwrap_or(midpoint - self.spacing * self.levels_per_side as f64);
        self.grid_upper_bound = self
            .sell_orders
            .values()
            .copied()
            .reduce(f64::max)
            .unwrap_or(midpoint + self.spacing * self.levels_per_side as f64);

        crate::logger::log(
            "[GRID]",
            &format!(
                "Grid built: midpoint {:.prec$}, spacing {:.prec$}, {} buys, {} sells",
                midpoint,
                self.spacing,
                self.buy_orders.len(),
                self.sell_orders.len(),
                prec = self.price_decimals as usize,
            ),
        );

        let prec = self.price_decimals as usize;
        let mut signals: Vec<TradeSignal> = Vec::new();

        let mut buy_prices: Vec<f64> = self.buy_orders.values().copied().collect();
        buy_prices.sort_by(|a, b| b.partial_cmp(a).unwrap());
        for price in buy_prices {
            signals.push(TradeSignal::Buy {
                price,
                quantity: self.qty,
                reason: format!("Grid initial buy at {:.prec$}", price, prec = prec),
                price_decimals: self.price_decimals,
            });
        }

        let mut sell_prices: Vec<f64> = self.sell_orders.values().copied().collect();
        sell_prices.sort_by(|a, b| a.partial_cmp(b).unwrap());
        for price in sell_prices {
            signals.push(TradeSignal::Sell {
                price,
                quantity: self.qty,
                reason: format!("Grid initial sell at {:.prec$}", price, prec = prec),
                price_decimals: self.price_decimals,
            });
        }

        signals
    }

    fn grid_lower(&self) -> Option<f64> {
        self.buy_orders.values().copied().reduce(f64::min)
    }

    fn grid_upper(&self) -> Option<f64> {
        self.sell_orders.values().copied().reduce(f64::max)
    }

    fn format_range(lower: Option<f64>, upper: Option<f64>) -> String {
        let side = |v: Option<f64>, name: &str| {
            v.map(|p| format!("{:.2}", p))
                .unwrap_or_else(|| format!("no {}", name))
        };
        format!("[{}–{}]", side(lower, "buys"), side(upper, "sells"))
    }

    /// Drain every tracked level into `Cancel` signals so a rebuild does not
    /// orphan the resting exchange orders (plan/08 step 3). Callers prepend
    /// these to `build_grid`'s output.
    fn drain_to_cancels(&mut self) -> Vec<TradeSignal> {
        let prec = self.price_decimals as usize;
        let mut signals: Vec<TradeSignal> =
            Vec::with_capacity(self.buy_orders.len() + self.sell_orders.len());
        for price in self.buy_orders.values().copied() {
            signals.push(TradeSignal::Cancel {
                price,
                is_buy: true,
                reason: format!("Rebuild: cancel stale buy {:.prec$}", price, prec = prec),
            });
        }
        for price in self.sell_orders.values().copied() {
            signals.push(TradeSignal::Cancel {
                price,
                is_buy: false,
                reason: format!("Rebuild: cancel stale sell {:.prec$}", price, prec = prec),
            });
        }
        self.buy_orders.clear();
        self.sell_orders.clear();
        self.emitted_buy_prices.clear();
        self.emitted_sell_prices.clear();
        signals
    }

    /// True when no grid level on either side rests within
    /// `recenter_band × spacing` of the mid — the fill-less deadlock plan/08
    /// step 3 re-centers out of. Not triggered while a side still has a level
    /// near enough to fill.
    fn in_dead_zone(&self, mid: f64) -> bool {
        let band = self.recenter_band * self.spacing;
        let near = |orders: &HashMap<u64, f64>| orders.values().any(|&p| (p - mid).abs() <= band);
        !near(&self.buy_orders) && !near(&self.sell_orders)
    }

    fn rebuild_cooldown_active(&self, now: DateTime<Utc>) -> bool {
        self.last_rebuild_at
            .map(|t| now - t < chrono::Duration::seconds(REBUILD_COOLDOWN_SECS))
            .unwrap_or(false)
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
                        "Price {:.2} outside preserved grid {} — rebuilding.",
                        price, range
                    ),
                );
                let mut signals = self.drain_to_cancels();
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
        {
            crate::logger::log(
                "[GRID]",
                &format!(
                    "Price {:.2} is far outside grid [{:.2}–{:.2}] — rebuilding.",
                    price, self.grid_lower_bound, self.grid_upper_bound
                ),
            );
            let mut signals = self.drain_to_cancels();
            self.sized = false;
            signals.extend(self.build_grid(price));
            self.last_rebuild_at = Some(tick.timestamp);
            self.last_price = Some(price);
            return signals;
        }

        // Fill-less re-centering (plan/08 step 3): a stalled grid whose nearest
        // level on both sides drifted outside the band can never slide on fills
        // again — cancel the stale ladder and rebuild around the mid. The
        // cooldown stops a fast wick from thrashing cancel/replace cycles.
        if self.in_dead_zone(price) && !self.rebuild_cooldown_active(tick.timestamp) {
            crate::logger::log(
                "[GRID]",
                &format!(
                    "No grid level within {:.1} × spacing of mid {:.2} ({}) — re-centering.",
                    self.recenter_band,
                    price,
                    Self::format_range(self.grid_lower(), self.grid_upper()),
                ),
            );
            let mut signals = self.drain_to_cancels();
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
            let triggered: Vec<(u64, f64)> = self
                .sell_orders
                .iter()
                .filter(|&(k, &v)| v <= price && !self.emitted_sell_prices.contains(k))
                .map(|(&k, &v)| (k, v))
                .collect();
            for (key, sell_price) in triggered {
                self.emitted_sell_prices.insert(key);
                signals.push(TradeSignal::Sell {
                    price: sell_price,
                    quantity: self.qty,
                    reason: format!("Grid sell at {:.prec$}", sell_price, prec = prec),
                    price_decimals: self.price_decimals,
                });
            }
        } else if price < prev_price && self.risk_allows_buy(price) {
            let triggered: Vec<(u64, f64)> = self
                .buy_orders
                .iter()
                .filter(|&(k, &v)| v >= price && !self.emitted_buy_prices.contains(k))
                .map(|(&k, &v)| (k, v))
                .collect();
            for (key, buy_price) in triggered {
                self.emitted_buy_prices.insert(key);
                signals.push(TradeSignal::Buy {
                    price: buy_price,
                    quantity: self.qty,
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
            self.buy_orders.remove(&self.price_key(fill_price));
            self.emitted_buy_prices.remove(&self.price_key(fill_price));
            self.book.record_buy(fill_price, self.qty);
            self.total_buys += 1;

            if self.sell_orders.len() >= self.levels_per_side as usize
                && let Some(outer_sell) = self.sell_orders.values().copied().reduce(f64::max)
            {
                self.sell_orders.remove(&self.price_key(outer_sell));
                signals.push(TradeSignal::Cancel {
                    price: outer_sell,
                    is_buy: false,
                    reason: format!(
                        "Sliding grid: replace outermost sell {:.prec$}",
                        outer_sell,
                        prec = prec
                    ),
                });
            }

            // Counter-sell: the exit leg of the round trip. Reduces exposure, so
            // it is gated only by the no-short inventory check — never by the
            // trend filter or stop (plan/08 step 1).
            if self.can_place_sell() {
                let counter = ((current_price + self.spacing) * m).round() / m;
                self.sell_orders.insert(self.price_key(counter), counter);
                crate::logger::log(
                    "[GRID]",
                    &format!(
                        "Buy filled @ {:.prec$} — counter sell at {:.prec$}",
                        fill_price,
                        counter,
                        prec = prec
                    ),
                );
                signals.push(TradeSignal::Sell {
                    price: counter,
                    quantity: self.qty,
                    reason: format!("Grid counter sell at {:.prec$}", counter, prec = prec),
                    price_decimals: self.price_decimals,
                });
            }

            // Extension buy: fresh exposure below the ladder — the one move the
            // trend filter gates (only against a real downtrend).
            if self.risk_allows_buy(current_price)
                && !self.trend_blocks_extension_buy(current_price)
            {
                let lowest_buy = self
                    .buy_orders
                    .values()
                    .copied()
                    .reduce(f64::min)
                    .unwrap_or(fill_price);
                let extension = ((lowest_buy - self.spacing) * m).round() / m;
                if extension > 0.0 {
                    self.buy_orders.insert(self.price_key(extension), extension);
                    self.grid_lower_bound = extension;
                    signals.push(TradeSignal::Buy {
                        price: extension,
                        quantity: self.qty,
                        reason: format!("Grid extension buy at {:.prec$}", extension, prec = prec),
                        price_decimals: self.price_decimals,
                    });
                }
            }
        } else {
            self.sell_orders.remove(&self.price_key(fill_price));
            self.emitted_sell_prices.remove(&self.price_key(fill_price));
            self.book.record_sell(fill_price, self.qty);
            self.total_sells += 1;

            if self.buy_orders.len() >= self.levels_per_side as usize
                && let Some(outer_buy) = self.buy_orders.values().copied().reduce(f64::min)
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
            // the risk checks only — never by the trend filter (plan/08 step 1).
            if self.risk_allows_buy(current_price) {
                let counter = ((current_price - self.spacing) * m).round() / m;
                if counter > 0.0 {
                    self.buy_orders.insert(self.price_key(counter), counter);
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
                        quantity: self.qty,
                        reason: format!("Grid counter buy at {:.prec$}", counter, prec = prec),
                        price_decimals: self.price_decimals,
                    });
                } else {
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

            if self.can_place_sell() {
                let highest_sell = self
                    .sell_orders
                    .values()
                    .copied()
                    .reduce(f64::max)
                    .unwrap_or(fill_price);
                let extension = ((highest_sell + self.spacing) * m).round() / m;
                self.sell_orders
                    .insert(self.price_key(extension), extension);
                self.grid_upper_bound = extension;
                signals.push(TradeSignal::Sell {
                    price: extension,
                    quantity: self.qty,
                    reason: format!("Grid extension sell at {:.prec$}", extension, prec = prec),
                    price_decimals: self.price_decimals,
                });
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
        self.unprofitable = false;
        crate::logger::log("[GRID]", &format!("Spacing updated: {:.8}", new_spacing));
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
                    "Reconnected — grid preserved ({} buys, {} sells, range ~{}), resuming on next tick.",
                    self.buy_orders.len(),
                    self.sell_orders.len(),
                    Self::format_range(self.grid_lower(), self.grid_upper()),
                ),
            );
        }
    }

    fn position(&self) -> f64 {
        self.book.position
    }

    fn unrealized_pnl(&self, mid: f64) -> f64 {
        if self.book.position > 0.0 && mid > 0.0 {
            (mid - self.avg_cost()) * self.book.position
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

    fn serialize_state(&self) -> Option<String> {
        let state = GridState {
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
            unprofitable: self.unprofitable,
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
            Err(e) => {
                crate::logger::log_warn(
                    "[GRID]",
                    &format!("Could not restore grid state ({}) — starting fresh.", e),
                );
                return;
            }
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
        self.unprofitable = state.unprofitable;
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
                "Grid state restored: {} buys, {} sells, qty {:.8}, position {:.8}, realized {:.8}.",
                self.buy_orders.len(),
                self.sell_orders.len(),
                self.qty,
                self.book.position,
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
        let status = if self.unfundable {
            "NO — capital cannot fund a level at min_notional, grid not building"
        } else if self.unprofitable {
            "NO — spacing below fee floor, grid not building"
        } else {
            "yes"
        };
        Some(format!(
            "GridBot\n  Levels/side:  {}\n  Range:        {:.prec$} – {:.prec$}\n  Spacing:      {:.prec$}\n  Sizing:       {}\n  Qty/level:    {:.8}\n  Fees:         maker {:.6}, taker {:.6}\n  Trades:       {} buys, {} sells\n  Orders:       {} buy open, {} sell open\n  Position:     {:.8} (net qty), max {}\n  Avg cost:     {:.prec$}\n  Trend:        {} (EMA period {:.0}, threshold {:.4})\n  Realized PnL: {:.8}\n  Fees paid:    {:.8}\n  Unrealized:   {:.8} (at last mid)\n  Profitable:   {}",
            self.levels_per_side,
            lower, upper,
            self.spacing,
            sizing,
            self.qty,
            self.maker_fee, self.taker_fee,
            self.total_buys, self.total_sells,
            self.buy_orders.len(), self.sell_orders.len(),
            self.book.position, max_pos,
            self.avg_cost(),
            self.trend_label(), self.trend_ema_period, self.trend_threshold,
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
    fn unprofitable_grid_refuses_to_build() {
        // fee floor = 2 × 0.01 × 100 = 2.0; spacing 0.5 is below it.
        let o = opts(&[
            ("levels", "3"),
            ("qty", "1"),
            ("spacing", "0.5"),
            ("maker_fee", "0.01"),
        ]);
        let mut g = GridBot::new(&o).unwrap();
        let signals = g.build_grid(100.0);
        assert!(signals.is_empty(), "expected no orders, got {}", signals.len());
        assert!(g.unprofitable);
        assert!(g.summary().unwrap().contains("NO"));
    }

    #[test]
    fn allow_unprofitable_overrides_fee_floor() {
        let o = opts(&[
            ("levels", "3"),
            ("qty", "1"),
            ("spacing", "0.5"),
            ("maker_fee", "0.01"),
            ("allow_unprofitable", "true"),
        ]);
        let mut g = GridBot::new(&o).unwrap();
        let signals = g.build_grid(100.0);
        assert!(!signals.is_empty());
        assert!(!g.unprofitable);
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
        assert_eq!(count_sells(&sigs), 1, "counter sell must always follow a buy fill");
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
        assert!((restored.book.position - g.book.position).abs() < 1e-9);
        assert!((restored.book.position_cost - g.book.position_cost).abs() < 1e-9);
        assert_eq!(restored.total_buys, g.total_buys);
        assert_eq!(restored.total_sells, g.total_sells);
        assert_eq!(restored.last_price, g.last_price);
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

    fn count_sells(signals: &[TradeSignal]) -> usize {
        signals.iter().filter(|s| matches!(s, TradeSignal::Sell { .. })).count()
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
        assert_eq!(g.sell_orders.len(), 0, "no held base → sell ladder starts empty");
        assert!(!g.buy_orders.is_empty(), "buy ladder should be populated");
        assert_eq!(count_sells(&sigs), 0, "no initial sell signals without inventory");
    }

    #[test]
    fn no_short_invariant_caps_sell_emission() {
        // Hold exactly one unit of base: only one sell level may rest, and once it
        // fills the would-be extension sell is suppressed (no net short).
        let o = opts(&[
            ("levels", "3"),
            ("qty", "1"),
            ("spacing", "10"),
            ("trend_filter", "off"),
            ("initial_base_balance", "1"),
        ]);
        let mut g = GridBot::new(&o).unwrap();
        let sigs = g.build_grid(100.0);
        assert_eq!(count_sells(&sigs), 1, "only 1 sell backed by 1 unit of held base");
        assert!((g.book.position - 1.0).abs() < 1e-9, "seeded position {}", g.book.position);

        // The single sell fills; inventory is now flat, so the extension sell must
        // be suppressed — the grid never emits a sell that would drive position < 0.
        let after = g.on_fill(110.0, false, 110.0);
        assert!(g.book.position >= -1e-9, "position must not go net short: {}", g.book.position);
        assert_eq!(count_sells(&after), 0, "extension sell suppressed when flat");
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
        assert!((restored.book.position - g.book.position).abs() < 1e-9);
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

    fn count_cancels(signals: &[TradeSignal]) -> usize {
        signals.iter().filter(|s| matches!(s, TradeSignal::Cancel { .. })).count()
    }

    #[test]
    fn dead_zone_recenters_with_cancels_then_respects_cooldown() {
        // spacing 1, levels 2 → buys at 99/98, no sells (no held base). Mid 105
        // is inside the ×0.95/×1.05 far-outside bounds but > 2.5 × spacing from
        // every level: the pre-plan/08 deadlock (no fill → no slide → no fill).
        let o = opts(&[("levels", "2"), ("qty", "1"), ("spacing", "1"), ("trend_filter", "off")]);
        let mut g = GridBot::new(&o).unwrap();
        g.build_grid(100.0);
        assert!(g.on_tick(&tick(105.0, 0)).is_empty(), "first tick soft-resumes");

        let sigs = g.on_tick(&tick(105.0, 10));
        assert_eq!(count_cancels(&sigs), 2, "both stale buys cancelled");
        assert!(has_buy(&sigs), "fresh ladder built around mid");

        // Drift into another dead zone inside the cooldown → no rebuild.
        let sigs = g.on_tick(&tick(110.0, 20));
        assert_eq!(count_cancels(&sigs), 0, "no rebuild within cooldown");

        // After the cooldown elapses the re-center fires.
        let sigs = g.on_tick(&tick(110.0, 10 + REBUILD_COOLDOWN_SECS + 1));
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
}
