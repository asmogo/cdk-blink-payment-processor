use anyhow::{bail, Context, Result};
use figment::{
    providers::{Format, Serialized, Toml},
    Figment,
};
use serde::{Deserialize, Serialize};

/// Blink backend configuration
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BackendConfig {
    /// Blink GraphQL API endpoint
    #[serde(default = "default_api_url")]
    pub api_url: String,

    /// Blink API key (required)
    #[serde(default)]
    pub api_key: String,

    /// Wallet ID to use for all operations.
    /// When empty, the account's default BTC wallet is resolved at startup.
    #[serde(default)]
    pub wallet_id: String,
}

impl Default for BackendConfig {
    fn default() -> Self {
        Self {
            api_url: default_api_url(),
            api_key: String::new(),
            wallet_id: String::new(),
        }
    }
}

/// Main configuration structure
///
/// Loads configuration from config.toml and environment variables.
/// Environment variables take precedence over file configuration.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Config {
    /// Blink backend configuration
    #[serde(default)]
    pub backend: BackendConfig,

    /// gRPC server listen address
    #[serde(default = "default_address")]
    pub address: String,

    /// gRPC server listen port
    #[serde(default = "default_port")]
    pub port: u16,

    /// TLS config for the gRPC server
    #[serde(default)]
    pub tls_enable: bool,
    #[serde(default = "default_tls_cert_path")]
    pub tls_cert_path: String,
    #[serde(default = "default_tls_key_path")]
    pub tls_key_path: String,
}

fn default_api_url() -> String {
    "https://api.blink.sv/graphql".to_string()
}

fn default_address() -> String {
    "0.0.0.0".to_string()
}

fn default_port() -> u16 {
    50051
}

fn default_tls_cert_path() -> String {
    "certs/server.crt".to_string()
}

fn default_tls_key_path() -> String {
    "certs/server.key".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            backend: BackendConfig::default(),
            address: default_address(),
            port: default_port(),
            tls_enable: false,
            tls_cert_path: default_tls_cert_path(),
            tls_key_path: default_tls_key_path(),
        }
    }
}

impl Config {
    /// Load from config.toml (if present) and environment variables.
    /// Environment variables override file values.
    pub fn load() -> Result<Self> {
        let base: Config = Default::default();
        let mut fig = Figment::from(Serialized::defaults(base));
        if std::path::Path::new("config.toml").exists() {
            fig = fig.merge(Toml::file("config.toml"));
        }
        let mut cfg: Config = fig.extract().context("failed to parse configuration")?;

        if let Ok(v) = std::env::var("BLINK_API_URL") {
            cfg.backend.api_url = v;
        }
        if let Ok(v) = std::env::var("BLINK_API_KEY") {
            cfg.backend.api_key = v;
        }
        if let Ok(v) = std::env::var("BLINK_WALLET_ID") {
            cfg.backend.wallet_id = v;
        }
        if let Ok(v) = std::env::var("SERVER_ADDRESS") {
            cfg.address = v;
        }
        if let Ok(v) = std::env::var("SERVER_PORT") {
            cfg.port = v
                .parse()
                .with_context(|| format!("invalid SERVER_PORT value `{v}`"))?;
        }
        if let Ok(v) = std::env::var("TLS_ENABLE") {
            cfg.tls_enable = parse_bool_env("TLS_ENABLE", &v)?;
        }
        if let Ok(v) = std::env::var("TLS_CERT_PATH") {
            cfg.tls_cert_path = v;
        }
        if let Ok(v) = std::env::var("TLS_KEY_PATH") {
            cfg.tls_key_path = v;
        }

        Ok(cfg)
    }

    pub fn from_env() -> Result<Self> {
        Self::load()
    }
}

fn parse_bool_env(name: &str, value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => bail!("invalid {name} value `{value}`; expected true or false"),
    }
}

#[cfg(test)]
mod tests {
    use super::Config;
    use serial_test::serial;
    use std::{
        env, fs,
        path::{Path, PathBuf},
    };

    const ENV_KEYS: [&str; 8] = [
        "BLINK_API_URL",
        "BLINK_API_KEY",
        "BLINK_WALLET_ID",
        "SERVER_ADDRESS",
        "SERVER_PORT",
        "TLS_ENABLE",
        "TLS_CERT_PATH",
        "TLS_KEY_PATH",
    ];

