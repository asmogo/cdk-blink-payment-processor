//! Configuration management for the CDK Blink Payment Processor.
//!
//! This module handles loading and merging configuration from multiple sources with a clear
//! priority order:
//!   1. Hard-coded defaults (see `Config::default()`)
//!   2. `config.toml` file (if present in the current working directory)
//!   3. Environment variables (highest priority, override file and defaults)
//!
//! The configuration system uses:
//! - **Figment** for merging configuration from multiple sources
//! - **humantime_serde** for parsing human-readable duration strings (e.g., "30s", "1m", "1h")
//! - **serde** for serialization/deserialization across TOML and environment variables
//!
//! # Configuration Priority
//!
//! When loading configuration, values are applied in this order (later sources override earlier ones):
//! 1. Built-in defaults from `Config::default()`
//! 2. Values from `config.toml` (only if file exists)
//! 3. Environment variable overrides (highest priority)
//!
//! # Invalid Values
//!
//! - Invalid environment variable values fall back to the previously loaded value (TOML or default)
//! - Invalid TOML files cause the entire config to reset to defaults
//! - All fallbacks are silent to allow partial configuration updates

use figment::{
    providers::{Format, Serialized, Toml},
    Figment,
};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Complete configuration for the CDK Blink Payment Processor server.
///
/// This struct contains all runtime configuration needed for:
/// - Connecting to the Blink GraphQL API
/// - Running the gRPC server
/// - Configuring TLS for secure communication
/// - Tuning gRPC connection parameters
///
/// # Supported Environment Variables
///
/// All fields can be overridden via environment variables:
/// - `BLINK_API_URL`: Blink GraphQL endpoint (default: "https://api.blink.sv/graphql")
/// - `BLINK_API_KEY`: Authentication token for Blink API
/// - `BLINK_WALLET_ID`: Wallet ID to use for transactions
/// - `SERVER_PORT`: gRPC server listening port (default: 50051)
/// - `TLS_ENABLE`: Enable TLS for gRPC server ("true", "1", "yes" → enabled)
/// - `TLS_CERT_PATH`: Path to TLS certificate file (default: "certs/server.crt")
/// - `TLS_KEY_PATH`: Path to TLS private key file (default: "certs/server.key")
/// - `KEEP_ALIVE_INTERVAL`: Ping interval for idle connections (default: "30s")
/// - `KEEP_ALIVE_TIMEOUT`: Timeout for ping response (default: "10s")
/// - `MAX_CONNECTION_AGE`: Maximum duration for a single connection (default: "30m")
///
/// Duration values support human-readable formats like "30s", "5m", "1h", "1d", etc.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Config {
    /// URL of the Blink GraphQL API endpoint.
    /// This must be a valid HTTPS URL pointing to the Blink service.
    pub blink_api_url: String,

    /// API authentication key for the Blink service.
    /// This key must be obtained from the Blink dashboard and has sufficient permissions
    /// to create invoices and send payments.
    pub blink_api_key: String,

    /// Default wallet ID to use for all transactions.
    /// Transactions will be created/received using this wallet unless overridden in the request.
    pub blink_wallet_id: String,

    /// Port on which the gRPC server listens.
    /// Valid range is 0-65535, but typically use 1024+ on Unix systems to avoid requiring root.
    pub server_port: u16,

    /// Enable TLS for the gRPC server.
    /// When true, the server requires valid certificate and key files at the paths specified
    /// in `tls_cert_path` and `tls_key_path`.
    pub tls_enable: bool,

    /// Path to the TLS certificate file (in PEM format).
    /// Only used if `tls_enable` is true. Path is relative to current working directory.
    pub tls_cert_path: String,

    /// Path to the TLS private key file (in PEM format).
    /// Only used if `tls_enable` is true. Path is relative to current working directory.
    /// Must not be world-readable for security reasons.
    pub tls_key_path: String,

    /// Interval between keepalive pings for idle gRPC connections.
    /// Helps detect dead connections and prevent premature disconnection.
    /// Typical value: 30 seconds. Cannot be less than 10 seconds (enforced by gRPC library).
    #[serde(default = "default_keep_alive_interval", with = "humantime_serde")]
    pub keep_alive_interval: Duration,

    /// Timeout waiting for a keepalive ping response from the client.
    /// If no response is received within this time, the connection is closed.
    /// Typical value: 10 seconds. Must be less than `keep_alive_interval`.
    #[serde(default = "default_keep_alive_timeout", with = "humantime_serde")]
    pub keep_alive_timeout: Duration,

    /// Maximum age for a gRPC connection before it's closed.
    /// Helps prevent resource exhaustion from long-lived connections and enables graceful
    /// load balancer drain periods. Typical value: 30 minutes (1800 seconds).
    #[serde(default = "default_max_connection_age", with = "humantime_serde")]
    pub max_connection_age: Duration,
}

