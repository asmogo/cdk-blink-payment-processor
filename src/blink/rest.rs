use anyhow::{anyhow, Context, Result};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{debug, info, warn, error, instrument};

use crate::settings::Config;

#[derive(Clone)]
pub struct BlinkClient {
    http: Client,
    pub(crate) base_url: Url,
    api_key: String,
    wallet_id: String,
}

impl BlinkClient {
    pub fn new(cfg: &Config) -> Result<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs(15))
            .build()?;
        let base_url = Url::parse(&cfg.blink_api_url)?;
        Ok(Self {
            http,
            base_url,
            api_key: cfg.blink_api_key.clone(),
            wallet_id: cfg.blink_wallet_id.clone(),
        })
    }

    pub fn api_url(&self) -> &Url { &self.base_url }
    pub fn api_key_str(&self) -> &str { &self.api_key }

    #[instrument(skip(self, variables, query), fields(url=%self.base_url))]
    async fn gql<T: for<'de> Deserialize<'de>>(&self, query: &str, variables: Value) -> Result<T> {
        let body = serde_json::json!({
            "query": query,
            "variables": variables,
        });
        info!("blink.gql request sent");
        let res = self
            .http
            .post(self.base_url.clone())
            .header("Content-Type", "application/json")
            .header("X-API-KEY", &self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| { error!(error=?e, "blink.gql network error"); e })?;
        let status = res.status();
        let txt = res.text().await.map_err(|e| { error!(error=?e, "blink.gql read body failed"); e })?;
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
        struct GraphQLError { message: String }
        let parsed: GraphQL<T> = serde_json::from_str(&txt)
            .with_context(|| format!("decode gql response: {}", txt))?;
        if let Some(errs) = parsed.errors.as_ref() {
            if !errs.is_empty() {
                error!(errors=?errs, "blink.gql returned errors");
            }
        }
        parsed.data.ok_or_else(|| anyhow!("missing data"))
    }

    #[instrument(skip(self), fields(url=%self.base_url))]
    pub async fn get_default_wallet(&self) -> Result<Wallet> {
        info!("querying default wallet");
        let q = r#"
        query me { me { defaultAccount { wallets { id walletCurrency balance } } } }
        "#;
        #[derive(Deserialize)]
        struct Resp { me: Me }
        #[derive(Deserialize)]
        struct Me { #[serde(rename = "defaultAccount")] default_account: DefaultAccount }
        #[derive(Deserialize)]
        struct DefaultAccount { wallets: Vec<Wallet> }
        let resp: Resp = self.gql(q, serde_json::json!({})).await?;
        let wallets = resp.me.default_account.wallets;
        info!(count = wallets.len(), "wallets fetched");
        if let Some(w) = wallets.iter().find(|w| w.wallet_currency == "BTC").cloned() {
            info!(wallet_id=%w.id, currency=%w.wallet_currency, "selected BTC wallet");
            Ok(w)
        } else {
            match wallets.into_iter().next() {
                Some(w) => { info!(wallet_id=%w.id, currency=%w.wallet_currency, "selected first wallet"); Ok(w) }
                None => { error!("no wallet found"); Err(anyhow!("no wallet found")) }
            }
        }
    }

    #[instrument(skip(self), fields(amount, memo))]
    pub async fn create_invoice(&self, amount: u64, memo: &str) -> Result<InvoiceDetails> {
        let wallet_id = if self.wallet_id.is_empty() {
            let w = self.get_default_wallet().await?; w.id
        } else {
            self.wallet_id.clone()
        };
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
        struct Resp { #[serde(rename = "lnInvoiceCreate")] ln_invoice_create: LnInvoiceCreate }
        #[derive(Deserialize)]
        struct LnInvoiceCreate { invoice: Option<InvoiceDetails>, errors: Option<Vec<ErrorDetail>> }
        let resp: Resp = self
            .gql(
                q,
                serde_json::json!({
                    "input": {"walletId": wallet_id, "amount": amount, "memo": memo}
                }),
            )
            .await?;
        if let Some(errs) = resp.ln_invoice_create.errors.as_ref() {
            if !errs.is_empty() {
                error!(errors=?errs, "invoice creation errors");
            }
        }
        let inv = resp.ln_invoice_create.invoice.ok_or_else(|| anyhow!("invoice not present"))?;
        info!(hash=%inv.payment_hash, satoshis=inv.satoshis, "invoice created");
        Ok(inv)
    }

    #[instrument(skip(self), fields(bolt11_len = bolt11.len()))]
    pub async fn make_payment(&self, bolt11: &str) -> Result<String> {
        info!("making payment");
        let q = r#"
        mutation lnInvoicePaymentSend($input: LnInvoicePaymentInput!) {
          lnInvoicePaymentSend(input:$input) { status errors { message } }
        }
        "#;
        #[derive(Deserialize)]
        struct Resp { #[serde(rename = "lnInvoicePaymentSend")] ln_invoice_payment_send: PaymentSend }
        #[derive(Deserialize)]
        struct PaymentSend { status: String, errors: Option<Vec<ErrorDetail>> }
        let wallet_id = if self.wallet_id.is_empty() {
            self.get_default_wallet().await?.id
        } else { self.wallet_id.clone() };
        let resp: Resp = self
            .gql(q, serde_json::json!({"input": {"walletId": wallet_id, "paymentRequest": bolt11, "memo": ""}}))
            .await?;
        if let Some(errs) = resp.ln_invoice_payment_send.errors.as_ref() {
            if !errs.is_empty() { error!(errors=?errs, "payment returned errors"); }
        }
        info!(status=%resp.ln_invoice_payment_send.status, "payment done");
        Ok(resp.ln_invoice_payment_send.status)
    }

    #[instrument(skip(self), fields(id))]
    #[instrument(skip(self), fields(req_len = payment_request.len()))]
    pub async fn check_invoice_status_by_request(&self, payment_request: &str) -> Result<(String, String, String)> {
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
        struct Resp { #[serde(rename = "lnInvoicePaymentStatusByPaymentRequest")] by_req: StatusByReq }
        #[derive(Deserialize)]
        struct StatusByReq { status: String, #[serde(rename="paymentHash")] payment_hash: String, #[serde(rename="paymentPreimage")] payment_preimage: Option<String> }
        let resp: Resp = match self
            .gql(q, serde_json::json!({"input": {"paymentRequest": payment_request}}))
            .await
        {
            Ok(r) => r,
            Err(e) => {
                // log, map, or add context
                tracing::error!(error=?e, "failed to query status by paymentRequest");
                return Err(e); // or: return Err(anyhow::anyhow!("status query failed: {e}"));
            }
        };

        let s = resp.by_req;
        let preimage = s.payment_preimage.unwrap_or_default();
        info!(status=%s.status, hash=%s.payment_hash, preimage_len=preimage.len(), "status by request queried");
        Ok((s.status, s.payment_hash, preimage))
    }

    pub async fn get_outgoing_payment(&self, id: &str) -> Result<Payment> {
        info!(%id, "querying outgoing payment by id");
        let q = r#"
        query payment($id: ID!) { payment(id: $id) { id amount createdAt } }
        "#;
        #[derive(Deserialize)]
        struct Resp { payment: Payment }
        let resp: Resp = self.gql(q, serde_json::json!({"id": id})).await?;
        info!(payment_id=%resp.payment.id, amount=resp.payment.amount, "outgoing payment fetched");
        Ok(resp.payment)
    }

    #[instrument(skip(self), fields(hash))]
    pub async fn check_invoice_status_by_hash(&self, payment_hash: &str) -> Result<(String, String, String)> {
        info!(hash=%payment_hash, "checking invoice status by hash");
        let q = r#"
        query ($input: LnInvoicePaymentStatusByHashInput!) {
          lnInvoicePaymentStatusByHash(input: $input) {
            status paymentPreimage paymentRequest
          }
        }
        "#;
        #[derive(Deserialize)]
        struct Resp { #[serde(rename = "lnInvoicePaymentStatusByHash")] ln_invoice_payment_status_by_hash: StatusByHash }
        #[derive(Deserialize)]
        struct StatusByHash { status: String, #[serde(rename="paymentPreimage")] payment_preimage: Option<String>, #[serde(rename="paymentRequest")] payment_request: String }
        let resp: Resp = self
            .gql(q, serde_json::json!({"input": {"paymentHash": payment_hash}}))
            .await?;
        let s = resp.ln_invoice_payment_status_by_hash;
        let preimage = s.payment_preimage.unwrap_or_default();
        info!(status=%s.status, preimage_len=preimage.len(), req_len=s.payment_request.len(), "status queried");
        Ok((s.status, s.payment_request, preimage))
    }

    #[instrument(skip(self), fields(hash))]
    pub async fn check_invoice_status_with_retry(&self, payment_hash: &str) -> Result<(String, String, String)> {
        let mut backoff = initial_backoff();
        let mut attempts = 0u32;
        loop {
            match self.check_invoice_status_by_hash(payment_hash).await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    attempts += 1;
                    warn!(attempts, ?backoff, error=?e, "check status failed, retrying");
                    if attempts > 3 { error!(?e, "giving up after retries"); return Err(e); }
                    sleep(backoff).await;
                    backoff = std::cmp::min(backoff * 2, max_backoff());
                }
            }
        }
    }

    #[instrument(skip(self), fields(offer_len = offer.len()))]
    pub async fn get_offer_quote(&self, offer: &str) -> Result<Quote> {
        info!("getting offer quote");
        let q = r#"
        query quote($offerId: ID!) { quote(offerId: $offerId) { id amount currency expires } }
        "#;
        #[derive(Deserialize)]
        struct Resp { quote: Quote }
        let resp: Resp = self.gql(q, serde_json::json!({"offerId": offer})).await?;
        info!(amount=resp.quote.amount, currency=%resp.quote.currency, "offer quote received");
        Ok(resp.quote)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Wallet { pub id: String, #[serde(rename="walletCurrency")] pub wallet_currency: String, pub balance: i64 }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ErrorDetail { pub message: String }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InvoiceDetails {
    #[serde(rename="paymentRequest")] pub payment_request: String,
    #[serde(rename="paymentHash")] pub payment_hash: String,
    #[serde(rename="paymentSecret")] pub payment_secret: String,
    pub satoshis: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Payment { pub id: String, pub amount: i64, #[serde(rename="createdAt")] pub created_at: String }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Quote { pub id: String, pub amount: i64, pub currency: String, pub expires: String }

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use serde_json::json;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    // no yield_now needed in these tests
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    fn mk_client(server: &MockServer, api_key: &str, wallet_id: &str) -> BlinkClient {
        let cfg = crate::settings::Config {
            blink_api_url: format!("{}/graphql", server.uri()),
            blink_api_key: api_key.to_string(),
            blink_wallet_id: wallet_id.to_string(),
            server_port: 0,
            tls_enable: false,
            tls_cert_path: "".into(),
            tls_key_path: "".into(),
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
                ResponseTemplate::new(500)
                    .set_body_json(json!({"errors":[{"message":"boom"}]})),
            )
            .mount(&server)
            .await;

        let client = mk_client(&server, "k", "w");
        let res: Result<serde_json::Value> = client.gql("query { x }", json!({})).await;
        assert!(
            res.is_err(),
            "non-200 with no data should map to error (missing data)"
        );
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
        assert!(
            res.is_err(),
            "presence of GraphQL errors without data should be an error"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_authorization_header_is_set_if_configured() {
        // Authorization in this client is via X-API-KEY header.
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(header("X-API-KEY", "my-secret"))
            .and(header("Content-Type", "application/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "ok": true }
            })))
            .mount(&server)
            .await;

        #[derive(Deserialize)]
        struct OkResp {
            ok: bool,
        }

        let client = mk_client(&server, "my-secret", "wallet-abc");
        let out: OkResp = client.gql("query { ok }", json!({})).await.expect("gql ok");
        assert!(out.ok);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_create_invoice_success_and_shape() {
        let server = MockServer::start().await;

        // Ensure request carries the mutation and variables with provided walletId/amount/memo
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
    async fn test_make_payment_success_and_error_mapping() {
        let server = MockServer::start().await;

        // Success case
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

        // Error case (GraphQL errors and no data -> gql error)
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

        // Paid
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

        // Pending
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

        // Expired
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

        // PAID
        let (status, req, pre) = client
            .check_invoice_status_by_hash("hash-paid")
            .await
            .expect("paid");
        assert_eq!(status, "PAID");
        assert_eq!(req, "req-paid");
        assert_eq!(pre, "pre");

        // PENDING
        let (status, req, pre) = client
            .check_invoice_status_by_hash("hash-pending")
            .await
            .expect("pending");
        assert_eq!(status, "PENDING");
        assert_eq!(req, "req-pending");
        assert_eq!(pre, "");

        // EXPIRED
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
        // This client retries only on errors (not on "pending" statuses).
        // Simulate two failures (non-200) then a success while driving virtual time.
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

// Backoff configuration separated for testability
#[cfg(test)]
fn initial_backoff() -> Duration { Duration::from_millis(10) }
#[cfg(not(test))]
fn initial_backoff() -> Duration { Duration::from_secs(1) }

#[cfg(test)]
fn max_backoff() -> Duration { Duration::from_millis(100) }
#[cfg(not(test))]
fn max_backoff() -> Duration { Duration::from_secs(30) }

