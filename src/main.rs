use anyhow::Result;
use rmcp::{ServiceExt, transport::io::stdio};
use tracing_subscriber::EnvFilter;

use shadowpty::pty_manager::PtyManager;
use shadowpty::server::ShadowPtyServer;

#[tokio::main]
async fn main() -> Result<()> {
    // CRITICAL: Initialize tracing to stderr exclusively so stdout is not corrupted for JSON-RPC.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    tracing::info!("Initializing ShadowPTY MCP server");

    let manager = PtyManager::new();
    let server = ShadowPtyServer::new(manager);

    let service = server
        .serve(stdio())
        .await
        .inspect_err(|e| tracing::error!("Failed to serve MCP server: {:?}", e))?;

    tracing::info!("ShadowPTY MCP server listening on stdio");
    service.waiting().await?;

    Ok(())
}