impl Default for Config {
    /// Returns the default configuration for the payment processor.
    ///
    /// Default values are designed for development use. Production deployments should:
    /// 1. Provide valid Blink API credentials via environment variables or config.toml
    /// 2. Enable TLS (set `TLS_ENABLE=true` and provide certificate/key paths)
    /// 3. Tune connection parameters based on expected load
    ///
    /// # Defaults
    ///
    /// - **blink_api_url**: "https://api.blink.sv/graphql" (official Blink API)
    /// - **blink_api_key**: "" (must be provided by user)
    /// - **blink_wallet_id**: "" (must be provided by user)
    /// - **server_port**: 50051 (standard gRPC port)
    /// - **tls_enable**: false (disabled for development)
    /// - **tls_cert_path**: "certs/server.crt"
    /// - **tls_key_path**: "certs/server.key"
    /// - **keep_alive_interval**: 30 seconds
    /// - **keep_alive_timeout**: 10 seconds
    /// - **max_connection_age**: 30 minutes (1800 seconds)
    fn default() -> Self {
        Self {
            blink_api_url: "https://api.blink.sv/graphql".to_string(),
            blink_api_key: "".to_string(),
            blink_wallet_id: "".to_string(),
            server_port: 50051,
            tls_enable: false,
            tls_cert_path: "certs/server.crt".to_string(),
            tls_key_path: "certs/server.key".to_string(),
            keep_alive_interval: default_keep_alive_interval(),
            keep_alive_timeout: default_keep_alive_timeout(),
            max_connection_age: default_max_connection_age(),
        }
    }
}

