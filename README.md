# CDK Blink Payment Processor

A [CDK payment processor](https://github.com/cashubtc/cdk-payment-processors) that connects a
[Cashu Development Kit (CDK)](https://github.com/cashubtc/cdk) mint to Blink's Lightning
infrastructure. It implements the CDK `MintPayment` interface (exposed over gRPC by
[`cdk-payment-processor`](https://crates.io/crates/cdk-payment-processor)) on top of Blink's
GraphQL and WebSocket APIs:

- Creates incoming Lightning invoices (BOLT11) via Blink
- Produces payment quotes with real routing-fee probes
- Sends outgoing payments
- Checks incoming/outgoing payment status
- Streams incoming payment updates over a resilient WebSocket subscription

Core modules:

- `MintPayment` backend: [src/backend.rs](src/backend.rs)
- Entry point: [src/main.rs](src/main.rs)
- Blink GraphQL client: [src/blink/rest.rs](src/blink/rest.rs)
- Blink WebSocket client: [src/blink/ws.rs](src/blink/ws.rs)
- Configuration loader: [src/settings.rs](src/settings.rs)

### Key Features

- **Lightning Network Integration**: Create and pay BOLT11 invoices via Blink's API
- **Real-time Payment Streaming**: WebSocket-based incoming payment updates with automatic reconnection
- **Fee Probing**: Quotes use Blink's `lnInvoiceFeeProbe` for accurate routing fees
- **Payment Status Tracking**: Check status of incoming and outgoing payments by payment hash
- **TLS Support**: Optional TLS encryption for production deployments
- **Configurable**: Environment variables and config file support

## Requirements

- Rust (stable toolchain) and Cargo
- Network access to the Blink API
- A Blink API key (<https://dashboard.blink.sv>)

## Setup and Configuration

Configuration can be provided via a `config.toml` file in the working directory or via
environment variables. Environment variables override file values.

Example [config.toml](config.toml):

```toml
address = "0.0.0.0"
port = 50051

# TLS configuration (paths to PEM files)
# tls_enable = false
# tls_cert_path = "certs/server.crt"
# tls_key_path = "certs/server.key"

[backend]
api_url = "https://api.blink.sv/graphql"
api_key = "<your key>"
wallet_id = ""        # optional; default BTC wallet is resolved when empty
```

Environment variables:

- `BLINK_API_URL`
- `BLINK_API_KEY` (required)
- `BLINK_WALLET_ID`
- `SERVER_ADDRESS`
- `SERVER_PORT`
- `TLS_ENABLE` (`true`/`false`)
- `TLS_CERT_PATH`
- `TLS_KEY_PATH`

Example run with env:

```
BLINK_API_KEY=your_api_key \
BLINK_WALLET_ID=your_wallet_id_or_empty \
SERVER_PORT=50051 \
RUST_LOG=info \
cargo run --release
```

## Build

```
cargo build --release
```

## Run

By default the server listens on `0.0.0.0:50051` (plaintext).

```
RUST_LOG=info cargo run --release
```

## How it works

The processor does not implement the gRPC service itself. Instead,
[src/backend.rs](src/backend.rs) implements the
[`MintPayment`](https://docs.rs/cdk-common/latest/cdk_common/payment/trait.MintPayment.html)
trait from `cdk-common`, and [src/main.rs](src/main.rs) exposes it through
`PaymentProcessorServer` from the `cdk-payment-processor` crate, which handles the gRPC
protocol (including the `x-cdk-protocol-version` handshake) for all CDK payment processors.

Supported payment options:

- **BOLT11** only, unit `sat`. BOLT12, onchain, and custom payment methods are declared as
  unsupported in `GetSettings`.
- Fee quotes use Blink's `lnInvoiceFeeProbe`; if the probe fails, a conservative estimate
  (1% of the amount, min 1 sat) is returned.
- `MakePayment` maps Blink statuses to melt quote states (`SUCCESS` → `PAID`,
  `PENDING` → `PENDING`, `FAILURE` → `FAILED`) and returns the payment preimage as the
  payment proof when available.

## Blink integration

HTTP GraphQL (via [src/blink/rest.rs](src/blink/rest.rs)):

- Base URL: `BLINK_API_URL` (e.g., <https://api.blink.sv/graphql>)
- Headers: `Content-Type: application/json`, `X-API-KEY: <your_api_key>`
- Operations used:
  - `query me { defaultAccount { wallets { id walletCurrency balance } } }`
  - `mutation lnInvoiceCreate(input: { walletId, amount, memo })`
  - `mutation lnInvoiceFeeProbe(input: { walletId, paymentRequest })`
  - `mutation lnInvoicePaymentSend(input: { walletId, paymentRequest, memo })`
  - `query lnInvoicePaymentStatusByPaymentRequest(input: { paymentRequest })`
  - `query lnInvoicePaymentStatusByHash(input: { paymentHash })`

WebSocket GraphQL (via [src/blink/ws.rs](src/blink/ws.rs)):

- Derived endpoint: replace `https://api.blink.sv/graphql` with `wss://ws.blink.sv/graphql`
- Subprotocol: `graphql-transport-ws`
- Handshake:
  - Send `connection_init` with payload `{ "X-API-KEY": "<your_api_key>" }`
  - Then subscribe with `subscription myUpdates { myUpdates { ... update { ... on LnUpdate { status transaction { id direction settlementAmount initiationVia { ... on InitiationViaLn { paymentRequest paymentHash } } } } } } }`
- The client reconnects with exponential backoff and answers ping/pong keep-alives. Paid
  updates are translated into CDK `PaymentReceived` events.

## Connecting a mint

Point `cdk-mintd` at this processor as its Lightning backend (gRPC payment processor),
using the same listen address/port and matching TLS settings. The mint and the processor
must agree on the CDK payment processor protocol version (handled automatically via the
`x-cdk-protocol-version` header).

## Development notes

- Logging uses `tracing`; configure with `RUST_LOG` (e.g., `RUST_LOG=info`).
- Configuration precedence: defaults → `config.toml` → environment variables.
- No persistent storage; all state is derived from Blink at request time.
- Run tests with `cargo test` (unit tests stub the Blink API with wiremock).

## Security

- Never commit real API keys. Use environment variables or a local, untracked config.toml.
- Restrict network access appropriately and run behind a firewall when exposing the gRPC port.
- Enable TLS (`TLS_ENABLE=true` with valid cert/key) for any non-localhost deployment.
