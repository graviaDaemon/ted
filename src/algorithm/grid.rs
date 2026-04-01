use crate::algorithm::Algorithm;
use crate::api::{MarketData, TradeSignal};
use std::collections::{HashMap, HashSet};

pub struct GridBot {
    num_levels: u32,
    quantity: f64,
    spacing: f64,
    price_decimals: u32,
    initial_base: Option<f64>,

    lower: f64,
    upper: f64,
    levels: Vec<f64>,

    buy_orders: HashSet<usize>,
    sell_orders: HashSet<usize>,
    seeded_sells: HashSet<usize>,

    last_price: Option<f64>,
    position: f64,
    realized_pnl: f64,
    total_buys: u32,
    total_sells: u32,
}

impl GridBot {
    pub fn new(options: &HashMap<String, String>) -> Result<Self, String> {
        let spacing_str = options
            .get("spacing")
            .ok_or_else(|| "Missing required option: spacing".to_string())?;

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

        let num_levels = options
            .get("levels")
            .ok_or_else(|| "Missing required option: levels".to_string())?
            .parse::<u32>()
            .map_err(|_| "Option 'levels' must be a positive integer".to_string())?;

        if num_levels < 2 {
            return Err("Option 'levels' must be at least 2".to_string());
        }

        let quantity = options
            .get("qty")
            .ok_or_else(|| "Missing required option: qty".to_string())?
            .parse::<f64>()
            .map_err(|_| "Option 'qty' must be a valid number".to_string())?;

        if quantity <= 0.0 {
            return Err("Option 'qty' must be positive".to_string());
        }

        let initial_base = match options.get("initial_base") {
            None => None,
            Some(v) => {
                let x = v
                    .parse::<f64>()
                    .map_err(|_| "Option 'initial_base' must be a valid number".to_string())?;
                if x < 0.0 {
                    return Err("Option 'initial_base' cannot be negative".to_string());
                }
                Some(x)
            }
        };

        Ok(GridBot {
            num_levels,
            quantity,
            spacing,
            price_decimals,
            initial_base,
            lower: 0.0,
            upper: 0.0,
            levels: Vec::new(),
            buy_orders: HashSet::new(),
            sell_orders: HashSet::new(),
            seeded_sells: HashSet::new(),
            last_price: None,
            position: initial_base.unwrap_or(0.0),
            realized_pnl: 0.0,
            total_buys: 0,
            total_sells: 0,
        })
    }

    fn decimals_from_price(price: f64) -> u32 {
        if price <= 0.0 {
            return 2;
        }
        let magnitude = price.log10().floor() as i32;
        match magnitude {
            m if m >= 2 => 2,  // $100+   → 2 dp
            m if m >= 0 => 4,  // $1–$99  → 4 dp
            m if m >= -2 => 6, // $0.01–$0.99 → 6 dp
            _ => 8,            // sub-cent → 8 dp
        }
    }

    fn build_grid(&mut self, price: f64) {
        let m = 10_f64.powi(self.price_decimals as i32);
        let half = (self.num_levels / 2) as f64;
        self.lower = ((price - half * self.spacing) * m).round() / m;
        self.upper = ((self.lower + self.num_levels as f64 * self.spacing) * m).round() / m;
        self.levels = (0..=self.num_levels)
            .map(|i| ((self.lower + i as f64 * self.spacing) * m).round() / m)
            .collect();
        crate::logger::log(
            "[GRID]",
            &format!(
                "Grid built: {:.6} – {:.6}, {} levels, spacing {:.6}",
                self.lower, self.upper, self.num_levels, self.spacing
            ),
        );
        if self.lower <= 0.0 {
            crate::logger::log(
                "[GRID]",
                &format!(
                    "Warning: spacing {:.6} at price {:.6} produces a lower bound of {:.6} — lower grid levels will be skipped. Consider reducing spacing.",
                    self.spacing, price, self.lower
                ),
            );
        }
    }
}

impl Algorithm for GridBot {
    fn name(&self) -> &str {
        "grid"
    }

