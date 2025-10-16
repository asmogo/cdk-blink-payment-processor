use anyhow::Result;
use async_trait::async_trait;
use lightning_invoice::Bolt11Invoice;
use std::ops::Div;
use tokio::sync::mpsc;
use tonic::{Request, Response, Status};

use crate::blink::rest::BlinkClient;
use crate::blink::ws::WSClient;
use crate::pb::cdk_payment_processor as pb;
use crate::settings::Config;

pub struct PaymentProcessorService {
    cfg: Config,
    blink: BlinkClient,
}

impl PaymentProcessorService {
    pub async fn try_new(cfg: Config) -> Result<Self> {
        let blink = BlinkClient::new(&cfg)?;
        Ok(Self { cfg, blink })
    }
}

#[async_trait]
impl pb::cdk_payment_processor_server::CdkPaymentProcessor for PaymentProcessorService {
    async fn get_settings(
        &self,
        _request: Request<pb::EmptyRequest>,
    ) -> Result<Response<pb::SettingsResponse>, Status> {
        Ok(Response::new(pb::SettingsResponse { inner: "{\"bolt11\":true, \"mpp\":false, \"unit\":\"sat\", \"invoice_description\":false, \"bolt12\":false, \"amountless\":false}".to_string() }))
    }

    async fn create_payment(
        &self,
        request: Request<pb::CreatePaymentRequest>,
    ) -> Result<Response<pb::CreatePaymentResponse>, Status> {
        let req = request.into_inner();
        // Amount from options
        let mut amount: u64 = 0;
        if let Some(opts) = req.options.and_then(|o| o.options) {
            match opts {
                pb::incoming_payment_options::Options::Bolt11(b11) => amount = b11.amount,
                pb::incoming_payment_options::Options::Bolt12(b12) => {
                    amount = b12.amount.unwrap_or_default()
                }
            }
        }
        let inv = self
            .blink
            .create_invoice(amount, &req.unit)
            .await
            .map_err(|e| Status::internal(format!("create invoice failed: {}", e)))?;

        // Align to Go: RequestIdentifier should be PAYMENT_HASH with the invoice's payment hash
        let resp = pb::CreatePaymentResponse {
            request_identifier: Some(pb::PaymentIdentifier {
                r#type: pb::PaymentIdentifierType::PaymentHash as i32,
                value: Some(pb::payment_identifier::Value::Hash(
                    inv.payment_hash.clone(),
                )),
            }),
            request: inv.payment_request.clone(),
            expiry: None,
        };
        Ok(Response::new(resp))
    }

    async fn get_payment_quote(
        &self,
        request: Request<pb::PaymentQuoteRequest>,
    ) -> Result<Response<pb::PaymentQuoteResponse>, Status> {
        let req = request.into_inner();
        let (_st, payment_hash, _preimage) = self
            .blink
            .check_invoice_status_by_request(&req.request)
            .await
            .map_err(|e| {
                Status::internal(format!(
                    "failed to resolve payment hash from invoice: {}",
                    e
                ))
            })?;

        match pb::OutgoingPaymentRequestType::from_i32(req.request_type) {
            Some(pb::OutgoingPaymentRequestType::Bolt11Invoice) => {
                // Decode locally using lightning-invoice
                let inv: Bolt11Invoice = req
                    .request
                    .parse()
                    .map_err(|e| Status::invalid_argument(format!("invalid bolt11: {}", e)))?;
                let amount_msat = inv
                    .amount_milli_satoshis()
                    .ok_or_else(|| Status::invalid_argument("amountless invoice unsupported"))?;
                let amount_sat = amount_msat / 1000;
                let payment_identifier = pb::PaymentIdentifier {
                    r#type: pb::PaymentIdentifierType::PaymentHash as i32,
                    value: Some(pb::payment_identifier::Value::Hash(payment_hash.clone())),
                };
                Ok(Response::new(pb::PaymentQuoteResponse {
                    request_identifier: Some(payment_identifier),
                    amount: amount_sat as u64,
                    fee: 0,
                    state: pb::QuoteState::Unknown as i32,
                    unit: req.unit,
                }))
            }
            Some(pb::OutgoingPaymentRequestType::Bolt12Offer) => {
                let q = self
                    .blink
                    .get_offer_quote(&req.request)
                    .await
                    .map_err(|e| Status::internal(format!("offer quote failed: {}", e)))?;
                Ok(Response::new(pb::PaymentQuoteResponse {
                    request_identifier: None,
                    amount: q.amount as u64,
                    fee: 0,
                    state: pb::QuoteState::Unknown as i32,
                    unit: q.currency,
                }))
            }
            None => Err(Status::invalid_argument("unsupported request type")),
        }
    }