impl Config {
    /// Load configuration from all sources with proper priority ordering.
    ///
    /// Configuration loading follows this sequence:
    ///
    /// 1. **Defaults**: Initialize with hard-coded defaults via `Config::default()`
    /// 2. **TOML File**: If `config.toml` exists in the current directory, merge its values
    /// 3. **Environment**: Override with any environment variables found
    ///
    /// This design ensures that:
    /// - Missing config files don't cause errors (defaults are always available)
    /// - Environment variables can override any other source (important for containerized deployments)
    /// - Invalid TOML causes fallback to defaults (graceful degradation)
    /// - Invalid environment values fall back to previous values without error
    ///
    /// # Environment Variables
    ///
    /// All supported variables are documented in `Config` struct documentation.
    /// Invalid values are silently ignored with fallback to previous value.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// // Load from defaults + optional config.toml + optional env vars
    /// let cfg = Config::load();
    /// assert!(cfg.server_port > 0);
    /// ```
    pub fn load() -> Self {
        // STEP 1: Start with defaults as the baseline.
        // This ensures all fields have valid values even if config.toml or env are missing.
        let base: Config = Default::default();
        let mut fig = Figment::from(Serialized::defaults(base));

        // STEP 2: Merge config.toml if it exists.
        // Use figment's file provider to safely handle missing files (no error raised).
        // If the TOML has syntax errors, we'll fall back to defaults in the extract() call.
        if std::path::Path::new("config.toml").exists() {
            fig = fig.merge(Toml::file("config.toml"));
        }

        // STEP 3: Extract configuration from the merged sources.
        // If extraction fails (e.g., invalid TOML syntax), use defaults.
        let mut cfg: Config = fig.extract().unwrap_or_default();

        // STEP 4: Apply environment variable overrides.
        // Environment variables have the highest priority and override both defaults and TOML values.
        // Each env var is checked independently; invalid values silently fall back to the current value.

        // Blink API endpoint URL (any string value is accepted, no validation)
        if let Ok(v) = std::env::var("BLINK_API_URL") {
            cfg.blink_api_url = v;
        }

        // Blink API authentication key (sensitive - should come from secrets management in production)
        if let Ok(v) = std::env::var("BLINK_API_KEY") {
            cfg.blink_api_key = v;
        }

        // Wallet ID for transactions (must match a wallet accessible with the API key)
        if let Ok(v) = std::env::var("BLINK_WALLET_ID") {
            cfg.blink_wallet_id = v;
        }

        // Server port number (parsed as u16, must be 1-65535 for valid use)
        // Falls back to current value if parsing fails (e.g., "not-a-number" or "99999")
        if let Ok(v) = std::env::var("SERVER_PORT") {
            cfg.server_port = v.parse().unwrap_or(cfg.server_port);
        }

        // TLS enablement (accepts multiple formats for convenience)
        // Enabled: "1", "true", "TRUE", "yes", "YES"
        // Disabled: anything else (including "false", "0", "no", etc.)
        if let Ok(v) = std::env::var("TLS_ENABLE") {
            cfg.tls_enable = matches!(v.as_str(), "1" | "true" | "TRUE" | "yes" | "YES");
        }

        // Path to TLS certificate file (PEM format, relative or absolute)
        if let Ok(v) = std::env::var("TLS_CERT_PATH") {
            cfg.tls_cert_path = v;
        }

        // Path to TLS private key file (PEM format, relative or absolute)
        // File permissions should restrict this to prevent unauthorized access
        if let Ok(v) = std::env::var("TLS_KEY_PATH") {
            cfg.tls_key_path = v;
        }

        // Keep-alive interval for idle connections (uses humantime format)
        // Examples: "30s", "1m", "1h"
        // Falls back to current value if parse fails
        if let Ok(v) = std::env::var("KEEP_ALIVE_INTERVAL") {
            cfg.keep_alive_interval = parse_duration_env(&v, cfg.keep_alive_interval);
        }

        // Keep-alive timeout (how long to wait for ping response)
        // Must be less than keep_alive_interval or gRPC will reject it
        if let Ok(v) = std::env::var("KEEP_ALIVE_TIMEOUT") {
            cfg.keep_alive_timeout = parse_duration_env(&v, cfg.keep_alive_timeout);
        }

        // Maximum connection age (connections older than this are closed)
        // Useful for load balancer graceful drains and preventing resource leaks
        if let Ok(v) = std::env::var("MAX_CONNECTION_AGE") {
            cfg.max_connection_age = parse_duration_env(&v, cfg.max_connection_age);
        }

        cfg
    }

    /// Alias for `Config::load()`.
    ///
    /// Provided for semantic clarity when loading configuration from the environment.
    /// Behaves identically to `Config::load()`.
    pub fn from_env() -> Self {
        Self::load()
    }
}

/// Parse a duration string using humantime format, with fallback to current value.
///
/// # Supported Formats
///
/// The humantime library supports these duration formats:
/// - Seconds: "30s", "60sec"
/// - Minutes: "5m", "30min"
/// - Hours: "1h", "24hr"
/// - Days: "1d", "7day"
/// - Combined: "1h30m", "2d3h4m"
///
/// # Behavior
///
/// If parsing fails, the `current` value is returned unchanged. This allows graceful
/// degradation if an environment variable contains an invalid duration string.
///
/// # Examples
///
/// ```ignore
/// let current = Duration::from_secs(60);
/// let parsed = parse_duration_env("30s", current);
/// assert_eq!(parsed, Duration::from_secs(30));
///
/// let invalid = parse_duration_env("invalid", current);
/// assert_eq!(invalid, current); // Falls back
/// ```
fn parse_duration_env(value: &str, current: Duration) -> Duration {
    humantime::parse_duration(value).unwrap_or(current)
}

