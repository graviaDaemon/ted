use serde::Deserialize;

fn default_throttle_ms() -> u64 { 300 }

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct Config {
    pub auth_endpoint: String,
    pub pub_endpoint: String,
    pub ws_endpoint: String,
    pub auth_ws_endpoint: String,
    pub key: String,
    pub secret: String,
    #[serde(default = "default_throttle_ms")]
    pub throttle_ms: u64,
}

impl Config {
    pub fn load_config(path: &str) -> Result<Config, Box<dyn std::error::Error>> {
        let file = std::fs::File::open(path)?;
        let config: Config = serde_json::from_reader(file)?;
        Ok(config)
    }
}