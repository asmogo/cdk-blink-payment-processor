mod pb;
mod server;
mod blink;
mod settings;

use crate::server::PaymentProcessorService;
use anyhow::Result;
use std::net::SocketAddr;
use tonic::transport::Server;
use tracing_subscriber::EnvFilter;
use tonic_reflection::server::Builder as ReflectionBuilder;

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

    // Enable gRPC reflection so tools like grpcurl can discover services and methods
    let reflection_svc = ReflectionBuilder::configure()
        .register_encoded_file_descriptor_set(pb::FILE_DESCRIPTOR_SET)
        .build_v1()?; // use v1 to satisfy grpcurl reflection

    Server::builder()
        .add_service(reflection_svc)
        .add_service(pb::cdk_payment_processor::cdk_payment_processor_server::CdkPaymentProcessorServer::new(
            svc,
        ))
        .serve(addr)
        .await?;

    Ok(())
}