/// Default keep-alive ping interval for gRPC connections.
///
/// 30 seconds is a reasonable balance between:
/// - Early detection of dead connections (higher frequency = faster detection)
/// - Reduced overhead and network load (lower frequency = less network traffic)
///
/// Note: gRPC enforces a minimum of 10 seconds for this interval.
fn default_keep_alive_interval() -> Duration {
    Duration::from_secs(30)
}

/// Default timeout for keep-alive ping responses.
///
/// 10 seconds is appropriate for most deployments because:
/// - It's long enough for normal network latency (typically < 1-5 seconds)
/// - It's short enough to quickly detect truly dead connections
/// - It must be less than keep_alive_interval
///
/// If a client doesn't respond within this time, the connection is closed by the server.
fn default_keep_alive_timeout() -> Duration {
    Duration::from_secs(10)
}

/// Default maximum age for a single gRPC connection.
///
/// 30 minutes (1800 seconds) provides good balance between:
/// - **Connection reuse**: 30 minutes allows most client-server interactions to reuse connections
/// - **Resource efficiency**: Periodic connection closure prevents resource exhaustion
/// - **Load balancing**: Works well with load balancer drain periods during rolling deploys
/// - **Client reconnection**: Gives clients time to establish new connections before drain
///
/// For high-throughput services, consider lowering this to 5-15 minutes.
/// For low-traffic services, can be increased to 1-2 hours.
fn default_max_connection_age() -> Duration {
    Duration::from_secs(1800)
}

#[cfg(test)]
mod tests {
    //! Configuration loading tests with proper isolation.
    //!
    //! All tests use guards to isolate environment state:
    //! - `EnvGuard`: Clears environment variables before test, restores after
    //! - `CwdGuard`: Changes to temp directory, restores original after
    //!
    //! This prevents test cross-contamination and allows running tests in parallel
    //! (with `#[serial]` for tests that need sequential execution).

    use super::Config;
    use serial_test::serial;
    use std::{
        env, fs,
        path::{Path, PathBuf},
    };
    use std::time::Duration;

    /// Environment variable keys that Config::load respects.
    /// Used by EnvGuard to clear/restore state around each test.
    const ENV_KEYS: [&str; 9] = [
        "BLINK_API_URL",
        "BLINK_API_KEY",
        "BLINK_WALLET_ID",
        "SERVER_PORT",
        "KEEP_ALIVE_INTERVAL",
        "KEEP_ALIVE_TIMEOUT",
        "MAX_CONNECTION_IDLE",
        "MAX_CONNECTION_AGE",
        "MAX_CONNECTION_AGE_GRACE",
    ];

    /// RAII guard to change working directory and restore it on drop.
    ///
    /// Allows tests to safely change cwd without affecting other tests or the main process.
    /// The original directory is always restored, even if a test panics.
    struct CwdGuard {
        orig: PathBuf,
    }

    impl CwdGuard {
        /// Change to a new directory, saving the current one for restoration.
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

    /// RAII guard to clear environment variables and restore them on drop.
    ///
    /// Before entering a test, saves the current values of specific env vars,
    /// clears them, then restores them on drop. Ensures tests don't see
    /// leftover state from the shell or previous tests.
    struct EnvGuard {
        saved: Vec<(String, Option<String>)>,
    }

    impl EnvGuard {
        /// Save and clear the specified environment variables.
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

    /// Create a unique temp directory with isolated config file for tests.
    ///
    /// Each test gets its own temporary directory based on:
    /// - Provided prefix (for identification)
    /// - Current process ID (for parallel test safety)
    /// - Current timestamp (for multiple runs of the same test)
    ///
    /// This ensures tests can create/modify config.toml without interfering with each other.
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

