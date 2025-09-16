FROM rust:1-bookworm AS builder

# Install build dependencies (protoc, ca-certificates)
RUN apt-get update && apt-get install -y --no-install-recommends \
    protobuf-compiler ca-certificates pkg-config && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy source
COPY . .

# Build release binary
RUN cargo build --release


FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/*

# Create non-root user
RUN useradd -m -u 10001 appuser

WORKDIR /app

# Copy the built binary
COPY --from=builder /app/target/release/cdk-blink-payment-processor /usr/local/bin/cdk-blink-payment-processor

EXPOSE 50051

ENV RUST_LOG=info

USER appuser

ENTRYPOINT ["/usr/local/bin/cdk-blink-payment-processor"]