    struct CwdGuard {
        orig: PathBuf,
    }

    impl CwdGuard {
        fn change_to<P: AsRef<Path>>(dir: P) -> Self {
            let orig = env::current_dir().expect("get current dir");
            env::set_current_dir(&dir).expect("set current dir");
            Self { orig }
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = env::set_current_dir(&self.orig);
        }
    }

    struct EnvGuard {
        saved: Vec<(String, Option<String>)>,
    }

    impl EnvGuard {
        fn new(keys: &[&str]) -> Self {
            let saved = keys
                .iter()
                .map(|k| ((*k).to_string(), env::var(k).ok()))
                .collect::<Vec<_>>();
            for k in keys {
                env::remove_var(k);
            }
            Self { saved }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in self.saved.drain(..) {
                if let Some(val) = v {
                    env::set_var(&k, val);
                } else {
                    env::remove_var(&k);
                }
            }
        }
    }

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let cwd = env::current_dir().expect("get current dir");
        let base = cwd.join("target").join("test-tmp");
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time went backwards")
            .as_millis();
        let dir = base.join(format!("{}-{}-{}", prefix, std::process::id(), ts));
        fs::create_dir_all(&dir).expect("create unique temp dir");
        dir
    }

    #[test]
    #[serial]
    fn test_load_defaults() {
        let _env = EnvGuard::new(&ENV_KEYS);
        let dir = unique_temp_dir("settings-defaults");
        let _cwd = CwdGuard::change_to(&dir);

        let cfg = Config::load().expect("load defaults");
        assert_eq!(cfg.backend.api_url, "https://api.blink.sv/graphql");
        assert!(cfg.backend.api_key.is_empty());
        assert_eq!(cfg.address, "0.0.0.0");
        assert_eq!(cfg.port, 50051);
        assert!(!cfg.tls_enable);
    }

    #[test]
    #[serial]
    fn test_load_from_toml_file() {
        let _env = EnvGuard::new(&ENV_KEYS);
        let dir = unique_temp_dir("settings-toml");
        let _cwd = CwdGuard::change_to(&dir);

        fs::write(
            dir.join("config.toml"),
            r#"
address = "127.0.0.1"
port = 12345

[backend]
api_url = "https://example.test/graphql"
api_key = "test-key"
wallet_id = "test-wallet"
"#,
        )
        .expect("write config.toml");

        let cfg = Config::load().expect("load toml");
        assert_eq!(cfg.backend.api_url, "https://example.test/graphql");
        assert_eq!(cfg.backend.api_key, "test-key");
        assert_eq!(cfg.backend.wallet_id, "test-wallet");
        assert_eq!(cfg.address, "127.0.0.1");
        assert_eq!(cfg.port, 12345);
    }

    #[test]
    #[serial]
    fn test_env_overrides_take_precedence() {
        let _env = EnvGuard::new(&ENV_KEYS);
        let dir = unique_temp_dir("settings-env");
        let _cwd = CwdGuard::change_to(&dir);

        fs::write(
            dir.join("config.toml"),
            r#"
port = 11111

[backend]
api_url = "https://file.test/graphql"
api_key = "file-key"
"#,
        )
        .expect("write config.toml");

        env::set_var("BLINK_API_URL", "https://env.test/graphql");
        env::set_var("BLINK_API_KEY", "env-key");
        env::set_var("BLINK_WALLET_ID", "env-wallet");
        env::set_var("SERVER_PORT", "54321");
        env::set_var("TLS_ENABLE", "true");

        let cfg = Config::load().expect("load env");
        assert_eq!(cfg.backend.api_url, "https://env.test/graphql");
        assert_eq!(cfg.backend.api_key, "env-key");
        assert_eq!(cfg.backend.wallet_id, "env-wallet");
        assert_eq!(cfg.port, 54321);
        assert!(cfg.tls_enable);
    }

    #[test]
    #[serial]
    fn test_invalid_port_is_an_error() {
        let _env = EnvGuard::new(&ENV_KEYS);
        let dir = unique_temp_dir("settings-invalid");
        let _cwd = CwdGuard::change_to(&dir);

        env::set_var("SERVER_PORT", "not-a-number");
        assert!(Config::load().is_err());
    }
}
