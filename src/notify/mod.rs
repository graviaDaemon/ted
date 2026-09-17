//! Email escalation + optional local-model narrative (plan/12). Used by the
//! headless circuit breaker and the monthly report. Never in the trade path.

use crate::config::config::{EmailConfig, OllamaConfig};

/// Send a plain-text email to every configured recipient. A no-op (with a log
/// line) when email is unconfigured, so the breaker still halts without it.
pub async fn send_email(cfg: Option<&EmailConfig>, subject: &str, body: &str) -> Result<(), String> {
    use lettre::message::header::ContentType;
    use lettre::transport::smtp::authentication::Credentials;
    use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

    let Some(cfg) = cfg else {
        crate::logger::log_warn("[NOTIFY]", "Email not configured — skipping send.");
        return Ok(());
    };
    if cfg.to.is_empty() {
        return Err("email.to is empty".into());
    }

    let from = cfg.from.parse().map_err(|e| format!("bad email.from '{}': {}", cfg.from, e))?;
    let mut builder = Message::builder().from(from).subject(subject);
    for rcpt in &cfg.to {
        let mbox = rcpt.parse().map_err(|e| format!("bad recipient '{}': {}", rcpt, e))?;
        builder = builder.to(mbox);
    }
    let email = builder
        .header(ContentType::TEXT_PLAIN)
        .body(body.to_string())
        .map_err(|e| format!("build email: {}", e))?;

    let creds = Credentials::new(cfg.user.clone(), cfg.pass.clone());
    let builder = if cfg.starttls {
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&cfg.smtp_host)
    } else {
        AsyncSmtpTransport::<Tokio1Executor>::relay(&cfg.smtp_host)
    }
    .map_err(|e| format!("smtp relay setup: {}", e))?;
    let mailer = builder.port(cfg.smtp_port).credentials(creds).build();

    AsyncTransport::send(&mailer, email)
        .await
        .map_err(|e| format!("smtp send: {}", e))?;
    crate::logger::log("[NOTIFY]", &format!("Sent email '{}' to {} recipient(s).", subject, cfg.to.len()));
    Ok(())
}

/// Ask the local model to write the narrative for a report. Returns `fallback`
/// (a deterministic summary) if Ollama is unconfigured or unreachable — the email
/// always goes out with useful content regardless.
pub async fn narrate(ollama: Option<&OllamaConfig>, prompt: &str, fallback: &str) -> String {
    let Some(cfg) = ollama else {
        return fallback.to_string();
    };
    match try_generate(cfg, prompt).await {
        Ok(text) if !text.trim().is_empty() => text,
        Ok(_) => fallback.to_string(),
        Err(e) => {
            crate::logger::log_warn("[NOTIFY]", &format!("Ollama narrate failed ({}) — using fallback.", e));
            fallback.to_string()
        }
    }
}

async fn try_generate(cfg: &OllamaConfig, prompt: &str) -> Result<String, String> {
    let url = format!("{}/api/generate", cfg.endpoint.trim_end_matches('/'));
    let body = serde_json::json!({ "model": cfg.model, "prompt": prompt, "stream": false });
    let resp = reqwest::Client::new()
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let json: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    Ok(json.get("response").and_then(|v| v.as_str()).unwrap_or("").to_string())
}
