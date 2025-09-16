use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use http::HeaderValue;
use serde_json::{json, Value};
use tokio::task::JoinHandle;
use tokio::time::{sleep, Duration, Instant, interval};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tracing::{debug, info, warn, error, instrument};

#[derive(Clone, Debug)]
pub struct WSConfig {
    pub ping_interval: Duration,
    pub ack_timeout: Duration,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
}

impl Default for WSConfig {
    fn default() -> Self {
        Self {
            ping_interval: Duration::from_secs(30),
            ack_timeout: Duration::from_secs(10),
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(30),
        }
    }
}

pub struct WSClient {
    pub url: String,
    pub api_key: String,
    pub cfg: WSConfig,
}

impl WSClient {
    pub fn new(http_url: &str, api_key: &str) -> Self {
        let mut ws = http_url.replace("https://", "wss://").replace("http://", "ws://");
        ws = ws.replace("api.", "ws.");
        Self { url: ws, api_key: api_key.to_string(), cfg: WSConfig::default() }
    }

    /// Runs a resilient myUpdates subscription, reconnecting and resubscribing on errors.
    /// Forwards raw JSON payloads for `next` / `data` messages to `out`.
    #[instrument(skip(self, out), fields(url=%self.url))]
    pub async fn stream_my_updates(&self, out: tokio::sync::mpsc::Sender<Value>) -> Result<JoinHandle<()>> {
        let url_str = self.url.clone();
        let api_key = self.api_key.clone();
        let cfg = self.cfg.clone();

        let handle = tokio::spawn(async move {
            let mut backoff = cfg.initial_backoff;
            loop {
                if out.is_closed() { debug!("myUpdates stream stopping: receiver closed"); break; }

                // Build WS request from String so tungstenite fills required headers (incl Sec-WebSocket-Key)
                let mut req = match url_str.clone().into_client_request() {
                    Ok(r) => r,
                    Err(e) => { error!(error=?e, "failed to build ws request"); sleep(backoff).await; backoff = (backoff * 2).min(cfg.max_backoff); continue; }
                };
                // Subprotocol
                req.headers_mut().insert("Sec-WebSocket-Protocol", HeaderValue::from_static("graphql-transport-ws"));

                match connect_async(req).await {
                    Ok((mut ws, _resp)) => {
                        // INIT
                        if let Err(e) = ws.send(Message::Text(json!({"type":"connection_init","payload":{"X-API-KEY": api_key}}).to_string())).await {
                            error!(error=?e, "failed to send connection_init");
                            let _ = ws.close(None).await;
                            sleep(backoff).await; backoff = (backoff * 2).min(cfg.max_backoff); continue;
                        }
                        // Await ACK
                        let ack_deadline = Instant::now() + cfg.ack_timeout;
                        let mut got_ack = false;
                        while Instant::now() < ack_deadline {
                            match tokio::time::timeout(Duration::from_millis(500), ws.next()).await {
                                Ok(Some(Ok(Message::Text(txt)))) => {
                                    if let Ok(v) = serde_json::from_str::<Value>(&txt) {
                                        if v.get("type").and_then(|t| t.as_str()) == Some("connection_ack") { got_ack = true; break; }
                                    }
                                }
                                Ok(Some(Ok(Message::Ping(payload)))) => { let _ = ws.send(Message::Pong(payload)).await; }
                                Ok(Some(Ok(_))) => {}
                                Ok(Some(Err(e))) => { error!(error=?e, "ws read error awaiting ack"); break; }
                                Ok(None) => { error!("ws closed before ack"); break; }
                                Err(_) => {}
                            }
                        }
                        if !got_ack {
                            warn!("no connection_ack received");
                            let _ = ws.close(None).await;
                            sleep(backoff).await; backoff = (backoff * 2).min(cfg.max_backoff); continue;
                        }
                        info!("ws connected");

                        // Subscribe
                        let sub_id = format!("sub-{}", chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0));
                        let query = "subscription myUpdates { myUpdates { errors { message } me { id } update { __typename ... on LnUpdate { status transaction { id direction settlementAmount initiationVia { __typename ... on InitiationViaLn { paymentRequest paymentHash } } } } } } }";
                        if let Err(e) = ws.send(Message::Text(json!({"id": sub_id, "type": "subscribe", "payload": {"query": query}}).to_string())).await {
                            error!(error=?e, "failed to subscribe");
                            let _ = ws.close(None).await;
                            sleep(backoff).await; backoff = (backoff * 2).min(cfg.max_backoff); continue;
                        }

                        // Reset backoff
                        backoff = cfg.initial_backoff;
                        let mut ping_int = interval(cfg.ping_interval);

                        loop {
                            tokio::select! {
                                _ = ping_int.tick() => {
                                    if let Err(e) = ws.send(Message::Ping(Vec::new())).await { warn!(error=?e, "ping failed"); break; }
                                }
                                msg = ws.next() => {
                                    match msg {
                                        Some(Ok(Message::Text(txt))) => {
                                            if let Ok(v) = serde_json::from_str::<Value>(&txt) {
                                                let typ = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
                                                if typ == "next" || typ == "data" { let _ = out.send(v).await; }
                                                else if typ == "complete" || typ == "connection_error" { break; }
                                            } else { debug!("ws non-json: {}", txt); }
                                        }
                                        Some(Ok(Message::Ping(payload))) => { let _ = ws.send(Message::Pong(payload)).await; }
                                        Some(Ok(Message::Close(_))) => { warn!("ws close received"); break; }
                                        Some(Ok(_)) => {}
                                        Some(Err(e)) => { error!(error=?e, "ws read error"); break; }
                                        None => { warn!("ws stream ended"); break; }
                                    }
                                }
                            }
                            if out.is_closed() { let _ = ws.close(None).await; break; }
                        }
                    }
                    Err(e) => { error!(error=?e, "ws connect failed"); }
                }
                sleep(backoff).await;
                backoff = (backoff * 2).min(cfg.max_backoff);
            }
        });
        Ok(handle)
    }
}
