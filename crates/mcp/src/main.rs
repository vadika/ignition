//! ignition-mcp: stdio MCP server exposing ignition microVM sandboxes.

mod session;
mod tools;
mod vsock_client;

use std::path::PathBuf;
use std::time::Duration;

use rmcp::{ServiceExt, transport::stdio};

use session::SessionConfig;
use tools::Mcp;

fn env_path(key: &str, default: &str) -> PathBuf {
    std::env::var(key).map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(default))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();
    let cfg = SessionConfig {
        boot_bin: env_path("IGN_MCP_BOOT", "target/debug/boot"),
        kernel: env_path("IGN_MCP_KERNEL", "kimage/out/Image"),
        rootfs: env_path("IGN_MCP_ROOTFS", "kimage/out/rootfs-tools.ext4"),
        store: env_path("IGN_MCP_STORE", "mcp-store"),
        base: std::env::var("IGN_MCP_BASE").unwrap_or_else(|_| "tools-base".into()),
        uds_dir: std::env::temp_dir(),
        max_sessions: std::env::var("IGN_MCP_MAX_SESSIONS").ok()
            .and_then(|v| v.parse().ok()).unwrap_or(8),
        idle: Duration::from_secs(std::env::var("IGN_MCP_IDLE_SECS").ok()
            .and_then(|v| v.parse().ok()).unwrap_or(600)),
        net: std::env::var("IGN_MCP_NET").is_ok(),
    };

    let server = Mcp::new(cfg);

    let mgr = server.manager();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            mgr.lock().await.reap_idle();
        }
    });

    let service = server.clone().serve(stdio()).await?;
    service.waiting().await?;
    server.manager().lock().await.shutdown();
    Ok(())
}
