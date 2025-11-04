//! gRPC Service Implementation for CDK Payment Processor
//!
//! This module implements the `CdkPaymentProcessor` gRPC service that bridges CDK payment
//! processors to Blink's Lightning Network APIs. It handles:
//! - Creating incoming payments (BOLT11 invoices)
//! - Getting payment quotes for outgoing payments
//! - Sending outgoing payments (Lightning transactions)
//! - Checking payment status (incoming and outgoing)
//! - Streaming real-time payment updates via WebSocket

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

/// Main service struct implementing the `CdkPaymentProcessor` gRPC service
/// 
/// # Fields
/// - `cfg`: Configuration containing API keys and server settings
/// - `blink`: HTTP client for communicating with Blink GraphQL API
pub struct PaymentProcessorService {
    cfg: Config,
    blink: BlinkClient,
}

impl PaymentProcessorService {
    /// Create a new service instance and validate Blink API connectivity
    ///
    /// # Errors
    /// Returns error if BlinkClient initialization fails (e.g., invalid API key or network issues)
    pub async fn try_new(cfg: Config) -> Result<Self> {
        let blink = BlinkClient::new(&cfg)?;
        Ok(Self { cfg, blink })
    }
}

#[async_trait]
impl pb::cdk_payment_processor_server::CdkPaymentProcessor for PaymentProcessorService {
    /// GetSettings - Returns supported payment methods and service capabilities
    ///
    /// This method is called by CDK mints during startup to discover what payment methods
    /// this processor supports. The response is a JSON string detailing capabilities.
    ///
    /// # Returns
    /// JSON string with fields:
    /// - `bolt11`: true - BOLT11 invoices are supported
    /// - `mpp`: false - Multi-part payments not supported
    /// - `unit`: "sat" - Unit is satoshis
    /// - `invoice_description`: false - Invoice descriptions not required
    /// - `bolt12`: false - BOLT12 offers not fully supported
    /// - `amountless`: false - Amountless invoices not supported
    async fn get_settings(
        &self,
        _request: Request<pb::EmptyRequest>,
    ) -> Result<Response<pb::SettingsResponse>, Status> {
        Ok(Response::new(pb::SettingsResponse { inner: "{\"bolt11\":true, \"mpp\":false, \"unit\":\"sat\", \"invoice_description\":false, \"bolt12\":false, \"amountless\":false}".to_string() }))
    }

