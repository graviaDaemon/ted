// algorithms/bbands.rhai
//
// Bollinger Bands mean-reversion strategy.
// Options:
//   period=<int>    rolling window length         (default 20)
//   mult=<float>    band width in std deviations  (default 2.0)
//   qty=<float>     order quantity                (default 0.001)
let period = if "period" in options {
    options["period"].parse_int()
} else {
    20
};
let mult = if "mult"   in options {
    options["mult"].parse_float()
} else {
    2.0
};
let qty = if "qty"    in options {
    options["qty"].parse_float()
} else {
    0.001
};
let prices = [];

fn std_dev(prices, mean) {
    let variance = prices.reduce(|acc, p| acc + (p - mean) * (p - mean), 0.0) / prices.len();
    sqrt(variance)
}

fn on_tick(price, bid, ask, volume) {
    prices.push(price);
    if prices.len() > period {
        prices.remove(0);
    }
    if prices.len() < period {
        return ();
    }

    let mean = prices.reduce(|a, b| a + b, 0.0) / prices.len();
    let sd = std_dev(prices, mean);
    let upper = mean + mult * sd;
    let lower = mean - mult * sd;

    if price <= lower {
        return #{
            kind: "buy",
            price: price,
            qty: qty,
            reason: `BB buy: price ${price} <= lower band ${lower}`
        };
    }
    if price >= upper {
        return #{
            kind: "sell",
            price: price,
            qty: qty,
            reason: `BB sell: price ${price} >= upper band ${upper}`
        };
    }
    ()
}

fn summary() {
    if prices.len() < period {
        return `BB(${period} x${mult}): warming up (${prices.len()}/${period})`;
    }
    let mean = prices.reduce(|a, b| a + b, 0.0) / prices.len();
    let sd = std_dev(prices, mean);
    `BB(${period} x${mult}): mean=${mean}  lower=${mean - mult * sd}  upper=${mean + mult * sd}  sd=${sd}`
}