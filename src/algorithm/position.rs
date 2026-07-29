/// One open buy lot with its own resting exit level (plan/09). `entry_cost`
/// folds the entry fee in, so `entry_cost / qty` is the true per-unit breakeven
/// basis before the sell-side fee.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Lot {
    pub qty: f64,
    pub entry_price: f64,
    pub entry_cost: f64,
    pub exit_price: f64,
}

/// Per-lot position accounting for the grid (plan/09): every buy fill becomes a
/// lot carrying its own exit level, and sells realize PnL against the lot(s)
/// resting at the filled level rather than a blended average cost — so a fill
/// can never silently book inventory out below what it cost.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct LotBook {
    pub lots: Vec<Lot>,
    pub realized_pnl: f64,
    pub fees_paid: f64,
    maker_fee: f64,
}

const LOT_QTY_EPS: f64 = 1e-9;
const LOT_PRICE_EPS: f64 = 1e-6;

impl LotBook {
    pub fn new(maker_fee: f64) -> Self {
        LotBook {
            lots: Vec::new(),
            realized_pnl: 0.0,
            fees_paid: 0.0,
            maker_fee,
        }
    }

    /// Seed pre-existing base inventory as an ordinary lot (no fee, no realized
    /// PnL — opening inventory, not a fill). The caller supplies the exit level.
    pub fn seed(&mut self, qty: f64, price: f64, exit_price: f64) {
        if qty <= 0.0 || price <= 0.0 {
            return;
        }
        self.lots.push(Lot {
            qty,
            entry_price: price,
            entry_cost: price * qty,
            exit_price,
        });
    }

    /// Record a buy fill as a new lot. The fee is folded into the cost basis.
    pub fn record_buy(&mut self, price: f64, qty: f64, exit_price: f64) -> &Lot {
        let fee = price * qty * self.maker_fee;
        self.fees_paid += fee;
        self.lots.push(Lot {
            qty,
            entry_price: price,
            entry_cost: price * qty + fee,
            exit_price,
        });
        self.lots.last().unwrap()
    }

    /// Record a sell fill of `qty` at `price`, matching the lot(s) whose
    /// `exit_price` equals the fill level (FIFO among equal levels). Returns the
    /// realized PnL of this fill, net of both legs' fees. An out-of-band fill
    /// (no lot resting at that level) falls back to global FIFO with a warning —
    /// never silently dropped.
    pub fn record_sell(&mut self, price: f64, qty: f64) -> f64 {
        let fee = price * qty * self.maker_fee;
        self.fees_paid += fee;
        let mut remaining = qty;
        let mut realized = self.consume(price, &mut remaining, true);
        if remaining > LOT_QTY_EPS && !self.lots.is_empty() {
            crate::logger::log_warn(
                "[POSITION]",
                &format!(
                    "Sell {:.8} @ {:.8} has no lot exit at that level — falling back to FIFO across {} open lot(s).",
                    qty, price, self.lots.len()
                ),
            );
            realized += self.consume(price, &mut remaining, false);
        }
        if remaining > LOT_QTY_EPS {
            crate::logger::log_warn(
                "[POSITION]",
                &format!(
                    "Sell of {:.8} @ {:.8} exceeds open lots by {:.8} — un-backed remainder not booked; spot grid should prevent this.",
                    qty, price, remaining
                ),
            );
        }
        self.realized_pnl += realized;
        realized
    }

    fn consume(&mut self, price: f64, remaining: &mut f64, match_exit: bool) -> f64 {
        let mut realized = 0.0;
        let mut i = 0;
        while i < self.lots.len() && *remaining > LOT_QTY_EPS {
            if match_exit && (self.lots[i].exit_price - price).abs() >= LOT_PRICE_EPS {
                i += 1;
                continue;
            }
            let lot = &mut self.lots[i];
            let take = remaining.min(lot.qty);
            let unit_cost = lot.entry_cost / lot.qty;
            realized += (price * (1.0 - self.maker_fee) - unit_cost) * take;
            lot.qty -= take;
            lot.entry_cost -= unit_cost * take;
            *remaining -= take;
            if lot.qty <= LOT_QTY_EPS {
                self.lots.remove(i);
            } else {
                i += 1;
            }
        }
        realized
    }