    async fn make_payment(
        &self,
        request: Request<pb::MakePaymentRequest>,
    ) -> Result<Response<pb::MakePaymentResponse>, Status> {
        let req = request.into_inner();
        let mut bolt11 = String::new();

        if let Some(v) = req.payment_options.and_then(|v| v.options) {
            match v {
                pb::outgoing_payment_variant::Options::Bolt11(b11) => {
                    bolt11 = b11.bolt11;
                }
                pb::outgoing_payment_variant::Options::Bolt12(b12) => bolt11 = b12.offer, // simplistic, actual BOLT12 flow differs
            }
        }
        let status = self
            .blink
            .make_payment(&bolt11)
            .await
            .map_err(|e| Status::internal(format!("make payment failed: {}", e)))?;

        // After sending payment, derive a proper PaymentIdentifier:
        // We can derive payment hash from the bolt11 via Blink API (status by request)

        let (_st, payment_hash, _preimage) = self
            .blink
            .check_invoice_status_by_request(&bolt11)
            .await
            .map_err(|e| {
                Status::internal(format!(
                    "failed to resolve payment hash from invoice: {}",
                    e
                ))
            })?;

        let quote_state = match status.as_str() {
            "SUCCESS" => pb::QuoteState::Paid,
            "PENDING" => pb::QuoteState::Pending,
            _ => pb::QuoteState::Unknown,
        } as i32;
        let inv: Bolt11Invoice = bolt11
            .parse()
            .map_err(|e| Status::invalid_argument(format!("invalid bolt11: {}", e)))?;
        let msat = inv.amount_milli_satoshis().ok_or_else(|| 0);

        let resp = pb::MakePaymentResponse {
            payment_identifier: Some(pb::PaymentIdentifier {
                r#type: pb::PaymentIdentifierType::PaymentHash as i32,
                value: Some(pb::payment_identifier::Value::Hash(payment_hash)),
            }),
            payment_proof: Some(status),
            status: quote_state,
            total_spent: msat.unwrap().div(1000),
            unit: "sat".to_string(),
        };
        Ok(Response::new(resp))
    }

    async fn check_incoming_payment(
        &self,
        request: Request<pb::CheckIncomingPaymentRequest>,
    ) -> Result<Response<pb::CheckIncomingPaymentResponse>, Status> {
        let req = request.into_inner();
        let pid = req
            .request_identifier
            .ok_or_else(|| Status::invalid_argument("missing request_identifier"))?;
        let hash = match pid.value {
            Some(pb::payment_identifier::Value::Hash(h)) => h,
            _ => return Err(Status::invalid_argument("expected payment hash")),
        };
        let (status, payment_request, preimage) = self
            .blink
            .check_invoice_status_with_retry(&hash)
            .await
            .map_err(|e| Status::internal(format!("query blink failed: {}", e)))?;
        let mut amount_sat = 0u64;
        if status.eq_ignore_ascii_case("PAID") {
            if let Ok(inv) = payment_request.parse::<Bolt11Invoice>() {
                amount_sat = inv.amount_milli_satoshis().unwrap_or(0) as u64 / 1000;
            }
        }
        let resp = pb::CheckIncomingPaymentResponse {
            payments: vec![pb::WaitIncomingPaymentResponse {
                payment_identifier: Some(pb::PaymentIdentifier {
                    r#type: pb::PaymentIdentifierType::PaymentHash as i32,
                    value: Some(pb::payment_identifier::Value::Hash(hash)),
                }),
                payment_amount: amount_sat,
                unit: "sat".to_string(),
                payment_id: preimage,
            }],
        };
        Ok(Response::new(resp))
    }

