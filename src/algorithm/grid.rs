use crate::algorithm::Algorithm;
use crate::api::{MarketData, TradeSignal};
use std::collections::HashMap;

pub struct GridBot {
    levels_per_side: u32,
    qty: f64,
    spacing: f64,
    price_decimals: u32,
    spread_ratio: f64,

    buy_orders: HashMap<u64, f64>,
    sell_orders: HashMap<u64, f64>,

    base_balance: f64,
    quote_balance: f64,

    last_price: Option<f64>,
    position: f64,
    realized_pnl: f64,
    total_buys: u32,
    total_sells: u32,
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

        let spread_ratio = options
            .get("spread_ratio")
            .map(|v| v.parse::<f64>().unwrap_or(1.1))
            .unwrap_or(1.1);

        let base_balance = options
            .get("initial_base_balance")
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0);

        let quote_balance = options
            .get("initial_quote_balance")
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0);

        Ok(GridBot {
            levels_per_side,
            qty,
            spacing,
            price_decimals,
            spread_ratio,
            buy_orders: HashMap::new(),
            sell_orders: HashMap::new(),
            base_balance,
            quote_balance,
            last_price: None,
            position: 0.0,
            realized_pnl: 0.0,
            total_buys: 0,
            total_sells: 0,
        })
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
        let price_min = Self::decimals_from_price(midpoint);
        if self.price_decimals < price_min {
            self.price_decimals = price_min;
        }

        let m = 10_f64.powi(self.price_decimals as i32);

        let seeded_sells =
            (self.levels_per_side as f64).min((self.base_balance / self.qty).floor()) as u32;
        let seeded_buys = (self.levels_per_side as f64)
            .min((self.quote_balance / (midpoint * self.qty)).floor())
            as u32;

        self.buy_orders.clear();
        self.sell_orders.clear();

        let gap = self.spacing * self.spread_ratio;

        for i in 0..seeded_buys {
            let price = ((midpoint - gap - i as f64 * self.spacing) * m).round() / m;
            if price > 0.0 {
                self.buy_orders.insert(self.price_key(price), price);
            }
        }

        for i in 0..seeded_sells {
            let price = ((midpoint + gap + i as f64 * self.spacing) * m).round() / m;
            self.sell_orders.insert(self.price_key(price), price);
        }

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

        if seeded_buys == 0 {
            crate::logger::log(
                "[GRID]",
                "Insufficient funds to place even one buy order — refusing to start.",
            );
            return vec![];
        }

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

        let lower = self.grid_lower().unwrap_or(0.0);
        let upper = self.grid_upper().unwrap_or(f64::MAX);
        if (!self.buy_orders.is_empty() && price < lower * 0.95)
            || (!self.sell_orders.is_empty() && price > upper * 1.05)
        {
            crate::logger::log(
                "[GRID]",
                &format!(
                    "Price {:.2} is far outside grid [{:.2}–{:.2}] — rebuilding.",
                    price, lower, upper
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

        if price > prev_price {
            let triggered: Vec<(u64, f64)> = self
                .sell_orders
                .iter()
                .filter(|&(_, &v)| v <= price)
                .map(|(&k, &v)| (k, v))
                .collect();
            for (key, sell_price) in triggered {
                self.sell_orders.remove(&key);
                signals.push(TradeSignal::Sell {
                    price: sell_price,
                    quantity: self.qty,
                    reason: format!("Grid sell at {:.prec$}", sell_price, prec = prec),
                    price_decimals: self.price_decimals,
                });
            }
        } else if price < prev_price {
            let triggered: Vec<(u64, f64)> = self
                .buy_orders
                .iter()
                .filter(|&(_, &v)| v >= price)
                .map(|(&k, &v)| (k, v))
                .collect();
            for (key, buy_price) in triggered {
                self.buy_orders.remove(&key);
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

    fn on_fill(&mut self, price: f64, is_buy: bool) -> Vec<TradeSignal> {
        let m = 10_f64.powi(self.price_decimals as i32);
        let prec = self.price_decimals as usize;

        if is_buy {
            self.buy_orders.remove(&self.price_key(price));
            self.position += self.qty;
            self.total_buys += 1;

            let counter = ((price + self.spacing) * m).round() / m;
            self.sell_orders.insert(self.price_key(counter), counter);
            crate::logger::log(
                "[GRID]",
                &format!(
                    "Buy filled @ {:.prec$} — counter sell at {:.prec$}",
                    price,
                    counter,
                    prec = prec
                ),
            );
            vec![TradeSignal::Sell {
                price: counter,
                quantity: self.qty,
                reason: format!("Grid counter sell at {:.prec$}", counter, prec = prec),
                price_decimals: self.price_decimals,
            }]
        } else {
            self.sell_orders.remove(&self.price_key(price));
            self.position -= self.qty;
            self.total_sells += 1;
            self.realized_pnl += self.spacing * self.qty;

            let counter = ((price - self.spacing) * m).round() / m;
            if counter > 0.0 {
                self.buy_orders.insert(self.price_key(counter), counter);
                crate::logger::log(
                    "[GRID]",
                    &format!(
                        "Sell filled @ {:.prec$} — counter buy at {:.prec$}",
                        price,
                        counter,
                        prec = prec
                    ),
                );
                vec![TradeSignal::Buy {
                    price: counter,
                    quantity: self.qty,
                    reason: format!("Grid counter buy at {:.prec$}", counter, prec = prec),
                    price_decimals: self.price_decimals,
                }]
            } else {
                crate::logger::log(
                    "[GRID]",
                    &format!(
                        "Sell filled @ {:.prec$} — counter buy skipped (price would be <= 0)",
                        price,
                        prec = prec
                    ),
                );
                vec![]
            }
        }
    }

    fn on_balance_update(&mut self, base: f64, quote: f64) {
        self.base_balance = base;
        self.quote_balance = quote;
    }

    fn on_spacing_update(&mut self, new_spacing: f64) {
        self.spacing = new_spacing;
        crate::logger::log("[GRID]", &format!("Spacing updated: {:.8}", new_spacing));
    }

    fn on_reconnect(&mut self) {
        self.last_price = None;
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

    fn summary(&self) -> Option<String> {
        let lower = self.grid_lower().unwrap_or(0.0);
        let upper = self.grid_upper().unwrap_or(0.0);
        Some(format!(
            "GridBot\n  Levels/side:  {}\n  Range:        {:.prec$} – {:.prec$}\n  Spacing:      {:.prec$}  |  Spread ratio: {:.2}\n  Trades:       {} buys, {} sells\n  Orders:       {} buy open, {} sell open\n  Position:     {:.8} (net qty)\n  Realized PnL: {:.8}",
            self.levels_per_side,
            lower, upper,
            self.spacing, self.spread_ratio,
            self.total_buys, self.total_sells,
            self.buy_orders.len(), self.sell_orders.len(),
            self.position,
            self.realized_pnl,
            prec = self.price_decimals as usize,
        ))
    }
}
