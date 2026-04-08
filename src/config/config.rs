use serde::Deserialize;

fn default_throttle_ms() -> u64 {
    300
}
fn default_log_retention() -> u32 {
    7
}
fn default_atr_refresh_mins() -> u64 {
    60
}

#[derive(Debug, Deserialize, Clone)]
pub struct ApiConfig {
    pub auth_endpoint: String,
    pub pub_endpoint: String,
    pub ws_endpoint: String,
    pub auth_ws_endpoint: String,
    pub key: String,
    pub secret: String
}

#[derive(Debug, Deserialize, Clone)]
pub struct StartupDefaults {
    #[serde(default = "default_throttle_ms")]
    pub throttle_ms: u64,
    #[serde(default = "default_log_retention")]
    pub log_retention: u32,
    #[serde(default = "default_atr_refresh_mins")]
    pub atr_refresh_mins: u64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub api: ApiConfig,
    pub startup_defaults: StartupDefaults,
}

impl Config {
    pub fn active_key(&self) -> &str {
        &self.api.key
    }

    pub fn active_secret(&self) -> &str {
        &self.api.secret
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
                 \x20 \"api\": {{ \"auth_endpoint\": \"...\", \"pub_endpoint\": \"...\", \"ws_endpoint\": \"...\", \"auth_ws_endpoint\": \"...\", \"key\": \"...\", \"secret\": \"...\" }},\n\
                 \x20 \"startup_defaults\": {{ \"throttle_ms\": 300, \"log_retention\": 7, \"atr_refresh_mins\": 60 }}\n\
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
        Ok(())
    }
}
