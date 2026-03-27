use serde_json::json;
use chrono::Utc;
use tokio::time::{sleep, Duration};

use crate::api::auth::sign_rest_request;
use crate::api::types::{OrderResult, TradeSignal};
use crate::config::config::Config;

/// Places a EXCHANGE LIMIT order on Bitfinex via the authenticated REST API.
///
/// - Buy signal  → positive amount
/// - Sell signal → negative amount
///
/// Returns `OrderResult` on success, or an error describing the failure.
/// The caller is responsible for only calling this when `dry_run = false`.
pub async fn place_order(
    signal: &TradeSignal,
    symbol: &str,
    config: &Config,
    client: &reqwest::Client,
) -> Result<OrderResult, Box<dyn std::error::Error + Send + Sync>> {
    let (price, quantity, sign, price_decimals) = match signal {
        TradeSignal::Buy  { price, quantity, price_decimals, .. } => (price, quantity,  1.0_f64, *price_decimals),
        TradeSignal::Sell { price, quantity, price_decimals, .. } => (price, quantity, -1.0_f64, *price_decimals),
    };

    let amount = sign * quantity;
    let path   = "/v2/auth/w/order/submit";

    let body = json!({
        "type":   "EXCHANGE LIMIT",
        "symbol": format!("t{}", symbol),
        "price":  format!("{:.prec$}", price, prec = price_decimals as usize),
        "amount": format!("{:.8}", amount),
    })
    .to_string();

    let url = format!("{}{}", config.auth_endpoint.trim_end_matches('/'), path);

    // Retry up to 3 times on HTTP 429 (rate limited).
    // A fresh nonce is generated on every attempt: Bitfinex requires strictly
    // increasing nonces and will reject a reused value with a nonce error.
    const MAX_ATTEMPTS: u32 = 3;
    let mut last_status = reqwest::StatusCode::OK;
    let mut last_text   = String::new();
    'retry: for attempt in 1..=MAX_ATTEMPTS {
        let nonce = Utc::now().timestamp_millis().to_string();
        let sig   = sign_rest_request(&config.secret, path, &nonce, &body);
        let response = client
            .post(&url)
            .header("Content-Type",  "application/json")
            .header("bfx-nonce",     &nonce)
            .header("bfx-apikey",    &config.key)
            .header("bfx-signature", &sig)
            .body(body.clone())
            .send()
            .await?;

        last_status = response.status();
        if last_status == reqwest::StatusCode::TOO_MANY_REQUESTS && attempt < MAX_ATTEMPTS {
            // Respect Retry-After header if present, otherwise back off 1 s.
            let retry_after: u64 = response
                .headers()
                .get("Retry-After")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse().ok())
                .unwrap_or(1);
            crate::logger::log(
                "[API]",
                &format!(
                    "Rate limited (429) on order submit — retrying in {}s (attempt {}/{}).",
                    retry_after, attempt, MAX_ATTEMPTS
                ),
            );
            sleep(Duration::from_secs(retry_after)).await;
            continue 'retry;
        }
        last_text = response.text().await?;
        break 'retry;
    }
    let (http_status, text) = (last_status, last_text);

    if !http_status.is_success() {
        return Err(format!("HTTP {}: {}", http_status, text).into());
    }

    // Bitfinex REST response: [MTS, "on-req", null, null, [[order_id, ...]], null, STATUS, TEXT]
    let val: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| format!("Failed to parse order response: {} — raw: {}", e, text))?;

    let status   = val[6].as_str().unwrap_or("UNKNOWN").to_string();
    let text_msg = val[7].as_str().unwrap_or("").to_string();

    if status != "SUCCESS" {
        return Err(format!("Order rejected ({}): {}", status, text_msg).into());
    }

    let order_id = val[4][0][0].as_i64().unwrap_or(0);

    Ok(OrderResult { order_id, status, text: text_msg })
}

pub async fn cancel_order(
    order_id: i64,
    config: &Config,
    client: &reqwest::Client,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let path  = "/v2/auth/w/order/cancel";
    let nonce = Utc::now().timestamp_millis().to_string();
    let body  = serde_json::json!({ "id": order_id }).to_string();
    let sig   = sign_rest_request(&config.secret, path, &nonce, &body);
    let url   = format!("{}{}", config.auth_endpoint.trim_end_matches('/'), path);

    let response = client
        .post(&url)
        .header("Content-Type",  "application/json")
        .header("bfx-nonce",     &nonce)
        .header("bfx-apikey",    &config.key)
        .header("bfx-signature", &sig)
        .body(body)
        .send()
        .await?;

    let http_status = response.status();
    let text        = response.text().await?;

    if !http_status.is_success() {
        return Err(format!("HTTP {} cancelling order {}: {}", http_status, order_id, text).into());
    }

    let val: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| format!("Failed to parse cancel response: {} — raw: {}", e, text))?;
    let status = val[6].as_str().unwrap_or("UNKNOWN");
    if status != "SUCCESS" {
        crate::logger::log(
            "[API]",
            &format!("Cancel order {} returned status '{}' — may already be filled/cancelled.", order_id, status),
        );
    }
    Ok(())
}