    /// Write a config.toml file with the given TOML content.
    fn write_config(dir: &Path, toml: &str) {
        fs::write(dir.join("config.toml"), toml).expect("write config.toml");
    }

    /// Test 1: Configuration defaults are applied when no config file exists and no env vars set.
    ///
    /// This ensures that running Config::load() with a clean environment returns
    /// the expected hard-coded defaults for all fields.
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

    /// Test 2: Configuration is correctly loaded from config.toml file.
    ///
    /// Verifies that values in config.toml override the hard-coded defaults
    /// when the file exists and is valid.
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

    /// Test 3: Environment variables take precedence over config.toml and defaults.
    ///
    /// This is the critical test for ensuring environment variables override all other sources,
    /// which is essential for containerized deployments where env vars are the primary
    /// configuration method.
    #[test]
    #[serial]
    fn test_env_overrides_take_precedence() {
        let _env = EnvGuard::new(&ENV_KEYS);
        let dir = unique_temp_dir("settings-env-over");
        let _cwd = CwdGuard::change_to(&dir);

        // Set up conflicting values in TOML
        let toml = r#"
blink_api_url = "https://file.test/graphql"
blink_api_key = "file-key"
blink_wallet_id = "file-wallet"
server_port = 11111
"#;
        write_config(&dir, toml);

        // Set environment variables with different values
        env::set_var("BLINK_API_URL", "https://env.test/graphql");
        env::set_var("BLINK_API_KEY", "env-key");
        env::set_var("BLINK_WALLET_ID", "env-wallet");
        env::set_var("SERVER_PORT", "54321");

        let cfg = Config::load();

        // Environment values should win
        assert_eq!(cfg.blink_api_url, "https://env.test/graphql");
        assert_eq!(cfg.blink_api_key, "env-key");
        assert_eq!(cfg.blink_wallet_id, "env-wallet");
        assert_eq!(cfg.server_port, 54321);
    }

    /// Test 4: Invalid configuration values are handled gracefully.
    ///
    /// Covers multiple failure scenarios:
    /// - Invalid env var format (not-a-number) -> fallback
    /// - Out-of-range env var (70000 > u16::MAX) -> fallback
    /// - Negative env var (-1) -> fallback
    /// - Invalid TOML syntax -> reset to defaults
    /// - Non-URL strings in API URL -> accepted as-is (no validation)
    #[test]
    #[serial]
    fn test_invalid_values_error_or_clamp() {
        let _env = EnvGuard::new(&ENV_KEYS);
        let dir = unique_temp_dir("settings-invalid");
        let _cwd = CwdGuard::change_to(&dir);

        // Scenario A: Provide valid file port; env SERVER_PORT unparsable -> fallback to file value.
        let toml_ok = r#"
blink_api_url = "https://valid.test/graphql"
server_port = 1000
"#;
        write_config(&dir, toml_ok);
        env::set_var("SERVER_PORT", "not-a-number");
        let cfg = Config::load();
        assert_eq!(
            cfg.server_port, 1000,
            "unparsable env should fallback to file value"
        );

        // Scenario B: Env SERVER_PORT out-of-range for u16 -> also fallback to file value.
        env::set_var("SERVER_PORT", "70000"); // > u16::MAX (65535)
        let cfg = Config::load();
        assert_eq!(
            cfg.server_port, 1000,
            "out-of-range env should fallback to file value"
        );

        // Scenario C: Negative env -> fallback
        env::set_var("SERVER_PORT", "-1");
        let cfg = Config::load();
        assert_eq!(
            cfg.server_port, 1000,
            "negative env should fallback to file value"
        );

        // Scenario D: Invalid server_port in TOML (-1) makes extraction fail and defaults used.
        let bad_toml = r#"
blink_api_url = "https://will_be_ignored"
server_port = -1
"#;
        write_config(&dir, bad_toml);
        env::remove_var("SERVER_PORT");
        let cfg = Config::load();
        let def = Config::default();
        assert_eq!(cfg.blink_api_url, def.blink_api_url, "invalid TOML should reset to defaults");
        assert_eq!(cfg.server_port, def.server_port, "invalid TOML should reset to defaults");

        // Scenario E: "Malformed" URL strings are accepted as-is (no URL validation in Config).
        // This is intentional - URL validation should happen at a higher layer.
        env::set_var("BLINK_API_URL", "::::not a url::::");
        let cfg = Config::load();
        assert_eq!(cfg.blink_api_url, "::::not a url::::");
    }

