//! Blink GraphQL API client.
//!
//! Handles invoice creation, outgoing payments, fee probing, and status
//! queries against Blink's GraphQL endpoint.

use anyhow::{anyhow, Context, Result};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{debug, error, info, instrument, warn};

use crate::settings::BackendConfig;

/// HTTP client for the Blink GraphQL API
#[derive(Clone)]
pub struct BlinkClient {
    http: Client,
    pub(crate) base_url: Url,
    api_key: String,
    wallet_id: String,
}

impl BlinkClient {
    /// Create a new BlinkClient from backend configuration
    pub fn new(cfg: &BackendConfig) -> Result<Self> {
        let http = Client::builder().timeout(Duration::from_secs(15)).build()?;
        let base_url = Url::parse(&cfg.api_url)?;
        Ok(Self {
            http,
            base_url,
            api_key: cfg.api_key.clone(),
            wallet_id: cfg.wallet_id.clone(),
        })
    }

    /// Get the Blink API URL
    pub fn api_url(&self) -> &Url {
        &self.base_url
    }

    /// Get the Blink API key
    pub fn api_key_str(&self) -> &str {
        &self.api_key
    }

    /// Override the wallet id (used after resolving the default wallet)
    pub fn set_wallet_id(&mut self, wallet_id: String) {
        self.wallet_id = wallet_id;
    }

