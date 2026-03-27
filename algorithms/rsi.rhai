// algorithms/rsi.rhai
//
// RSI (Relative Strength Index) strategy.
// Options:
//   period=<int>       lookback period        (default 14)
//   oversold=<float>   buy below this level   (default 30.0)
//   overbought=<float> sell above this level  (default 70.0)
//   qty=<float>        order quantity         (default 0.001)

let period = if "period"     in options {
    options["period"].parse_int()
} else {
    14
};
let oversold = if "oversold"   in options {
    options["oversold"].parse_float()
} else {
    30.0
};
let overbought = if "overbought" in options {
    options["overbought"].parse_float()
} else {
    70.0
};
let qty = if "qty"        in options {
    options["qty"].parse_float()
} else {
    0.001
};
let prices = [];
let last_rsi = 50.0;

fn calc_rsi(prices, period) {
    // needs period+1 prices to produce `period` diffs
    if prices.len() < period + 1 {
        return 50.0;
    }

    let gains = 0.0;
    let losses = 0.0;
    let start = prices.len() - period;
    let i = start;

    while i < prices.len() {
        let diff = prices[i] - prices[i - 1];
        if diff > 0.0 {
            gains  += diff;
        }
        else {
            losses -= diff;
        }
        i += 1;
    }

    let avg_gain = gains / period;
    let avg_loss = losses / period;
    if avg_loss == 0.0 {
        return 100.0;
    }

    let rs = avg_gain / avg_loss;
    100.0 - (100.0 / (1.0 + rs))
}

fn on_tick(price, bid, ask, volume) {
    prices.push(price);
    // keep period+2 so the oldest diff is always available
    if prices.len() > period + 2 {
        prices.remove(0);
    }
    if prices.len() < period + 1 {
        return ();
    }

    let r = calc_rsi(prices, period);
    last_rsi = r;

    if r < oversold {
        return # {
            kind: "buy",
            price: price,
            qty: qty,
            reason: `RSI oversold: ${r}`
        };
    }
    if r > overbought {
        return # {
            kind: "sell",
            price: price,
            qty: qty,
            reason: `RSI overbought: ${r}`
        };
    }
    ()
}

fn summary() {
    `RSI(${period}): current=${last_rsi}  buy<${oversold}  sell>${overbought}`
}