    /// Net base held across all open lots.
    pub fn position(&self) -> f64 {
        self.lots.iter().map(|l| l.qty).sum()
    }

    /// Blended per-unit cost of the open lots (0 when flat) — the derived view
    /// trait reporting uses; per-lot exits never price off this.
    pub fn avg_cost(&self) -> f64 {
        let pos = self.position();
        if pos > 0.0 {
            self.notional() / pos
        } else {
            0.0
        }
    }

    /// Total quote committed to open lots (fee-inclusive cost basis).
    pub fn notional(&self) -> f64 {
        self.lots.iter().map(|l| l.entry_cost).sum()
    }
}

/// Average-cost position accounting shared by the live grid (`GridBot`) and the
/// offline backtester, so realized PnL and fees are computed identically in both
/// paths and cannot drift apart. Fees are folded into the cost basis on buys and
/// charged against proceeds on sells, matching the live grid's original inline
/// accounting exactly.
/// Since plan/09 the grid itself runs on `LotBook`; `AvgCostBook` remains for
/// the backtester's `Sim` internal wallet accounting.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct AvgCostBook {
    pub position: f64,
    pub position_cost: f64,
    pub realized_pnl: f64,
    pub fees_paid: f64,
    maker_fee: f64,
}

impl AvgCostBook {
    pub fn new(maker_fee: f64) -> Self {
        AvgCostBook {
            position: 0.0,
            position_cost: 0.0,
            realized_pnl: 0.0,
            fees_paid: 0.0,
            maker_fee,
        }
    }

    /// Record a buy fill of `qty` at `price`. The fee is added to the cost basis.
    pub fn record_buy(&mut self, price: f64, qty: f64) {
        self.position += qty;
        let fee = price * qty * self.maker_fee;
        self.position_cost += price * qty + fee;
        self.fees_paid += fee;
    }

