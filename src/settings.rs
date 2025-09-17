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


#[cfg(test)]
mod tests {
    use super::Config;
    use serial_test::serial;
    use std::{
        env,
        fs,
        path::{Path, PathBuf},
    };

    // Environment variable keys used by Config::load
    const ENV_KEYS: [&str; 4] = [
        "BLINK_API_URL",
        "BLINK_API_KEY",
        "BLINK_WALLET_ID",
        "SERVER_PORT",
    ];

    // Guard to change current working directory and restore it on drop.
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

    // Guard to clear a set of env vars and restore previous values on drop.
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

    // Create a unique temp directory under target/test-tmp for isolation.
    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let cwd = env::current_dir().expect("get current dir");
        let base = cwd.join("target").join("test-tmp");
        let _ = fs::create_dir_all(&base);
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time went backwards")
            .as_millis();
        let pid = std::process::id();
        let dir = base.join(format!("{}-{}-{}", prefix, pid, ts));
        fs::create_dir_all(&dir).expect("create unique temp dir");
        dir
    }

    fn write_config(dir: &Path, toml: &str) {
        fs::write(dir.join("config.toml"), toml).expect("write config.toml");
    }

    #[test]
    #[serial]
    fn test_load_defaults() {
        // Ensure no env overrides and no config file by working in a fresh temp dir.
        let _env = EnvGuard::new(&ENV_KEYS);
        let dir = unique_temp_dir("settings-defaults");
        let _cwd = CwdGuard::change_to(&dir);

        let cfg = Config::load();
        let def = Config::default();

        assert_eq!(cfg.blink_api_url, def.blink_api_url);
        assert_eq!(cfg.blink_api_key, def.blink_api_key);
        assert_eq!(cfg.blink_wallet_id, def.blink_wallet_id);
        assert_eq!(cfg.server_port, def.server_port);
    }

    #[test]
    #[serial]
    fn test_load_from_toml_file() {
        let _env = EnvGuard::new(&ENV_KEYS);
        let dir = unique_temp_dir("settings-toml");
        let _cwd = CwdGuard::change_to(&dir);

        let toml = r#"
blink_api_url = "https://example.test/graphql"
blink_api_key = "test-key"
blink_wallet_id = "test-wallet"
server_port = 12345
"#;
        write_config(&dir, toml);

        let cfg = Config::load();
        assert_eq!(cfg.blink_api_url, "https://example.test/graphql");
        assert_eq!(cfg.blink_api_key, "test-key");
        assert_eq!(cfg.blink_wallet_id, "test-wallet");
        assert_eq!(cfg.server_port, 12345);
    }

    #[test]
    #[serial]
    fn test_env_overrides_take_precedence() {
        let _env = EnvGuard::new(&ENV_KEYS);
        let dir = unique_temp_dir("settings-env-over");
        let _cwd = CwdGuard::change_to(&dir);

        let toml = r#"
blink_api_url = "https://file.test/graphql"
blink_api_key = "file-key"
blink_wallet_id = "file-wallet"
server_port = 11111
"#;
        write_config(&dir, toml);

        env::set_var("BLINK_API_URL", "https://env.test/graphql");
        env::set_var("BLINK_API_KEY", "env-key");
        env::set_var("BLINK_WALLET_ID", "env-wallet");
        env::set_var("SERVER_PORT", "54321");

        let cfg = Config::load();

        assert_eq!(cfg.blink_api_url, "https://env.test/graphql");
        assert_eq!(cfg.blink_api_key, "env-key");
        assert_eq!(cfg.blink_wallet_id, "env-wallet");
        assert_eq!(cfg.server_port, 54321);
    }

    #[test]
    #[serial]
    fn test_invalid_values_error_or_clamp() {
        // A) Provide valid file port; env SERVER_PORT unparsable -> fallback to file value.
        let _env = EnvGuard::new(&ENV_KEYS);
        let dir = unique_temp_dir("settings-invalid");
        let _cwd = CwdGuard::change_to(&dir);

        let toml_ok = r#"
blink_api_url = "https://valid.test/graphql"
server_port = 1000
"#;
        write_config(&dir, toml_ok);
        env::set_var("SERVER_PORT", "not-a-number");
        let cfg = Config::load();
        assert_eq!(cfg.server_port, 1000, "unparsable env should fallback to file value");

        // B) Env SERVER_PORT out-of-range for u16 -> also fallback to file value.
        env::set_var("SERVER_PORT", "70000"); // > u16::MAX
        let cfg = Config::load();
        assert_eq!(cfg.server_port, 1000, "out-of-range env should fallback to file value");

        // C) Negative env -> fallback
        env::set_var("SERVER_PORT", "-1");
        let cfg = Config::load();
        assert_eq!(cfg.server_port, 1000, "negative env should fallback to file value");

        // D) Invalid server_port in TOML (-1) makes extraction fail and defaults used.
        let bad_toml = r#"
blink_api_url = "https://will_be_ignored"
server_port = -1
"#;
        write_config(&dir, bad_toml);
        env::remove_var("SERVER_PORT");
        let cfg = Config::load();
        let def = Config::default();
        assert_eq!(cfg.blink_api_url, def.blink_api_url);
        assert_eq!(cfg.server_port, def.server_port);

        // E) "Malformed" URL strings are accepted as-is (no URL validation in Config).
        env::set_var("BLINK_API_URL", "::::not a url::::");
        let cfg = Config::load();
        assert_eq!(cfg.blink_api_url, "::::not a url::::");
    }

    #[test]
    #[serial]
    fn test_merge_partial_config() {
        let _env = EnvGuard::new(&ENV_KEYS);
        let dir = unique_temp_dir("settings-partial");
        let _cwd = CwdGuard::change_to(&dir);

        let toml = r#"
blink_api_key = "partial-key"
"#;
        write_config(&dir, toml);

        let cfg = Config::load();
        let def = Config::default();

        assert_eq!(cfg.blink_api_key, "partial-key");
        assert_eq!(cfg.blink_api_url, def.blink_api_url);
        assert_eq!(cfg.blink_wallet_id, def.blink_wallet_id);
        assert_eq!(cfg.server_port, def.server_port);
    }

    #[test]
    #[serial]
    fn test_roundtrip_serde_if_applicable() {
        let cfg = Config {
            blink_api_url: "https://roundtrip.test/graphql".to_string(),
            blink_api_key: "roundtrip-key".to_string(),
            blink_wallet_id: "roundtrip-wallet".to_string(),
            server_port: 25000,
        };
        let json = serde_json::to_string(&cfg).expect("serialize config");
        let back: Config = serde_json::from_str(&json).expect("deserialize config");
        assert_eq!(back.blink_api_url, cfg.blink_api_url);
        assert_eq!(back.blink_api_key, cfg.blink_api_key);
        assert_eq!(back.blink_wallet_id, cfg.blink_wallet_id);
        assert_eq!(back.server_port, cfg.server_port);
    }

    #[test]
    #[serial]
    fn proptest_valid_port_bounds() {
        use proptest::prelude::*;
        use proptest::test_runner::TestRunner;

        let mut runner = TestRunner::default();
        runner
            .run(&any::<u16>(), |port| {
                let _env = EnvGuard::new(&ENV_KEYS);
                let dir = unique_temp_dir("settings-prop");
                let _cwd = CwdGuard::change_to(&dir);

                env::set_var("SERVER_PORT", port.to_string());
                let cfg = Config::load();
                prop_assert_eq!(cfg.server_port, port);
                Ok(())
            })
            .unwrap();
    }
}
