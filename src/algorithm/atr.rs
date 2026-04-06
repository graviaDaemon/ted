use crate::api::candles::Candle;

pub fn compute_atr(candles: &[Candle], period: usize) -> Result<f64, String> {
    if candles.len() < period + 1 {
        return Err(format!(
            "Not enough candles to compute ATR: need {}, got {}",
            period + 1,
            candles.len()
        ));
    }

    // Bitfinex returns candles newest-first; reverse so index 0 is oldest
    let candles: Vec<&Candle> = candles.iter().rev().collect();

    let tr_values: Vec<f64> = candles
        .windows(2)
        .map(|w| {
            let prev = w[0];
            let curr = w[1];
            let hl = curr.high - curr.low;
            let hpc = (curr.high - prev.close).abs();
            let lpc = (curr.low - prev.close).abs();
            hl.max(hpc).max(lpc)
        })
        .collect();

    let atr = tr_values.iter().take(period).sum::<f64>() / period as f64;
    Ok(atr)
}
