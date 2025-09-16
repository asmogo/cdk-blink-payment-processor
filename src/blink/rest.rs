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
        let mut backoff = Duration::from_secs(1);
        let mut attempts = 0u32;
        loop {
            match self.check_invoice_status_by_hash(payment_hash).await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    attempts += 1;
                    warn!(attempts, ?backoff, error=?e, "check status failed, retrying");
                    if attempts > 3 { error!(?e, "giving up after retries"); return Err(e); }
                    sleep(backoff).await;
                    backoff = std::cmp::min(backoff * 2, Duration::from_secs(30));
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
