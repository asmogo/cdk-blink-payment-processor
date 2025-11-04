//! Blink WebSocket Subscription Client
//!
//! This module handles WebSocket connections to Blink's GraphQL subscription endpoint.
//! It provides resilient, auto-reconnecting subscriptions to payment updates.
//!
//! ## Protocol
//! Uses the "graphql-transport-ws" subprotocol for GraphQL subscriptions over WebSocket.
//! This protocol requires:
//! 1. Connection init with authentication
//! 2. Wait for connection_ack
//! 3. Send subscription with query
//! 4. Receive data messages until complete/error
//! 5. Handle ping/pong for keep-alive
//!
//! ## Resilience
//! - Automatic reconnection with exponential backoff on connection failure
//! - Detects and recovers from stale connections
//! - Respects client shutdown (receiver closed)

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use http::HeaderValue;
use serde_json::{json, Value};
use tokio::task::JoinHandle;
use tokio::time::{interval, sleep, Duration, Instant};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use tracing::{debug, error, info, instrument, warn};

/// Configuration for WebSocket connection behavior
///
/// # Fields
/// - `ping_interval`: How often to send ping messages (keep-alive)
/// - `ack_timeout`: Max time to wait for connection_ack from server
/// - `initial_backoff`: Initial retry delay on connection failure
/// - `max_backoff`: Maximum retry delay (prevents infinite growth)
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

/// WebSocket client for Blink GraphQL subscriptions
///
/// This client handles establishing and maintaining a WebSocket connection to Blink's
/// GraphQL subscription endpoint, with automatic reconnection on failure.
pub struct WSClient {
    pub url: String,
    pub api_key: String,
    pub cfg: WSConfig,
}

impl WSClient {
    /// Create a new WebSocket client from HTTP API URL
    ///
    /// # Parameters
    /// - `http_url`: HTTP GraphQL endpoint (e.g., "https://api.blink.sv/graphql")
    /// - `api_key`: Blink API key for authentication
    ///
    /// # Returns
    /// A new WSClient with the URL converted to WebSocket format
    ///
    /// # URL Conversion
    /// - https:// → wss:// (secure WebSocket)
    /// - http:// → ws:// (plain WebSocket)
    /// - api.blink.sv → ws.blink.sv (different endpoint for subscriptions)
    pub fn new(http_url: &str, api_key: &str) -> Self {
        // Convert HTTP URL to WebSocket URL
        let mut ws = http_url
            .replace("https://", "wss://")
            .replace("http://", "ws://");
        // Blink uses a different subdomain for WebSocket subscriptions
        ws = ws.replace("api.", "ws.");
        Self {
            url: ws,
            api_key: api_key.to_string(),
            cfg: WSConfig::default(),
        }
    }

