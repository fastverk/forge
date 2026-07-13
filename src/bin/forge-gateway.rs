//! forge-gateway — serves `forge.v1.ForgeService` over gRPC.
//!
//! The daemon holds no forge credential of its own; the caller's identity travels
//! in request metadata (see `forge::gateway`). Bind address from
//! `$FORGE_GATEWAY_BIND` (default `0.0.0.0:50055`) — distinct from the
//! `$FORGE_GATEWAY_ADDR` the *clients* (the console forge plugin) dial.

use std::net::SocketAddr;

use forge::gateway::ForgeGateway;
use tonic::transport::Server;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let addr: SocketAddr = std::env::var("FORGE_GATEWAY_BIND")
        .unwrap_or_else(|_| "0.0.0.0:50055".to_string())
        .parse()?;

    tracing::info!(%addr, "starting forge-gateway (forge.v1.ForgeService)");

    Server::builder()
        .add_service(ForgeGateway::default().into_server())
        .serve(addr)
        .await?;
    Ok(())
}