    /// Record a sell fill of `qty` at `price`. Returns the realized PnL of this
    /// fill alone (positive = a winning round-trip leg), letting callers compute
    /// win rate without re-deriving the accounting.
    pub fn record_sell(&mut self, price: f64, qty: f64) -> f64 {
        self.position -= qty;
        let fee = price * qty * self.maker_fee;
        self.fees_paid += fee;
        let realized = if self.position + qty > 0.0 {
            let avg_cost = self.position_cost / (self.position + qty);
            let r = (price * (1.0 - self.maker_fee) - avg_cost) * qty;
            self.position_cost -= avg_cost * qty;
            if self.position_cost < 0.0 {
                self.position_cost = 0.0;
            }
            r
        } else {
            // Selling with no inventory books a zero-basis short. The spot grid
            // caps emitted sells at held base, so this should be unreachable in
            // normal operation — warn if reality diverges (e.g. an out-of-band
            // fill) so it surfaces instead of silently corrupting the position.
            crate::logger::log_warn(
                "[POSITION]",
                &format!(
                    "record_sell of {:.8} @ {:.8} with no inventory (position would go to {:.8}) — booking zero-basis short; spot grid should prevent this.",
                    qty, price, self.position
                ),
            );
            -fee
        };
        self.realized_pnl += realized;
        realized
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lot_round_trip_zero_fee() {
        let mut b = LotBook::new(0.0);
        b.record_buy(100.0, 1.0, 110.0);
        assert!((b.position() - 1.0).abs() < 1e-12);
        assert!((b.notional() - 100.0).abs() < 1e-12);
        let r = b.record_sell(110.0, 1.0);
        assert!((r - 10.0).abs() < 1e-12, "realized {}", r);
        assert!((b.realized_pnl - 10.0).abs() < 1e-12);
        assert!(b.lots.is_empty());
    }

    #[test]
    fn lot_round_trip_with_fee_both_legs() {
        let mut b = LotBook::new(0.001);
        b.record_buy(100.0, 1.0, 110.0);
        let r = b.record_sell(110.0, 1.0);
        let expected = 10.0 - 100.0 * 0.001 - 110.0 * 0.001;
        assert!((r - expected).abs() < 1e-12, "realized {} expected {}", r, expected);
        assert!((b.fees_paid - (100.0 * 0.001 + 110.0 * 0.001)).abs() < 1e-12);
    }

    #[test]
    fn sell_matches_lot_at_exit_level_not_fifo() {
        let mut b = LotBook::new(0.0);
        b.record_buy(100.0, 1.0, 110.0); // older, higher lot
        b.record_buy(80.0, 1.0, 90.0); // newer, cheaper lot
        // The 90 exit fills: must close the 80 lot, not the FIFO-first 100 lot.
        let r = b.record_sell(90.0, 1.0);
        assert!((r - 10.0).abs() < 1e-12, "realized {}", r);
        assert_eq!(b.lots.len(), 1);
        assert!((b.lots[0].entry_price - 100.0).abs() < 1e-12);
    }

    #[test]
    fn shared_exit_level_consumes_fifo_among_equals() {
        let mut b = LotBook::new(0.0);
        b.record_buy(100.0, 1.0, 105.0);
        b.record_buy(101.0, 1.0, 105.0);
        // Partial fill of the shared level: FIFO among the equal-keyed lots.
        let r = b.record_sell(105.0, 1.0);
        assert!((r - 5.0).abs() < 1e-12, "oldest lot (entry 100) closes first, got {}", r);
        assert_eq!(b.lots.len(), 1);
        assert!((b.lots[0].entry_price - 101.0).abs() < 1e-12);
        let r2 = b.record_sell(105.0, 1.0);
        assert!((r2 - 4.0).abs() < 1e-12);
        assert!(b.lots.is_empty());
    }

    #[test]
    fn out_of_band_sell_falls_back_to_global_fifo() {
        let mut b = LotBook::new(0.0);
        b.record_buy(100.0, 1.0, 110.0);
        // Fill at a level no lot rests at → global FIFO fallback, still booked.
        let r = b.record_sell(120.0, 1.0);
        assert!((r - 20.0).abs() < 1e-12, "realized {}", r);
        assert!(b.lots.is_empty());
        assert!((b.realized_pnl - 20.0).abs() < 1e-12);
    }

    #[test]
    fn unbacked_sell_books_nothing_beyond_lots() {
        let mut b = LotBook::new(0.0);
        b.record_buy(100.0, 1.0, 110.0);
        let r = b.record_sell(110.0, 2.0);
        assert!((r - 10.0).abs() < 1e-12, "only the backed unit realizes, got {}", r);
        assert!((b.position() - 0.0).abs() < 1e-12, "position never goes negative");
    }

    #[test]
    fn seed_creates_feeless_lot() {
        let mut b = LotBook::new(0.001);
        b.seed(2.0, 50.0, 60.0);
        assert!((b.position() - 2.0).abs() < 1e-12);
        assert!((b.notional() - 100.0).abs() < 1e-12, "no fee on seeded inventory");
        assert!((b.fees_paid - 0.0).abs() < 1e-12);
        assert!((b.avg_cost() - 50.0).abs() < 1e-12);
    }

    #[test]
    fn lot_book_serde_round_trips() {
        let mut b = LotBook::new(0.001);
        b.seed(1.0, 90.0, 100.0);
        b.record_buy(100.0, 1.5, 110.0);
        b.record_sell(110.0, 1.5);
        let json = serde_json::to_string(&b).unwrap();
        let r: LotBook = serde_json::from_str(&json).unwrap();
        assert_eq!(r.lots.len(), b.lots.len());
        assert!((r.position() - b.position()).abs() < 1e-12);
        assert!((r.realized_pnl - b.realized_pnl).abs() < 1e-12);
        assert!((r.fees_paid - b.fees_paid).abs() < 1e-12);
        assert!((r.maker_fee - b.maker_fee).abs() < 1e-12);
    }
}