    fn on_tick(&mut self, tick: &MarketData) -> Vec<TradeSignal> {
        let price = tick.last_price;

        if self.last_price.is_none() {
            if !self.levels.is_empty() && price >= self.lower && price <= self.upper {
                crate::logger::log(
                    "[GRID]",
                    &format!(
                        "Soft resume at {:.2} — grid [{:.2}–{:.2}] intact.",
                        price, self.lower, self.upper
                    ),
                );
                self.last_price = Some(price);
                return vec![];
            }

            if !self.levels.is_empty() {
                crate::logger::log(
                    "[GRID]",
                    &format!(
                        "Price {:.2} outside preserved grid [{:.2}–{:.2}] — rebuilding.",
                        price, self.lower, self.upper
                    ),
                );
                self.levels.clear();
                self.buy_orders.clear();
                self.sell_orders.clear();
                self.seeded_sells.clear();
            }

            let price_min = Self::decimals_from_price(price);
            if self.price_decimals < price_min {
                crate::logger::log(
                    "[GRID]",
                    &format!(
                        "price_decimals {} from spacing is less than minimum {} for price {:.8} — upgrading.",
                        self.price_decimals, price_min, price
                    ),
                );
                self.price_decimals = price_min;
            }
            self.build_grid(price);

            let mut signals: Vec<TradeSignal> = Vec::new();
            let prec = self.price_decimals as usize;

            let mut buy_levels: Vec<(usize, f64)> = self
                .levels
                .iter()
                .copied()
                .enumerate()
                .filter(|&(_, level)| level < price && level > 0.0)
                .collect();
            buy_levels.reverse(); // nearest first

            if let Some(&(i, level)) = buy_levels.first() {
                signals.push(TradeSignal::Buy {
                    price: level,
                    quantity: self.quantity,
                    reason: format!(
                        "Grid initial buy at {:.prec$} (level {}/{})",
                        level,
                        i,
                        self.num_levels,
                        prec = prec
                    ),
                    price_decimals: self.price_decimals,
                });
            }

            for &(i, _) in buy_levels.iter().skip(1) {
                self.buy_orders.insert(i);
            }

            let max_sells = match self.initial_base {
                None => 0,
                Some(base) => {
                    let n = (base / self.quantity).floor() as usize;
                    let remainder = base - (n as f64 * self.quantity);
                    if remainder > 1e-8 {
                        crate::logger::log(
                            "[GRID]",
                            &format!(
                                "initial_base {:.8} is not evenly divisible by qty {:.8}. \
                                 {:.8} units will not be tracked by the grid.",
                                base, self.quantity, remainder
                            ),
                        );
                    }
                    n
                }
            };

            let mut seeded = 0;
            for (i, &level) in self.levels.iter().enumerate() {
                if level > price && level > 0.0 && seeded < max_sells {
                    self.seeded_sells.insert(i);
                    signals.push(TradeSignal::Sell {
                        price: level,
                        quantity: self.quantity,
                        reason: format!(
                            "Grid initial sell at {:.prec$} (level {}/{})",
                            level,
                            i,
                            self.num_levels,
                            prec = prec
                        ),
                        price_decimals: self.price_decimals,
                    });
                    seeded += 1;
                }
            }

            crate::logger::log(
                "[GRID]",
                &format!(
                    "Initial orders: {} buy(s), {} sell(s) — grid active.",
                    signals
                        .iter()
                        .filter(|s| matches!(s, TradeSignal::Buy { .. }))
                        .count(),
                    signals
                        .iter()
                        .filter(|s| matches!(s, TradeSignal::Sell { .. }))
                        .count(),
                ),
            );

            self.last_price = Some(price);
            return signals;
        }

        if price < self.lower || price > self.upper {
            crate::logger::log(
                "[GRID]",
                &format!(
                    "Price {:.2} is outside grid range [{:.2}, {:.2}] — skipping tick",
                    price, self.lower, self.upper
                ),
            );
            self.last_price = Some(price);
            return vec![];
        }

        let prev_price = self.last_price.unwrap();
        let mut signals: Vec<TradeSignal> = Vec::new();

        if price > prev_price {
            let mut triggered: Vec<usize> = self
                .sell_orders
                .iter()
                .copied()
                .filter(|&i| self.levels[i] <= price)
                .collect();
            triggered.sort_unstable();

            for idx in triggered {
                let sell_price = self.levels[idx];
                self.sell_orders.remove(&idx);

                signals.push(TradeSignal::Sell {
                    price: sell_price,
                    quantity: self.quantity,
                    reason: format!(
                        "Grid sell at {:.prec$} (level {}/{})",
                        sell_price,
                        idx,
                        self.num_levels,
                        prec = self.price_decimals as usize
                    ),
                    price_decimals: self.price_decimals,
                });
            }
        } else if price < prev_price {
            let mut triggered: Vec<usize> = self
                .buy_orders
                .iter()
                .copied()
                .filter(|&i| self.levels[i] >= price)
                .collect();
            triggered.sort_unstable_by(|a, b| b.cmp(a));

            for idx in triggered {
                let buy_price = self.levels[idx];
                self.buy_orders.remove(&idx);

                signals.push(TradeSignal::Buy {
                    price: buy_price,
                    quantity: self.quantity,
                    reason: format!(
                        "Grid buy at {:.prec$} (level {}/{})",
                        buy_price,
                        idx,
                        self.num_levels,
                        prec = self.price_decimals as usize
                    ),
                    price_decimals: self.price_decimals,
                });
            }
        }

        self.last_price = Some(price);
        signals
    }

