//! CDK Blink Payment Processor - gRPC Server Entry Point
//!
//! This module initializes and starts the gRPC server that bridges CDK payment processors
//! to Blink's GraphQL and WebSocket APIs. It handles:
//! - Configuration loading from environment
//! - TLS/SSL certificate management (auto-generation if needed)
//! - gRPC reflection setup for service discovery
//! - HTTP/2 keepalive configuration
//! - Server startup and shutdown

mod blink;
mod pb;
mod server;
mod settings;

use crate::server::PaymentProcessorService;
use anyhow::Result;
use rcgen::generate_simple_self_signed;
use std::{fs, net::SocketAddr, path::Path};
use tonic::transport::{Identity, Server, ServerTlsConfig};
use tonic_reflection::server::Builder as ReflectionBuilder;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize structured logging with environment filter (default: info level)
    // This allows controlling log levels via RUST_LOG environment variable
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse().unwrap()))
        .init();

    // Load configuration from multiple sources in priority order:
    // 1. Environment variables (highest priority)
    // 2. config.toml file (if present)
    // 3. Hardcoded defaults (lowest priority)
    let cfg = settings::Config::from_env();

    // Validate required configuration: BLINK_API_KEY must be set
    // This prevents starting the server with invalid configuration
    if cfg.blink_api_key.is_empty() {
        tracing::error!("BLINK_API_KEY not set; exiting");
        return Ok(());
    }

    // Parse server listen address from configuration
    // Format: "0.0.0.0:{port}" to listen on all interfaces
    let addr: SocketAddr = format!("0.0.0.0:{}", cfg.server_port).parse()?;

    // Initialize the gRPC service implementation
    // This creates BlinkClient internally and validates connectivity to Blink API
    let svc = PaymentProcessorService::try_new(cfg.clone()).await?;

    // Enable gRPC reflection for service discovery
    // This allows tools like grpcurl to list available services and methods without
    // additional configuration: grpcurl -plaintext localhost:50051 list
    let reflection_svc = ReflectionBuilder::configure()
        .register_encoded_file_descriptor_set(pb::FILE_DESCRIPTOR_SET)
        .build_v1()?; // use v1 to satisfy grpcurl reflection

    // Build the gRPC server with HTTP/2 keepalive settings
    // These settings prevent connections from being dropped due to NAT/firewall inactivity
    let mut builder = Server::builder()
        .http2_keepalive_interval(Some(cfg.keep_alive_interval))
        .http2_keepalive_timeout(Some(cfg.keep_alive_timeout))
        .max_connection_age(cfg.max_connection_age);

    // Configure TLS/SSL if enabled
    if cfg.tls_enable {
        // Construct paths for certificate, key, and CA files
        let cert_path = Path::new(&cfg.tls_cert_path);
        let key_path = Path::new(&cfg.tls_key_path);
        let ca_path = {
            let stem = cert_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("cert");
            let ca_file = format!("{stem}.ca.pem");
            cert_path.with_file_name(ca_file)
        };

        // Auto-generate self-signed certificates if not present
        // Useful for development/testing, but production should use CA-signed certificates
        if !cert_path.exists() || !key_path.exists() {
            tracing::warn!(
                cert=%cert_path.display(), key=%key_path.display(),
                "TLS enabled but certificate or key not found; generating CA and server certificate"
            );
            // Create directories for certificate files
            if let Some(parent) = cert_path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            if let Some(parent) = key_path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            if let Some(parent) = ca_path.parent() {
                let _ = fs::create_dir_all(parent);
            }

            // Generate self-signed CA certificate for development/testing
            let mut ca_params = rcgen::CertificateParams::default();
            ca_params
                .distinguished_name
                .push(rcgen::DnType::CommonName, "CDK Blink Dev CA");
            ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
            ca_params.key_usages = vec![
                rcgen::KeyUsagePurpose::KeyCertSign,
                rcgen::KeyUsagePurpose::CrlSign,
                rcgen::KeyUsagePurpose::DigitalSignature,
            ];
            let ca_cert = rcgen::Certificate::from_params(ca_params).expect("generate CA cert");

            // Generate server certificate signed by the CA
            // Includes both localhost and 127.0.0.1 for local testing
            let mut server_params =
                rcgen::CertificateParams::new(vec!["localhost".into(), "127.0.0.1".into()]);
            server_params
                .distinguished_name
                .push(rcgen::DnType::CommonName, "localhost");
            server_params
                .distinguished_name
                .push(rcgen::DnType::CommonName, "127.0.0.1");
            server_params
                .extended_key_usages
                .push(rcgen::ExtendedKeyUsagePurpose::ServerAuth);

            let server_cert =
                rcgen::Certificate::from_params(server_params).expect("generate server cert");
            let server_cert_pem = server_cert
                .serialize_pem_with_signer(&ca_cert)
                .expect("serialize server cert pem");
            let server_key_pem = server_cert.serialize_private_key_pem();
            let ca_cert_pem = ca_cert.serialize_pem().expect("serialize ca pem");

            // Write certificate and key files to disk
            fs::write(&cert_path, server_cert_pem).expect("write server cert pem");
            fs::write(&key_path, server_key_pem).expect("write server key pem");
            fs::write(&ca_path, ca_cert_pem).expect("write ca cert pem");
        }

        // Load server certificate and append CA certificate to form a certificate chain
        // This ensures clients receive the CA issuer for certificate validation
        let mut chain_pem = fs::read(&cfg.tls_cert_path)?;
        if let Ok(ca_pem) = fs::read(&ca_path) {
            chain_pem.extend_from_slice(b"\n");
            chain_pem.extend_from_slice(&ca_pem);
        }

        // Load private key and create TLS identity
        let key = fs::read(&cfg.tls_key_path)?;
        let identity = Identity::from_pem(chain_pem, key);
        builder = builder.tls_config(ServerTlsConfig::new().identity(identity))?;
        tracing::info!(
            addr=%addr,
            cert=%cfg.tls_cert_path,
            key=%cfg.tls_key_path,
            ca=%ca_path.display(),
            "Starting TLS-enabled gRPC server (with CA chain)"
        );
    } else {
        tracing::info!(addr=%addr, "Starting plaintext gRPC server");
    }

    // Add reflection service and payment processor service to the server
    // Then bind to the configured address and start listening for incoming connections
    builder
        .add_service(reflection_svc)
        .add_service(
            pb::cdk_payment_processor::cdk_payment_processor_server::CdkPaymentProcessorServer::new(
                svc,
            ),
        )
        .serve(addr)
        .await?;

    Ok(())
}
