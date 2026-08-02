//! Blink backend implementing the CDK `MintPayment` trait.
//!
//! Uses Blink's GraphQL API for invoices, payments, and status queries, and
//! Blink's WebSocket subscription endpoint for real-time payment events.

use std::collections::HashMap;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use async_trait::async_trait;
use cdk_common::bitcoin::hashes::Hash;
use cdk_common::nuts::{CurrencyUnit, MeltQuoteState};
use cdk_common::payment::{
    Bolt11Settings, CreateIncomingPaymentResponse, Error, Event, IncomingPaymentOptions,
    MakePaymentResponse, MintPayment, OutgoingPaymentOptions, PaymentIdentifier,
    PaymentQuoteResponse, SettingsResponse, WaitPaymentResponse,
};
use cdk_common::{Amount, Bolt11Invoice};
use futures_core::Stream;
use tokio::sync::{mpsc, watch};
use tokio_stream::wrappers::ReceiverStream;

use crate::blink::rest::BlinkClient;
use crate::blink::ws::WSClient;
use crate::settings::BackendConfig;

struct PaymentEventStreamActivity {
    active_streams: Arc<AtomicUsize>,
    active: std::sync::atomic::AtomicBool,
}

impl PaymentEventStreamActivity {
    fn new(active_streams: Arc<AtomicUsize>) -> Self {
        active_streams.fetch_add(1, Ordering::Relaxed);
        Self {
            active_streams,
            active: std::sync::atomic::AtomicBool::new(true),
        }
    }

