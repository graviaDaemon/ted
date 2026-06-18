use crate::algorithm::Algorithm;
use crate::algorithm::position::AvgCostBook;
use crate::api::{MarketData, TradeSignal};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, PartialEq)]
enum TrendFilter {
    Off,
    Ema,
}

/// Dynamic grid state persisted for resume. Configuration-derived fields
/// (levels, qty, fees, trend/risk settings) are not serialized — they are
/// rebuilt from options on spawn; only the evolving runtime state is restored.
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
    ema: Option<f64>,
    last_price: Option<f64>,
    unprofitable: bool,
    emitted_buy_prices: HashSet<u64>,
    emitted_sell_prices: HashSet<u64>,
}

pub struct GridBot {
    levels_per_side: u32,
    qty: f64,
    spacing: f64,
    price_decimals: u32,

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
    ema: Option<f64>,

    max_position: f64,
    stop_loss_pct: Option<f64>,

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

        let qty = options
            .get("qty")
            .ok_or("Missing required option: qty")?
            .parse::<f64>()
            .map_err(|_| "Option 'qty' must be a valid number".to_string())?;

        if qty <= 0.0 {
            return Err("Option 'qty' must be positive".to_string());
        }

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
            .unwrap_or(0.0);

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
            qty,
            spacing,
            price_decimals,
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
            ema: None,
            max_position,
            stop_loss_pct,
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
            TrendFilter::Ema => match (self.ema, self.last_price) {
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

    /// True when the strategy may place new buy orders: inside inventory cap,
    /// not stopped out, and not fighting a strong uptrend.
    fn buys_enabled(&self, price: f64) -> bool {
        if self.book.position >= self.max_position {
            return false;
        }
        if self.stopped_out(price) {
            return false;
        }
        match self.trend_filter {
            TrendFilter::Off => true,
            TrendFilter::Ema => match self.ema {
                Some(ema) if ema > 0.0 => (price - ema) / ema <= self.trend_threshold,
                _ => true,
            },
        }
    }

    /// True when the strategy may place new sell orders: inside inventory cap,
    /// not stopped out, and not fighting a strong downtrend.
    fn sells_enabled(&self, price: f64) -> bool {
        if self.book.position <= -self.max_position {
            return false;
        }
        if self.stopped_out(price) {
            return false;
        }
        match self.trend_filter {
            TrendFilter::Off => true,
            TrendFilter::Ema => match self.ema {
                Some(ema) if ema > 0.0 => (price - ema) / ema >= -self.trend_threshold,
                _ => true,
            },
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

    fn update_ema(&mut self, price: f64) {
        if self.trend_filter == TrendFilter::Off {
            return;
        }
        let alpha = 2.0 / (self.trend_ema_period + 1.0);
        self.ema = Some(match self.ema {
            Some(e) => e + alpha * (price - e),
            None => price,
        });
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

    fn build_grid(&mut self, midpoint: f64) -> Vec<TradeSignal> {
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

        for i in 0..self.levels_per_side {
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
}

impl Algorithm for GridBot {
    fn name(&self) -> &str {
        "grid"
    }

    fn on_tick(&mut self, tick: &MarketData) -> Vec<TradeSignal> {
        let price = (tick.bid + tick.ask) / 2.0;
        self.update_ema(price);

        if self.last_price.is_none() {
            if !self.buy_orders.is_empty() || !self.sell_orders.is_empty() {
                let lower = self.grid_lower().unwrap_or(f64::MIN);
                let upper = self.grid_upper().unwrap_or(f64::MAX);
                if price >= lower && price <= upper {
                    crate::logger::log(
                        "[GRID]",
                        &format!(
                            "Soft resume at {:.2} — grid [{:.2}–{:.2}] intact.",
                            price, lower, upper
                        ),
                    );
                    self.last_price = Some(price);
                    return vec![];
                }
                crate::logger::log(
                    "[GRID]",
                    &format!(
                        "Price {:.2} outside preserved grid [{:.2}–{:.2}] — rebuilding.",
                        price, lower, upper
                    ),
                );
                self.buy_orders.clear();
                self.sell_orders.clear();
            }

            let signals = self.build_grid(price);
            if signals.is_empty() {
                return vec![];
            }
            self.last_price = Some(price);
            return signals;
        }

        if self.buy_orders.is_empty() && self.sell_orders.is_empty() {
            let signals = self.build_grid(price);
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
            self.buy_orders.clear();
            self.sell_orders.clear();
            let signals = self.build_grid(price);
            self.last_price = Some(price);
            return signals;
        }

        let prev_price = self.last_price.unwrap();
        let mut signals: Vec<TradeSignal> = Vec::new();
        let prec = self.price_decimals as usize;

        if price > prev_price && self.sells_enabled(price) {
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
        } else if price < prev_price && self.buys_enabled(price) {
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

            if self.sells_enabled(current_price) {
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

            if self.buys_enabled(current_price) {
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

            if self.buys_enabled(current_price) {
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

            if self.sells_enabled(current_price) {
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
            let lower = self.grid_lower().unwrap_or(0.0);
            let upper = self.grid_upper().unwrap_or(0.0);
            crate::logger::log(
                "[GRID]",
                &format!(
                    "Reconnected — grid preserved ({} buys, {} sells, range ~{:.2}–{:.2}), resuming on next tick.",
                    self.buy_orders.len(), self.sell_orders.len(), lower, upper
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
            ema: self.ema,
            last_price: self.last_price,
            unprofitable: self.unprofitable,
            emitted_buy_prices: self.emitted_buy_prices.clone(),
            emitted_sell_prices: self.emitted_sell_prices.clone(),
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
        self.ema = state.ema;
        self.last_price = state.last_price;
        self.unprofitable = state.unprofitable;
        self.emitted_buy_prices = state.emitted_buy_prices;
        self.emitted_sell_prices = state.emitted_sell_prices;
        crate::logger::log_info(
            "[GRID]",
            &format!(
                "Grid state restored: {} buys, {} sells, position {:.8}, realized {:.8}.",
                self.buy_orders.len(),
                self.sell_orders.len(),
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
        Some(format!(
            "GridBot\n  Levels/side:  {}\n  Range:        {:.prec$} – {:.prec$}\n  Spacing:      {:.prec$}\n  Fees:         maker {:.6}, taker {:.6}\n  Trades:       {} buys, {} sells\n  Orders:       {} buy open, {} sell open\n  Position:     {:.8} (net qty), max {}\n  Avg cost:     {:.prec$}\n  Trend:        {} (EMA period {:.0}, threshold {:.4})\n  Realized PnL: {:.8}\n  Fees paid:    {:.8}\n  Unrealized:   {:.8} (at last mid)\n  Profitable:   {}",
            self.levels_per_side,
            lower, upper,
            self.spacing,
            self.maker_fee, self.taker_fee,
            self.total_buys, self.total_sells,
            self.buy_orders.len(), self.sell_orders.len(),
            self.book.position, max_pos,
            self.avg_cost(),
            self.trend_label(), self.trend_ema_period, self.trend_threshold,
            self.book.realized_pnl,
            self.book.fees_paid,
            unrealized,
            if self.unprofitable { "NO — spacing below fee floor, grid not building" } else { "yes" },
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
    fn trend_up_suppresses_new_buys_flat_allows_them() {
        let o = opts(&[
            ("levels", "3"),
            ("qty", "1"),
            ("spacing", "10"),
            ("trend_threshold", "0.0"),
        ]);

        // Uptrend: current price above EMA → no new buy on a sell fill.
        let mut up = GridBot::new(&o).unwrap();
        up.ema = Some(100.0);
        let sigs = up.on_fill(110.0, false, 110.0);
        assert!(!has_buy(&sigs), "uptrend should suppress new buys");

        // Flat: current price equals EMA → counter buy is emitted.
        let mut flat = GridBot::new(&o).unwrap();
        flat.ema = Some(110.0);
        let sigs = flat.on_fill(110.0, false, 110.0);
        assert!(has_buy(&sigs), "flat trend should allow new buys");
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
}
