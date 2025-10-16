# CDK Blink Payment Processor (Rust)

A gRPC service that bridges the CDK payment processor proto to Blink’s GraphQL and WebSocket APIs. It exposes a single gRPC server that:
- Creates incoming Lightning invoices (BOLT11) via Blink
- Produces payment quotes
- Sends outgoing payments
- Checks incoming/outgoing payment status
- Streams incoming payment updates over a resilient WebSocket subscription

Core modules:
- Server service implementation: [src/server/mod.rs](src/server/mod.rs)
- Entry point: [src/main.rs](src/main.rs)
- Blink GraphQL client: [src/blink/rest.rs](src/blink/rest.rs)
- Blink WebSocket client: [src/blink/ws.rs](src/blink/ws.rs)
- Protobuf definition: [src/payment_processor.proto](src/payment_processor.proto)
- Protobuf build script: [build.rs](build.rs)
- Configuration loader: [src/settings.rs](src/settings.rs)

Status: core RPCs and streaming implemented. No persistent database.

## Requirements

- Rust (stable toolchain) and Cargo
- protoc (Protocol Buffers compiler) available on PATH
  - macOS: brew install protobuf
  - Linux: install distro package (e.g., apt-get install -y protobuf-compiler)
- Network access to Blink API

## Setup and Configuration

Configuration can be provided via a config file at the repository root or environment variables. Default values are in [src/settings.rs](src/settings.rs).

Example [config.toml](config.toml):
```toml
blink_api_url = "https://api.blink.sv/graphql"
blink_api_key = "<your key>"
blink_wallet_id = ""        # optional; default wallet used when empty
server_port = 50051         # gRPC listen port

# HTTP/2 keep-alive and connection age configuration (humantime durations)
keep_alive_interval = "30s"
keep_alive_timeout = "10s"
max_connection_age = "30m"

# TLS configuration
# Enable TLS on the gRPC server. If true and the cert/key don't exist,
# the server will generate a self-signed cert for localhost and write it
# to the given paths.
tls_enable = false
tls_cert_path = "certs/server.crt"
tls_key_path = "certs/server.key"
```

Environment variables override file values:
- BLINK_API_URL
- BLINK_API_KEY
- BLINK_WALLET_ID
- SERVER_PORT
- KEEP_ALIVE_INTERVAL          # e.g. "45s"
- KEEP_ALIVE_TIMEOUT           # e.g. "15s"
- MAX_CONNECTION_IDLE          # e.g. "10m"
- MAX_CONNECTION_AGE           # e.g. "1h"
- MAX_CONNECTION_AGE_GRACE     # e.g. "2m"

Example run with env:
```
BLINK_API_URL=https://api.blink.sv/graphql \
BLINK_API_KEY=your_api_key \
BLINK_WALLET_ID=your_wallet_id_or_empty \
SERVER_PORT=50051 \
RUST_LOG=info \
cargo run --release
```

## Build

The build script compiles the proto during build using tonic-build (prost).

- Proto: [src/payment_processor.proto](src/payment_processor.proto)
- Build script: [build.rs](build.rs)

Build release:
```
cargo build --release
```

## Run

By default the server listens on 0.0.0.0:SERVER_PORT.

```
RUST_LOG=info cargo run --release
```

## gRPC API surface

Service: cdk_payment_processor.CdkPaymentProcessor (generated into [src/pb.rs](src/pb.rs) from [src/payment_processor.proto](src/payment_processor.proto))

RPCs:
- GetSettings(EmptyRequest) -> SettingsResponse
- CreatePayment(CreatePaymentRequest) -> CreatePaymentResponse
- GetPaymentQuote(PaymentQuoteRequest) -> PaymentQuoteResponse
- MakePayment(MakePaymentRequest) -> MakePaymentResponse
- CheckIncomingPayment(CheckIncomingPaymentRequest) -> CheckIncomingPaymentResponse
- CheckOutgoingPayment(CheckOutgoingPaymentRequest) -> MakePaymentResponse
- WaitIncomingPayment(EmptyRequest) -> stream WaitIncomingPaymentResponse

Notes:
- Currency unit is typically "sat".
- PaymentIdentifier supports types: PAYMENT_HASH, PAYMENT_ID, etc. When sending JSON via grpcurl, set the enum name (e.g., "PAYMENT_HASH") and provide the corresponding field (hash or id).

## Usage examples (grpcurl)

All examples assume a local server on 127.0.0.1:50051 with plaintext gRPC.

- GetSettings
```
grpcurl -plaintext -d '{}' 127.0.0.1:50051 cdk_payment_processor.CdkPaymentProcessor/GetSettings
```

