use std::collections::{HashMap, HashSet};
use crate::algorithm::Algorithm;
use crate::api::{MarketData, TradeSignal};

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

    /// Derive the minimum decimal places needed to represent a price meaningfully.
    /// Used to cross-check the user-supplied spacing precision.
    fn decimals_from_price(price: f64) -> u32 {
        if price <= 0.0 { return 2; }
        let magnitude = price.log10().floor() as i32;
        match magnitude {
            m if m >= 2  => 2,   // $100+   → 2 dp
            m if m >= 0  => 4,   // $1–$99  → 4 dp
            m if m >= -2 => 6,   // $0.01–$0.99 → 6 dp
            _            => 8,   // sub-cent → 8 dp
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
    }
}

impl Algorithm for GridBot {
    fn name(&self) -> &str {
        "grid"
    }

    fn on_tick(&mut self, tick: &MarketData) -> Vec<TradeSignal> {
        let price = tick.last_price;

        if self.last_price.is_none() {
            // Cross-check spacing-derived precision with the actual price magnitude.
            // e.g. spacing="1" gives price_decimals=0, but a $0.50 asset needs at least 4 dp.
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

            // Emit a limit buy for every level below the current price so they are
            // placed as resting orders on the exchange immediately at startup.
            let prec = self.price_decimals as usize;
            // Collect all levels below current price, nearest first (reversed).
            // Only place the single nearest buy as a live order; all others go into
            // buy_orders so on_tick chains them progressively as price drops.
            // This prevents self-trade conflicts: if the nearest buy fills and seeds
            // a sell at level N+1, there must be no live buy sitting at level N+1.
            let mut buy_levels: Vec<(usize, f64)> = self.levels.iter()
                .copied()
                .enumerate()
                .filter(|&(_, level)| level < price)
                .collect();
            buy_levels.reverse(); // nearest first

            if let Some(&(i, level)) = buy_levels.first() {
                signals.push(TradeSignal::Buy {
                    price: level,
                    quantity: self.quantity,
                    reason: format!(
                        "Grid initial buy at {:.prec$} (level {}/{})",
                        level, i, self.num_levels, prec = prec
                    ),
                    price_decimals: self.price_decimals,
                });
            }
            // Park the rest in buy_orders — they will be placed when price dips to them.
            for &(i, _) in buy_levels.iter().skip(1) {
                self.buy_orders.insert(i);
            }

            // Emit limit sells for any pre-held base inventory supplied via initial_base.
            // Without initial_base no sell orders are placed and the grid buys first.
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
                if level > price && seeded < max_sells {
                    self.seeded_sells.insert(i);
                    signals.push(TradeSignal::Sell {
                        price: level,
                        quantity: self.quantity,
                        reason: format!(
                            "Grid initial sell at {:.prec$} (level {}/{})",
                            level, i, self.num_levels, prec = prec
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
                    signals.iter().filter(|s| matches!(s, TradeSignal::Buy { .. })).count(),
                    signals.iter().filter(|s| matches!(s, TradeSignal::Sell { .. })).count(),
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
                // Position update and opposite-side seeding are deferred to on_fill,
                // which is called once the exchange confirms the order executed.
                signals.push(TradeSignal::Sell {
                    price: sell_price,
                    quantity: self.quantity,
                    reason: format!(
                        "Grid sell at {:.prec$} (level {}/{})",
                        sell_price, idx, self.num_levels, prec = self.price_decimals as usize
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
                // Position update and opposite-side seeding are deferred to on_fill.
                signals.push(TradeSignal::Buy {
                    price: buy_price,
                    quantity: self.quantity,
                    reason: format!(
                        "Grid buy at {:.prec$} (level {}/{})",
                        buy_price, idx, self.num_levels, prec = self.price_decimals as usize
                    ),
                    price_decimals: self.price_decimals,
                });
            }
        }

        self.last_price = Some(price);
        signals
    }

    fn on_fill(&mut self, price: f64, is_buy: bool) {
        // Locate the grid level nearest to the filled price (within 1 % of spacing).
        let tolerance = self.spacing * 0.1;
        let idx = match self.levels.iter().position(|&l| (l - price).abs() < tolerance) {
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
                        price, self.levels[idx + 1]
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
                        price, self.levels[idx - 1]
                    ),
                );
            }
        }
    }

    fn on_reconnect(&mut self) {
        self.last_price = None;
        self.lower = 0.0;
        self.upper = 0.0;
        self.levels.clear();
        self.buy_orders.clear();
        self.sell_orders.clear();
        self.seeded_sells.clear();
        crate::logger::log("[GRID]", "Reconnected — grid reset. Will re-centre on next tick.");
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
