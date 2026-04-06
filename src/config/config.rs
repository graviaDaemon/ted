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
}

#[derive(Debug, Deserialize, Clone)]
pub struct CredentialsConfig {
    pub live_key: String,
    pub live_secret: String,
    #[serde(default)]
    pub paper_key: String,
    #[serde(default)]
    pub paper_secret: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct StartupDefaults {
    #[serde(default)]
    pub paper: bool,
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
    pub credentials: CredentialsConfig,
    pub startup_defaults: StartupDefaults,
}

impl Config {
    pub fn effective_key(&self, paper: bool) -> &str {
        if paper {
            &self.credentials.paper_key
        } else {
            &self.credentials.live_key
        }
    }

    pub fn effective_secret(&self, paper: bool) -> &str {
        if paper {
            &self.credentials.paper_secret
        } else {
            &self.credentials.live_secret
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
                 \x20 \"api\": {{ \"auth_endpoint\": \"...\", \"pub_endpoint\": \"...\", \"ws_endpoint\": \"...\", \"auth_ws_endpoint\": \"...\" }},\n\
                 \x20 \"credentials\": {{ \"live_key\": \"...\", \"live_secret\": \"...\", \"paper_key\": \"\", \"paper_secret\": \"\" }},\n\
                 \x20 \"startup_defaults\": {{ \"paper\": false, \"throttle_ms\": 300, \"log_retention\": 7, \"atr_refresh_mins\": 60 }}\n\
                 }}\n\
                 \n\
                 See config.template.json for the full template.",
                path, e
            )
            .into()
        })
    }
}