    /// Subscribe to real-time payment updates via WebSocket with automatic reconnection
    ///
    /// This method:
    /// 1. Connects to Blink WebSocket GraphQL endpoint
    /// 2. Authenticates with API key
    /// 3. Subscribes to myUpdates for incoming payments
    /// 4. Forwards payment events to the provided channel
    /// 5. Automatically reconnects on failure with exponential backoff
    /// 6. Respects client shutdown when receiver closes
    ///
    /// # Parameters
    /// - `out`: Channel to send payment update JSON objects to
    ///
    /// # Returns
    /// - Ok(JoinHandle): Handle to the background task managing the subscription
    /// - Err: If spawning the task fails
    ///
    /// # Backoff Strategy
    /// - Initial delay: 1 second
    /// - Multiplies by 2 on each failure
    /// - Caps at max_backoff (30 seconds)
    /// - Resets to initial on successful connection
    ///
    /// # Subscription Query
    /// Queries myUpdates and filters for LnUpdate events with payment details:
    /// - status: "PAID", "PENDING", etc.
    /// - settlementAmount: Satoshis received
    /// - paymentHash/paymentRequest: Invoice details
    ///
    /// # GraphQL Transport Protocol
    /// The connection follows graphql-transport-ws protocol:
    /// 1. connection_init: Authenticate with API key
    /// 2. connection_ack: Server confirms connection
    /// 3. subscribe: Send subscription query
    /// 4. next/data: Receive updates
    /// 5. ping/pong: Keep-alive
    #[instrument(skip(self, out), fields(url=%self.url))]
    pub async fn stream_my_updates(
        &self,
        out: tokio::sync::mpsc::Sender<Value>,
    ) -> Result<JoinHandle<()>> {
        let url_str = self.url.clone();
        let api_key = self.api_key.clone();
        let cfg = self.cfg.clone();

        let handle = tokio::spawn(async move {
            let mut backoff = cfg.initial_backoff;
            loop {
                // Check if receiver has been closed (client disconnected)
                if out.is_closed() {
                    debug!("myUpdates stream stopping: receiver closed");
                    break;
                }

                // Build WebSocket request with required headers
                let mut req = match url_str.clone().into_client_request() {
                    Ok(r) => r,
                    Err(e) => {
                        error!(error=?e, "failed to build ws request");
                        sleep(backoff).await;
                        backoff = (backoff * 2).min(cfg.max_backoff);
                        continue;
                    }
                };
                // Set GraphQL subprotocol header for Blink
                req.headers_mut().insert(
                    "Sec-WebSocket-Protocol",
                    HeaderValue::from_static("graphql-transport-ws"),
                );

                match connect_async(req).await {
                    Ok((mut ws, _resp)) => {
                        // Step 1: Send connection_init with API key for authentication
                        if let Err(e) = ws
                            .send(Message::Text(
                                json!({"type":"connection_init","payload":{"X-API-KEY": api_key}})
                                    .to_string(),
                            ))
                            .await
                        {
                            error!(error=?e, "failed to send connection_init");
                            let _ = ws.close(None).await;
                            sleep(backoff).await;
                            backoff = (backoff * 2).min(cfg.max_backoff);
                            continue;
                        }
                        
                        // Step 2: Wait for connection_ack from server
                        let ack_deadline = Instant::now() + cfg.ack_timeout;
                        let mut got_ack = false;
                        while Instant::now() < ack_deadline {
                            match tokio::time::timeout(Duration::from_millis(500), ws.next()).await
                            {
                                Ok(Some(Ok(Message::Text(txt)))) => {
                                    if let Ok(v) = serde_json::from_str::<Value>(&txt) {
                                        // Check for connection_ack message
                                        if v.get("type").and_then(|t| t.as_str())
                                            == Some("connection_ack")
                                        {
                                            got_ack = true;
                                            break;
                                        }
                                    }
                                }
                                // Handle server ping - respond with pong
                                Ok(Some(Ok(Message::Ping(payload)))) => {
                                    let _ = ws.send(Message::Pong(payload)).await;
                                }
                                Ok(Some(Ok(_))) => {}
                                Ok(Some(Err(e))) => {
                                    error!(error=?e, "ws read error awaiting ack");
                                    break;
                                }
                                Ok(None) => {
                                    error!("ws closed before ack");
                                    break;
                                }
                                Err(_) => {}
                            }
                        }
                        
                        if !got_ack {
                            warn!("no connection_ack received");
                            let _ = ws.close(None).await;
                            sleep(backoff).await;
                            backoff = (backoff * 2).min(cfg.max_backoff);
                            continue;
                        }
                        info!("ws connected");

                        // Step 3: Send subscription request for myUpdates
                        let sub_id = format!(
                            "sub-{}",
                            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
                        );
                        // GraphQL subscription to receive incoming payment updates
                        let query = "subscription myUpdates { myUpdates { errors { message } me { id } update { __typename ... on LnUpdate { status transaction { id direction settlementAmount initiationVia { __typename ... on InitiationViaLn { paymentRequest paymentHash } } } } } } }";
                        if let Err(e) = ws.send(Message::Text(json!({"id": sub_id, "type": "subscribe", "payload": {"query": query}}).to_string())).await {
                            error!(error=?e, "failed to subscribe");
                            let _ = ws.close(None).await;
                            sleep(backoff).await; 
                            backoff = (backoff * 2).min(cfg.max_backoff); 
                            continue;
                        }

                        // Reset backoff on successful connection and subscription
                        backoff = cfg.initial_backoff;
                        let mut ping_int = interval(cfg.ping_interval);

                        // Step 4: Main message loop - receive updates and forward to channel
                        loop {
                            tokio::select! {
                                // Send periodic ping messages to keep connection alive
                                _ = ping_int.tick() => {
                                    if let Err(e) = ws.send(Message::Ping(Vec::new())).await { 
                                        warn!(error=?e, "ping failed"); 
                                        break; 
                                    }
                                }
                                // Handle incoming WebSocket messages
                                msg = ws.next() => {
                                    match msg {
                                        Some(Ok(Message::Text(txt))) => {
                                            if let Ok(v) = serde_json::from_str::<Value>(&txt) {
                                                let typ = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
                                                // Forward "next" and "data" messages (subscription events)
                                                if typ == "next" || typ == "data" { 
                                                    let _ = out.send(v).await; 
                                                }
                                                // Break loop on completion or error
                                                else if typ == "complete" || typ == "connection_error" { 
                                                    break; 
                                                }
                                            } else { 
                                                debug!("ws non-json: {}", txt); 
                                            }
                                        }
                                        // Respond to server ping with pong
                                        Some(Ok(Message::Ping(payload))) => { 
                                            let _ = ws.send(Message::Pong(payload)).await; 
                                        }
                                        // Server initiated close
                                        Some(Ok(Message::Close(_))) => { 
                                            warn!("ws close received"); 
                                            break; 
                                        }
                                        Some(Ok(_)) => {}
                                        Some(Err(e)) => { 
                                            error!(error=?e, "ws read error"); 
                                            break; 
                                        }
                                        None => { 
                                            warn!("ws stream ended"); 
                                            break; 
                                        }
                                    }
                                }
                            }
                            // Check if client has closed the receiver
                            if out.is_closed() {
                                let _ = ws.close(None).await;
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        error!(error=?e, "ws connect failed");
                    }
                }
                // Sleep before retry with exponential backoff
                sleep(backoff).await;
                backoff = (backoff * 2).min(cfg.max_backoff);
            }
        });
        Ok(handle)
    }
}
