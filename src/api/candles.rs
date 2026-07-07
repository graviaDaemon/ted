use crate::config::config::Config;

#[allow(dead_code)]
#[derive(Clone)]
pub struct Candle {
    pub timestamp: i64,
    pub open: f64,
    pub close: f64,
    pub high: f64,
    pub low: f64,
    pub volume: f64,
}

pub async fn fetch_candles(
    symbol: &str,
    timeframe: &str,
    period: usize,
    config: &Config,
    client: &reqwest::Client,
) -> Result<Vec<Candle>, Box<dyn std::error::Error + Send + Sync>> {
    let base = config.api.pub_endpoint
        .trim_end_matches('/')
        .trim_end_matches("v2")
        .trim_end_matches('/');
    let url = format!(
        "{}/v2/candles/trade:{}:t{}/hist?limit={}&sort=-1",
        base,
        timeframe,
        symbol,
        period + 1,
    );

    let response = client.get(&url).send().await?;
    let http_status = response.status();
    let text = response.text().await?;

    if !http_status.is_success() {
        return Err(format!("HTTP {} fetching candles for {}: {}", http_status, symbol, text).into());
    }

    parse_candles(&text).map_err(|e| e.into())
}

/// Parse a Bitfinex candles `hist` response (array of `[mts, open, close, high,
/// low, volume]` rows) into `Candle`s, preserving the response order.
fn parse_candles(text: &str) -> Result<Vec<Candle>, String> {
    let val: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| format!("Failed to parse candles response: {} — raw: {}", e, text))?;

    let arr = val.as_array()
        .ok_or_else(|| format!("Expected array in candles response, got: {}", text))?;

    let mut candles = Vec::with_capacity(arr.len());
    for item in arr {
        let row = item.as_array()
            .ok_or_else(|| format!("Expected candle row to be array: {}", item))?;
        if row.len() < 6 {
            return Err(format!("Candle row has fewer than 6 fields: {}", item));
        }
        candles.push(Candle {
            timestamp: row[0].as_i64().unwrap_or(0),
            open:      row[1].as_f64().unwrap_or(0.0),
            close:     row[2].as_f64().unwrap_or(0.0),
            high:      row[3].as_f64().unwrap_or(0.0),
            low:       row[4].as_f64().unwrap_or(0.0),
            volume:    row[5].as_f64().unwrap_or(0.0),
        });
    }

    Ok(candles)
}

/// Fetch up to `limit` historical candles for `symbol`, paginating backwards in
/// time when `limit` exceeds Bitfinex's per-request cap (~10000). Returns candles
/// sorted oldest-first (ascending timestamp), deduplicated — the order the
/// backtester replays in.
pub async fn fetch_candle_history(
    symbol: &str,
    timeframe: &str,
    limit: usize,
    config: &Config,
    client: &reqwest::Client,
) -> Result<Vec<Candle>, Box<dyn std::error::Error + Send + Sync>> {
    const PER_REQ: usize = 10_000;

    let base = config.api.pub_endpoint
        .trim_end_matches('/')
        .trim_end_matches("v2")
        .trim_end_matches('/');

    let mut collected: Vec<Candle> = Vec::new();
    let mut end: Option<i64> = None;

    while collected.len() < limit {
        let want = (limit - collected.len()).min(PER_REQ);
        let mut url = format!(
            "{}/v2/candles/trade:{}:t{}/hist?limit={}&sort=-1",
            base, timeframe, symbol, want,
        );
        if let Some(end_ms) = end {
            url.push_str(&format!("&end={}", end_ms));
        }

        let response = client.get(&url).send().await?;
        let http_status = response.status();
        let text = response.text().await?;
        if !http_status.is_success() {
            return Err(format!("HTTP {} fetching candles for {}: {}", http_status, symbol, text).into());
        }

        // Newest-first within the batch.
        let batch = parse_candles(&text)?;
        if batch.is_empty() {
            break;
        }
        let oldest = batch.iter().map(|c| c.timestamp).min().unwrap_or(0);
        let batch_len = batch.len();
        collected.extend(batch);

        if batch_len < want {
            break; // exhausted available history
        }
        end = Some(oldest - 1);
    }

    collected.sort_by_key(|c| c.timestamp);
    collected.dedup_by_key(|c| c.timestamp);
    Ok(collected)
}
