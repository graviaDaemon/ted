/// Average-cost position accounting shared by the live grid (`GridBot`) and the
/// offline backtester, so realized PnL and fees are computed identically in both
/// paths and cannot drift apart. Fees are folded into the cost basis on buys and
/// charged against proceeds on sells, matching the live grid's original inline
/// accounting exactly.
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

    /// Average cost of the open long position (0 when flat or short).
    pub fn avg_cost(&self) -> f64 {
        if self.position > 0.0 {
            self.position_cost / self.position
        } else {
            0.0
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
            -fee
        };
        self.realized_pnl += realized;
        realized
    }
}