- CreatePayment (BOLT11 invoice for 1000 sat)
```
grpcurl -plaintext -d '{
  "unit": "sat",
  "options": { "bolt11": { "description": "test invoice", "amount": 1000, "unix_expiry": 300 } }
}' 127.0.0.1:50051 cdk_payment_processor.CdkPaymentProcessor/CreatePayment
```

- GetPaymentQuote (for a BOLT11 invoice)
```
grpcurl -plaintext -d '{
  "request": "lnbc1...",
  "unit": "sat",
  "request_type": "BOLT11_INVOICE"
}' 127.0.0.1:50051 cdk_payment_processor.CdkPaymentProcessor/GetPaymentQuote
```

- MakePayment (send a BOLT11 invoice)
```
grpcurl -plaintext -d '{
  "payment_options": { "bolt11": { "bolt11": "lnbc1..." } }
}' 127.0.0.1:50051 cdk_payment_processor.CdkPaymentProcessor/MakePayment
```

- CheckIncomingPayment by payment hash
```
grpcurl -plaintext -d '{
  "request_identifier": { "type": "PAYMENT_HASH", "hash": "000abc..." }
}' 127.0.0.1:50051 cdk_payment_processor.CdkPaymentProcessor/CheckIncomingPayment
```

- CheckOutgoingPayment by payment id or hash
```
# By payment id
grpcurl -plaintext -d '{
  "request_identifier": { "type": "PAYMENT_ID", "id": "payment-id-123" }
}' 127.0.0.1:50051 cdk_payment_processor.CdkPaymentProcessor/CheckOutgoingPayment

# By payment hash
grpcurl -plaintext -d '{
  "request_identifier": { "type": "PAYMENT_HASH", "hash": "000abc..." }
}' 127.0.0.1:50051 cdk_payment_processor.CdkPaymentProcessor/CheckOutgoingPayment
```

- WaitIncomingPayment (server streaming)
```
grpcurl -plaintext -d '{}' 127.0.0.1:50051 cdk_payment_processor.CdkPaymentProcessor/WaitIncomingPayment
```

## Blink integrations

HTTP GraphQL (via [src/blink/rest.rs](src/blink/rest.rs)):
- Base URL: blink_api_url (e.g., https://api.blink.sv/graphql)
- Headers: Content-Type: application/json, X-API-KEY: <your_api_key>
- Operations used:
  - query me { defaultAccount { wallets { id walletCurrency balance } } }
  - mutation lnInvoiceCreate(input: { walletId, amount, memo })
  - mutation lnInvoicePaymentSend(input: { walletId, paymentRequest, memo })
  - query lnInvoicePaymentStatusByPaymentRequest(input: { paymentRequest })
  - query lnInvoicePaymentStatusByHash(input: { paymentHash })
  - query payment(id: ID!)
  - query quote(offerId: ID!)

WebSocket GraphQL (via [src/blink/ws.rs](src/blink/ws.rs)):
- Derived endpoint: replace https://api.blink.sv/graphql with wss://ws.blink.sv/graphql
- Subprotocol: graphql-transport-ws
- Handshake:
  - Send connection_init with payload: { "X-API-KEY": "<your_api_key>" }
  - Then subscribe with: subscription myUpdates { myUpdates { errors { message } me { id } update { __typename ... on LnUpdate { status transaction { id direction settlementAmount initiationVia { __typename ... on InitiationViaLn { paymentRequest paymentHash } } } } } } }
- The service reconnects with exponential backoff and responds to pings. Incoming payloads are translated to WaitIncomingPaymentResponse.

## Protobuf and code generation

- The protobuf schema is in [src/payment_processor.proto](src/payment_processor.proto).
- [build.rs](build.rs) runs tonic-build to generate server and client code at build time.
- The generated Rust module is included from [src/pb.rs](src/pb.rs).
- The gRPC server is started in [src/main.rs](src/main.rs) and the service implementation lives in [src/server/mod.rs](src/server/mod.rs).

## Development notes

- Logging uses tracing; configure with RUST_LOG (e.g., RUST_LOG=info).
- Configuration precedence: defaults -> config.toml -> environment variables, as implemented in [src/settings.rs](src/settings.rs).
- Invoice decoding uses lightning-invoice to derive amounts when needed.
- No persistent storage; all state is in-memory and derived from Blink.

## Security

- Never commit real API keys. Use environment variables or a local, untracked config.toml.
- Restrict network access appropriately and run behind a firewall when exposing the gRPC port.