    /// Test 5: Partial configuration merges correctly with defaults.
    ///
    /// When config.toml only specifies some fields, the rest should be filled in
    /// from defaults. This tests that the merge is additive, not replacement.
    #[test]
    #[serial]
    fn test_merge_partial_config() {
        let _env = EnvGuard::new(&ENV_KEYS);
        let dir = unique_temp_dir("settings-partial");
        let _cwd = CwdGuard::change_to(&dir);

        // Only specify one field in TOML
        let toml = r#"
blink_api_key = "partial-key"
"#;
        write_config(&dir, toml);

        let cfg = Config::load();
        let def = Config::default();

        // Specified field has custom value
        assert_eq!(cfg.blink_api_key, "partial-key");

        // Unspecified fields have defaults
        assert_eq!(cfg.blink_api_url, def.blink_api_url);
        assert_eq!(cfg.blink_wallet_id, def.blink_wallet_id);
        assert_eq!(cfg.server_port, def.server_port);
    }

    /// Test 6: Configuration can be serialized and deserialized via serde.
    ///
    /// Ensures that the Config struct can round-trip through JSON serialization,
    /// which is important for testing, logging, and potential future use cases.
    #[test]
    #[serial]
    fn test_roundtrip_serde_if_applicable() {
        // Create a config with custom values
        let cfg = Config {
            blink_api_url: "https://roundtrip.test/graphql".to_string(),
            blink_api_key: "roundtrip-key".to_string(),
            blink_wallet_id: "roundtrip-wallet".to_string(),
            server_port: 25000,
            tls_enable: true,
            tls_cert_path: "p.crt".into(),
            tls_key_path: "p.key".into(),
            keep_alive_interval: Duration::from_secs(15),
            keep_alive_timeout: Duration::from_secs(5),
            max_connection_age: Duration::from_secs(900),
        };

        // Serialize to JSON string
        let json = serde_json::to_string(&cfg).expect("serialize config");

        // Deserialize back from JSON
        let back: Config = serde_json::from_str(&json).expect("deserialize config");

        // Verify all fields survive the round-trip
        assert_eq!(back.blink_api_url, cfg.blink_api_url);
        assert_eq!(back.blink_api_key, cfg.blink_api_key);
        assert_eq!(back.blink_wallet_id, cfg.blink_wallet_id);
        assert_eq!(back.server_port, cfg.server_port);
    }

    /// Test 7: Property-based test for port number bounds (u16 valid range).
    ///
    /// Uses proptest to generate arbitrary u16 values and verify that all valid
    /// port numbers can be successfully parsed from environment variables.
    /// This catches any edge cases or numeric overflow issues.
    #[test]
    #[serial]
    fn proptest_valid_port_bounds() {
        use proptest::prelude::*;
        use proptest::test_runner::TestRunner;

        let mut runner = TestRunner::default();
        runner
            .run(&any::<u16>(), |port| {
                // Each property test iteration gets isolated environment
                let _env = EnvGuard::new(&ENV_KEYS);
                let dir = unique_temp_dir("settings-prop");
                let _cwd = CwdGuard::change_to(&dir);

                // Set port from generated value
                env::set_var("SERVER_PORT", port.to_string());
                let cfg = Config::load();

                // Should parse and preserve the value exactly
                prop_assert_eq!(cfg.server_port, port);
                Ok(())
            })
            .unwrap();
    }
}
