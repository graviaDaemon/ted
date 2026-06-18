use serde::Deserialize;
use std::sync::Once;

fn default_throttle_ms() -> u64 {
    300
}
fn default_log_retention() -> u32 {
    7
}
fn default_atr_refresh_mins() -> u64 {
    60
}
fn default_log_level() -> String {
    "info".to_string()
}
fn default_snapshot_retention_days() -> u32 {
    30
}
fn default_snapshot_interval_secs() -> u64 {
    60
}

/// Process-wide credential selection. The engine is spawned once with a single
/// `Config`, so credential routing is process-wide (not per-runner) for this
/// changeset — see plan/01. `Live` reads `api.key/secret`; `Paper` reads
/// `api.paper_key/paper_secret`, falling back to the live pair with a one-time
/// warning when paper credentials are absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CredentialMode {
    #[default]
    Live,
    Paper,
}

static PAPER_KEY_WARN: Once = Once::new();
static PAPER_SECRET_WARN: Once = Once::new();

#[derive(Debug, Deserialize, Clone)]
pub struct ApiConfig {
    pub auth_endpoint: String,
    pub pub_endpoint: String,
    pub ws_endpoint: String,
    pub auth_ws_endpoint: String,
    pub key: String,
    pub secret: String,
    #[serde(default)]
    pub paper_key: Option<String>,
    #[serde(default)]
    pub paper_secret: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct StartupDefaults {
    #[serde(default = "default_throttle_ms")]
    pub throttle_ms: u64,
    #[serde(default = "default_log_retention")]
    pub log_retention: u32,
    #[serde(default = "default_atr_refresh_mins")]
    pub atr_refresh_mins: u64,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default = "default_snapshot_retention_days")]
    pub snapshot_retention_days: u32,
    #[serde(default = "default_snapshot_interval_secs")]
    pub snapshot_interval_secs: u64,
    #[serde(default)]
    pub default_maker_fee: f64,
    #[serde(default)]
    pub default_taker_fee: f64,
    #[serde(default)]
    pub paper: bool,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub api: ApiConfig,
    pub startup_defaults: StartupDefaults,
    /// Set at startup from `startup_defaults.paper`; never read from JSON.
    #[serde(skip)]
    pub credential_mode: CredentialMode,
}

impl Config {
    pub fn active_key(&self, mode: CredentialMode) -> &str {
        match mode {
            CredentialMode::Live => &self.api.key,
            CredentialMode::Paper => {
                match self.api.paper_key.as_deref().filter(|k| !k.is_empty()) {
                    Some(k) => k,
                    None => {
                        PAPER_KEY_WARN.call_once(|| {
                            crate::logger::log_warn(
                                "[CONFIG]",
                                "paper_key not set — falling back to live api.key for paper mode.",
                            )
                        });
                        &self.api.key
                    }
                }
            }
        }
    }

    pub fn active_secret(&self, mode: CredentialMode) -> &str {
        match mode {
            CredentialMode::Live => &self.api.secret,
            CredentialMode::Paper => {
                match self.api.paper_secret.as_deref().filter(|s| !s.is_empty()) {
                    Some(s) => s,
                    None => {
                        PAPER_SECRET_WARN.call_once(|| {
                            crate::logger::log_warn(
                                "[CONFIG]",
                                "paper_secret not set — falling back to live api.secret for paper mode.",
                            )
                        });
                        &self.api.secret
                    }
                }
            }
        }
    }