    /// CreatePayment - Create an incoming payment (invoice) for receiving funds
    ///
    /// This RPC creates a BOLT11 invoice on the Blink network that can be used to receive
    /// Lightning Network payments. The invoice is generated with the requested amount.
    ///
    /// # Request
    /// - `unit`: Currency unit (typically "sat" for satoshis)
    /// - `options`: Nested union with either Bolt11 or Bolt12 options
    ///   - `bolt11.amount`: Amount in satoshis
    ///
    /// # Returns
    /// - `request_identifier`: Payment hash for tracking this invoice
    /// - `request`: BOLT11 invoice string (can be shared with payers)
    /// - `expiry`: Optional expiration timestamp
    ///
    /// # Example Flow
    /// 1. CDK mint calls this with amount 1000 satoshis
    /// 2. We call Blink API to create invoice
    /// 3. Return the invoice string and its payment hash
    /// 4. Payer scans the invoice QR code and sends payment
    /// 5. Payment status is tracked via WaitIncomingPayment stream
    async fn create_payment(
        &self,
        request: Request<pb::CreatePaymentRequest>,
    ) -> Result<Response<pb::CreatePaymentResponse>, Status> {
        let req = request.into_inner();
        // Extract amount from the nested payment options (BOLT11 or BOLT12)
        let mut amount: u64 = 0;
        if let Some(opts) = req.options.and_then(|o| o.options) {
            match opts {
                pb::incoming_payment_options::Options::Bolt11(b11) => amount = b11.amount,
                pb::incoming_payment_options::Options::Bolt12(b12) => {
                    amount = b12.amount.unwrap_or_default()
                }
            }
        }
        // Call Blink API to create the invoice on the Lightning Network
        let inv = self
            .blink
            .create_invoice(amount, &req.unit)
            .await
            .map_err(|e| Status::internal(format!("create invoice failed: {}", e)))?;

        // Build response with the invoice and its payment hash
        // The payment hash is used later to track when this invoice is paid
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

    /// GetPaymentQuote - Get a quote for paying an outgoing invoice or offer
    ///
    /// Before sending a payment, the client should call this to get a quote showing the
    /// actual amount and fees that will be charged. This allows the client to verify the
    /// payment cost before committing.
    ///
    /// # Request
    /// - `request`: BOLT11 invoice string or BOLT12 offer
    /// - `unit`: Currency unit (typically "sat")
    /// - `request_type`: Type of request (BOLT11_INVOICE = 0, BOLT12_OFFER = 1)
    ///
    /// # Returns
    /// - `request_identifier`: Payment hash extracted from the invoice
    /// - `amount`: Amount in satoshis
    /// - `fee`: Routing fees (currently returns 0)
    /// - `state`: Payment state (UNKNOWN for quotes)
    /// - `unit`: Currency unit as requested
    ///
    /// # Example
    /// Client has an invoice "lnbc1000..." and wants to send it:
    /// 1. Call GetPaymentQuote to see exact cost
    /// 2. User confirms the amount is acceptable
    /// 3. Call MakePayment to actually send it
    async fn get_payment_quote(
        &self,
        request: Request<pb::PaymentQuoteRequest>,
    ) -> Result<Response<pb::PaymentQuoteResponse>, Status> {
        let req = request.into_inner();
        // Query Blink to extract payment hash from the invoice string
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
                // Parse and decode the BOLT11 invoice locally to extract amount
                let inv: Bolt11Invoice = req
                    .request
                    .parse()
                    .map_err(|e| Status::invalid_argument(format!("invalid bolt11: {}", e)))?;
                // BOLT11 amounts are in millisatoshis; convert to satoshis
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
                // BOLT12 support is partial - fetch quote from Blink
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

    /// MakePayment - Send an outgoing Lightning Network payment
    ///
    /// This RPC sends an actual Lightning Network payment through Blink. It executes the
    /// payment and returns the status immediately. The payment may continue in the background
    /// if it's still routing.
    ///
    /// # Request
    /// - `payment_options`: Nested union with either Bolt11 or Bolt12 options
    ///   - `bolt11.bolt11`: BOLT11 invoice string to pay
    ///   - `bolt11.max_fee_amount`: Maximum routing fee allowed (satoshis)
    ///   - `bolt11.timeout_secs`: Payment timeout in seconds
    ///
    /// # Returns
    /// - `payment_identifier`: Payment hash for tracking
    /// - `payment_proof`: Status response from Blink (SUCCESS, PENDING, FAILED)
    /// - `status`: Enum status (PAID, PENDING, FAILED, UNKNOWN)
    /// - `total_spent`: Total satoshis spent including fees
    /// - `unit`: "sat" for satoshis
    ///
    /// # Important Notes
    /// - Payment status is returned immediately, but payment routing may still be in progress
    /// - Use CheckOutgoingPayment to query the status later
    /// - Errors are returned as gRPC errors with detailed messages
    async fn make_payment(
        &self,
        request: Request<pb::MakePaymentRequest>,
    ) -> Result<Response<pb::MakePaymentResponse>, Status> {
        let req = request.into_inner();
        // Extract invoice string from the nested payment options
        let mut bolt11 = String::new();

        if let Some(v) = req.payment_options.and_then(|v| v.options) {
            match v {
                pb::outgoing_payment_variant::Options::Bolt11(b11) => {
                    bolt11 = b11.bolt11;
                }
                pb::outgoing_payment_variant::Options::Bolt12(b12) => {
                    // BOLT12 support is simplified; treat offer as bolt11 for now
                    bolt11 = b12.offer;
                }
            }
        }
        // Send the payment through Blink - this performs the actual Lightning transaction
        let status = self
            .blink
            .make_payment(&bolt11)
            .await
            .map_err(|e| Status::internal(format!("make payment failed: {}", e)))?;

        // Query Blink to extract payment hash from the invoice for response
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

        // Map Blink API status strings to our protobuf QuoteState enum
        let quote_state = match status.as_str() {
            "SUCCESS" => pb::QuoteState::Paid,
            "PENDING" => pb::QuoteState::Pending,
            _ => pb::QuoteState::Unknown,
        } as i32;
        // Decode the invoice to extract the amount
        let inv: Bolt11Invoice = bolt11
            .parse()
            .map_err(|e| Status::invalid_argument(format!("invalid bolt11: {}", e)))?;
        let msat = inv.amount_milli_satoshis().ok_or_else(|| 0);

        // Build response with payment status and amount
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

    /// CheckIncomingPayment - Query the status of an incoming payment by payment hash
    ///
    /// This is a polling-based status check for an invoice. It returns whether the invoice
    /// has been paid, and if so, the payment amount and preimage.
    ///
    /// # Request
    /// - `request_identifier`: PaymentIdentifier with PAYMENT_HASH type and hash value
    ///   - The hash is obtained from CreatePayment response
    ///
    /// # Returns
    /// - `payments`: Vector of payment responses (typically 0 or 1 element)
    ///   - Empty if invoice not yet paid
    ///   - Contains payment details if PAID
    ///   - `payment_amount`: Amount received in satoshis
    ///   - `payment_id`: Preimage (can be empty)
    ///
    /// # Polling Strategy
    /// CDK mints typically poll this endpoint periodically to check if invoices have been paid.
    /// For real-time updates, use WaitIncomingPayment (streaming) instead.
    ///
    /// # Example
    /// 1. CDK created invoice, got payment hash "abc123"
    /// 2. Poll CheckIncomingPayment with hash "abc123" every 1-2 seconds
    /// 3. When payments array is non-empty, invoice has been paid
    async fn check_incoming_payment(
        &self,
        request: Request<pb::CheckIncomingPaymentRequest>,
    ) -> Result<Response<pb::CheckIncomingPaymentResponse>, Status> {
        let req = request.into_inner();
        // Extract the payment hash from the request identifier
        let pid = req
            .request_identifier
            .ok_or_else(|| Status::invalid_argument("missing request_identifier"))?;
        let hash = match pid.value {
            Some(pb::payment_identifier::Value::Hash(h)) => h,
            _ => return Err(Status::invalid_argument("expected payment hash")),
        };
        // Query Blink API for the status of this invoice with retry logic
        let (status, payment_request, preimage) = self
            .blink
            .check_invoice_status_with_retry(&hash)
            .await
            .map_err(|e| Status::internal(format!("query blink failed: {}", e)))?;
        
        // Only return payment info if status is PAID
        let mut amount_sat = 0u64;
        if status.eq_ignore_ascii_case("PAID") {
            // Parse the invoice to extract amount
            if let Ok(inv) = payment_request.parse::<Bolt11Invoice>() {
                amount_sat = inv.amount_milli_satoshis().unwrap_or(0) as u64 / 1000;
            }
        }
        // Build response with payment details
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

    /// WaitIncomingPayment - Server-streaming RPC for real-time payment updates
    ///
    /// This is the primary method for CDK mints to receive real-time notifications of incoming
    /// payments. It establishes a long-lived gRPC stream and a WebSocket connection to Blink,
    /// forwarding all incoming payment updates as they occur.
    ///
    /// # Behavior
    /// - Opens a WebSocket connection to Blink's GraphQL subscription endpoint
    /// - Subscribes to "myUpdates" events (payment notifications)
    /// - Filters for PAID status updates only
    /// - Streams each update to the client as a gRPC message
    /// - Automatically reconnects if WebSocket connection drops (see ws.rs for details)
    ///
    /// # Returns
    /// Server stream of WaitIncomingPaymentResponse messages:
    /// - `payment_identifier`: Payment hash of the incoming payment
    /// - `payment_amount`: Amount received in satoshis
    /// - `unit`: "sat" for satoshis
    /// - `payment_id`: Preimage (payment proof)
    ///
    /// # Connection Lifecycle
    /// 1. Client calls WaitIncomingPayment
    /// 2. We establish WebSocket to Blink
    /// 3. Subscribe to payment updates
    /// 4. For each payment received:
    ///    - WebSocket receives JSON event
    ///    - We parse and validate it
    ///    - Send to gRPC client via stream
    /// 5. Stream continues until client disconnects or error occurs
    ///
    /// # Example Usage
    /// ```bash
    /// grpcurl -plaintext -d '{}' localhost:50051 \
    ///   cdk_payment_processor.CdkPaymentProcessor/WaitIncomingPayment
    /// ```
    async fn wait_incoming_payment(
        &self,
        _request: Request<pb::EmptyRequest>,
    ) -> Result<Response<Self::WaitIncomingPaymentStream>, Status> {
        // Channel 1: For sending processed payment responses to gRPC client
        let (app_tx, app_rx) =
            mpsc::channel::<Result<pb::WaitIncomingPaymentResponse, Status>>(100);

        // Channel 2: For raw WebSocket JSON events from the subscription
        let (ws_tx, mut ws_rx) = mpsc::channel::<serde_json::Value>(100);

        // Connect to Blink WebSocket and start streaming updates
        let ws = WSClient::new(self.blink.api_url().as_str(), self.blink.api_key_str());
        let _handle = ws
            .stream_my_updates(ws_tx)
            .await
            .map_err(|e| Status::internal(format!("ws connect failed: {}", e)))?;

        // Spawn background task to transform WebSocket JSON into gRPC messages
        // This task runs independently and forwards updates to the client
        tokio::spawn(async move {
            use pb::{payment_identifier, PaymentIdentifier};
            while let Some(v) = ws_rx.recv().await {
                // Extract the nested update object from the WebSocket JSON response
                let maybe_update = v
                    .get("payload")
                    .and_then(|p| p.get("data"))
                    .and_then(|d| d.get("myUpdates"))
                    .and_then(|mu| mu.get("update"));
                
                if let Some(update) = maybe_update {
                    // Extract payment status from the update
                    let status = update.get("status").and_then(|s| s.as_str()).unwrap_or("");

                    // Preimage may be absent in some events; use empty string as fallback
                    let preimage = update
                        .get("preimage")
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_string();

                    let txn = update.get("transaction");

                    // Extract settlement amount in satoshis from transaction details
                    let settlement_sat = txn
                        .and_then(|t| t.get("settlementAmount"))
                        .and_then(|a| a.as_i64())
                        .unwrap_or(0);

                    // Extract payment hash from the transaction's initiation details
                    let payment_hash = txn
                        .and_then(|t| t.get("initiationVia"))
                        .and_then(|iv| iv.get("paymentHash").and_then(|h| h.as_str()))
                        .unwrap_or("")
                        .to_string();
                    
                    // Only forward PAID status updates
                    if !status.eq_ignore_ascii_case("PAID") {
                        continue;
                    }
                    
                    // Skip events without a payment hash
                    if payment_hash.is_empty() {
                        continue;
                    }
                    
                    // Build the response for the client
                    let resp = pb::WaitIncomingPaymentResponse {
                        payment_identifier: Some(PaymentIdentifier {
                            r#type: pb::PaymentIdentifierType::PaymentHash as i32,
                            value: Some(payment_identifier::Value::Hash(payment_hash)),
                        }),
                        payment_amount: settlement_sat.max(0) as u64,
                        unit: "sat".to_string(),
                        payment_id: preimage,
                    };
                    // Send to client via gRPC stream
                    let _ = app_tx.send(Ok(resp)).await;
                }
            }
        });

        // Return the receiver stream for the client to consume
        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            app_rx,
        )))
    }
}