    fn deactivate(&self) {
        if self.active.swap(false, Ordering::Relaxed) {
            self.active_streams.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

impl Drop for PaymentEventStreamActivity {
    fn drop(&mut self) {
        self.deactivate();
    }
}

/// Event stream that releases its activity marker on completion or drop.
struct PaymentEventStream {
    receiver: ReceiverStream<Event>,
    activity: Arc<PaymentEventStreamActivity>,
}

impl PaymentEventStream {
    fn new(receiver: mpsc::Receiver<Event>, active_streams: Arc<AtomicUsize>) -> Self {
        Self {
            receiver: ReceiverStream::new(receiver),
            activity: Arc::new(PaymentEventStreamActivity::new(active_streams)),
        }
    }
}

impl Stream for PaymentEventStream {
    type Item = Event;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.get_mut().receiver).poll_next(cx)
    }
}

impl Drop for PaymentEventStream {
    fn drop(&mut self) {
        self.activity.deactivate();
    }
}

/// Blink Lightning backend
pub struct BlinkBackend {
    client: BlinkClient,
    event_cancel: watch::Sender<()>,
    active_streams: Arc<AtomicUsize>,
}

impl BlinkBackend {
    /// Create a new Blink backend.
    ///
    /// Validates the API key and resolves the wallet id (querying the
    /// account's default BTC wallet when none is configured).
    pub async fn new(config: &BackendConfig) -> anyhow::Result<Self> {
        if config.api_key.is_empty() {
            anyhow::bail!("Blink API key is required (set BLINK_API_KEY)");
        }

        let mut client = BlinkClient::new(config)?;
        if config.wallet_id.is_empty() {
            let wallet = client.get_default_wallet().await?;
            tracing::info!(
                wallet_id = %wallet.id,
                currency = %wallet.wallet_currency,
                "resolved default Blink wallet"
            );
            client.set_wallet_id(wallet.id);
        }

        let (event_cancel, _) = watch::channel(());
        Ok(Self {
            client,
            event_cancel,
            active_streams: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// Decode a Blink hex payment hash into raw bytes
    fn payment_hash_bytes(hash_hex: &str) -> Result<[u8; 32], Error> {
        let bytes = hex::decode(hash_hex).map_err(|_| Error::InvalidHash)?;
        bytes.try_into().map_err(|_| Error::InvalidHash)
    }

    /// Encode a payment identifier as a Blink hex payment hash
    fn payment_hash_hex(payment_identifier: &PaymentIdentifier) -> Result<String, Error> {
        match payment_identifier {
            PaymentIdentifier::PaymentHash(hash) => Ok(hex::encode(hash)),
            _ => Err(Error::Custom(
                "Unsupported payment identifier type".to_string(),
            )),
        }
    }

    /// Extract the invoice amount in satoshis
    fn invoice_amount_sats(invoice: &Bolt11Invoice) -> Result<u64, Error> {
        let amount_msat = invoice.amount_milli_satoshis().ok_or(Error::AmountMismatch)?;
        let amount_sats = amount_msat.div_ceil(1000);
        if amount_sats == 0 {
            return Err(Error::AmountMismatch);
        }
        Ok(amount_sats)
    }

    /// Map a Blink payment status to a melt quote state
    fn melt_state(status: &str) -> MeltQuoteState {
        match status.to_ascii_uppercase().as_str() {
            "SUCCESS" | "PAID" => MeltQuoteState::Paid,
            "PENDING" => MeltQuoteState::Pending,
            "FAILURE" | "FAILED" | "EXPIRED" => MeltQuoteState::Failed,
            _ => MeltQuoteState::Unknown,
        }
    }

    /// Map a raw Blink `myUpdates` WebSocket event to a CDK payment event
    fn ws_event_to_cdk(v: &serde_json::Value) -> Option<Event> {
        let update = v
            .get("payload")
            .and_then(|p| p.get("data"))
            .and_then(|d| d.get("myUpdates"))
            .and_then(|mu| mu.get("update"))?;

        let status = update.get("status").and_then(|s| s.as_str()).unwrap_or("");
        if !status.eq_ignore_ascii_case("PAID") {
            return None;
        }

        let txn = update.get("transaction")?;
        let payment_hash = txn
            .get("initiationVia")
            .and_then(|iv| iv.get("paymentHash"))
            .and_then(|h| h.as_str())?;
        if payment_hash.is_empty() {
            return None;
        }

        let settlement_sat = txn
            .get("settlementAmount")
            .and_then(|a| a.as_i64())
            .unwrap_or(0)
            .max(0) as u64;

        let payment_id = txn
            .get("id")
            .and_then(|id| id.as_str())
            .unwrap_or(payment_hash)
            .to_string();

        Some(Event::PaymentReceived(WaitPaymentResponse {
            payment_identifier: PaymentIdentifier::PaymentHash(
                Self::payment_hash_bytes(payment_hash).ok()?,
            ),
            payment_amount: Amount::new(settlement_sat, CurrencyUnit::Sat),
            payment_id,
        }))
    }
}

#[async_trait]
impl MintPayment for BlinkBackend {
    type Err = Error;

    async fn stop(&self) -> Result<(), Self::Err> {
        self.cancel_payment_event_stream();
        Ok(())
    }

    async fn get_settings(&self) -> Result<SettingsResponse, Self::Err> {
        Ok(SettingsResponse {
            unit: "sat".to_string(),
            bolt11: Some(Bolt11Settings {
                mpp: false,
                amountless: false,
                invoice_description: true,
            }),
            bolt12: None,
            onchain: None,
            custom: HashMap::new(),
        })
    }

    async fn create_incoming_payment_request(
        &self,
        options: IncomingPaymentOptions,
    ) -> Result<CreateIncomingPaymentResponse, Self::Err> {
        let IncomingPaymentOptions::Bolt11(opts) = options else {
            return Err(Error::UnsupportedPaymentOption);
        };

        let amount_sats = opts.amount.to_sat()?;
        if amount_sats == 0 {
            return Err(Error::AmountMismatch);
        }

        let memo = opts.description.unwrap_or_default();
        let invoice = self
            .client
            .create_invoice(amount_sats, &memo)
            .await
            .map_err(|e| Error::Lightning(e.into()))?;

        let payment_hash = Self::payment_hash_bytes(&invoice.payment_hash)?;

        Ok(CreateIncomingPaymentResponse {
            request_lookup_id: PaymentIdentifier::PaymentHash(payment_hash),
            request: invoice.payment_request,
            expiry: opts.unix_expiry,
            extra_json: None,
        })
    }

    async fn get_payment_quote(
        &self,
        unit: &CurrencyUnit,
        options: OutgoingPaymentOptions,
    ) -> Result<PaymentQuoteResponse, Self::Err> {
        let OutgoingPaymentOptions::Bolt11(opts) = options else {
            return Err(Error::UnsupportedPaymentOption);
        };

        let amount_sats = Self::invoice_amount_sats(&opts.bolt11)?;
        let bolt11 = opts.bolt11.to_string();

        // Probe the real routing fee from Blink; fall back to a conservative
        // estimate (1%, min 1 sat) if the probe fails.
        let fee_sats = match self.client.probe_fee(&bolt11).await {
            Ok(fee) => fee,
            Err(e) => {
                tracing::warn!(error=?e, "Blink fee probe failed, using fallback estimate");
                (amount_sats / 100).max(1)
            }
        };

        let payment_hash = opts.bolt11.payment_hash().to_byte_array();

        Ok(PaymentQuoteResponse {
            request_lookup_id: Some(PaymentIdentifier::PaymentHash(payment_hash)),
            amount: Amount::new(amount_sats, unit.clone()),
            fee: Amount::new(fee_sats, unit.clone()),
            state: MeltQuoteState::Unpaid,
            extra_json: None,
            estimated_blocks: None,
            fee_options: None,
        })
    }

    async fn make_payment(
        &self,
        unit: &CurrencyUnit,
        options: OutgoingPaymentOptions,
    ) -> Result<MakePaymentResponse, Self::Err> {
        let OutgoingPaymentOptions::Bolt11(opts) = options else {
            return Err(Error::UnsupportedPaymentOption);
        };

        let amount_sats = Self::invoice_amount_sats(&opts.bolt11)?;
        let bolt11 = opts.bolt11.to_string();
        let payment_hash = opts.bolt11.payment_hash().to_byte_array();
        let payment_identifier = PaymentIdentifier::PaymentHash(payment_hash);

        let status = self
            .client
            .make_payment(&bolt11)
            .await
            .map_err(|e| {
                let msg = e.to_string().to_lowercase();
                if msg.contains("already paid") {
                    Error::InvoiceAlreadyPaid
                } else if msg.contains("pending") {
                    Error::InvoicePaymentPending
                } else {
                    Error::Lightning(e.into())
                }
            })?;

        // Fetch the preimage as the payment proof once available
        let preimage = if status.eq_ignore_ascii_case("SUCCESS") {
            self.client
                .check_invoice_status_by_request(&bolt11)
                .await
                .map(|(_, _, preimage)| preimage)
                .ok()
                .filter(|p| !p.is_empty())
        } else {
            None
        };

        Ok(MakePaymentResponse {
            payment_lookup_id: payment_identifier,
            payment_proof: preimage,
            status: Self::melt_state(&status),
            total_spent: Amount::new(amount_sats, unit.clone()),
        })
    }

    async fn wait_payment_event(
        &self,
    ) -> Result<Pin<Box<dyn Stream<Item = Event> + Send>>, Self::Err> {
        let (sender, receiver) = mpsc::channel(100);
        let (ws_tx, mut ws_rx) = mpsc::channel(100);

        let ws = WSClient::new(self.client.api_url().as_str(), self.client.api_key_str());
        ws.stream_my_updates(ws_tx)
            .await
            .map_err(|e| Error::Lightning(e.into()))?;

        let mut cancel = self.event_cancel.subscribe();
        let stream = PaymentEventStream::new(receiver, Arc::clone(&self.active_streams));
        let activity = Arc::clone(&stream.activity);

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.changed() => break,
                    _ = sender.closed() => break,
                    event = ws_rx.recv() => {
                        match event {
                            Some(v) => {
                                if let Some(event) = BlinkBackend::ws_event_to_cdk(&v) {
                                    if sender.send(event).await.is_err() {
                                        break;
                                    }
                                }
                            }
                            None => break,
                        }
                    }
                }
            }
            // Dropping ws_rx closes the channel, which stops the WebSocket
            // reconnect loop in the background task.
            activity.deactivate();
        });

