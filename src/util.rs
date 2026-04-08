pub fn extract_currencies(symbol: &str) -> (String, String) {
    if let Some(pos) = symbol.find(':') {
        return (symbol[..pos].to_string(), symbol[pos + 1..].to_string());
    }
    if symbol.len() == 6 {
        return (symbol[..3].to_string(), symbol[3..].to_string());
    }
    const KNOWN_QUOTES: &[&str] = &["USD", "UST", "EUR", "BTC", "ETH", "EOS", "XCH"];
    for q in KNOWN_QUOTES {
        if symbol.len() > q.len() && symbol.ends_with(q) {
            let base = &symbol[..symbol.len() - q.len()];
            return (base.to_string(), q.to_string());
        }
    }
    crate::logger::log(
        "[RUNNER]",
        &format!(
            "Warning: could not parse currencies from symbol '{}'.",
            symbol
        ),
    );
    (String::new(), String::new())
}