    /// Send a GraphQL query to Blink and deserialize the `data` field
    #[instrument(skip(self, variables, query), fields(url=%self.base_url))]
    async fn gql<T: for<'de> Deserialize<'de>>(&self, query: &str, variables: Value) -> Result<T> {
        let body = serde_json::json!({
            "query": query,
            "variables": variables,
        });
        debug!("blink.gql request sent");
        let res = self
            .http
            .post(self.base_url.clone())
            .header("Content-Type", "application/json")
            .header("X-API-KEY", &self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                error!(error=?e, "blink.gql network error");
                e
            })?;
        let status = res.status();
        let txt = res.text().await.map_err(|e| {
            error!(error=?e, "blink.gql read body failed");
            e
        })?;
        debug!(?status, body_len = txt.len(), "blink.gql response received");
        if !status.is_success() {
            error!(?status, body_snippet = %txt.chars().take(200).collect::<String>(), "blink.gql non-200 status");
        }
        #[derive(Deserialize)]
        struct GraphQL<T> {
            data: Option<T>,
            errors: Option<Vec<GraphQLError>>,
        }
        #[derive(Deserialize, Debug)]
        struct GraphQLError {
            message: String,
        }
        let parsed: GraphQL<T> =
            serde_json::from_str(&txt).with_context(|| format!("decode gql response: {}", txt))?;
        match parsed.data {
            Some(data) => {
                if let Some(errs) = parsed.errors.as_ref().filter(|e| !e.is_empty()) {
                    warn!(errors=?errs, "blink.gql returned partial errors");
                }
                Ok(data)
            }
            None => {
                let messages = parsed
                    .errors
                    .unwrap_or_default()
                    .into_iter()
                    .map(|e| e.message)
                    .collect::<Vec<_>>()
                    .join("; ");
                if messages.is_empty() {
                    Err(anyhow!("missing data"))
                } else {
                    Err(anyhow!(messages))
                }
            }
        }
    }

    /// Get the default BTC wallet of the authenticated account
    #[instrument(skip(self), fields(url=%self.base_url))]
    pub async fn get_default_wallet(&self) -> Result<Wallet> {
        info!("querying default wallet");
        let q = r#"
        query me { me { defaultAccount { wallets { id walletCurrency balance } } } }
        "#;
        #[derive(Deserialize)]
        struct Resp {
            me: Me,
        }
        #[derive(Deserialize)]
        struct Me {
            #[serde(rename = "defaultAccount")]
            default_account: DefaultAccount,
        }
        #[derive(Deserialize)]
        struct DefaultAccount {
            wallets: Vec<Wallet>,
        }
        let resp: Resp = self.gql(q, serde_json::json!({})).await?;
        let wallets = resp.me.default_account.wallets;
        info!(count = wallets.len(), "wallets fetched");
        if let Some(w) = wallets.iter().find(|w| w.wallet_currency == "BTC").cloned() {
            info!(wallet_id=%w.id, currency=%w.wallet_currency, "selected BTC wallet");
            Ok(w)
        } else {
            match wallets.into_iter().next() {
                Some(w) => {
                    info!(wallet_id=%w.id, currency=%w.wallet_currency, "selected first wallet");
                    Ok(w)
                }
                None => Err(anyhow!("no wallet found")),
            }
        }
    }

    /// Resolve the configured wallet id, querying the default wallet if unset
    pub async fn resolve_wallet_id(&self) -> Result<String> {
        if !self.wallet_id.is_empty() {
            return Ok(self.wallet_id.clone());
        }
        Ok(self.get_default_wallet().await?.id)
    }

    /// Create a BOLT11 invoice for receiving Lightning payments
    ///
    /// Returns the invoice details including the BOLT11 payment request and
    /// its payment hash.
    #[instrument(skip(self), fields(amount, memo))]
    pub async fn create_invoice(&self, amount: u64, memo: &str) -> Result<InvoiceDetails> {
        let wallet_id = self.resolve_wallet_id().await?;
        info!(%wallet_id, amount, memo, "creating invoice");
        let q = r#"
        mutation LnInvoiceCreate($input: LnInvoiceCreateInput!) {
          lnInvoiceCreate(input: $input) {
            invoice { paymentRequest paymentHash paymentSecret satoshis }
            errors { message }
          }
        }
        "#;
        #[derive(Deserialize)]
        struct Resp {
            #[serde(rename = "lnInvoiceCreate")]
            ln_invoice_create: LnInvoiceCreate,
        }
        #[derive(Deserialize)]
        struct LnInvoiceCreate {
            invoice: Option<InvoiceDetails>,
            errors: Option<Vec<ErrorDetail>>,
        }
        let resp: Resp = self
            .gql(
                q,
                serde_json::json!({
                    "input": {"walletId": wallet_id, "amount": amount, "memo": memo}
                }),
            )
            .await?;
        if let Some(errs) = resp
            .ln_invoice_create
            .errors
            .as_ref()
            .filter(|e| !e.is_empty())
        {
            let messages = errs
                .iter()
                .map(|e| e.message.as_str())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(anyhow!(messages));
        }
        let inv = resp
            .ln_invoice_create
            .invoice
            .ok_or_else(|| anyhow!("invoice not present"))?;
        info!(hash=%inv.payment_hash, satoshis=inv.satoshis, "invoice created");
        Ok(inv)
    }

    /// Probe the routing fee for paying a BOLT11 invoice, in satoshis
    #[instrument(skip(self), fields(bolt11_len = bolt11.len()))]
    pub async fn probe_fee(&self, bolt11: &str) -> Result<u64> {
        let wallet_id = self.resolve_wallet_id().await?;
        let q = r#"
        mutation lnInvoiceFeeProbe($input: LnInvoiceFeeProbeInput!) {
          lnInvoiceFeeProbe(input: $input) { amount errors { message } }
        }
        "#;
        #[derive(Deserialize)]
        struct Resp {
            #[serde(rename = "lnInvoiceFeeProbe")]
            ln_invoice_fee_probe: FeeProbe,
        }
        #[derive(Deserialize)]
        struct FeeProbe {
            amount: Option<i64>,
            errors: Option<Vec<ErrorDetail>>,
        }
        let resp: Resp = self
            .gql(
                q,
                serde_json::json!({"input": {"walletId": wallet_id, "paymentRequest": bolt11}}),
            )
            .await?;
        if let Some(errs) = resp
            .ln_invoice_fee_probe
            .errors
            .as_ref()
            .filter(|e| !e.is_empty())
        {
            let messages = errs
                .iter()
                .map(|e| e.message.as_str())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(anyhow!(messages));
        }
        let amount = resp
            .ln_invoice_fee_probe
            .amount
            .ok_or_else(|| anyhow!("fee probe amount not present"))?;
        Ok(amount.max(0) as u64)
    }

    /// Send a Lightning payment for a BOLT11 invoice
    ///
    /// Returns the payment status as reported by Blink
    /// ("SUCCESS", "PENDING", or "FAILURE").
    #[instrument(skip(self), fields(bolt11_len = bolt11.len()))]
    pub async fn make_payment(&self, bolt11: &str) -> Result<String> {
        info!("making payment");
        let q = r#"
        mutation lnInvoicePaymentSend($input: LnInvoicePaymentInput!) {
          lnInvoicePaymentSend(input:$input) { status errors { message } }
        }
        "#;
        #[derive(Deserialize)]
        struct Resp {
            #[serde(rename = "lnInvoicePaymentSend")]
            ln_invoice_payment_send: PaymentSend,
        }
        #[derive(Deserialize)]
        struct PaymentSend {
            status: String,
            errors: Option<Vec<ErrorDetail>>,
        }
        let wallet_id = self.resolve_wallet_id().await?;
        let resp: Resp = self
            .gql(q, serde_json::json!({"input": {"walletId": wallet_id, "paymentRequest": bolt11, "memo": ""}}))
            .await?;
        if let Some(errs) = resp
            .ln_invoice_payment_send
            .errors
            .as_ref()
            .filter(|e| !e.is_empty())
        {
            let messages = errs
                .iter()
                .map(|e| e.message.as_str())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(anyhow!(messages));
        }
        info!(status=%resp.ln_invoice_payment_send.status, "payment done");
        Ok(resp.ln_invoice_payment_send.status)
    }

    /// Query the status of a payment by its BOLT11 payment request.
    ///
    /// Returns `(status, payment_hash, payment_preimage)`.
    #[instrument(skip(self), fields(req_len = payment_request.len()))]
    pub async fn check_invoice_status_by_request(
        &self,
        payment_request: &str,
    ) -> Result<(String, String, String)> {
        info!("checking invoice status by request");
        let q = r#"
        query LnInvoicePaymentStatusByPaymentRequest($input: LnInvoicePaymentStatusByPaymentRequestInput!) {
            lnInvoicePaymentStatusByPaymentRequest(input: $input) {
                status
                paymentHash
                paymentPreimage
            }
        }
        "#;
        #[derive(Deserialize)]
        struct Resp {
            #[serde(rename = "lnInvoicePaymentStatusByPaymentRequest")]
            by_req: StatusByReq,
        }
        #[derive(Deserialize)]
        struct StatusByReq {
            status: String,
            #[serde(rename = "paymentHash")]
            payment_hash: String,
            #[serde(rename = "paymentPreimage")]
            payment_preimage: Option<String>,
        }
        let resp: Resp = self
            .gql(
                q,
                serde_json::json!({"input": {"paymentRequest": payment_request}}),
            )
            .await?;
        let s = resp.by_req;
        let preimage = s.payment_preimage.unwrap_or_default();
        info!(status=%s.status, hash=%s.payment_hash, "status by request queried");
        Ok((s.status, s.payment_hash, preimage))
    }

    /// Query the status of a payment by its payment hash.
    ///
    /// Returns `(status, payment_request, payment_preimage)`.
    #[instrument(skip(self), fields(hash))]
    pub async fn check_invoice_status_by_hash(
        &self,
        payment_hash: &str,
    ) -> Result<(String, String, String)> {
        info!(hash=%payment_hash, "checking invoice status by hash");
        let q = r#"
        query ($input: LnInvoicePaymentStatusByHashInput!) {
          lnInvoicePaymentStatusByHash(input: $input) {
            status paymentPreimage paymentRequest
          }
        }
        "#;
        #[derive(Deserialize)]
        struct Resp {
            #[serde(rename = "lnInvoicePaymentStatusByHash")]
            ln_invoice_payment_status_by_hash: StatusByHash,
        }
        #[derive(Deserialize)]
        struct StatusByHash {
            status: String,
            #[serde(rename = "paymentPreimage")]
            payment_preimage: Option<String>,
            #[serde(rename = "paymentRequest")]
            payment_request: String,
        }
        let resp: Resp = self
            .gql(
                q,
                serde_json::json!({"input": {"paymentHash": payment_hash}}),
            )
            .await?;
        let s = resp.ln_invoice_payment_status_by_hash;
        let preimage = s.payment_preimage.unwrap_or_default();
        info!(status=%s.status, "status by hash queried");
        Ok((s.status, s.payment_request, preimage))
    }

    /// Query payment status by hash with exponential backoff retries
    pub async fn check_invoice_status_with_retry(
        &self,
        payment_hash: &str,
    ) -> Result<(String, String, String)> {
        let mut backoff = initial_backoff();
        let mut attempts = 0u32;
        loop {
            match self.check_invoice_status_by_hash(payment_hash).await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    attempts += 1;
                    warn!(attempts, ?backoff, error=?e, "check status failed, retrying");
                    if attempts > 3 {
                        error!(?e, "giving up after retries");
                        return Err(e);
                    }
                    sleep(backoff).await;
                    backoff = std::cmp::min(backoff * 2, max_backoff());
                }
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Wallet {
    pub id: String,
    #[serde(rename = "walletCurrency")]
    pub wallet_currency: String,
    pub balance: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ErrorDetail {
    pub message: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InvoiceDetails {
    #[serde(rename = "paymentRequest")]
    pub payment_request: String,
    #[serde(rename = "paymentHash")]
    pub payment_hash: String,
    #[serde(rename = "paymentSecret")]
    pub payment_secret: String,
    pub satoshis: i64,
}

#[cfg(test)]
fn initial_backoff() -> Duration {
    Duration::from_millis(10)
}
#[cfg(not(test))]
fn initial_backoff() -> Duration {
    Duration::from_secs(1)
}

#[cfg(test)]
fn max_backoff() -> Duration {
    Duration::from_millis(100)
}
#[cfg(not(test))]
fn max_backoff() -> Duration {
    Duration::from_secs(30)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use serde_json::json;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    fn mk_client(server: &MockServer, api_key: &str, wallet_id: &str) -> BlinkClient {
        let cfg = BackendConfig {
            api_url: format!("{}/graphql", server.uri()),
            api_key: api_key.to_string(),
            wallet_id: wallet_id.to_string(),
        };
        BlinkClient::new(&cfg).expect("blink client")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_gql_success_returns_data() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(header("Content-Type", "application/json"))
            .and(header("X-API-KEY", "secret-key"))
            .and(body_string_contains("\"query\""))
            .and(body_string_contains("\"variables\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "ok": true }
            })))
            .mount(&server)
            .await;

        #[derive(Deserialize)]
        struct OkResp {
            ok: bool,
        }

        let client = mk_client(&server, "secret-key", "wallet-1");
        let out: OkResp = client.gql("query { ok }", json!({})).await.expect("gql ok");
        assert!(out.ok, "should parse and return data.ok=true");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_gql_http_non_200_is_error() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(
                ResponseTemplate::new(500).set_body_json(json!({"errors":[{"message":"boom"}]})),
            )
            .mount(&server)
            .await;

        let client = mk_client(&server, "k", "w");
        let res: Result<serde_json::Value> = client.gql("query { x }", json!({})).await;
        assert!(res.is_err(), "non-200 with no data should map to error");
        assert!(res.unwrap_err().to_string().contains("boom"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_gql_graphql_errors_field_is_error() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "errors":[{"message":"boom"}]
            })))
            .mount(&server)
            .await;

        let client = mk_client(&server, "k", "w");
        let res: Result<serde_json::Value> = client.gql("query { x }", json!({})).await;
        assert!(res.is_err(), "GraphQL errors without data should be an error");
        assert!(res.unwrap_err().to_string().contains("boom"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_create_invoice_success_and_shape() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("mutation LnInvoiceCreate"))
            .and(body_string_contains("wallet-123"))
            .and(body_string_contains("\"amount\""))
            .and(body_string_contains("\"memo\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "lnInvoiceCreate": {
                        "invoice": {
                            "paymentRequest": "lnbc1...",
                            "paymentHash": "abc123",
                            "paymentSecret": "s3cr3t",
                            "satoshis": 4242
                        },
                        "errors": []
                    }
                }
            })))
            .mount(&server)
            .await;

        let client = mk_client(&server, "key", "wallet-123");
        let inv = client.create_invoice(4242, "hello").await.expect("invoice");
        assert_eq!(inv.payment_request, "lnbc1...");
        assert_eq!(inv.payment_hash, "abc123");
        assert_eq!(inv.payment_secret, "s3cr3t");
        assert_eq!(inv.satoshis, 4242);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_create_invoice_mutation_errors_are_returned() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "lnInvoiceCreate": {
                        "invoice": null,
                        "errors": [{"message": "amount too small"}]
                    }
                }
            })))
            .mount(&server)
            .await;

        let client = mk_client(&server, "key", "wallet-123");
        let res = client.create_invoice(1, "hello").await;
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("amount too small"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_probe_fee_success() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("mutation lnInvoiceFeeProbe"))
            .and(body_string_contains("lnbc1test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "lnInvoiceFeeProbe": { "amount": 3, "errors": [] }
                }
            })))
            .mount(&server)
            .await;

        let client = mk_client(&server, "key", "wallet-xyz");
        let fee = client.probe_fee("lnbc1test").await.expect("fee");
        assert_eq!(fee, 3);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_make_payment_success_and_error_mapping() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("mutation lnInvoicePaymentSend"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "lnInvoicePaymentSend": { "status":"SUCCESS", "errors":[] }
                }
            })))
            .mount(&server)
            .await;

        let client = mk_client(&server, "key", "wallet-xyz");
        let status = client.make_payment("bolt11-xxx").await.expect("payment ok");
        assert_eq!(status, "SUCCESS");

        let server_err = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "errors":[{"message":"payment failed"}]
            })))
            .mount(&server_err)
            .await;

        let client_err = mk_client(&server_err, "key", "wallet-xyz");
        let res = client_err.make_payment("bolt11-yyy").await;
        assert!(res.is_err(), "GraphQL errors without data should yield error");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_check_invoice_status_variants() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("hash-paid"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "lnInvoicePaymentStatusByHash": {
                        "status":"PAID",
                        "paymentPreimage":"pre",
                        "paymentRequest":"req-paid"
                    }
                }
            })))
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("hash-pending"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "lnInvoicePaymentStatusByHash": {
                        "status":"PENDING",
                        "paymentPreimage":null,
                        "paymentRequest":"req-pending"
                    }
                }
            })))
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("hash-expired"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "lnInvoicePaymentStatusByHash": {
                        "status":"EXPIRED",
                        "paymentPreimage":null,
                        "paymentRequest":"req-expired"
                    }
                }
            })))
            .mount(&server)
            .await;

        let client = mk_client(&server, "k", "w");

        let (status, req, pre) = client
            .check_invoice_status_by_hash("hash-paid")
            .await
            .expect("paid");
        assert_eq!(status, "PAID");
        assert_eq!(req, "req-paid");
        assert_eq!(pre, "pre");

        let (status, req, pre) = client
            .check_invoice_status_by_hash("hash-pending")
            .await
            .expect("pending");
        assert_eq!(status, "PENDING");
        assert_eq!(req, "req-pending");
        assert_eq!(pre, "");

        let (status, req, pre) = client
            .check_invoice_status_by_hash("hash-expired")
            .await
            .expect("expired");
        assert_eq!(status, "EXPIRED");
        assert_eq!(req, "req-expired");
        assert_eq!(pre, "");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_check_invoice_status_with_retry_respects_backoff() {
        let server = MockServer::start().await;

        let counter = Arc::new(AtomicUsize::new(0));
        let c2 = counter.clone();

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(move |_req: &Request| {
                let n = c2.fetch_add(1, Ordering::SeqCst) + 1;
                if n <= 2 {
                    ResponseTemplate::new(500).set_body_json(json!({
                        "errors":[{"message": format!("fail {}", n)}]
                    }))
                } else {
                    ResponseTemplate::new(200).set_body_json(json!({
                        "data": {
                            "lnInvoicePaymentStatusByHash": {
                                "status":"PAID",
                                "paymentPreimage":"pre",
                                "paymentRequest":"req"
                            }
                        }
                    }))
                }
            })
            .mount(&server)
            .await;

        let client = mk_client(&server, "k", "w");

        let task = tokio::spawn(async move {
            client
                .check_invoice_status_with_retry("any-hash")
                .await
                .expect("eventual success")
        });

        let (_status, req, pre) = task.await.expect("join");
        assert_eq!(req, "req");
        assert_eq!(pre, "pre");
        assert_eq!(
            counter.load(Ordering::SeqCst),
            3,
            "should have performed 3 total requests (2 fails + 1 success)"
        );
    }
}