    async fn check_outgoing_payment(
        &self,
        request: Request<pb::CheckOutgoingPaymentRequest>,
    ) -> Result<Response<pb::MakePaymentResponse>, Status> {
        let req = request.into_inner();
        let pid = req
            .request_identifier
            .ok_or_else(|| Status::invalid_argument("missing request_identifier"))?;

        // Support both PaymentId and PaymentHash for better compatibility.
        match pid.value {
            Some(pb::payment_identifier::Value::Id(id)) => {
                let payment = self.blink.get_outgoing_payment(&id).await.map_err(|e| {
                    Status::internal(format!("failed to fetch outgoing payment: {}", e))
                })?;
                let resp = pb::MakePaymentResponse {
                    payment_identifier: Some(pb::PaymentIdentifier {
                        r#type: pb::PaymentIdentifierType::PaymentId as i32,
                        value: Some(pb::payment_identifier::Value::Id(payment.id.clone())),
                    }),
                    payment_proof: None,
                    status: pb::QuoteState::Unknown as i32,
                    total_spent: payment.amount as u64,
                    unit: "sat".to_string(),
                };
                Ok(Response::new(resp))
            }
            Some(pb::payment_identifier::Value::Hash(hash)) => {
                // Resolve status by hash; if PAID, we can surface total_spent by re-decoding bolt11 if available
                let (status, payment_request, _preimage) = self
                    .blink
                    .check_invoice_status_with_retry(&hash)
                    .await
                    .map_err(|e| {
                        Status::internal(format!("failed to check payment by hash: {}", e))
                    })?;
                let state = match status.as_str() {
                    "PAID" => pb::QuoteState::Paid,
                    "PENDING" => pb::QuoteState::Pending,
                    "FAILED" => pb::QuoteState::Failed,
                    _ => pb::QuoteState::Unknown,
                } as i32;
                let mut total_spent = 0u64;
                if state == pb::QuoteState::Paid as i32 {
                    if let Ok(inv) = payment_request.parse::<Bolt11Invoice>() {
                        total_spent = inv.amount_milli_satoshis().unwrap_or(0) as u64 / 1000;
                    }
                }
                let resp = pb::MakePaymentResponse {
                    payment_identifier: Some(pb::PaymentIdentifier {
                        r#type: pb::PaymentIdentifierType::PaymentHash as i32,
                        value: Some(pb::payment_identifier::Value::Hash(hash)),
                    }),
                    payment_proof: None,
                    status: state,
                    total_spent,
                    unit: "sat".to_string(),
                };
                Ok(Response::new(resp))
            }
            _ => Err(Status::invalid_argument("expected payment id or hash")),
        }
    }

    type WaitIncomingPaymentStream =
        tokio_stream::wrappers::ReceiverStream<Result<pb::WaitIncomingPaymentResponse, Status>>;

    async fn wait_incoming_payment(
        &self,
        _request: Request<pb::EmptyRequest>,
    ) -> Result<Response<Self::WaitIncomingPaymentStream>, Status> {
        // Application stream to client
        let (app_tx, app_rx) =
            mpsc::channel::<Result<pb::WaitIncomingPaymentResponse, Status>>(100);

        // Internal channel for raw WS JSON
        let (ws_tx, mut ws_rx) = mpsc::channel::<serde_json::Value>(100);

        // Start WS and forward LnUpdate events into ws_tx
        let ws = WSClient::new(self.blink.api_url().as_str(), self.blink.api_key_str());
        let _handle = ws
            .stream_my_updates(ws_tx)
            .await
            .map_err(|e| Status::internal(format!("ws connect failed: {}", e)))?;

        // Transform WS JSON into WaitIncomingPaymentResponse and send over app_tx
        tokio::spawn(async move {
            use pb::{payment_identifier, PaymentIdentifier};
            while let Some(v) = ws_rx.recv().await {
                let maybe_update = v
                    .get("payload")
                    .and_then(|p| p.get("data"))
                    .and_then(|d| d.get("myUpdates"))
                    .and_then(|mu| mu.get("update"));
                if let Some(update) = maybe_update {
                    // Be tolerant: not all payloads include __typename
                    // let typename = update.get("__typename").and_then(|s| s.as_str()).unwrap_or("");
                    // if typename == "LnUpdate" {
                    // Extract fields
                    let status = update.get("status").and_then(|s| s.as_str()).unwrap_or("");

                    // preimage may be absent in some events
                    let preimage = update
                        .get("preimage")
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_string();

                    let txn = update.get("transaction");

                    // settlementAmount is present in the provided payload
                    let settlement_sat = txn
                        .and_then(|t| t.get("settlementAmount"))
                        .and_then(|a| a.as_i64())
                        .unwrap_or(0);

                    // initiationVia is a flat object with paymentHash/paymentRequest in this payload
                    let payment_hash = txn
                        .and_then(|t| t.get("initiationVia"))
                        .and_then(|iv| iv.get("paymentHash").and_then(|h| h.as_str()))
                        .unwrap_or("")
                        .to_string();
                    // has to be PAID status
                    if !status.eq_ignore_ascii_case("PAID") {
                        continue;
                    }
                    if payment_hash.is_empty() {
                        continue;
                    }
                    let resp = pb::WaitIncomingPaymentResponse {
                        payment_identifier: Some(PaymentIdentifier {
                            r#type: pb::PaymentIdentifierType::PaymentHash as i32,
                            value: Some(payment_identifier::Value::Hash(payment_hash)),
                        }),
                        // Use the actual amount from the event
                        payment_amount: settlement_sat.max(0) as u64,
                        unit: "sat".to_string(),
                        // Use preimage if present; empty string otherwise
                        payment_id: preimage,
                    };
                    let _ = app_tx.send(Ok(resp)).await;
                    // }
                }
            }
        });

        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            app_rx,
        )))
    }
}