    fn on_fill(&mut self, price: f64, is_buy: bool) {
        let tolerance = self.spacing * 0.1;
        let idx = match self
            .levels
            .iter()
            .position(|&l| (l - price).abs() < tolerance)
        {
            Some(i) => i,
            None => {
                crate::logger::log(
                    "[GRID]",
                    &format!("on_fill: no level found near {:.6} — ignoring.", price),
                );
                return;
            }
        };

        if is_buy {
            self.position += self.quantity;
            self.total_buys += 1;
            if idx + 1 < self.levels.len() {
                self.sell_orders.insert(idx + 1);
                crate::logger::log(
                    "[GRID]",
                    &format!(
                        "Buy filled @ {:.6} — sell seeded at {:.6}",
                        price,
                        self.levels[idx + 1]
                    ),
                );
            }
        } else {
            self.position -= self.quantity;
            self.total_sells += 1;
            if !self.seeded_sells.contains(&idx) {
                self.realized_pnl += self.spacing * self.quantity;
            }
            self.seeded_sells.remove(&idx);
            if idx > 0 {
                self.buy_orders.insert(idx - 1);
                crate::logger::log(
                    "[GRID]",
                    &format!(
                        "Sell filled @ {:.6} — buy seeded at {:.6}",
                        price,
                        self.levels[idx - 1]
                    ),
                );
            }
        }
    }

    fn on_reconnect(&mut self) {
        self.last_price = None;
        if self.levels.is_empty() {
            crate::logger::log(
                "[GRID]",
                "Reconnected — no grid built yet, will initialise on next tick.",
            );
        } else {
            crate::logger::log(
                "[GRID]",
                &format!(
                    "Reconnected — grid preserved ({:.2}–{:.2}), resuming on next tick.",
                    self.lower, self.upper
                ),
            );
        }
    }

    fn on_live_enabled(&mut self) {
        self.buy_orders.clear();
        self.sell_orders.clear();
        self.seeded_sells.clear();
        self.levels.clear();
        self.last_price = None;
        crate::logger::log(
            "[GRID]",
            "Live enabled — grid reset, will rebuild on next tick.",
        );
    }

    fn summary(&self) -> Option<String> {
        let base_mode = match self.initial_base {
            None => "no initial base (buy first)".to_string(),
            Some(b) => format!("{:.8} units held at start", b),
        };
        Some(format!(
            "GridBot\n  Range:        {:.6} – {:.6}\n  Spacing:      {:.6}  |  Levels: {}\n  Initial base: {}\n  Trades:       {} buys, {} sells\n  Orders:       {} buy open, {} sell open\n  Position:     {:.8} (net qty)\n  Realized PnL: {:.8}",
            self.lower, self.upper,
            self.spacing, self.num_levels,
            base_mode,
            self.total_buys, self.total_sells,
            self.buy_orders.len(), self.sell_orders.len(),
            self.position,
            self.realized_pnl,
        ))
    }
}