        Ok(Box::pin(stream))
    }

    fn is_payment_event_stream_active(&self) -> bool {
        self.active_streams.load(Ordering::Relaxed) > 0
    }

    fn cancel_payment_event_stream(&self) {
        let _ = self.event_cancel.send(());
    }

    async fn check_incoming_payment_status(
        &self,
        payment_identifier: &PaymentIdentifier,
    ) -> Result<Vec<WaitPaymentResponse>, Self::Err> {
        let hash = Self::payment_hash_hex(payment_identifier)?;

        let (status, payment_request, preimage) = self
            .client
            .check_invoice_status_with_retry(&hash)
            .await
            .map_err(|e| Error::Lightning(e.into()))?;

        if !status.eq_ignore_ascii_case("PAID") {
            return Ok(vec![]);
        }

        let amount_sats = Bolt11Invoice::from_str(&payment_request)
            .ok()
            .and_then(|inv| inv.amount_milli_satoshis())
            .map(|msat| msat.div_ceil(1000))
            .unwrap_or(0);

        Ok(vec![WaitPaymentResponse {
            payment_identifier: payment_identifier.clone(),
            payment_amount: Amount::new(amount_sats, CurrencyUnit::Sat),
            payment_id: preimage,
        }])
    }

    async fn check_outgoing_payment(
        &self,
        payment_identifier: &PaymentIdentifier,
    ) -> Result<MakePaymentResponse, Self::Err> {
        let hash = Self::payment_hash_hex(payment_identifier)?;

        let (status, payment_request, preimage) = self
            .client
            .check_invoice_status_with_retry(&hash)
            .await
            .map_err(|e| Error::Lightning(e.into()))?;

        let state = Self::melt_state(&status);

        let total_spent = if state == MeltQuoteState::Paid {
            Bolt11Invoice::from_str(&payment_request)
                .ok()
                .and_then(|inv| inv.amount_milli_satoshis())
                .map(|msat| msat.div_ceil(1000))
                .unwrap_or(0)
        } else {
            0
        };

        let payment_proof = if state == MeltQuoteState::Paid && !preimage.is_empty() {
            Some(preimage)
        } else {
            None
        };

        Ok(MakePaymentResponse {
            payment_lookup_id: payment_identifier.clone(),
            payment_proof,
            status: state,
            total_spent: Amount::new(total_spent, CurrencyUnit::Sat),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn maps_blink_statuses_to_melt_states() {
        assert_eq!(BlinkBackend::melt_state("SUCCESS"), MeltQuoteState::Paid);
        assert_eq!(BlinkBackend::melt_state("PAID"), MeltQuoteState::Paid);
        assert_eq!(BlinkBackend::melt_state("PENDING"), MeltQuoteState::Pending);
        assert_eq!(BlinkBackend::melt_state("FAILURE"), MeltQuoteState::Failed);
        assert_eq!(BlinkBackend::melt_state("FAILED"), MeltQuoteState::Failed);
        assert_eq!(BlinkBackend::melt_state("EXPIRED"), MeltQuoteState::Failed);
        assert_eq!(BlinkBackend::melt_state("???"), MeltQuoteState::Unknown);
    }

    #[test]
    fn decodes_payment_hash_hex() {
        let hash = [7u8; 32];
        let hex_str = hex::encode(hash);
        assert_eq!(BlinkBackend::payment_hash_bytes(&hex_str).unwrap(), hash);
        assert!(BlinkBackend::payment_hash_bytes("not-hex").is_err());
        assert!(BlinkBackend::payment_hash_bytes("abcd").is_err());
    }

    #[test]
    fn converts_paid_ws_event_to_payment_received() {
        let event = json!({
            "payload": {
                "data": {
                    "myUpdates": {
                        "update": {
                            "status": "PAID",
                            "transaction": {
                                "id": "txn-1",
                                "direction": "RECEIVE",
                                "settlementAmount": 1234,
                                "initiationVia": {
                                    "paymentHash": hex::encode([9u8; 32])
                                }
                            }
                        }
                    }
                }
            }
        });

        let Some(Event::PaymentReceived(resp)) = BlinkBackend::ws_event_to_cdk(&event) else {
            panic!("expected PaymentReceived event");
        };
        assert_eq!(
            resp.payment_identifier,
            PaymentIdentifier::PaymentHash([9u8; 32])
        );
        assert_eq!(resp.payment_amount, Amount::new(1234, CurrencyUnit::Sat));
        assert_eq!(resp.payment_id, "txn-1");
    }

    #[test]
    fn ignores_non_paid_ws_events() {
        let event = json!({
            "payload": {
                "data": {
                    "myUpdates": {
                        "update": {
                            "status": "PENDING",
                            "transaction": {
                                "id": "txn-1",
                                "settlementAmount": 1234,
                                "initiationVia": {
                                    "paymentHash": hex::encode([9u8; 32])
                                }
                            }
                        }
                    }
                }
            }
        });
        assert!(BlinkBackend::ws_event_to_cdk(&event).is_none());

        let malformed = json!({"payload": {"data": {}}});
        assert!(BlinkBackend::ws_event_to_cdk(&malformed).is_none());
    }
}
