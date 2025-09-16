use figment::{Figment, providers::{Format, Toml, Serialized}};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Config {
    pub blink_api_url: String,
    pub blink_api_key: String,
    pub blink_wallet_id: String,
    pub server_port: u16,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            blink_api_url: "https://api.blink.sv/graphql".to_string(),
            blink_api_key: "".to_string(),
            blink_wallet_id: "".to_string(),
            server_port: 50051,
        }
    }
}

impl Config {
    /// Load from config.toml (if present) and environment variables.
    /// Environment variables override file values.
    /// Supported env keys: BLINK_API_URL, BLINK_API_KEY, BLINK_WALLET_ID, SERVER_PORT
    pub fn load() -> Self {
        // 1) Start with defaults + config.toml (NOT nested)
        let base: Config = Default::default();
        let mut cfg: Config = Figment::from(Serialized::defaults(base))
            .merge(Toml::file("config.toml"))
            .extract()
            .unwrap_or_default();

        // 2) Overlay environment variables explicitly
        if let Ok(v) = std::env::var("BLINK_API_URL")      { cfg.blink_api_url = v; }
        if let Ok(v) = std::env::var("BLINK_API_KEY")      { cfg.blink_api_key = v; }
        if let Ok(v) = std::env::var("BLINK_WALLET_ID")    { cfg.blink_wallet_id = v; }
        if let Ok(v) = std::env::var("SERVER_PORT")        { cfg.server_port = v.parse().unwrap_or(cfg.server_port); }

        cfg
    }

    pub fn from_env() -> Self { Self::load() }
}