    pub fn load_config(path: &str) -> Result<Config, Box<dyn std::error::Error>> {
        let file =
            std::fs::File::open(path).map_err(|e| format!("Could not open {}: {}", path, e))?;
        serde_json::from_reader(file).map_err(|e| {
            format!(
                "Failed to parse {}: {}\n\
                 \n\
                 Expected nested format:\n\
                 {{\n\
                 \x20 \"api\": {{ \"auth_endpoint\": \"...\", \"pub_endpoint\": \"...\", \"ws_endpoint\": \"...\", \"auth_ws_endpoint\": \"...\", \"key\": \"...\", \"secret\": \"...\", \"paper_key\": \"...\", \"paper_secret\": \"...\" }},\n\
                 \x20 \"startup_defaults\": {{ \"throttle_ms\": 300, \"log_retention\": 7, \"atr_refresh_mins\": 60, \"log_level\": \"info\", \"snapshot_retention_days\": 30, \"snapshot_interval_secs\": 60, \"default_maker_fee\": 0.0, \"default_taker_fee\": 0.0, \"paper\": false }}\n\
                 }}\n\
                 \n\
                 See config.template.json for the full template.",
                path, e
            )
            .into()
        })
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.api.key.is_empty()       { return Err("api.key is empty".into()); }
        if self.api.secret.is_empty()    { return Err("api.secret is empty".into()); }
        if self.api.ws_endpoint.is_empty()            { return Err("api.ws_endpoint is empty".into()); }
        if self.api.auth_ws_endpoint.is_empty()       { return Err("api.auth_ws_endpoint is empty".into()); }
        if self.api.auth_endpoint.is_empty()          { return Err("api.auth_endpoint is empty".into()); }

        // log_level must name a known level. from_str maps unknown → Info, so
        // round-trip the parsed value to detect typos rather than silently
        // defaulting them.
        let lvl = self.startup_defaults.log_level.trim().to_lowercase();
        let known = ["trace", "debug", "info", "warn", "warning", "error", "critical", "crit"];
        if !known.contains(&lvl.as_str()) {
            return Err(format!(
                "startup_defaults.log_level '{}' is not a known level (trace/debug/info/warn/error/critical)",
                self.startup_defaults.log_level
            ));
        }

        // When paper mode is the process-wide default, paper credentials are
        // optional: active_key/active_secret fall back to the live pair with a
        // one-time warning. Nothing to hard-fail here.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEGACY: &str = r#"{
        "api": {
            "auth_endpoint": "https://api.bitfinex.com",
            "pub_endpoint": "https://api-pub.bitfinex.com",
            "ws_endpoint": "wss://api-pub.bitfinex.com/ws/2",
            "auth_ws_endpoint": "wss://api.bitfinex.com/ws/2",
            "key": "live-key",
            "secret": "live-secret"
        },
        "startup_defaults": {
            "throttle_ms": 300,
            "log_retention": 7,
            "atr_refresh_mins": 60
        }
    }"#;

    const NEW_SHAPE: &str = r#"{
        "api": {
            "auth_endpoint": "https://api.bitfinex.com",
            "pub_endpoint": "https://api-pub.bitfinex.com",
            "ws_endpoint": "wss://api-pub.bitfinex.com/ws/2",
            "auth_ws_endpoint": "wss://api.bitfinex.com/ws/2",
            "key": "live-key",
            "secret": "live-secret",
            "paper_key": "paper-key",
            "paper_secret": "paper-secret"
        },
        "startup_defaults": {
            "throttle_ms": 300,
            "log_retention": 7,
            "atr_refresh_mins": 60,
            "log_level": "debug",
            "snapshot_retention_days": 14,
            "snapshot_interval_secs": 30,
            "default_maker_fee": 0.001,
            "default_taker_fee": 0.002,
            "paper": true
        }
    }"#;

    #[test]
    fn parses_legacy_shape_with_defaults() {
        let cfg: Config = serde_json::from_str(LEGACY).expect("legacy config should parse");
        assert_eq!(cfg.api.paper_key, None);
        assert_eq!(cfg.startup_defaults.log_level, "info");
        assert_eq!(cfg.startup_defaults.snapshot_retention_days, 30);
        assert_eq!(cfg.startup_defaults.snapshot_interval_secs, 60);
        assert_eq!(cfg.startup_defaults.default_maker_fee, 0.0);
        assert!(!cfg.startup_defaults.paper);
        assert_eq!(cfg.credential_mode, CredentialMode::Live);
        cfg.validate().expect("legacy config should validate");
    }

    #[test]
    fn parses_new_shape() {
        let cfg: Config = serde_json::from_str(NEW_SHAPE).expect("new config should parse");
        assert_eq!(cfg.api.paper_key.as_deref(), Some("paper-key"));
        assert_eq!(cfg.startup_defaults.log_level, "debug");
        assert_eq!(cfg.startup_defaults.snapshot_retention_days, 14);
        assert_eq!(cfg.startup_defaults.snapshot_interval_secs, 30);
        assert_eq!(cfg.startup_defaults.default_maker_fee, 0.001);
        assert_eq!(cfg.startup_defaults.default_taker_fee, 0.002);
        assert!(cfg.startup_defaults.paper);
        cfg.validate().expect("new config should validate");
        assert_eq!(cfg.active_key(CredentialMode::Paper), "paper-key");
        assert_eq!(cfg.active_key(CredentialMode::Live), "live-key");
    }

    #[test]
    fn rejects_unknown_log_level() {
        let cfg: Config = serde_json::from_str(
            &NEW_SHAPE.replace("\"log_level\": \"debug\"", "\"log_level\": \"loud\""),
        )
        .expect("config should still parse");
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn paper_key_falls_back_to_live_when_absent() {
        let cfg: Config = serde_json::from_str(LEGACY).expect("legacy config should parse");
        assert_eq!(cfg.active_key(CredentialMode::Paper), "live-key");
        assert_eq!(cfg.active_secret(CredentialMode::Paper), "live-secret");
    }
}
