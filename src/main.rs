mod pb;
mod server;
mod blink;
mod settings;

use crate::server::PaymentProcessorService;
use anyhow::Result;
use std::net::SocketAddr;
use tonic::transport::Server;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    // Logging
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse().unwrap()))
        .init();

    // Load configuration from environment
    let cfg = settings::Config::from_env();

    let addr: SocketAddr = format!("0.0.0.0:{}", cfg.server_port).parse()?;

    let svc = PaymentProcessorService::try_new(cfg.clone()).await?;

    tracing::info!("Starting gRPC server on {}", addr);

    Server::builder()
        .add_service(pb::cdk_payment_processor::cdk_payment_processor_server::CdkPaymentProcessorServer::new(
            svc,
        ))
        .serve(addr)
        .await?;

    Ok(())
}
