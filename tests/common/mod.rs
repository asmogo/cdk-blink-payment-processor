#![allow(dead_code)]

use std::{
    fs,
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread::sleep,
    time::{Duration, Instant},
};

pub use wiremock::{Mock, MockServer, ResponseTemplate};

/// Start a Wiremock HTTP server for stubbing Blink REST calls.
pub async fn start_wiremock() -> MockServer {
    MockServer::start().await
}

/// Build a reqwest client with reasonable timeouts for tests.
pub fn test_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .connect_timeout(Duration::from_secs(5))
        .build()
        .expect("failed to build reqwest client")
}

/// Busy-wait until a TCP port is accepting connections or timeout elapses.
pub fn wait_for_port(addr: SocketAddr, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if TcpStream::connect(addr).is_ok() {
            return Ok(());
        }
        sleep(Duration::from_millis(50));
    }
    Err(format!(
        "port {} not accepting connections after {:?}",
        addr, timeout
    ))
}

/// Create a unique directory under target/test-tmp for test artifacts.
/// This avoids depending on external crates while providing isolation.
pub fn unique_temp_dir(prefix: &str) -> PathBuf {
    let base = Path::new("target").join("test-tmp");
    let _ = fs::create_dir_all(&base);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time went backwards")
        .as_millis();
    let pid = std::process::id();
    let dir = base.join(format!("{}-{}-{}", prefix, pid, ts));
    fs::create_dir_all(&dir).expect("failed to create unique temp dir");
    dir
}

/// Spawn a long-lived child process and kill it on drop.
/// Useful fallback for black-box binary tests when in-process bootstrap is unavailable.
pub struct ChildGuard {
    child: Child,
}

impl ChildGuard {
    /// Spawn a process with args and environment; optionally set working directory.
    pub fn spawn(
        cmd: &str,
        args: &[&str],
        env: &[(&str, &str)],
        cwd: Option<&Path>,
    ) -> std::io::Result<Self> {
        let mut command = Command::new(cmd);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        for (k, v) in env {
            command.env(k, v);
        }
        if let Some(dir) = cwd {
            command.current_dir(dir);
        }
        let child = command.spawn()?;
        Ok(Self { child })
    }

    /// Return OS process id.
    pub fn id(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Run an async block with Tokio's time paused, useful for deterministic backoff/retry tests.
pub async fn with_paused_time<F, Fut, T>(f: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    tokio::time::pause();
    let out = f().await;
    tokio::time::resume();
    out
}

/// Convenience matchers re-export for Wiremock.
pub mod wm {
    pub use wiremock::matchers;
}

/// Utility to bind to an ephemeral TCP port and return the listener and its socket address.
/// Can be used by servers that support taking a bound listener.
pub fn bind_ephemeral(localhost_only: bool) -> (TcpListener, SocketAddr) {
    let host = if localhost_only { "127.0.0.1" } else { "0.0.0.0" };
    let listener = TcpListener::bind((host, 0)).expect("failed to bind to ephemeral port");
    let addr = listener.local_addr().expect("failed to get local addr");
    (listener, addr)
}