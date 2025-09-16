# Rust port: CDK Blink Payment Processor

This is a Rust rewrite of the gRPC payment processor that bridges the CDK proto to Blink's GraphQL + WebSocket APIs.

Status: streaming and core RPCs implemented. No persistent DB.

## Build

- Requires Rust (stable)
- Uses tonic + prost to compile proto

```
cargo build
```

## Configuration (TOML + env)

- A `config.toml` file in the `rust/` directory can be used (an example is provided):

```
# rust/config.toml
blink_api_url = "https://api.blink.sv/graphql"
blink_api_key = "your_api_key"
blink_wallet_id = "your_wallet_id"
server_port = 50051
```

- Environment variables override file values:
  - BLINK_API_URL
  - BLINK_API_KEY
  - BLINK_WALLET_ID
  - SERVER_PORT

## Run

```
RUST_LOG=info cargo run
```

Or with explicit env vars:

```
BLINK_API_URL=https://api.blink.sv/graphql \
BLINK_API_KEY=your_api_key \
BLINK_WALLET_ID=your_wallet_id \
SERVER_PORT=50051 \
RUST_LOG=info \
cargo run
```

## Proto

The build script compiles `src/payment_processor.proto` into Rust code with tonic.

## Notes

- No stateful DB is used.
- WaitIncomingPayment streams LnUpdate events via Blink WebSocket myUpdates with reconnection & resubscription.
- BOLT11 decoding is performed locally via `lightning-invoice`.
- Error mapping is simplified to tonic::Status equivalents.
