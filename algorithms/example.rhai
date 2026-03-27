// algorithms/example.rhai
//
// Reference SMA strategy. Option keys:
//   threshold=<float>   price deviation from SMA to trigger  (default 0.01 = 1%)
//   period=<int>        number of ticks to average            (default 20)
//   qty=<float>         order quantity                        (default 0.001)

let threshold = if "threshold" in options {
    options["threshold"].parse_float()
} else {
    0.01
};
let period = if "period"    in options {
    options["period"].parse_int()
} else {
    20
};
let qty = if "qty"       in options {
    options["qty"].parse_float()
} else {
    0.001
};
let prices = [];

fn on_tick(price, bid, ask, volume) {
    prices.push(price);
    if prices.len() > period {
        prices.remove(0);
    }
    if prices.len() < period {
        return ();
    }

    let avg = prices.reduce(|a, b| a + b, 0.0) / prices.len();

    if price < avg * (1.0 - threshold) {
        return # {
            kind: "buy",
            price: price,
            qty: qty,
            reason: `price ${price} is below SMA ${avg}`
        };
    }
    if price > avg * (1.0 + threshold) {
        return # {
            kind: "sell",
            price: price,
            qty: qty,
            reason: `price ${price} is above SMA ${avg}`
        };
    }
    ()
}

fn summary() {
    let avg = if prices.len() > 0 {
        prices.reduce(|a, b| a + b, 0.0) / prices.len()
    } else {
        0.0
    };
    `Example SMA: period=${period} threshold=${threshold} current_avg=${avg}`
}
